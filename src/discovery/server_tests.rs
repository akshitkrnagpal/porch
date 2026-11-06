use super::*;
fn temp() -> PathBuf {
    let path = std::env::temp_dir().join(format!("porch-discovery-server-{}", random().unwrap()));
    fs::create_dir_all(&path).unwrap();
    path
}
#[test]
fn explicit_private_bind_only() {
    for value in [
        "0.0.0.0",
        "::",
        "8.8.8.8",
        "224.0.0.1",
        "example.com",
        "2001:4860:4860::8888",
    ] {
        assert!(validate_bind(value).is_err(), "{value}");
    }
    for value in [
        "127.0.0.2",
        "192.168.1.3",
        "10.0.0.2",
        "100.64.0.1",
        "::1",
        "fd7a:115c:a1e0::1",
    ] {
        assert!(validate_bind(value).is_ok(), "{value}");
    }
}
#[test]
fn invitations_expire_and_are_consumed_and_bearers_can_be_revoked() {
    let dir = temp();
    let token = random().unwrap();
    let auth = Auth {
        invitation: Some(Invitation {
            hash: digest(&token),
            expires: now() + 60,
        }),
        clients: vec![],
    };
    write(
        &dir,
        "discovery-auth.json",
        &serde_json::to_vec(&auth).unwrap(),
    )
    .unwrap();
    assert!(enroll(&dir, "bad").unwrap().is_none());
    let bearer = enroll(&dir, &token).unwrap().unwrap();
    assert!(enroll(&dir, &token).unwrap().is_none());
    assert!(paired(&dir, &bearer).unwrap());
    assert!(!paired(&dir, &token).unwrap());
    let expired = Auth {
        invitation: Some(Invitation {
            hash: digest(&token),
            expires: now() - 1,
        }),
        clients: vec![],
    };
    write(
        &dir,
        "discovery-auth.json",
        &serde_json::to_vec(&expired).unwrap(),
    )
    .unwrap();
    assert!(!paired(&dir, &bearer).unwrap());
    assert!(enroll(&dir, &token).unwrap().is_none());
    let _ = fs::remove_dir_all(dir);
}
#[test]
fn catalog_keeps_identity_stable_and_reports_local_only_routes_without_leaking_targets() {
    let dir = temp();
    let m = machine(&dir).unwrap();
    assert_eq!(m.id, machine(&dir).unwrap().id);
    let data = State {
        routes: vec![Route {
            name: "web".into(),
            target: "127.0.0.1:9000".into(),
            pid: None,
        }],
        ..Default::default()
    };
    write(&dir, "state.json", &serde_json::to_vec(&data).unwrap()).unwrap();
    let local = catalog(&dir, m.clone(), false).unwrap();
    assert!(!local.apps[0].remote_access);
    assert!(local.apps[0]
        .unavailable_reason
        .as_ref()
        .unwrap()
        .contains("Local only"));
    let remote = catalog(&dir, m, true).unwrap();
    assert!(remote.apps[0].remote_access);
    assert_eq!(local.apps[0].id, remote.apps[0].id);
    assert!(!serde_json::to_string(&remote).unwrap().contains("9000"));
    let _ = fs::remove_dir_all(dir);
}
#[test]
fn certificate_and_pin_persist() {
    let dir = temp();
    ensure_certificate(&dir, "127.0.0.1").unwrap();
    let a = cert_der(&dir).unwrap();
    ensure_certificate(&dir, "127.0.0.2").unwrap();
    assert_eq!(a, cert_der(&dir).unwrap());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(dir.join("discovery-key.pem"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    let _ = fs::remove_dir_all(dir);
}

struct Fixture {
    dir: PathBuf,
    addr: SocketAddr,
    cert: rustls::pki_types::CertificateDer<'static>,
    task: tokio::task::JoinHandle<Result<()>>,
    app: tokio::task::JoinHandle<std::io::Result<()>>,
    machine: Machine,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
        self.app.abort();
        let _ = fs::remove_dir_all(&self.dir);
    }
}
async fn fixture(label: &'static str) -> Fixture {
    use axum::{
        extract::ws::{Message, WebSocketUpgrade},
        response::IntoResponse,
        routing::get,
    };
    let app_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let app_addr = app_listener.local_addr().unwrap();
    let router = axum::Router::new()
        .route(
            "/hello",
            get(move |headers: axum::http::HeaderMap| async move {
                format!(
                    "{label}|{}|{}",
                    headers.get("host").unwrap().to_str().unwrap(),
                    headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("none")
                )
            }),
        )
        .route(
            "/ws",
            get(|ws: WebSocketUpgrade| async move {
                ws.on_upgrade(|mut socket| async move {
                    while let Some(Ok(msg)) = socket.recv().await {
                        if let Message::Text(text) = msg {
                            let _ = socket.send(Message::Text(text)).await;
                        } else {
                            break;
                        }
                    }
                })
                .into_response()
            }),
        );
    let app = tokio::spawn(async move { axum::serve(app_listener, router).await });
    let dir = temp();
    let machine = machine(&dir).unwrap();
    let data = State {
        routes: vec![Route {
            name: "web".into(),
            target: format!("http://{app_addr}"),
            pid: None,
        }],
        ..Default::default()
    };
    write(&dir, "state.json", &serde_json::to_vec(&data).unwrap()).unwrap();
    ensure_certificate(&dir, "127.0.0.1").unwrap();
    let cert = cert_der(&dir).unwrap().remove(0);
    let bytes = fs::read(dir.join("discovery-key.pem")).unwrap();
    let key = rustls_pemfile::private_key(&mut bytes.as_slice())
        .unwrap()
        .unwrap();
    let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![cert.clone()], key)
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
    let context = Arc::new(ContextState {
        dir: dir.clone(),
        machine: machine.clone(),
        share_apps: true,
        client: upstream(&dir).unwrap(),
        tunnels: Arc::new(tokio::sync::Semaphore::new(64)),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(connections(
        listener,
        acceptor,
        context,
        Arc::new(tokio::sync::Semaphore::new(64)),
    ));
    Fixture {
        dir,
        addr,
        cert,
        task,
        app,
        machine,
    }
}
async fn tls_stream(f: &Fixture) -> tokio_rustls::client::TlsStream<tokio::net::TcpStream> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(f.cert.clone()).unwrap();
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(
            rustls::pki_types::ServerName::IpAddress(f.addr.ip().into()),
            tokio::net::TcpStream::connect(f.addr).await.unwrap(),
        )
        .await
        .unwrap()
}
async fn call(
    f: &Fixture,
    method: Method,
    path: &str,
    token: Option<&str>,
    origin: Option<&str>,
    body: String,
) -> (StatusCode, Bytes) {
    let stream = tls_stream(f).await;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("host", f.addr.to_string());
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    if let Some(origin) = origin {
        request = request.header("origin", origin);
    }
    let response = sender
        .send_request(request.body(Full::new(Bytes::from(body))).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, body)
}
async fn pair(f: &Fixture) -> String {
    let invitation = random().unwrap();
    write(
        &f.dir,
        "discovery-auth.json",
        &serde_json::to_vec(&Auth {
            invitation: Some(Invitation {
                hash: digest(&invitation),
                expires: now() + 60,
            }),
            clients: vec![],
        })
        .unwrap(),
    )
    .unwrap();
    let (status, body) = call(
        f,
        Method::POST,
        "/v1/pair",
        None,
        None,
        serde_json::to_string(&PairRequest {
            token: invitation.clone(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let pair: PairResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(pair.machine.id, f.machine.id);
    let (status, _) = call(
        f,
        Method::POST,
        "/v1/pair",
        None,
        None,
        serde_json::to_string(&PairRequest { token: invitation }).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    pair.token
}
#[tokio::test]
async fn real_tls_pair_auth_origin_limits_and_two_distinct_nodes_route_to_correct_app() {
    let a = fixture("machine-a").await;
    let b = fixture("machine-b").await;
    let (status, body) = call(&a, Method::GET, "/v1/catalog", None, None, String::new()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(!String::from_utf8_lossy(&body).contains("web"));
    let (status, _) = call(
        &a,
        Method::POST,
        "/v1/pair",
        None,
        Some("https://evil.example"),
        "{}".into(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(&a, Method::POST, "/v1/pair", None, None, "X".repeat(2048)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let bearer_a = pair(&a).await;
    let bearer_b = pair(&b).await;
    let (status, body) = call(
        &a,
        Method::GET,
        "/v1/catalog",
        Some(&bearer_a),
        None,
        String::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let catalog_a: Catalog = serde_json::from_slice(&body).unwrap();
    let (status, body) = call(
        &b,
        Method::GET,
        "/v1/catalog",
        Some(&bearer_b),
        None,
        String::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let catalog_b: Catalog = serde_json::from_slice(&body).unwrap();
    assert_ne!(catalog_a.machine.id, catalog_b.machine.id);
    assert_ne!(catalog_a.apps[0].id, catalog_b.apps[0].id);
    for (fixture, catalog, bearer, label) in [
        (&a, &catalog_a, &bearer_a, "machine-a"),
        (&b, &catalog_b, &bearer_b, "machine-b"),
    ] {
        let path = format!("/v1/apps/{}/proxy/hello", catalog.apps[0].id);
        let (status, body) = call(
            fixture,
            Method::GET,
            &path,
            Some(bearer),
            None,
            String::new(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, format!("{label}|web.localhost:7462|none"));
    }
    let (status, _) = call(
        &b,
        Method::GET,
        &format!("/v1/apps/{}/proxy/hello", catalog_a.apps[0].id),
        Some(&bearer_b),
        None,
        String::new(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &a,
        Method::GET,
        "/v1/catalog",
        Some(&bearer_b),
        None,
        String::new(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
#[tokio::test]
async fn authenticated_remote_websocket_uses_registered_route() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
    let f = fixture("ws-machine").await;
    let bearer = pair(&f).await;
    let id = route_id(&f.machine, "web");
    let mut request = format!("wss://{}/v1/apps/{id}/proxy/ws", f.addr)
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {bearer}").parse().unwrap());
    let (mut socket, _) = tokio_tungstenite::client_async(request, tls_stream(&f).await)
        .await
        .unwrap();
    socket
        .send(Message::Text("remote-echo".into()))
        .await
        .unwrap();
    let message = tokio::time::timeout(Duration::from_secs(3), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(message, Message::Text("remote-echo".into()));
}
