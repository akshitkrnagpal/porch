//! Route health and automatic stale-route reconciliation.
//!
//! A route is `dead` when its launcher identity no longer matches. Dead routes
//! whose app processes are also gone are removed automatically. Dead routes
//! with surviving (orphaned) processes are reported, because removing them
//! safely means terminating those processes (`porch prune --force`).
use crate::state::{self, Route, State};
use anyhow::Result;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Ok,
    NotListening,
    /// Some owned process is job-control stopped (state T).
    Stopped,
    /// The launcher is gone; `orphaned` when app processes survive it.
    Dead {
        orphaned: bool,
    },
}
impl Health {
    pub fn label(self) -> &'static str {
        match self {
            Health::Ok => "ok",
            Health::NotListening => "not listening",
            Health::Stopped => "stopped",
            Health::Dead { .. } => "dead",
        }
    }
}

/// One row of the process table.
#[derive(Debug, Clone)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    pub pgid: u32,
    pub stat: String,
}

/// Snapshot of the process table (empty when `ps` is unavailable).
pub fn processes() -> Vec<Proc> {
    #[cfg(unix)]
    {
        let Ok(out) = std::process::Command::new("ps")
            .args(["-axo", "pid=,ppid=,pgid=,stat="])
            .output()
        else {
            return Vec::new();
        };
        parse_ps(&String::from_utf8_lossy(&out.stdout))
    }
    #[cfg(not(unix))]
    {
        Vec::new()
    }
}
pub fn parse_ps(output: &str) -> Vec<Proc> {
    output
        .lines()
        .filter_map(|line| {
            let mut f = line.split_whitespace();
            Some(Proc {
                pid: f.next()?.parse().ok()?,
                ppid: f.next()?.parse().ok()?,
                pgid: f.next()?.parse().ok()?,
                stat: f.next()?.to_owned(),
            })
        })
        .collect()
}

/// Every live process in `group` plus their descendants.
pub fn tree(procs: &[Proc], group: u32) -> Vec<u32> {
    let mut pids: Vec<u32> = procs
        .iter()
        .filter(|p| (p.pgid == group || p.pid == group) && !p.stat.starts_with('Z'))
        .map(|p| p.pid)
        .collect();
    let mut i = 0;
    while i < pids.len() {
        let parent = pids[i];
        for p in procs {
            if p.ppid == parent && !pids.contains(&p.pid) && !p.stat.starts_with('Z') {
                pids.push(p.pid);
            }
        }
        i += 1;
    }
    pids
}

fn stopped(procs: &[Proc], pids: &[u32]) -> bool {
    procs
        .iter()
        .any(|p| pids.contains(&p.pid) && p.stat.starts_with('T'))
}

/// Owned pids (launcher and app tree) of a route, for stop detection.
fn owned_pids(s: &State, r: &Route, procs: &[Proc]) -> Vec<u32> {
    let mut pids = Vec::new();
    if let Some(owner) = s.owners.get(&r.name) {
        pids.push(owner.pid);
        if let Some(child) = owner.child {
            pids.extend(tree(procs, child));
        }
    } else if let Some(pid) = r.pid {
        pids.push(pid);
    }
    pids
}

fn orphaned(s: &State, r: &Route) -> bool {
    s.owners.get(&r.name).is_some_and(|o| {
        o.child
            .is_some_and(|c| state::group_matches(c, o.child_started.as_deref()))
    })
}

fn listening(r: &Route) -> bool {
    target_listening(&r.target)
}

/// [`target_listening`], remembered for two seconds per target, for pages
/// that many tabs refresh (the dashboard) so views don't probe every app.
pub fn target_listening_cached(target: &str) -> bool {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    static CACHE: Mutex<Option<HashMap<String, (bool, Instant)>>> = Mutex::new(None);
    if let Ok(mut cache) = CACHE.lock() {
        let map = cache.get_or_insert_with(HashMap::new);
        if let Some((up, at)) = map.get(target) {
            if at.elapsed() < Duration::from_secs(2) {
                return *up;
            }
        }
        if map.len() > 1024 {
            map.clear();
        }
    }
    let up = target_listening(target);
    if let Ok(mut cache) = CACHE.lock() {
        cache
            .get_or_insert_with(HashMap::new)
            .insert(target.to_owned(), (up, Instant::now()));
    }
    up
}

/// Does the route's target accept connections the way the proxy dials it?
///
/// The proxy dials plain-HTTP loopback targets as `localhost`, which reaches an
/// app on either 127.0.0.1 or ::1, so both are probed. Remote alias targets
/// are not probed.
pub fn target_listening(target: &str) -> bool {
    let target = if target.contains("://") {
        target.to_owned()
    } else {
        format!("http://{target}")
    };
    let Ok(u) = url::Url::parse(&target) else {
        return true;
    };
    let Some(port) = u.port_or_known_default() else {
        return true;
    };
    let loopback = match u.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        None => return true,
    };
    if loopback {
        let v4 = std::net::IpAddr::from(std::net::Ipv4Addr::LOCALHOST);
        let v6 = std::net::IpAddr::from(std::net::Ipv6Addr::LOCALHOST);
        return [v4, v6]
            .into_iter()
            .any(|ip| crate::ports::accepts_on((ip, port).into()));
    }
    match u.host() {
        Some(url::Host::Ipv4(ip)) => {
            crate::ports::accepts_on((std::net::IpAddr::from(ip), port).into())
        }
        Some(url::Host::Ipv6(ip)) => {
            crate::ports::accepts_on((std::net::IpAddr::from(ip), port).into())
        }
        _ => true,
    }
}

pub fn evaluate(s: &State, r: &Route, procs: &[Proc]) -> Health {
    if !state::route_alive(s, r) {
        return Health::Dead {
            orphaned: orphaned(s, r),
        };
    }
    if r.pid.is_some() && stopped(procs, &owned_pids(s, r, procs)) {
        return Health::Stopped;
    }
    if listening(r) {
        Health::Ok
    } else {
        Health::NotListening
    }
}

/// Route health plus how long it has been stopped, for listings.
#[derive(Debug, Clone)]
pub struct RouteHealth {
    pub route: Route,
    pub health: Health,
    pub stopped_for: Option<Duration>,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Remove dead routes whose processes are gone, remember when routes were first
/// seen stopped, and return the health of every remaining route.
pub fn reconcile() -> Result<(Vec<String>, Vec<RouteHealth>)> {
    reconcile_with(true)
}

/// Like [`reconcile`], but with `apply: false` nothing is written: the dead
/// routes that would be removed are returned instead (for `porch doctor`).
pub fn reconcile_with(apply: bool) -> Result<(Vec<String>, Vec<RouteHealth>)> {
    let procs = processes();
    let snapshot = state::load()?;
    let mut removable = Vec::new();
    let mut stopped_now = Vec::new();
    let mut rows = Vec::new();
    for r in &snapshot.routes {
        let health = evaluate(&snapshot, r, &procs);
        match health {
            Health::Dead { orphaned: false } if r.pid.is_some() => {
                removable.push((r.name.clone(), r.pid));
                continue;
            }
            Health::Stopped => stopped_now.push(r.name.clone()),
            _ => {}
        }
        rows.push((r.clone(), health));
    }
    let mut since = snapshot.stopped_since.clone();
    since.retain(|n, _| stopped_now.contains(n));
    for n in &stopped_now {
        since.entry(n.clone()).or_insert_with(now);
    }
    let mut removed = Vec::new();
    if !apply {
        removed = removable.into_iter().map(|(n, _)| n).collect();
    } else if !removable.is_empty() || since != snapshot.stopped_since {
        state::transaction(|s| {
            for (name, pid) in &removable {
                // Only remove the exact registration we judged dead.
                if s.routes.iter().any(|r| &r.name == name && r.pid == *pid)
                    && s.routes
                        .iter()
                        .find(|r| &r.name == name)
                        .is_some_and(|r| !state::route_alive(s, r))
                {
                    state::remove_route(s, name);
                    removed.push(name.clone());
                }
            }
            s.stopped_since = since.clone();
            Ok(())
        })?;
    }
    let t = now();
    let rows = rows
        .into_iter()
        .map(|(route, health)| RouteHealth {
            stopped_for: (health == Health::Stopped)
                .then(|| since.get(&route.name))
                .flatten()
                .map(|s| Duration::from_secs(t.saturating_sub(*s))),
            route,
            health,
        })
        .collect();
    Ok((removed, rows))
}

/// Reconcile, reporting but never failing the caller.
pub fn reconcile_quietly() {
    if let Err(e) = reconcile() {
        eprintln!("porch: stale route cleanup skipped: {e:#}");
    }
}

/// Periodic cleanup for the long-running proxy.
pub async fn reconcile_forever(every: Duration) {
    loop {
        tokio::time::sleep(every).await;
        let _ = tokio::task::spawn_blocking(reconcile).await;
    }
}

/// Seconds to wait for a launched app to accept connections (0 disables).
pub fn listen_timeout() -> Duration {
    let secs = state::env("LISTEN_TIMEOUT")
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
        .unwrap_or(15.0);
    Duration::from_secs_f64(secs)
}

/// After launch, warn (once) when nothing accepts on the assigned port.
pub async fn listen_check(port: u16, group: u32, command: String) {
    let timeout = listen_timeout();
    if timeout.is_zero() {
        return;
    }
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let ok = tokio::task::spawn_blocking(move || crate::ports::accepts(port))
            .await
            .unwrap_or(false);
        if ok {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let message = tokio::task::spawn_blocking(move || not_listening_message(port, group, &command))
        .await
        .unwrap_or_default();
    if !message.is_empty() {
        eprintln!("{message}");
    }
}

fn not_listening_message(port: u16, group: u32, command: &str) -> String {
    let secs = listen_timeout().as_secs_f64();
    let mut lines = vec![format!(
        "porch: warning: nothing is listening on port {port} after {secs:.0}s, so the proxy cannot reach this app."
    )];
    if crate::ports::accepts_ipv6_only(port) {
        lines.push(format!(
            "  The app listens on [::1]:{port} only; bind it to 127.0.0.1 (Porch sets HOST=127.0.0.1)."
        ));
    }
    let procs = processes();
    let elsewhere: Vec<_> = crate::ports::listeners_of(&tree(&procs, group))
        .into_iter()
        .filter(|l| l.port != port)
        .collect();
    if let Some(l) = elsewhere.first() {
        lines.push(format!(
            "  The app is listening on {} instead ({}). Make it use the PORT env var (Porch set PORT={port}),",
            l.port, l
        ));
        lines.push(format!(
            "  or pin Porch to that port: `porch run --app-port {} {command}`",
            l.port
        ));
    } else {
        lines.push(format!(
            "  Make sure the app uses the PORT env var (Porch set PORT={port}) and check its logs above; then run `porch list`"
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    #[test]
    fn ipv6_only_app_counts_as_listening_like_the_proxy_sees_it() {
        let Ok(v6) = std::net::TcpListener::bind("[::1]:0") else {
            return; // no IPv6 loopback on this machine
        };
        let port = v6.local_addr().unwrap().port();
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_err() {
            return; // the IPv4 side is taken by something else
        }
        assert!(super::target_listening(&format!("http://127.0.0.1:{port}")));
        drop(v6);
    }

    use super::*;
    #[test]
    fn process_tree_follows_group_and_descendants() {
        let procs = parse_ps("10 1 10 Ss\n11 10 10 S+\n12 11 12 T\n13 1 13 S\n14 12 12 Z\n");
        let mut t = tree(&procs, 10);
        t.sort();
        assert_eq!(t, vec![10, 11, 12]);
        assert!(stopped(&procs, &t));
        assert!(!stopped(&procs, &[10, 11]));
    }
}
