//! Opt-in authenticated discovery agent. This is separate from the app proxy lifecycle.
use super::types::*;
use crate::state::{self, ProcessIdentity, ProxySettings, Route, State};
use anyhow::{bail, Context, Result};
use bytes::Bytes;
use fs2::FileExt;
use http_body_util::{combinators::UnsyncBoxBody, BodyExt, Full, Limited};
use hyper::{body::Incoming, header, Method, Request, Response, StatusCode, Version};
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::{TokioExecutor, TokioIo, TokioTimer},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    convert::Infallible,
    fs::{self, OpenOptions},
    io::Write,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type Body = UnsyncBoxBody<Bytes, BoxError>;
type Upstream = Client<hyper_rustls::HttpsConnector<HttpConnector>, Body>;
const MAX_CLIENTS: usize = 64;
const MAX_ROUTES: usize = 512;
const MAX_UPLOAD: usize = 16 * 1024 * 1024;
const PAIR_LIFETIME: u64 = 600;
/// Pairing attempts allowed per source address per minute (code and token).
const PAIR_RATE_LIMIT: usize = 10;

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub(super) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|v| format!("{v:02x}")).collect()
}
pub(super) fn random() -> Result<String> {
    let mut value = [0u8; 32];
    getrandom::fill(&mut value).map_err(|e| anyhow::anyhow!("secure random generation: {e}"))?;
    Ok(hex(&value))
}
fn digest(value: &str) -> String {
    hex(&Sha256::digest(value.as_bytes()))
}
pub(super) fn equal(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |n, (x, y)| n | (x ^ y)) == 0
}
fn read<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => {
            if bytes.len() > MAX_BODY {
                bail!("discovery state exceeds size limit")
            }
            Ok(Some(
                serde_json::from_slice(&bytes).context("invalid discovery state")?,
            ))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
/// Private atomic files do not reuse proxy lifecycle state or modify trust stores.
pub(super) fn write(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    if fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_symlink()) {
        bail!("discovery state directory cannot be a symlink")
    }
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    let path = dir.join(format!("{name}.{}.{}.tmp", std::process::id(), random()?));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(&path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(path, dir.join(name))?;
    // Windows cannot open a directory as a file, and its renames need no
    // directory sync to be durable.
    #[cfg(unix)]
    fs::File::open(dir)?.sync_all()?;
    Ok(())
}
pub(super) fn lock(dir: &Path) -> Result<fs::File> {
    // Preparing the directory through an atomic marker ensures private permissions.
    write(dir, "discovery-format", b"1\n")?;
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(dir.join("discovery-auth.lock"))?;
    file.lock_exclusive()?;
    Ok(file)
}
#[derive(Default, Serialize, Deserialize)]
struct Auth {
    invitation: Option<Invitation>,
    #[serde(default)]
    clients: Vec<String>,
    /// Bearer digest -> machine ID for clients enrolled through code pairing, so
    /// `porch unpair` can revoke exactly one machine.
    #[serde(default)]
    labels: BTreeMap<String, String>,
    #[serde(default)]
    pairing: Option<PairingWindow>,
    #[serde(default)]
    pairing_result: Option<PairingResult>,
}
#[derive(Serialize, Deserialize)]
struct PairingWindow {
    id: String,
    /// SHA-256 of `id \0 code`; the code itself is never written to disk.
    hash: String,
    expires: u64,
    failures: u32,
}
#[derive(Clone, Serialize, Deserialize)]
struct PairingResult {
    id: String,
    paired: Option<String>,
    error: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct Invitation {
    hash: String,
    expires: u64,
}
#[cfg(unix)]
fn os_hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len() - 1) } != 0 {
        return None;
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    Some(String::from_utf8_lossy(&buf[..end]).into_owned())
}
#[cfg(not(unix))]
fn os_hostname() -> Option<String> {
    None
}
fn machine(dir: &Path) -> Result<Machine> {
    let _lock = lock(dir)?;
    if let Some(value) = read(&dir.join("discovery-machine.json"))? {
        return Ok(value);
    }
    // Windows has no HOSTNAME; COMPUTERNAME is always set there.
    let name = ["HOSTNAME", "COMPUTERNAME"]
        .into_iter()
        .find_map(|v| std::env::var(v).ok().filter(|s| !s.trim().is_empty()))
        .or_else(os_hostname)
        .unwrap_or_else(|| "Porch machine".into());
    let value = Machine {
        id: random()?,
        name: name.chars().filter(|c| !c.is_control()).take(128).collect(),
    };
    write(dir, "discovery-machine.json", &serde_json::to_vec(&value)?)?;
    Ok(value)
}
pub fn identity() -> Result<Machine> {
    machine(&state::dir())
}
/// A literal private, CGNAT, ULA, link-local or loopback address is mandatory.
pub fn validate_bind(bind: &str) -> Result<IpAddr> {
    let ip: IpAddr = bind
        .parse()
        .context("discovery --bind requires an IP address on the intended interface")?;
    let allowed = match ip {
        IpAddr::V4(v) => {
            v.is_private()
                || v.is_loopback()
                || v.is_link_local()
                || (v.octets()[0] == 100 && (64..=127).contains(&v.octets()[1]))
        }
        IpAddr::V6(v) => v.is_loopback() || v.is_unique_local() || v.is_unicast_link_local(),
    };
    if !allowed || ip.is_unspecified() || ip.is_multicast() {
        bail!("discovery must bind one explicit private LAN, tailnet or loopback IP; wildcard/public binds are refused")
    }
    Ok(ip)
}
pub fn configure(value: ServerConfig) -> Result<()> {
    validate_bind(&value.bind)?;
    if value.port == 0 {
        bail!("discovery port must be 1-65535")
    }
    identity()?;
    ensure_certificate(&state::dir(), &value.bind)?;
    write(
        &state::dir(),
        "discovery-config.json",
        &serde_json::to_vec_pretty(&value)?,
    )
}
pub fn config() -> Result<Option<ServerConfig>> {
    read(&state::dir().join("discovery-config.json"))
}
/// Pairing is consent to share apps; the running agent reads this per request.
pub fn set_share_apps(share: bool) -> Result<()> {
    let mut value = config()?.context("discovery is disabled")?;
    if value.share_apps != share {
        value.share_apps = share;
        write(
            &state::dir(),
            "discovery-config.json",
            &serde_json::to_vec_pretty(&value)?,
        )?;
    }
    Ok(())
}
fn private_v4(ip: std::net::Ipv4Addr) -> bool {
    ip.is_private() && !ip.is_loopback()
}
fn tailnet_v4(ip: std::net::Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    a == 100 && (64..=127).contains(&b)
}
#[cfg(unix)]
fn interface_addresses() -> Vec<std::net::Ipv4Addr> {
    let mut out = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs allocates a list we walk read-only and free exactly once.
    unsafe {
        if libc::getifaddrs(&mut list) != 0 {
            return out;
        }
        let mut cursor = list;
        while !cursor.is_null() {
            let entry = &*cursor;
            if !entry.ifa_addr.is_null()
                && i32::from((*entry.ifa_addr).sa_family) == libc::AF_INET
                && entry.ifa_flags & (libc::IFF_UP as libc::c_uint) != 0
            {
                let addr = &*(entry.ifa_addr as *const libc::sockaddr_in);
                out.push(std::net::Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr)));
            }
            cursor = entry.ifa_next;
        }
        libc::freeifaddrs(list);
    }
    out
}
#[cfg(windows)]
fn interface_addresses() -> Vec<std::net::Ipv4Addr> {
    crate::win_net::interface_addresses()
}
#[cfg(not(any(unix, windows)))]
fn interface_addresses() -> Vec<std::net::Ipv4Addr> {
    vec![]
}
/// The local source address the OS would use to reach `target`. A UDP connect
/// selects a route without sending any packet.
fn route_source(target: IpAddr) -> Option<IpAddr> {
    let socket = std::net::UdpSocket::bind(if target.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    })
    .ok()?;
    socket.connect(SocketAddr::new(target, 9)).ok()?;
    Some(socket.local_addr().ok()?.ip())
}
/// Choose the private interface to share on: the one that reaches `peer` when
/// known, otherwise the Tailscale 100.x address, otherwise the primary private
/// LAN address. Public and wildcard addresses are never chosen.
pub fn auto_bind(peer: Option<IpAddr>) -> Result<IpAddr> {
    if let Some(peer) = peer {
        if peer.is_loopback() {
            return Ok(peer);
        }
        if let Some(ip) = route_source(peer).filter(|ip| validate_bind(&ip.to_string()).is_ok()) {
            return Ok(ip);
        }
    }
    let all = interface_addresses();
    if let Some(ip) = all.iter().find(|ip| tailnet_v4(**ip)) {
        return Ok(IpAddr::V4(*ip));
    }
    if let Some(IpAddr::V4(ip)) = route_source(IpAddr::from([1, 1, 1, 1])) {
        if private_v4(ip) {
            return Ok(IpAddr::V4(ip));
        }
    }
    if let Some(ip) = all.iter().find(|ip| private_v4(**ip)) {
        return Ok(IpAddr::V4(*ip));
    }
    bail!("no private LAN or Tailscale address found; connect to a network or pass --bind IP")
}
pub fn disable() -> Result<()> {
    match fs::remove_file(state::dir().join("discovery-config.json")) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}
fn ensure_certificate(dir: &Path, bind: &str) -> Result<()> {
    let _lock = lock(dir)?;
    let cert = dir.join("discovery-cert.pem");
    let key = dir.join("discovery-key.pem");
    if cert.exists() && key.exists() {
        return Ok(());
    }
    if cert.exists() || key.exists() {
        bail!("discovery certificate/key incomplete; restore both files or explicitly reset discovery identity")
    }
    let pair = rcgen::generate_simple_self_signed(vec![bind.into(), "porch-agent.local".into()])?;
    write(
        dir,
        "discovery-key.pem",
        pair.key_pair.serialize_pem().as_bytes(),
    )?;
    write(dir, "discovery-cert.pem", pair.cert.pem().as_bytes())?;
    Ok(())
}
fn cert_der(dir: &Path) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    let bytes = fs::read(dir.join("discovery-cert.pem"))?;
    Ok(rustls_pemfile::certs(&mut bytes.as_slice()).collect::<std::io::Result<_>>()?)
}
pub fn fingerprint() -> Result<String> {
    let bind = config()?
        .context("discovery is disabled; enable it with an explicit bind first")?
        .bind;
    ensure_certificate(&state::dir(), &bind)?;
    fingerprint_in(&state::dir())
}
fn fingerprint_in(dir: &Path) -> Result<String> {
    let cert = cert_der(dir)?;
    Ok(hex(&Sha256::digest(
        cert.first()
            .context("missing discovery certificate")?
            .as_ref(),
    )))
}
pub struct PairingCode {
    pub id: String,
    pub code: String,
    pub expires: u64,
}
fn six_digits() -> Result<String> {
    // Rejection sampling keeps every code equally likely.
    loop {
        let mut bytes = [0u8; 4];
        getrandom::fill(&mut bytes)
            .map_err(|e| anyhow::anyhow!("secure random generation: {e}"))?;
        let value = u32::from_le_bytes(bytes);
        if value < u32::MAX - (u32::MAX % 1_000_000) {
            return Ok(format!("{:06}", value % 1_000_000));
        }
    }
}
/// Open (or replace) the single pairing window and return its one-time code.
pub fn open_pairing() -> Result<PairingCode> {
    config()?.context("discovery is disabled")?;
    open_pairing_in(&state::dir(), PAIR_CODE_LIFETIME)
}
fn open_pairing_in(dir: &Path, lifetime: u64) -> Result<PairingCode> {
    let _lock = lock(dir)?;
    let id = random()?;
    let code = six_digits()?;
    let mut auth: Auth = read(&dir.join("discovery-auth.json"))?.unwrap_or_default();
    let expires = now() + lifetime;
    auth.pairing = Some(PairingWindow {
        hash: digest(&format!("{id}\0{code}")),
        id: id.clone(),
        expires,
        failures: 0,
    });
    write(dir, "discovery-auth.json", &serde_json::to_vec(&auth)?)?;
    Ok(PairingCode { id, code, expires })
}
pub enum PairingProgress {
    Waiting,
    Paired(String),
    Closed(String),
}
/// What happened to a pairing window opened by this process.
pub fn pairing_progress(id: &str) -> Result<PairingProgress> {
    pairing_progress_in(&state::dir(), id)
}
fn pairing_progress_in(dir: &Path, id: &str) -> Result<PairingProgress> {
    let auth: Auth = read(&dir.join("discovery-auth.json"))?.unwrap_or_default();
    if let Some(result) = auth.pairing_result.filter(|r| r.id == id) {
        return Ok(match (result.paired, result.error) {
            (Some(name), _) => PairingProgress::Paired(name),
            (None, error) => {
                PairingProgress::Closed(error.unwrap_or_else(|| "pairing window closed".into()))
            }
        });
    }
    match auth.pairing {
        Some(w) if w.id == id && w.expires > now() => Ok(PairingProgress::Waiting),
        Some(w) if w.id == id => Ok(PairingProgress::Closed(
            "pairing code expired; run porch pair again".into(),
        )),
        _ => Ok(PairingProgress::Closed(
            "pairing window was replaced or revoked".into(),
        )),
    }
}
/// Close the window (Ctrl-C). Harmless if it already closed.
pub fn close_pairing(id: &str) -> Result<()> {
    let dir = state::dir();
    let _lock = lock(&dir)?;
    let mut auth: Auth = read(&dir.join("discovery-auth.json"))?.unwrap_or_default();
    if auth.pairing.as_ref().is_some_and(|w| w.id == id) {
        auth.pairing = None;
        write(&dir, "discovery-auth.json", &serde_json::to_vec(&auth)?)?;
    }
    Ok(())
}
fn pairing_open(dir: &Path) -> bool {
    read::<Auth>(&dir.join("discovery-auth.json"))
        .ok()
        .flatten()
        .and_then(|a| a.pairing)
        .is_some_and(|w| w.expires > now() && w.failures < PAIR_CODE_MAX_FAILURES)
}
enum CodeOutcome {
    Accepted(String),
    Wrong,
    Closed,
}
/// Check a code in constant time. Five wrong guesses or expiry close the window;
/// success consumes it and enrolls a new bearer labelled with the peer's machine ID.
fn try_code(dir: &Path, code: &str, peer: &Machine) -> Result<CodeOutcome> {
    let _lock = lock(dir)?;
    let mut auth: Auth = read(&dir.join("discovery-auth.json"))?.unwrap_or_default();
    let Some(window) = auth.pairing.as_mut() else {
        return Ok(CodeOutcome::Closed);
    };
    if window.expires <= now() || window.failures >= PAIR_CODE_MAX_FAILURES {
        let id = window.id.clone();
        auth.pairing = None;
        auth.pairing_result = Some(PairingResult {
            id,
            paired: None,
            error: Some("pairing code expired; run porch pair again".into()),
        });
        write(dir, "discovery-auth.json", &serde_json::to_vec(&auth)?)?;
        return Ok(CodeOutcome::Closed);
    }
    let candidate = digest(&format!("{}\0{code}", window.id));
    if !equal(&candidate, &window.hash) {
        window.failures += 1;
        if window.failures >= PAIR_CODE_MAX_FAILURES {
            let id = window.id.clone();
            auth.pairing = None;
            auth.pairing_result = Some(PairingResult {
                id,
                paired: None,
                error: Some(
                    "too many wrong pairing codes; the code was cancelled. Run porch pair again"
                        .into(),
                ),
            });
        }
        write(dir, "discovery-auth.json", &serde_json::to_vec(&auth)?)?;
        return Ok(CodeOutcome::Wrong);
    }
    let id = window.id.clone();
    // Re-pairing replaces this machine's previous credential.
    let stale: Vec<String> = auth
        .labels
        .iter()
        .filter(|(_, m)| **m == peer.id)
        .map(|(d, _)| d.clone())
        .collect();
    auth.clients.retain(|c| !stale.contains(c));
    auth.labels.retain(|d, _| !stale.contains(d));
    if auth.clients.len() >= MAX_CLIENTS {
        bail!("client enrollment limit reached; revoke old clients locally")
    }
    let bearer = random()?;
    auth.clients.push(digest(&bearer));
    auth.labels.insert(digest(&bearer), peer.id.clone());
    auth.pairing = None;
    auth.pairing_result = Some(PairingResult {
        id,
        paired: Some(peer.name.clone()),
        error: None,
    });
    write(dir, "discovery-auth.json", &serde_json::to_vec(&auth)?)?;
    Ok(CodeOutcome::Accepted(bearer))
}
/// Joining side: issue a credential the host will use to reach this machine.
pub fn mint_client(label: &str) -> Result<String> {
    let dir = state::dir();
    let _lock = lock(&dir)?;
    let mut auth: Auth = read(&dir.join("discovery-auth.json"))?.unwrap_or_default();
    if auth.clients.len() >= MAX_CLIENTS {
        bail!("client enrollment limit reached; revoke old clients locally")
    }
    let bearer = random()?;
    auth.clients.push(digest(&bearer));
    auth.labels.insert(digest(&bearer), label.into());
    write(&dir, "discovery-auth.json", &serde_json::to_vec(&auth)?)?;
    Ok(bearer)
}
/// Bind a minted credential to the machine that accepted it, replacing older ones.
pub fn label_client(bearer: &str, machine_id: &str) -> Result<()> {
    let dir = state::dir();
    let _lock = lock(&dir)?;
    let mut auth: Auth = read(&dir.join("discovery-auth.json"))?.unwrap_or_default();
    let own = digest(bearer);
    let stale: Vec<String> = auth
        .labels
        .iter()
        .filter(|(d, m)| **m == machine_id && **d != own)
        .map(|(d, _)| d.clone())
        .collect();
    auth.clients.retain(|c| !stale.contains(c));
    auth.labels.retain(|d, _| !stale.contains(d));
    auth.labels.insert(own, machine_id.into());
    write(&dir, "discovery-auth.json", &serde_json::to_vec(&auth)?)
}
pub fn revoke_bearer(bearer: &str) -> Result<()> {
    revoke_where(&state::dir(), |d, _| d == digest(bearer))
}
/// Revoke every credential this machine issued to `machine_id`.
pub fn revoke_machine(machine_id: &str) -> Result<()> {
    revoke_where(&state::dir(), |_, m| m == Some(machine_id))
}
fn revoke_where(dir: &Path, f: impl Fn(&str, Option<&str>) -> bool) -> Result<()> {
    let _lock = lock(dir)?;
    let mut auth: Auth = read(&dir.join("discovery-auth.json"))?.unwrap_or_default();
    let labels = auth.labels.clone();
    auth.clients
        .retain(|d| !f(d, labels.get(d).map(String::as_str)));
    auth.labels.retain(|d, m| !f(d, Some(m)));
    write(dir, "discovery-auth.json", &serde_json::to_vec(&auth)?)
}
pub fn create_pairing_token() -> Result<String> {
    config()?.context("discovery is disabled")?;
    let dir = state::dir();
    let _lock = lock(&dir)?;
    let token = random()?;
    let mut auth: Auth = read(&dir.join("discovery-auth.json"))?.unwrap_or_default();
    auth.invitation = Some(Invitation {
        hash: digest(&token),
        expires: now() + PAIR_LIFETIME,
    });
    write(&dir, "discovery-auth.json", &serde_json::to_vec(&auth)?)?;
    Ok(token)
}
pub fn revoke_clients() -> Result<()> {
    let dir = state::dir();
    let _lock = lock(&dir)?;
    write(
        &dir,
        "discovery-auth.json",
        &serde_json::to_vec(&Auth::default())?,
    )
}
fn paired(dir: &Path, token: &str) -> Result<bool> {
    if token.len() != 64 {
        return Ok(false);
    }
    let hash = digest(token);
    let auth: Auth = read(&dir.join("discovery-auth.json"))?.unwrap_or_default();
    Ok(auth.clients.iter().any(|v| equal(v, &hash)))
}
fn enroll(dir: &Path, token: &str) -> Result<Option<String>> {
    if token.len() != 64 {
        return Ok(None);
    }
    let _lock = lock(dir)?;
    let mut auth: Auth = read(&dir.join("discovery-auth.json"))?.unwrap_or_default();
    if !auth
        .invitation
        .as_ref()
        .is_some_and(|v| v.expires > now() && equal(&v.hash, &digest(token)))
    {
        return Ok(None);
    }
    if auth.clients.len() >= MAX_CLIENTS {
        bail!("client enrollment limit reached; revoke old clients locally")
    }
    let bearer = random()?;
    auth.clients.push(digest(&bearer));
    auth.invitation = None;
    write(dir, "discovery-auth.json", &serde_json::to_vec(&auth)?)?;
    Ok(Some(bearer))
}
fn running(dir: &Path) -> bool {
    read::<ProcessIdentity>(&dir.join("proxy.identity"))
        .ok()
        .flatten()
        .is_some_and(|p| {
            state::matches(&p)
                && fs::read_to_string(dir.join("proxy.ready"))
                    .ok()
                    .is_some_and(|v| v.trim() == p.pid.to_string())
        })
}
fn route_id(machine: &Machine, name: &str) -> String {
    digest(&format!("{}\0{name}", machine.id))
}
fn route_url(settings: &ProxySettings, name: &str) -> String {
    let (name, path) = name.split_once('/').map_or((name, ""), |(n, p)| (n, p));
    let url = settings.url(name);
    if path.is_empty() {
        url
    } else {
        format!("{url}/{path}")
    }
}
fn catalog(dir: &Path, machine: Machine, share: bool) -> Result<Catalog> {
    let data: State = read(&dir.join("state.json"))?.unwrap_or_default();
    let settings: ProxySettings = read(&dir.join("proxy.json"))?.unwrap_or_default();
    let proxy_running = running(dir);
    let procs = crate::health::processes();
    let apps = data
        .routes
        .iter()
        .take(MAX_ROUTES)
        .map(|route| {
            let alive = state::route_alive(&data, route);
            let supported = route_target(route).is_ok();
            let remote_access = share && alive && supported;
            App {
                id: route_id(&machine, &route.name),
                name: route.name.clone(),
                local_url: route_url(&settings, &route.name),
                // Same check as `porch list`, so the menu agrees with the CLI.
                health: match crate::health::evaluate(&data, route, &procs) {
                    crate::health::Health::Ok => Health::Healthy,
                    crate::health::Health::NotListening => Health::Unreachable,
                    crate::health::Health::Stopped | crate::health::Health::Dead { .. } => {
                        Health::Stopped
                    }
                },
                remote_access,
                unavailable_reason: if !alive {
                    Some("App process is stopped".into())
                } else if !share {
                    Some("Local only; remote app transport is disabled on this machine".into())
                } else if !supported {
                    Some("Route target is not a supported HTTP URL".into())
                } else {
                    None
                },
            }
        })
        .collect();
    Ok(Catalog {
        version: PROTOCOL_VERSION,
        machine,
        apps,
        proxy_running,
        generated_at: now(),
    })
}
pub fn local_catalog() -> Result<Catalog> {
    let dir = state::dir();
    catalog(
        &dir,
        machine(&dir)?,
        config()?.is_some_and(|c| c.share_apps),
    )
}
fn response(status: StatusCode, message: impl Into<Bytes>) -> Response<Body> {
    let mut r = Response::new(
        Full::new(message.into())
            .map_err(|e: Infallible| match e {})
            .boxed_unsync(),
    );
    *r.status_mut() = status;
    r.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    r
}
fn json<T: Serialize>(value: &T) -> Response<Body> {
    match serde_json::to_vec(value) {
        Ok(bytes) if bytes.len() <= MAX_BODY => {
            let mut r = response(StatusCode::OK, bytes);
            r.headers_mut().insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/json"),
            );
            r
        }
        _ => response(StatusCode::INTERNAL_SERVER_ERROR, "catalog unavailable"),
    }
}
fn route_target(route: &Route) -> Result<url::Url> {
    let target = if route.target.contains("://") {
        route.target.clone()
    } else {
        format!("http://{}", route.target)
    };
    let target = url::Url::parse(&target)?;
    if !matches!(target.scheme(), "http" | "https")
        || target.host().is_none()
        || !target.username().is_empty()
        || target.password().is_some()
    {
        bail!("unsupported route target")
    };
    Ok(target)
}
fn strip(headers: &mut header::HeaderMap) {
    let named: Vec<_> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|v| v.trim().to_owned())
        .collect();
    for name in named {
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
#[derive(Clone)]
struct ContextState {
    dir: PathBuf,
    machine: Machine,
    /// Used when no config file exists (tests); the config file wins so that
    /// `porch pair` can turn sharing on without restarting the agent.
    share_apps: bool,
    client: Upstream,
    tunnels: Arc<tokio::sync::Semaphore>,
    attempts: Arc<Mutex<HashMap<IpAddr, VecDeque<Instant>>>>,
}
impl ContextState {
    fn sharing(&self) -> bool {
        read::<ServerConfig>(&self.dir.join("discovery-config.json"))
            .ok()
            .flatten()
            .map_or(self.share_apps, |c| c.share_apps)
    }
    /// Per-source pairing rate limit, independent of the per-window failure cap.
    fn allow_attempt(&self, source: IpAddr) -> bool {
        let Ok(mut all) = self.attempts.lock() else {
            return false;
        };
        let cutoff = Instant::now() - Duration::from_secs(60);
        all.retain(|_, v| {
            v.retain(|t| *t > cutoff);
            !v.is_empty()
        });
        if all.len() > 4096 {
            return false;
        }
        let entry = all.entry(source).or_default();
        if entry.len() >= PAIR_RATE_LIMIT {
            return false;
        }
        entry.push_back(Instant::now());
        true
    }
}
async fn small_body<T: serde::de::DeserializeOwned>(
    req: Request<Incoming>,
    limit: usize,
) -> Option<T> {
    let body = tokio::time::timeout(
        Duration::from_secs(5),
        Limited::new(req.into_body(), limit).collect(),
    )
    .await
    .ok()?
    .ok()?
    .to_bytes();
    serde_json::from_slice(&body).ok()
}
fn valid_secret(token: &str) -> bool {
    (32..=512).contains(&token.len())
        && token
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
}
async fn pair_code(req: Request<Incoming>, context: &ContextState) -> Result<Response<Body>> {
    let Some(input) = small_body::<PairCodeRequest>(req, 4096).await else {
        return Ok(response(StatusCode::BAD_REQUEST, "invalid pairing request"));
    };
    let code_ok = input.code.len() == 6 && input.code.bytes().all(|b| b.is_ascii_digit());
    let peer_ok = input.version == PROTOCOL_VERSION
        && super::client::validate_machine(&input.machine).is_ok()
        && input.machine.id != context.machine.id
        && super::client::validate_endpoint(&input.endpoint).is_ok()
        && super::client::normalize_pin(&input.fingerprint).is_ok()
        && valid_secret(&input.token);
    if !code_ok || !peer_ok {
        return Ok(response(StatusCode::BAD_REQUEST, "invalid pairing request"));
    }
    // The joiner binds the code to the certificate it pinned. A TLS-terminating
    // interceptor relaying the request presents a different certificate.
    let own = fingerprint_in(&context.dir)?;
    let seen = super::client::normalize_pin(&input.host_fingerprint)
        .map(|p| super::client::hex_pin(&p))
        .unwrap_or_default();
    if !equal(&own, &seen) {
        return Ok(response(
            StatusCode::CONFLICT,
            "certificate mismatch; the pairing connection was intercepted",
        ));
    }
    match try_code(&context.dir, &input.code, &input.machine)? {
        CodeOutcome::Closed => Ok(response(
            StatusCode::GONE,
            "this machine is not waiting to pair; run porch pair on it",
        )),
        CodeOutcome::Wrong => Ok(response(StatusCode::UNAUTHORIZED, "wrong pairing code")),
        CodeOutcome::Accepted(token) => {
            let peer = Peer {
                machine: input.machine,
                endpoint: input.endpoint,
                fingerprint: super::client::hex_pin(&super::client::normalize_pin(
                    &input.fingerprint,
                )?),
                token: input.token,
                sources: vec![Source::Explicit],
                last_seen: now(),
                slug: String::new(),
            };
            if super::client::upsert_peer_in(&context.dir, peer).is_err() {
                revoke_where(&context.dir, |d, _| d == digest(&token))?;
                return Ok(response(
                    StatusCode::CONFLICT,
                    "cannot store the pairing machine; remove an unused peer and retry",
                ));
            }
            Ok(json(&PairResponse {
                version: PROTOCOL_VERSION,
                machine: context.machine.clone(),
                token,
            }))
        }
    }
}
async fn request(
    mut req: Request<Incoming>,
    context: Arc<ContextState>,
    source: IpAddr,
) -> Result<Response<Body>> {
    if req.headers().contains_key(header::ORIGIN) {
        return Ok(response(
            StatusCode::FORBIDDEN,
            "browser origins are not allowed on the agent API",
        ));
    }
    if req.uri().path() == "/v1/pairing" && req.method() == Method::GET {
        return Ok(json(&PairingStatus {
            version: PROTOCOL_VERSION,
            open: pairing_open(&context.dir),
        }));
    }
    let pairing = matches!(req.uri().path(), "/v1/pair" | "/v1/pair-code");
    if pairing && req.method() == Method::POST && !context.allow_attempt(source) {
        return Ok(response(
            StatusCode::TOO_MANY_REQUESTS,
            "too many pairing attempts; wait a minute",
        ));
    }
    if req.uri().path() == "/v1/pair-code" && req.method() == Method::POST {
        return pair_code(req, &context).await;
    }
    if req.uri().path() == "/v1/pair" && req.method() == Method::POST {
        let body = match tokio::time::timeout(
            Duration::from_secs(5),
            Limited::new(req.into_body(), 1024).collect(),
        )
        .await
        {
            Ok(Ok(v)) => v.to_bytes(),
            _ => return Ok(response(StatusCode::BAD_REQUEST, "invalid pairing request")),
        };
        let input: PairRequest = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => return Ok(response(StatusCode::BAD_REQUEST, "invalid pairing request")),
        };
        return Ok(match enroll(&context.dir, &input.token) {
            Ok(Some(token)) => json(&PairResponse {
                version: PROTOCOL_VERSION,
                machine: context.machine.clone(),
                token,
            }),
            Ok(None) => response(StatusCode::UNAUTHORIZED, "invalid or expired pairing token"),
            Err(_) => response(
                StatusCode::CONFLICT,
                "enrollment unavailable; check local agent state",
            ),
        });
    }
    let token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "));
    if !token.is_some_and(|t| paired(&context.dir, t).unwrap_or(false)) {
        return Ok(response(
            StatusCode::UNAUTHORIZED,
            "pair this machine before requesting its catalog",
        ));
    }
    if req.uri().path() == "/v1/catalog" && req.method() == Method::GET {
        return Ok(json(&catalog(
            &context.dir,
            context.machine.clone(),
            context.sharing(),
        )?));
    }
    if req.uri().path() == "/v1/unpair" && req.method() == Method::POST {
        // The caller forgets us; forget it too and revoke its credential.
        let bearer = token.unwrap_or_default().to_owned();
        let auth: Auth = read(&context.dir.join("discovery-auth.json"))?.unwrap_or_default();
        let machine = auth.labels.get(&digest(&bearer)).cloned();
        revoke_where(&context.dir, |d, _| d == digest(&bearer))?;
        if let Some(machine) = machine {
            let _ = super::client::remove_peer_in(&context.dir, &machine);
        }
        return Ok(response(StatusCode::OK, "unpaired"));
    }
    let Some((app_id, app_path)) = req
        .uri()
        .path()
        .strip_prefix("/v1/apps/")
        .and_then(|v| v.split_once("/proxy"))
    else {
        return Ok(response(StatusCode::NOT_FOUND, "unknown agent endpoint"));
    };
    if !app_path.is_empty() && !app_path.starts_with('/') {
        return Ok(response(StatusCode::NOT_FOUND, "unknown app endpoint"));
    }
    if !context.sharing() {
        return Ok(response(
            StatusCode::FORBIDDEN,
            "remote app transport is disabled",
        ));
    }
    if app_id.len() != 64 {
        return Ok(response(StatusCode::NOT_FOUND, "unknown app"));
    }
    let data: State = read(&context.dir.join("state.json"))?.unwrap_or_default();
    let Some(route) = data
        .routes
        .iter()
        .find(|r| route_id(&context.machine, &r.name) == app_id && state::route_alive(&data, r))
    else {
        return Ok(response(
            StatusCode::NOT_FOUND,
            "app is no longer registered",
        ));
    };
    let settings: ProxySettings = read(&context.dir.join("proxy.json"))?.unwrap_or_default();
    let mut target = route_target(route)?;
    let base = target.path().trim_end_matches('/');
    let app_path = if app_path.is_empty() { "/" } else { app_path };
    let path = format!("{base}{app_path}");
    target.set_path(&path);
    target.set_query(req.uri().query());
    // Url::set_path escapes existing percent encodings. Use the validated target authority plus raw app path.
    let uri = format!(
        "{}://{}{}{}",
        target.scheme(),
        &target[url::Position::BeforeHost..url::Position::AfterPort],
        path,
        req.uri()
            .query()
            .map(|q| format!("?{q}"))
            .unwrap_or_default()
    );
    let uri: hyper::Uri = uri.parse().context("invalid route URI")?;
    let websocket = req
        .headers()
        .get(header::UPGRADE)
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"));
    let tunnel_permit = if websocket {
        match context.tunnels.clone().try_acquire_owned() {
            Ok(permit) => Some(permit),
            Err(_) => {
                return Ok(response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "app tunnel limit reached",
                ))
            }
        }
    } else {
        None
    };
    let downstream = websocket.then(|| hyper::upgrade::on(&mut req));
    let origin = req.headers_mut().remove("x-porch-app-origin");
    let auth = req.headers_mut().remove("x-porch-app-authorization");
    let original_host = req.headers_mut().remove("x-porch-app-host");
    strip(req.headers_mut());
    req.headers_mut().remove(header::AUTHORIZATION);
    // Never let a bearer or arbitrary internal control header reach a development app.
    let private: Vec<_> = req
        .headers()
        .keys()
        .filter(|h| h.as_str().starts_with("x-porch-app-"))
        .cloned()
        .collect();
    for h in private {
        req.headers_mut().remove(h);
    }
    if let Some(v) = origin {
        req.headers_mut().insert(header::ORIGIN, v);
    }
    if let Some(v) = auth {
        req.headers_mut().insert(header::AUTHORIZATION, v);
    }
    if let Some(v) = original_host {
        req.headers_mut().insert("x-forwarded-host", v);
    }
    let host = route_url(&settings, &route.name);
    let host = url::Url::parse(&host)?;
    let host = &host[url::Position::BeforeHost..url::Position::AfterPort];
    req.headers_mut().insert(header::HOST, host.parse()?);
    req.headers_mut().insert(
        "x-forwarded-proto",
        header::HeaderValue::from_static("http"),
    );
    if websocket {
        req.headers_mut().insert(
            header::UPGRADE,
            header::HeaderValue::from_static("websocket"),
        );
        req.headers_mut().insert(
            header::CONNECTION,
            header::HeaderValue::from_static("upgrade"),
        );
    }
    *req.uri_mut() = uri;
    *req.version_mut() = Version::HTTP_11;
    let (parts, body) = req.into_parts();
    let req = Request::from_parts(
        parts,
        Limited::new(body, MAX_UPLOAD)
            .map_err(|e| -> BoxError { e })
            .boxed_unsync(),
    );
    let mut upstream =
        match tokio::time::timeout(Duration::from_secs(15), context.client.request(req)).await {
            Ok(Ok(v)) => v,
            _ => {
                return Ok(response(
                    StatusCode::BAD_GATEWAY,
                    "registered app is unavailable",
                ))
            }
        };
    if upstream.status() == StatusCode::SWITCHING_PROTOCOLS {
        if let Some(downstream) = downstream {
            let remote = hyper::upgrade::on(&mut upstream);
            tokio::spawn(async move {
                let _permit = tunnel_permit;
                if let Ok((a, b)) = tokio::try_join!(downstream, remote) {
                    let mut a = TokioIo::new(a);
                    let mut b = TokioIo::new(b);
                    let _ = tokio::io::copy_bidirectional(&mut a, &mut b).await;
                }
            });
        } else {
            return Ok(response(StatusCode::BAD_GATEWAY, "unexpected app upgrade"));
        }
    } else {
        strip(upstream.headers_mut());
    }
    Ok(upstream.map(|body| body.map_err(|e| -> BoxError { Box::new(e) }).boxed_unsync()))
}
fn upstream(dir: &Path) -> Result<Upstream> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    if let Ok(bytes) = fs::read(dir.join("ca.pem")) {
        for cert in rustls_pemfile::certs(&mut bytes.as_slice()).flatten() {
            let _ = roots.add(cert);
        }
    }
    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_root_certificates(roots)
    .with_no_client_auth();
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    http.set_connect_timeout(Some(Duration::from_secs(5)));
    let connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(tls)
        .https_or_http()
        .enable_http1()
        .wrap_connector(http);
    Ok(Client::builder(TokioExecutor::new()).build(connector))
}
struct Advertisement {
    daemon: mdns_sd::ServiceDaemon,
    fullname: String,
}
impl Drop for Advertisement {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}
fn advertise(machine: &Machine, ip: IpAddr, port: u16) -> Result<Advertisement> {
    let daemon = mdns_sd::ServiceDaemon::new()?;
    daemon.disable_interface(mdns_sd::IfKind::All)?;
    daemon.enable_interface(mdns_sd::IfKind::Addr(ip))?;
    let properties = [("id", machine.id.as_str()), ("version", "1")];
    let info = mdns_sd::ServiceInfo::new(
        SERVICE_TYPE,
        &format!("Porch-{}", &machine.id[..12]),
        &format!("porch-{}.local.", &machine.id[..12]),
        ip,
        port,
        &properties[..],
    )?;
    let fullname = info.get_fullname().to_owned();
    daemon.register(info)?;
    Ok(Advertisement { daemon, fullname })
}
pub async fn serve() -> Result<()> {
    let config =
        config()?.context("discovery is disabled; run porch discovery enable --bind IP")?;
    let ip = validate_bind(&config.bind)?;
    let dir = state::dir();
    let machine = machine(&dir)?;
    ensure_certificate(&dir, &config.bind)?;
    let certificates = cert_der(&dir)?;
    let bytes = fs::read(dir.join("discovery-key.pem"))?;
    let key =
        rustls_pemfile::private_key(&mut bytes.as_slice())?.context("missing discovery key")?;
    let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(certificates, key)?;
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    let listener = tokio::net::TcpListener::bind(SocketAddr::new(ip, config.port))
        .await
        .context("binding discovery interface")?;
    let _advertisement = if config.advertise {
        match advertise(&machine, ip, config.port) {
            Ok(v) => Some(v),
            Err(e) => {
                eprintln!("Porch discovery advertisement unavailable: {e}");
                None
            }
        }
    } else {
        None
    };
    let context = Arc::new(ContextState {
        dir: dir.clone(),
        machine,
        share_apps: config.share_apps,
        client: upstream(&dir)?,
        tunnels: Arc::new(tokio::sync::Semaphore::new(64)),
        attempts: Arc::default(),
    });
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
    let slots = Arc::new(tokio::sync::Semaphore::new(64));
    state::atomic_write("discovery.ready", std::process::id().to_string().as_bytes())?;
    connections(listener, acceptor, context, slots).await
}
async fn connections(
    listener: tokio::net::TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
    context: Arc<ContextState>,
    slots: Arc<tokio::sync::Semaphore>,
) -> Result<()> {
    loop {
        let (stream, remote) = listener.accept().await?;
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            drop(stream);
            continue;
        };
        let acceptor = acceptor.clone();
        let context = context.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let Ok(Ok(stream)) =
                tokio::time::timeout(Duration::from_secs(5), acceptor.accept(stream)).await
            else {
                return;
            };
            let service = hyper::service::service_fn(move |req| {
                let context = context.clone();
                async move {
                    Ok::<_, Infallible>(match request(req, context, remote.ip()).await {
                        Ok(v) => v,
                        Err(_) => {
                            response(StatusCode::INTERNAL_SERVER_ERROR, "agent request failed")
                        }
                    })
                }
            });
            let mut builder = hyper::server::conn::http1::Builder::new();
            builder
                .timer(TokioTimer::new())
                .header_read_timeout(Duration::from_secs(10))
                .max_headers(64)
                .max_buf_size(32 * 1024);
            let _ = builder
                .serve_connection(TokioIo::new(stream), service)
                .with_upgrades()
                .await;
        });
    }
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
