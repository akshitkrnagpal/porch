//! Authenticated peer browsing, enrollment and loopback browser transports.
//!
//! Discovery records are untrusted hints. They never confer trust and are never
//! used to fetch an app URL. Only explicitly pinned peers can be queried or relayed.
use super::types::*;
use anyhow::{bail, Context, Result};
use axum::body::Body;
use futures_util::{stream, StreamExt};
use hyper::{body::Incoming, header, Request, Response, StatusCode};
use hyper_util::{
    client::legacy::Client,
    rt::{TokioExecutor, TokioIo, TokioTimer},
};
use rustls::{
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
    DigitallySignedStruct, SignatureScheme,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    convert::Infallible,
    fs,
    io::Write,
    net::{IpAddr, SocketAddr},
    process::Stdio,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::io::AsyncReadExt;
use url::Url;

const PEERS_FILE: &str = "discovery-peers.json";
const CACHE_FILE: &str = "discovery-candidates.json";
const MAX_PEERS: usize = 128;
const MAX_CANDIDATES: usize = 256;
const CANDIDATE_TTL: u64 = 90;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(4);

type PinnedClient =
    Client<hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>, Body>;
/// Connections to one paired agent. Requests share a single HTTP/2 connection
/// when the agent offers it, so a page's hundreds of module requests neither
/// open a TLS connection each nor run into the agent's connection limit.
/// WebSocket upgrades need HTTP/1.1 and use their own connections.
#[derive(Clone)]
pub(super) struct RelayClient {
    requests: PinnedClient,
    upgrades: PinnedClient,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Candidate URLs cannot trigger DNS lookups, redirects or access to public hosts.
/// Loopback permits explicit isolated two-node setups and SSH-forwarded agents.
pub fn validate_endpoint(value: &str) -> Result<Url> {
    let u = Url::parse(value).context("peer endpoint must be an HTTPS literal-IP URL")?;
    if u.scheme() != "https"
        || !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
        || u.path() != "/"
        || u.port_or_known_default() == Some(0)
    {
        bail!("peer endpoint must be https://PRIVATE-IP:PORT with no path or credentials");
    }
    let ip = match u.host() {
        Some(url::Host::Ipv4(ip)) => IpAddr::V4(ip),
        Some(url::Host::Ipv6(ip)) => IpAddr::V6(ip),
        _ => bail!("peer endpoint must use a literal private LAN, tailnet or loopback address"),
    };
    if !allowed_ip(ip) {
        bail!("peer endpoint address is outside private LAN/tailnet/loopback ranges");
    }
    Ok(u)
}
pub(super) fn allowed_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, _, _] = ip.octets();
            ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || (a == 100 && (64..=127).contains(&b))
        }
        IpAddr::V6(ip) => ip.is_loopback() || ip.octets()[0] & 0xfe == 0xfc,
    }
}
pub(super) fn endpoint(ip: IpAddr, port: u16) -> Option<String> {
    if !allowed_ip(ip) || port == 0 {
        return None;
    }
    Some(format!("https://{}/", SocketAddr::new(ip, port)))
}
pub(super) fn normalize_pin(pin: &str) -> Result<[u8; 32]> {
    let p: String = pin.chars().filter(|c| *c != ':').collect();
    if p.len() != 64 || !p.bytes().all(|c| c.is_ascii_hexdigit()) {
        bail!("certificate fingerprint must contain 64 SHA-256 hex digits");
    }
    let mut bytes = [0; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&p[i * 2..i * 2 + 2], 16)?;
    }
    Ok(bytes)
}
pub(super) fn hex_pin(pin: &[u8; 32]) -> String {
    pin.iter().map(|v| format!("{v:02x}")).collect()
}

#[derive(Debug)]
struct PinnedCertificate {
    fingerprint: [u8; 32],
    provider: Arc<rustls::crypto::CryptoProvider>,
}
impl ServerCertVerifier for PinnedCertificate {
    fn verify_server_cert(
        &self,
        cert: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        let actual: [u8; 32] = Sha256::digest(cert.as_ref()).into();
        if actual != self.fingerprint {
            return Err(rustls::Error::General("Porch peer certificate fingerprint changed; enroll explicitly after verifying the new fingerprint".into()));
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
pub(super) fn tls_config(pin: &str) -> Result<rustls::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(PinnedCertificate {
        fingerprint: normalize_pin(pin)?,
        provider: provider.clone(),
    });
    Ok(rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth())
}
pub(super) fn http_client(pin: &str) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .use_preconfigured_tls(tls_config(pin)?)
        .https_only(true)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(2))
        .timeout(REQUEST_TIMEOUT)
        .build()?)
}
pub(super) async fn bounded_json<T: DeserializeOwned>(response: reqwest::Response) -> Result<T> {
    if !response.status().is_success() {
        bail!(
            "peer rejected request (HTTP {}); check enrollment and token",
            response.status().as_u16()
        );
    }
    if response
        .content_length()
        .is_some_and(|n| n > MAX_BODY as u64)
    {
        bail!("peer response exceeds protocol size limit");
    }
    let mut data = Vec::new();
    let mut chunks = response.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.context("reading peer response")?;
        if data.len() + chunk.len() > MAX_BODY {
            bail!("peer response exceeds protocol size limit");
        }
        data.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&data).context("invalid peer protocol response")
}
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}
pub(super) fn validate_machine(machine: &Machine) -> Result<()> {
    if !valid_id(&machine.id)
        || machine.name.is_empty()
        || machine.name.len() > 256
        || machine.name.chars().any(char::is_control)
    {
        bail!("invalid peer machine identity");
    }
    Ok(())
}
fn validate_catalog(catalog: &Catalog, expected_id: &str) -> Result<()> {
    validate_machine(&catalog.machine)?;
    if catalog.version != PROTOCOL_VERSION
        || catalog.machine.id != expected_id
        || catalog.apps.len() > 1024
    {
        bail!("peer catalog version or machine identity mismatch");
    }
    let mut ids = std::collections::HashSet::new();
    for app in &catalog.apps {
        if !valid_id(&app.id)
            || !ids.insert(&app.id)
            || app.name.is_empty()
            || app.name.len() > 512
            || app.name.chars().any(char::is_control)
        {
            bail!("invalid or duplicate peer app identity");
        }
        let u = Url::parse(&app.local_url).context("invalid app URL in peer catalog")?;
        if !matches!(u.scheme(), "http" | "https")
            || u.host().is_none()
            || !u.username().is_empty()
            || u.password().is_some()
            || app.local_url.len() > 4096
        {
            bail!("invalid app URL in peer catalog");
        }
    }
    Ok(())
}
fn read_json<T: DeserializeOwned + Default>(file: &str) -> Result<T> {
    read_json_in(&crate::state::dir(), file)
}
fn read_json_in<T: DeserializeOwned + Default>(dir: &std::path::Path, file: &str) -> Result<T> {
    match fs::read(dir.join(file)) {
        Ok(bytes) if bytes.len() <= MAX_BODY => {
            serde_json::from_slice(&bytes).with_context(|| format!("invalid {file}"))
        }
        Ok(_) => bail!("{file} exceeds size limit"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e.into()),
    }
}
/// A short hostname label for a machine name: `Akshits-Mac-mini.local` -> `akshits-mac-mini`.
pub fn slug(name: &str) -> String {
    let base = name.split('.').next().unwrap_or(name);
    let mut out = String::new();
    for c in base.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out: String = out.trim_matches('-').chars().take(40).collect();
    let out = out.trim_end_matches('-').to_owned();
    if out.is_empty() {
        "peer".into()
    } else {
        out
    }
}
/// Assign missing slugs in file (pairing) order. Existing slugs never change,
/// so URLs stay stable; a later machine with the same name becomes `name-2`.
fn assign_slugs(peers: &mut [Peer]) {
    for p in peers.iter_mut() {
        if p.slug.is_empty()
            || p.slug.len() > 63
            || !p
                .slug
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        {
            p.slug.clear();
        }
    }
    let reserved: Vec<String> = peers
        .iter()
        .filter(|p| !p.slug.is_empty())
        .map(|p| p.slug.clone())
        .collect();
    let mut taken = std::collections::HashSet::new();
    for p in peers.iter_mut() {
        if !p.slug.is_empty() && taken.insert(p.slug.clone()) {
            continue;
        }
        let base = slug(&p.machine.name);
        let mut candidate = base.clone();
        let mut n = 2;
        while taken.contains(&candidate) || reserved.contains(&candidate) {
            candidate = format!("{base}-{n}");
            n += 1;
        }
        taken.insert(candidate.clone());
        p.slug = candidate;
    }
}
pub fn peers() -> Result<Vec<Peer>> {
    peers_in(&crate::state::dir())
}
pub(super) fn peers_in(dir: &std::path::Path) -> Result<Vec<Peer>> {
    let mut peers: Vec<Peer> = read_json_in(dir, PEERS_FILE)?;
    assign_slugs(&mut peers);
    if peers.len() > MAX_PEERS {
        bail!("too many enrolled peers");
    }
    for p in &peers {
        validate_endpoint(&p.endpoint)?;
        normalize_pin(&p.fingerprint)?;
        validate_machine(&p.machine)?;
        if p.token.len() < 32
            || p.token.len() > 512
            || !p
                .token
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
        {
            bail!("invalid enrolled peer token");
        }
    }
    Ok(peers)
}
fn update_peers(f: impl FnOnce(&mut Vec<Peer>) -> Result<()>) -> Result<()> {
    crate::state::load()?;
    update_peers_in(&crate::state::dir(), f)
}
fn update_peers_in(
    dir: &std::path::Path,
    f: impl FnOnce(&mut Vec<Peer>) -> Result<()>,
) -> Result<()> {
    use fs2::FileExt;
    fs::create_dir_all(dir)?;
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(false).write(true).read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let lock = options.open(dir.join("discovery-peers.lock"))?;
    lock.lock_exclusive()?;
    let mut all = peers_in(dir)?;
    f(&mut all)?;
    if all.len() > MAX_PEERS {
        bail!("peer enrollment limit reached; remove an unused peer first");
    }
    assign_slugs(&mut all);
    super::server::write(dir, PEERS_FILE, &serde_json::to_vec_pretty(&all)?)
}
/// Insert or replace a peer by machine ID, keeping an existing slug.
pub(super) fn upsert_peer_in(dir: &std::path::Path, mut peer: Peer) -> Result<Peer> {
    let mut stored = None;
    update_peers_in(dir, |all| {
        if let Some(old) = all.iter_mut().find(|p| p.machine.id == peer.machine.id) {
            peer.slug = old.slug.clone();
            *old = peer.clone();
        } else {
            peer.slug = String::new();
            all.push(peer.clone());
        }
        Ok(())
    })?;
    for p in peers_in(dir)? {
        if p.machine.id == peer.machine.id {
            stored = Some(p);
        }
    }
    stored.context("peer was not stored")
}
pub(super) fn remove_peer_in(dir: &std::path::Path, id: &str) -> Result<()> {
    update_peers_in(dir, |all| {
        all.retain(|p| p.machine.id != id);
        Ok(())
    })
}
/// Resolve a user-supplied machine reference: ID, exact name or URL slug.
pub fn find_peer(reference: &str) -> Result<Peer> {
    let all = peers()?;
    let wanted = reference.to_ascii_lowercase();
    let matches: Vec<_> = all
        .iter()
        .filter(|p| {
            p.machine.id == reference
                || p.machine.name.eq_ignore_ascii_case(reference)
                || p.slug == wanted
        })
        .collect();
    match matches.as_slice() {
        [one] => Ok((*one).clone()),
        [] => bail!("no paired machine named {reference}; see porch peers list"),
        _ => bail!("{reference} matches several paired machines; use its ID from porch peers list"),
    }
}
pub fn remove_peer(id: &str) -> Result<()> {
    update_peers(|all| {
        let old = all.len();
        all.retain(|p| p.machine.id != id);
        if all.len() == old {
            bail!("peer is not enrolled");
        }
        Ok(())
    })
}
/// Both the fingerprint and the single-use pairing token must be transferred
/// through a channel the user already trusts. Discovery does not supply either.
pub async fn pair(endpoint: &str, fingerprint: &str, token: &str) -> Result<Peer> {
    let endpoint = validate_endpoint(endpoint)?.to_string();
    let pin = hex_pin(&normalize_pin(fingerprint)?);
    if token.len() < 32
        || token.len() > 512
        || !token
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
    {
        bail!("invalid pairing token");
    }
    let response = http_client(&pin)?
        .post(format!("{endpoint}v1/pair"))
        .json(&PairRequest {
            token: token.into(),
        })
        .send()
        .await
        .context("pairing connection failed (verify address and certificate fingerprint)")?;
    let response: PairResponse = bounded_json(response).await?;
    if response.version != PROTOCOL_VERSION {
        bail!("unsupported peer protocol version");
    }
    validate_machine(&response.machine)?;
    if response.token.len() < 32
        || response.token.len() > 512
        || !response
            .token
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
    {
        bail!("invalid enrollment response token");
    }
    let peer = Peer {
        machine: response.machine,
        endpoint,
        fingerprint: pin,
        token: response.token,
        sources: vec![Source::Explicit],
        last_seen: now(),
        slug: String::new(),
    };
    crate::state::load()?;
    upsert_peer_in(&crate::state::dir(), peer)
}
pub(super) async fn fetch_catalog(peer: &Peer, endpoint: &str) -> Result<Catalog> {
    if let Some(shared) = super::gateway::fetch_shared(peer, endpoint).await {
        return shared;
    }
    let endpoint = validate_endpoint(endpoint)?;
    let response = http_client(&peer.fingerprint)?
        .get(endpoint.join("v1/catalog")?)
        .bearer_auth(&peer.token)
        .send()
        .await
        .context("peer is unreachable or its pinned certificate does not match")?;
    let catalog: Catalog = bounded_json(response).await?;
    validate_catalog(&catalog, &peer.machine.id)?;
    Ok(catalog)
}

/// The catalog over the relay's pooled connection: no TLS handshake when the
/// relay is already connected, and the connection it opens serves the page next.
pub(super) async fn relay_catalog(peer: &Peer, client: &RelayClient) -> Result<Catalog> {
    use http_body_util::BodyExt;
    let uri: hyper::Uri = validate_endpoint(&peer.endpoint)?
        .join("v1/catalog")?
        .as_str()
        .parse()?;
    let request = Request::get(uri)
        .header(header::AUTHORIZATION, format!("Bearer {}", peer.token))
        .body(Body::empty())?;
    let fetch = async {
        let response = client
            .requests
            .request(request)
            .await
            .context("peer is unreachable or its pinned certificate does not match")?;
        if !response.status().is_success() {
            bail!(
                "peer rejected request (HTTP {}); check enrollment and token",
                response.status().as_u16()
            );
        }
        let body = http_body_util::Limited::new(response.into_body(), MAX_BODY)
            .collect()
            .await
            .map_err(|e| anyhow::anyhow!("reading peer response: {e}"))?
            .to_bytes();
        let catalog: Catalog =
            serde_json::from_slice(&body).context("invalid peer protocol response")?;
        validate_catalog(&catalog, &peer.machine.id)?;
        Ok(catalog)
    };
    tokio::time::timeout(REQUEST_TIMEOUT, fetch)
        .await
        .context("peer catalog request timed out")?
}

#[derive(Default, Serialize, Deserialize)]
struct CandidateCache {
    entries: BTreeMap<String, Vec<Candidate>>,
}
impl CandidateCache {
    fn expire(&mut self, timestamp: u64) {
        self.entries.retain(|_, all| {
            all.retain(|c| {
                timestamp.saturating_sub(c.last_seen) <= CANDIDATE_TTL
                    && validate_endpoint(&c.endpoint).is_ok()
            });
            !all.is_empty()
        });
    }
    fn insert(&mut self, key: String, mut candidates: Vec<Candidate>) {
        candidates.truncate(8);
        if self.entries.len() < MAX_CANDIDATES || self.entries.contains_key(&key) {
            self.entries.insert(key, candidates);
        }
    }
    fn remove(&mut self, key: &str) {
        self.entries.remove(key);
    }
    fn all(&self) -> Vec<Candidate> {
        self.entries
            .values()
            .flatten()
            .take(MAX_CANDIDATES)
            .cloned()
            .collect()
    }
}
fn browse_lan(mut cache: CandidateCache) -> Result<CandidateCache> {
    use mdns_sd::{ServiceDaemon, ServiceEvent};
    let daemon = ServiceDaemon::new().context("mDNS browsing unavailable")?;
    let events = daemon.browse(SERVICE_TYPE)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) {
        let Ok(event) = events.recv_timeout(remaining) else {
            break;
        };
        match event {
            ServiceEvent::ServiceResolved(info) => {
                if info
                    .get_property_val_str("version")
                    .or_else(|| info.get_property_val_str("v"))
                    != Some("1")
                {
                    continue;
                }
                let Some(id) = info.get_property_val_str("id").filter(|id| valid_id(id)) else {
                    continue;
                };
                let candidates = info
                    .get_addresses()
                    .iter()
                    .filter_map(|ip| endpoint(ip.to_ip_addr(), info.get_port()))
                    .map(|endpoint| Candidate {
                        machine_id: Some(id.into()),
                        name: format!("Porch {}", &id[..id.len().min(12)]),
                        endpoint,
                        source: Source::Lan,
                        last_seen: now(),
                        node: None,
                    })
                    .collect();
                cache.insert(info.get_fullname().into(), candidates);
            }
            ServiceEvent::ServiceRemoved(_, name) => cache.remove(&name),
            _ => {}
        }
    }
    let _ = daemon.stop_browse(SERVICE_TYPE);
    let _ = daemon.shutdown();
    cache.expire(now());
    Ok(cache)
}
/// Parse only online, non-self Tailscale addresses. MagicDNS and mDNS are never
/// assumed to traverse the tailnet. An agent still needs explicit enrollment.
pub fn tailscale_candidates(bytes: &[u8], timestamp: u64) -> Result<Vec<Candidate>> {
    if bytes.len() > MAX_BODY {
        bail!("Tailscale status exceeds size limit");
    }
    let root: serde_json::Value =
        serde_json::from_slice(bytes).context("invalid Tailscale status JSON")?;
    if root
        .get("BackendState")
        .and_then(|v| v.as_str())
        .is_some_and(|s| s != "Running")
    {
        return Ok(vec![]);
    }
    let own = root.get("Self").unwrap_or(&serde_json::Value::Null);
    let own_id = own.get("ID").and_then(|v| v.as_str());
    let own_key = own.get("PublicKey").and_then(|v| v.as_str());
    let mut out = Vec::new();
    if let Some(peers) = root.get("Peer").and_then(|v| v.as_object()) {
        for (key, p) in peers.iter().take(MAX_CANDIDATES) {
            if p.get("Online").and_then(|v| v.as_bool()) != Some(true)
                || own_key == Some(key.as_str())
                || own_id.is_some_and(|id| p.get("ID").and_then(|v| v.as_str()) == Some(id))
            {
                continue;
            }
            let node = p
                .get("ID")
                .and_then(|v| v.as_str())
                .filter(|id| !id.is_empty() && id.len() <= 128)
                .unwrap_or(key.as_str())
                .to_owned();
            let name = p
                .get("HostName")
                .and_then(|v| v.as_str())
                .unwrap_or("Tailnet machine");
            if name.len() > 256 || name.chars().any(char::is_control) {
                continue;
            }
            if let Some(addresses) = p.get("TailscaleIPs").and_then(|v| v.as_array()) {
                for ip in addresses
                    .iter()
                    .take(8)
                    .filter_map(|v| v.as_str())
                    .filter_map(|s| s.parse::<IpAddr>().ok())
                {
                    // Status data is untrusted input. Only Tailscale's assigned ranges.
                    let tailnet = match ip {
                        IpAddr::V4(v) => {
                            let [a, b, _, _] = v.octets();
                            a == 100 && (64..=127).contains(&b)
                        }
                        IpAddr::V6(v) => v.octets()[..6] == [0xfd, 0x7a, 0x11, 0x5c, 0xa1, 0xe0],
                    };
                    if tailnet {
                        if let Some(endpoint) = endpoint(ip, DEFAULT_PORT) {
                            out.push(Candidate {
                                machine_id: None,
                                name: name.into(),
                                endpoint,
                                source: Source::Tailscale,
                                last_seen: timestamp,
                                node: Some(node.clone()),
                            });
                        }
                    }
                }
            }
        }
    }
    out.truncate(MAX_CANDIDATES);
    Ok(out)
}
async fn tailscale_status() -> Result<Vec<Candidate>> {
    let mut command = tokio::process::Command::new("tailscale");
    command
        .args(["status", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .context("Tailscale is unavailable; LAN and explicit peers still work")?;
    let mut stdout = child
        .stdout
        .take()
        .context("Tailscale stdout unavailable")?
        .take(MAX_BODY as u64 + 1);
    let result = tokio::time::timeout(Duration::from_secs(3), async {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).await?;
        let status = child.wait().await?;
        if !status.success() {
            bail!("Tailscale status is unavailable; LAN and explicit peers still work");
        }
        tailscale_candidates(&bytes, now())
    })
    .await;
    match result {
        Ok(r) => r,
        Err(_) => bail!("Tailscale status timed out; LAN and explicit peers still work"),
    }
}
pub(super) async fn collect_candidates(refresh: bool) -> Result<(Vec<Candidate>, Vec<String>)> {
    let mut cache: CandidateCache = read_json(CACHE_FILE)?;
    cache.expire(now());
    let mut warnings = Vec::new();
    if refresh {
        let (lan, tailnet) = tokio::join!(
            tokio::task::spawn_blocking(move || browse_lan(cache)),
            tailscale_status()
        );
        cache = match lan {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => {
                warnings.push(e.to_string());
                CandidateCache::default()
            }
            Err(e) => {
                warnings.push(format!("mDNS browsing failed: {e}"));
                CandidateCache::default()
            }
        };
        cache
            .entries
            .retain(|key, _| !key.starts_with("tailscale:"));
        match tailnet {
            Ok(all) => {
                for c in all {
                    cache.insert(format!("tailscale:{}", c.endpoint), vec![c]);
                }
            }
            Err(e) => warnings.push(e.to_string()),
        }
        cache.expire(now());
        crate::state::atomic_write(CACHE_FILE, &serde_json::to_vec(&cache)?)?;
    }
    Ok((cache.all(), warnings))
}
pub async fn candidates() -> Result<Vec<Candidate>> {
    Ok(collect_candidates(true).await?.0)
}

async fn query_peer_inner(mut peer: Peer, candidates: &[Candidate]) -> PeerCatalog {
    let mut endpoints = vec![(peer.endpoint.clone(), Source::Explicit)];
    for c in candidates {
        // mDNS's claimed identity does not confer trust: TLS and catalog identity
        // must still match. Tailnet candidates are tried with the same pinned key.
        if (c.machine_id.as_ref() == Some(&peer.machine.id) || c.source == Source::Tailscale)
            && !endpoints.iter().any(|(u, _)| u == &c.endpoint)
        {
            endpoints.push((c.endpoint.clone(), c.source.clone()));
        }
        if endpoints.len() >= 8 {
            break;
        }
    }
    let mut last_error = "peer is unreachable".to_owned();
    for (endpoint, source) in endpoints {
        match fetch_catalog(&peer, &endpoint).await {
            Ok(catalog) => {
                peer.machine = catalog.machine.clone();
                peer.endpoint = endpoint;
                peer.last_seen = now();
                if !peer.sources.contains(&source) {
                    peer.sources.push(source);
                }
                // An authenticated identity at one address deduplicates LAN+tailnet
                // records for that machine, without trusting unauthenticated hints.
                for c in candidates.iter().filter(|c| {
                    c.machine_id.as_ref() == Some(&peer.machine.id) || c.endpoint == peer.endpoint
                }) {
                    if !peer.sources.contains(&c.source) {
                        peer.sources.push(c.source.clone());
                    }
                }
                return PeerCatalog {
                    slug: peer.slug,
                    machine: peer.machine,
                    endpoint: peer.endpoint,
                    sources: peer.sources,
                    last_seen: peer.last_seen,
                    catalog: Some(catalog),
                    error: None,
                };
            }
            Err(e) => last_error = e.to_string(),
        }
    }
    PeerCatalog {
        slug: peer.slug,
        machine: peer.machine,
        endpoint: peer.endpoint,
        sources: peer.sources,
        last_seen: peer.last_seen,
        catalog: None,
        error: Some(last_error),
    }
}
fn unavailable_peer(peer: Peer, message: &str) -> PeerCatalog {
    PeerCatalog {
        slug: peer.slug,
        machine: peer.machine,
        endpoint: peer.endpoint,
        sources: peer.sources,
        last_seen: peer.last_seen,
        catalog: None,
        error: Some(message.into()),
    }
}
async fn query_peer(peer: Peer, candidates: &[Candidate]) -> PeerCatalog {
    match tokio::time::timeout(
        Duration::from_secs(6),
        query_peer_inner(peer.clone(), candidates),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => unavailable_peer(peer, "peer refresh timed out; retry or reconnect discovery"),
    }
}
pub async fn snapshot(refresh: bool) -> Result<Snapshot> {
    let local = super::server::local_catalog()?;
    let (mut candidates, mut warnings) = collect_candidates(refresh).await?;
    candidates.retain(|c| c.machine_id.as_ref() != Some(&local.machine.id));
    let enrolled: Vec<_> = peers()?
        .into_iter()
        .filter(|p| p.machine.id != local.machine.id)
        .collect();
    // One bad peer cannot turn a menu refresh into minutes of sequential timeouts.
    // Keep every enrolled machine visible with an explicit error if the overall
    // refresh budget expires; never retain its stale app catalog.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
    let mut pending = stream::iter(enrolled.iter().cloned().map(|p| query_peer(p, &candidates)))
        .buffer_unordered(16);
    let mut completed = BTreeMap::new();
    loop {
        match tokio::time::timeout_at(deadline, pending.next()).await {
            Ok(Some(peer)) => {
                completed.insert(peer.machine.id.clone(), peer);
            }
            Ok(None) => break,
            Err(_) => {
                warnings.push("Peer refresh reached its 12-second budget; unresolved machines remain visible without stale apps".into());
                break;
            }
        }
    }
    drop(pending);
    let results: Vec<_> = enrolled
        .into_iter()
        .map(|p| {
            completed
                .remove(&p.machine.id)
                .unwrap_or_else(|| unavailable_peer(p, "peer refresh budget exceeded; retry"))
        })
        .collect();
    let successful: Vec<_> = results.iter().filter(|p| p.catalog.is_some()).collect();
    if !successful.is_empty() {
        update_peers(|all| {
            record_refreshed(all, &successful);
            Ok(())
        })?;
    }
    let candidates = unpaired_candidates(candidates, &results);
    Ok(Snapshot {
        version: PROTOCOL_VERSION,
        local,
        peers: results,
        candidates,
        warnings,
    })
}
/// Asks the running proxy, which fetches over the connections its relay keeps
/// for browsers, so listing or opening a machine's apps warms the connection
/// and catalog the page then uses. `None` when no proxy answers (stopped, an
/// older version, or bound away from loopback): callers then fetch themselves.
async fn from_proxy<T: DeserializeOwned>(query: &str, timeout: Duration) -> Option<T> {
    use crate::proxy::{PEERS_PATH, PEERS_TOKEN_HEADER};
    if !crate::cli::proxy_running() {
        return None;
    }
    let settings = crate::state::proxy_settings().ok()?;
    let token = fs::read_to_string(crate::state::dir().join("proxy.hosts-sync-token")).ok()?;
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .http1_only()
        .timeout(timeout)
        .resolve(
            "localhost",
            SocketAddr::from(([127, 0, 0, 1], settings.port)),
        );
    if settings.tls {
        let pem = fs::read(crate::state::dir().join("ca.pem")).ok()?;
        builder = builder.add_root_certificate(reqwest::Certificate::from_pem(&pem).ok()?);
    }
    let url = format!(
        "{}://localhost:{}{PEERS_PATH}?{query}",
        if settings.tls { "https" } else { "http" },
        settings.port
    );
    let response = builder
        .build()
        .ok()?
        .get(url)
        .header(PEERS_TOKEN_HEADER, token.trim())
        .send()
        .await
        .ok()?;
    if !response.status().is_success() || response.content_length().unwrap_or(0) > 16 * 1024 * 1024
    {
        return None;
    }
    serde_json::from_slice(&response.bytes().await.ok()?).ok()
}
/// Paired machines and their apps, as `snapshot`, preferably through the proxy.
pub async fn list(refresh: bool) -> Result<Snapshot> {
    let (query, timeout) = if refresh {
        ("refresh=1", Duration::from_secs(30))
    } else {
        ("", Duration::from_secs(20))
    };
    match from_proxy(query, timeout).await {
        Some(snapshot) => Ok(snapshot),
        None => snapshot(refresh).await,
    }
}
/// One paired machine's apps, as `refresh_peer`, preferably through the proxy.
pub async fn open_peer(local_id: &str, peer_id: &str) -> Result<PeerCatalog> {
    if valid_id(peer_id) {
        let query = format!("peer={peer_id}");
        if let Some(peer) = from_proxy(&query, Duration::from_secs(10)).await {
            return Ok(peer);
        }
    }
    refresh_peer(local_id, peer_id).await
}
/// Persist the address, sources and identity a successful refresh authenticated.
fn record_refreshed(all: &mut [Peer], successful: &[&PeerCatalog]) {
    for p in all {
        if let Some(good) = successful.iter().find(|g| g.machine.id == p.machine.id) {
            p.endpoint = good.endpoint.clone();
            p.sources = good.sources.clone();
            p.machine = good.machine.clone();
            p.last_seen = good.last_seen;
        }
    }
}
/// Refresh one enrolled peer, as `snapshot` would, without querying any other.
/// Opening an app on a reachable machine must not wait out the connect timeout
/// of an unrelated peer that is offline.
pub async fn refresh_peer(local_id: &str, peer_id: &str) -> Result<PeerCatalog> {
    crate::state::load()?;
    refresh_peer_in(&crate::state::dir(), local_id, peer_id).await
}
pub(super) async fn refresh_peer_in(
    dir: &std::path::Path,
    local_id: &str,
    peer_id: &str,
) -> Result<PeerCatalog> {
    let peer = peers_in(dir)?
        .into_iter()
        .find(|p| p.machine.id == peer_id && p.machine.id != local_id)
        .context("paired peer unavailable")?;
    // The cached hints `snapshot(false)` uses; never browse the network here.
    let mut cache: CandidateCache = read_json_in(dir, CACHE_FILE)?;
    cache.expire(now());
    let mut candidates = cache.all();
    candidates.retain(|c| c.machine_id.as_deref() != Some(local_id));
    let result = query_peer(peer, &candidates).await;
    if result.catalog.is_some() {
        update_peers_in(dir, |all| {
            record_refreshed(all, &[&result]);
            Ok(())
        })?;
    }
    Ok(result)
}
fn endpoint_ip(endpoint: &str) -> Option<IpAddr> {
    match Url::parse(endpoint).ok()?.host()? {
        url::Host::Ipv4(ip) => Some(ip.into()),
        url::Host::Ipv6(ip) => Some(ip.into()),
        url::Host::Domain(_) => None,
    }
}
/// "Discovered machines" lists only machines that still need pairing. A paired
/// machine is recognised by its authenticated id (mDNS) or by its address. A
/// Tailscale hint guesses the default port, so match on the IP alone: a peer
/// paired on another port must not reappear as a second, unpaired machine.
/// Every address of a matched Tailscale node is hidden with it.
pub(super) fn unpaired_candidates(
    candidates: Vec<Candidate>,
    peers: &[PeerCatalog],
) -> Vec<Candidate> {
    let paired = |c: &Candidate| {
        let ip = endpoint_ip(&c.endpoint);
        peers.iter().any(|p| {
            c.machine_id.as_ref() == Some(&p.machine.id)
                || (ip.is_some() && endpoint_ip(&p.endpoint) == ip)
        })
    };
    let paired_nodes: Vec<String> = candidates
        .iter()
        .filter(|c| paired(c))
        .filter_map(|c| c.node.clone())
        .collect();
    candidates
        .into_iter()
        .filter(|c| !paired(c) && !c.node.as_ref().is_some_and(|n| paired_nodes.contains(n)))
        .collect()
}

#[derive(Clone)]
pub(super) struct RelayContext {
    pub(super) peer: Peer,
    pub(super) app_id: String,
    pub(super) client: RelayClient,
    /// Browser-visible origin; the request Host must match its authority.
    pub(super) relay_url: Url,
    pub(super) local_app: Url,
}
pub(super) fn relay_client(pin: &str) -> Result<RelayClient> {
    let connector = |h2: bool| -> Result<_> {
        // Small TLS records (response heads, short modules) must not wait on
        // Nagle's algorithm for the previous segment's ACK.
        let mut http = hyper_util::client::legacy::connect::HttpConnector::new();
        http.enforce_http(false);
        http.set_nodelay(true);
        // As for catalog requests: a refresh tries several addresses in turn.
        http.set_connect_timeout(Some(Duration::from_secs(2)));
        let builder = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(tls_config(pin)?)
            .https_only();
        // Offering both lets agents from before HTTP/2 answer over HTTP/1.1.
        Ok(if h2 {
            builder.enable_all_versions().wrap_connector(http)
        } else {
            builder.enable_http1().wrap_connector(http)
        })
    };
    let requests = Client::builder(TokioExecutor::new())
        .timer(TokioTimer::new())
        .pool_timer(TokioTimer::new())
        // A link that dies silently (sleep, a network change) fails every
        // request on the shared connection; pings notice within seconds.
        .http2_keep_alive_interval(Duration::from_secs(10))
        .http2_keep_alive_timeout(Duration::from_secs(5))
        .http2_keep_alive_while_idle(true)
        // Keep the warm connection between page loads. (hyper's fixed HTTP/2
        // windows already exceed a typical link's bandwidth-delay product; an
        // adaptive window would start at 64 KiB and throttle a cold page load.)
        .pool_idle_timeout(Duration::from_secs(300))
        .build(connector(true)?);
    let upgrades = Client::builder(TokioExecutor::new()).build(connector(false)?);
    Ok(RelayClient { requests, upgrades })
}
fn plain_error(status: StatusCode, message: &str) -> Response<Body> {
    let mut r = Response::new(Body::from(message.to_owned()));
    *r.status_mut() = status;
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    r
}
fn strip_hop_headers(headers: &mut header::HeaderMap) {
    let names: Vec<String> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(','))
        .map(|s| s.trim().to_owned())
        .collect();
    for name in names {
        headers.remove(name);
    }
    for name in [
        "connection",
        "keep-alive",
        "proxy-connection",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        headers.remove(name);
    }
}
/// True when a redirect host would send the browser to a service on its own
/// machine. Matches on the typed host so bracketed IPv6 literals (`[::1]`),
/// IPv4-mapped/compatible IPv6, numeric IPv4 forms the URL parser normalizes
/// (`0x7f.1`, `2130706433`), unspecified addresses and FQDN `localhost.` are all caught.
fn is_local_redirect_host(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(host)) => {
            let host = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
            host == "localhost" || host.ends_with(".localhost")
        }
        Some(url::Host::Ipv4(ip)) => ip.is_loopback() || ip.is_unspecified(),
        Some(url::Host::Ipv6(ip)) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || ip
                    .to_ipv4()
                    .is_some_and(|v4| v4.is_loopback() || v4.is_unspecified())
        }
        None => false,
    }
}
fn rewrite_location(value: &str, local_app: &Url, relay_url: &Url) -> Result<String> {
    // Resolve the reference the way the browser will: against the relay page.
    // WHATWG parsing treats backslashes, mixed slashes and leading whitespace in
    // `\\host`, `/\host` or ` //host` as authority references, so a plain
    // `Url::parse` or a `starts_with("//")` check is not enough to classify them.
    let Ok(browser_target) = relay_url.join(value) else {
        bail!("remote app sent an invalid redirect");
    };
    if browser_target.origin() == relay_url.origin() {
        // Relative references (and links back to this relay) stay as sent so the
        // browser resolves them against the real request path.
        return Ok(value.into());
    }
    // The same reference as the remote app sees it, to recognise its own origin.
    let Ok(target) = local_app.join(value) else {
        bail!("remote app sent an invalid redirect");
    };
    // Only rewrite when the reference names the app's authority itself, not when
    // it merely inherits the app's origin (`http:path` is relative to an http
    // base but an authority reference to the browser on an https relay).
    let mut probe = local_app.clone();
    probe.set_host(Some("porch-redirect-probe.invalid"))?;
    let names_authority = probe
        .join(value)
        .is_ok_and(|probed| probed.host() == target.host());
    // Rewrite whenever the browser would land on the app's origin, or the
    // reference explicitly names the app's authority (e.g. `//app:port` on an
    // https relay for an http app).
    let same_app = if browser_target.origin() == local_app.origin() {
        Some(browser_target.clone())
    } else if names_authority && target.origin() == local_app.origin() {
        Some(target)
    } else {
        None
    };
    if let Some(mut target) = same_app {
        target
            .set_scheme(relay_url.scheme())
            .map_err(|_| anyhow::anyhow!("invalid redirect"))?;
        target.set_host(relay_url.host_str())?;
        target
            .set_port(relay_url.port())
            .map_err(|_| anyhow::anyhow!("invalid redirect"))?;
        return Ok(target.into());
    }
    if is_local_redirect_host(&browser_target) {
        bail!("remote app redirected to a different loopback service; use a relative URL or the app's Porch URL");
    }
    Ok(value.into())
}
pub(super) async fn relay_request(
    mut req: Request<Incoming>,
    context: RelayContext,
) -> std::result::Result<Response<Body>, Infallible> {
    let expected =
        context.relay_url[url::Position::BeforeHost..url::Position::AfterPort].to_owned();
    if req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        != Some(expected.as_str())
        || req.uri().authority().is_some()
        || req.method() == hyper::Method::CONNECT
    {
        return Ok(plain_error(
            StatusCode::MISDIRECTED_REQUEST,
            "Use this relay's exact app.localhost URL",
        ));
    }
    if let Some(origin) = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    {
        if origin != format!("{}://{expected}", context.relay_url.scheme()) {
            return Ok(plain_error(
                StatusCode::FORBIDDEN,
                "Cross-origin browser access to a Porch relay is rejected",
            ));
        }
    }
    let upgrade = req
        .headers()
        .get(header::UPGRADE)
        .cloned()
        .filter(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"));
    let downstream = upgrade.as_ref().map(|_| hyper::upgrade::on(&mut req));
    let client = if upgrade.is_some() {
        &context.client.upgrades
    } else {
        &context.client.requests
    };
    let suffix = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let uri = format!(
        "{}v1/apps/{}/proxy{}",
        context.peer.endpoint, context.app_id, suffix
    );
    let Ok(uri) = uri.parse::<hyper::Uri>() else {
        return Ok(plain_error(
            StatusCode::BAD_REQUEST,
            "Invalid relay request path",
        ));
    };
    // Untrusted browser headers cannot inject the agent's internal forwarding metadata.
    let internal: Vec<_> = req
        .headers()
        .keys()
        .filter(|k| k.as_str().starts_with("x-porch-app-"))
        .cloned()
        .collect();
    for name in internal {
        req.headers_mut().remove(name);
    }
    let browser_auth = req.headers_mut().remove(header::AUTHORIZATION);
    let browser_origin = req.headers_mut().remove(header::ORIGIN);
    let browser_host = req.headers_mut().remove(header::HOST);
    // Set by the local proxy (this relay only listens on loopback).
    let browser_proto = req
        .headers()
        .get("x-forwarded-proto")
        .filter(|v| *v == "https" || *v == "http")
        .cloned();
    strip_hop_headers(req.headers_mut());
    if let Some(v) = browser_auth {
        req.headers_mut().insert("x-porch-app-authorization", v);
    }
    if browser_origin.is_some() {
        if let Ok(v) = context.local_app.origin().ascii_serialization().parse() {
            req.headers_mut().insert("x-porch-app-origin", v);
        }
    }
    if let Some(v) = browser_host {
        req.headers_mut().insert("x-porch-app-host", v);
    }
    if let Some(v) = browser_proto {
        req.headers_mut().insert("x-porch-app-proto", v);
    }
    req.headers_mut().insert(
        header::AUTHORIZATION,
        format!("Bearer {}", context.peer.token)
            .parse()
            .expect("validated enrollment token"),
    );
    req.headers_mut().insert(
        header::HOST,
        uri.authority()
            .expect("validated endpoint")
            .as_str()
            .parse()
            .expect("literal authority"),
    );
    if let Some(upgrade) = upgrade {
        req.headers_mut().insert(header::UPGRADE, upgrade);
        req.headers_mut().insert(
            header::CONNECTION,
            header::HeaderValue::from_static("upgrade"),
        );
    }
    *req.uri_mut() = uri;
    let result =
        tokio::time::timeout(Duration::from_secs(30), client.request(req.map(Body::new))).await;
    let mut response = match result {
        Ok(Ok(r)) => r,
        _ => {
            return Ok(plain_error(
                StatusCode::BAD_GATEWAY,
                "Remote Porch app is unavailable or its certificate changed",
            ))
        }
    };
    if response.status() == StatusCode::SWITCHING_PROTOCOLS {
        if let Some(downstream) = downstream {
            let upstream = hyper::upgrade::on(&mut response);
            tokio::spawn(async move {
                if let Ok((a, b)) = tokio::try_join!(downstream, upstream) {
                    let mut a = TokioIo::new(a);
                    let mut b = TokioIo::new(b);
                    let _ = tokio::io::copy_bidirectional(&mut a, &mut b).await;
                }
            });
        } else {
            return Ok(plain_error(
                StatusCode::BAD_GATEWAY,
                "Unexpected remote protocol upgrade",
            ));
        }
    } else {
        strip_hop_headers(response.headers_mut());
    }
    if let Some(location) = response
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
    {
        let translated = match rewrite_location(location, &context.local_app, &context.relay_url) {
            Ok(v) => v,
            Err(e) => return Ok(plain_error(StatusCode::BAD_GATEWAY, &e.to_string())),
        };
        if let Ok(value) = translated.parse() {
            response.headers_mut().insert(header::LOCATION, value);
        }
    }
    // Host-only cookies stay on this app's unique .localhost hostname; ports alone
    // do not isolate cookies. Keep Secure/HttpOnly/SameSite attributes unchanged.
    let cookies: Vec<_> = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .cloned()
        .collect();
    response.headers_mut().remove(header::SET_COOKIE);
    for cookie in cookies {
        if let Ok(value) = cookie.to_str() {
            let value = value
                .split(';')
                .filter(|part| {
                    !part
                        .trim_start()
                        .to_ascii_lowercase()
                        .starts_with("domain=")
                })
                .collect::<Vec<_>>()
                .join(";");
            if let Ok(value) = value.parse() {
                response.headers_mut().append(header::SET_COOKIE, value);
            }
        }
    }
    // Agent bearer credentials must never appear in browser-visible metadata.
    response.headers_mut().remove(header::AUTHORIZATION);
    Ok(response.map(Body::new))
}
/// Foreground relay; emits a unique app-specific .localhost URL after binding 127.0.0.1.
/// The relay owns no app process and exit never stops the remote application.
pub async fn relay(peer_id: &str, app_id: &str, bind_port: u16) -> Result<()> {
    let peer = peers()?
        .into_iter()
        .find(|p| p.machine.id == peer_id)
        .context("peer is not enrolled")?;
    let catalog = fetch_catalog(&peer, &peer.endpoint).await?;
    let app = catalog
        .apps
        .iter()
        .find(|a| a.id == app_id)
        .context("app is absent from peer catalog")?;
    if !app.remote_access {
        bail!(
            "{}",
            app.unavailable_reason
                .as_deref()
                .unwrap_or("peer has not explicitly enabled app sharing")
        );
    }
    let listener = crate::sockets::bind(SocketAddr::from(([127, 0, 0, 1], bind_port)), true)?;
    listener.set_nonblocking(true)?;
    let listener = tokio::net::TcpListener::from_std(listener)?;
    let address = listener.local_addr()?;
    let client = relay_client(&peer.fingerprint)?;
    let scope: String = Sha256::digest(format!("{}\0{app_id}", peer.machine.id).as_bytes())
        .iter()
        .map(|v| format!("{v:02x}"))
        .collect();
    let relay_url = Url::parse(&format!(
        "http://porch-{}.localhost:{}",
        &scope[..32],
        address.port()
    ))?;
    let context = RelayContext {
        peer,
        app_id: app_id.into(),
        client,
        relay_url: relay_url.clone(),
        local_app: Url::parse(&app.local_url)?,
    };
    println!(
        "{}",
        serde_json::json!({"url":relay_url.as_str().trim_end_matches('/')})
    );
    std::io::stdout().flush()?;
    let permits = Arc::new(tokio::sync::Semaphore::new(128));
    loop {
        let (stream, peer) = listener.accept().await?;
        if !peer.ip().is_loopback() {
            continue;
        }
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            continue;
        };
        let context = context.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let service =
                hyper::service::service_fn(move |req| relay_request(req, context.clone()));
            let mut builder = hyper::server::conn::http1::Builder::new();
            builder
                .header_read_timeout(Duration::from_secs(10))
                .timer(hyper_util::rt::TokioTimer::new())
                .max_buf_size(32768);
            let _ = builder
                .serve_connection(TokioIo::new(stream), service)
                .with_upgrades()
                .await;
        });
    }
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
