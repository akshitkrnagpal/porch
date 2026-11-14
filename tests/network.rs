mod common;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Full};
use hyper::{header, Request};
use hyper_util::rt::{TokioExecutor, TokioIo};
use porch::state::{ProxySettings, Route};
use std::{sync::Arc, time::Duration};
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
    for cert in porch::pem::certs(&mut std::io::BufReader::new(raw.as_slice())) {
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
    std::env::set_var("PORCH_CLEAN_URLS", "0");
    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up = upstream.local_addr().unwrap().port();
    let first_seen = Arc::new(tokio::sync::Notify::new());
    let sse_first_seen = first_seen.clone();
    let slow_started = Arc::new(tokio::sync::Notify::new());
    let slow_release = Arc::new(tokio::sync::Notify::new());
    let (started, release) = (slow_started.clone(), slow_release.clone());
    let app = axum::Router::new()
        .route("/", axum::routing::get(|| async { "hello" }))
        .route("/cookie", axum::routing::get(|h: header::HeaderMap| async move {
            h.get_all("cookie").iter().map(|v| v.to_str().unwrap()).collect::<Vec<_>>().join("|")
        }))
        .route("/slow", axum::routing::get(move || { let (started, release) = (started.clone(), release.clone()); async move {
            started.notify_one();
            release.notified().await;
            "slow done"
        }}))
        .route("/headers", axum::routing::get(|h:header::HeaderMap| async move {
            serde_json::json!({"host":h.get("host").unwrap().to_str().unwrap(),"proto":h.get("x-forwarded-proto").unwrap().to_str().unwrap(),"hops":h.get("x-portless-hops").unwrap().to_str().unwrap(),"porch_hops":h.get("x-porch-hops").unwrap().to_str().unwrap(),"port":h.get("x-forwarded-port").unwrap().to_str().unwrap()}).to_string()
        }))
        .route("/sse", axum::routing::get(move || { let first_seen = sse_first_seen.clone(); async move {
            // The second event waits for the client to confirm the first, so
            // the first can only have arrived if the proxy streamed it.
            let events=futures_util::stream::unfold(0,move |n| { let first_seen=first_seen.clone(); async move {
                if n>=2 { return None; }
                if n==1 { first_seen.notified().await; }
                Some((Ok::<_,std::convert::Infallible>(axum::response::sse::Event::default().data(if n==0 {"first"} else {"second"})), n+1))
            }});
            axum::response::Sse::new(events)
        }}))
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
        // Only the HTTPS proxy has a redirect listener (a test may claim
        // eight ports).
        let redirect_port = if tls { free_port() } else { 0 };
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
        // HTTP/1.1, so the Host-header routing checks below mean the same
        // thing over TLS; HTTP/2 is exercised separately further down.
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .http1_only()
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
        // The upstream holds the rest of the response until the test confirms
        // the first event, so the first event (and the headers before it) can
        // only arrive if the proxy streams. Every step is bounded so a
        // buffering proxy fails instead of hanging; a passing run never waits
        // out a bound.
        let bound = Duration::from_secs(30);
        let mut stream = tokio::time::timeout(bound, client.get(format!("{url}/sse")).send())
            .await
            .expect("response headers must stream before the body completes")
            .unwrap()
            .bytes_stream();
        // Chunks are not events: read up to each complete SSE frame.
        let mut buffer = String::new();
        let mut next_event = async |buffer: &mut String| -> String {
            loop {
                if let Some(end) = buffer.find("\n\n") {
                    let event = buffer[..end].to_owned();
                    buffer.drain(..end + 2);
                    return event;
                }
                let chunk = tokio::time::timeout(bound, stream.next())
                    .await
                    .expect("an event must stream before the response completes")
                    .expect("stream ended before the event")
                    .unwrap();
                buffer.push_str(&String::from_utf8_lossy(&chunk));
            }
        };
        assert_eq!(next_event(&mut buffer).await, "data: first");
        first_seen.notify_one();
        assert_eq!(next_event(&mut buffer).await, "data: second");
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
            // Browsers split cookies into separate fields over HTTP/2; the
            // HTTP/1.1 app gets them as one header (RFC 9113 8.2.3).
            let cookies = Request::builder()
                .uri(format!("https://app.localhost:{port}/cookie"))
                .header("cookie", "a=1")
                .header("cookie", "b=2")
                .body(Full::new(Bytes::new()))
                .unwrap();
            let cookies = sender.send_request(cookies).await.unwrap();
            assert_eq!(
                cookies.into_body().collect().await.unwrap().to_bytes(),
                "a=1; b=2"
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
            .http1_only()
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
    // The IPv4 side of the port stays bound but never listens, so the proxy's
    // 127.0.0.1 attempt is refused rather than reaching another process that
    // happens to listen on the same port number over IPv4.
    let (v6, v6port, _v4_refused) = loop {
        let v6 = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
        let port = v6.local_addr().unwrap().port();
        let v4 = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::STREAM,
            Some(socket2::Protocol::TCP),
        )
        .unwrap();
        if v4
            .bind(&std::net::SocketAddr::from(([127, 0, 0, 1], port)).into())
            .is_ok()
        {
            break (v6, port, v4);
        }
    };
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
            // What `porch alias v6 PORT` stores: localhost reaches ::1 too.
            target: format!("http://localhost:{v6port}"),
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
    // Stopping the proxy closes its listener at once and lets a request in
    // flight finish.
    let port = free_port();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let proxy = tokio::spawn(porch::proxy::serve_until(
        ProxySettings {
            port,
            tls: false,
            ..ProxySettings::default()
        },
        async move {
            let _ = stopped.await;
            anyhow::Ok(())
        },
    ));
    ready(port).await;
    let slow = tokio::spawn(
        client
            .get(format!("http://127.0.0.1:{port}/slow"))
            .header("host", "app.localhost")
            .send(),
    );
    tokio::time::timeout(Duration::from_secs(10), slow_started.notified())
        .await
        .expect("slow request reached the app");
    stop.send(()).unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .is_ok()
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "listener still open after stop"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!proxy.is_finished(), "stopped without draining");
    slow_release.notify_one();
    let r = tokio::time::timeout(Duration::from_secs(10), slow)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), "slow done");
    tokio::time::timeout(Duration::from_secs(10), proxy)
        .await
        .expect("proxy returned after draining")
        .unwrap()
        .unwrap();
    // A supplied certificate/key pair is served and verified without replacing the local CA.
    let pair = rcgen::generate_simple_self_signed(vec!["app.localhost".into()]).unwrap();
    let cert = dir.join("custom.pem");
    let key = dir.join("custom-key.pem");
    std::fs::write(&cert, pair.cert.pem()).unwrap();
    std::fs::write(&key, pair.key_pair.serialize_pem()).unwrap();
    // A port of its own: on Linux the stopped proxy's closed connections can
    // keep its port from being bound again for a while.
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
    std::env::set_var("PORCH_CLEAN_URLS", "0");
    v6server.abort();
    server.abort();
    std::fs::remove_dir_all(dir).unwrap();
}
