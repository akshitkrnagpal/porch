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
            .env("PORCH_PORT", self.port.to_string())
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
    assert_eq!(v["routes"][0]["target"], "http://127.0.0.1:43122");
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
    child.kill().unwrap();
    let status = child.wait().unwrap();
    #[cfg(unix)]
    assert_eq!(status.code(), Some(143));
    #[cfg(windows)]
    {
        let _ = status;
        s.ok(&["prune", "--force"]);
    }
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
    let listener = std::net::TcpListener::bind(("127.0.0.1", s.port)).unwrap();
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
    let mut server=s.command(&["secure","node","-e","require('http').createServer((q,r)=>r.end('from-secure')).listen(process.env.PORT,'127.0.0.1')"])
      .env("PORCH_HTTPS", "1").stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    s.wait_route("secure");
    let settings: serde_json::Value =
        serde_json::from_slice(&std::fs::read(s.root.join("state/proxy.json")).unwrap()).unwrap();
    assert_eq!(settings["tls"], true);
    let script=format!("const https=require('https');let n=0;function fetch(){{https.get({{hostname:'127.0.0.1',port:{},servername:'secure.localhost',headers:{{host:'secure.localhost'}}}},r=>{{let b='';r.on('data',c=>b+=c);r.on('end',()=>{{console.log(b);process.exit(b==='from-secure'?0:1)}})}}).on('error',e=>{{if(++n<50)setTimeout(fetch,30);else{{console.error(e);process.exit(1)}}}})}}fetch()",s.port);
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
fn proxy_recovers_after_accept_fd_exhaustion() {
    let script = r#"
import os,resource,socket,subprocess,sys,tempfile,time
with tempfile.TemporaryDirectory(prefix='porch-accept-') as root:
 with socket.socket() as s:
  s.bind(('127.0.0.1',0));port=s.getsockname()[1]
 env=os.environ.copy();env.update(PORCH_STATE_DIR=root,PORCH_SYNC_HOSTS='0',PORCH_SKIP_TRUST='1')
 def limit():resource.setrlimit(resource.RLIMIT_NOFILE,(64,64))
 with open(root+'/log','w+') as log:
  child=subprocess.Popen([sys.argv[1],'proxy','start','--foreground','--no-tls','--bind','127.0.0.1','--port',str(port)],env=env,stdout=log,stderr=log,preexec_fn=limit)
  sockets=[]
  try:
   deadline=time.monotonic()+10
   while not os.path.exists(root+'/proxy.ready'):
    assert child.poll() is None,'proxy exited before ready'
    assert time.monotonic()<deadline,'proxy never ready'
    time.sleep(.02)
   for i in range(110):
    c=socket.create_connection(('127.0.0.1',port),timeout=2)
    c.sendall(b'GET / HTTP/1.1\r\n');sockets.append(c)
   time.sleep(.3)
   for c in sockets:c.close()
   sockets=[]
   assert child.poll() is None,'accept error terminated daemon'
   deadline=time.monotonic()+5
   while True:
    try:
     with socket.create_connection(('127.0.0.1',port),timeout=1) as c:
      c.sendall(b'GET / HTTP/1.1\r\nHost: unknown.localhost\r\nConnection: close\r\n\r\n')
      c.settimeout(1);reply=c.recv(4096)
      if b'404' in reply:break
    except (OSError,TimeoutError):pass
    assert time.monotonic()<deadline,'listener did not recover after freeing descriptors'
   log.flush();log.seek(0);assert 'accept failed' in log.read(),'fixture failed to exhaust accept descriptors'
  finally:
   for c in sockets:c.close()
   if child.poll() is None:child.terminate()
   try:child.wait(timeout=5)
   except subprocess.TimeoutExpired:child.kill();child.wait()
"#;
    let out = Command::new("python3")
        .args(["-c", script, env!("CARGO_BIN_EXE_porch")])
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
