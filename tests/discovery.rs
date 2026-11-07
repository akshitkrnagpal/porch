//! Two isolated Porch installations on loopback, using the actual CLI and TLS agent.
//! This proves machine targeting; it does not claim physical LAN/tailnet reachability.
mod common;
use porch::discovery::types::{Snapshot, PROTOCOL_VERSION};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::io::AsyncWriteExt;

struct Nodes(Vec<PathBuf>);
impl Drop for Nodes {
    fn drop(&mut self) {
        for dir in &self.0 {
            for args in [
                vec!["discovery", "disconnect"],
                vec!["discovery", "disable"],
                vec!["proxy", "stop"],
            ] {
                let _ = std::process::Command::new(env!("CARGO_BIN_EXE_porch"))
                    .args(args)
                    .env("PORCH_STATE_DIR", dir)
                    .env("PORCH_CLEAN_URLS", "0")
                    .env("PORCH_SYNC_HOSTS", "0")
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}
async fn command(dir: &Path, args: &[&str], input: Option<&str>) -> std::process::Output {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_porch"));
    command
        .args(args)
        .env("PORCH_STATE_DIR", dir)
        .env("PORCH_CLEAN_URLS", "0")
        .env("PORCH_SYNC_HOSTS", "0")
        .env("HOSTNAME", dir.file_name().unwrap())
        .env("DISPLAY", "")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    if let Some(input) = input {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .await
            .unwrap();
    }
    tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
        .await
        .expect("CLI bounded completion")
        .unwrap()
}
async fn ok(dir: &Path, args: &[&str]) -> String {
    let output = command(dir, args, None).await;
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
use common::free_port;
async fn upstream(label: &'static str) -> (u16, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    use axum::extract::ws::{Message, WebSocketUpgrade};
    let app = axum::Router::new()
        .route(
            "/ws",
            axum::routing::get(move |ws: WebSocketUpgrade| async move {
                ws.on_upgrade(move |mut socket| async move {
                    while let Some(Ok(Message::Text(text))) = socket.recv().await {
                        let reply = format!("{label}:{text}");
                        if socket.send(Message::Text(reply.into())).await.is_err() {
                            break;
                        }
                    }
                })
            }),
        )
        .fallback(
            move |uri: axum::http::Uri, headers: axum::http::HeaderMap| async move {
                format!(
                    "{label}:{}:{}",
                    uri,
                    headers.get("host").unwrap().to_str().unwrap()
                )
            },
        );
    (
        port,
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }),
    )
}
#[tokio::test]
async fn two_nodes_pair_catalog_open_revoke_and_keep_proxy_separate() {
    let root = std::env::temp_dir().join(format!("porch-discovery-{}", std::process::id()));
    let a = root.join("node-a");
    let b = root.join("node-b");
    let _cleanup = Nodes(vec![a.clone(), b.clone()]);
    let (port_a, task_a) = upstream("machine-A").await;
    let (port_b, task_b) = upstream("machine-B").await;
    let pa = free_port().to_string();
    let pb = free_port().to_string();
    for (node, port, target) in [(&a, &pa, port_a), (&b, &pb, port_b)] {
        ok(
            node,
            &[
                "proxy",
                "start",
                "--unprivileged",
                "--no-tls",
                "--port",
                port,
                "--bind",
                "127.0.0.1",
            ],
        )
        .await;
        ok(node, &["alias", "web", &target.to_string()]).await;
    }
    let identity_before = std::fs::read(b.join("proxy.identity")).unwrap();
    let agent_port = free_port().to_string();
    let endpoint = format!("https://127.0.0.1:{agent_port}");
    let output = command(
        &b,
        &[
            "discovery",
            "enable",
            "--bind",
            "0.0.0.0",
            "--port",
            &agent_port,
        ],
        None,
    )
    .await;
    assert!(!output.status.success(), "wildcard catalog must be refused");
    ok(
        &b,
        &[
            "discovery",
            "enable",
            "--bind",
            "127.0.0.1",
            "--port",
            &agent_port,
            "--no-advertise",
        ],
    )
    .await;
    let pin = ok(&b, &["discovery", "fingerprint"])
        .await
        .trim()
        .to_owned();
    let token = ok(&b, &["discovery", "pair-token"]).await;
    let bad = command(
        &a,
        &["peers", "add", &endpoint, "--fingerprint", &"0".repeat(64)],
        Some(&token),
    )
    .await;
    assert!(
        !bad.status.success(),
        "wrong certificate pin must fail before consuming token"
    );
    let paired = command(
        &a,
        &["peers", "add", &endpoint, "--fingerprint", &pin],
        Some(&token),
    )
    .await;
    assert!(
        paired.status.success(),
        "pair: {}",
        String::from_utf8_lossy(&paired.stderr)
    );
    let paired: Value = serde_json::from_slice(&paired.stdout).unwrap();
    assert!(
        paired.get("token").is_none(),
        "pair command must not disclose bearer credentials"
    );
    let peer = paired["machine"]["id"].as_str().unwrap();
    let reused = command(
        &a,
        &["peers", "add", &endpoint, "--fingerprint", &pin],
        Some(&token),
    )
    .await;
    assert!(!reused.status.success(), "one-time enrollment token reused");
    let catalog = ok(&a, &["list", "--all", "--json"]).await;
    let snapshot: Snapshot = serde_json::from_str(&catalog).unwrap();
    assert_eq!(snapshot.version, PROTOCOL_VERSION);
    assert_eq!(snapshot.peers.len(), 1);
    let remote = snapshot.peers[0].catalog.as_ref().unwrap();
    assert_ne!(snapshot.local.machine.id, remote.machine.id);
    assert!(
        !remote.apps[0].remote_access,
        "catalog enrollment alone does not share apps"
    );
    let local_only = command(&a, &["open", "web", "--peer", peer, "--url-only"], None).await;
    assert!(!local_only.status.success());
    // Explicitly grant route transport, preserving the existing loopback proxy settings and PID.
    ok(&b, &["discovery", "disable"]).await;
    ok(
        &b,
        &[
            "discovery",
            "enable",
            "--bind",
            "127.0.0.1",
            "--port",
            &agent_port,
            "--no-advertise",
            "--share-apps",
        ],
    )
    .await;
    assert_eq!(
        identity_before,
        std::fs::read(b.join("proxy.identity")).unwrap()
    );
    let url = ok(&a, &["open", "web", "--peer", peer, "--url-only"])
        .await
        .trim()
        .to_owned();
    let parsed = url::Url::parse(&url).unwrap();
    assert_eq!(
        url,
        format!("http://web.node-b.localhost:{pa}"),
        "readable URL"
    );
    let http = reqwest::Client::builder()
        .no_proxy()
        .resolve(
            parsed.host_str().unwrap(),
            std::net::SocketAddr::from(([127, 0, 0, 1], parsed.port().unwrap())),
        )
        .build()
        .unwrap();
    let body = http
        .get(format!("{}/nested?q=remote", url.trim_end_matches('/')))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        body.starts_with("machine-B:/nested?q=remote:web.localhost:"),
        "{body}"
    );
    assert!(!body.contains("machine-A"));
    let forged = http
        .get(&url)
        .header("Host", "evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(
        forged.status(),
        reqwest::StatusCode::MISDIRECTED_REQUEST,
        "relay DNS rebinding boundary"
    );
    // API has strict browser-origin and bearer boundaries, exercised separately by server tests.
    ok(&b, &["discovery", "revoke"]).await;
    let revoked = http.get(&url).send().await.unwrap();
    assert!(
        matches!(revoked.status().as_u16(), 401 | 502),
        "revoked bearer must not reach B: {}",
        revoked.status()
    );
    let stale: Snapshot =
        serde_json::from_str(&ok(&a, &["peers", "refresh", "--json"]).await).unwrap();
    assert!(stale.peers[0].catalog.is_none());
    assert!(stale.peers[0].error.is_some());
    // Re-enrolling must replace a cached relay that still holds the revoked bearer.
    let renewed = ok(&b, &["discovery", "pair-token"]).await;
    let enrolled = command(
        &a,
        &["peers", "add", &endpoint, "--fingerprint", &pin],
        Some(&renewed),
    )
    .await;
    assert!(enrolled.status.success());
    let renewed_url = ok(&a, &["open", "web", "--peer", peer, "--url-only"])
        .await
        .trim()
        .to_owned();
    let renewed_parsed = url::Url::parse(&renewed_url).unwrap();
    let renewed_http = reqwest::Client::builder()
        .no_proxy()
        .resolve(
            renewed_parsed.host_str().unwrap(),
            std::net::SocketAddr::from(([127, 0, 0, 1], renewed_parsed.port().unwrap())),
        )
        .build()
        .unwrap();
    let renewed_body = renewed_http
        .get(&renewed_url)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        renewed_body.starts_with("machine-B:"),
        "renewed relay still uses revoked credentials: {renewed_body}"
    );
    ok(&a, &["peers", "remove", peer]).await;
    let removed: Snapshot =
        serde_json::from_str(&ok(&a, &["list", "--all", "--json"]).await).unwrap();
    assert!(removed.peers.is_empty());
    assert_eq!(ok(&b, &["proxy", "status"]).await.trim(), "running");
    ok(&b, &["discovery", "disable"]).await;
    assert_eq!(ok(&b, &["proxy", "status"]).await.trim(), "running");
    task_a.abort();
    task_b.abort();
}

fn readable_client(url: &str) -> reqwest::Client {
    let parsed = url::Url::parse(url).unwrap();
    reqwest::Client::builder()
        .no_proxy()
        .resolve(
            parsed.host_str().unwrap(),
            std::net::SocketAddr::from(([127, 0, 0, 1], parsed.port().unwrap())),
        )
        .build()
        .unwrap()
}
async fn get(url: &str) -> String {
    readable_client(url)
        .get(format!("{url}/from?x=1"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}
/// `porch pair` on the host: returns the code and the still-waiting process.
async fn show_code(dir: &Path, port: &str) -> (String, Host) {
    use tokio::io::AsyncBufReadExt;
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_porch"))
        .args(["pair", "--bind", "127.0.0.1", "--port", port])
        .env("PORCH_STATE_DIR", dir)
        .env("PORCH_CLEAN_URLS", "0")
        .env("PORCH_SYNC_HOSTS", "0")
        .env("HOSTNAME", dir.file_name().unwrap())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let line = tokio::time::timeout(Duration::from_secs(20), lines.next_line())
        .await
        .expect("code shown")
        .unwrap()
        .unwrap();
    let code = line.strip_prefix("Pairing code: ").expect(&line).to_owned();
    assert_eq!(code.len(), 6);
    // Keep reading stdout so the final "Paired with" line is observable.
    let rest = tokio::spawn(async move {
        let mut out = String::new();
        while let Ok(Some(l)) = lines.next_line().await {
            out.push_str(&l);
            out.push('\n');
        }
        out
    });
    (code, Host { child, rest })
}
struct Host {
    child: tokio::process::Child,
    rest: tokio::task::JoinHandle<String>,
}
/// Exit status, remaining stdout and stderr of the waiting `porch pair`.
async fn finish(host: Host) -> (bool, String, String) {
    let output = tokio::time::timeout(Duration::from_secs(20), host.child.wait_with_output())
        .await
        .expect("pair host exits")
        .unwrap();
    let stdout = host.rest.await.unwrap();
    (
        output.status.success(),
        stdout,
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}
async fn setup(
    name: &str,
) -> (
    PathBuf,
    PathBuf,
    String,
    String,
    Nodes,
    Vec<tokio::task::JoinHandle<()>>,
) {
    let root = std::env::temp_dir().join(format!("porch-pair-{name}-{}", std::process::id()));
    let a = root.join("node-a");
    let b = root.join("node-b");
    let nodes = Nodes(vec![a.clone(), b.clone()]);
    let (port_a, task_a) = upstream("machine-A").await;
    let (port_b, task_b) = upstream("machine-B").await;
    let pa = free_port().to_string();
    let pb = free_port().to_string();
    for (node, port, target) in [(&a, &pa, port_a), (&b, &pb, port_b)] {
        ok(
            node,
            &[
                "proxy",
                "start",
                "--unprivileged",
                "--no-tls",
                "--port",
                port,
                "--bind",
                "127.0.0.1",
            ],
        )
        .await;
        ok(node, &["alias", "web", &target.to_string()]).await;
    }
    (a, b, pa, pb, nodes, vec![task_a, task_b])
}
#[tokio::test]
async fn code_pairing_is_mutual_and_apps_open_at_readable_urls_with_websockets() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
    let (a, b, pa, pb, _nodes, tasks) = setup("mutual").await;
    let agent_a = free_port().to_string();
    let agent_b = free_port().to_string();
    let host = format!("127.0.0.1:{agent_b}");
    let (code, waiting) = show_code(&b, &agent_b).await;
    let wrong = if code == "000000" { "111111" } else { "000000" };
    let join = |code: &str| {
        vec![
            "pair".to_owned(),
            code.to_owned(),
            "--host".into(),
            host.clone(),
            "--bind".into(),
            "127.0.0.1".into(),
            "--port".into(),
            agent_a.clone(),
        ]
    };
    let args = join(wrong);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let rejected = command(&a, &args, None).await;
    assert!(!rejected.status.success());
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("wrong pairing code"),
        "{}",
        String::from_utf8_lossy(&rejected.stderr)
    );
    // A failed join leaves this machine's sharing as it was: off.
    let status: Value =
        serde_json::from_str(&ok(&a, &["discovery", "status", "--json"]).await).unwrap();
    assert_eq!(status["running"], false, "{status}");
    assert!(status["config"].is_null(), "{status}");
    let args = join(&code);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let joined = ok(&a, &args).await;
    assert_eq!(joined.trim(), "Paired with node-b");
    let (success, stdout, stderr) = finish(waiting).await;
    assert!(success, "{stderr}");
    assert_eq!(stdout.trim(), "Paired with node-a");
    // One exchange: each side can now list and open the other's apps.
    let url_b = ok(&a, &["open", "web", "--peer", "node-b", "--url-only"]).await;
    assert_eq!(url_b.trim(), format!("http://web.node-b.localhost:{pa}"));
    let url_a = ok(&b, &["open", "web", "--peer", "node-a", "--url-only"]).await;
    assert_eq!(url_a.trim(), format!("http://web.node-a.localhost:{pb}"));
    let body = get(url_b.trim()).await;
    assert!(
        body.starts_with("machine-B:/from?x=1:web.localhost:"),
        "{body}"
    );
    let body = get(url_a.trim()).await;
    assert!(
        body.starts_with("machine-A:/from?x=1:web.localhost:"),
        "{body}"
    );
    let listed = ok(&a, &["list", "--all"]).await;
    assert!(
        listed.contains(&format!("http://web.node-b.localhost:{pa}")),
        "{listed}"
    );
    // Unknown machine/app names under a paired slug do not leak or misroute.
    let missing = readable_client(&format!("http://nope.node-b.localhost:{pa}"))
        .get(format!("http://nope.node-b.localhost:{pa}/"))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);
    // WebSocket through A's proxy, the gateway and B's agent to B's app.
    let stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{pa}"))
        .await
        .unwrap();
    let request = format!("ws://web.node-b.localhost:{pa}/ws")
        .into_client_request()
        .unwrap();
    let (mut socket, _) = tokio_tungstenite::client_async(request, stream)
        .await
        .unwrap();
    socket.send(Message::Text("hello".into())).await.unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(reply, Message::Text("machine-B:hello".into()));
    drop(socket);
    // URLs are stable: a proxy restart keeps the same name and it still works.
    ok(&a, &["proxy", "stop"]).await;
    ok(
        &a,
        &[
            "proxy",
            "start",
            "--unprivileged",
            "--no-tls",
            "--port",
            &pa,
            "--bind",
            "127.0.0.1",
        ],
    )
    .await;
    let again = ok(&a, &["open", "web", "--peer", "node-b", "--url-only"]).await;
    assert_eq!(again, url_b);
    assert!(get(again.trim()).await.starts_with("machine-B:"));
    // Unpair forgets both directions in one command.
    let unpaired: Value =
        serde_json::from_str(&ok(&a, &["unpair", "node-b", "--json"]).await).unwrap();
    assert_eq!(unpaired["machine"]["name"], "node-b");
    assert_eq!(unpaired["reached"], true, "{unpaired}");
    let left: Snapshot = serde_json::from_str(&ok(&a, &["list", "--all", "--json"]).await).unwrap();
    assert!(left.peers.is_empty());
    let right: Snapshot =
        serde_json::from_str(&ok(&b, &["list", "--all", "--json"]).await).unwrap();
    assert!(right.peers.is_empty(), "B still lists A after unpair");
    for t in tasks {
        t.abort();
    }
}
#[tokio::test]
async fn five_wrong_codes_close_pairing_for_everyone() {
    let (a, b, _pa, _pb, _nodes, tasks) = setup("lockout").await;
    let agent_a = free_port().to_string();
    let agent_b = free_port().to_string();
    let host = format!("127.0.0.1:{agent_b}");
    let (code, waiting) = show_code(&b, &agent_b).await;
    let wrong = if code == "000000" { "111111" } else { "000000" };
    for _ in 0..5 {
        let out = command(
            &a,
            &[
                "pair",
                wrong,
                "--host",
                &host,
                "--bind",
                "127.0.0.1",
                "--port",
                &agent_a,
            ],
            None,
        )
        .await;
        assert!(!out.status.success());
    }
    let (success, _, stderr) = finish(waiting).await;
    assert!(!success);
    assert!(stderr.contains("too many wrong pairing codes"), "{stderr}");
    let late = command(
        &a,
        &[
            "pair",
            &code,
            "--host",
            &host,
            "--bind",
            "127.0.0.1",
            "--port",
            &agent_a,
        ],
        None,
    )
    .await;
    assert!(!late.status.success(), "right code after lockout must fail");
    assert!(
        String::from_utf8_lossy(&late.stderr).contains("no longer waiting"),
        "{}",
        String::from_utf8_lossy(&late.stderr)
    );
    let peers: Snapshot =
        serde_json::from_str(&ok(&a, &["list", "--all", "--json"]).await).unwrap();
    assert!(peers.peers.is_empty());
    for t in tasks {
        t.abort();
    }
}
#[tokio::test]
async fn shown_codes_report_status_and_cancel_for_the_tray() {
    let (a, b, _pa, _pb, _nodes, tasks) = setup("status").await;
    let agent_a = free_port().to_string();
    let agent_b = free_port().to_string();
    let host = format!("127.0.0.1:{agent_b}");
    let shown: Value = serde_json::from_str(
        &ok(
            &b,
            &[
                "pair",
                "--no-wait",
                "--json",
                "--bind",
                "127.0.0.1",
                "--port",
                &agent_b,
            ],
        )
        .await,
    )
    .unwrap();
    let id = shown["id"].as_str().expect("id for pair status").to_owned();
    let code = shown["code"].as_str().unwrap().to_owned();
    let expires_in = shown["expires_in"].as_u64().unwrap();
    assert!((290..=300).contains(&expires_in), "{shown}");
    let status = |dir: PathBuf, id: String| async move {
        serde_json::from_str::<Value>(&ok(&dir, &["pair", "status", &id, "--json"]).await).unwrap()
    };
    let waiting = status(b.clone(), id.clone()).await;
    assert_eq!(waiting["state"], "waiting");
    assert!(waiting["expires_in"].as_u64().unwrap() <= expires_in);
    // A wrong code comes back as a structured, retryable error.
    let wrong = if code == "000000" { "111111" } else { "000000" };
    let join = |code: &str| {
        let host = host.clone();
        let agent_a = agent_a.clone();
        let a = a.clone();
        let code = code.to_owned();
        async move {
            command(
                &a,
                &[
                    "pair",
                    &code,
                    "--host",
                    &host,
                    "--bind",
                    "127.0.0.1",
                    "--port",
                    &agent_a,
                    "--json",
                ],
                None,
            )
            .await
        }
    };
    let out = join(wrong).await;
    assert!(!out.status.success());
    let error: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(error["error"]["code"], "wrong_code", "{error}");
    assert_eq!(error["error"]["retry"], true);
    let cancelled: Value =
        serde_json::from_str(&ok(&b, &["pair", "cancel", &id, "--json"]).await).unwrap();
    assert_eq!(cancelled["state"], "cancelled");
    assert_eq!(cancelled["reason"], "user");
    // Idempotent.
    let again: Value =
        serde_json::from_str(&ok(&b, &["pair", "cancel", &id, "--json"]).await).unwrap();
    assert_eq!(again["state"], "cancelled");
    assert_eq!(status(b.clone(), id.clone()).await["state"], "cancelled");
    let out = join(&code).await;
    assert!(!out.status.success(), "cancelled code must not work");
    let error: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(error["error"]["code"], "not_waiting", "{error}");
    // A new code replaces the old one; it pairs, and the host sees it confirmed.
    let shown: Value = serde_json::from_str(
        &ok(
            &b,
            &[
                "pair",
                "--no-wait",
                "--json",
                "--bind",
                "127.0.0.1",
                "--port",
                &agent_b,
            ],
        )
        .await,
    )
    .unwrap();
    let id2 = shown["id"].as_str().unwrap().to_owned();
    assert_eq!(status(b.clone(), id.clone()).await["state"], "cancelled");
    let out = join(shown["code"].as_str().unwrap()).await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let paired = status(b.clone(), id2).await;
    assert_eq!(paired["state"], "paired", "{paired}");
    assert_eq!(paired["peer"]["name"], "node-a");
    for t in tasks {
        t.abort();
    }
}
/// A fake launchctl that actually runs the generated LaunchAgent, so the test
/// proves the plist starts a working agent, not just that a file was written.
#[cfg(unix)]
fn fake_launchctl(bin: &Path) {
    let script = r#"#!/usr/bin/env python3
import os, plistlib, signal, subprocess, sys
log = os.environ["FAKE_LAUNCHCTL_LOG"]
pidfile = log + ".pid"
with open(log, "a") as f:
    f.write(" ".join(sys.argv[1:]) + "\n")
a = sys.argv[1:]
if a[0] == "bootstrap":
    with open(a[2], "rb") as f:
        job = plistlib.load(f)
    env = dict(os.environ, **job.get("EnvironmentVariables", {}))
    out = open(job["StandardErrorPath"], "a")
    p = subprocess.Popen(job["ProgramArguments"], env=env, stdin=subprocess.DEVNULL,
                         stdout=out, stderr=out, start_new_session=True)
    open(pidfile, "w").write(str(p.pid))
elif a[0] == "bootout":
    try:
        os.kill(int(open(pidfile).read()), signal.SIGTERM)
        os.remove(pidfile)
    except (FileNotFoundError, ProcessLookupError, ValueError):
        sys.exit(3)
"#;
    std::fs::create_dir_all(bin).unwrap();
    let path = bin.join("launchctl");
    common::write_executable(&path, script);
}
#[cfg(unix)]
#[tokio::test]
async fn autostart_hands_sharing_to_a_login_agent_and_disable_keeps_it_running() {
    let root = std::env::temp_dir().join(format!("porch-autostart-{}", std::process::id()));
    let node = root.join("node");
    let bin = root.join("bin");
    let plist = root.join("LaunchAgents/dev.porch.discovery.plist");
    let _cleanup = Nodes(vec![node.clone()]);
    fake_launchctl(&bin);
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let run = |args: &[&str]| {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_porch"))
            .args(args)
            .env("PORCH_STATE_DIR", &node)
            .env("PORCH_CLEAN_URLS", "0")
            .env("PORCH_SYNC_HOSTS", "0")
            .env("PORCH_SERVICE_PLATFORM", "macos")
            .env("PORCH_DISCOVERY_AUTOSTART_FILE", &plist)
            .env("FAKE_LAUNCHCTL_LOG", root.join("launchctl.log"))
            .env("PATH", &path)
            .output()
            .unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned()
                + &String::from_utf8_lossy(&out.stderr),
        )
    };
    let pid = || std::fs::read_to_string(node.join("discovery.ready")).unwrap_or_default();

    // Sharing must be on first; nothing is written otherwise.
    let (ok, out) = run(&["discovery", "autostart", "enable"]);
    assert!(!ok && out.contains("sharing is off"), "{out}");
    assert!(!plist.exists());

    let port = free_port().to_string();
    let (ok, out) = run(&[
        "discovery",
        "enable",
        "--bind",
        "127.0.0.1",
        "--port",
        &port,
        "--no-advertise",
    ]);
    assert!(ok, "{out}");
    let manual = pid();
    assert!(!manual.is_empty());
    assert!(run(&["discovery", "status"])
        .1
        .contains("Start at login: off"));

    // Enable: the hand-started agent is replaced by the launchd-run one.
    let (ok, out) = run(&["discovery", "autostart", "enable"]);
    let supervised = pid();
    // On failure, show why the launchd-started agent never became ready.
    let evidence = || {
        format!(
            "enable: {out}\nmanual pid {manual}, ready pid {supervised:?}\nlaunchctl calls:\n{}\ndiscovery.log:\n{}",
            std::fs::read_to_string(root.join("launchctl.log")).unwrap_or_default(),
            std::fs::read_to_string(node.join("discovery.log")).unwrap_or_default()
        )
    };
    assert!(ok && out.contains("now starts at login."), "{}", evidence());
    assert!(!out.contains("not ready yet"), "{}", evidence());
    let body = std::fs::read_to_string(&plist).unwrap();
    assert!(body.contains("<string>serve</string><string>--autostart</string>"));
    assert!(
        !supervised.is_empty() && supervised != manual,
        "{}",
        evidence()
    );
    let (_, status) = run(&["discovery", "status"]);
    assert!(
        status.contains("running") && status.contains("Start at login: on"),
        "{status}"
    );
    assert_eq!(
        run(&["discovery", "autostart", "status"]).1.trim(),
        "enabled"
    );

    // A second login-time start while the agent runs exits cleanly (no fail loop).
    let (ok, out) = run(&["discovery", "serve", "--autostart"]);
    assert!(ok && out.contains("already running"), "{out}");

    // Another state directory neither sees nor removes this startup file.
    let other = root.join("other");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_porch"))
        .args(["discovery", "disable"])
        .env("PORCH_STATE_DIR", &other)
        .env("PORCH_CLEAN_URLS", "0")
        .env("PORCH_SERVICE_PLATFORM", "macos")
        .env("PORCH_DISCOVERY_AUTOSTART_FILE", &plist)
        .env("FAKE_LAUNCHCTL_LOG", root.join("launchctl.log"))
        .env("PATH", &path)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(plist.exists());

    // Disable login startup: the file goes, sharing stays on with a new agent.
    let (ok, out) = run(&["discovery", "autostart", "disable"]);
    assert!(ok && out.contains("Sharing is unchanged"), "{out}");
    assert!(!plist.exists());
    let after = pid();
    assert!(!after.is_empty() && after != supervised);
    assert!(run(&["discovery", "status"]).1.contains("running"));

    // Turning sharing off also removes login startup.
    assert!(run(&["discovery", "autostart", "enable"]).0);
    assert!(plist.exists());
    assert!(run(&["discovery", "disable"]).0);
    assert!(!plist.exists());
    let (ok, out) = run(&["discovery", "serve", "--autostart"]);
    assert!(ok && out.contains("disabled"), "{out}");
    let _ = std::fs::remove_dir_all(&root);
}

/// A machine on the network that advertises an open pairing window without
/// knowing the code. `legacy` speaks only the old version 1 exchange.
struct Rogue {
    addr: std::net::SocketAddr,
    seen: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
    task: tokio::task::JoinHandle<()>,
}
async fn rogue(legacy: bool) -> Rogue {
    use http_body_util::{BodyExt, Full};
    use hyper::{body::Incoming, Request, Response};
    use std::sync::{Arc, Mutex};
    let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let key = rustls::pki_types::PrivateKeyDer::try_from(cert.key_pair.serialize_der()).unwrap();
    let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![cert.cert.der().clone()], key)
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let log = seen.clone();
    let hex = |b: &[u8]| b.iter().map(|v| format!("{v:02x}")).collect::<String>();
    let task = tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let acceptor = acceptor.clone();
            let log = log.clone();
            tokio::spawn(async move {
                let Ok(stream) = acceptor.accept(tcp).await else {
                    return;
                };
                let service = hyper::service::service_fn(move |req: Request<Incoming>| {
                    let log = log.clone();
                    async move {
                        let path = req.uri().path().to_owned();
                        let body = req.into_body().collect().await?.to_bytes();
                        log.lock()
                            .unwrap()
                            .push((path.clone(), String::from_utf8_lossy(&body).into_owned()));
                        let reply = match (legacy, path.as_str()) {
                            (true, "/v1/pairing") => r#"{"version":1,"open":true}"#.to_owned(),
                            (false, "/v1/pairing") => format!(
                                r#"{{"version":1,"open":true,"pairing":2,"salt":"{}"}}"#,
                                "5a".repeat(32)
                            ),
                            // A real SPAKE2 reply for a guessed code.
                            (false, "/v1/pair-code/start") => {
                                let (_, msg) = spake2::Spake2::<spake2::Ed25519Group>::start_b(
                                    &spake2::Password::new(b"guess"),
                                    &spake2::Identity::new(b"porch-pair-v2-joiner"),
                                    &spake2::Identity::new(b"porch-pair-v2-host"),
                                );
                                format!(r#"{{"session":"s","msg":"{}"}}"#, hex(&msg))
                            }
                            (false, "/v1/pair-code/confirm") => {
                                format!(r#"{{"confirm":"{}"}}"#, "ab".repeat(32))
                            }
                            // What a successful impersonation would return.
                            _ => format!(
                                r#"{{"version":1,"machine":{{"id":"rogue","name":"rogue"}},"token":"{}","mac":"{}"}}"#,
                                "t".repeat(64),
                                "00".repeat(32)
                            ),
                        };
                        Ok::<_, hyper::Error>(Response::new(Full::new(bytes::Bytes::from(reply))))
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    Rogue { addr, seen, task }
}
async fn join_rogue(legacy: bool) -> (String, Vec<(String, String)>, PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "porch-rogue-{}-{}",
        if legacy { "v1" } else { "v2" },
        std::process::id()
    ));
    let node = root.join("node-a");
    let _nodes = Nodes(vec![node.clone()]);
    let r = rogue(legacy).await;
    let out = command(
        &node,
        &[
            "pair",
            "482913",
            "--host",
            &r.addr.to_string(),
            "--bind",
            "127.0.0.1",
            "--port",
            &free_port().to_string(),
        ],
        None,
    )
    .await;
    assert!(!out.status.success(), "joined a rogue machine");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    // Nothing that grants access was created: no sharing, no credential, no peer.
    assert!(
        !node.join("discovery-config.json").exists(),
        "sharing was turned on"
    );
    let auth = std::fs::read_to_string(node.join("discovery-auth.json")).unwrap_or_default();
    assert!(
        !auth.contains("\"clients\":[\""),
        "a credential was minted: {auth}"
    );
    let peers = command(&node, &["list", "--all", "--json"], None).await;
    let peers: Snapshot = serde_json::from_slice(&peers.stdout).unwrap();
    assert!(peers.peers.is_empty());
    r.task.abort();
    let seen = r.seen.lock().unwrap().clone();
    (stderr, seen, node)
}
#[tokio::test]
async fn a_rogue_machine_claiming_open_pairing_gets_neither_code_nor_credentials() {
    let (stderr, seen, _) = join_rogue(false).await;
    assert!(
        stderr.contains("did not prove it knows the pairing code"),
        "{stderr}"
    );
    let paths: Vec<&str> = seen.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(
        paths,
        [
            "/v1/pairing",
            "/v1/pair-code/start",
            "/v1/pair-code/confirm"
        ],
        "the joiner stops before sending credentials"
    );
    // Every request carries only the exchange's own fields: values the rogue
    // issued (salt, session) and fixed-size cryptographic hex. The code has
    // no field to appear in. (A substring check for "482913" would fail by
    // chance on random hex.)
    let hex = |v: &Value, len: usize| {
        v.as_str()
            .is_some_and(|s| s.len() == len && s.bytes().all(|b| b.is_ascii_hexdigit()))
    };
    for (path, body) in &seen {
        assert!(
            !body.contains("token"),
            "{path} carried a credential: {body}"
        );
        if body.is_empty() {
            continue;
        }
        let fields: Value = serde_json::from_str(body).unwrap();
        let fields = fields.as_object().unwrap();
        let ok = match path.as_str() {
            "/v1/pair-code/start" => {
                fields.len() == 3
                    && fields["pairing"].is_u64()
                    && fields["salt"] == "5a".repeat(32)
                    && hex(&fields["msg"], 66)
            }
            "/v1/pair-code/confirm" => {
                fields.len() == 2 && fields["session"] == "s" && hex(&fields["confirm"], 64)
            }
            _ => false,
        };
        assert!(ok, "{path} carried unexpected fields: {body}");
    }
}
#[tokio::test]
async fn joiners_refuse_to_downgrade_to_the_insecure_exchange() {
    let (stderr, seen, _) = join_rogue(true).await;
    assert!(stderr.contains("older Porch"), "{stderr}");
    let paths: Vec<&str> = seen.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(
        paths,
        ["/v1/pairing"],
        "nothing is sent to an old-protocol agent"
    );
}

/// A paired machine serving one catalog over pinned TLS, counting requests.
async fn catalog_peer(
    catalog: Value,
    token: &'static str,
) -> (
    String,
    String,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    use hyper::{body::Incoming, Request, Response};
    use sha2::Digest;
    use std::sync::{atomic::Ordering, Arc};
    let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let pin: String = sha2::Sha256::digest(cert.cert.der())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let key = rustls::pki_types::PrivateKeyDer::try_from(cert.key_pair.serialize_der()).unwrap();
    let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![cert.cert.der().clone()], key)
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("https://{}/", listener.local_addr().unwrap());
    let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = hits.clone();
    let body = catalog.to_string();
    let task = tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let (acceptor, counted, body) = (acceptor.clone(), counted.clone(), body.clone());
            tokio::spawn(async move {
                let Ok(stream) = acceptor.accept(tcp).await else {
                    return;
                };
                let service = hyper::service::service_fn(move |req: Request<Incoming>| {
                    counted.fetch_add(1, Ordering::Relaxed);
                    let authorized = req.uri().path() == "/v1/catalog"
                        && req
                            .headers()
                            .get("authorization")
                            .is_some_and(|h| h.as_bytes() == format!("Bearer {token}").as_bytes());
                    let mut response = Response::new(http_body_util::Full::new(
                        bytes::Bytes::from(if authorized {
                            body.clone()
                        } else {
                            String::new()
                        }),
                    ));
                    if !authorized {
                        *response.status_mut() = hyper::StatusCode::UNAUTHORIZED;
                    }
                    async move { Ok::<_, std::convert::Infallible>(response) }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    (endpoint, pin, hits, task)
}
#[tokio::test]
async fn opening_an_app_on_one_peer_never_waits_on_an_unreachable_other_peer() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let root = std::env::temp_dir().join(format!("porch-open-one-peer-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let node = root.join("node-a");
    std::fs::create_dir_all(&node).unwrap();
    let _nodes = Nodes(vec![node.clone()]);
    let token = "t0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let app = |id: &str, name: &str| {
        serde_json::json!({
            "id": id, "name": name, "local_url": "http://app.localhost:7462",
            "health": "healthy", "remote_access": true, "unavailable_reason": null
        })
    };
    let catalog = serde_json::json!({
        "version": PROTOCOL_VERSION,
        "machine": {"id": "machine-b", "name": "Remote Mac"},
        // "My Notes" cannot be a hostname, so it needs a relay; "web" can.
        "apps": [app("notes", "My Notes"), app("web", "web")],
        "proxy_running": true,
        "generated_at": 1
    });
    let (endpoint, pin, reachable_hits, reachable) = catalog_peer(catalog, token).await;
    // An offline machine that accepts TCP and never answers: contacting it at
    // all would cost the client its connect and request timeouts.
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let silent_endpoint = format!("https://{}/", silent.local_addr().unwrap());
    let silent_hits = std::sync::Arc::new(AtomicUsize::new(0));
    let counted = silent_hits.clone();
    let offline = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((tcp, _)) = silent.accept().await {
            counted.fetch_add(1, Ordering::Relaxed);
            held.push(tcp);
        }
    });
    let peer = |id: &str, name: &str, endpoint: &str, pin: &str| {
        serde_json::json!({
            "machine": {"id": id, "name": name}, "endpoint": endpoint,
            "fingerprint": pin, "token": token, "sources": ["explicit"], "last_seen": 0
        })
    };
    // The offline machine is enrolled first, as a refresh of every peer would
    // meet it first.
    let peers = serde_json::json!([
        peer(
            "machine-offline",
            "Offline Box",
            &silent_endpoint,
            &"0".repeat(64)
        ),
        peer("machine-b", "Remote Mac", &endpoint, &pin),
    ]);
    std::fs::write(
        node.join("discovery-peers.json"),
        serde_json::to_vec(&peers).unwrap(),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            node.join("discovery-peers.json"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }

    // The relay path: `porch open` starts a relay and prints its URL.
    let url = ok(
        &node,
        &["open", "notes", "--peer", "machine-b", "--url-only"],
    )
    .await;
    let url = url.trim();
    assert!(
        url.starts_with("http://porch-") && url.contains(".localhost:"),
        "{url}"
    );
    // A hostname-routed app gets as far as the local-proxy check, which means
    // the selected peer's catalog was fetched and the app matched.
    let web = command(
        &node,
        &["open", "web", "--peer", "remote-mac", "--url-only"],
        None,
    )
    .await;
    assert!(
        String::from_utf8_lossy(&web.stderr).contains("local proxy is stopped"),
        "{}",
        String::from_utf8_lossy(&web.stderr)
    );
    assert!(reachable_hits.load(Ordering::Relaxed) >= 2);
    assert_eq!(
        silent_hits.load(Ordering::Relaxed),
        0,
        "opening an app on machine-b must not contact another paired machine"
    );
    // The selected peer's authenticated refresh is still recorded.
    let stored: Value =
        serde_json::from_slice(&std::fs::read(node.join("discovery-peers.json")).unwrap()).unwrap();
    let last_seen = |id: &str| {
        stored
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["machine"]["id"] == id)
            .unwrap()["last_seen"]
            .as_u64()
            .unwrap()
    };
    assert!(last_seen("machine-b") > 0);
    assert_eq!(last_seen("machine-offline"), 0);
    reachable.abort();
    offline.abort();
}
