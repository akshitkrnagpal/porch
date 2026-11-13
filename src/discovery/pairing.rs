//! Code pairing. `porch pair` shows a six-digit code; `porch pair CODE` on the
//! other machine finds it and runs a SPAKE2 key exchange keyed by the code and
//! bound to the certificate it pinned. Only after the other machine proves it
//! knows the code does this machine turn on sharing, mint a credential for it
//! and pin its certificate; then both enroll each other.
use super::{client, pake, server, types::*};
use anyhow::{bail, Context, Result};
use rustls::{
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
    DigitallySignedStruct, SignatureScheme,
};
use sha2::{Digest, Sha256};
use std::{
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};

/// Accepts any certificate but records it. Used only for the unauthenticated
/// pairing probe; the code is then sent over a connection pinned to this record.
#[derive(Debug)]
struct Capture {
    seen: Mutex<Option<[u8; 32]>>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}
impl ServerCertVerifier for Capture {
    fn verify_server_cert(
        &self,
        cert: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        if let Ok(mut seen) = self.seen.lock() {
            *seen = Some(Sha256::digest(cert.as_ref()).into());
        }
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

enum Probe {
    Open {
        pin: String,
        salt: String,
    },
    /// Showing a code but speaking only the insecure version 1 exchange.
    Outdated,
    Closed,
}
/// Is the agent showing a pairing code? Returns its certificate fingerprint
/// and window salt (both unauthenticated until the key exchange succeeds).
async fn probe(endpoint: &str) -> Result<Probe> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let capture = Arc::new(Capture {
        seen: Mutex::new(None),
        provider: provider.clone(),
    });
    let tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(capture.clone())
        .with_no_client_auth();
    let http = reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        .https_only(true)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(3))
        .build()?;
    let response = http.get(format!("{endpoint}v1/pairing")).send().await?;
    let status: PairingStatus = client::bounded_json(response).await?;
    if status.version != PROTOCOL_VERSION || !status.open {
        return Ok(Probe::Closed);
    }
    // Never fall back to the version 1 exchange: that is the downgrade an
    // impersonator would ask for.
    let Some(salt) = status.salt.filter(|s| {
        status.pairing == PAIRING_VERSION
            && (16..=128).contains(&s.len())
            && s.bytes().all(|b| b.is_ascii_alphanumeric())
    }) else {
        return Ok(Probe::Outdated);
    };
    let seen = capture
        .seen
        .lock()
        .ok()
        .and_then(|s| *s)
        .context("no certificate observed")?;
    Ok(Probe::Open {
        pin: client::hex_pin(&seen),
        salt,
    })
}

fn endpoint_ip(endpoint: &str) -> Option<IpAddr> {
    match url::Url::parse(endpoint).ok()?.host()? {
        url::Host::Ipv4(ip) => Some(IpAddr::V4(ip)),
        url::Host::Ipv6(ip) => Some(IpAddr::V6(ip)),
        url::Host::Domain(_) => None,
    }
}

/// A `porch pair CODE` failure with the message the tray shows for it.
pub fn failure(code: &str) -> JoinFailure {
    let (code, message, retry) = match code {
        "wrong_code" => (
            code,
            "That code didn't work. Check the code on the other machine.",
            true,
        ),
        "not_waiting" => (
            code,
            "That machine is no longer waiting. Show a new code there.",
            false,
        ),
        "rate_limited" => (
            code,
            "Too many attempts. Wait a minute and try again.",
            false,
        ),
        "certificate_mismatch" => (
            code,
            "The other machine's identity could not be verified. Pairing stopped.",
            false,
        ),
        "host_full" => (
            code,
            "The other machine couldn't save this pairing. Remove an unused machine there.",
            false,
        ),
        "already_paired" => (
            code,
            "A different machine is already paired under the same ID. Unpair the old one, then pair again.",
            false,
        ),
        "no_host" => (
            code,
            "No machine is showing a code. Show one there first, or enter its address.",
            true,
        ),
        "multiple_hosts" => (
            code,
            "More than one machine is waiting. Pick one under Found nearby.",
            true,
        ),
        "unreachable" => (
            code,
            "Couldn't reach that machine. Check it's on the same network or Tailscale.",
            true,
        ),
        "invalid_code" => (
            code,
            "A pairing code is the 6 digits shown on the other machine.",
            true,
        ),
        "local" => (code, "This machine could not get ready to pair.", false),
        _ => (
            "protocol",
            "Pairing could not be confirmed. Check Paired machines on both sides before retrying.",
            false,
        ),
    };
    JoinFailure {
        code: code.into(),
        message: message.into(),
        retry,
        candidates: vec![],
        detail: None,
    }
}
fn fail(code: &str, detail: impl Into<String>) -> anyhow::Error {
    let detail = detail.into();
    JoinFailure {
        detail: (!detail.is_empty()).then_some(detail),
        ..failure(code)
    }
    .into()
}

/// Where to look for the machine showing the code: an explicit `--host`
/// (IP, IP:port, DNS/MagicDNS name or a discovered machine name), otherwise
/// every LAN (mDNS) and Tailscale hint.
async fn candidate_endpoints(host: Option<&str>, port: u16) -> Result<Vec<JoinCandidate>> {
    let mut out: Vec<JoinCandidate> = Vec::new();
    fn push(out: &mut Vec<JoinCandidate>, endpoint: Option<String>, source: Source, name: &str) {
        if let Some(endpoint) = endpoint {
            if !out.iter().any(|x| x.endpoint == endpoint) {
                out.push(JoinCandidate {
                    name: name.into(),
                    endpoint,
                    source,
                });
            }
        }
    }
    let Some(host) = host else {
        let (candidates, warnings) = client::collect_candidates(true).await?;
        for c in candidates {
            push(&mut out, Some(c.endpoint), c.source, &c.name);
        }
        if out.is_empty() {
            for w in warnings {
                eprintln!("Porch: {w}");
            }
        }
        return Ok(out);
    };
    let host = host.trim();
    if let Ok(addr) = host.parse::<SocketAddr>() {
        push(
            &mut out,
            client::endpoint(addr.ip(), addr.port()),
            Source::Explicit,
            host,
        );
    } else if let Ok(ip) = host.trim_matches(['[', ']']).parse::<IpAddr>() {
        push(&mut out, client::endpoint(ip, port), Source::Explicit, host);
    } else {
        let (name, port) = match host.rsplit_once(':') {
            Some((n, p)) if p.parse::<u16>().is_ok() => (n, p.parse().unwrap_or(port)),
            _ => (host, port),
        };
        if let Ok(Ok(addrs)) = tokio::time::timeout(
            Duration::from_secs(3),
            tokio::net::lookup_host((name, port)),
        )
        .await
        {
            for a in addrs {
                push(
                    &mut out,
                    client::endpoint(a.ip(), a.port()),
                    Source::Explicit,
                    name,
                );
            }
        }
        if out.is_empty() {
            let wanted = client::slug(name);
            let (candidates, _) = client::collect_candidates(true).await?;
            for c in candidates
                .into_iter()
                .filter(|c| c.name.eq_ignore_ascii_case(name) || client::slug(&c.name) == wanted)
            {
                push(&mut out, Some(c.endpoint), c.source, &c.name);
            }
        }
    }
    if out.is_empty() {
        return Err(fail(
            "unreachable",
            format!("{host} did not resolve to a private LAN, Tailscale or loopback address"),
        ));
    }
    Ok(out)
}

/// Make sure this machine's agent runs with sharing on, so pairing is mutual.
async fn ensure_agent(bind: Option<&str>, port: u16, peer: Option<IpAddr>) -> Result<ServerConfig> {
    if super::running() {
        if let Some(config) = server::config()? {
            server::set_share_apps(true)?;
            return Ok(ServerConfig {
                share_apps: true,
                ..config
            });
        }
    }
    let ip = match bind {
        Some(b) => server::validate_bind(b)?,
        None => server::auto_bind(peer)?,
    };
    let config = ServerConfig {
        bind: ip.to_string(),
        port,
        advertise: !ip.is_loopback(),
        share_apps: true,
    };
    super::enable(config.clone()).await?;
    Ok(config)
}
/// Sharing as it was before a join turned it on, so a failed join can put it back.
struct Sharing {
    running: bool,
    config: Option<ServerConfig>,
}
impl Sharing {
    fn capture() -> Self {
        Self {
            running: super::running(),
            config: server::config().ok().flatten(),
        }
    }
    async fn restore(self) -> Result<()> {
        if self.running {
            if self.config.is_some_and(|c| !c.share_apps) {
                server::set_share_apps(false)?;
            }
            return Ok(());
        }
        if super::running() {
            super::stop_agent().await?;
        }
        match self.config {
            Some(config) => {
                if server::config()?.is_some_and(|now| {
                    now.bind != config.bind
                        || now.port != config.port
                        || now.advertise != config.advertise
                        || now.share_apps != config.share_apps
                }) {
                    server::configure(config)?;
                }
            }
            None => server::disable()?,
        }
        Ok(())
    }
}

pub struct Shown {
    pub id: String,
    pub code: String,
    pub expires: u64,
    pub machine: Machine,
    pub endpoint: String,
}
/// Host side: start sharing if needed and open a five-minute pairing window.
pub async fn show(bind: Option<&str>, port: u16) -> Result<Shown> {
    let config = ensure_agent(bind, port, None).await?;
    let opened = server::open_pairing()?;
    let ip: IpAddr = config.bind.parse()?;
    Ok(Shown {
        id: opened.id,
        code: opened.code,
        expires: opened.expires,
        machine: server::identity()?,
        endpoint: client::endpoint(ip, config.port).unwrap_or_default(),
    })
}
/// How long the host waits for an accepted peer's first authenticated request.
pub const CONFIRM_GRACE: Duration = Duration::from_secs(60);
pub enum Waited {
    /// The other machine stored the pairing and has connected.
    Paired(String),
    /// The code was accepted but the other machine has not connected yet.
    Unconfirmed(String),
}
/// Block until the window opened by `show` pairs, closes, or Ctrl-C.
pub async fn wait(id: &str) -> Result<Waited> {
    let poll = async {
        let mut accepted: Option<std::time::Instant> = None;
        loop {
            let progress = server::pairing_progress(id)?;
            let name = || {
                progress
                    .peer
                    .as_ref()
                    .map(|p| p.name.clone())
                    .unwrap_or_default()
            };
            match progress.state {
                PairState::Waiting => {}
                PairState::Accepted => {
                    if accepted
                        .get_or_insert_with(std::time::Instant::now)
                        .elapsed()
                        >= CONFIRM_GRACE
                    {
                        return Ok(Waited::Unconfirmed(name()));
                    }
                }
                PairState::Paired => return Ok(Waited::Paired(name())),
                _ => bail!("{}", progress.message()),
            }
            tokio::time::sleep(Duration::from_millis(200)).await
        }
    };
    tokio::select! {
        r = poll => r,
        _ = tokio::signal::ctrl_c() => {
            server::close_pairing(id)?;
            bail!("pairing cancelled; the code no longer works")
        }
    }
}
/// `porch pair status ID`.
pub fn status(id: &str) -> Result<PairStatusReport> {
    Ok(report(id, server::pairing_progress(id)?))
}
/// `porch pair cancel ID`.
pub fn cancel(id: &str) -> Result<PairStatusReport> {
    Ok(report(id, server::close_pairing(id)?))
}
fn report(id: &str, progress: server::PairingProgress) -> PairStatusReport {
    PairStatusReport {
        version: PROTOCOL_VERSION,
        id: id.into(),
        state: progress.state,
        reason: progress.reason,
        expires: progress.expires,
        expires_in: progress
            .expires
            .filter(|_| progress.state == PairState::Waiting)
            .map(|e| e.saturating_sub(server::now())),
        peer: progress.peer,
    }
}

fn normalize_code(code: &str) -> Result<String> {
    let code: String = code
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();
    if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return Err(fail(
            "invalid_code",
            "enter the 6 digits shown by `porch pair` on the other machine",
        ));
    }
    Ok(code)
}

/// Joining side: find the one machine showing a code and pair with it in both
/// directions. The code is a secret, so it is sent to at most one machine:
/// several waiting machines without `--host` is an error, not a race.
pub async fn join(code: &str, host: Option<&str>, bind: Option<&str>, port: u16) -> Result<Peer> {
    let code = normalize_code(code)?;
    let candidates = candidate_endpoints(host, port).await?;
    let own = if super::running() {
        server::fingerprint().ok()
    } else {
        None
    };
    let probes = futures_util::future::join_all(
        candidates
            .iter()
            .map(|c| async move { probe(&c.endpoint).await.map_err(|e| format!("{e:#}")) }),
    )
    .await;
    let (target, pin, salt) = choose(
        candidates.into_iter().zip(probes).collect(),
        own.as_deref(),
        host,
    )?;
    let before = Sharing::capture();
    match join_one(
        &code,
        &salt,
        &target.endpoint,
        target.source,
        &pin,
        bind,
        port,
    )
    .await
    {
        Ok(peer) => Ok(peer),
        Err(e) => {
            // A failed join must not leave app sharing on as a side effect.
            if let Err(r) = before.restore().await {
                eprintln!("Porch: could not restore sharing settings: {r:#}");
            }
            Err(e)
        }
    }
}

/// Turn probe results into the one machine to run the key exchange with, its
/// pin and window salt. Agents that only speak the insecure version 1
/// exchange are never chosen; if they are the only ones showing a code, that
/// is a `protocol` failure asking to update them.
fn choose(
    probed: Vec<(JoinCandidate, std::result::Result<Probe, String>)>,
    own: Option<&str>,
    host: Option<&str>,
) -> Result<(JoinCandidate, String, String)> {
    let mut salts = std::collections::HashMap::new();
    let mut outdated = Vec::new();
    let mut open = false;
    let probed: Vec<_> = probed
        .into_iter()
        .map(|(candidate, probe)| {
            let result = match probe {
                Ok(Probe::Open { pin, salt }) => {
                    open = true;
                    salts.insert(pin.clone(), salt);
                    Ok(Some(pin))
                }
                Ok(Probe::Outdated) => {
                    outdated.push(candidate.endpoint.trim_end_matches('/').to_owned());
                    Ok(None)
                }
                Ok(Probe::Closed) => Ok(None),
                Err(e) => Err(e),
            };
            (candidate, result)
        })
        .collect();
    if !open && !outdated.is_empty() {
        return Err(fail(
            "protocol",
            format!(
                "{} runs an older Porch with an insecure pairing protocol; update Porch on it and show a new code",
                outdated.join(", ")
            ),
        ));
    }
    let (target, pin) = select_target(probed, own, host)?;
    let salt = salts.remove(&pin).unwrap_or_default();
    Ok((target, pin, salt))
}

/// Choose the one machine to send the code to from probe results (its
/// certificate pin when showing a code, `None` when not, or why it failed).
fn select_target(
    probed: Vec<(JoinCandidate, std::result::Result<Option<String>, String>)>,
    own: Option<&str>,
    host: Option<&str>,
) -> Result<(JoinCandidate, String)> {
    let mut open: Vec<(JoinCandidate, String)> = Vec::new();
    let mut reachable = false;
    let mut last_error = None;
    for (candidate, result) in probed {
        let pin = match result {
            Ok(Some(pin)) => pin,
            Ok(None) => {
                reachable = true;
                continue;
            }
            Err(e) => {
                last_error = Some(e);
                continue;
            }
        };
        // One machine reachable over LAN and tailnet presents one certificate.
        if own == Some(pin.as_str()) || open.iter().any(|(_, p)| *p == pin) {
            reachable = true;
            continue;
        }
        open.push((candidate, pin));
    }
    match open.len() {
        1 => Ok(open.remove(0)),
        0 if host.is_none() => Err(fail(
            "no_host",
            "run `porch pair` on the other machine first, or pass --host NAME-OR-IP",
        )),
        0 if reachable => Err(fail(
            "not_waiting",
            format!(
                "{} is not showing a pairing code; run porch pair on it",
                host.unwrap_or_default()
            ),
        )),
        0 => Err(fail("unreachable", last_error.unwrap_or_default())),
        _ => Err(JoinFailure {
            candidates: open.into_iter().map(|(c, _)| c).collect(),
            detail: Some("choose one with --host NAME-OR-IP".into()),
            ..failure("multiple_hosts")
        }
        .into()),
    }
}

/// Map a refused `/v1/pair-code` response. Status first: older agents send plain text.
async fn refused(response: reqwest::Response) -> anyhow::Error {
    let status = response.status();
    let text = String::from_utf8_lossy(&error_body(response).await).into_owned();
    let structured = serde_json::from_str::<PairCodeError>(&text).ok();
    let detail: String = match &structured {
        Some(e) => e.error.message.clone(),
        None => text,
    }
    .chars()
    .filter(|c| !c.is_control())
    .take(200)
    .collect();
    let code = match status.as_u16() {
        401 => "wrong_code",
        410 => "not_waiting",
        429 => "rate_limited",
        409 => match structured.as_ref().map(|e| e.error.code.as_str()) {
            Some("host_full") => "host_full",
            Some("already_paired") => "already_paired",
            None if !detail.contains("certificate") => "host_full",
            // Anything else about the host's identity stops pairing, no bypass.
            _ => "certificate_mismatch",
        },
        _ => "protocol",
    };
    fail(
        code,
        if detail.is_empty() {
            status.to_string()
        } else {
            detail
        },
    )
}

/// An error reply is at most a short JSON message. Read no more than that, so
/// whatever answered can't stream a large body into `porch pair CODE`.
const MAX_ERROR_BODY: usize = 4 * 1024;
async fn error_body(response: reqwest::Response) -> Vec<u8> {
    use futures_util::StreamExt;
    let mut out = Vec::new();
    let mut chunks = response.bytes_stream();
    while let Some(Ok(chunk)) = chunks.next().await {
        let room = MAX_ERROR_BODY - out.len();
        out.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if out.len() >= MAX_ERROR_BODY {
            break;
        }
    }
    out
}

/// Read a successful JSON reply from a pairing step, or map the refusal.
async fn step<T: serde::de::DeserializeOwned>(
    request: reqwest::RequestBuilder,
) -> std::result::Result<T, anyhow::Error> {
    let response = request.send().await.map_err(|e| {
        // reqwest's own message omits the cause (timeout, refused, reset).
        let mut detail = e.to_string();
        let mut source = std::error::Error::source(&e);
        while let Some(cause) = source {
            detail.push_str(&format!(": {cause}"));
            source = cause.source();
        }
        fail(
            "unreachable",
            format!("pairing connection failed: {detail}"),
        )
    })?;
    if !response.status().is_success() {
        return Err(refused(response).await);
    }
    client::bounded_json(response)
        .await
        .map_err(|e| fail("protocol", format!("{e:#}")))
}

/// Run the key exchange with the chosen machine. Nothing that grants access to
/// this machine (sharing, a bearer, a pinned peer) exists until it proved it
/// knows the code for the certificate pinned here.
async fn join_one(
    code: &str,
    salt: &str,
    endpoint: &str,
    source: Source,
    pin: &str,
    bind: Option<&str>,
    port: u16,
) -> Result<Peer> {
    let pin_bytes = client::normalize_pin(pin).map_err(|e| fail("protocol", format!("{e:#}")))?;
    let http = client::http_client(pin).map_err(|e| fail("local", format!("{e:#}")))?;
    let (state, msg) = pake::joiner_start(&pake::password(salt, code));
    let started: PairStarted = step(http.post(format!("{endpoint}v1/pair-code/start")).json(
        &PairStart {
            pairing: PAIRING_VERSION,
            salt: salt.into(),
            msg,
        },
    ))
    .await?;
    let key =
        pake::joiner_finish(state, &started.msg).map_err(|e| fail("protocol", format!("{e:#}")))?;
    let confirmed: PairConfirmed = step(http.post(format!("{endpoint}v1/pair-code/confirm")).json(
        &PairConfirm {
            session: started.session.clone(),
            confirm: key.joiner_confirm(&pin_bytes),
        },
    ))
    .await?;
    if !key.check_host_confirm(&pin_bytes, &confirmed.confirm) {
        return Err(fail(
            "certificate_mismatch",
            format!(
                "{} did not prove it knows the pairing code; it may be impersonating the machine showing it. Nothing was shared",
                endpoint.trim_end_matches('/')
            ),
        ));
    }
    // Authenticated: only now start sharing and issue a credential.
    let config = ensure_agent(bind, port, endpoint_ip(endpoint))
        .await
        .map_err(|e| fail("local", format!("{e:#}")))?;
    let prepared = async {
        let ip: IpAddr = config.bind.parse()?;
        let own_endpoint =
            client::endpoint(ip, config.port).context("invalid local agent address")?;
        let machine = server::identity()?;
        let fingerprint = server::fingerprint()?;
        let bearer = server::mint_client(server::PENDING_LABEL)?;
        anyhow::Ok((own_endpoint, machine, fingerprint, bearer))
    }
    .await;
    let (own_endpoint, machine, fingerprint, bearer) =
        prepared.map_err(|e| fail("local", format!("{e:#}")))?;
    let own_id = machine.id.clone();
    let result = async {
        let request = PairFinish {
            session: started.session,
            mac: key.joiner_payload(&machine, &own_endpoint, &fingerprint, &bearer),
            machine,
            endpoint: own_endpoint,
            fingerprint,
            token: bearer.clone(),
        };
        let response: PairFinished = step(
            http.post(format!("{endpoint}v1/pair-code/finish"))
                .json(&request),
        )
        .await?;
        if response.version != PROTOCOL_VERSION {
            return Err(fail("protocol", "unsupported peer protocol version"));
        }
        client::validate_machine(&response.machine)
            .map_err(|e| fail("protocol", format!("{e:#}")))?;
        if !(32..=512).contains(&response.token.len())
            || !response
                .token
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
        {
            return Err(fail("protocol", "invalid enrollment response token"));
        }
        if !key.check_host_payload(&response.machine, &response.token, &response.mac) {
            return Err(fail("protocol", "pairing response failed authentication"));
        }
        let mut sources = vec![Source::Explicit];
        if !sources.contains(&source) {
            sources.push(source);
        }
        let peer = Peer {
            machine: response.machine,
            endpoint: endpoint.into(),
            fingerprint: pin.into(),
            token: response.token,
            sources,
            last_seen: server::now(),
            slug: String::new(),
        };
        crate::state::load()?;
        let paired = client::peers_in(&crate::state::dir()).unwrap_or_default();
        if let Some(e) = refuse_host(&own_id, &paired, &peer) {
            // It already stored this machine: have it forget it again.
            let _ =
                tokio::time::timeout(Duration::from_secs(5), client::unpair_remote(&peer)).await;
            return Err(e);
        }
        // The host accepted the code: from here on, store both sides.
        server::label_client(&bearer, &peer.machine.id)?;
        client::upsert_peer_in(&crate::state::dir(), peer).map_err(|e| {
            let code = if e.is::<client::IdentityConflict>() {
                "already_paired"
            } else {
                "protocol"
            };
            fail(code, format!("{e:#}"))
        })
    }
    .await;
    let peer = match result {
        Ok(peer) => peer,
        Err(e) => {
            let _ = server::revoke_bearer(&bearer);
            return Err(e);
        }
    };
    // The first authenticated request tells the host this side stored the
    // pairing. If it fails now, the next catalog refresh confirms instead.
    if let Err(e) = client::fetch_catalog(&peer, endpoint).await {
        eprintln!(
            "Porch: paired, but {} has not confirmed yet: {e:#}",
            peer.machine.name
        );
    }
    Ok(peer)
}

pub struct Unpaired {
    pub peer: Peer,
    /// Why the other machine was not reached, if it wasn't.
    pub unreached: Option<String>,
    /// No credential here is recorded as the other machine's, but some aren't
    /// recorded as anyone's, so it may still have access.
    pub access_unknown: bool,
}
/// The host knows the code, but its identity is only its word: it can't be
/// this machine, or take over another paired machine's.
fn refuse_host(own_id: &str, paired: &[Peer], host: &Peer) -> Option<anyhow::Error> {
    if host.machine.id == own_id {
        return Some(fail(
            "protocol",
            format!(
                "{} claims this machine's own ID",
                host.endpoint.trim_end_matches('/')
            ),
        ));
    }
    client::identity_conflict(paired, &host.machine.id, &host.fingerprint)
        .map(|c| fail("already_paired", c.to_string()))
}

/// Forget a machine: revoke its access here first, then tell it (best effort,
/// five seconds).
pub async fn unpair(reference: &str) -> Result<Unpaired> {
    let peer = client::find_peer(reference)?;
    let revoked = server::revoke_machine(&peer.machine.id)?;
    super::disconnect_peer(&peer.machine.id).await?;
    client::remove_peer(&peer.machine.id)?;
    let unreached =
        match tokio::time::timeout(Duration::from_secs(5), client::unpair_remote(&peer)).await {
            Ok(Ok(())) => None,
            // The alternate form includes the cause (timeout, refused, reset),
            // which reqwest's own message omits.
            Ok(Err(e)) => Some(format!("{e:#}")),
            Err(_) => Some("no answer within 5 seconds".into()),
        };
    Ok(Unpaired {
        peer,
        unreached,
        access_unknown: revoked.revoked == 0 && revoked.unidentified > 0,
    })
}

/// The browser URL template for a paired machine's apps.
pub fn url_template(slug: &str) -> String {
    let settings = crate::state::proxy_settings().unwrap_or_default();
    settings.url(&format!("APP.{slug}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn at(name: &str, ip: &str) -> JoinCandidate {
        JoinCandidate {
            name: name.into(),
            endpoint: format!("https://{ip}:7463/"),
            source: Source::Lan,
        }
    }
    fn code_of(e: anyhow::Error) -> JoinFailure {
        e.downcast::<JoinFailure>().expect("structured failure")
    }
    fn open(pin: char) -> std::result::Result<Probe, String> {
        Ok(Probe::Open {
            pin: pin.to_string().repeat(64),
            salt: format!("salt{pin}"),
        })
    }
    #[test]
    fn outdated_agents_are_never_chosen_and_alone_are_a_protocol_failure() {
        let e = choose(
            vec![
                (at("old", "10.0.0.2"), Ok(Probe::Outdated)),
                (at("idle", "10.0.0.3"), Ok(Probe::Closed)),
            ],
            None,
            None,
        )
        .err()
        .unwrap();
        let e = code_of(e);
        assert_eq!(e.code, "protocol");
        assert!(e.detail.unwrap().contains("https://10.0.0.2:7463"));
        // Mixed versions: the current agent is chosen with its own salt.
        let (target, pin, salt) = choose(
            vec![
                (at("old", "10.0.0.2"), Ok(Probe::Outdated)),
                (at("new", "10.0.0.4"), open('b')),
            ],
            None,
            None,
        )
        .unwrap();
        assert_eq!(target.name, "new");
        assert_eq!(pin, "b".repeat(64));
        assert_eq!(salt, "saltb");
        // Two current agents stay ambiguous, outdated ones aside.
        let e = choose(
            vec![
                (at("old", "10.0.0.2"), Ok(Probe::Outdated)),
                (at("one", "10.0.0.4"), open('b')),
                (at("two", "10.0.0.5"), open('c')),
            ],
            None,
            None,
        )
        .err()
        .unwrap();
        let e = code_of(e);
        assert_eq!(e.code, "multiple_hosts");
        assert_eq!(e.candidates.len(), 2);
        // This machine's own agent is never a target.
        let e = choose(
            vec![(at("me", "10.0.0.4"), open('b'))],
            Some(&"b".repeat(64)),
            Some("10.0.0.4"),
        )
        .err()
        .unwrap();
        assert_eq!(code_of(e).code, "not_waiting");
    }
    #[test]
    fn the_code_goes_to_exactly_one_waiting_machine() {
        let pin = |c: char| Ok(Some(c.to_string().repeat(64)));
        // Two different machines showing codes: refuse instead of racing them.
        let e = select_target(
            vec![
                (at("mini", "192.168.1.20"), pin('a')),
                (at("rogue", "192.168.1.66"), pin('b')),
            ],
            None,
            None,
        )
        .unwrap_err();
        let e = code_of(e);
        assert_eq!(e.code, "multiple_hosts");
        assert_eq!(e.candidates.len(), 2);
        assert!(e.to_string().contains("rogue"));
        // Even an explicit --host that matched two machines is refused.
        let e = select_target(
            vec![
                (at("mini", "192.168.1.20"), pin('a')),
                (at("mini", "192.168.1.21"), pin('b')),
            ],
            None,
            Some("mini"),
        )
        .unwrap_err();
        assert_eq!(code_of(e).code, "multiple_hosts");
        // One machine over LAN and Tailscale (same certificate) is one target;
        // this machine's own agent and closed or failing probes are skipped.
        let (target, chosen) = select_target(
            vec![
                (at("self", "192.168.1.2"), pin('0')),
                (at("mini", "192.168.1.20"), pin('a')),
                (at("mini", "100.64.0.20"), pin('a')),
                (at("closed", "192.168.1.30"), Ok(None)),
                (at("gone", "192.168.1.40"), Err("timed out".into())),
            ],
            Some(&"0".repeat(64)),
            None,
        )
        .unwrap();
        assert_eq!(target.endpoint, "https://192.168.1.20:7463/");
        assert_eq!(chosen, "a".repeat(64));
    }
    #[test]
    fn nothing_waiting_is_explained() {
        let none = select_target(vec![(at("x", "10.0.0.2"), Ok(None))], None, None);
        assert_eq!(code_of(none.unwrap_err()).code, "no_host");
        let closed = select_target(
            vec![(at("x", "10.0.0.2"), Ok(None))],
            None,
            Some("10.0.0.2"),
        );
        assert_eq!(code_of(closed.unwrap_err()).code, "not_waiting");
        let down = select_target(
            vec![(at("x", "10.0.0.2"), Err("refused".into()))],
            None,
            Some("10.0.0.2"),
        );
        let down = code_of(down.unwrap_err());
        assert_eq!(down.code, "unreachable");
        assert!(down.retry);
    }
    fn host(id: &str, pin: char) -> Peer {
        Peer {
            machine: Machine {
                id: id.into(),
                name: "mini".into(),
            },
            endpoint: "https://10.0.0.2:7463/".into(),
            fingerprint: pin.to_string().repeat(64),
            token: "t".repeat(64),
            sources: vec![Source::Explicit],
            last_seen: 0,
            slug: "mini".into(),
        }
    }
    #[test]
    fn a_host_cannot_claim_this_machine_or_another_paired_one() {
        let paired = vec![host("mini-id", 'a')];
        // Claiming the joiner's own ID.
        let e = refuse_host("me", &paired, &host("me", 'b')).unwrap();
        assert_eq!(code_of(e).code, "protocol");
        // Claiming a paired machine's ID with another certificate.
        let e = refuse_host("me", &paired, &host("mini-id", 'b')).unwrap();
        let e = code_of(e);
        assert_eq!(e.code, "already_paired");
        assert!(e.detail.unwrap().contains("porch unpair mini-id"));
        // The same machine pairing again, or a new one, is fine.
        assert!(refuse_host("me", &paired, &host("mini-id", 'a')).is_none());
        assert!(refuse_host("me", &paired, &host("new-id", 'b')).is_none());
    }
    #[tokio::test]
    async fn refusals_read_a_bounded_body() {
        let huge = format!(
            r#"{{"error":{{"code":"host_full","message":"{}"}}}}"#,
            "x".repeat(1024 * 1024)
        );
        let response: reqwest::Response = hyper::http::Response::builder()
            .status(409)
            .body(huge.clone())
            .unwrap()
            .into();
        assert_eq!(error_body(response).await.len(), MAX_ERROR_BODY);
        // Too long to parse, so mapped by status, with a short detail.
        let response: reqwest::Response = hyper::http::Response::builder()
            .status(429)
            .body(huge)
            .unwrap()
            .into();
        let e = code_of(refused(response).await);
        assert_eq!(e.code, "rate_limited");
        assert!(e.detail.unwrap().len() <= 200);
        let ok: reqwest::Response = hyper::http::Response::builder()
            .status(409)
            .body(r#"{"error":{"code":"already_paired","message":"m"}}"#.to_owned())
            .unwrap()
            .into();
        assert_eq!(code_of(refused(ok).await).code, "already_paired");
    }
    #[test]
    fn codes_are_normalized_and_failures_have_stable_codes() {
        assert_eq!(normalize_code(" 012-345 ").unwrap(), "012345");
        assert_eq!(normalize_code("012 345").unwrap(), "012345");
        assert_eq!(
            code_of(normalize_code("12345").unwrap_err()).code,
            "invalid_code"
        );
        assert_eq!(failure("anything else").code, "protocol");
        assert!(failure("wrong_code").retry);
        assert!(!failure("certificate_mismatch").retry);
    }
}
