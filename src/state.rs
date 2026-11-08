use anyhow::{bail, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    pub routes: Vec<Route>,
    #[serde(default)]
    pub owners: BTreeMap<String, ProcessIdentity>,
    #[serde(default)]
    pub shares: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(default)]
    pub compose_projects: BTreeMap<String, Vec<String>>,
    /// Aliases that dial a loopback port with their own scheme, Host and TLS
    /// name (`porch alias NAME PORT --upstream ...`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub upstreams: BTreeMap<String, Upstream>,
    /// Routes never offered to paired machines (`--private`).
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub private: std::collections::BTreeSet<String>,
    /// Unix seconds when a route's processes were first seen job-control stopped.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub stopped_since: BTreeMap<String, u64>,
}
/// How the proxy talks to an alias's loopback port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Upstream {
    /// Speak TLS to the backend.
    pub tls: bool,
    /// `host[:port]` sent as Host (and, for TLS, the server name checked);
    /// `None` keeps the browser's Host.
    pub authority: Option<String>,
    /// Skip backend certificate verification (explicit `--insecure` only).
    #[serde(default)]
    pub insecure: bool,
}
impl Upstream {
    /// Parse `https`, `https://host[:port]` or `http://host[:port]`.
    pub fn parse(spec: &str, insecure: bool) -> Result<Upstream> {
        if spec == "https" {
            return Ok(Upstream {
                tls: true,
                authority: None,
                insecure,
            });
        }
        let u = url::Url::parse(spec).with_context(|| {
            format!(
                "--upstream must be https, https://HOST[:PORT] or http://HOST[:PORT], not {spec:?}"
            )
        })?;
        let tls = match u.scheme() {
            "https" => true,
            "http" => false,
            _ => bail!("--upstream must use http or https"),
        };
        if u.host_str().is_none()
            || !u.username().is_empty()
            || u.password().is_some()
            || !matches!(u.path(), "" | "/")
            || u.query().is_some()
            || u.fragment().is_some()
            || spec.chars().any(char::is_whitespace)
        {
            bail!("--upstream takes only a scheme, host and optional port, like https://api.internal:8443");
        }
        if insecure && !tls {
            bail!("--insecure only applies to an https upstream");
        }
        Ok(Upstream {
            tls,
            authority: Some(u[url::Position::BeforeHost..url::Position::AfterPort].to_owned()),
            insecure,
        })
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    pub name: String,
    pub target: String,
    pub pid: Option<u32>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub started: String,
    pub child: Option<u32>,
    #[serde(default)]
    pub child_started: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProxySettings {
    pub port: u16,
    pub tls: bool,
    pub tlds: Vec<String>,
    pub wildcard: bool,
    pub lan: bool,
    pub lan_ip: Option<String>,
    pub unprivileged: bool,
    pub bind: Option<String>,
    pub cert: Option<PathBuf>,
    pub key: Option<PathBuf>,
    pub redirect_port: Option<u16>,
    /// The no-sudo standard port (80/443) to return to when it frees up,
    /// after a busy moment made this run fall back to a high port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_port: Option<u16>,
}
impl Default for ProxySettings {
    fn default() -> Self {
        Self {
            port: crate::ports::DEFAULT_PROXY_PORT,
            tls: false,
            tlds: vec!["localhost".into()],
            wildcard: false,
            lan: false,
            lan_ip: None,
            unprivileged: true,
            bind: None,
            cert: None,
            key: None,
            redirect_port: None,
            preferred_port: None,
        }
    }
}
impl ProxySettings {
    pub fn hostname(&self, name: &str) -> String {
        let n = name.trim_end_matches('.');
        if self.lan {
            if n.ends_with(".local") {
                n.into()
            } else {
                format!("{n}.local")
            }
        } else if self.tlds.iter().any(|t| n.ends_with(&format!(".{t}"))) {
            n.into()
        } else {
            format!(
                "{n}.{}",
                self.tlds.first().map(String::as_str).unwrap_or("localhost")
            )
        }
    }
    pub fn url(&self, name: &str) -> String {
        let suffix = if self.port == if self.tls { 443 } else { 80 } {
            String::new()
        } else {
            format!(":{}", self.port)
        };
        format!(
            "{}://{}{suffix}",
            if self.tls { "https" } else { "http" },
            self.hostname(name)
        )
    }
}
pub fn env(key: &str) -> Option<String> {
    std::env::var(format!("PORCH_{key}"))
        .ok()
        .or_else(|| std::env::var(format!("PORTLESS_{key}")).ok())
}
pub fn dir() -> PathBuf {
    env("STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| invoking_home().join(".porch"))
}
fn prepare() -> Result<()> {
    let d = dir();
    if fs::symlink_metadata(&d).is_ok_and(|m| m.file_type().is_symlink()) {
        bail!("state directory cannot be a symlink")
    };
    let existed = d.exists();
    fs::create_dir_all(&d)?;
    #[cfg(unix)]
    if !existed && unsafe { libc::geteuid() } == 0 {
        if let (Ok(uid), Ok(gid)) = (std::env::var("SUDO_UID"), std::env::var("SUDO_GID")) {
            if let (Ok(uid), Ok(gid)) = (uid.parse::<u32>(), gid.parse::<u32>()) {
                chown(&d, uid, gid)?;
            }
        }
    }
    #[cfg(not(unix))]
    let _ = existed;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(d, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
fn options() -> OpenOptions {
    let mut o = OpenOptions::new();
    o.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    o
}
pub fn atomic_write(name: &str, bytes: &[u8]) -> Result<()> {
    prepare()?;
    let p = dir().join(format!("{name}.{}.tmp", std::process::id()));
    let mut o = options();
    o.create(true).truncate(true);
    let mut f = o.open(&p)?;
    inherit_owner(&p)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    fs::rename(p, dir().join(name))?;
    #[cfg(unix)]
    fs::File::open(dir())?.sync_all()?;
    Ok(())
}
pub fn load() -> Result<State> {
    prepare()?;
    match fs::read(dir().join("state.json")) {
        Ok(b) => Ok(serde_json::from_slice(&b).context("invalid route state")?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(e) => Err(e.into()),
    }
}
pub fn transaction<T>(f: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
    prepare()?;
    let mut o = options();
    o.create(true);
    let lock = o.open(dir().join("state.lock"))?;
    inherit_owner(&dir().join("state.lock"))?;
    lock.lock_exclusive()?;
    let mut s = load()?;
    let result = f(&mut s)?;
    atomic_write("state.json", &serde_json::to_vec_pretty(&s)?)?;
    Ok(result)
}
pub fn mutate(f: impl FnOnce(&mut State)) -> Result<()> {
    transaction(|s| {
        f(s);
        Ok(())
    })
}
pub fn proxy_settings() -> Result<ProxySettings> {
    prepare()?;
    match fs::read(dir().join("proxy.json")) {
        Ok(b) => Ok(serde_json::from_slice(&b).context("invalid proxy settings")?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ProxySettings::default()),
        Err(e) => Err(e.into()),
    }
}
pub fn save_proxy_settings(s: &ProxySettings) -> Result<()> {
    atomic_write("proxy.json", &serde_json::to_vec_pretty(s)?)
}
pub fn identity(pid: u32) -> Option<ProcessIdentity> {
    if pid == 0 || pid > i32::MAX as u32 {
        return None;
    }
    #[cfg(target_os = "macos")]
    {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
        let count = unsafe {
            libc::proc_pidinfo(
                pid as i32,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if count != size {
            return None;
        }
        let info = unsafe { info.assume_init() };
        if info.pbi_status == libc::SZOMB {
            return None;
        }
        Some(ProcessIdentity {
            pid,
            started: format!("{}:{}", info.pbi_start_tvsec, info.pbi_start_tvusec),
            child: None,
            child_started: None,
        })
    }
    #[cfg(target_os = "linux")]
    {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let rest = stat.rsplit_once(") ")?.1;
        let fields: Vec<_> = rest.split_whitespace().collect();
        if fields.first() == Some(&"Z") {
            return None;
        }
        Some(ProcessIdentity {
            pid,
            started: fields.get(19)?.to_string(),
            child: None,
            child_started: None,
        })
    }
    #[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
    {
        let out = std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "lstart="])
            .output()
            .ok()?;
        let started = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if out.status.success() && !started.is_empty() {
            Some(ProcessIdentity {
                pid,
                started,
                child: None,
                child_started: None,
            })
        } else {
            None
        }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::{
            Foundation::{CloseHandle, FILETIME},
            System::Threading::{
                GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
            },
        };
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return None;
        }
        let mut created = FILETIME::default();
        let mut exited = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        let mut code = 0;
        let okay = unsafe {
            GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) != 0
                && GetExitCodeProcess(handle, &mut code) != 0
                && code == 259
        };
        unsafe { CloseHandle(handle) };
        okay.then(|| ProcessIdentity {
            pid,
            started: (((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64)
                .to_string(),
            child: None,
            child_started: None,
        })
    }
}
pub fn matches(i: &ProcessIdentity) -> bool {
    identity(i.pid).is_some_and(|x| x.started == i.started)
}
pub fn alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    #[cfg(unix)]
    {
        if unsafe { libc::kill(pid as i32, 0) } != 0 {
            return std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        }
        identity(pid).is_some()
    }
    #[cfg(windows)]
    {
        identity(pid).is_some()
    }
}
pub fn route_alive(s: &State, r: &Route) -> bool {
    r.pid.is_none()
        || s.owners
            .get(&r.name)
            .map_or_else(|| r.pid.is_some_and(alive), matches)
}
pub fn remove_route(s: &mut State, name: &str) {
    s.routes.retain(|r| r.name != name);
    s.owners.remove(name);
    s.shares.remove(name);
    s.stopped_since.remove(name);
    s.private.remove(name);
    s.upstreams.remove(name);
    for names in s.compose_projects.values_mut() {
        names.retain(|n| n != name);
    }
}

fn invoking_home() -> PathBuf {
    #[cfg(unix)]
    if let Ok(user) = std::env::var("SUDO_USER") {
        if let Ok(user) = std::ffi::CString::new(user) {
            unsafe {
                let info = libc::getpwnam(user.as_ptr());
                if !info.is_null() {
                    return PathBuf::from(
                        std::ffi::CStr::from_ptr((*info).pw_dir)
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
            }
        }
    }
    // Windows does not set HOME; without this the state directory would be a
    // relative `.porch` in whatever directory Porch was started from.
    #[cfg(windows)]
    if let Some(profile) = std::env::var_os("USERPROFILE").filter(|p| !p.is_empty()) {
        return PathBuf::from(profile);
    }
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}
#[cfg(unix)]
fn chown(path: &std::path::Path, uid: u32, gid: u32) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let p = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    if unsafe { libc::lchown(p.as_ptr(), uid, gid) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
pub fn inherit_owner(path: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    if unsafe { libc::geteuid() } == 0 {
        use std::os::unix::fs::MetadataExt;
        let m = fs::metadata(dir())?;
        chown(path, m.uid(), m.gid())?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Elevated cleanup may signal only app processes belonging to the state-directory owner.
pub fn process_owned(pid: u32) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if unsafe { libc::geteuid() } != 0 {
            return true;
        }
        let Ok(owner) = fs::metadata(dir()).map(|m| m.uid()) else {
            return false;
        };
        #[cfg(target_os = "macos")]
        {
            let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
            let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
            let n = unsafe {
                libc::proc_pidinfo(
                    pid as i32,
                    libc::PROC_PIDTBSDINFO,
                    0,
                    info.as_mut_ptr().cast(),
                    size,
                )
            };
            n == size && unsafe { info.assume_init() }.pbi_uid == owner
        }
        #[cfg(target_os = "linux")]
        {
            fs::metadata(format!("/proc/{pid}")).is_ok_and(|m| m.uid() == owner)
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = (pid, owner);
            false
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

/// Identify surviving members even when an orphaned group's original leader exited.
pub fn group_matches(group: u32, started: Option<&str>) -> bool {
    #[cfg(unix)]
    {
        let Ok(out) = std::process::Command::new("ps")
            .args(["-axo", "pid=,pgid=,stat="])
            .output()
        else {
            return false;
        };
        let members: Vec<u32> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|line| {
                let mut p = line.split_whitespace();
                let pid = p.next()?.parse().ok()?;
                let pgid: u32 = p.next()?.parse().ok()?;
                let status = p.next()?;
                (pgid == group && !status.starts_with('Z')).then_some(pid)
            })
            .collect();
        if members.is_empty() || members.iter().any(|pid| !process_owned(*pid)) {
            return false;
        }
        if members.contains(&group) {
            return identity(group).is_some_and(|i| Some(i.started.as_str()) == started);
        }
        true
    }
    #[cfg(windows)]
    {
        identity(group).is_some_and(|i| Some(i.started.as_str()) == started)
    }
}

/// Routes the proxy can serve, cached between requests.
///
/// The proxy used to re-read `state.json` and re-check every route's process on
/// each request. This keeps the parsed state until the file changes and keeps
/// the liveness answers for a short window, so a request costs one `stat`.
pub fn live_routes() -> Result<std::sync::Arc<State>> {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant, SystemTime};
    #[derive(PartialEq, Clone)]
    struct Stamp {
        path: PathBuf,
        modified: Option<SystemTime>,
        len: u64,
        #[cfg(unix)]
        ino: u64,
    }
    struct Cached {
        stamp: Option<Stamp>,
        at: Instant,
        state: Arc<State>,
    }
    static CACHE: Mutex<Option<Cached>> = Mutex::new(None);
    const LIVENESS_TTL: Duration = Duration::from_millis(750);
    let path = dir().join("state.json");
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let stamp = match fs::metadata(&path) {
        Ok(m) => Some(Stamp {
            path: path.clone(),
            modified: m.modified().ok(),
            len: m.len(),
            #[cfg(unix)]
            ino: std::os::unix::fs::MetadataExt::ino(&m),
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    if let Some(c) = cache.as_ref() {
        let same_file = match (&c.stamp, &stamp) {
            (Some(a), Some(b)) => a == b,
            (None, None) => c.state.routes.is_empty(),
            _ => false,
        };
        if same_file && c.at.elapsed() < LIVENESS_TTL {
            return Ok(c.state.clone());
        }
    }
    let mut state = load()?;
    let alive: Vec<Route> = state
        .routes
        .iter()
        .filter(|r| route_alive(&state, r))
        .cloned()
        .collect();
    state.routes = alive;
    let state = Arc::new(state);
    *cache = Some(Cached {
        stamp,
        at: Instant::now(),
        state: state.clone(),
    });
    Ok(state)
}

#[cfg(test)]
mod upstream_tests {
    use super::Upstream;
    #[test]
    fn upstream_specs() {
        assert_eq!(
            Upstream::parse("https", false).unwrap(),
            Upstream {
                tls: true,
                authority: None,
                insecure: false
            }
        );
        assert_eq!(
            Upstream::parse("https://api.internal:8443", true).unwrap(),
            Upstream {
                tls: true,
                authority: Some("api.internal:8443".into()),
                insecure: true
            }
        );
        assert_eq!(
            Upstream::parse("http://api.internal", false).unwrap(),
            Upstream {
                tls: false,
                authority: Some("api.internal".into()),
                insecure: false
            }
        );
        for bad in [
            "ftp://x",
            "https://u:p@x",
            "https://x/path",
            "https://x?q=1",
            "https://x#f",
            "tls",
        ] {
            assert!(Upstream::parse(bad, false).is_err(), "{bad}");
        }
        assert!(
            Upstream::parse("http://x", true).is_err(),
            "--insecure needs https"
        );
    }
}
