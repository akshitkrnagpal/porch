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
/// Source addresses the pairing rate limiter tracks at once.
const MAX_ATTEMPT_SOURCES: usize = 4096;

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
type CachedFile = (
    super::client::FileVersion,
    Arc<dyn std::any::Any + Send + Sync>,
);
/// [`read`] for state files consulted on every request (credentials, sharing,
/// routes): parsed again only when the file changes. Every write replaces the
/// file, so a revocation or a change of sharing applies to the next request.
fn read_cached<T>(path: &Path) -> Result<Option<Arc<T>>>
where
    T: serde::de::DeserializeOwned + Send + Sync + 'static,
{
    static CACHE: Mutex<Option<HashMap<PathBuf, CachedFile>>> = Mutex::new(None);
    let Some(version) = super::client::file_version(path) else {
        return Ok(read::<T>(path)?.map(Arc::new));
    };
    if let Some((v, value)) = CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|c| c.get(path))
    {
        if *v == version {
            if let Ok(value) = value.clone().downcast::<T>() {
                return Ok(Some(value));
            }
        }
    }
    // Keyed by the version seen before reading: a write in between makes the
    // next request read again.
    let Some(value) = read::<T>(path)?.map(Arc::new) else {
        return Ok(None);
    };
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let cache = cache.get_or_insert_with(HashMap::new);
    if cache.len() > 64 {
        cache.clear();
    }
    cache.insert(path.to_owned(), (version, value.clone()));
    Ok(Some(value))
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
const FORMAT_MARKER: &[u8] = b"1\n";
pub(super) fn lock(dir: &Path) -> Result<fs::File> {
    // Preparing the directory through an atomic marker ensures private
    // permissions. Once the marker is in place that was done, so later locks
    // skip the rewrite (and its fsyncs).
    let prepared = fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir())
        && fs::read(dir.join("discovery-format")).is_ok_and(|v| v == FORMAT_MARKER);
    if !prepared {
        write(dir, "discovery-format", FORMAT_MARKER)?;
    }
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
    /// Public salt for the key exchange. Windows written by older versions lack
    /// it and are treated as closed.
    #[serde(default)]
    salt: String,
    /// SHA-256 of `salt \0 code` (the SPAKE2 password); the code itself is never
    /// written to disk.
    hash: String,
    expires: u64,
    failures: u32,
}
/// How the last pairing window ended. `paired`/`error` are what agents before
/// 0.4 wrote and read; `state` and the rest are absent in their records.
#[derive(Clone, Default, Serialize, Deserialize)]
struct PairingResult {
    id: String,
    /// Peer name once the code was accepted.
    paired: Option<String>,
    error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    state: Option<PairState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    peer_id: Option<String>,
    /// Digest of the bearer issued on acceptance. Its first authenticated
    /// request moves the result from accepted to paired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bearer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires: Option<u64>,
}
impl PairingResult {
    fn closed(id: String, expires: u64, state: PairState, reason: Option<&str>) -> Self {
        Self {
            id,
            error: Some(closed_message(state, reason)),
            state: Some(state),
            reason: reason.map(str::to_owned),
            expires: Some(expires),
            ..Self::default()
        }
    }
    /// The state, deriving it for records written by older agents.
    fn state(&self) -> PairState {
        match (self.state, &self.paired, &self.error) {
            (Some(state), _, _) => state,
            (None, Some(_), _) => PairState::Paired,
            (None, None, Some(e)) if e.contains("too many") => PairState::Cancelled,
            (None, None, _) => PairState::Expired,
        }
    }
    fn reason(&self) -> Option<String> {
        match (&self.reason, self.state, &self.error) {
            (Some(r), _, _) => Some(r.clone()),
            (None, None, Some(e)) if e.contains("too many") => Some("too_many_failures".into()),
            _ => None,
        }
    }
}
/// Terminal-friendly explanation of a closed pairing code.
pub fn closed_message(state: PairState, reason: Option<&str>) -> String {
    match (state, reason) {
        (PairState::Expired, _) => "pairing code expired; run porch pair again".into(),
        (PairState::Cancelled, Some("too_many_failures")) => {
            "too many wrong pairing codes; the code was cancelled. Run porch pair again".into()
        }
        (PairState::Cancelled, Some("host_full")) => "this machine cannot store another paired machine; remove an unused one with porch unpair, then run porch pair again".into(),
        (PairState::Cancelled, Some("already_paired")) => "the machine that entered the code claims the ID of a machine already paired here, with a different certificate. If that machine was reinstalled, run porch unpair for it, then run porch pair again".into(),
        (PairState::Cancelled, _) => "pairing cancelled; the code no longer works".into(),
        (PairState::Replaced, _) => {
            "another pairing code was started; this one no longer works".into()
        }
        _ => "pairing window closed".into(),
    }
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
    // The identity is written once, atomically; reading it needs no lock.
    if let Some(value) = read(&dir.join("discovery-machine.json"))? {
        return Ok(value);
    }
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
    let name: String = name
        .chars()
        .filter(|c| !c.is_control() && !super::client::is_format_char(*c))
        .take(128)
        .collect();
    let value = Machine {
        id: random()?,
        name: if name.trim().is_empty() {
            "Porch machine".into()
        } else {
            name
        },
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
    Ok(crate::pem::certs(&mut bytes.as_slice()).collect::<std::io::Result<_>>()?)
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
    let salt = random()?;
    let code = six_digits()?;
    let mut auth: Auth = read(&dir.join("discovery-auth.json"))?.unwrap_or_default();
    if let Some(old) = auth.pairing.take() {
        // Whoever was showing the old code learns why it stopped working.
        let state = if old.expires > now() {
            PairState::Replaced
        } else {
            PairState::Expired
        };
        auth.pairing_result = Some(PairingResult::closed(old.id, old.expires, state, None));
    }
    let expires = now() + lifetime;
    auth.pairing = Some(PairingWindow {
        hash: super::pake::password(&salt, &code),
        salt,
        id: id.clone(),
        expires,
        failures: 0,
    });
    write(dir, "discovery-auth.json", &serde_json::to_vec(&auth)?)?;
    Ok(PairingCode { id, code, expires })
}
/// Where one pairing code stands, as `porch pair status` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairingProgress {
    pub state: PairState,
    pub reason: Option<String>,
    pub expires: Option<u64>,
    pub peer: Option<Machine>,
}
impl PairingProgress {
    pub fn message(&self) -> String {
        closed_message(self.state, self.reason.as_deref())
    }
}
pub fn pairing_progress(id: &str) -> Result<PairingProgress> {
    pairing_progress_in(&state::dir(), id)
}
fn pairing_progress_in(dir: &Path, id: &str) -> Result<PairingProgress> {
    let auth: Auth = read(&dir.join("discovery-auth.json"))?.unwrap_or_default();
    Ok(progress_of(&auth, id))
}
fn progress_of(auth: &Auth, id: &str) -> PairingProgress {
    if let Some(result) = auth.pairing_result.as_ref().filter(|r| r.id == id) {
        return PairingProgress {
            state: result.state(),
            reason: result.reason(),
            expires: result.expires,
            peer: result.paired.as_ref().map(|name| Machine {
                id: result.peer_id.clone().unwrap_or_default(),
                name: name.clone(),
            }),
        };
    }
    let progress = |state, reason: Option<&str>, expires| PairingProgress {
        state,
        reason: reason.map(str::to_owned),
        expires,
        peer: None,
    };
    match &auth.pairing {
        Some(w) if w.id == id && w.failures >= PAIR_CODE_MAX_FAILURES => progress(
            PairState::Cancelled,
            Some("too_many_failures"),
            Some(w.expires),
        ),
        Some(w) if w.id == id && w.expires > now() => {
            progress(PairState::Waiting, None, Some(w.expires))
        }
        Some(w) if w.id == id => progress(PairState::Expired, None, Some(w.expires)),
        // Replaced, revoked, or an ID this machine never issued.
        _ => progress(PairState::Replaced, None, None),
    }
}
/// Cancel a code (Ctrl-C, `porch pair cancel`). Idempotent: a code that already
/// closed or paired keeps its outcome, which is returned.
pub fn close_pairing(id: &str) -> Result<PairingProgress> {
    close_pairing_in(&state::dir(), id)
}
fn close_pairing_in(dir: &Path, id: &str) -> Result<PairingProgress> {
    let _lock = lock(dir)?;
    let mut auth: Auth = read(&dir.join("discovery-auth.json"))?.unwrap_or_default();
    if let Some(w) = auth.pairing.as_ref().filter(|w| w.id == id) {
        let state = if w.expires > now() {
            PairState::Cancelled
        } else {
            PairState::Expired
        };
        let reason = (state == PairState::Cancelled).then_some("user");
        auth.pairing_result = Some(PairingResult::closed(id.into(), w.expires, state, reason));
        auth.pairing = None;
        write(dir, "discovery-auth.json", &serde_json::to_vec(&auth)?)?;
    }
    Ok(progress_of(&auth, id))
}
/// The open window's public salt and SPAKE2 password, if a code is showing.
fn open_window(dir: &Path) -> Option<(String, String)> {
    open_window_state(dir).map(|w| (w.salt, w.hash))
}
fn open_window_state(dir: &Path) -> Option<PairingWindow> {
    read::<Auth>(&dir.join("discovery-auth.json"))
        .ok()
        .flatten()
        .and_then(|a| a.pairing)
        .filter(|w| w.expires > now() && w.failures < PAIR_CODE_MAX_FAILURES && !w.salt.is_empty())
}
#[cfg(test)]
fn pairing_open(dir: &Path) -> bool {
    open_window(dir).is_some()
}
enum CodeOutcome {
    Accepted(String),
    Closed,
    /// The code was right but this machine has no room for another client.
    Full,
}
/// Count a failed key confirmation (a wrong code) against the window `salt`.
/// Five close it, even for the right code.
fn code_failed(dir: &Path, salt: &str) -> Result<()> {
    let _lock = lock(dir)?;
    let mut auth: Auth = read(&dir.join("discovery-auth.json"))?.unwrap_or_default();
    let Some(window) = auth.pairing.as_mut().filter(|w| equal(&w.salt, salt)) else {
        return Ok(());
    };
    window.failures += 1;
    if window.failures >= PAIR_CODE_MAX_FAILURES {
        let (id, expires) = (window.id.clone(), window.expires);
        auth.pairing = None;
        auth.pairing_result = Some(PairingResult::closed(
            id,
            expires,
            PairState::Cancelled,
            Some("too_many_failures"),
        ));
    }
    write(dir, "discovery-auth.json", &serde_json::to_vec(&auth)?)
}
/// The joiner proved the code but claims the identity of another paired
/// machine: close the window so whoever shows the code learns why.
fn code_conflict(dir: &Path, salt: &str) -> Result<()> {
    let _lock = lock(dir)?;
    let mut auth: Auth = read(&dir.join("discovery-auth.json"))?.unwrap_or_default();
    let Some(window) = auth.pairing.take_if(|w| equal(&w.salt, salt)) else {
        return Ok(());
    };
    auth.pairing_result = Some(PairingResult::closed(
        window.id,
        window.expires,
        PairState::Cancelled,
        Some("already_paired"),
    ));
    write(dir, "discovery-auth.json", &serde_json::to_vec(&auth)?)
}
/// The joiner proved the code for window `salt`: consume the window and enroll
/// a new bearer labelled with the peer's machine ID. `replaces` is set when it
/// is a paired machine pairing again (the same certificate), whose previous
/// credentials it replaces; otherwise credentials labelled with its ID stay.
fn code_accepted(dir: &Path, salt: &str, peer: &Machine, replaces: bool) -> Result<CodeOutcome> {
    let _lock = lock(dir)?;
    let mut auth: Auth = read(&dir.join("discovery-auth.json"))?.unwrap_or_default();
    let Some(window) = auth.pairing.as_ref().filter(|w| equal(&w.salt, salt)) else {
        return Ok(CodeOutcome::Closed);
    };
    let (id, expires) = (window.id.clone(), window.expires);
    if window.expires <= now() || window.failures >= PAIR_CODE_MAX_FAILURES {
        let (state, reason) = if window.failures >= PAIR_CODE_MAX_FAILURES {
            (PairState::Cancelled, Some("too_many_failures"))
        } else {
            (PairState::Expired, None)
        };
        auth.pairing = None;
        auth.pairing_result = Some(PairingResult::closed(id, expires, state, reason));
        write(dir, "discovery-auth.json", &serde_json::to_vec(&auth)?)?;
        return Ok(CodeOutcome::Closed);
    }
    auth.pairing = None;
    // Re-pairing replaces this machine's previous credential.
    let stale: Vec<String> = auth
        .labels
        .iter()
        .filter(|(_, m)| replaces && **m == peer.id)
        .map(|(d, _)| d.clone())
        .collect();
    let mut clients = auth.clients.clone();
    clients.retain(|c| !stale.contains(c));
    if clients.len() >= MAX_CLIENTS {
        auth.pairing_result = Some(PairingResult::closed(
            id,
            expires,
            PairState::Cancelled,
            Some("host_full"),
        ));
        write(dir, "discovery-auth.json", &serde_json::to_vec(&auth)?)?;
        return Ok(CodeOutcome::Full);
    }
    auth.clients = clients;
    auth.labels.retain(|d, _| !stale.contains(d));
    let bearer = random()?;
    auth.clients.push(digest(&bearer));
    auth.labels.insert(digest(&bearer), peer.id.clone());
    auth.pairing_result = Some(PairingResult {
        id,
        paired: Some(peer.name.clone()),
        state: Some(PairState::Accepted),
        peer_id: Some(peer.id.clone()),
        bearer: Some(digest(&bearer)),
        expires: Some(expires),
        ..PairingResult::default()
    });
    write(dir, "discovery-auth.json", &serde_json::to_vec(&auth)?)?;
    Ok(CodeOutcome::Accepted(bearer))
}
/// The host could not store the joiner after accepting its code: take back the
/// credential and report the code as cancelled for lack of room.
fn reject_accepted(dir: &Path, bearer: &str) -> Result<()> {
    let _lock = lock(dir)?;
    let mut auth: Auth = read(&dir.join("discovery-auth.json"))?.unwrap_or_default();
    let hash = digest(bearer);
    auth.clients.retain(|c| *c != hash);
    auth.labels.remove(&hash);
    if let Some(r) = auth
        .pairing_result
        .as_mut()
        .filter(|r| r.bearer.as_deref() == Some(hash.as_str()))
    {
        *r = PairingResult::closed(
            r.id.clone(),
            r.expires.unwrap_or_default(),
            PairState::Cancelled,
            Some("host_full"),
        );
    }
    write(dir, "discovery-auth.json", &serde_json::to_vec(&auth)?)
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
    revoke_where(&state::dir(), |d, _| d == digest(bearer)).map(|_| ())
}
/// What revoking one machine's access did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Revoked {
    /// Credentials issued to that machine, now revoked.
    pub revoked: usize,
    /// Credentials whose machine isn't recorded (enrolled with `porch peers
    /// add` from Porch 0.5.2 or earlier, or left by an interrupted pairing).
    /// Any of them could be that machine's.
    pub unidentified: usize,
}
/// Revoke every credential this machine issued to `machine_id`.
pub fn revoke_machine(machine_id: &str) -> Result<Revoked> {
    revoke_machine_in(&state::dir(), machine_id)
}
fn revoke_machine_in(dir: &Path, machine_id: &str) -> Result<Revoked> {
    let revoked = revoke_where(dir, |_, m| m == Some(machine_id))?;
    let auth: Auth = read(&dir.join("discovery-auth.json"))?.unwrap_or_default();
    let unidentified = auth
        .clients
        .iter()
        .filter(|d| auth.labels.get(*d).is_none_or(|m| m == PENDING_LABEL))
        .count();
    Ok(Revoked {
        revoked,
        unidentified,
    })
}
/// Label of a credential minted for a pairing that hasn't finished yet.
pub const PENDING_LABEL: &str = "pending";
/// Remove the credentials `f` selects; returns how many.
fn revoke_where(dir: &Path, f: impl Fn(&str, Option<&str>) -> bool) -> Result<usize> {
    let _lock = lock(dir)?;
    let mut auth: Auth = read(&dir.join("discovery-auth.json"))?.unwrap_or_default();
    let labels = auth.labels.clone();
    let before = auth.clients.len();
    auth.clients
        .retain(|d| !f(d, labels.get(d).map(String::as_str)));
    auth.labels.retain(|d, m| !f(d, Some(m)));
    write(dir, "discovery-auth.json", &serde_json::to_vec(&auth)?)?;
    Ok(before - auth.clients.len())
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
#[cfg(test)]
fn paired(dir: &Path, token: &str) -> Result<bool> {
    if token.len() != 64 {
        return Ok(false);
    }
    let hash = digest(token);
    let auth: Auth = read(&dir.join("discovery-auth.json"))?.unwrap_or_default();
    Ok(auth.clients.iter().any(|v| equal(v, &hash)))
}
/// Authenticate a bearer. The first request made with the credential a pairing
/// code just issued proves the joiner stored it, which completes that pairing.
fn authorize(dir: &Path, token: &str) -> Result<bool> {
    if token.len() != 64 {
        return Ok(false);
    }
    let hash = digest(token);
    let auth = read_cached::<Auth>(&dir.join("discovery-auth.json"))?.unwrap_or_default();
    if !auth.clients.iter().any(|v| equal(v, &hash)) {
        return Ok(false);
    }
    let confirms = |auth: &Auth| {
        auth.pairing_result.as_ref().is_some_and(|r| {
            r.state == Some(PairState::Accepted) && r.bearer.as_deref() == Some(hash.as_str())
        })
    };
    if confirms(&auth) {
        let _lock = lock(dir)?;
        let mut auth: Auth = read(&dir.join("discovery-auth.json"))?.unwrap_or_default();
        if confirms(&auth) {
            if let Some(r) = auth.pairing_result.as_mut() {
                r.state = Some(PairState::Paired);
                r.bearer = None;
            }
            write(dir, "discovery-auth.json", &serde_json::to_vec(&auth)?)?;
        }
    }
    Ok(true)
}
/// Explicit enrollment with a pair token. The credential is labelled with the
/// enrolling machine's ID when it sends one, so `porch unpair` can revoke it.
fn enroll(dir: &Path, token: &str, machine: Option<&str>) -> Result<Option<String>> {
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
    if let Some(machine) = machine {
        auth.labels.insert(digest(&bearer), machine.into());
    }
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
/// The app catalog. `for_peer` is the copy sent to paired machines, which
/// never mentions private apps; the local copy (tray, `porch open`) keeps them.
fn catalog(dir: &Path, machine: Machine, share: bool, for_peer: bool) -> Result<Catalog> {
    let data: State = read(&dir.join("state.json"))?.unwrap_or_default();
    let settings: ProxySettings = read(&dir.join("proxy.json"))?.unwrap_or_default();
    let proxy_running = running(dir);
    let procs = crate::health::processes();
    let apps = data
        .routes
        .iter()
        .filter(|route| !(for_peer && data.private.contains(&route.name)))
        .take(MAX_ROUTES)
        .map(|route| {
            let alive = state::route_alive(&data, route);
            let supported = route_target(route).is_ok();
            let private = data.private.contains(&route.name);
            let remote_access = share && alive && supported && !private;
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
                unavailable_reason: if private {
                    Some("Private to this machine (--private)".into())
                } else if !alive {
                    Some("App process is stopped".into())
                } else if !share {
                    Some("Local only; remote app transport is disabled on this machine".into())
                } else if !supported {
                    Some(
                        "Only apps on this machine can be shared; this route points elsewhere"
                            .into(),
                    )
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
        false,
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
    // Paired machines may only reach apps on this machine. An alias to another
    // host (a LAN service, a cloud metadata address) must not become a relay.
    let local = match target.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };
    if !local {
        bail!("route target is not on this machine")
    }
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
/// Headers through which a client could claim another address, host, scheme
/// or path to the app. A paired machine's request never carries them through.
fn strip_forwarding(headers: &mut header::HeaderMap) {
    let names: Vec<_> = headers
        .keys()
        .filter(|h| {
            let h = h.as_str();
            h.starts_with("x-forwarded-")
                || h.starts_with("x-original-")
                || matches!(
                    h,
                    "forwarded"
                        | "x-real-ip"
                        | "x-client-ip"
                        | "true-client-ip"
                        | "x-rewrite-url"
                        | "x-host"
                        | "x-proxyuser-ip"
                        | "cf-connecting-ip"
                        | "fastly-client-ip"
                        | "x-cluster-client-ip"
                )
        })
        .cloned()
        .collect();
    for name in names {
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
    /// For `--upstream` aliases: verified, and `--insecure` (loopback only).
    loopback: crate::upstream::LoopbackClient<Body>,
    loopback_insecure: crate::upstream::LoopbackClient<Body>,
    tunnels: Arc<tokio::sync::Semaphore>,
    attempts: Arc<Mutex<HashMap<IpAddr, VecDeque<Instant>>>>,
    /// In-progress code-pairing key exchanges.
    sessions: Arc<Mutex<Sessions>>,
    catalog: Arc<tokio::sync::Mutex<Option<CatalogCopy>>>,
}
#[derive(Default)]
struct Sessions {
    /// By random session ID.
    live: HashMap<String, PairSession>,
    /// Key confirmations being checked right now. Each may become a wrong
    /// guess, so together with the window's recorded failures they never
    /// exceed the cap.
    checking: u32,
}
impl Sessions {
    fn expire(&mut self) {
        self.live
            .retain(|_, s| s.started.elapsed() < PAIR_SESSION_LIFETIME);
    }
    /// Drop the oldest exchange that hasn't proved the code yet, among those
    /// `f` selects. Proved ones are kept: they hold the right code.
    fn evict_oldest(&mut self, f: impl Fn(&PairSession) -> bool) -> bool {
        let oldest = self
            .live
            .iter()
            .filter(|(_, s)| !s.confirmed && f(s))
            .min_by_key(|(_, s)| s.started)
            .map(|(id, _)| id.clone());
        oldest.is_some_and(|id| self.live.remove(&id).is_some())
    }
}
type CatalogKey = (
    bool,
    Option<super::client::FileVersion>,
    Option<super::client::FileVersion>,
);
type CatalogCopy = (Instant, CatalogKey, Catalog);
const CATALOG_REUSE: Duration = Duration::from_secs(1);
/// How long an app may take to start its response after the last upload
/// data arrived. Long-polls and slow renders fit; a hung app still ends.
const RESPONSE_IDLE: Duration = Duration::from_secs(300);
struct PairSession {
    salt: String,
    key: super::pake::Key,
    confirmed: bool,
    started: Instant,
    source: IpAddr,
}
/// A key exchange must finish within this time (the joiner may start its agent).
const PAIR_SESSION_LIFETIME: Duration = Duration::from_secs(60);
const MAX_PAIR_SESSIONS: usize = 16;
/// Unproved exchanges one source address may hold; a new one replaces its oldest.
const MAX_PAIR_SESSIONS_PER_SOURCE: usize = 2;
/// Releases a counted key confirmation however its check ends.
struct Checking<'a>(&'a Mutex<Sessions>);
impl Drop for Checking<'_> {
    fn drop(&mut self) {
        let mut all = self.0.lock().unwrap_or_else(|p| p.into_inner());
        all.checking = all.checking.saturating_sub(1);
    }
}
impl ContextState {
    fn sessions(&self) -> std::sync::MutexGuard<'_, Sessions> {
        self.sessions.lock().unwrap_or_else(|p| p.into_inner())
    }
    fn take_session(&self, id: &str) -> Option<PairSession> {
        let mut all = self.sessions();
        all.expire();
        all.live.remove(id)
    }
    fn sharing(&self) -> bool {
        read_cached::<ServerConfig>(&self.dir.join("discovery-config.json"))
            .ok()
            .flatten()
            .map_or(self.share_apps, |c| c.share_apps)
    }
    /// The catalog for paired machines. Building it runs `ps` and probes
    /// every app, so a copy is reused for a second while sharing and the
    /// route and proxy files are unchanged, and concurrent requests share one
    /// build. It never decides access: each relayed request is checked itself.
    async fn peer_catalog(&self, share: bool) -> Result<Catalog> {
        let key = (
            share,
            super::client::file_version(&self.dir.join("state.json")),
            super::client::file_version(&self.dir.join("proxy.json")),
        );
        let mut cached = self.catalog.lock().await;
        if let Some((at, k, catalog)) = cached.as_ref() {
            if *k == key && at.elapsed() < CATALOG_REUSE {
                return Ok(catalog.clone());
            }
        }
        let (dir, machine) = (self.dir.clone(), self.machine.clone());
        let catalog =
            tokio::task::spawn_blocking(move || catalog(&dir, machine, share, true)).await??;
        *cached = Some((Instant::now(), key, catalog.clone()));
        Ok(catalog)
    }
    /// Per-source pairing rate limit, independent of the per-window failure cap.
    fn allow_attempt(&self, source: IpAddr) -> bool {
        // A panic elsewhere must not lock everyone out until restart.
        let mut all = self
            .attempts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // `Instant` can be younger than a minute (Windows counts from boot),
        // where subtracting would panic.
        if let Some(cutoff) = Instant::now().checked_sub(Duration::from_secs(60)) {
            all.retain(|_, v| {
                v.retain(|t| *t > cutoff);
                !v.is_empty()
            });
        }
        // Too many sources to track: forget the one seen longest ago rather
        // than refusing everyone. Each key exchange still allows one guess and
        // the code's own cap still holds.
        if all.len() >= MAX_ATTEMPT_SOURCES && !all.contains_key(&source) {
            let oldest = all
                .iter()
                .min_by_key(|(_, v)| v.back().copied())
                .map(|(ip, _)| *ip);
            if let Some(ip) = oldest {
                all.remove(&ip);
            }
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
/// `/v1/pair-code` errors: JSON with a stable code. Status codes match the
/// plain-text bodies of older agents.
fn pair_error(status: StatusCode, code: &str, message: &str) -> Response<Body> {
    let body = serde_json::to_vec(&PairCodeError {
        error: PairCodeErrorBody {
            code: code.into(),
            message: message.into(),
        },
    })
    .unwrap_or_default();
    let mut r = response(status, body);
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    r
}
fn host_full() -> Response<Body> {
    pair_error(
        StatusCode::CONFLICT,
        "host_full",
        "cannot store the pairing machine; remove an unused peer and retry",
    )
}
fn bad_pairing_request() -> Response<Body> {
    pair_error(
        StatusCode::BAD_REQUEST,
        "protocol",
        "invalid pairing request",
    )
}
fn not_waiting() -> Response<Body> {
    pair_error(
        StatusCode::GONE,
        "not_waiting",
        "this machine is not waiting to pair; run porch pair on it",
    )
}
fn session_expired() -> Response<Body> {
    pair_error(
        StatusCode::GONE,
        "not_waiting",
        "pairing session expired; run porch pair CODE again",
    )
}
/// Step 1: answer the joiner's SPAKE2 message. Nothing here reveals the code
/// or lets a caller test a guess; only a key confirmation (step 2) does.
async fn pair_start(
    req: Request<Incoming>,
    context: &ContextState,
    source: IpAddr,
) -> Result<Response<Body>> {
    let Some(input) = small_body::<PairStart>(req, 1024).await else {
        return Ok(bad_pairing_request());
    };
    if input.pairing != PAIRING_VERSION {
        return Ok(pair_error(
            StatusCode::BAD_REQUEST,
            "protocol",
            "unsupported pairing protocol; update Porch on both machines",
        ));
    }
    let Some((salt, password)) = open_window(&context.dir) else {
        return Ok(not_waiting());
    };
    if !equal(&salt, &input.salt) {
        return Ok(pair_error(
            StatusCode::GONE,
            "not_waiting",
            "the pairing code on this machine changed; enter the new code",
        ));
    }
    let Ok((msg, key)) = super::pake::host_respond(&password, &input.msg) else {
        return Ok(bad_pairing_request());
    };
    let session = random()?;
    let mut all = context.sessions();
    all.expire();
    // Abandoned exchanges must not fill the table: one source keeps only its
    // newest few, and a full table drops the oldest unproved one.
    while all
        .live
        .values()
        .filter(|s| s.source == source && !s.confirmed)
        .count()
        >= MAX_PAIR_SESSIONS_PER_SOURCE
    {
        if !all.evict_oldest(|s| s.source == source) {
            break;
        }
    }
    if all.live.len() >= MAX_PAIR_SESSIONS && !all.evict_oldest(|_| true) {
        return Ok(pair_error(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "too many pairing attempts; wait a minute",
        ));
    }
    all.live.insert(
        session.clone(),
        PairSession {
            salt,
            key,
            confirmed: false,
            started: Instant::now(),
            source,
        },
    );
    Ok(json(&PairStarted { session, msg }))
}
/// Step 2: the joiner proves the key, bound to the certificate it pinned. A
/// wrong proof is a wrong code (counted); a relaying interceptor presenting
/// another certificate fails the same way. Only then does the host prove itself.
async fn pair_confirm(req: Request<Incoming>, context: &ContextState) -> Result<Response<Body>> {
    let Some(input) = small_body::<PairConfirm>(req, 1024).await else {
        return Ok(bad_pairing_request());
    };
    let pin = super::client::normalize_pin(&fingerprint_in(&context.dir)?)?;
    // Count the guess before testing it, atomically with reading the window's
    // failures: concurrent confirmations can't test more codes than the cap,
    // and none is tested once the code is closed.
    let (mut session, checking) = {
        let mut all = context.sessions();
        all.expire();
        let Some(session) = all.live.remove(&input.session).filter(|s| !s.confirmed) else {
            return Ok(session_expired());
        };
        let Some(window) =
            open_window_state(&context.dir).filter(|w| equal(&w.salt, &session.salt))
        else {
            return Ok(not_waiting());
        };
        if window.failures + all.checking >= PAIR_CODE_MAX_FAILURES {
            return Ok(pair_error(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                "too many pairing attempts; wait a minute",
            ));
        }
        all.checking += 1;
        (session, Checking(&context.sessions))
    };
    if !session.key.check_joiner_confirm(&pin, &input.confirm) {
        code_failed(&context.dir, &session.salt)?;
        drop(checking);
        return Ok(pair_error(
            StatusCode::UNAUTHORIZED,
            "wrong_code",
            "wrong pairing code",
        ));
    }
    drop(checking);
    if open_window(&context.dir).is_none_or(|(salt, _)| !equal(&salt, &session.salt)) {
        return Ok(not_waiting());
    }
    let confirm = session.key.host_confirm(&pin);
    session.confirmed = true;
    context.sessions().live.insert(input.session, session);
    Ok(json(&PairConfirmed { confirm }))
}
/// Step 3: both sides proved the code. Store the joiner, enroll its credential.
async fn pair_finish(req: Request<Incoming>, context: &ContextState) -> Result<Response<Body>> {
    let Some(input) = small_body::<PairFinish>(req, 4096).await else {
        return Ok(bad_pairing_request());
    };
    let Some(session) = context.take_session(&input.session).filter(|s| s.confirmed) else {
        return Ok(session_expired());
    };
    let peer_ok = super::client::validate_machine(&input.machine).is_ok()
        && input.machine.id != context.machine.id
        && input.machine.id != PENDING_LABEL
        && super::client::validate_endpoint(&input.endpoint).is_ok()
        && super::client::normalize_pin(&input.fingerprint).is_ok()
        && valid_secret(&input.token)
        && session.key.check_joiner_payload(
            &input.machine,
            &input.endpoint,
            &input.fingerprint,
            &input.token,
            &input.mac,
        );
    if !peer_ok {
        return Ok(bad_pairing_request());
    }
    // Machine IDs are public. A paired machine is replaced only by itself
    // (its pinned certificate), never by whoever claims its ID.
    let paired = super::client::peers_in(&context.dir)?;
    if let Some(conflict) =
        super::client::identity_conflict(&paired, &input.machine.id, &input.fingerprint)
    {
        code_conflict(&context.dir, &session.salt)?;
        return Ok(pair_error(
            StatusCode::CONFLICT,
            "already_paired",
            &format!(
                "a different machine is already paired here as {}; if this machine was reinstalled, run porch unpair for it on that machine, then pair again",
                conflict.existing.name
            ),
        ));
    }
    let replaces = paired.iter().any(|p| p.machine.id == input.machine.id);
    match code_accepted(&context.dir, &session.salt, &input.machine, replaces)? {
        CodeOutcome::Closed => Ok(not_waiting()),
        CodeOutcome::Full => Ok(host_full()),
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
            if let Err(e) = super::client::upsert_peer_in(&context.dir, peer) {
                reject_accepted(&context.dir, &token)?;
                if e.is::<super::client::IdentityConflict>() {
                    return Ok(pair_error(
                        StatusCode::CONFLICT,
                        "already_paired",
                        "a different machine is already paired here with this machine's ID",
                    ));
                }
                return Ok(host_full());
            }
            let mac = session.key.host_payload(&context.machine, &token);
            Ok(json(&PairFinished {
                version: PROTOCOL_VERSION,
                machine: context.machine.clone(),
                token,
                mac,
            }))
        }
    }
}
async fn request(
    mut req: Request<Incoming>,
    context: Arc<ContextState>,
    source: IpAddr,
    connection: &Connection,
) -> Result<Response<Body>> {
    if req.headers().contains_key(header::ORIGIN) {
        return Ok(response(
            StatusCode::FORBIDDEN,
            "browser origins are not allowed on the agent API",
        ));
    }
    if req.uri().path() == "/v1/pairing" && req.method() == Method::GET {
        let salt = open_window(&context.dir).map(|(salt, _)| salt);
        return Ok(json(&PairingStatus {
            version: PROTOCOL_VERSION,
            open: salt.is_some(),
            pairing: PAIRING_VERSION,
            salt,
        }));
    }
    // Each key exchange allows one guess, so limiting starts limits guesses;
    // confirm and finish need a session that a start created.
    let pairing = matches!(
        req.uri().path(),
        "/v1/pair" | "/v1/pair-code" | "/v1/pair-code/start"
    );
    if pairing && req.method() == Method::POST && !context.allow_attempt(source) {
        let message = "too many pairing attempts; wait a minute";
        return Ok(if req.uri().path().starts_with("/v1/pair-code") {
            pair_error(StatusCode::TOO_MANY_REQUESTS, "rate_limited", message)
        } else {
            response(StatusCode::TOO_MANY_REQUESTS, message)
        });
    }
    if req.method() == Method::POST {
        match req.uri().path() {
            // Version 1 sent the code and a live credential to whoever answered.
            // Refuse it without reading the body or touching the window.
            "/v1/pair-code" => {
                return Ok(pair_error(
                    StatusCode::UPGRADE_REQUIRED,
                    "protocol",
                    "this machine uses a newer, authenticated pairing protocol; update Porch on the joining machine",
                ))
            }
            "/v1/pair-code/start" => return pair_start(req, &context, source).await,
            "/v1/pair-code/confirm" => return pair_confirm(req, &context).await,
            "/v1/pair-code/finish" => return pair_finish(req, &context).await,
            _ => {}
        }
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
        // Older clients send no machine; an invalid one is refused before the
        // single-use token is spent.
        let machine = match &input.machine {
            Some(m)
                if super::client::validate_machine(m).is_err()
                    || m.id == context.machine.id
                    || m.id == PENDING_LABEL =>
            {
                return Ok(response(StatusCode::BAD_REQUEST, "invalid pairing request"))
            }
            Some(m) => Some(m.id.as_str()),
            None => None,
        };
        return Ok(match enroll(&context.dir, &input.token, machine) {
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
    if !token.is_some_and(|t| authorize(&context.dir, t).unwrap_or(false)) {
        return Ok(response(
            StatusCode::UNAUTHORIZED,
            "pair this machine before requesting its catalog",
        ));
    }
    connection.authenticated(source);
    if req.uri().path() == "/v1/catalog" && req.method() == Method::GET {
        return Ok(json(&context.peer_catalog(context.sharing()).await?));
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
    let data = read_cached::<State>(&context.dir.join("state.json"))?.unwrap_or_default();
    let Some(route) = data.routes.iter().find(|r| {
        route_id(&context.machine, &r.name) == app_id
            && !data.private.contains(&r.name)
            && state::route_alive(&data, r)
    }) else {
        return Ok(response(
            StatusCode::NOT_FOUND,
            "app is no longer registered",
        ));
    };
    let settings =
        read_cached::<ProxySettings>(&context.dir.join("proxy.json"))?.unwrap_or_default();
    // The catalog never offers such a route; refuse it as sharing would.
    let Ok(mut target) = route_target(route) else {
        return Ok(response(
            StatusCode::FORBIDDEN,
            "Only apps on this machine can be shared; this route points elsewhere",
        ));
    };
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
    // `--upstream` aliases: the same scheme, Host and certificate policy as
    // the local proxy uses, still dialing only this computer.
    let upstream_spec = data.upstreams.get(&route.name).cloned();
    let uri = match &upstream_spec {
        Some(up) => {
            let pq = format!(
                "{path}{}",
                req.uri()
                    .query()
                    .map(|q| format!("?{q}"))
                    .unwrap_or_default()
            );
            crate::upstream::request(up, target.port_or_known_default().unwrap_or(80), &pq).0
        }
        None => uri,
    };
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
    let original_proto = req
        .headers_mut()
        .remove("x-porch-app-proto")
        .filter(|v| v == "https" || v == "http");
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
    // The paired machine cannot speak for the client address, host or scheme
    // the app sees; those are set below from what this agent knows.
    strip_forwarding(req.headers_mut());
    if let Some(v) = origin {
        req.headers_mut().insert(header::ORIGIN, v);
    }
    if let Some(v) = auth {
        req.headers_mut().insert(header::AUTHORIZATION, v);
    }
    let public = route_url(&settings, &route.name);
    let public = url::Url::parse(&public)?;
    let host = match upstream_spec.as_ref().and_then(|u| u.authority.clone()) {
        Some(authority) => authority,
        None => public[url::Position::BeforeHost..url::Position::AfterPort].to_owned(),
    };
    req.headers_mut().insert(header::HOST, host.parse()?);
    // The scheme and host the browser used on the paired machine, so apps
    // build correct redirects and secure cookies, and the paired machine's
    // address as the client.
    let proto = original_proto.unwrap_or(header::HeaderValue::from_static("http"));
    let forwarded_host = original_host
        .filter(|v| {
            v.to_str()
                .is_ok_and(|h| h.parse::<hyper::http::uri::Authority>().is_ok())
        })
        .unwrap_or(host.parse()?);
    let forwarded_port = forwarded_host
        .to_str()
        .ok()
        .and_then(|h| h.parse::<hyper::http::uri::Authority>().ok())
        .and_then(|a| a.port_u16())
        .unwrap_or(if proto == "https" { 443 } else { 80 });
    req.headers_mut()
        .insert("x-forwarded-for", source.to_string().parse()?);
    req.headers_mut().insert("x-forwarded-host", forwarded_host);
    req.headers_mut().insert("x-forwarded-proto", proto);
    req.headers_mut()
        .insert("x-forwarded-port", forwarded_port.to_string().parse()?);
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
    let declared = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > MAX_UPLOAD as u64) {
        return Ok(too_large());
    }
    let (parts, body) = req.into_parts();
    let (body, last) = super::client::Watched::new(body);
    let over_limit = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = over_limit.clone();
    let req = Request::from_parts(
        parts,
        Limited::new(body, MAX_UPLOAD)
            .map_err(move |e| -> BoxError {
                if e.is::<http_body_util::LengthLimitError>() {
                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                e
            })
            .boxed_unsync(),
    );
    // Connecting to the app is bounded by the clients' connect timeouts; the
    // response may then take as long as it needs, counted from the end of
    // the upload.
    let sent = async {
        match &upstream_spec {
            Some(up) if up.insecure => context.loopback_insecure.request(req).await,
            Some(_) => context.loopback.request(req).await,
            None => context.client.request(req).await,
        }
    };
    let mut upstream = match super::client::within_idle(sent, &last, RESPONSE_IDLE).await {
        Some(Ok(v)) => v,
        Some(Err(_)) if over_limit.load(std::sync::atomic::Ordering::Relaxed) => {
            return Ok(too_large())
        }
        Some(Err(_)) => {
            return Ok(response(
                StatusCode::BAD_GATEWAY,
                "registered app is unavailable",
            ))
        }
        None => {
            return Ok(response(
                StatusCode::GATEWAY_TIMEOUT,
                "registered app did not answer in time",
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
        // An `--upstream` backend's redirects to itself become redirects to
        // the app's Porch URL, which the paired machine's relay maps onward.
        if let Some(up) = &upstream_spec {
            let public_origin = public.origin().ascii_serialization();
            let rewritten = upstream
                .headers()
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|l| {
                    crate::upstream::rewrite_location(
                        l,
                        &crate::upstream::origins(up),
                        &public_origin,
                    )
                })
                .and_then(|l| header::HeaderValue::from_str(&l).ok());
            if let Some(v) = rewritten {
                upstream.headers_mut().insert(header::LOCATION, v);
            }
        }
    }
    Ok(upstream.map(|body| body.map_err(|e| -> BoxError { Box::new(e) }).boxed_unsync()))
}
fn too_large() -> Response<Body> {
    response(
        StatusCode::PAYLOAD_TOO_LARGE,
        format!(
            "Uploads to an app on a paired machine are limited to {} MB",
            MAX_UPLOAD / (1024 * 1024)
        ),
    )
}
fn upstream(dir: &Path) -> Result<Upstream> {
    let tls = tls_roots(dir)?;
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    http.set_nodelay(true);
    http.set_connect_timeout(Some(Duration::from_secs(5)));
    let connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(tls)
        .https_or_http()
        .enable_http1()
        .wrap_connector(http);
    Ok(Client::builder(TokioExecutor::new()).build(connector))
}
/// TLS for reaching apps: the system roots plus Porch's own CA.
fn tls_roots(dir: &Path) -> Result<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    if let Ok(bytes) = fs::read(dir.join("ca.pem")) {
        for cert in crate::pem::certs(&mut bytes.as_slice()).flatten() {
            let _ = roots.add(cert);
        }
    }
    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(tls)
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
    let key = crate::pem::private_key(&mut bytes.as_slice())?.context("missing discovery key")?;
    let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(certificates, key)?;
    // HTTP/2 lets a paired machine's relay multiplex a page's requests on one
    // connection; HTTP/1.1 stays for older Porch versions and WebSockets.
    tls.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
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
        loopback: crate::upstream::client(tls_roots(&dir)?),
        loopback_insecure: crate::upstream::client(crate::upstream::insecure_tls()?),
        tunnels: Arc::new(tokio::sync::Semaphore::new(MAX_TUNNELS)),
        attempts: Arc::default(),
        sessions: Arc::default(),
        catalog: Arc::default(),
    });
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
    raise_open_file_limit();
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    state::atomic_write("discovery.ready", std::process::id().to_string().as_bytes())?;
    connections(listener, acceptor, context, slots).await
}
/// HTTP/2 for paired machines' relays. Upload data counts against the
/// connection's window until the app reads it, so the connection window is
/// far above the per-request one: an app that leaves one upload unread must
/// not stall every other request from that machine.
pub(super) fn http2_settings<E>(mut builder: hyper_util::server::conn::auto::Http2Builder<'_, E>) {
    builder
        .timer(TokioTimer::new())
        .initial_stream_window_size(super::client::STREAM_WINDOW)
        .initial_connection_window_size(super::client::CONNECTION_WINDOW)
        .max_concurrent_streams(256)
        .max_header_list_size(32 * 1024)
        .keep_alive_interval(Some(Duration::from_secs(20)))
        .keep_alive_timeout(Duration::from_secs(10));
}
/// Connections plus the app connections and tunnels they relay need more
/// descriptors than macOS's default soft limit of 256.
#[cfg(unix)]
fn raise_open_file_limit() {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit/setrlimit read and write the struct passed by pointer.
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) == 0 && limit.rlim_cur < 4096 {
            limit.rlim_cur = limit.rlim_max.min(4096);
            libc::setrlimit(libc::RLIMIT_NOFILE, &limit);
        }
    }
}
#[cfg(not(unix))]
fn raise_open_file_limit() {}
/// Connections the agent serves at once.
const MAX_CONNECTIONS: usize = 256;
/// Upgraded connections (WebSockets) relayed at once. They release their
/// connection slot when they upgrade, so they don't crowd out catalog
/// requests or pairing.
const MAX_TUNNELS: usize = 128;
/// How many connections sources get. A source that has authenticated
/// (or is a paired machine's address) is trusted; any other device on the
/// network shares a smaller pool, a few connections each, so it can't take
/// the slots paired machines need.
#[derive(Clone, Copy, Debug)]
struct ConnectionLimits {
    per_source: usize,
    untrusted: usize,
    per_untrusted_source: usize,
    /// A connection that has never authenticated is closed after this long
    /// without a request in progress...
    idle: Duration,
    /// ...and after this long in any case. Pairing needs less than a minute.
    lifetime: Duration,
}
impl Default for ConnectionLimits {
    fn default() -> Self {
        Self {
            per_source: 128,
            untrusted: 64,
            per_untrusted_source: 8,
            idle: Duration::from_secs(15),
            lifetime: Duration::from_secs(90),
        }
    }
}
/// How long an address stays trusted after its last authenticated request.
const TRUSTED_SOURCE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_TRUSTED_SOURCES: usize = 1024;
#[derive(Default)]
struct Admission {
    open: HashMap<IpAddr, usize>,
    untrusted: usize,
    trusted: HashMap<IpAddr, Instant>,
}
impl Admission {
    fn is_trusted(&self, ip: IpAddr) -> bool {
        self.trusted
            .get(&ip)
            .is_some_and(|t| t.elapsed() < TRUSTED_SOURCE_TTL)
    }
    fn trust(&mut self, ip: IpAddr) {
        if self.trusted.len() >= MAX_TRUSTED_SOURCES && !self.trusted.contains_key(&ip) {
            self.trusted.retain(|_, t| t.elapsed() < TRUSTED_SOURCE_TTL);
            if self.trusted.len() >= MAX_TRUSTED_SOURCES {
                let oldest = self
                    .trusted
                    .iter()
                    .min_by_key(|(_, t)| **t)
                    .map(|(ip, _)| *ip);
                if let Some(oldest) = oldest {
                    self.trusted.remove(&oldest);
                }
            }
        }
        self.trusted.insert(ip, Instant::now());
    }
}
/// Admit a connection from `source`, or refuse it when its pool is full.
fn admit(
    admission: &Arc<Mutex<Admission>>,
    source: IpAddr,
    limits: &ConnectionLimits,
) -> Option<Connection> {
    let mut a = admission.lock().unwrap_or_else(|p| p.into_inner());
    let trusted = a.is_trusted(source);
    let open = a.open.get(&source).copied().unwrap_or_default();
    let allowed = if trusted {
        open < limits.per_source
    } else {
        open < limits.per_untrusted_source && a.untrusted < limits.untrusted
    };
    if !allowed {
        return None;
    }
    *a.open.entry(source).or_default() += 1;
    if !trusted {
        a.untrusted += 1;
    }
    Some(Connection {
        admission: admission.clone(),
        source,
        untrusted: std::sync::atomic::AtomicBool::new(!trusted),
        authenticated: Default::default(),
        active: Default::default(),
        last_active: Mutex::new(Instant::now()),
    })
}
/// One admitted connection and what it has done, for closing ones that never
/// authenticate. Gives its place back when dropped.
struct Connection {
    admission: Arc<Mutex<Admission>>,
    source: IpAddr,
    /// Counted in the untrusted pool.
    untrusted: std::sync::atomic::AtomicBool,
    authenticated: std::sync::atomic::AtomicBool,
    active: std::sync::atomic::AtomicUsize,
    last_active: Mutex<Instant>,
}
impl Drop for Connection {
    fn drop(&mut self) {
        let mut a = self.admission.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(n) = a.open.get_mut(&self.source) {
            *n -= 1;
            if *n == 0 {
                a.open.remove(&self.source);
            }
        }
        if *self.untrusted.get_mut() {
            a.untrusted -= 1;
        }
    }
}
impl Connection {
    /// A request on this connection authenticated: its source is trusted from
    /// now on, and the connection leaves the untrusted pool.
    fn authenticated(&self, source: IpAddr) {
        use std::sync::atomic::Ordering;
        if !self.authenticated.swap(true, Ordering::Relaxed) {
            let mut a = self.admission.lock().unwrap_or_else(|p| p.into_inner());
            a.trust(source);
            if self.untrusted.swap(false, Ordering::Relaxed) {
                a.untrusted -= 1;
            }
        }
    }
    fn touch(&self, delta: isize) {
        use std::sync::atomic::Ordering;
        if delta > 0 {
            self.active.fetch_add(1, Ordering::Relaxed);
        } else {
            self.active.fetch_sub(1, Ordering::Relaxed);
        }
        *self.last_active.lock().unwrap_or_else(|p| p.into_inner()) = Instant::now();
    }
    /// Resolves once this connection should be closed for never authenticating.
    async fn expired(&self, opened: Instant, limits: ConnectionLimits) {
        use std::sync::atomic::Ordering;
        let tick = limits.idle.min(Duration::from_secs(1)) / 2;
        loop {
            tokio::time::sleep(tick).await;
            if self.authenticated.load(Ordering::Relaxed) {
                return std::future::pending().await;
            }
            let idle = self.active.load(Ordering::Relaxed) == 0
                && self
                    .last_active
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .elapsed()
                    >= limits.idle;
            if idle || opened.elapsed() >= limits.lifetime {
                return;
            }
        }
    }
}
/// Ends a request's activity on the connection however the request ends.
struct Active<'a>(&'a Connection);
impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.0.touch(-1);
    }
}
async fn connections(
    listener: tokio::net::TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
    context: Arc<ContextState>,
    slots: Arc<tokio::sync::Semaphore>,
) -> Result<()> {
    connections_limited(
        listener,
        acceptor,
        context,
        slots,
        ConnectionLimits::default(),
    )
    .await
}
async fn connections_limited(
    listener: tokio::net::TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
    context: Arc<ContextState>,
    slots: Arc<tokio::sync::Semaphore>,
    limits: ConnectionLimits,
) -> Result<()> {
    let admission = Arc::new(Mutex::new(Admission::default()));
    // Paired machines' addresses are trusted from the start, so they keep
    // their headroom across agent restarts.
    if let Ok(peers) = super::client::peers_in(&context.dir) {
        let mut a = admission.lock().unwrap_or_else(|p| p.into_inner());
        for ip in peers.iter().filter_map(|p| endpoint_ip(&p.endpoint)) {
            a.trust(ip);
        }
    }
    loop {
        // A failed accept (out of file descriptors, a connection reset before
        // it was accepted) must not stop the agent.
        let (stream, remote) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("Porch discovery: accepting a connection failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let _ = stream.set_nodelay(true);
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            drop(stream);
            continue;
        };
        let Some(connection) = admit(&admission, remote.ip(), &limits) else {
            drop(stream);
            continue;
        };
        let connection = Arc::new(connection);
        let acceptor = acceptor.clone();
        let context = context.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let opened = Instant::now();
            let Ok(Ok(stream)) =
                tokio::time::timeout(Duration::from_secs(5), acceptor.accept(stream)).await
            else {
                return;
            };
            let service = {
                let connection = connection.clone();
                hyper::service::service_fn(move |req| {
                    let context = context.clone();
                    let connection = connection.clone();
                    async move {
                        connection.touch(1);
                        let _active = Active(&connection);
                        Ok::<_, Infallible>(
                            match request(req, context, remote.ip(), &connection).await {
                                Ok(v) => v,
                                Err(_) => response(
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    "agent request failed",
                                ),
                            },
                        )
                    }
                })
            };
            let mut builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
            builder
                .http1()
                .timer(TokioTimer::new())
                .header_read_timeout(Duration::from_secs(10))
                .max_headers(64)
                .max_buf_size(32 * 1024);
            http2_settings(builder.http2());
            let conn = builder.serve_connection_with_upgrades(TokioIo::new(stream), service);
            tokio::pin!(conn);
            tokio::select! {
                _ = conn.as_mut() => {}
                _ = connection.expired(opened, limits) => {
                    // Never authenticated: let a request in progress finish, then close.
                    conn.as_mut().graceful_shutdown();
                    let _ = tokio::time::timeout(Duration::from_secs(5), conn).await;
                }
            }
        });
    }
}
fn endpoint_ip(endpoint: &str) -> Option<IpAddr> {
    match url::Url::parse(endpoint).ok()?.host()? {
        url::Host::Ipv4(ip) => Some(ip.into()),
        url::Host::Ipv6(ip) => Some(ip.into()),
        url::Host::Domain(_) => None,
    }
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
