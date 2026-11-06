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
    let app = axum::Router::new().fallback(
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
    assert!(parsed.host_str().unwrap().ends_with(".localhost"));
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
    assert_eq!(revoked.status(), reqwest::StatusCode::UNAUTHORIZED);
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
