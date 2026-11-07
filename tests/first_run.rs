//! First-run behavior: defaults, port conflicts, listen checks, stale cleanup,
//! proxy upgrades, setup and doctor --fix. Every case uses isolated state and
//! never binds the real default port 7462.
#![cfg(unix)]
use std::{
    path::PathBuf,
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

struct Sandbox {
    root: PathBuf,
}
impl Sandbox {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "porch-first-run-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("state")).unwrap();
        Self { root }
    }
    /// A command without PORCH_PORT, so saved settings decide the port.
    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_porch"));
        c.args(args)
            .current_dir(&self.root)
            .env_remove("PORCH_PORT")
            .env_remove("PORTLESS_PORT")
            .env_remove("PORCH_HTTPS")
            .env("PORCH_STATE_DIR", self.root.join("state"))
            .env("PORCH_SYNC_HOSTS", "0")
            .env("PORCH_SKIP_TRUST", "1")
            .env("CI", "1")
            .stdin(Stdio::null());
        c
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }
    fn ok(&self, args: &[&str]) -> (String, String) {
        let o = self.run(args);
        let (out, err) = (
            String::from_utf8_lossy(&o.stdout).into_owned(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
        );
        assert!(o.status.success(), "{args:?}: {out}\n{err}");
        (out, err)
    }
    /// Save settings on a free high port so tests never touch 7462.
    fn save_port(&self, port: u16, extra: serde_json::Value) {
        let mut v = serde_json::json!({"port": port, "tls": false, "tlds": ["localhost"], "unprivileged": true});
        for (k, val) in extra.as_object().unwrap() {
            v[k] = val.clone();
        }
        std::fs::write(
            self.root.join("state/proxy.json"),
            serde_json::to_vec(&v).unwrap(),
        )
        .unwrap();
    }
    fn settings(&self) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(self.root.join("state/proxy.json")).unwrap()).unwrap()
    }
    fn state(&self) -> serde_json::Value {
        std::fs::read(self.root.join("state/state.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_else(|| serde_json::json!({"routes": []}))
    }
    fn file(&self, name: &str) -> String {
        std::fs::read_to_string(self.root.join("state").join(name))
            .unwrap_or_default()
            .trim()
            .to_owned()
    }
    fn wait_for(&self, what: &str, mut f: impl FnMut() -> bool) {
        let d = Instant::now() + Duration::from_secs(20);
        while !f() {
            assert!(Instant::now() < d, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = self.command(&["proxy", "stop"]).output();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
/// A port whose next few neighbors are free too, outside the ephemeral range
/// that concurrently running tests allocate from.
fn quiet_port() -> u16 {
    // macOS clocks have microsecond granularity; mix in a per-test counter so
    // parallel tests probe different blocks.
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_micros() as u32;
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let seed = micros
        .wrapping_mul(2_654_435_761)
        .wrapping_add(n.wrapping_mul(1_009))
        .wrapping_add(std::process::id());
    (0..2000u32)
        .map(|i| 21000 + (seed.wrapping_add(i * 7) % 8000) as u16 / 8 * 8)
        .find(|p| (*p..*p + 6).all(|q| q != 7462 && q != 7463 && porch::ports::available(q)))
        .unwrap()
}
fn try_hold(port: u16) -> std::io::Result<socket2::Socket> {
    let socket = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )?;
    socket.set_reuse_address(true)?;
    let addr: std::net::SocketAddr = ([0, 0, 0, 0], port).into();
    socket.bind(&addr.into())?;
    socket.listen(16)?;
    Ok(socket)
}
/// Occupy a specific port like a real server does, retrying briefly while
/// parallel tests probe nearby ports.
fn hold(port: u16) -> socket2::Socket {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match try_hold(port) {
            Ok(s) => return s,
            Err(e) if Instant::now() < deadline => {
                let _ = e;
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("cannot hold port {port}: {e}"),
        }
    }
}
/// A quiet port, already occupied by this test process.
fn held_port() -> (u16, socket2::Socket) {
    loop {
        let port = quiet_port();
        if let Ok(s) = try_hold(port) {
            return (port, s);
        }
    }
}

#[test]
fn default_is_plain_http_on_7462_without_privileges() {
    let s = Sandbox::new();
    let (url, _) = s.ok(&["get", "web", "--no-worktree"]);
    assert_eq!(url.trim(), "http://web.localhost:7462");
    let (out, _) = s.ok(&["list", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["proxy"]["port"], 7462);
    assert_eq!(v["proxy"]["tls"], false);
    assert_eq!(v["proxy"]["unprivileged"], true);
}

#[test]
fn high_port_implies_no_hosts_sync_even_for_legacy_settings() {
    let s = Sandbox::new();
    let port = quiet_port();
    // Legacy hand-edited setting: high port with hosts sync left on.
    s.save_port(port, serde_json::json!({"unprivileged": false}));
    s.ok(&["proxy", "start"]);
    assert_eq!(s.settings()["port"], port);
    assert_eq!(s.settings()["unprivileged"], true);
}

#[test]
fn busy_saved_port_moves_to_next_free_port_and_names_owner() {
    let s = Sandbox::new();
    let (port, _held) = held_port();
    s.save_port(port, serde_json::json!({}));
    let (_, err) = s.ok(&["proxy", "start"]);
    assert!(err.contains(&format!("Port {port} is used by")), "{err}");
    // The owner is this test process.
    let me = std::process::id().to_string();
    assert!(err.contains(&format!("pid {me}")), "{err}");
    let moved = s.settings()["port"].as_u64().unwrap() as u16;
    assert_ne!(moved, port);
    assert_ne!(moved, 7463);
    assert_eq!(s.ok(&["proxy", "status"]).0.trim(), "running");
}

#[test]
fn explicit_busy_port_fails_with_owner_and_command() {
    let s = Sandbox::new();
    let (port, _held) = held_port();
    let o = s.run(&["proxy", "start", "--port", &port.to_string()]);
    assert!(!o.status.success());
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("unavailable"), "{err}");
    assert!(
        err.contains(&format!("pid {}", std::process::id())),
        "{err}"
    );
    assert!(err.contains("`porch proxy start --port "), "{err}");
    assert!(!s.root.join("state/proxy.ready").exists());
}

#[test]
fn app_that_ignores_port_produces_listen_warning() {
    use std::io::BufRead;
    let s = Sandbox::new();
    s.save_port(quiet_port(), serde_json::json!({}));
    let other = quiet_port();
    // The app stays up until the warning is observed; its lifetime must not
    // race the diagnosis, which inspects the app's listeners after the timeout.
    std::fs::write(
        s.root.join("wrong.py"),
        format!("import socket,time\ns=socket.socket()\ns.bind(('127.0.0.1',{other}))\ns.listen()\ntime.sleep(60)\n"),
    )
    .unwrap();
    let mut porch = s
        .command(&["wrong", "python3", "wrong.py"])
        .env("PORCH_LISTEN_TIMEOUT", "1.5")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (lines, received) = std::sync::mpsc::channel();
    let stderr = porch.stderr.take().unwrap();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stderr)
            .lines()
            .map_while(Result::ok)
        {
            if lines.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut err = String::new();
    while !err.contains("`porch run --app-port") {
        let left = deadline.saturating_duration_since(Instant::now());
        match received.recv_timeout(left) {
            Ok(line) => {
                err.push_str(&line);
                err.push('\n');
            }
            Err(_) => break,
        }
    }
    unsafe {
        libc::kill(porch.id() as i32, libc::SIGTERM);
    }
    let _ = porch.wait();
    assert!(err.contains("nothing is listening on port"), "{err}");
    assert!(
        err.contains(&format!("listening on {other} instead")),
        "{err}"
    );
    assert!(err.contains("`porch run --app-port"), "{err}");
}

#[test]
fn listen_check_is_silent_for_correct_apps_and_can_be_disabled() {
    let s = Sandbox::new();
    s.save_port(quiet_port(), serde_json::json!({}));
    std::fs::write(
        s.root.join("good.py"),
        "import os,socket,time\ns=socket.socket()\ns.bind(('127.0.0.1',int(os.environ['PORT'])))\ns.listen()\ntime.sleep(2.5)\n",
    )
    .unwrap();
    let o = s
        .command(&["good", "python3", "good.py"])
        .env("PORCH_LISTEN_TIMEOUT", "2")
        .output()
        .unwrap();
    assert!(!String::from_utf8_lossy(&o.stderr).contains("nothing is listening"));
    let o = s
        .command(&["off", "python3", "-c", "import time;time.sleep(1.5)"])
        .env("PORCH_LISTEN_TIMEOUT", "0")
        .output()
        .unwrap();
    assert!(!String::from_utf8_lossy(&o.stderr).contains("nothing is listening"));
}

fn launch_server(s: &Sandbox, name: &str) -> (std::process::Child, u32) {
    std::fs::write(
        s.root.join("serve.py"),
        "import os,socket,time\ns=socket.socket()\ns.bind(('127.0.0.1',int(os.environ['PORT'])))\ns.listen()\nwhile True: time.sleep(1)\n",
    )
    .unwrap();
    let child = s
        .command(&[name, "python3", "serve.py"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut app = 0;
    s.wait_for("app child", || {
        app = s.state()["owners"][name]["child"].as_u64().unwrap_or(0) as u32;
        app != 0
    });
    (child, app)
}
fn health(s: &Sandbox, name: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(&s.ok(&["list", "--json"]).0).unwrap();
    v["routes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == name)
        .map(|r| r["health"].as_str().unwrap().to_owned())
}

#[test]
fn killing_the_app_removes_its_route_and_list_shows_health() {
    let s = Sandbox::new();
    s.save_port(quiet_port(), serde_json::json!({}));
    let (mut launcher, app) = launch_server(&s, "killed");
    s.wait_for("ok health", || {
        health(&s, "killed").as_deref() == Some("ok")
    });
    assert!(s.ok(&["list"]).0.contains("HEALTH"));
    unsafe { libc::kill(app as i32, libc::SIGKILL) };
    launcher.wait().unwrap();
    s.wait_for("route removal", || health(&s, "killed").is_none());
}

#[test]
fn dead_launcher_route_is_pruned_automatically() {
    let s = Sandbox::new();
    s.save_port(quiet_port(), serde_json::json!({}));
    std::fs::write(
        s.root.join("state/state.json"),
        serde_json::to_vec(&serde_json::json!({
            "routes": [
                {"name": "ghost", "target": "http://127.0.0.1:1", "pid": std::process::id()},
                {"name": "static", "target": "http://127.0.0.1:2", "pid": null}
            ],
            "owners": {"ghost": {"pid": std::process::id(), "started": "not this process", "child": null}}
        }))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(health(&s, "ghost"), None);
    assert_eq!(health(&s, "static").as_deref(), Some("not listening"));
    assert!(s.state()["routes"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["name"] != "ghost"));
}

#[test]
fn stopped_launch_is_reported_and_doctor_fix_terminates_it() {
    let s = Sandbox::new();
    s.save_port(quiet_port(), serde_json::json!({}));
    let (mut launcher, app) = launch_server(&s, "frozen");
    s.wait_for("ok health", || {
        health(&s, "frozen").as_deref() == Some("ok")
    });
    unsafe { libc::kill(-(app as i32), libc::SIGSTOP) };
    s.wait_for("stopped health", || {
        health(&s, "frozen").as_deref() == Some("stopped")
    });
    // Without consent nothing is terminated.
    let (out, _) = s.ok(&["doctor", "--fix"]);
    assert!(out.contains("frozen"), "{out}");
    assert!(out.contains("porch doctor --fix --yes"), "{out}");
    assert_eq!(health(&s, "frozen").as_deref(), Some("stopped"));
    let (out, _) = s.ok(&["doctor", "--fix", "--yes"]);
    assert!(out.contains("terminated the launch"), "{out}");
    launcher.wait().unwrap();
    s.wait_for("route removal", || health(&s, "frozen").is_none());
    s.wait_for("app exit", || !porch::state::alive(app));
}

#[test]
fn outdated_proxy_restarts_transparently_and_keeps_routes() {
    let s = Sandbox::new();
    s.save_port(quiet_port(), serde_json::json!({}));
    s.ok(&["alias", "kept", "45678"]);
    s.ok(&["proxy", "start"]);
    let before = s.file("proxy.pid");
    assert_eq!(s.file("proxy.version"), env!("CARGO_PKG_VERSION"));
    std::fs::write(s.root.join("state/proxy.version"), "0.0.1").unwrap();
    let (_, err) = s.ok(&["proxy", "start"]);
    assert!(err.contains("Restarting the Porch proxy"), "{err}");
    assert_ne!(s.file("proxy.pid"), before);
    assert_eq!(s.file("proxy.version"), env!("CARGO_PKG_VERSION"));
    assert!(s.ok(&["list"]).0.contains("kept.localhost"));
    // Same version: no restart.
    let pid = s.file("proxy.pid");
    s.ok(&["proxy", "start"]);
    assert_eq!(s.file("proxy.pid"), pid);
}

#[test]
fn setup_yes_and_doctor_fix_yes_run_cleanly_and_idempotently() {
    let s = Sandbox::new();
    let port = quiet_port();
    for _ in 0..2 {
        let (out, _) = s.ok(&["setup", "--yes", "--port", &port.to_string()]);
        assert!(out.contains("Porch is ready"), "{out}");
        assert!(out.contains("plain HTTP, no sudo"), "{out}");
        assert!(out.contains("Start at login: off"), "{out}");
        assert!(out.contains("porch run"), "{out}");
        assert_eq!(s.settings()["port"], port);
        assert_eq!(s.ok(&["proxy", "status"]).0.trim(), "running");
    }
    let (out, _) = s.ok(&["doctor", "--fix", "--yes"]);
    assert!(out.contains("No problems found"), "{out}");
}

#[test]
fn setup_avoids_a_busy_port_and_doctor_fix_moves_a_stopped_proxy() {
    let s = Sandbox::new();
    let (port, held) = held_port();
    s.save_port(port, serde_json::json!({}));
    let (out, _) = s.ok(&["setup", "--yes"]);
    assert!(out.contains(&format!("Port {port} is used by")), "{out}");
    let moved = s.settings()["port"].as_u64().unwrap() as u16;
    assert_ne!(moved, port);
    s.ok(&["proxy", "stop"]);
    drop(held);
    // Now occupy the new port while the proxy is stopped.
    let _held = hold(moved);
    let (out, _) = s.ok(&["doctor", "--fix", "--yes"]);
    assert!(
        out.contains(&format!("proxy port {moved} is used by")),
        "{out}"
    );
    assert!(out.contains("moved Porch to port"), "{out}");
    assert_ne!(s.settings()["port"], moved);
}
