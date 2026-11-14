mod common;
use std::{
    path::PathBuf,
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};
struct Sandbox {
    root: PathBuf,
    port: u16,
}
impl Sandbox {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "porch-cli-{}-{}-{}",
            std::process::id(),
            {
                static NEXT: std::sync::atomic::AtomicUsize =
                    std::sync::atomic::AtomicUsize::new(0);
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            },
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        Self {
            root,
            port: common::free_port(),
        }
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_porch"));
        c.args(args)
            .current_dir(&self.root)
            .env("PORCH_STATE_DIR", self.root.join("state"))
            .env("PORCH_CLEAN_URLS", "0")
            .env("PORCH_PORT", self.port.to_string())
            .env("PORCH_APP_PORTS", common::app_ports(self.port))
            .env("PORCH_HTTPS", "0")
            .env("PORCH_SYNC_HOSTS", "0")
            .env("PORCH_SKIP_TRUST", "1")
            .env("CI", "1");
        c
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }
    /// Asserts the exit code and reports porch's own output on mismatch, so a
    /// startup failure is distinguishable from lost child exit status.
    fn exits(&self, args: &[&str], code: i32) {
        let o = self.run(args);
        assert_eq!(
            o.status.code(),
            Some(code),
            "{args:?} in {}\nstdout: {}\nstderr: {}",
            self.root.display(),
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
    }
    fn ok(&self, args: &[&str]) -> String {
        let o = self.run(args);
        assert!(
            o.status.success(),
            "{args:?} in {}: {}",
            self.root.display(),
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8(o.stdout).unwrap()
    }
    fn state(&self) -> serde_json::Value {
        serde_json::from_slice(
            &std::fs::read(self.root.join("state/state.json"))
                .unwrap_or_else(|_| b"{\"routes\":[]}".to_vec()),
        )
        .unwrap()
    }
    fn wait_route(&self, name: &str) {
        let d = Instant::now() + Duration::from_secs(15);
        while !self.state()["routes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["name"] == name)
        {
            assert!(Instant::now() < d, "route never appeared: {name}");
            std::thread::sleep(Duration::from_millis(30));
        }
    }
    fn write_server(&self) {
        std::fs::write(
            self.root.join("server.py"),
            r#"import os,json,http.server,socket
# HTTPServer resolves its server name during bind; keep this fixture independent of runner DNS.
socket.getfqdn=lambda name: "localhost"
open('child-env.json','w').write(json.dumps(dict(os.environ)))
class Server(http.server.BaseHTTPRequestHandler):
 def do_GET(self):
  self.send_response(200);self.end_headers();self.wfile.write(os.environ['PORTLESS_URL'].encode())
 def log_message(self,*args):pass
http.server.HTTPServer(('127.0.0.1',int(os.environ['PORT'])),Server).serve_forever()
"#,
        )
        .unwrap();
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = self.command(&["proxy", "stop"]).output();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
#[test]
fn aliases_public_urls_scoping_force_and_json() {
    let s = Sandbox::new();
    s.ok(&["alias", "api.demo", "43121"]);
    assert_eq!(
        s.ok(&["get", "api.demo", "--no-worktree"]).trim(),
        format!("http://api.demo.localhost:{}", s.port)
    );
    assert!(!s.run(&["alias", "api.demo", "43122"]).status.success());
    s.ok(&["alias", "api.demo", "43122", "--force"]);
    let v: serde_json::Value = serde_json::from_str(&s.ok(&["list", "--json"])).unwrap();
    assert_eq!(v["routes"][0]["target"], "http://localhost:43122");
    assert!(s.ok(&["list"]).contains("api.demo.localhost"));
    s.ok(&["alias", "--remove", "api.demo"]);
    assert_eq!(s.state()["routes"].as_array().unwrap().len(), 0);
    assert!(s
        .ok(&["get", "unregistered", "--no-worktree"])
        .contains("unregistered.localhost"));
}
#[test]
fn malformed_flags_and_names_fail_without_mutation() {
    let s = Sandbox::new();
    for a in [
        vec!["run", "--app-port", "0", "echo"],
        vec!["run", "--unknown", "echo"],
        vec!["alias", "bad/name", "1234"],
        vec!["alias", "-bad", "1234"],
        vec!["alias", "good", "ftp://x"],
        vec!["proxy", "start", "--port", "0"],
        vec!["proxy", "start", "--cert", "missing"],
    ] {
        assert!(!s.run(&a).status.success(), "{a:?}");
    }
    assert_eq!(s.state()["routes"].as_array().unwrap().len(), 0);
}
#[test]
fn concurrent_writers_preserve_every_route() {
    let s = Sandbox::new();
    let mut children = Vec::new();
    for i in 0..24 {
        children.push(
            s.command(&["alias", &format!("route{i}"), "1234"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
    }
    for mut c in children {
        assert!(c.wait().unwrap().success());
    }
    assert_eq!(s.state()["routes"].as_array().unwrap().len(), 24);
}
#[test]
fn real_autostart_launch_forward_env_signal_and_readiness() {
    let s = Sandbox::new();
    s.write_server();
    let app_port = common::free_port().to_string();
    let mut child = s
        .command(&[
            "demo",
            "--app-port",
            &app_port,
            "--",
            "python3",
            "server.py",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    s.wait_route("demo");
    assert!(s.root.join("state/proxy.ready").is_file());
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let d = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(r) = client
                .get(format!("http://127.0.0.1:{}/", s.port))
                .header("host", "demo.localhost")
                .send()
                .await
            {
                if r.status().is_success() {
                    assert_eq!(
                        r.text().await.unwrap(),
                        format!("http://demo.localhost:{}", s.port)
                    );
                    break;
                }
            }
            assert!(Instant::now() < d);
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    });
    let env: serde_json::Value =
        serde_json::from_slice(&std::fs::read(s.root.join("child-env.json")).unwrap()).unwrap();
    assert_eq!(env["PORT"], app_port);
    assert_eq!(env["HOST"], "127.0.0.1");
    #[cfg(unix)]
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    };
    #[cfg(windows)]
    {
        assert!(
            porch::shutdown::request(child.id()),
            "launcher must accept a graceful stop request"
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("launcher did not exit after a graceful stop request");
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    }
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(143));
    assert!(s.state()["routes"].as_array().unwrap().is_empty());
}
#[test]
fn bare_script_named_run_reserved_name_and_exit_code() {
    let s = Sandbox::new();
    std::fs::write(s.root.join("package.json"),r#"{"name":"fixture","scripts":{"dev":"node -e \"process.exit(23)\"","start":"node -e \"process.exit(17)\""},"portless":{"name":"configured"}}"#).unwrap();
    s.exits(&[], 23);
    s.exits(&["--script", "start"], 17);
    s.exits(
        &[
            "run",
            "--name",
            "explicit",
            "node",
            "-e",
            "process.exit(31)",
        ],
        31,
    );
    s.exits(&["--name", "proxy", "node", "-e", "process.exit(19)"], 19);
    assert!(s.state()["routes"].as_array().unwrap().is_empty());
}
#[test]
fn bypass_does_not_start_proxy_or_inject_port() {
    let s = Sandbox::new();
    let o = s
        .command(&[
            "run",
            "node",
            "-e",
            "console.log(process.env.PORTLESS_URL||'bypassed')",
        ])
        .env("PORTLESS", "0")
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(String::from_utf8(o.stdout).unwrap().trim(), "bypassed");
    assert!(!s.root.join("state/proxy.pid").exists());
}
#[test]
fn daemon_remembers_settings_and_rejects_bind_failure() {
    let s = Sandbox::new();
    s.ok(&[
        "proxy",
        "start",
        "--no-tls",
        "--tld",
        "localhost,test",
        "--wildcard",
    ]);
    s.ok(&["proxy", "stop"]);
    assert_eq!(s.ok(&["proxy", "status"]).trim(), "stopped");
    s.ok(&["run", "node", "-e", "process.exit(0)"]);
    let v: serde_json::Value =
        serde_json::from_slice(&std::fs::read(s.root.join("state/proxy.json")).unwrap()).unwrap();
    assert_eq!(v["tlds"], serde_json::json!(["localhost", "test"]));
    assert_eq!(v["wildcard"], true);
    s.ok(&["proxy", "stop"]);
    // Occupy the port with a live listener. The stopped proxy's connections can
    // leave the port in TIME_WAIT, which blocks a plain bind but is not what
    // this test is about.
    let listener = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )
    .unwrap();
    let address: socket2::SockAddr = std::net::SocketAddr::from(([127, 0, 0, 1], s.port)).into();
    // Unix: SO_REUSEADDR binds over TIME_WAIT, never over a live listener.
    #[cfg(unix)]
    {
        listener.set_reuse_address(true).unwrap();
        listener.bind(&address).unwrap();
    }
    // Windows: SO_REUSEADDR would also bind over a live listener, so wait for
    // the port to be free instead. The bound is a watchdog (Windows keeps
    // TIME_WAIT for at most 240s); a passing run binds at once or soon after.
    #[cfg(windows)]
    {
        let deadline = Instant::now() + Duration::from_secs(240);
        while let Err(e) = listener.bind(&address) {
            // Only "in use" is worth waiting out.
            assert_eq!(e.kind(), std::io::ErrorKind::AddrInUse, "{e}");
            assert!(
                Instant::now() < deadline,
                "port {} never freed: {e}",
                s.port
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    listener.listen(16).unwrap();
    assert!(!s.run(&["proxy", "start"]).status.success());
    assert!(!s.root.join("state/proxy.ready").exists());
    drop(listener);
}
#[cfg(unix)]
#[test]
fn orphan_prune_kills_child_group_and_preserves_static_alias() {
    let s = Sandbox::new();
    s.write_server();
    s.ok(&["alias", "static", "12345"]);
    let mut child = s
        .command(&["orphan", "python3", "server.py"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    s.wait_route("orphan");
    let d = Instant::now() + Duration::from_secs(5);
    let pid = loop {
        if let Some(p) = s.state()["owners"]["orphan"]["child"].as_u64() {
            break p as u32;
        }
        assert!(Instant::now() < d);
        std::thread::sleep(Duration::from_millis(30));
    };
    unsafe {
        libc::kill(child.id() as i32, libc::SIGKILL);
    };
    child.wait().unwrap();
    s.ok(&["prune", "--force"]);
    assert_eq!(s.state()["routes"].as_array().unwrap().len(), 1);
    assert_eq!(s.state()["routes"][0]["name"], "static");
    let d = Instant::now() + Duration::from_secs(5);
    while porch::state::alive(pid) {
        assert!(Instant::now() < d, "orphan child still alive");
        std::thread::sleep(Duration::from_millis(30));
    }
}

#[cfg(unix)]
#[test]
fn https_opt_in_ca_environment_allows_node_to_call_another_app() {
    let s = Sandbox::new();
    let mut server=s.command(&["secure","node","-e","const fs=require('fs');require('http').createServer((q,r)=>r.end('from-secure')).listen(process.env.PORT,'127.0.0.1',()=>{fs.writeFileSync('secure.tmp','');fs.renameSync('secure.tmp','secure.ready')})"])
      .env("PORCH_HTTPS", "1").stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    s.wait_route("secure");
    // A registered route only means the app was started; the client must not
    // reach it (and get a 502) before it listens.
    let d = Instant::now() + Duration::from_secs(30);
    while !s.root.join("secure.ready").exists() {
        assert!(Instant::now() < d, "secure app never listened");
        std::thread::sleep(Duration::from_millis(30));
    }
    let settings: serde_json::Value =
        serde_json::from_slice(&std::fs::read(s.root.join("state/proxy.json")).unwrap()).unwrap();
    assert_eq!(settings["tls"], true);
    // One request: the proxy and the app are both ready, so any failure is real.
    let script=format!("const https=require('https');https.get({{hostname:'127.0.0.1',port:{},servername:'secure.localhost',headers:{{host:'secure.localhost'}}}},r=>{{let b='';r.on('data',c=>b+=c);r.on('end',()=>{{console.log(b);if(b!=='from-secure')console.error('status '+r.statusCode+': '+b);process.exit(b==='from-secure'?0:1)}})}}).on('error',e=>{{console.error(e);process.exit(1)}})",s.port);
    let output = s
        .command(&["client", "node", "-e", &script])
        .env("PORCH_HTTPS", "1")
        .output()
        .unwrap();
    unsafe {
        libc::kill(server.id() as i32, libc::SIGTERM);
    };
    let _ = server.wait();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        "from-secure"
    );
    let state = s.state();
    assert!(state["routes"].as_array().unwrap().is_empty(), "{state:#}");
}

#[cfg(unix)]
#[test]
fn concurrent_autostarts_reserve_distinct_ports_and_one_ready_daemon() {
    let s = Sandbox::new();
    let mut children = Vec::new();
    for name in ["one", "two", "three"] {
        children.push(
            s.command(&[name, "node", "-e", "setInterval(()=>{},1000)"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
    }
    for name in ["one", "two", "three"] {
        s.wait_route(name);
    }
    let state = s.state();
    let mut ports = state["routes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["target"].as_str().unwrap())
        .collect::<Vec<_>>();
    ports.sort();
    ports.dedup();
    for mut child in children {
        unsafe {
            libc::kill(child.id() as i32, libc::SIGTERM);
        };
        let _ = child.wait();
    }
    assert_eq!(ports.len(), 3);
    assert!(s.state()["routes"].as_array().unwrap().is_empty());
    assert_eq!(s.ok(&["proxy", "status"]).trim(), "running");
}
#[test]
fn global_reserved_name_bypass_and_unregistered_get_work() {
    let s = Sandbox::new();
    let out = s.run(&[
        "--prefix",
        "review",
        "--name",
        "list",
        "node",
        "-e",
        "console.log(process.env.PORTLESS_URL)",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8(out.stdout)
        .unwrap()
        .contains("review.list.localhost"));
    assert_eq!(
        s.ok(&["get", "backend", "--no-worktree"]).trim(),
        format!("http://backend.localhost:{}", s.port)
    );
}

#[cfg(unix)]
#[test]
fn self_daemonized_server_inherits_route_until_explicit_takeover() {
    let s = Sandbox::new();
    std::fs::write(
        s.root.join("daemon.py"),
        r#"import os,http.server,socket
socket.getfqdn=lambda name: "localhost"
pid=os.fork()
if pid:os._exit(0)
os.setsid()
fd=os.open(os.devnull,os.O_RDWR)
for target in (0,1,2):os.dup2(fd,target)
open('daemon.pid','w').write(str(os.getpid()))
class Server(http.server.BaseHTTPRequestHandler):
 def do_GET(self):self.send_response(200);self.end_headers();self.wfile.write(b'daemon survived')
 def log_message(self,*args):pass
http.server.HTTPServer(('127.0.0.1',int(os.environ['PORT'])),Server).serve_forever()
"#,
    )
    .unwrap();
    let out = s.run(&["daemon", "python3", "daemon.py"]);
    let pid = std::fs::read_to_string(s.root.join("daemon.pid"))
        .unwrap()
        .parse::<u32>()
        .unwrap();
    // Explicit takeover is responsible for stopping the retained detached server.
    let retained = s.state()["routes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["name"] == "daemon" && r["pid"] == pid);
    // The daemon left the launched command's group (setsid): only its own pid
    // is recorded, so stopping the route never signals a group.
    let single_process = s.state()["owners"]["daemon"]["child"].is_null();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let body = runtime.block_on(async {
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://127.0.0.1:{}", s.port))
            .header("host", "daemon.localhost")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    });
    let takeover = s.run(&["alias", "daemon", "12345", "--force"]);
    if porch::state::alive(pid) {
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(retained);
    assert!(single_process, "{}", s.state());
    assert_eq!(body, "daemon survived");
    assert!(
        takeover.status.success(),
        "{}",
        String::from_utf8_lossy(&takeover.stderr)
    );
}

#[test]
fn proxy_and_app_refuse_wildcard_port_collisions() {
    for ipv6 in [false, true] {
        let domain = if ipv6 {
            socket2::Domain::IPV6
        } else {
            socket2::Domain::IPV4
        };
        let socket =
            socket2::Socket::new(domain, socket2::Type::STREAM, Some(socket2::Protocol::TCP))
                .unwrap();
        // Match real servers that enable reuse on macOS, including dual-stack [::].
        socket.set_reuse_address(true).unwrap();
        if ipv6 {
            socket.set_only_v6(false).unwrap();
        }
        let addr: std::net::SocketAddr = if ipv6 { "[::]:0" } else { "0.0.0.0:0" }.parse().unwrap();
        socket.bind(&addr.into()).unwrap();
        socket.listen(16).unwrap();
        let port = socket.local_addr().unwrap().as_socket().unwrap().port();
        let s = Sandbox::new();
        let out = s.run(&[
            "proxy",
            "start",
            "--no-tls",
            "--bind",
            "127.0.0.1",
            "--port",
            &port.to_string(),
        ]);
        assert!(
            !out.status.success(),
            "proxy silently shared {} wildcard port {}",
            if ipv6 { "dualstack" } else { "IPv4" },
            port
        );
        assert!(String::from_utf8_lossy(&out.stderr).contains("unavailable"));
        s.ok(&["proxy", "start", "--no-tls"]);
        let out = s.run(&["collision", "--app-port", &port.to_string(), "--", "true"]);
        assert!(
            !out.status.success(),
            "app silently shared wildcard port {port}"
        );
        assert!(String::from_utf8_lossy(&out.stderr).contains("unavailable"));
    }
}

#[cfg(unix)]
#[test]
fn proxy_stays_within_its_descriptor_limit() {
    // Two daemons with a soft limit of 64 descriptors and 110 clients that
    // never finish a request head. With a hard limit of 64 the proxy can't
    // raise its limit: it serves what fits, sheds the rest without running
    // out of descriptors, and recovers. With room to raise it, it serves all
    // of them and still answers a new client.
    let script = r#"
import os,resource,socket,subprocess,sys,tempfile,time
exe=sys.argv[1]
_,parent_hard=resource.getrlimit(resource.RLIMIT_NOFILE)
def run(hard,port):
 with tempfile.TemporaryDirectory(prefix='porch-fds-') as root:
  env=os.environ.copy();env.update(PORCH_STATE_DIR=root,PORCH_SYNC_HOSTS='0',PORCH_SKIP_TRUST='1',PORCH_CLEAN_URLS='0')
  def limit():resource.setrlimit(resource.RLIMIT_NOFILE,(64,hard))
  def fresh(timeout):
   with socket.create_connection(('127.0.0.1',port),timeout=timeout) as c:
    c.sendall(b'GET / HTTP/1.1\r\nHost: unknown.localhost\r\nConnection: close\r\n\r\n')
    c.settimeout(timeout);return c.recv(4096)
  with open(root+'/log','w+') as log:
   child=subprocess.Popen([exe,'proxy','start','--foreground','--no-tls','--bind','127.0.0.1','--port',str(port)],env=env,stdout=log,stderr=log,preexec_fn=limit)
   sockets=[]
   try:
    deadline=time.monotonic()+10
    while not os.path.exists(root+'/proxy.ready'):
     assert child.poll() is None,'proxy exited before ready: '+open(root+'/log').read()
     assert time.monotonic()<deadline,'proxy never ready'
     time.sleep(.02)
    for i in range(110):
     c=socket.create_connection(('127.0.0.1',port),timeout=2)
     c.sendall(b'GET / HTTP/1.1\r\n');sockets.append(c)
    if hard==64:
     # Connections are accepted in order: once the last one is shed, the
     # proxy has handled every one.
     sockets[-1].settimeout(10)
     try:assert sockets[-1].recv(1)==b'','connection beyond the cap was served'
     except ConnectionResetError:pass
    else:
     assert b'404' in fresh(5),'no room for a new client'
    # A separate handle: seeking `log` would move the daemon's write offset.
    assert 'accept failed' not in open(root+'/log').read(),'proxy ran out of descriptors'
    assert child.poll() is None,'connection flood terminated daemon'
    for c in sockets:c.close()
    sockets=[]
    deadline=time.monotonic()+5
    while True:
     try:
      if b'404' in fresh(1):break
     except (OSError,TimeoutError):pass
     assert time.monotonic()<deadline,'proxy did not recover after the flood'
   finally:
    for c in sockets:c.close()
    if child.poll() is None:child.terminate()
    try:child.wait(timeout=5)
    except subprocess.TimeoutExpired:child.kill();child.wait()
# A port each: on Linux the first daemon's closed connections can keep its
# port from being bound again for a while.
run(64,int(sys.argv[2]))
if parent_hard==resource.RLIM_INFINITY or parent_hard>=4096:run(4096,int(sys.argv[3]))
"#;
    let out = Command::new("python3")
        .args([
            "-c",
            script,
            env!("CARGO_BIN_EXE_porch"),
            // Private ports: one freed by Python could be reassigned before
            // the daemon binds it.
            &common::free_port().to_string(),
            &common::free_port().to_string(),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn https_opt_in_keeps_the_no_sudo_port() {
    let s = Sandbox::new();
    let url = |extra: &[(&str, &str)]| {
        let mut c = s.command(&["get", "web"]);
        c.env_remove("PORCH_PORT").env("PORCH_HTTPS", "1");
        for (k, v) in extra {
            c.env(k, v);
        }
        let o = c.output().unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8(o.stdout).unwrap().trim().to_owned()
    };
    // HTTPS no longer implies 443 (and sudo); it stays on the default high port.
    assert_eq!(url(&[]), "https://web.localhost:7462");
    // The standard port is still available when asked for explicitly.
    assert_eq!(url(&[("PORCH_PORT", "443")]), "https://web.localhost");
}

#[test]
fn saved_proxy_ports_survive_the_https_default_change() {
    let s = Sandbox::new();
    let state = s.root.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let url = |saved: &str, https: &str| {
        std::fs::write(state.join("proxy.json"), saved).unwrap();
        let o = s
            .command(&["get", "web"])
            .env_remove("PORCH_PORT")
            .env("PORCH_HTTPS", https)
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8(o.stdout).unwrap().trim().to_owned()
    };
    // Existing HTTPS-on-443 setups keep their port.
    assert_eq!(
        url(r#"{"port":443,"tls":true,"tlds":["localhost"]}"#, "1"),
        "https://web.localhost"
    );
    // Switching protocols never lands on the other protocol's privileged port.
    assert_eq!(
        url(r#"{"port":80,"tls":false,"tlds":["localhost"]}"#, "1"),
        "https://web.localhost:7462"
    );
    assert_eq!(
        url(r#"{"port":443,"tls":true,"tlds":["localhost"]}"#, "0"),
        "http://web.localhost:7462"
    );
    // A custom high port is kept when HTTPS is turned on.
    assert_eq!(
        url(r#"{"port":8080,"tls":false,"tlds":["localhost"]}"#, "1"),
        "https://web.localhost:8080"
    );
}

#[test]
fn bare_domain_serves_a_dashboard_and_app_list() {
    let s = Sandbox::new();
    let up = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let up_port = up.local_addr().unwrap().port();
    s.ok(&["alias", "web", &up_port.to_string()]);
    s.ok(&["alias", "--private", "secret", "1"]);
    s.ok(&["proxy", "start"]);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let get = |path: &str, accept: &str| {
            client
                .get(format!("http://127.0.0.1:{}{path}", s.port))
                .header("host", "localhost")
                .header("accept", accept.to_owned())
                .send()
        };
        let page = get("/", "text/html").await.unwrap();
        assert_eq!(page.status(), 200);
        let html = page.text().await.unwrap();
        assert!(
            html.contains("Your apps") && html.contains("web.localhost"),
            "{html}"
        );
        let json: serde_json::Value = get("/_porch/apps.json", "*/*")
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let apps = json["apps"].as_array().unwrap();
        let web = apps.iter().find(|a| a["name"] == "web").unwrap();
        assert_eq!(web["health"], "ok");
        let secret = apps.iter().find(|a| a["name"] == "secret").unwrap();
        assert_eq!(secret["private"], true);
        assert_eq!(secret["health"], "not listening");
        // An unknown name is still a 404, not the dashboard.
        let missing = client
            .get(format!("http://127.0.0.1:{}/", s.port))
            .header("host", "nope.localhost")
            .header("accept", "text/html")
            .send()
            .await
            .unwrap();
        assert_eq!(missing.status(), 404);
    });
    drop(up);
}

#[test]
fn private_survives_partial_alias_edits_and_urls_are_stored_canonically() {
    let s = Sandbox::new();
    std::fs::create_dir_all(s.root.join("state")).unwrap();
    std::fs::write(
        s.root.join("state/proxy.json"),
        serde_json::to_vec(&serde_json::json!({
            "port": s.port, "tls": false, "tlds": ["localhost", "test"], "unprivileged": true
        }))
        .unwrap(),
    )
    .unwrap();
    s.ok(&["alias", "hidden", "3000", "--private"]);
    s.ok(&["remove", "hidden", "--tld", "test"]);
    let state = s.state();
    let private: Vec<_> = state["private"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    let routes: Vec<_> = state["routes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(routes, ["hidden.localhost"]);
    assert_eq!(private, ["hidden.localhost"]);
    // Scoped replacement keeps the untouched domain private too.
    s.ok(&["alias", "both", "3001", "--private"]);
    s.ok(&["alias", "both", "3002", "--tld", "test", "--force"]);
    let state = s.state();
    let private = state["private"].to_string();
    assert!(private.contains("both.localhost"), "{state}");
    // URLs are stored percent-encoded, credentials refused.
    s.ok(&["alias", "spaced", "http://127.0.0.1:3003/hello world"]);
    assert!(s
        .state()
        .to_string()
        .contains("http://127.0.0.1:3003/hello%20world"));
    assert!(!s
        .run(&["alias", "creds", "http://u:p@127.0.0.1:3004"])
        .status
        .success());
}

#[test]
fn upstream_alias_dials_loopback_with_its_own_host_and_verifies_tls() {
    let s = Sandbox::new();
    let dir = s.root.join("backend");
    std::fs::create_dir_all(&dir).unwrap();
    let cert = Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-keyout",
            "k.pem",
            "-out",
            "c.pem",
            "-subj",
            "/CN=api.internal",
            "-addext",
            "subjectAltName=DNS:api.internal",
            "-days",
            "1",
        ])
        .current_dir(&dir)
        .output();
    if !cert.is_ok_and(|o| o.status.success()) {
        eprintln!("skipping: openssl unavailable");
        return;
    }
    let tls_port = common::free_port();
    let plain_port = common::free_port();
    let script = "import http.server,ssl,sys\nclass H(http.server.BaseHTTPRequestHandler):\n def do_GET(self):\n  b=('host='+str(self.headers.get('Host'))).encode();self.send_response(200);self.send_header('Content-Length',str(len(b)));self.end_headers();self.wfile.write(b)\n def log_message(self,*a):pass\ns=http.server.HTTPServer(('127.0.0.1',int(sys.argv[1])),H)\nif sys.argv[2]=='tls':\n c=ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER);c.load_cert_chain('c.pem','k.pem');s.socket=c.wrap_socket(s.socket,server_side=True)\nprint('ready',flush=True)\ns.serve_forever()\n";
    struct Servers(Vec<std::process::Child>);
    impl Drop for Servers {
        fn drop(&mut self) {
            for server in &mut self.0 {
                let _ = server.kill();
                let _ = server.wait();
            }
        }
    }
    let mut servers: Vec<_> = [(tls_port, "tls"), (plain_port, "plain")]
        .into_iter()
        .map(|(port, mode)| {
            Command::new("python3")
                .args(["-c", script, &port.to_string(), mode])
                .current_dir(&dir)
                .stdout(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for server in &mut servers {
        let mut line = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(server.stdout.as_mut().unwrap()),
            &mut line,
        )
        .unwrap();
    }
    let servers = Servers(servers);
    s.ok(&[
        "alias",
        "verified",
        &tls_port.to_string(),
        "--upstream",
        "https://api.internal",
    ]);
    s.ok(&[
        "alias",
        "insecure",
        &tls_port.to_string(),
        "--upstream",
        "https://api.internal",
        "--insecure",
    ]);
    s.ok(&[
        "alias",
        "plain",
        &plain_port.to_string(),
        "--upstream",
        "http://api.internal:8080",
    ]);
    s.ok(&["proxy", "start"]);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let get = |name: &str| {
            client
                .get(format!("http://127.0.0.1:{}/", s.port))
                .header("host", format!("{name}.localhost"))
                .send()
        };
        // A self-signed backend is refused unless the alias opted out.
        let verified = get("verified").await.unwrap();
        assert_eq!(verified.status(), 502);
        assert!(verified
            .text()
            .await
            .unwrap()
            .contains("upstream_certificate"));
        assert_eq!(
            get("insecure").await.unwrap().text().await.unwrap(),
            "host=api.internal"
        );
        assert_eq!(
            get("plain").await.unwrap().text().await.unwrap(),
            "host=api.internal:8080"
        );
    });
    drop(servers);
}

#[cfg(unix)]
#[test]
fn detached_logs_survive_force_replacement_and_invalid_utf8() {
    let s = Sandbox::new();
    let app = "import os,socket,sys,time\ns=socket.socket()\ns.bind(('127.0.0.1',int(os.environ['PORT'])))\ns.listen()\nfor i in range(5): print('line',i,flush=True)\nsys.stdout.buffer.write(b'bad \\xff byte\\n');sys.stdout.flush()\nwhile True:\n print('tick',flush=True);time.sleep(0.05)";
    s.ok(&["run", "--detach", "--name", "bg", "python3", "-c", app]);
    let deadline = Instant::now() + Duration::from_secs(15);
    while !String::from_utf8_lossy(&s.run(&["logs", "bg"]).stdout).contains("tick") {
        assert!(Instant::now() < deadline, "no output from the first run");
        std::thread::sleep(Duration::from_millis(50));
    }
    // One invalid byte no longer makes the whole log unreadable.
    let all = s.run(&["logs", "bg"]);
    assert!(
        all.status.success(),
        "{}",
        String::from_utf8_lossy(&all.stderr)
    );
    let all = String::from_utf8(all.stdout).expect("logs prints valid UTF-8");
    assert!(
        all.contains("line 0") && all.contains("bad \u{FFFD} byte"),
        "{all}"
    );
    let last = s.ok(&["logs", "bg", "--lines", "1"]);
    assert_eq!(last.lines().count(), 1, "{last}");
    // Replacing it: the old run's output moves to bg.log.1, and the new log
    // never gets the old launcher's writes (or a hole of NUL bytes).
    s.ok(&["run", "--detach", "--force", "--name", "bg", "python3", "-c", "import os,socket,time\ns=socket.socket()\ns.bind(('127.0.0.1',int(os.environ['PORT'])))\ns.listen()\nprint('second run',flush=True)\nwhile True: time.sleep(1)"]);
    let deadline = Instant::now() + Duration::from_secs(15);
    while !s.ok(&["logs", "bg"]).contains("second run") {
        assert!(Instant::now() < deadline, "no output from the second run");
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(300));
    let log = std::fs::read(s.root.join("state/logs/bg.log")).unwrap();
    let previous = std::fs::read(s.root.join("state/logs/bg.log.1")).unwrap();
    s.ok(&["stop", "bg"]);
    assert!(!log.contains(&0), "NUL bytes in the new log");
    assert!(
        !String::from_utf8_lossy(&log).contains("tick"),
        "old run wrote into the new log"
    );
    assert!(String::from_utf8_lossy(&previous).contains("line 0"));
}

#[test]
fn detached_runs_answer_log_and_stop() {
    let s = Sandbox::new();
    let o = s.run(&[
        "run", "--detach", "--name", "bg", "python3", "-c",
        "import os,socket,time\ns=socket.socket()\ns.bind(('127.0.0.1',int(os.environ['PORT'])))\ns.listen()\nprint('hello from bg',flush=True)\nwhile True: time.sleep(1)",
    ]);
    let out = String::from_utf8_lossy(&o.stdout).into_owned();
    assert!(
        o.status.success(),
        "{out}{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert_eq!(out.trim(), format!("http://bg.localhost:{}", s.port));
    // The app answers once run returns (allowing a moment for the health
    // probe on slow CI runners); on failure, show what the app printed.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let list: serde_json::Value = serde_json::from_str(&s.ok(&["list", "--json"])).unwrap();
        if list["routes"][0]["health"] == "ok" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{list}\nrun stderr: {}\nlog: {}",
            String::from_utf8_lossy(&o.stderr),
            String::from_utf8_lossy(&s.run(&["logs", "bg"]).stdout)
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    // The app listens before it prints, so its line can land in the log just
    // after health turns ok.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let logs = s.ok(&["logs", "bg"]);
        if logs.contains("hello from bg") {
            break;
        }
        assert!(Instant::now() < deadline, "{logs}");
        std::thread::sleep(Duration::from_millis(50));
    }
    // A second copy is refused without touching the running one or its log.
    let again = s.run(&["run", "--detach", "--name", "bg", "python3", "-c", "pass"]);
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("already running"));
    assert!(s.ok(&["logs", "bg"]).contains("hello from bg"));
    assert!(s.ok(&["stop", "bg"]).contains("Stopped"));
    let list: serde_json::Value = serde_json::from_str(&s.ok(&["list", "--json"])).unwrap();
    assert!(list["routes"].as_array().unwrap().is_empty(), "{list}");
    assert!(!s.run(&["stop", "bg"]).status.success());
    // A command that dies before answering reports it instead of hanging.
    let o = s.run(&[
        "run",
        "--detach",
        "--name",
        "dies",
        "python3",
        "-c",
        "raise SystemExit(3)",
    ]);
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("exited"));
}
/// An app that does nothing for a minute; `porch stop` ends it sooner.
const IDLE: [&str; 3] = ["python3", "-c", "import time\ntime.sleep(60)"];
#[test]
fn private_applies_wherever_the_flag_is_given() {
    let s = Sandbox::new();
    let launches: Vec<(&str, Vec<&str>, bool)> = vec![
        ("p-run", vec!["run", "--private", "--name", "p-run"], true),
        ("p-named", vec!["p-named", "--private"], true),
        ("p-flag", vec!["--name", "p-flag", "--private"], true),
        ("p-first", vec!["--private", "--name", "p-first"], true),
        ("p-top", vec!["--private", "p-top"], true),
        ("shared", vec!["shared"], false),
    ];
    let mut children = Vec::new();
    for (name, args, _) in &launches {
        let args: Vec<&str> = args.iter().chain(IDLE.iter()).copied().collect();
        children.push(
            s.command(&args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        s.wait_route(name);
    }
    // PORCH_PRIVATE takes the usual spellings of "on".
    let args: Vec<&str> = ["run", "--name", "p-env"]
        .iter()
        .chain(IDLE.iter())
        .copied()
        .collect();
    children.push(
        s.command(&args)
            .env("PORCH_PRIVATE", "Yes")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    s.wait_route("p-env");
    let list: serde_json::Value = serde_json::from_str(&s.ok(&["list", "--json"])).unwrap();
    let private = |name: &str| {
        list["routes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == name)
            .unwrap_or_else(|| panic!("{name} missing: {list}"))["private"]
            .clone()
    };
    for (name, _, expected) in &launches {
        assert_eq!(private(name), *expected, "{name}: {list}");
    }
    assert_eq!(private("p-env"), true, "{list}");
    for name in [
        "p-run", "p-named", "p-flag", "p-first", "p-top", "shared", "p-env",
    ] {
        s.ok(&["stop", name]);
    }
    for mut c in children {
        c.wait().unwrap();
    }
}
#[test]
fn launch_options_before_name_or_separator_are_options() {
    let s = Sandbox::new();
    // -d before --name: `python3` is the command, not the app name.
    let o = s.run(&[
        "-d", "--name", "api", "python3", "-c",
        "import os,socket,time\ns=socket.socket()\ns.bind(('127.0.0.1',int(os.environ['PORT'])))\ns.listen()\nwhile True: time.sleep(1)",
    ]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(
        String::from_utf8_lossy(&o.stdout).trim(),
        format!("http://api.localhost:{}", s.port)
    );
    let names: Vec<_> = s.state()["routes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].clone())
        .collect();
    assert_eq!(names, vec![serde_json::json!("api")]);
    s.ok(&["stop", "api"]);
    // --private before `--`: the rest goes to the dev script, not into a name.
    std::fs::write(
        s.root.join("package.json"),
        r#"{"name":"fixture","scripts":{"dev":"node -e \"process.exit(7)\""}}"#,
    )
    .unwrap();
    s.exits(&["--private", "--", "--filter=web"], 7);
    s.exits(&["--prefix=", "--script=dev", "--", "--filter=web"], 7);
    // An app name and --name together are refused rather than one dropped.
    let o = s.run(&["web", "--name", "api", "node", "-e", "0"]);
    assert!(!o.status.success());
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("two app names"),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
}
#[test]
fn on_off_environment_values_are_strict() {
    let s = Sandbox::new();
    let o = s
        .command(&["list"])
        .env("PORCH_HTTPS", "maybe")
        .output()
        .unwrap();
    assert!(!o.status.success());
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(
        err.contains("PORCH_HTTPS") && err.contains("maybe"),
        "{err}"
    );
    for (value, scheme) in [("on", "https"), ("OFF", "http"), ("no", "http")] {
        let o = s
            .command(&["get", "x", "--no-worktree"])
            .env("PORCH_HTTPS", value)
            .output()
            .unwrap();
        let out = String::from_utf8_lossy(&o.stdout);
        assert!(out.starts_with(&format!("{scheme}://")), "{value}: {out}");
    }
}
#[test]
fn lan_from_an_app_launch_is_not_saved_and_lan_mode_warns() {
    let s = Sandbox::new();
    let state = s.root.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let local = format!("http://x.local:{}", s.port);
    let localhost = format!("http://x.localhost:{}", s.port);
    // The app's --lan reaches the proxy as PORCH_LAN; nothing is written.
    let o = s
        .command(&["get", "x", "--no-worktree"])
        .env("PORCH_LAN", "1")
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&o.stdout).trim(), local);
    assert!(!state.join("proxy.json").exists());
    // A proxy an app put in LAN mode, now stopped: the next start is local again.
    std::fs::write(
        state.join("proxy.json"),
        format!(
            r#"{{"port":{},"tls":false,"tlds":["local"],"lan":true,"lan_temporary":["localhost","test"],"unprivileged":true}}"#,
            s.port
        ),
    )
    .unwrap();
    assert_eq!(s.ok(&["get", "x", "--no-worktree"]).trim(), localhost);
    // LAN mode saved by an older Porch (or `proxy start --lan`) stays on, with a warning.
    std::fs::write(
        state.join("proxy.json"),
        format!(
            r#"{{"port":{},"tls":false,"tlds":["local"],"lan":true,"unprivileged":true}}"#,
            s.port
        ),
    )
    .unwrap();
    assert_eq!(s.ok(&["get", "x", "--no-worktree"]).trim(), local);
    let list = s.ok(&["list"]);
    assert!(
        list.contains("LAN mode is on") && list.contains("--no-lan"),
        "{list}"
    );
    let doctor = String::from_utf8_lossy(&s.run(&["doctor"]).stdout).into_owned();
    assert!(doctor.contains("LAN mode is on"), "{doctor}");
    // --no-lan turns it off and saves that.
    s.ok(&["proxy", "start", "--no-lan"]);
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(state.join("proxy.json")).unwrap()).unwrap();
    assert_eq!(saved["lan"], false, "{saved}");
    assert_eq!(saved["tlds"], serde_json::json!(["localhost"]), "{saved}");
    assert!(!s.ok(&["list"]).contains("LAN mode"));
    s.ok(&["proxy", "stop"]);
}
#[test]
fn fields_from_a_newer_porch_survive_and_its_marker_is_respected() {
    let s = Sandbox::new();
    let state = s.root.join("state");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        state.join("state.json"),
        r#"{"routes":[],"future_field":{"keep":["me"]}}"#,
    )
    .unwrap();
    std::fs::write(
        state.join("proxy.json"),
        format!(
            r#"{{"port":{},"tls":false,"unprivileged":true,"future_setting":7}}"#,
            s.port
        ),
    )
    .unwrap();
    s.ok(&["alias", "kept", "43123"]);
    assert_eq!(
        s.state()["future_field"],
        serde_json::json!({"keep": ["me"]})
    );
    s.ok(&["proxy", "start"]);
    s.ok(&["proxy", "stop"]);
    let proxy: serde_json::Value =
        serde_json::from_slice(&std::fs::read(state.join("proxy.json")).unwrap()).unwrap();
    assert_eq!(proxy["future_setting"], 7, "{proxy}");
    // A newer Porch that needs its fields kept marks the file; this one won't write it.
    let mut v = s.state();
    v["min_writer"] = 99.into();
    std::fs::write(state.join("state.json"), v.to_string()).unwrap();
    let o = s.run(&["alias", "refused", "43124"]);
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("newer Porch"));
    assert_eq!(s.state()["min_writer"], 99);
    assert_eq!(s.state()["routes"].as_array().unwrap().len(), 1);
}

/// `porch mcp` over pipes, started the way an agent's client starts it: from
/// an environment without `CI` or `NO_COLOR`.
struct Mcp {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    stdout: std::io::BufReader<std::process::ChildStdout>,
    next: u64,
}
impl Mcp {
    fn start(s: &Sandbox, tools: &str) -> Self {
        let mut child = s
            .command(&["mcp", "--tools", tools])
            .env_remove("CI")
            .env_remove("NO_COLOR")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut mcp = Self {
            stdin: child.stdin.take().unwrap(),
            stdout: std::io::BufReader::new(child.stdout.take().unwrap()),
            child,
            next: 0,
        };
        mcp.request("initialize", serde_json::json!({}));
        mcp
    }
    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        use std::io::{BufRead, Write};
        self.next += 1;
        let id = self.next;
        writeln!(
            self.stdin,
            "{}",
            serde_json::json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
        )
        .unwrap();
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        let response: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["id"], id, "{response}");
        response["result"].clone()
    }
    /// A tool's result and its text.
    fn call(&mut self, name: &str, arguments: serde_json::Value) -> (bool, String) {
        let result = self.request(
            "tools/call",
            serde_json::json!({"name":name,"arguments":arguments}),
        );
        (
            result["isError"] == true,
            result["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        )
    }
}
impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
/// Stops an app when the test ends, passed or failed, so a failing assertion
/// never leaves it running.
struct StopOnDrop<'a>(&'a Sandbox, &'static str);
impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        let _ = self.0.run(&["stop", self.1]);
    }
}
const LISTEN_FOREVER: &str = "import os,socket,time\ns=socket.socket()\ns.bind(('127.0.0.1',int(os.environ['PORT'])))\ns.listen()\nwhile True: time.sleep(1)";

#[test]
fn mcp_run_leaves_the_apps_environment_alone() {
    let s = Sandbox::new();
    s.write_server();
    let mut mcp = Mcp::start(&s, "core,apps");
    let _app = StopOnDrop(&s, "envcheck");
    let root = s.root.display().to_string();
    let (error, text) = mcp.call(
        "run",
        serde_json::json!({"command":["python3","server.py"],"name":"envcheck","cwd":root}),
    );
    assert!(!error, "{text}");
    let env: serde_json::Value =
        serde_json::from_slice(&std::fs::read(s.root.join("child-env.json")).unwrap()).unwrap();
    // The MCP server keeps porch non-interactive without changing what the
    // app sees: no CI or NO_COLOR it didn't have in a terminal.
    for key in ["CI", "NO_COLOR", "PORCH_READ_ONLY"] {
        assert!(env.get(key).is_none(), "{key} leaked into the app");
    }
    let (error, text) = mcp.call("stop", serde_json::json!({"name":"envcheck"}));
    assert!(!error, "{text}");
}

#[test]
fn mcp_alias_cannot_stop_an_app_without_the_apps_profile() {
    let s = Sandbox::new();
    let _app = StopOnDrop(&s, "bg");
    s.ok(&[
        "run",
        "--detach",
        "--name",
        "bg",
        "python3",
        "-c",
        LISTEN_FOREVER,
    ]);
    let owner = s.state()["owners"]["bg"]["pid"].clone();
    assert!(owner.is_u64(), "{}", s.state());
    let mut core = Mcp::start(&s, "core");
    let (error, text) = core.call(
        "alias_add",
        serde_json::json!({"name":"bg","port":9,"force":true}),
    );
    assert!(error, "{text}");
    assert!(text.contains("belongs to an app"), "{text}");
    // The app and its route are untouched.
    assert_eq!(s.state()["owners"]["bg"]["pid"], owner);
    assert!(s.ok(&["list", "--json"]).contains("bg.localhost"));
    // Aliases made over MCP stay off paired machines unless asked.
    assert!(
        !core
            .call("alias_add", serde_json::json!({"name":"db","port":9}))
            .0
    );
    assert!(
        !core
            .call(
                "alias_add",
                serde_json::json!({"name":"shared","port":9,"private":false})
            )
            .0
    );
    let private = s.state()["private"].clone();
    assert_eq!(private, serde_json::json!(["db"]), "{}", s.state());
    // Replacing another alias with force still works.
    let (error, text) = core.call(
        "alias_add",
        serde_json::json!({"name":"db","port":10,"force":true}),
    );
    assert!(!error, "{text}");
    drop(core);
    // With the apps profile, where stop exists, force may take over the name.
    let mut apps = Mcp::start(&s, "core,apps");
    let (error, text) = apps.call(
        "alias_add",
        serde_json::json!({"name":"bg","port":9,"force":true}),
    );
    assert!(!error, "{text}");
    assert!(s.state()["owners"].get("bg").is_none(), "{}", s.state());
}

#[test]
fn mcp_run_that_times_out_stops_the_app_it_started() {
    let s = Sandbox::new();
    let mut mcp = Mcp::start(&s, "core,apps");
    let _app = StopOnDrop(&s, "slow");
    let root = s.root.display().to_string();
    // Never listens, so `porch run --detach` would wait its full 15 seconds.
    let (error, text) = mcp.call(
        "run",
        serde_json::json!({"command":["python3","-c","import time\ntime.sleep(60)"],"name":"slow","cwd":root,"timeoutMs":2000}),
    );
    assert!(error, "{text}");
    assert!(text.contains("slow was stopped"), "{text}");
    assert!(
        !s.state()["routes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["name"] == "slow"),
        "{}",
        s.state()
    );
    // A retry starts clean instead of reporting "already running".
    let (error, text) = mcp.call(
        "run",
        serde_json::json!({"command":["python3","-c",LISTEN_FOREVER],"name":"slow","cwd":root}),
    );
    assert!(!error, "{text}");
    assert!(!mcp.call("stop", serde_json::json!({"name":"slow"})).0);
}

/// The menu runs `porch open|copy [--peer=ID] -- APP`, so ids from a paired
/// machine that start with "-" reach the lookup instead of the option parser.
#[test]
fn open_and_copy_take_dash_ids_in_the_form_the_menu_uses() {
    let s = Sandbox::new();
    for verb in ["open", "copy"] {
        let o = s.run(&[verb, "--peer=-machine", "--", "-app"]);
        let err = String::from_utf8_lossy(&o.stderr);
        assert_eq!(o.status.code(), Some(1), "{verb}: {err}");
        assert!(err.contains("no paired machine named -machine"), "{err}");
        let o = s.run(&[verb, "--", "-app"]);
        let err = String::from_utf8_lossy(&o.stderr);
        assert_eq!(o.status.code(), Some(1), "{verb}: {err}");
        assert!(!err.contains("unexpected argument"), "{err}");
    }
}
