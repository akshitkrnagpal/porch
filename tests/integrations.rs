//! Fake executable acceptance tests: never mutate machine hosts, trust, services or tunnels.
#![cfg(unix)]
mod common;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
/// A real "Porch Local CA" certificate; its SHA-1 was computed with `openssl x509 -fingerprint -sha1`.
const PORCH_CA: &str = include_str!("fixtures/porch-ca.pem");
const PORCH_CA_SHA1: &str = "8B7699335C1F0EDA581E025A5F45400A8BEF9354";
fn write_ca(s: &Sandbox) {
    fs::write(s.dir.join("state/ca.pem"), PORCH_CA).unwrap();
}
static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
struct Sandbox {
    dir: PathBuf,
    path: String,
    port: u16,
}
impl Sandbox {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "porch-integrations-{}-{}",
            std::process::id(),
            // A per-process counter: the clock (microseconds on macOS) gives
            // parallel tests the same name, and with it shared state.
            {
                static NEXT: std::sync::atomic::AtomicUsize =
                    std::sync::atomic::AtomicUsize::new(0);
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            }
        ));
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::create_dir_all(dir.join("state")).unwrap();
        fs::create_dir_all(dir.join("home")).unwrap();
        fs::write(dir.join("hosts"), "127.0.0.1 localhost\n# retain me\n").unwrap();
        let path = format!(
            "{}:{}",
            dir.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let port = common::free_port();
        Self { dir, path, port }
    }
    fn fake(&self, name: &str, script: &str) {
        let p = self.dir.join("bin").join(name);
        common::write_executable(&p,&format!("#!/usr/bin/env python3\nimport os,sys,json,time\na=sys.argv[1:]\nwith open(os.environ['PORCH_TEST_LOG'],'a') as f:f.write(json.dumps([os.path.basename(sys.argv[0])]+a)+'\\n')\n{script}\n"));
    }
    fn cmd(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_porch"));
        c.env("PORCH_STATE_DIR", self.dir.join("state"))
            .env("PORCH_CLEAN_URLS", "0")
            .env("PORCH_HOSTS_FILE", self.dir.join("hosts"))
            .env("PORCH_SYNC_HOSTS", "0")
            .env("PORCH_PORT", self.port.to_string())
            .env("PORCH_APP_PORTS", common::app_ports(self.port))
            .env("PORCH_HTTPS", "0")
            .env("PORCH_SERVICE_FILE", self.dir.join("service"))
            .env("PORCH_TRUST_DIR", self.dir.join("trust"))
            .env("HOME", self.dir.join("home"))
            .env("PORCH_TEST_ROOT", &self.dir)
            .env("PORCH_TEST_LOG", self.dir.join("log"))
            .env("PATH", &self.path);
        c
    }
    fn run(&self, args: &[&str]) -> Output {
        self.cmd().args(args).output().unwrap()
    }
    fn ok(&self, args: &[&str]) -> Output {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "{args:?}: {}\nproxy log: {}",
            String::from_utf8_lossy(&out.stderr),
            fs::read_to_string(self.dir.join("state/proxy.log")).unwrap_or_default()
        );
        out
    }
    fn log(&self) -> String {
        fs::read_to_string(self.dir.join("log")).unwrap_or_default()
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = self.cmd().args(["proxy", "stop"]).output();
        let _ = fs::remove_dir_all(&self.dir);
    }
}
/// Wait until `pid` has exited. A killed process Porch did not reap stays a
/// zombie until its new parent reaps it, which a container's init may never
/// do; a zombie has exited, so it counts as gone.
fn wait_gone(pid: i32, what: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while porch::state::alive(pid as u32) {
        assert!(std::time::Instant::now() < deadline, "{what}: pid {pid}");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
fn state(root: &Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(root.join("state/state.json")).unwrap()).unwrap()
}
#[test]
fn compose_rewrites_ports_and_reconciles_only_its_project() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let s = Sandbox::new();
    s.fake("docker",r#"
if 'config' in a:
 if os.environ.get('PORCH_COMPOSE_STDIN_TEST'):
  assert open(a[a.index('-f')+1]).read()=='services: {}\n'
  assert '--project-directory' in a
 print(json.dumps({'name':'shop','services':{'web':{'ports':[{'target':3000,'published':'3000'},{'target':8080,'published':'8080'}]},'db':{'ports':[{'target':5432,'published':'5432'}]}}}))
elif 'ps' in a:
 print(open(os.environ['PORCH_TEST_ROOT']+'/containers').read())
elif 'up' in a:
 model=json.load(open(a[a.index('-f')+1]));assert model['services']['db']['ports'][0]['published']=='5432'
 assert model['services']['web']['ports'][0]['host_ip']=='127.0.0.1'
 rows=[{'Project':'shop','Service':'web','State':'running','Publishers':[{'TargetPort':p['target'],'PublishedPort':int(p['published']),'Protocol':'tcp'} for p in model['services']['web']['ports']]}]
 open(os.environ['PORCH_TEST_ROOT']+'/containers','w').write(json.dumps(rows))
elif 'down' in a:
 open(os.environ['PORCH_TEST_ROOT']+'/containers','w').write('[]')
else:raise Exception(a)
"#);
    fs::write(s.dir.join("containers"), "[]").unwrap();
    s.ok(&["alias", "independent", "3456"]);
    s.ok(&[
        "compose",
        "-f",
        "compose.test.yml",
        "--profile=development",
        "-p",
        "shop",
        "up",
        "-d",
        "web",
    ]);
    let v = state(&s.dir);
    let names = v["routes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(names.contains(&"independent"));
    assert!(names.contains(&"web.shop"));
    assert!(names.contains(&"3000.web.shop"));
    assert!(names.contains(&"8080.web.shop"));
    assert!(!names.iter().any(|n| n.contains("db")));
    let first = fs::read(s.dir.join("state/compose-ports.json")).unwrap();
    s.ok(&["compose", "-p", "shop", "up", "-d"]);
    assert_eq!(
        fs::read(s.dir.join("state/compose-ports.json")).unwrap(),
        first
    );
    let mut stdin = s
        .cmd()
        .env("PORCH_COMPOSE_STDIN_TEST", "1")
        .args(["compose", "-f", "-", "-p", "shop", "up", "-d"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        stdin
            .stdin
            .take()
            .unwrap()
            .write_all(b"services: {}\n")
            .unwrap();
    }
    let output = stdin.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "stdin compose: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!fs::read_dir(s.dir.join("state"))
        .unwrap()
        .filter_map(Result::ok)
        .any(|e| e
            .file_name()
            .to_string_lossy()
            .starts_with("compose-stdin-")));
    s.ok(&["compose", "-p", "shop", "down"]);
    let v = state(&s.dir);
    assert_eq!(v["routes"].as_array().unwrap().len(), 1);
    assert_eq!(v["routes"][0]["name"], "independent");
    assert!(s.log().contains("--profile=development"));
    s.ok(&["alias", "web.shop.localhost", "3457"]);
    let count = s
        .log()
        .lines()
        .filter(|line| line.contains("\"up\""))
        .count();
    assert!(!s
        .run(&["compose", "-p", "shop", "up", "-d"])
        .status
        .success());
    assert_eq!(
        s.log()
            .lines()
            .filter(|line| line.contains("\"up\""))
            .count(),
        count,
        "Docker up ran despite canonical-host collision"
    );
    assert!(state(&s.dir)["routes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["name"] == "web.shop.localhost"));
}
#[test]
fn service_templates_persist_every_setting_for_each_platform() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for platform in ["macos", "linux", "windows"] {
        let s = Sandbox::new();
        for tool in ["launchctl", "systemctl", "schtasks"] {
            s.fake(tool, "");
        }
        let out = s
            .cmd()
            .env("PORCH_SERVICE_PLATFORM", platform)
            .args([
                "service",
                "install",
                "--port",
                "8443",
                "--https",
                "--tld",
                "localhost",
                "--tld",
                "test",
                "--lan",
                "--ip",
                "192.168.1.50",
                "--wildcard",
                "--cert",
                "certificate & file.pem",
                "--key",
                "private key.pem",
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let content = fs::read_to_string(s.dir.join("service")).unwrap();
        assert!(content.contains("8443"));
        assert!(content.contains("--wildcard"));
        assert!(content.contains("192.168.1.50"));
        assert!(content.contains("--cert"));
        if platform == "macos" {
            assert!(content.contains("certificate &amp; file.pem"));
        }
        if platform == "windows" {
            assert!(content.contains("S-1-5-18"));
            assert!(content.contains("--state-dir"));
        }
        let status = s
            .cmd()
            .env("PORCH_SERVICE_PLATFORM", platform)
            .args(["service", "status"])
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&status.stdout).contains("wildcard: true"));
        let uninstall = s
            .cmd()
            .env("PORCH_SERVICE_PLATFORM", platform)
            .args(["service", "uninstall"])
            .output()
            .unwrap();
        assert!(uninstall.status.success());
        assert!(!s.dir.join("service").exists());
    }
}
#[test]
fn hosts_manage_only_own_block_and_reject_symlinks() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let s = Sandbox::new();
    s.ok(&["alias", "api.demo", "3456"]);
    fs::write(
        s.dir.join("state/proxy.json"),
        r#"{"port":8443,"tls":true,"tlds":["localhost","dev.test"]}"#,
    )
    .unwrap();
    s.ok(&["hosts", "sync"]);
    let content = fs::read_to_string(s.dir.join("hosts")).unwrap();
    assert!(content.contains("127.0.0.1 api.demo.localhost\n::1 api.demo.localhost"));
    assert!(content.contains("api.demo.dev.test"));
    assert!(content.contains("# retain me"));
    s.ok(&["hosts", "clean"]);
    assert_eq!(
        fs::read_to_string(s.dir.join("hosts")).unwrap(),
        "127.0.0.1 localhost\n# retain me\n"
    );
    fs::remove_file(s.dir.join("hosts")).unwrap();
    fs::write(s.dir.join("unrelated"), "unrelated").unwrap();
    std::os::unix::fs::symlink(s.dir.join("unrelated"), s.dir.join("hosts")).unwrap();
    assert!(!s.run(&["hosts", "sync"]).status.success());
    assert_eq!(
        fs::read_to_string(s.dir.join("unrelated")).unwrap(),
        "unrelated"
    );
}
#[test]
fn tunnel_readiness_environment_and_owned_cleanup() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let s = Sandbox::new();
    s.fake("tailscale",r#"
if a==['version']: print('1.2')
elif a==['status','--json']: print(json.dumps({'Self':{'DNSName':'box.team.ts.net.','Capabilities':['https','funnel']}}))
elif a==['serve','status','--json']: print(json.dumps({'TCP':{'443':{}}}))
"#);
    s.fake(
        "ngrok",
        r#"
if a==['version']: print('3.0')
else:
 open(os.environ['PORCH_TEST_ROOT']+'/ngrok-pid','w').write(str(os.getpid()))
 print('started tunnel url=https://demo.ngrok.app',flush=True)
 time.sleep(60)
"#,
    );
    s.fake(
        "cloudflared",
        r#"
if a==['--version']:print('1.0')
else:
 open(os.environ['PORCH_TEST_ROOT']+'/cloudflare-pid','w').write(str(os.getpid()))
 print('https://demo.trycloudflare.com',flush=True)
 time.sleep(0.1)
 print('Registered tunnel connection',flush=True)
 time.sleep(60)
"#,
    );
    let script="import os,time,json;root=os.environ['PORCH_TEST_ROOT'];open(root+'/app-env.tmp','w').write(json.dumps(dict(os.environ)));os.replace(root+'/app-env.tmp',root+'/app-env');time.sleep(60)";
    let mut child = s
        .cmd()
        .args([
            "shared",
            "--tailscale",
            "--ngrok",
            "--cloudflare",
            "python3",
            "-c",
            script,
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(12);
    while !s.dir.join("app-env").exists() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "sharing CLI exited before app launch"
        );
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("sharing CLI readiness timed out");
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let env: serde_json::Value =
        serde_json::from_slice(&fs::read(s.dir.join("app-env")).unwrap()).unwrap();
    assert_eq!(
        env["PORTLESS_TAILSCALE_URL"],
        "https://box.team.ts.net:8443"
    );
    assert_eq!(env["PORTLESS_NGROK_URL"], "https://demo.ngrok.app");
    for provider in ["TAILSCALE", "NGROK", "CLOUDFLARE"] {
        assert_eq!(
            env[format!("PORCH_{provider}_URL")],
            env[format!("PORTLESS_{provider}_URL")]
        );
    }
    assert_eq!(
        env["PORTLESS_CLOUDFLARE_URL"],
        "https://demo.trycloudflare.com"
    );
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(143));
    // Porch removes the Tailscale registration before it exits.
    assert!(s
        .log()
        .contains("\"serve\", \"--yes\", \"--https=8443\", \"off\""));
    for name in ["ngrok-pid", "cloudflare-pid"] {
        let pid: i32 = fs::read_to_string(s.dir.join(name))
            .unwrap()
            .parse()
            .unwrap();
        wait_gone(pid, "owned tunnel survived Drop");
    }
}

#[test]
fn trust_install_and_cleanup_use_only_recorded_certificate() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for (platform, system) in [
        ("macos", true),
        ("linux", true),
        ("windows", false),
        ("macos", false),
        ("linux", false),
    ] {
        let s = Sandbox::new();
        for tool in ["security", "update-ca-certificates"] {
            s.fake(tool, "");
        }
        // Listing a database shows the nickname Porch installed.
        s.fake(
            "certutil",
            &format!(
                "if a==['-L','-d',a[2]]:print('Porch Local CA {}   C,,')",
                &PORCH_CA_SHA1[..12]
            ),
        );
        // A Firefox profile alongside the shared Chrome NSS database.
        let profile = s.dir.join("home/.mozilla/firefox/abc.default");
        fs::create_dir_all(&profile).unwrap();
        fs::write(profile.join("cert9.db"), "").unwrap();
        write_ca(&s);
        fs::write(s.dir.join("state/ca-key.pem"), "fixture key").unwrap();
        let mut args = vec!["trust"];
        if system {
            args.push("--system");
        }
        let out = s
            .cmd()
            .env("PORCH_TRUST_PLATFORM", platform)
            .args(&args)
            .output()
            .unwrap();
        let label = format!("{platform} system={system}");
        assert!(
            out.status.success(),
            "{label}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let record: serde_json::Value =
            serde_json::from_slice(&fs::read(s.dir.join("state/trust.json")).unwrap()).unwrap();
        assert_eq!(record["platform"], platform);
        assert_eq!(
            record["scope"],
            if system { "system" } else { "user" },
            "{label}"
        );
        let log = s.log();
        match (platform, system) {
            ("linux", true) => assert!(s.dir.join("trust/porch-8B7699335C1F.crt").exists()),
            ("macos", true) => assert!(log.contains("/Library/Keychains/System.keychain")),
            ("macos", false) => {
                let keychain = s.dir.join("home/Library/Keychains/login.keychain-db");
                assert!(log.contains(&*keychain.to_string_lossy()), "{log}");
                assert!(!log.contains("System.keychain"), "{log}");
            }
            ("linux", false) => {
                // No system CA tooling: the shared NSS db is created, and both dbs get the CA.
                assert!(!log.contains("update-ca-certificates"), "{log}");
                assert!(s.dir.join("home/.pki/nssdb").is_dir());
                assert!(log.contains("--empty-password"), "{log}");
                assert_eq!(log.matches("\"-A\"").count(), 2, "{log}");
                assert!(log.contains(&*profile.to_string_lossy()), "{log}");
            }
            _ => {}
        }
        let out = s
            .cmd()
            .env("PORCH_TRUST_PLATFORM", platform)
            .args(["clean"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{label}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!s.dir.join("state/trust.json").exists());
        let log = s.log();
        assert!(
            log.contains(match (platform, system) {
                ("macos", _) => "delete-certificate",
                ("linux", true) => "update-ca-certificates",
                ("linux", false) => "\"-D\"",
                _ => "-delstore",
            }),
            "{label}: {log}"
        );
    }
}

#[test]
fn user_trust_never_requests_sudo() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let s = Sandbox::new();
    s.fake("security", "");
    s.fake("sudo", "sys.exit(97)");
    write_ca(&s);
    // Without PORCH_TRUST_DIR the system scope would elevate; per-user trust must not.
    let out = s
        .cmd()
        .env_remove("PORCH_TRUST_DIR")
        .env("PORCH_TRUST_PLATFORM", "macos")
        .args(["trust"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!s.log().contains("[\"sudo\""), "{}", s.log());
}

#[test]
fn tunnel_failure_never_leaves_live_app_or_route() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for provider in ["ngrok", "cloudflare"] {
        for mode in ["early", "late"] {
            let s = Sandbox::new();
            s.fake(
                if provider == "ngrok" {
                    "ngrok"
                } else {
                    "cloudflared"
                },
                r#"
if a in [['version'],['--version']]:print('1.0')
else:
 if os.environ['PORCH_TUNNEL_TEST_MODE']=='early':
  print('authentication failed',file=sys.stderr,flush=True)
  sys.exit(2)
 print('started tunnel url=https://demo.ngrok.app',flush=True)
 print('https://demo.trycloudflare.com Registered tunnel connection',flush=True)
 # Fail only once the app runs, however long Porch takes to start it.
 app=os.environ['PORCH_TEST_ROOT']+'/app-pid'
 deadline=time.monotonic()+30
 while not os.path.exists(app) and time.monotonic()<deadline:time.sleep(0.02)
 sys.exit(9)
"#,
            );
            // The environment is complete before the pid appears (both renamed
            // into place): the fake tunnel fails as soon as it sees the pid.
            let script="import os,time,json;root=os.environ['PORCH_TEST_ROOT'];open(root+'/app-env.tmp','w').write(json.dumps(dict(os.environ)));os.replace(root+'/app-env.tmp',root+'/app-env');open(root+'/app-pid.tmp','w').write(str(os.getpid()));os.replace(root+'/app-pid.tmp',root+'/app-pid');time.sleep(60)";
            let flag = format!("--{provider}");
            let mut child = s
                .cmd()
                .env("PORCH_TUNNEL_TEST_MODE", mode)
                .args(["failure-app", &flag, "python3", "-c", script])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                if std::time::Instant::now() > deadline {
                    let _ = child.kill();
                    panic!("app outlived failed {provider} {mode} tunnel");
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            };
            assert!(!status.success());
            assert!(state(&s.dir)["routes"].as_array().unwrap().is_empty());
            if mode == "early" {
                assert!(!s.dir.join("app-pid").exists());
            } else {
                let pid: i32 = fs::read_to_string(s.dir.join("app-pid"))
                    .unwrap()
                    .parse()
                    .unwrap();
                wait_gone(pid, "app of a failed tunnel");
                let env: serde_json::Value =
                    serde_json::from_slice(&fs::read(s.dir.join("app-env")).unwrap()).unwrap();
                assert!(env[format!("PORTLESS_{}_URL", provider.to_uppercase())]
                    .as_str()
                    .is_some_and(|s| s.starts_with("https://")));
            }
        }
    }
}

#[test]
fn failed_trust_removal_retains_ca_for_retry() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let s = Sandbox::new();
    s.fake("security", "\nif 'delete-certificate' in a:sys.exit(2)");
    write_ca(&s);
    fs::write(s.dir.join("state/ca-key.pem"), "fixture key").unwrap();
    let out = s
        .cmd()
        .env("PORCH_TRUST_PLATFORM", "macos")
        .args(["trust"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(!s.run(&["clean"]).status.success());
    assert!(s.dir.join("state/ca.pem").exists());
    assert!(s.dir.join("state/ca-key.pem").exists());
    assert!(s.dir.join("state/trust.json").exists());
}

#[test]
fn tailscale_allocation_is_serialized_across_state_directories() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let one = Sandbox::new();
    let two = Sandbox::new();
    let registry = one.dir.join("tailnet.json");
    fs::write(&registry, "{}").unwrap();
    let fake = r#"
p=os.environ['PORCH_TAILSCALE_REGISTRY']
if a==['version']:print('1.0')
elif a==['status','--json']:print(json.dumps({'Self':{'DNSName':'box.team.ts.net.','Capabilities':['https']}}))
elif a==['serve','status','--json']:
 r=json.load(open(p));print(json.dumps({'Web':{'box.team.ts.net:'+port:{'Handlers':{'/':{'Proxy':target}}} for port,target in r.items()}}))
elif a[0]=='serve':
 port=next(arg.split('=')[1] for arg in a if arg.startswith('--https='));r=json.load(open(p))
 if a[-1]=='off':r.pop(port,None)
 else:
  assert port not in r,'duplicate Tailscale port'
  time.sleep(0.05)
  r[port]=a[-1]
 open(p,'w').write(json.dumps(r))
"#;
    for s in [&one, &two] {
        s.fake("tailscale", fake);
    }
    let script="import os,time,json;root=os.environ['PORCH_TEST_ROOT'];open(root+'/env.tmp','w').write(json.dumps(dict(os.environ)));os.replace(root+'/env.tmp',root+'/env');time.sleep(60)";
    let mut children = Vec::new();
    for (s, name) in [(&one, "one"), (&two, "two")] {
        children.push(
            s.cmd()
                .env("PORCH_TAILSCALE_REGISTRY", &registry)
                .args([name, "--tailscale", "python3", "-c", script])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while !one.dir.join("env").exists() || !two.dir.join("env").exists() {
        for child in &mut children {
            assert!(
                child.try_wait().unwrap().is_none(),
                "concurrent sharing launcher exited"
            );
        }
        if std::time::Instant::now() > deadline {
            for c in &mut children {
                let _ = c.kill();
            }
            panic!("concurrent sharing readiness timeout");
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let mut urls = Vec::new();
    for s in [&one, &two] {
        let env: serde_json::Value =
            serde_json::from_slice(&fs::read(s.dir.join("env")).unwrap()).unwrap();
        urls.push(env["PORTLESS_TAILSCALE_URL"].as_str().unwrap().to_owned());
    }
    urls.sort();
    assert_eq!(
        urls,
        ["https://box.team.ts.net", "https://box.team.ts.net:8443"]
    );
    // Stop sequentially to keep the deliberately simple fake registry writes atomic.
    for mut child in children {
        unsafe {
            libc::kill(child.id() as i32, libc::SIGTERM);
        }
        assert_eq!(child.wait().unwrap().code(), Some(143));
    }
    assert_eq!(fs::read_to_string(registry).unwrap(), "{}");
}

#[test]
fn compose_passthrough_preserves_stdin_output_and_exit_status() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let s = Sandbox::new();
    s.fake("docker","print('received:'+sys.stdin.read(),flush=True)\nprint('docker diagnostic',file=sys.stderr,flush=True)\nsys.exit(7)");
    let mut child = s
        .cmd()
        .args(["compose", "--profile", "dev", "exec", "app", "cat"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"hello from terminal")
            .unwrap();
    }
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(7));
    assert!(String::from_utf8_lossy(&out.stdout).contains("received:hello from terminal"));
    assert!(String::from_utf8_lossy(&out.stderr).contains("docker diagnostic"));
    assert!(s
        .log()
        .contains("\"--profile\", \"dev\", \"exec\", \"app\", \"cat\""));
}

#[test]
fn stalled_external_helper_has_bounded_lifetime() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let s = Sandbox::new();
    s.fake("security", "time.sleep(60)");
    write_ca(&s);
    let start = std::time::Instant::now();
    let out = s
        .cmd()
        .env("PORCH_TRUST_PLATFORM", "macos")
        .env("PORCH_COMMAND_TIMEOUT_MS", "500")
        .args(["trust"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    // The helper would run for a minute; returning well before proves the bound.
    assert!(start.elapsed() < std::time::Duration::from_secs(30));
    assert!(String::from_utf8_lossy(&out.stderr).contains("timed out"));
    // The helper may be killed before its interpreter runs a line, so find it
    // by its command line rather than by a file it would write.
    let helper = s.dir.join("bin/security");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let running = Command::new("pgrep")
            .args(["-f", &helper.to_string_lossy()])
            .output()
            .unwrap();
        // pgrep exits 1 for "no match"; anything else but 0 is a failed check.
        match running.status.code() {
            Some(1) => break,
            Some(0) => {}
            other => panic!(
                "pgrep failed ({other:?}): {}",
                String::from_utf8_lossy(&running.stderr)
            ),
        }
        // pgrep lists zombies too; an exited helper waiting for its reaper is gone.
        let pids: Vec<u32> = String::from_utf8_lossy(&running.stdout)
            .split_whitespace()
            .filter_map(|p| p.parse().ok())
            .collect();
        if pids.iter().all(|pid| !porch::state::alive(*pid)) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "stalled helper survived its timeout: {}",
            String::from_utf8_lossy(&running.stdout)
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[test]
fn hosts_permissions_are_actionable_and_localhost_hints_once() {
    use std::os::unix::fs::PermissionsExt;
    let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let s = Sandbox::new();
    fs::set_permissions(s.dir.join("hosts"), fs::Permissions::from_mode(0o444)).unwrap();
    assert!(s
        .cmd()
        .env("PORCH_SYNC_HOSTS", "1")
        .args(["proxy", "start", "--no-tls", "--tld", "test"])
        .output()
        .unwrap()
        .status
        .success());
    let out = s
        .cmd()
        .env("PORCH_SYNC_HOSTS", "1")
        .args(["alias", "app", "45678"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("porch hosts sync"),
        "permission guidance missing: {stderr}"
    );
    assert!(
        !stderr.contains("403"),
        "permission is not an authentication failure: {stderr}"
    );
    let token = fs::read_to_string(s.dir.join("state/proxy.hosts-sync-token")).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let r = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(format!("http://127.0.0.1:{}/_porch/hosts-sync", s.port))
            .header("x-porch-hosts-sync-token", token.trim())
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 409);
        assert!(r.text().await.unwrap().contains("porch hosts sync"));
    });
    s.ok(&["proxy", "stop"]);
    assert!(s
        .cmd()
        .env("PORCH_SYNC_HOSTS", "1")
        .args(["proxy", "start", "--no-tls", "--tld", "localhost"])
        .output()
        .unwrap()
        .status
        .success());
    for (name, first) in [("one", true), ("two", false)] {
        let out = s
            .cmd()
            .env("PORCH_SYNC_HOSTS", "1")
            .args(["alias", name, "45678"])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !stderr.contains("Hosts sync:"),
            "localhost route should use browser resolution: {stderr}"
        );
        assert_eq!(
            stderr.contains("Browsers resolve .localhost"),
            first,
            "{stderr}"
        );
    }
}

#[test]
fn doctor_does_not_call_a_foreign_listener_ready() {
    use std::io::{Read, Write};
    let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let s = Sandbox::new();
    fs::write(
        s.dir.join("state/proxy.json"),
        serde_json::to_vec(&porch::state::ProxySettings {
            port: s.port,
            tls: false,
            ..Default::default()
        })
        .unwrap(),
    )
    .unwrap();
    for name in ["node", "docker", "tailscale", "ngrok", "cloudflared"] {
        s.fake(name, "print('fixture version')");
    }
    // A foreign HTTP server that answers until doctor is done. It reads each
    // request's headers before replying: a reply followed by closing with the
    // request unread resets the connection, which doctor reports as
    // unavailable rather than foreign.
    let listener = std::net::TcpListener::bind(("127.0.0.1", s.port)).unwrap();
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop = done.clone();
    let server = std::thread::spawn(move || {
        for c in listener.incoming() {
            if stop.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            let Ok(mut c) = c else { continue };
            let mut request = Vec::new();
            let mut buf = [0; 4096];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                match c.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => request.extend_from_slice(&buf[..n]),
                }
            }
            let _ =
                c.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        }
    });
    let out = s.run(&["doctor"]);
    // A foreign program on the proxy port is a problem: doctor fails.
    assert_eq!(out.status.code(), Some(1));
    done.store(true, std::sync::atomic::Ordering::SeqCst);
    // Wake the accept loop so it sees the flag.
    let _ = std::net::TcpStream::connect(("127.0.0.1", s.port));
    server.join().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("foreign/unverified listener"), "{stdout}");
    assert!(!stdout.contains("proxy listener: reachable"), "{stdout}");
}

#[test]
fn trust_records_from_before_scopes_stay_system_wide() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let s = Sandbox::new();
    for tool in ["update-ca-certificates", "certutil"] {
        s.fake(tool, "");
    }
    write_ca(&s);
    fs::create_dir_all(s.dir.join("trust")).unwrap();
    let installed = s.dir.join("trust/porch-8B7699335C1F.crt");
    fs::write(&installed, PORCH_CA).unwrap();
    // A record written by Porch 0.3.x: no scope, keychain or NSS fields.
    fs::write(
        s.dir.join("state/trust.json"),
        serde_json::to_vec(&serde_json::json!({
            "platform": "linux",
            "fingerprint": PORCH_CA_SHA1,
            "installed": installed,
            "update": "update-ca-certificates",
            "windows": false,
        }))
        .unwrap(),
    )
    .unwrap();
    // Re-running trust keeps the existing system-wide install instead of adding per-user trust.
    let out = s
        .cmd()
        .env("PORCH_TRUST_PLATFORM", "linux")
        .args(["trust"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("already trusted system-wide"));
    assert!(!s.log().contains("certutil"), "{}", s.log());
    // Clean removes it through the system CA tooling.
    let out = s.run(&["clean"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!installed.exists());
    assert!(s.log().contains("update-ca-certificates"));
    assert!(!s.dir.join("state/trust.json").exists());
}

/// Seed a per-user Linux trust record covering the shared NSS database.
fn linux_user_trust(s: &Sandbox) -> PathBuf {
    write_ca(s);
    let db = s.dir.join("home/.pki/nssdb");
    fs::create_dir_all(&db).unwrap();
    fs::write(
        s.dir.join("state/trust.json"),
        serde_json::to_vec(&serde_json::json!({
            "platform": "linux",
            "fingerprint": PORCH_CA_SHA1,
            "installed": null,
            "update": null,
            "windows": false,
            "scope": "user",
            "nss_dbs": [db],
        }))
        .unwrap(),
    )
    .unwrap();
    db
}

#[test]
fn linux_user_trust_cleanup_removes_only_when_certutil_is_certain() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let nickname = format!("Porch Local CA {}", &PORCH_CA_SHA1[..12]);
    // A real-format `certutil -L -d` listing, with or without the Porch nickname.
    let list = |with: bool| {
        format!(
            "\nif a[0]=='-L':\n print('\\nCertificate Nickname{pad}Trust Attributes\\n{pad}SSL,S/MIME,JAR/XPI\\n'{row})",
            pad = " ".repeat(40),
            row = if with {
                format!("+'\\n{nickname}{}C,,'", " ".repeat(30))
            } else {
                String::new()
            }
        )
    };
    // (certutil script, clean succeeds, deletion attempted, stderr fragment)
    let cases = [
        (list(true), true, true, ""),
        (list(false), true, false, ""),
        (
            // A lookup failure reported with "not found" wording must not count as absent.
            "\nprint('certutil: Could not find cert: x\\n: PR_FILE_NOT_FOUND_ERROR: File not found',file=sys.stderr);sys.exit(255)".into(),
            false,
            false,
            "trust record kept for retry",
        ),
        (
            "\nprint('certutil: function failed: SEC_ERROR_BAD_DATABASE: security library: bad database.',file=sys.stderr);sys.exit(255)".into(),
            false,
            false,
            "bad database",
        ),
        (
            format!("{}\nif a[0]=='-D':\n print('certutil: could not delete',file=sys.stderr);sys.exit(255)", list(true)),
            false,
            true,
            "could not remove Porch CA",
        ),
    ];
    for (script, ok, deleted, stderr) in cases {
        let s = Sandbox::new();
        s.fake("certutil", &script);
        linux_user_trust(&s);
        let out = s
            .cmd()
            .env("PORCH_TRUST_PLATFORM", "linux")
            .args(["clean"])
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        let log = s.log();
        assert_eq!(out.status.success(), ok, "{script}: {err}");
        assert!(log.contains("\"-L\""), "{log}");
        assert_eq!(log.contains("\"-D\""), deleted, "{script}: {log}");
        if deleted {
            assert!(log.contains(&nickname), "{log}");
        }
        assert!(err.contains(stderr), "{err}");
        // The trust record survives every failure so cleanup can be retried.
        assert_eq!(!s.dir.join("state/trust.json").exists(), ok, "{script}");
        assert_eq!(!s.dir.join("state/ca.pem").exists(), ok, "{script}");
    }
}

#[test]
fn linux_user_trust_cleanup_without_certutil_keeps_record() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let s = Sandbox::new();
    linux_user_trust(&s);
    // Only the sandbox's empty bin directory: certutil cannot be found.
    let out = s
        .cmd()
        .env("PATH", s.dir.join("bin"))
        .env("PORCH_TRUST_PLATFORM", "linux")
        .args(["clean"])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{err}");
    assert!(err.contains("could not start certutil"), "{err}");
    assert!(s.dir.join("state/trust.json").exists());
    assert!(s.dir.join("state/ca.pem").exists());
}

#[test]
fn trust_needs_no_openssl() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let s = Sandbox::new();
    s.fake("openssl", "sys.exit(99)");
    s.fake("certutil", "");
    write_ca(&s);
    let out = s
        .cmd()
        .env("PORCH_TRUST_PLATFORM", "windows")
        .args(["trust"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let record: serde_json::Value =
        serde_json::from_slice(&fs::read(s.dir.join("state/trust.json")).unwrap()).unwrap();
    assert_eq!(record["fingerprint"], PORCH_CA_SHA1);
    assert!(!s.log().contains("openssl"), "{}", s.log());
    // A certificate that is not the Porch CA is refused before any trust store change.
    fs::write(
        s.dir.join("state/ca.pem"),
        include_str!("fixtures/other-ca.pem"),
    )
    .unwrap();
    fs::remove_file(s.dir.join("state/trust.json")).unwrap();
    let out = s
        .cmd()
        .env("PORCH_TRUST_PLATFORM", "windows")
        .args(["trust"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not issued as Porch Local CA"));
}

#[test]
fn linux_user_trust_cleanup_keeps_record_for_inaccessible_db() {
    use std::os::unix::fs::PermissionsExt;
    let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if unsafe { libc::geteuid() } == 0 {
        return; // root ignores directory permissions
    }
    let s = Sandbox::new();
    s.fake("certutil", "");
    linux_user_trust(&s);
    let pki = s.dir.join("home/.pki");
    fs::set_permissions(&pki, fs::Permissions::from_mode(0o000)).unwrap();
    let out = s
        .cmd()
        .env("PORCH_TRUST_PLATFORM", "linux")
        .args(["clean"])
        .output()
        .unwrap();
    fs::set_permissions(&pki, fs::Permissions::from_mode(0o755)).unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{err}");
    assert!(err.contains("could not access NSS database"), "{err}");
    assert!(s.dir.join("state/trust.json").exists());
}

#[test]
fn a_rotated_ca_loses_its_trust_when_the_new_one_is_trusted() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let s = Sandbox::new();
    s.fake("security", "");
    write_ca(&s);
    let trust = |s: &Sandbox| {
        let out = s
            .cmd()
            .env_remove("PORCH_TRUST_DIR")
            .env("PORCH_TRUST_PLATFORM", "macos")
            .args(["trust"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    trust(&s);
    let record: serde_json::Value =
        serde_json::from_slice(&fs::read(s.dir.join("state/trust.json")).unwrap()).unwrap();
    let old = record["fingerprint"].as_str().unwrap().to_owned();
    // The domains changed: the old CA is retired beside a new one.
    fs::rename(
        s.dir.join("state/ca.pem"),
        s.dir.join("state/ca.pem.retired-1"),
    )
    .unwrap();
    let made = Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-days",
            "1",
            "-subj",
            "/CN=Porch Local CA",
            "-keyout",
        ])
        .arg(s.dir.join("state/ca-key.pem"))
        .arg("-out")
        .arg(s.dir.join("state/ca.pem"))
        .output();
    if !made.is_ok_and(|o| o.status.success()) {
        eprintln!("skipping: openssl unavailable");
        return;
    }
    trust(&s);
    let log = s.log();
    assert!(
        log.contains("delete-certificate") && log.contains(&old),
        "the retired CA's trust must be removed: {log}"
    );
    let record: serde_json::Value =
        serde_json::from_slice(&fs::read(s.dir.join("state/trust.json")).unwrap()).unwrap();
    assert_ne!(record["fingerprint"].as_str().unwrap(), old);
}
