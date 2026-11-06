//! Code pairing. `porch pair` shows a six-digit code; `porch pair CODE` on the
//! other machine finds it, pins its certificate (trust on first use, bound to
//! the code) and both machines enroll each other in one exchange.
use super::{client, server, types::*};
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

/// Returns the agent's certificate fingerprint when it is showing a pairing code.
async fn probe(endpoint: &str) -> Result<Option<String>> {
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
        return Ok(None);
    }
    let seen = capture
        .seen
        .lock()
        .ok()
        .and_then(|s| *s)
        .context("no certificate observed")?;
    Ok(Some(client::hex_pin(&seen)))
}

fn endpoint_ip(endpoint: &str) -> Option<IpAddr> {
    match url::Url::parse(endpoint).ok()?.host()? {
        url::Host::Ipv4(ip) => Some(IpAddr::V4(ip)),
        url::Host::Ipv6(ip) => Some(IpAddr::V6(ip)),
        url::Host::Domain(_) => None,
    }
}

/// Where to look for the machine showing the code: an explicit `--host`
/// (IP, IP:port, DNS/MagicDNS name or a discovered machine name), otherwise
/// every LAN (mDNS) and Tailscale hint.
async fn candidate_endpoints(host: Option<&str>, port: u16) -> Result<Vec<(String, Source)>> {
    let mut out: Vec<(String, Source)> = Vec::new();
    fn push(out: &mut Vec<(String, Source)>, endpoint: Option<String>, source: Source) {
        if let Some(e) = endpoint {
            if !out.iter().any(|(x, _)| *x == e) {
                out.push((e, source));
            }
        }
    }
    let Some(host) = host else {
        let (candidates, warnings) = client::collect_candidates(true).await?;
        for c in candidates {
            push(&mut out, Some(c.endpoint), c.source);
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
        );
    } else if let Ok(ip) = host.trim_matches(['[', ']']).parse::<IpAddr>() {
        push(&mut out, client::endpoint(ip, port), Source::Explicit);
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
                push(&mut out, Some(c.endpoint), c.source);
            }
        }
    }
    if out.is_empty() {
        bail!("{host} did not resolve to a private LAN, Tailscale or loopback address")
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
/// Block until the window opened by `show` pairs, closes, or Ctrl-C.
pub async fn wait(id: &str) -> Result<String> {
    let poll = async {
        loop {
            match server::pairing_progress(id)? {
                server::PairingProgress::Waiting => {
                    tokio::time::sleep(Duration::from_millis(200)).await
                }
                server::PairingProgress::Paired(name) => return Ok(name),
                server::PairingProgress::Closed(reason) => bail!("{reason}"),
            }
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

fn normalize_code(code: &str) -> Result<String> {
    let code: String = code
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();
    if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
        bail!("a pairing code is the 6 digits shown by `porch pair` on the other machine")
    }
    Ok(code)
}

/// Joining side: find the machine showing `code`, pair with it in both directions.
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
            .map(|(endpoint, _)| async move { probe(endpoint).await.ok().flatten() }),
    )
    .await;
    let mut open: Vec<(String, Source, String)> = Vec::new();
    for ((endpoint, source), pin) in candidates.into_iter().zip(probes) {
        let Some(pin) = pin else { continue };
        // One machine reachable over LAN and tailnet presents one certificate.
        if own.as_ref() == Some(&pin) || open.iter().any(|(_, _, p)| *p == pin) {
            continue;
        }
        open.push((endpoint, source, pin));
    }
    if open.is_empty() {
        bail!(
            "no machine is showing a pairing code{}. Run `porch pair` on the other machine first{}",
            host.map(|h| format!(" at {h}")).unwrap_or_default(),
            if host.is_some() {
                ""
            } else {
                ", or pass --host NAME-OR-IP if it is not discovered automatically"
            }
        )
    }
    let mut errors = Vec::new();
    for (endpoint, source, pin) in open {
        match join_one(&code, &endpoint, source, &pin, bind, port).await {
            Ok(peer) => return Ok(peer),
            Err(e) => errors.push(format!("{e:#}")),
        }
    }
    bail!("{}", errors.join("; "))
}

async fn join_one(
    code: &str,
    endpoint: &str,
    source: Source,
    pin: &str,
    bind: Option<&str>,
    port: u16,
) -> Result<Peer> {
    let config = ensure_agent(bind, port, endpoint_ip(endpoint)).await?;
    let ip: IpAddr = config.bind.parse()?;
    let own_endpoint = client::endpoint(ip, config.port).context("invalid local agent address")?;
    let bearer = server::mint_client("pending")?;
    let request = PairCodeRequest {
        version: PROTOCOL_VERSION,
        code: code.into(),
        host_fingerprint: pin.into(),
        machine: server::identity()?,
        endpoint: own_endpoint,
        fingerprint: server::fingerprint()?,
        token: bearer.clone(),
    };
    let result = async {
        let response = client::http_client(pin)?
            .post(format!("{endpoint}v1/pair-code"))
            .json(&request)
            .send()
            .await
            .context("pairing connection failed")?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            let text: String = text.chars().filter(|c| !c.is_control()).take(200).collect();
            bail!(
                "{}",
                if text.is_empty() {
                    status.to_string()
                } else {
                    text
                }
            )
        }
        let response: PairResponse = client::bounded_json(response).await?;
        if response.version != PROTOCOL_VERSION {
            bail!("unsupported peer protocol version")
        }
        client::validate_machine(&response.machine)?;
        if !(32..=512).contains(&response.token.len())
            || !response
                .token
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
        {
            bail!("invalid enrollment response token")
        }
        Ok(response)
    }
    .await;
    let response = match result {
        Ok(r) => r,
        Err(e) => {
            let _ = server::revoke_bearer(&bearer);
            return Err(e);
        }
    };
    server::label_client(&bearer, &response.machine.id)?;
    let mut sources = vec![Source::Explicit];
    if !sources.contains(&source) {
        sources.push(source);
    }
    crate::state::load()?;
    client::upsert_peer_in(
        &crate::state::dir(),
        Peer {
            machine: response.machine,
            endpoint: endpoint.into(),
            fingerprint: pin.into(),
            token: response.token,
            sources,
            last_seen: server::now(),
            slug: String::new(),
        },
    )
}

/// Forget a machine on both sides when it is reachable, and locally always.
pub async fn unpair(reference: &str) -> Result<(Peer, bool)> {
    let peer = client::find_peer(reference)?;
    let remote = async {
        client::http_client(&peer.fingerprint)?
            .post(format!("{}v1/unpair", peer.endpoint))
            .bearer_auth(&peer.token)
            .send()
            .await?
            .error_for_status()?;
        anyhow::Ok(())
    };
    let reached = matches!(
        tokio::time::timeout(Duration::from_secs(5), remote).await,
        Ok(Ok(()))
    );
    server::revoke_machine(&peer.machine.id)?;
    super::disconnect_peer(&peer.machine.id).await?;
    client::remove_peer(&peer.machine.id)?;
    Ok((peer, reached))
}

/// The browser URL template for a paired machine's apps.
pub fn url_template(slug: &str) -> String {
    let settings = crate::state::proxy_settings().unwrap_or_default();
    settings.url(&format!("APP.{slug}"))
}
