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
    rt::{TokioExecutor, TokioIo},
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

type RelayClient =
    Client<hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>, Body>;

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
fn allowed_ip(ip: IpAddr) -> bool {
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
fn endpoint(ip: IpAddr, port: u16) -> Option<String> {
    if !allowed_ip(ip) || port == 0 {
        return None;
    }
    Some(format!("https://{}/", SocketAddr::new(ip, port)))
}
fn normalize_pin(pin: &str) -> Result<[u8; 32]> {
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
fn hex_pin(pin: &[u8; 32]) -> String {
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
fn tls_config(pin: &str) -> Result<rustls::ClientConfig> {
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
fn http_client(pin: &str) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .use_preconfigured_tls(tls_config(pin)?)
        .https_only(true)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(2))
        .timeout(REQUEST_TIMEOUT)
        .build()?)
}
async fn bounded_json<T: DeserializeOwned>(response: reqwest::Response) -> Result<T> {
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
fn validate_machine(machine: &Machine) -> Result<()> {
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
    match fs::read(crate::state::dir().join(file)) {
        Ok(bytes) if bytes.len() <= MAX_BODY => {
            serde_json::from_slice(&bytes).with_context(|| format!("invalid {file}"))
        }
        Ok(_) => bail!("{file} exceeds size limit"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e.into()),
    }
}
pub fn peers() -> Result<Vec<Peer>> {
    let peers: Vec<Peer> = read_json(PEERS_FILE)?;
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
    use fs2::FileExt;
    crate::state::load()?;
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(false).write(true).read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let lock = options.open(crate::state::dir().join("discovery-peers.lock"))?;
    lock.lock_exclusive()?;
    let mut all = peers()?;
    f(&mut all)?;
    if all.len() > MAX_PEERS {
        bail!("peer enrollment limit reached; remove an unused peer first");
    }
    crate::state::atomic_write(PEERS_FILE, &serde_json::to_vec_pretty(&all)?)
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
    };
    update_peers(|all| {
        all.retain(|p| p.machine.id != peer.machine.id);
        all.push(peer.clone());
        Ok(())
    })?;
    Ok(peer)
}
async fn fetch_catalog(peer: &Peer, endpoint: &str) -> Result<Catalog> {
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
async fn collect_candidates(refresh: bool) -> Result<(Vec<Candidate>, Vec<String>)> {
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
            for p in all {
                if let Some(good) = successful.iter().find(|g| g.machine.id == p.machine.id) {
                    p.endpoint = good.endpoint.clone();
                    p.sources = good.sources.clone();
                    p.machine = good.machine.clone();
                    p.last_seen = good.last_seen;
                }
            }
            Ok(())
        })?;
    }
    Ok(Snapshot {
        version: PROTOCOL_VERSION,
        local,
        peers: results,
        candidates,
        warnings,
    })
}

#[derive(Clone)]
struct RelayContext {
    peer: Peer,
    app_id: String,
    client: RelayClient,
    relay_url: Url,
    local_app: Url,
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
fn rewrite_location(value: &str, context: &RelayContext) -> Result<String> {
    let parsed = if value.starts_with("//") {
        context.local_app.join(value)
    } else {
        Url::parse(value)
    };
    let Ok(mut target) = parsed else {
        return Ok(value.into());
    };
    if target.origin() == context.local_app.origin() {
        target
            .set_scheme("http")
            .map_err(|_| anyhow::anyhow!("invalid redirect"))?;
        target.set_host(context.relay_url.host_str())?;
        target
            .set_port(context.relay_url.port())
            .map_err(|_| anyhow::anyhow!("invalid redirect"))?;
        return Ok(target.into());
    }
    let local_host = target.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host.ends_with(".localhost")
            || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
    });
    if local_host {
        bail!("remote app redirected to a different loopback service; use a relative URL or the app's Porch URL");
    }
    Ok(value.into())
}
async fn relay_request(
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
        if origin != format!("http://{expected}") {
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
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        context.client.request(req.map(Body::new)),
    )
    .await;
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
        let translated = match rewrite_location(location, &context) {
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
    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(tls_config(&peer.fingerprint)?)
        .https_only()
        .enable_http1()
        .build();
    let client = Client::builder(TokioExecutor::new()).build(https);
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
