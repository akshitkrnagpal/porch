//! Proxy port defaults, conflict ownership and TCP listener probes.
//!
//! Ownership lookups are best effort: they use `lsof` on macOS, `ss` (then
//! `lsof`) on Linux and the TCP table on Windows. Listeners owned by other
//! users may be invisible without administrator rights, so callers must cope
//! with an unknown owner.
#[cfg(unix)]
use std::process::Command;
use std::{
    fmt,
    net::{SocketAddr, TcpStream},
    time::Duration,
};

/// Plain-HTTP, unprivileged default proxy port.
pub const DEFAULT_PROXY_PORT: u16 = 7462;
/// Reserved for Porch discovery; never chosen as a proxy port.
pub const DISCOVERY_PORT: u16 = 7463;
/// Ports below this need elevation on most systems.
pub const FIRST_UNPRIVILEGED_PORT: u16 = 1024;

/// A process listening on a TCP port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listener {
    pub pid: u32,
    pub name: String,
    pub port: u16,
}
impl fmt::Display for Listener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (pid {})", self.name, self.pid)
    }
}

/// Human description of whoever holds `port`, for error messages.
pub fn describe_owner(port: u16) -> String {
    match owner(port) {
        Some(l) => l.to_string(),
        None if cfg!(windows) => {
            format!("another process (identify it with `netstat -ano -p tcp | findstr :{port}` in an administrator terminal)")
        }
        None => {
            format!("another process (identify it with `sudo lsof -nP -iTCP:{port} -sTCP:LISTEN`)")
        }
    }
}

/// Is this listener a Porch process (any state directory)?
pub fn is_porch(l: &Listener) -> bool {
    let name = l.name.rsplit(['/', '\\']).next().unwrap_or(&l.name);
    let name = name.strip_suffix(".exe").unwrap_or(name);
    name == "porch" || name.starts_with("porch-")
}

/// The first visible process listening on `port`.
pub fn owner(port: u16) -> Option<Listener> {
    all_listeners(Some(port), None).into_iter().next()
}

/// Every visible TCP listener owned by one of `pids`.
pub fn listeners_of(pids: &[u32]) -> Vec<Listener> {
    if pids.is_empty() {
        return Vec::new();
    }
    let mut out = all_listeners(None, Some(pids));
    out.retain(|l| pids.contains(&l.pid));
    out.sort_by_key(|l| (l.port, l.pid));
    out.dedup_by_key(|l| (l.port, l.pid));
    out
}

fn all_listeners(port: Option<u16>, pids: Option<&[u32]>) -> Vec<Listener> {
    #[cfg(target_os = "linux")]
    {
        let found = ss_listeners(port);
        if !found.is_empty() {
            return found;
        }
    }
    #[cfg(unix)]
    {
        lsof_listeners(port, pids)
    }
    #[cfg(windows)]
    {
        crate::win_net::tcp_listeners()
            .into_iter()
            .map(|(_, port, pid)| (port, pid))
            .filter(|(p, pid)| {
                *pid != 0
                    && port.is_none_or(|want| want == *p)
                    && pids.is_none_or(|ps| ps.contains(pid))
            })
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .map(|(port, pid)| Listener {
                pid,
                // Unnamed when the process belongs to another user or a service.
                name: crate::win_net::process_name(pid).unwrap_or_else(|| "unknown".into()),
                port,
            })
            .collect()
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (port, pids);
        Vec::new()
    }
}

#[cfg(unix)]
fn lsof_listeners(port: Option<u16>, pids: Option<&[u32]>) -> Vec<Listener> {
    let mut args = vec!["-nP".to_owned()];
    args.push(match port {
        Some(p) => format!("-iTCP:{p}"),
        None => "-iTCP".into(),
    });
    args.push("-sTCP:LISTEN".into());
    if let Some(pids) = pids {
        args.push("-a".into());
        args.push(format!(
            "-p{}",
            pids.iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    args.push("-Fpcn".into());
    let Ok(out) = Command::new("lsof").args(&args).output() else {
        return Vec::new();
    };
    let mut found = parse_lsof(&String::from_utf8_lossy(&out.stdout));
    if let Some(p) = port {
        found.retain(|l| l.port == p);
    }
    found
}

/// Parse `lsof -Fpcn` field output.
pub fn parse_lsof(output: &str) -> Vec<Listener> {
    let mut out = Vec::new();
    let (mut pid, mut name) = (None, String::new());
    for line in output.lines() {
        if let Some(v) = line.strip_prefix('p') {
            pid = v.parse::<u32>().ok();
            name.clear();
        } else if let Some(v) = line.strip_prefix('c') {
            name = v.to_owned();
        } else if let Some(v) = line.strip_prefix('n') {
            let port = v.rsplit_once(':').and_then(|(_, p)| p.parse::<u16>().ok());
            if let (Some(pid), Some(port)) = (pid, port) {
                let l = Listener {
                    pid,
                    name: name.clone(),
                    port,
                };
                if !out.contains(&l) {
                    out.push(l);
                }
            }
        }
    }
    out
}

#[cfg(target_os = "linux")]
fn ss_listeners(port: Option<u16>) -> Vec<Listener> {
    let Ok(out) = Command::new("ss").args(["-ltnpH"]).output() else {
        return Vec::new();
    };
    let mut found = parse_ss(&String::from_utf8_lossy(&out.stdout));
    if let Some(p) = port {
        found.retain(|l| l.port == p);
    }
    found
}

/// Parse `ss -ltnpH` output. Rows without visible process data are skipped.
pub fn parse_ss(output: &str) -> Vec<Listener> {
    let mut out = Vec::new();
    for line in output.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        let Some(port) = fields
            .get(3)
            .and_then(|a| a.rsplit_once(':'))
            .and_then(|(_, p)| p.parse::<u16>().ok())
        else {
            continue;
        };
        let Some(users) = line.find("users:((").map(|i| &line[i + 8..]) else {
            continue;
        };
        for entry in users.split("),(") {
            let name = entry.split('"').nth(1).unwrap_or("").to_owned();
            let pid = entry
                .split(',')
                .find_map(|f| f.trim().strip_prefix("pid="))
                .and_then(|p| p.trim_end_matches(')').parse::<u32>().ok());
            if let Some(pid) = pid {
                let l = Listener { pid, name, port };
                if !out.contains(&l) {
                    out.push(l);
                }
            }
        }
    }
    out
}

/// Whether a proxy could bind `port` right now on every interface.
pub fn available(port: u16) -> bool {
    crate::sockets::probe(port).is_ok()
}

/// Is a live listener (not merely lingering TIME_WAIT connections) holding `port`?
/// Recently closed proxy connections can keep a port unbindable for ~30s.
pub fn has_listener(port: u16) -> bool {
    !available(port)
        && (owner(port).is_some()
            || accepts(port)
            || accepts_on(SocketAddr::from(([0u16, 0, 0, 0, 0, 0, 0, 1], port))))
}

/// Wait until `port` can be bound, for ports held only by closing connections.
pub fn wait_released(port: u16, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    let mut announced = false;
    loop {
        if available(port) {
            return true;
        }
        if has_listener(port) || std::time::Instant::now() >= deadline {
            return false;
        }
        if !announced {
            eprintln!("Waiting for port {port} to be released by recently closed connections...");
            announced = true;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// The next bindable port after `after`, skipping Porch's reserved discovery port.
pub fn next_free(after: u16) -> Option<u16> {
    (after.checked_add(1)?..=after.saturating_add(200))
        .filter(|p| *p != DISCOVERY_PORT && *p >= FIRST_UNPRIVILEGED_PORT)
        .find(|p| available(*p))
}

/// Does something accept TCP connections on loopback `port`?
pub fn accepts(port: u16) -> bool {
    accepts_on(SocketAddr::from(([127, 0, 0, 1], port)))
}
/// Does something accept on IPv6 loopback only (the proxy targets 127.0.0.1)?
pub fn accepts_ipv6_only(port: u16) -> bool {
    !accepts(port) && accepts_on(SocketAddr::from(([0u16, 0, 0, 0, 0, 0, 0, 1], port)))
}
pub fn accepts_on(addr: SocketAddr) -> bool {
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_lsof_fields() {
        let out = "p123\nctailscaled\nn*:443\nn[::1]:443\np77\ncnode\nn127.0.0.1:4321\n";
        assert_eq!(
            parse_lsof(out),
            vec![
                Listener {
                    pid: 123,
                    name: "tailscaled".into(),
                    port: 443
                },
                Listener {
                    pid: 77,
                    name: "node".into(),
                    port: 4321
                }
            ]
        );
        assert_eq!(parse_lsof(out)[0].to_string(), "tailscaled (pid 123)");
    }
    #[test]
    fn parses_ss_rows() {
        let out = "LISTEN 0 511 127.0.0.1:4321 0.0.0.0:* users:((\"node\",pid=42,fd=20))\n\
                   LISTEN 0 4096 *:7462 *:* users:((\"porch\",pid=9,fd=7),(\"porch\",pid=10,fd=7))\n\
                   LISTEN 0 128 0.0.0.0:22 0.0.0.0:*\n";
        let found = parse_ss(out);
        assert_eq!(found.len(), 3);
        assert_eq!(found[0].port, 4321);
        assert_eq!(found[0].name, "node");
        assert_eq!(found[2].pid, 10);
        assert!(is_porch(&found[1]));
    }
    #[test]
    fn live_listener_is_distinguished_from_a_free_port() {
        // A private slot port: an ephemeral one, once released, can be taken by
        // a parallel test before the free-port assertions run.
        let port = crate::test_support::free_port();
        // Held by a child process: a listener in this process could outlive
        // its drop inside a child that another test forks at that moment.
        #[cfg(unix)]
        let (held, _) = crate::test_support::ChildListener::bind("127.0.0.1", port, false).unwrap();
        #[cfg(not(unix))]
        let held = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
        assert!(has_listener(port));
        assert!(!wait_released(port, Duration::from_millis(100)));
        drop(held);
        assert!(!has_listener(port));
        assert!(wait_released(port, Duration::from_secs(1)));
        // Bound without listening, like a port held by closing connections.
        // (Windows lets an exclusive wildcard bind share a port with a socket
        // bound to one address, so the port would count as available there.)
        #[cfg(unix)]
        {
            let (_bound, refused) = crate::test_support::refused_port();
            assert!(!available(refused));
            assert!(!has_listener(refused));
            assert!(!wait_released(refused, Duration::from_millis(100)));
        }
    }
    #[test]
    fn next_free_skips_discovery_and_busy_ports() {
        // The probes stay inside this test's private slot.
        let busy = crate::test_support::free_port();
        let _held = crate::sockets::reserve(busy).unwrap();
        let next = next_free(busy - 1);
        assert!(next.is_some_and(|p| p > busy && p < busy + 200), "{next:?}");
        assert_ne!(next_free(DEFAULT_PROXY_PORT), Some(DISCOVERY_PORT));
    }
}
