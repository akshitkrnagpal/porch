mod common;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Full};
use hyper::{header, Request};
use hyper_util::rt::{TokioExecutor, TokioIo};
use porch::state::{ProxySettings, Route};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, protocol::Role, Message};

use common::free_port;
async fn ready(port: u16) {
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("proxy readiness timeout");
}
fn tls_client(dir: &std::path::Path, h2: bool) -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    let raw = std::fs::read(dir.join("ca.pem")).unwrap();
    for cert in rustls_pemfile::certs(&mut std::io::BufReader::new(raw.as_slice())) {
        roots.add(cert.unwrap()).unwrap();
    }
    let mut cfg = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    cfg.alpn_protocols = vec![if h2 {
        b"h2".to_vec()
    } else {
        b"http/1.1".to_vec()
    }];
    Arc::new(cfg)
}
async fn tls_stream(
    dir: &std::path::Path,
    port: u16,
    h2: bool,
) -> tokio_rustls::client::TlsStream<tokio::net::TcpStream> {
    let stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    tokio_rustls::TlsConnector::from(tls_client(dir, h2))
        .connect("app.localhost".try_into().unwrap(), stream)
        .await
        .unwrap()
}
async fn echo<S>(mut ws: tokio_tungstenite::WebSocketStream<S>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    ws.send(Message::Text("wire echo".into())).await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .into_text()
            .unwrap(),
        "wire echo"
    );
    ws.close(None).await.unwrap();
}

#[tokio::test]
async fn actual_http_tls_h2_ws_streaming_routing_and_redirects() {
    let dir = std::env::temp_dir().join(format!("porch-network-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::env::set_var("PORCH_STATE_DIR", &dir);
    std::env::set_var("PORCH_SYNC_HOSTS", "0");
    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up = upstream.local_addr().unwrap().port();
    let app = axum::Router::new()
        .route("/", axum::routing::get(|| async { "hello" }))
        .route("/headers", axum::routing::get(|h:header::HeaderMap| async move {
            serde_json::json!({"host":h.get("host").unwrap().to_str().unwrap(),"proto":h.get("x-forwarded-proto").unwrap().to_str().unwrap(),"hops":h.get("x-portless-hops").unwrap().to_str().unwrap(),"porch_hops":h.get("x-porch-hops").unwrap().to_str().unwrap(),"port":h.get("x-forwarded-port").unwrap().to_str().unwrap()}).to_string()
        }))
        .route("/sse", axum::routing::get(|| async {
            let events=futures_util::stream::unfold(0,|n| async move {
                if n>=2 { return None; }
                if n==1 { tokio::time::sleep(Duration::from_millis(800)).await; }
                Some((Ok::<_,std::convert::Infallible>(axum::response::sse::Event::default().data(if n==0 {"first"} else {"second"})), n+1))
            });
            axum::response::Sse::new(events)
        }))
        .route("/ws", axum::routing::get(|ws:axum::extract::WebSocketUpgrade| async {
            ws.on_upgrade(|mut socket| async move {
                while let Some(Ok(message))=socket.recv().await {
                    if socket.send(message).await.is_err() { break; }
                }
            })
        }));
    let server = tokio::spawn(async move { axum::serve(upstream, app).await.unwrap() });
    porch::state::mutate(|s| {
        for name in ["app", "api.app", "scoped.test"] {
            s.routes.push(Route {
                name: name.into(),
                target: format!("http://127.0.0.1:{up}"),
                pid: None,
            });
        }
    })
    .unwrap();
    porch::state::mutate(|s| {
        let pid = std::process::id();
        s.routes.push(Route {
            name: "stale".into(),
            target: format!("http://127.0.0.1:{up}"),
            pid: Some(pid),
        });
        s.owners.insert(
            "stale".into(),
            porch::state::ProcessIdentity {
                pid,
                started: "different process start identity".into(),
                child: None,
                child_started: None,
            },
        );
    })
    .unwrap();
    for tls in [false, true] {
        let port = free_port();
        let redirect_port = free_port();
        let settings = ProxySettings {
            port,
            tls,
            tlds: vec![
                "localhost".into(),
                "test".into(),
                "dev.example.com".into(),
                "example.com".into(),
            ],
            redirect_port: tls.then_some(redirect_port),
            // This case exercises the privileged hosts-sync authentication path.
            unprivileged: false,
            ..ProxySettings::default()
        };
        porch::state::mutate(|s| {
            s.routes.retain(|r| r.name != "loop");
            s.routes.push(Route {
                name: "loop".into(),
                target: format!("http://127.0.0.1:{port}"),
                pid: None,
            });
        })
        .unwrap();
        let proxy = tokio::spawn(porch::proxy::serve_config(settings));
        ready(port).await;
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .resolve(
                "app.localhost",
                format!("127.0.0.1:{port}").parse().unwrap(),
            );
        if tls {
            builder = builder.add_root_certificate(
                reqwest::Certificate::from_pem(&std::fs::read(dir.join("ca.pem")).unwrap())
                    .unwrap(),
            );
        }
        let client = builder.build().unwrap();
        let url = format!(
            "{}://app.localhost:{port}",
            if tls { "https" } else { "http" }
        );
        let r = client.get(&url).send().await.unwrap();
        assert_eq!(r.text().await.unwrap(), "hello");
        if tls {
            let output=tokio::process::Command::new("python3").args(["-c",r#"import ssl,socket,sys
context=ssl.create_default_context(cafile=sys.argv[1])
context.verify_flags |= ssl.VERIFY_X509_STRICT
with context.wrap_socket(socket.create_connection(('127.0.0.1',int(sys.argv[2])),timeout=5),server_hostname='app.localhost') as s:
 s.sendall(b'GET / HTTP/1.1\r\nHost: app.localhost\r\nConnection: close\r\n\r\n')
 response=b''
 while True:
  data=s.recv(65536)
  if not data:break
  response+=data
 assert b'hello' in response,response
"#]).arg(dir.join("ca.pem")).arg(port.to_string()).output().await.unwrap();
            assert!(
                output.status.success(),
                "strict OpenSSL verification: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let h: serde_json::Value = client
            .get(format!("{url}/headers"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(h["host"], format!("app.localhost:{port}"));
        assert_eq!(h["proto"], if tls { "https" } else { "http" });
        assert_eq!(h["hops"], "1");
        assert_eq!(h["porch_hops"], "1");
        assert_eq!(h["port"], port.to_string());
        let start = Instant::now();
        let mut stream = client
            .get(format!("{url}/sse"))
            .send()
            .await
            .unwrap()
            .bytes_stream();
        let first = tokio::time::timeout(Duration::from_millis(500), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(String::from_utf8_lossy(&first).contains("first"));
        assert!(start.elapsed() < Duration::from_millis(600));
        let second = stream.next().await.unwrap().unwrap();
        assert!(String::from_utf8_lossy(&second).contains("second"));
        assert!(start.elapsed() >= Duration::from_millis(700));
        if !tls {
            let actual_loop = tokio::time::timeout(
                Duration::from_secs(5),
                client.get(&url).header("host", "loop.localhost").send(),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(actual_loop.status(), 508);
        }
        let looped = client
            .get(&url)
            .header("x-portless-hops", "5")
            .send()
            .await
            .unwrap();
        assert_eq!(looped.status(), 508);
        assert_eq!(
            looped.json::<serde_json::Value>().await.unwrap()["error"],
            "loop_detected"
        );
        assert_eq!(
            client
                .get(&url)
                .header("host", "stale.localhost")
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
        let foreign = client
            .get(&url)
            .header("host", "unrelated.example.org")
            .send()
            .await
            .unwrap();
        assert_eq!(foreign.status(), 421);
        assert_eq!(
            foreign.json::<serde_json::Value>().await.unwrap()["error"],
            "misdirected_request"
        );
        let auth_url = format!("http://127.0.0.1:{port}/_porch/hosts-sync");
        let challenge = "ab".repeat(32);
        let proof = client
            .head(&auth_url)
            .header("x-porch-hosts-sync-challenge", &challenge)
            .send()
            .await
            .unwrap();
        assert_eq!(proof.status(), 200);
        let token = std::fs::read_to_string(dir.join("proxy.hosts-sync-token")).unwrap();
        assert!(porch::integrations::verify_hosts_sync(
            token.trim(),
            &challenge,
            proof.headers()["x-porch-hosts-sync-proof"]
                .to_str()
                .unwrap()
        ));
        assert_eq!(client.post(&auth_url).send().await.unwrap().status(), 403);
        assert_eq!(
            client
                .head(&auth_url)
                .header("origin", "https://evil.example")
                .header("x-porch-hosts-sync-challenge", &challenge)
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
        let unknown = client
            .get(&url)
            .header("host", "unknown.localhost")
            .header("accept", "text/html")
            .send()
            .await
            .unwrap();
        assert_eq!(unknown.status(), 404);
        assert!(unknown.text().await.unwrap().contains("<!doctype html>"));
        // Plain and secure WebSocket paths both preserve bidirectional frames.
        let stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        let request = format!("ws://app.localhost:{port}/ws")
            .into_client_request()
            .unwrap();
        let (ws, _) = tokio_tungstenite::client_async(request, stream)
            .await
            .unwrap();
        echo(ws).await;
        if tls {
            let stream = tls_stream(&dir, port, false).await;
            let request = format!("wss://app.localhost:{port}/ws")
                .into_client_request()
                .unwrap();
            let (ws, _) = tokio_tungstenite::client_async(request, stream)
                .await
                .unwrap();
            echo(ws).await;
            for p in [port, redirect_port] {
                let r = client
                    .get(format!("http://127.0.0.1:{p}/path?q=1"))
                    .header("host", format!("app.localhost:{p}"))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(r.status(), 302);
                assert_eq!(
                    r.headers()["location"],
                    format!("https://app.localhost:{port}/path?q=1")
                );
            }
            // RFC 8441 bridges an h2 extended CONNECT to a classic upstream Upgrade.
            let stream = tls_stream(&dir, port, true).await;
            let (mut sender, conn) = hyper::client::conn::http2::handshake::<_, _, Full<Bytes>>(
                TokioExecutor::new(),
                TokioIo::new(stream),
            )
            .await
            .unwrap();
            let mut conn = Box::pin(conn);
            tokio::time::timeout(
                Duration::from_secs(5),
                futures_util::future::poll_fn(|cx| {
                    use std::future::Future;
                    if let std::task::Poll::Ready(result) = conn.as_mut().poll(cx) {
                        panic!("h2 closed before settings: {result:?}");
                    }
                    if conn.is_extended_connect_protocol_enabled() {
                        std::task::Poll::Ready(())
                    } else {
                        std::task::Poll::Pending
                    }
                }),
            )
            .await
            .unwrap();
            let conn = tokio::spawn(conn);
            let h2get = Request::builder()
                .uri(format!("https://app.localhost:{port}/"))
                .body(Full::new(Bytes::new()))
                .unwrap();
            let h2response = sender.send_request(h2get).await.unwrap();
            assert_eq!(h2response.version(), hyper::Version::HTTP_2);
            assert_eq!(
                h2response.into_body().collect().await.unwrap().to_bytes(),
                "hello"
            );
            let mut request = Request::builder()
                .method("CONNECT")
                .uri(format!("https://app.localhost:{port}/ws"))
                .version(hyper::Version::HTTP_2)
                .header("sec-websocket-version", "13")
                .body(Full::new(Bytes::new()))
                .unwrap();
            request
                .extensions_mut()
                .insert(hyper::ext::Protocol::from_static("websocket"));
            let mut response = sender.send_request(request).await.unwrap();
            assert_eq!(response.status(), 200);
            assert!(!response.headers().contains_key("sec-websocket-accept"));
            let upgraded = hyper::upgrade::on(&mut response).await.unwrap();
            let ws = tokio_tungstenite::WebSocketStream::from_raw_socket(
                TokioIo::new(upgraded),
                Role::Client,
                None,
            )
            .await;
            echo(ws).await;
            conn.abort();
        }
        // IPv6 listener is simultaneous with IPv4, not a choice between them.
        let client6 = reqwest::Client::builder()
            .no_proxy()
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap();
        assert_eq!(
            client6
                .get(format!(
                    "{}://[::1]:{port}/",
                    if tls { "https" } else { "http" }
                ))
                .header("host", "app.localhost")
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "hello"
        );
        // Dotted logical names and explicit TLD-scoped aliases resolve correctly.
        for host in [
            "api.app.localhost",
            "app.test",
            "app.dev.example.com",
            "scoped.test",
        ] {
            assert_eq!(
                client
                    .get(&url)
                    .header("host", host)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                200
            );
        }
        assert_eq!(
            client
                .get(&url)
                .header("host", "scoped.localhost")
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
        proxy.abort();
        let _ = proxy.await;
    }
    // Most-specific parent wins only when wildcard mode is enabled.
    let port = free_port();
    let proxy = tokio::spawn(porch::proxy::serve_config(ProxySettings {
        port,
        tls: false,
        wildcard: true,
        ..ProxySettings::default()
    }));
    ready(port).await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    assert_eq!(
        client
            .get(format!("http://127.0.0.1:{port}"))
            .header("host", "tenant.api.app.localhost")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "hello"
    );
    proxy.abort();
    let _ = proxy.await;
    // Numeric loopback aliases still reach IPv6-only application listeners.
    let v6 = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
    let v6port = v6.local_addr().unwrap().port();
    let v6server = tokio::spawn(async move {
        axum::serve(
            v6,
            axum::Router::new().route("/", axum::routing::get(|| async { "ipv6 upstream" })),
        )
        .await
        .unwrap()
    });
    porch::state::mutate(|s| {
        s.routes.push(Route {
            name: "v6".into(),
            target: format!("http://127.0.0.1:{v6port}"),
            pid: None,
        })
    })
    .unwrap();
    let port = free_port();
    let proxy = tokio::spawn(porch::proxy::serve_config(ProxySettings {
        port,
        tls: false,
        ..ProxySettings::default()
    }));
    ready(port).await;
    assert_eq!(
        client
            .get(format!("http://127.0.0.1:{port}"))
            .header("host", "v6.localhost")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "ipv6 upstream"
    );
    porch::state::mutate(|s| {
        s.shares.insert(
            "app".into(),
            std::collections::BTreeMap::from([(
                "PORTLESS_TAILSCALE_URL".into(),
                "https://dev.team.ts.net".into(),
            )]),
        );
        s.shares.insert(
            "v6".into(),
            std::collections::BTreeMap::from([(
                "PORTLESS_TAILSCALE_URL".into(),
                "https://dev.team.ts.net:8443".into(),
            )]),
        );
    })
    .unwrap();
    for (host, expected) in [
        ("dev.team.ts.net", "hello"),
        ("dev.team.ts.net:8443", "ipv6 upstream"),
        ("dev.team.ts.net:9999", "hello"),
    ] {
        assert_eq!(
            client
                .get(format!("http://127.0.0.1:{port}"))
                .header("host", host)
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            expected
        );
    }
    proxy.abort();
    let _ = proxy.await;
    // A supplied certificate/key pair is served and verified without replacing the local CA.
    let pair = rcgen::generate_simple_self_signed(vec!["app.localhost".into()]).unwrap();
    let cert = dir.join("custom.pem");
    let key = dir.join("custom-key.pem");
    std::fs::write(&cert, pair.cert.pem()).unwrap();
    std::fs::write(&key, pair.key_pair.serialize_pem()).unwrap();
    let port = free_port();
    let proxy = tokio::spawn(porch::proxy::serve_config(ProxySettings {
        port,
        tls: true,
        cert: Some(cert.clone()),
        key: Some(key),
        ..ProxySettings::default()
    }));
    ready(port).await;
    let custom = reqwest::Client::builder()
        .no_proxy()
        .add_root_certificate(
            reqwest::Certificate::from_pem(&std::fs::read(cert).unwrap()).unwrap(),
        )
        .resolve(
            "app.localhost",
            format!("127.0.0.1:{port}").parse().unwrap(),
        )
        .build()
        .unwrap();
    assert_eq!(
        custom
            .get(format!("https://app.localhost:{port}/"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "hello"
    );
    proxy.abort();
    let _ = proxy.await;
    // Mode policy is enforced even when a local caller possesses the token.
    let hosts = dir.join("hosts.fixture");
    std::fs::write(&hosts, "127.0.0.1 localhost\n").unwrap();
    std::env::set_var("PORCH_HOSTS_FILE", &hosts);
    std::env::set_var("PORCH_SYNC_HOSTS", "1");
    let port = free_port();
    let proxy = tokio::spawn(porch::proxy::serve_config(ProxySettings {
        port,
        tls: false,
        unprivileged: true,
        bind: Some("127.0.0.1".into()),
        ..ProxySettings::default()
    }));
    ready(port).await;
    let auth_url = format!("http://127.0.0.1:{port}/_porch/hosts-sync");
    let token = std::fs::read_to_string(dir.join("proxy.hosts-sync-token")).unwrap();
    let denied = client
        .post(&auth_url)
        .header("x-porch-hosts-sync-token", token.trim())
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 409);
    assert_eq!(
        denied.json::<serde_json::Value>().await.unwrap()["error"],
        "hosts_sync_disabled"
    );
    let challenge = client
        .head(&auth_url)
        .header("x-porch-hosts-sync-challenge", "ab".repeat(32))
        .send()
        .await
        .unwrap();
    assert_eq!(challenge.status(), 409);
    assert!(!challenge.headers().contains_key("x-porch-hosts-sync-proof"));
    assert_eq!(
        std::fs::read_to_string(&hosts).unwrap(),
        "127.0.0.1 localhost\n"
    );
    proxy.abort();
    let _ = proxy.await;
    std::env::remove_var("PORCH_HOSTS_FILE");
    std::env::set_var("PORCH_SYNC_HOSTS", "0");
    v6server.abort();
    server.abort();
    std::fs::remove_dir_all(dir).unwrap();
}
