//! Two isolated Porch installations on loopback, using the actual CLI and TLS agent.
//! This proves machine targeting; it does not claim physical LAN/tailnet reachability.
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
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
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
    let unpaired = ok(&a, &["unpair", "node-b"]).await;
    assert_eq!(unpaired.trim(), "Unpaired node-b");
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
        String::from_utf8_lossy(&late.stderr).contains("no machine is showing a pairing code"),
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
