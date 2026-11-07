//! Regression acceptance for takeover, orphan recovery, reservations and TLD removal.
//! Every daemon, child, hosts setting and state file is isolated from the machine configuration.
#![cfg(unix)]
mod common;
use std::{
    cell::RefCell,
    collections::BTreeSet,
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::{Child, Command, ExitStatus, Output, Stdio},
    time::{Duration, Instant},
};

struct Sandbox {
    root: PathBuf,
    port: u16,
    children: RefCell<Vec<Child>>,
    groups: RefCell<BTreeSet<u32>>,
}
impl Sandbox {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "porch-lifecycle-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("state")).unwrap();
        Self {
            root,
            port: free_port(),
            children: RefCell::new(vec![]),
            groups: RefCell::new(BTreeSet::new()),
        }
    }
    fn command(&self, args: &[&str]) -> Command {
        use std::os::unix::process::CommandExt;
        let mut command = Command::new(env!("CARGO_BIN_EXE_porch"));
        command
            .args(args)
            .current_dir(&self.root)
            .env("PORCH_STATE_DIR", self.root.join("state"))
            .env("PORCH_PORT", self.port.to_string())
            .env("PORCH_APP_PORTS", common::app_ports(self.port))
            .env("PORCH_HTTPS", "0")
            .env("PORCH_PREFIX", "")
            .env("PORCH_SYNC_HOSTS", "0")
            .env("PORCH_SKIP_TRUST", "1")
            .env("CI", "1")
            .env_remove("PORCH_ACTIVE")
            .env_remove("PORTLESS_ACTIVE")
            .env_remove("PORCH_APP_PORT")
            .env_remove("PORTLESS_APP_PORT")
            .env_remove("PORCH")
            .env_remove("PORTLESS")
            .process_group(0);
        command
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }
    fn ok(&self, args: &[&str]) -> Output {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
    fn spawn(&self, args: &[&str]) -> usize {
        let mut children = self.children.borrow_mut();
        let index = children.len();
        let log = fs::File::create(self.root.join(format!("launcher-{index}.log"))).unwrap();
        children.push(
            self.command(args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(log)
                .spawn()
                .unwrap(),
        );
        index
    }
    fn launcher_pid(&self, index: usize) -> u32 {
        self.children.borrow()[index].id()
    }
    fn wait_launcher(&self, index: usize) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = self.children.borrow_mut()[index].try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "launcher did not exit: {}",
                fs::read_to_string(self.root.join(format!("launcher-{index}.log")))
                    .unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn pid_file(&self, name: &str) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(raw) = fs::read_to_string(self.root.join(name)) {
                if let Ok(pid) = raw.trim().parse() {
                    return pid;
                }
            }
            assert!(
                Instant::now() < deadline,
                "{name} was not written; logs: {:?}",
                (0..self.children.borrow().len())
                    .map(
                        |i| fs::read_to_string(self.root.join(format!("launcher-{i}.log")))
                            .unwrap_or_default()
                    )
                    .collect::<Vec<_>>()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn track_group(&self, pid: u32) {
        self.groups.borrow_mut().insert(pid);
    }
    fn status(&self, host: &str) -> u16 {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write!(
            stream,
            "GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut output = String::new();
        stream.read_to_string(&mut output).unwrap();
        output.split_whitespace().nth(1).unwrap().parse().unwrap()
    }
    fn write_takeover_fixture(&self) {
        fs::write(self.root.join("takeover.py"),r#"import os,sys,socket,time,signal,pathlib
name,mode=sys.argv[1:]
s=socket.socket();s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
s.bind(('127.0.0.1',int(os.environ['PORT'])));s.listen()
def delayed(*a):
 time.sleep(1);sys.exit(0)
if mode=='delayed':signal.signal(signal.SIGTERM,delayed)
if mode=='ignore':signal.signal(signal.SIGTERM,signal.SIG_IGN)
pathlib.Path(name+'.pid').write_text(str(os.getpid()))
if name=='old':
 while True:time.sleep(.05)
while True:
 c,a=s.accept();c.recv(65536)
 body=name.encode();c.sendall(b'HTTP/1.1 200 OK\r\nContent-Length: '+str(len(body)).encode()+b'\r\nConnection: close\r\n\r\n'+body);c.close()
"#).unwrap();
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        // Read both fixture pid files and route ownership during failure cleanup:
        // an assertion may fire before the test registered its app group.
        let mut pids = self.groups.borrow().clone();
        if let Ok(raw) = fs::read(self.root.join("state/state.json")) {
            if let Ok(state) = serde_json::from_slice::<serde_json::Value>(&raw) {
                for owner in state["owners"]
                    .as_object()
                    .into_iter()
                    .flat_map(|o| o.values())
                {
                    if let Some(pid) = owner["child"].as_u64() {
                        pids.insert(pid as u32);
                    }
                }
            }
        }
        if let Ok(entries) = fs::read_dir(&self.root) {
            for entry in entries.flatten() {
                if entry.path().extension().is_some_and(|e| e == "pid") {
                    if let Some(pid) = fs::read_to_string(entry.path())
                        .ok()
                        .and_then(|s| s.trim().parse().ok())
                    {
                        pids.insert(pid);
                    }
                }
            }
        }
        for pid in pids {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
                libc::kill(pid as i32, libc::SIGKILL);
            }
        }
        for child in self.children.borrow_mut().iter_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = self.run(&["proxy", "stop"]);
        let _ = fs::remove_dir_all(&self.root);
    }
}

use common::free_port;
fn alive(pid: u32) -> bool {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "stat="])
        .output()
        .unwrap();
    let state = String::from_utf8_lossy(&output.stdout);
    let state = state.trim();
    !state.is_empty() && !state.starts_with('Z')
}
fn wait_dead(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while alive(pid) {
        assert!(Instant::now() < deadline, "process {pid} survived cleanup");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn force_reuses_fixed_port_after_delayed_and_ignored_termination() {
    for mode in ["delayed", "ignore"] {
        let sandbox = Sandbox::new();
        sandbox.write_takeover_fixture();
        let port = loop {
            let p = free_port();
            if p != sandbox.port {
                break p.to_string();
            }
        };
        let first = sandbox.spawn(&[
            "demo",
            "--app-port",
            &port,
            "python3",
            "takeover.py",
            "old",
            mode,
        ]);
        let old = sandbox.pid_file("old.pid");
        sandbox.track_group(old);
        let replacement = sandbox.spawn(&[
            "demo",
            "--app-port",
            &port,
            "--force",
            "python3",
            "takeover.py",
            "new",
            "normal",
        ]);
        let new = sandbox.pid_file("new.pid");
        sandbox.track_group(new);
        wait_dead(old);
        sandbox.wait_launcher(first);
        assert_eq!(
            sandbox.status("demo.localhost"),
            200,
            "replacement unavailable after {mode} termination"
        );
        unsafe {
            libc::kill(sandbox.launcher_pid(replacement) as i32, libc::SIGTERM);
        }
        sandbox.wait_launcher(replacement);
        wait_dead(new);
    }
}

#[test]
fn prune_kills_surviving_grandchild_after_launcher_and_group_leader_exit() {
    let sandbox = Sandbox::new();
    fs::write(
        sandbox.root.join("tree.py"),
        r#"import os,time,pathlib
worker=os.fork()
if worker==0:
 pathlib.Path('worker.pid').write_text(str(os.getpid()))
 while True:time.sleep(.05)
else:
 pathlib.Path('leader.pid').write_text(str(os.getpid()))
 while True:time.sleep(.05)
"#,
    )
    .unwrap();
    let launcher = sandbox.spawn(&["tree", "python3", "tree.py"]);
    let leader = sandbox.pid_file("leader.pid");
    sandbox.track_group(leader);
    let worker = sandbox.pid_file("worker.pid");
    // Simulate abrupt loss only after the launcher recorded the child it owns;
    // prune finds the surviving tree through that record.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let state: serde_json::Value = fs::read(sandbox.root.join("state/state.json"))
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or_default();
        let owner = &state["owners"]["tree"];
        if owner["child"] == leader && owner["child_started"].is_string() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "launcher never recorded its child: {state}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    unsafe {
        libc::kill(sandbox.launcher_pid(launcher) as i32, libc::SIGKILL);
    }
    sandbox.wait_launcher(launcher);
    unsafe {
        libc::kill(leader as i32, libc::SIGTERM);
    }
    wait_dead(leader);
    assert!(alive(worker), "fixture grandchild exited unexpectedly");
    sandbox.ok(&["prune", "--force"]);
    wait_dead(worker);
    let output = sandbox.ok(&["list", "--json"]);
    let routes: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(routes["routes"].as_array().unwrap().is_empty());
}

#[test]
fn automatic_allocator_refuses_blocked_ports_when_the_safe_pool_is_full() {
    let sandbox = Sandbox::new();
    let refused = |range: &str, why: &str| {
        let output = sandbox
            .command(&[
                "blocked",
                "python3",
                "-c",
                "open('spawned','w').write('unexpected')",
            ])
            .env("PORCH_APP_PORTS", range)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{why}: {stderr}");
        assert!(
            stderr.contains(&format!("all app ports {range} are occupied")),
            "{why}: {stderr}"
        );
        assert!(
            !sandbox.root.join("spawned").exists(),
            "{why}: child started"
        );
    };
    // Pools whose only ports are framework-blocked are full, not usable.
    refused("4045-4045", "allocated framework-blocked port 4045");
    refused("4190-4190", "allocated framework-blocked port 4190");
    // Every port of the sandbox's private pool is occupied.
    let pool = common::app_ports(sandbox.port);
    let (first, last) = pool.split_once('-').unwrap();
    let held: Vec<_> = (first.parse::<u16>().unwrap()..=last.parse().unwrap())
        .map(|port| TcpListener::bind(("127.0.0.1", port)).unwrap())
        .collect();
    refused(&pool, "allocated an occupied port");
    drop(held);
}

#[test]
fn dead_routes_do_not_consume_free_app_port_reservations() {
    let sandbox = Sandbox::new();
    let pool = common::app_ports(sandbox.port);
    let (first, last) = pool.split_once('-').unwrap();
    let pool = first.parse::<u16>().unwrap()..=last.parse::<u16>().unwrap();
    let dead:Vec<_>=pool.clone().map(|port|serde_json::json!({"name":format!("dead-{port}"),"target":format!("http://127.0.0.1:{port}"),"pid":2_000_000_000u32})).collect();
    fs::write(
        sandbox.root.join("state/state.json"),
        serde_json::to_vec(&serde_json::json!({"routes":dead})).unwrap(),
    )
    .unwrap();
    let output = sandbox.ok(&[
        "fresh",
        "python3",
        "-c",
        "import os;print(os.environ['PORT'])",
    ]);
    let port: u16 = String::from_utf8(output.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(pool.contains(&port), "{port} outside {pool:?}");
}

#[test]
fn removing_one_tld_from_an_unscoped_alias_preserves_the_other_domain() {
    let sandbox = Sandbox::new();
    sandbox.ok(&["proxy", "start", "--no-tls", "--tld", "localhost,test"]);
    // Unreachable upstream makes502 prove a route still exists without a separate fixture server.
    sandbox.ok(&["alias", "demo", "1"]);
    assert_eq!(sandbox.status("demo.test"), 502);
    assert_eq!(sandbox.status("demo.localhost"), 502);
    sandbox.ok(&["alias", "--remove", "demo", "--tld", "test"]);
    assert_eq!(sandbox.status("demo.test"), 404);
    assert_eq!(sandbox.status("demo.localhost"), 502);
}
