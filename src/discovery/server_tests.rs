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
        ..Default::default()
    };
    write(
        &dir,
        "discovery-auth.json",
        &serde_json::to_vec(&auth).unwrap(),
    )
    .unwrap();
    assert!(enroll(&dir, "bad", None).unwrap().is_none());
    let bearer = enroll(&dir, &token, None).unwrap().unwrap();
    assert!(enroll(&dir, &token, None).unwrap().is_none());
    assert!(paired(&dir, &bearer).unwrap());
    assert!(!paired(&dir, &token).unwrap());
    let expired = Auth {
        invitation: Some(Invitation {
            hash: digest(&token),
            expires: now() - 1,
        }),
        clients: vec![],
        ..Default::default()
    };
    write(
        &dir,
        "discovery-auth.json",
        &serde_json::to_vec(&expired).unwrap(),
    )
    .unwrap();
    assert!(!paired(&dir, &bearer).unwrap());
    assert!(enroll(&dir, &token, None).unwrap().is_none());
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
    let local = catalog(&dir, m.clone(), false, true).unwrap();
    assert!(!local.apps[0].remote_access);
    assert!(local.apps[0]
        .unavailable_reason
        .as_ref()
        .unwrap()
        .contains("Local only"));
    let remote = catalog(&dir, m, true, true).unwrap();
    assert!(remote.apps[0].remote_access);
    assert_eq!(local.apps[0].id, remote.apps[0].id);
    // Only the apps can carry a route's target. The whole catalog also holds
    // a timestamp and hashed ids, where the digits "9000" turn up by chance.
    let apps = serde_json::to_string(&remote.apps).unwrap();
    assert!(!apps.contains("127.0.0.1"), "{apps}");
    assert!(!apps.contains(":9000"), "{apps}");
    let _ = fs::remove_dir_all(dir);
}
#[test]
fn private_routes_and_remote_targets_are_never_offered_to_peers() {
    let dir = temp();
    let m = machine(&dir).unwrap();
    let route = |name: &str, target: &str| Route {
        name: name.into(),
        target: target.into(),
        pid: None,
    };
    let data = State {
        routes: vec![
            route("web", "http://127.0.0.1:9000"),
            route("secret", "http://127.0.0.1:9001"),
            route("router", "http://192.168.1.1"),
            route("metadata", "http://169.254.169.254"),
        ],
        private: ["secret".to_owned()].into_iter().collect(),
        ..Default::default()
    };
    write(&dir, "state.json", &serde_json::to_vec(&data).unwrap()).unwrap();
    let apps = catalog(&dir, m, true, true).unwrap().apps;
    let names: Vec<_> = apps.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["web", "router", "metadata"]);
    // The local catalog (tray, `porch open`) still lists the private app, but
    // never as remotely reachable.
    let local = catalog(&dir, machine(&dir).unwrap(), true, false)
        .unwrap()
        .apps;
    let secret = local.iter().find(|a| a.name == "secret").unwrap();
    assert!(!secret.remote_access);
    assert!(secret
        .unavailable_reason
        .as_ref()
        .unwrap()
        .contains("Private"));
    assert!(apps[0].remote_access);
    for app in &apps[1..] {
        assert!(!app.remote_access, "{}", app.name);
        assert!(app
            .unavailable_reason
            .as_ref()
            .unwrap()
            .contains("on this machine"));
    }
    let _ = fs::remove_dir_all(dir);
}
#[test]
fn catalog_health_matches_porch_list() {
    let dir = temp();
    let m = machine(&dir).unwrap();
    let up = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let up_port = up.local_addr().unwrap().port();
    // Held, never listening: the health probe is refused and cannot reach a
    // parallel test's listener on a reused ephemeral port.
    let (_down, down_port) = crate::test_support::refused_port();
    let route = |name: &str, port: u16| Route {
        name: name.into(),
        target: format!("http://127.0.0.1:{port}"),
        pid: None,
    };
    let data = State {
        routes: vec![route("web", up_port), route("db", down_port)],
        ..Default::default()
    };
    write(&dir, "state.json", &serde_json::to_vec(&data).unwrap()).unwrap();
    let apps = catalog(&dir, m, true, true).unwrap().apps;
    // A listening app is healthy, not "status unknown" in the menu.
    assert_eq!(apps[0].health, Health::Healthy);
    assert_eq!(apps[1].health, Health::Unreachable);
    drop(up);
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
            "/go",
            get(|| async {
                (
                    axum::http::StatusCode::FOUND,
                    [("location", "http://api.internal:8080/login?next=1")],
                )
            }),
        )
        .route(
            "/upload",
            axum::routing::post(|body: axum::body::Body| async move {
                axum::body::to_bytes(body, usize::MAX)
                    .await
                    .map(|b| b.len().to_string())
                    .unwrap_or_default()
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
    let key = crate::pem::private_key(&mut bytes.as_slice())
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
        loopback: crate::upstream::client(tls_roots(&dir).unwrap()),
        loopback_insecure: crate::upstream::client(crate::upstream::insecure_tls().unwrap()),
        tunnels: Arc::new(tokio::sync::Semaphore::new(64)),
        attempts: Arc::default(),
        sessions: Arc::default(),
        catalog: Arc::default(),
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
            ..Default::default()
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
            machine: None,
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
        serde_json::to_string(&PairRequest {
            token: invitation,
            machine: None,
        })
        .unwrap(),
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

fn joiner(name: &str) -> Machine {
    Machine {
        id: format!("{name}-id"),
        name: name.into(),
    }
}
fn progress(dir: &Path, id: &str) -> String {
    let p = pairing_progress_in(dir, id).unwrap();
    match (p.state, p.peer) {
        (PairState::Waiting, _) => "waiting".into(),
        (PairState::Accepted, Some(peer)) => format!("accepted:{}", peer.name),
        (PairState::Paired, Some(peer)) => format!("paired:{}", peer.name),
        (state, _) => format!(
            "{}:{}",
            serde_json::to_value(state).unwrap().as_str().unwrap(),
            p.reason.unwrap_or_default()
        ),
    }
}
/// What the key exchange does with a joiner's `code`: a wrong one fails the
/// key confirmation (counted), a right one enrolls through `code_accepted`.
enum Tried {
    Accepted(String),
    Wrong,
    Closed,
    Full,
}
fn try_code(dir: &Path, code: &str, peer: &Machine) -> Tried {
    let window = read::<Auth>(&dir.join("discovery-auth.json"))
        .unwrap()
        .and_then(|a| a.pairing);
    let Some(w) = window else {
        return Tried::Closed;
    };
    if open_window(dir).is_some() && super::super::pake::password(&w.salt, code) != w.hash {
        code_failed(dir, &w.salt).unwrap();
        return Tried::Wrong;
    }
    match code_accepted(dir, &w.salt, peer, true).unwrap() {
        CodeOutcome::Accepted(b) => Tried::Accepted(b),
        CodeOutcome::Closed => Tried::Closed,
        CodeOutcome::Full => Tried::Full,
    }
}
#[test]
fn pairing_code_is_single_use_and_labels_the_new_credential() {
    let dir = temp();
    let opened = open_pairing_in(&dir, PAIR_CODE_LIFETIME).unwrap();
    assert_eq!(opened.code.len(), 6);
    assert!(opened.code.bytes().all(|b| b.is_ascii_digit()));
    assert!(pairing_open(&dir));
    assert_eq!(progress(&dir, &opened.id), "waiting");
    let raw = fs::read_to_string(dir.join("discovery-auth.json")).unwrap();
    assert!(
        !raw.contains(&format!("\"{}\"", opened.code)),
        "code stored in clear"
    );
    let Tried::Accepted(bearer) = try_code(&dir, &opened.code, &joiner("laptop")) else {
        panic!("correct code rejected")
    };
    assert!(paired(&dir, &bearer).unwrap());
    // Accepted is not paired: the joiner has not proved it stored anything.
    assert_eq!(progress(&dir, &opened.id), "accepted:laptop");
    assert!(!pairing_open(&dir));
    assert!(!authorize(&dir, &"f".repeat(64)).unwrap());
    assert_eq!(progress(&dir, &opened.id), "accepted:laptop");
    assert!(authorize(&dir, &bearer).unwrap());
    assert_eq!(progress(&dir, &opened.id), "paired:laptop");
    assert!(matches!(
        try_code(&dir, &opened.code, &joiner("laptop")),
        Tried::Closed
    ));
    // Re-pairing the same machine replaces its old credential.
    let again = open_pairing_in(&dir, PAIR_CODE_LIFETIME).unwrap();
    let Tried::Accepted(second) = try_code(&dir, &again.code, &joiner("laptop")) else {
        panic!("re-pair rejected")
    };
    assert!(!paired(&dir, &bearer).unwrap());
    assert!(paired(&dir, &second).unwrap());
    revoke_where(&dir, |_, m| m == Some("laptop-id")).unwrap();
    assert!(!paired(&dir, &second).unwrap());
    let _ = fs::remove_dir_all(dir);
}
#[test]
fn five_wrong_codes_close_the_window_even_for_the_right_code() {
    let dir = temp();
    let opened = open_pairing_in(&dir, PAIR_CODE_LIFETIME).unwrap();
    let wrong = if opened.code == "000000" {
        "111111"
    } else {
        "000000"
    };
    for attempt in 1..=PAIR_CODE_MAX_FAILURES {
        assert!(
            matches!(try_code(&dir, wrong, &joiner("attacker")), Tried::Wrong),
            "attempt {attempt}"
        );
        assert_eq!(pairing_open(&dir), attempt < PAIR_CODE_MAX_FAILURES);
    }
    assert!(matches!(
        try_code(&dir, &opened.code, &joiner("laptop")),
        Tried::Closed
    ));
    assert_eq!(progress(&dir, &opened.id), "cancelled:too_many_failures");
    let auth: Auth = read(&dir.join("discovery-auth.json")).unwrap().unwrap();
    assert!(auth.clients.is_empty());
    let _ = fs::remove_dir_all(dir);
}
#[test]
fn expired_code_is_refused() {
    let dir = temp();
    let opened = open_pairing_in(&dir, 0).unwrap();
    assert!(!pairing_open(&dir));
    assert!(matches!(
        try_code(&dir, &opened.code, &joiner("laptop")),
        Tried::Closed
    ));
    assert_eq!(progress(&dir, &opened.id), "expired:");
    let _ = fs::remove_dir_all(dir);
}
#[test]
fn replacing_and_cancelling_codes_are_reported_and_idempotent() {
    let dir = temp();
    let first = open_pairing_in(&dir, PAIR_CODE_LIFETIME).unwrap();
    let second = open_pairing_in(&dir, PAIR_CODE_LIFETIME).unwrap();
    assert_eq!(progress(&dir, &first.id), "replaced:");
    assert_eq!(progress(&dir, &second.id), "waiting");
    let cancelled = close_pairing_in(&dir, &second.id).unwrap();
    assert_eq!(cancelled.state, PairState::Cancelled);
    assert_eq!(cancelled.reason.as_deref(), Some("user"));
    assert!(!pairing_open(&dir));
    // Cancelling again, or an unknown ID, changes nothing.
    assert_eq!(close_pairing_in(&dir, &second.id).unwrap(), cancelled);
    assert_eq!(
        close_pairing_in(&dir, "unknown").unwrap().state,
        PairState::Replaced
    );
    // Cancelling a code that already paired keeps the pairing.
    let third = open_pairing_in(&dir, PAIR_CODE_LIFETIME).unwrap();
    let Tried::Accepted(bearer) = try_code(&dir, &third.code, &joiner("laptop")) else {
        panic!("correct code rejected")
    };
    assert_eq!(
        close_pairing_in(&dir, &third.id).unwrap().state,
        PairState::Accepted
    );
    assert!(paired(&dir, &bearer).unwrap());
    let _ = fs::remove_dir_all(dir);
}
#[test]
fn results_written_by_older_agents_still_read() {
    let dir = temp();
    let legacy = |paired: Option<&str>, error: Option<&str>| {
        let auth = serde_json::json!({
            "clients": [], "pairing_result": {"id": "old", "paired": paired, "error": error}
        });
        fs::write(dir.join("discovery-auth.json"), auth.to_string()).unwrap();
        progress(&dir, "old")
    };
    assert_eq!(legacy(Some("laptop"), None), "paired:laptop");
    assert_eq!(
        legacy(None, Some("too many wrong pairing codes; …")),
        "cancelled:too_many_failures"
    );
    assert_eq!(legacy(None, Some("pairing code expired")), "expired:");
    let _ = fs::remove_dir_all(dir);
}
#[test]
fn a_full_host_cancels_the_code_without_enrolling() {
    let dir = temp();
    let mut auth = Auth::default();
    for i in 0..MAX_CLIENTS {
        auth.clients.push(digest(&format!("client-{i}")));
    }
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("discovery-auth.json"),
        serde_json::to_vec(&auth).unwrap(),
    )
    .unwrap();
    let opened = open_pairing_in(&dir, PAIR_CODE_LIFETIME).unwrap();
    assert!(matches!(
        try_code(&dir, &opened.code, &joiner("laptop")),
        Tried::Full
    ));
    assert_eq!(progress(&dir, &opened.id), "cancelled:host_full");
    let auth: Auth = read(&dir.join("discovery-auth.json")).unwrap().unwrap();
    assert_eq!(auth.clients.len(), MAX_CLIENTS);
    // Storing the joiner can also fail after acceptance; that takes it back.
    auth_reset(&dir);
    let opened = open_pairing_in(&dir, PAIR_CODE_LIFETIME).unwrap();
    let Tried::Accepted(bearer) = try_code(&dir, &opened.code, &joiner("laptop")) else {
        panic!("correct code rejected")
    };
    reject_accepted(&dir, &bearer).unwrap();
    assert!(!paired(&dir, &bearer).unwrap());
    assert_eq!(progress(&dir, &opened.id), "cancelled:host_full");
    let _ = fs::remove_dir_all(dir);
}
fn auth_reset(dir: &Path) {
    fs::write(dir.join("discovery-auth.json"), b"{}").unwrap();
}
fn window(dir: &Path) -> PairingWindow {
    read::<Auth>(&dir.join("discovery-auth.json"))
        .unwrap()
        .unwrap()
        .pairing
        .unwrap()
}
#[test]
fn a_proof_for_an_old_window_does_not_open_a_new_one() {
    let dir = temp();
    let first = open_pairing_in(&dir, PAIR_CODE_LIFETIME).unwrap();
    let old = window(&dir);
    assert_ne!(
        old.salt, first.id,
        "the public salt is not the private window id"
    );
    assert_eq!(
        old.hash,
        super::super::pake::password(&old.salt, &first.code)
    );
    open_pairing_in(&dir, PAIR_CODE_LIFETIME).unwrap();
    assert!(matches!(
        code_accepted(&dir, &old.salt, &joiner("laptop"), true).unwrap(),
        CodeOutcome::Closed
    ));
    code_failed(&dir, &old.salt).unwrap();
    assert_eq!(window(&dir).failures, 0);
    assert!(pairing_open(&dir));
    let _ = fs::remove_dir_all(dir);
}
#[test]
fn windows_from_the_old_protocol_are_closed() {
    let dir = temp();
    open_pairing_in(&dir, PAIR_CODE_LIFETIME).unwrap();
    let mut auth: Auth = read(&dir.join("discovery-auth.json")).unwrap().unwrap();
    auth.pairing.as_mut().unwrap().salt = String::new();
    write(
        &dir,
        "discovery-auth.json",
        &serde_json::to_vec(&auth).unwrap(),
    )
    .unwrap();
    assert!(!pairing_open(&dir));
    let _ = fs::remove_dir_all(dir);
}

/// A joiner (honest or not) driving the three-step exchange over real TLS.
struct Exchange {
    session: String,
    key: super::super::pake::Key,
    host_msg: String,
}
async fn start(f: &Fixture, code: &str) -> (StatusCode, Option<Exchange>) {
    use super::super::pake;
    let salt = read::<Auth>(&f.dir.join("discovery-auth.json"))
        .unwrap()
        .and_then(|a| a.pairing)
        .map(|w| w.salt)
        .unwrap_or_default();
    let (state, msg) = pake::joiner_start(&pake::password(&salt, code));
    let (status, body) = call(
        f,
        Method::POST,
        "/v1/pair-code/start",
        None,
        None,
        serde_json::to_string(&PairStart {
            pairing: PAIRING_VERSION,
            salt,
            msg,
        })
        .unwrap(),
    )
    .await;
    if status != StatusCode::OK {
        return (status, None);
    }
    let started: PairStarted = serde_json::from_slice(&body).unwrap();
    let key = pake::joiner_finish(state, &started.msg).unwrap();
    (
        status,
        Some(Exchange {
            session: started.session,
            key,
            host_msg: started.msg,
        }),
    )
}
async fn confirm(f: &Fixture, x: &Exchange, pin: &[u8; 32]) -> (StatusCode, Bytes) {
    call(
        f,
        Method::POST,
        "/v1/pair-code/confirm",
        None,
        None,
        serde_json::to_string(&PairConfirm {
            session: x.session.clone(),
            confirm: x.key.joiner_confirm(pin),
        })
        .unwrap(),
    )
    .await
}
fn finish_body(x: &Exchange, token: &str, mac: Option<String>) -> String {
    let machine = joiner("laptop");
    let endpoint = "https://127.0.0.1:7463/";
    let fingerprint = "ab".repeat(32);
    serde_json::to_string(&PairFinish {
        session: x.session.clone(),
        mac: mac.unwrap_or_else(|| {
            x.key
                .joiner_payload(&machine, endpoint, &fingerprint, token)
        }),
        machine,
        endpoint: endpoint.into(),
        fingerprint,
        token: token.into(),
    })
    .unwrap()
}
async fn finish(f: &Fixture, x: &Exchange, mac: Option<String>) -> (StatusCode, Bytes) {
    call(
        f,
        Method::POST,
        "/v1/pair-code/finish",
        None,
        None,
        finish_body(x, &"c".repeat(64), mac),
    )
    .await
}
fn host_pin(f: &Fixture) -> [u8; 32] {
    super::super::client::normalize_pin(&fingerprint_in(&f.dir).unwrap()).unwrap()
}
fn error_code(body: &Bytes) -> String {
    serde_json::from_slice::<PairCodeError>(body)
        .unwrap()
        .error
        .code
}
fn no_clients(f: &Fixture) -> bool {
    read::<Auth>(&f.dir.join("discovery-auth.json"))
        .unwrap()
        .unwrap_or_default()
        .clients
        .is_empty()
}

#[tokio::test]
async fn real_tls_code_exchange_is_mutual_single_use_and_stores_joiner() {
    let f = fixture("host").await;
    let status = |body: Bytes| serde_json::from_slice::<PairingStatus>(&body).unwrap();
    let (code, body) = call(&f, Method::GET, "/v1/pairing", None, None, String::new()).await;
    assert_eq!(code, StatusCode::OK);
    let closed = status(body);
    assert!(!closed.open && closed.salt.is_none());
    assert_eq!(closed.pairing, PAIRING_VERSION);
    let opened = open_pairing_in(&f.dir, PAIR_CODE_LIFETIME).unwrap();
    let (_, body) = call(&f, Method::GET, "/v1/pairing", None, None, String::new()).await;
    let shown = status(body);
    assert!(shown.open);
    assert_eq!(shown.salt.as_deref(), Some(window(&f.dir).salt.as_str()));
    let pin = host_pin(&f);

    let (code, x) = start(&f, &opened.code).await;
    assert_eq!(code, StatusCode::OK);
    let x = x.unwrap();
    // Only a hex group element crosses the wire (a substring check for the
    // code would fail by chance on random hex).
    assert_eq!(x.host_msg.len(), 66, "{}", x.host_msg);
    assert!(x.host_msg.bytes().all(|b| b.is_ascii_hexdigit()));
    // Credentials cannot skip the joiner's proof.
    let (code, _) = finish(&f, &x, None).await;
    assert_eq!(code, StatusCode::GONE, "finish before confirm");
    let (code, x) = start(&f, &opened.code).await;
    assert_eq!(code, StatusCode::OK);
    let x = x.unwrap();
    let (code, body) = confirm(&f, &x, &pin).await;
    assert_eq!(code, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(progress(&f.dir, &opened.id), "waiting");
    let confirmed: PairConfirmed = serde_json::from_slice(&body).unwrap();
    assert!(
        x.key.check_host_confirm(&pin, &confirmed.confirm),
        "host proves the code"
    );
    assert!(
        no_clients(&f),
        "nothing enrolled before the joiner's credentials arrive"
    );
    let (code, body) = finish(&f, &x, None).await;
    assert_eq!(code, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let response: PairFinished = serde_json::from_slice(&body).unwrap();
    assert_eq!(response.machine.id, f.machine.id);
    // Accepted is not paired until the joiner's first authenticated request.
    assert_eq!(progress(&f.dir, &opened.id), "accepted:laptop");
    assert!(x
        .key
        .check_host_payload(&response.machine, &response.token, &response.mac));
    let (code, _) = call(
        &f,
        Method::GET,
        "/v1/catalog",
        Some(&response.token),
        None,
        String::new(),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    // Mutual: the host stored the joiner with the credentials it supplied.
    let stored = super::super::client::peers_in(&f.dir).unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].machine.name, "laptop");
    assert_eq!(stored[0].token, "c".repeat(64));
    assert_eq!(stored[0].slug, "laptop");
    assert_eq!(progress(&f.dir, &opened.id), "paired:laptop");
    // Single use: the session and the window are gone.
    let (code, _) = finish(&f, &x, None).await;
    assert_eq!(code, StatusCode::GONE, "session replay");
    let (code, _) = start(&f, &opened.code).await;
    assert_eq!(code, StatusCode::GONE, "code is single-use");
    let (code, body) = call(
        &f,
        Method::POST,
        "/v1/pair-code/start",
        None,
        None,
        serde_json::to_string(&PairStart {
            pairing: PAIRING_VERSION,
            salt: "x".repeat(64),
            msg: "00".repeat(33),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(code, StatusCode::GONE);
    assert_eq!(error_code(&body), "not_waiting");
}

#[tokio::test]
async fn real_tls_wrong_code_interceptor_and_tampering_never_release_credentials() {
    let f = fixture("host").await;
    let opened = open_pairing_in(&f.dir, PAIR_CODE_LIFETIME).unwrap();
    let wrong = if opened.code == "000000" {
        "111111"
    } else {
        "000000"
    };
    let pin = host_pin(&f);
    // The version 1 endpoint is refused without reading the code or the window.
    let (code, body) = call(
        &f,
        Method::POST,
        "/v1/pair-code",
        None,
        None,
        format!(r#"{{"version":1,"code":"{}","host_fingerprint":"{}","machine":{{"id":"x","name":"x"}},"endpoint":"https://127.0.0.1:1/","fingerprint":"{}","token":"{}"}}"#,
            opened.code, hex(&pin), "ab".repeat(32), "c".repeat(64)),
    )
    .await;
    assert_eq!(code, StatusCode::UPGRADE_REQUIRED);
    assert_eq!(error_code(&body), "protocol");
    assert_eq!(window(&f.dir).failures, 0);

    // Wrong code: the host answers the exchange but never proves anything and
    // counts the failure.
    let (_, x) = start(&f, wrong).await;
    let x = x.unwrap();
    let (code, body) = confirm(&f, &x, &pin).await;
    assert_eq!(code, StatusCode::UNAUTHORIZED);
    assert_eq!(error_code(&body), "wrong_code");
    assert_eq!(window(&f.dir).failures, 1);
    let (code, _) = finish(&f, &x, None).await;
    assert_eq!(code, StatusCode::GONE);

    // Right code, but bound to another certificate (a relaying TLS interceptor).
    let (_, x) = start(&f, &opened.code).await;
    let (code, _) = confirm(&f, &x.unwrap(), &[0u8; 32]).await;
    assert_eq!(code, StatusCode::UNAUTHORIZED);
    assert_eq!(window(&f.dir).failures, 2);

    // A replayed confirmation ends the session.
    let (_, x) = start(&f, &opened.code).await;
    let x = x.unwrap();
    assert_eq!(confirm(&f, &x, &pin).await.0, StatusCode::OK);
    assert_eq!(confirm(&f, &x, &pin).await.0, StatusCode::GONE);
    assert_eq!(finish(&f, &x, None).await.0, StatusCode::GONE);
    // A tampered payload (here: a MAC from another key) is refused without
    // enrolling anyone.
    let (_, x) = start(&f, &opened.code).await;
    let x = x.unwrap();
    assert_eq!(confirm(&f, &x, &pin).await.0, StatusCode::OK);
    let (_, other) = start(&f, &opened.code).await;
    let forged = other.unwrap().key.joiner_payload(
        &joiner("laptop"),
        "https://127.0.0.1:7463/",
        &"ab".repeat(32),
        &"c".repeat(64),
    );
    let (code, _) = finish(&f, &x, Some(forged)).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert!(no_clients(&f));
    assert!(super::super::client::peers_in(&f.dir).unwrap().is_empty());
    assert!(pairing_open(&f.dir));

    // Three more failures close the window for everyone (5 in total).
    for _ in 0..3 {
        let (_, x) = start(&f, wrong).await;
        assert_eq!(
            confirm(&f, &x.unwrap(), &pin).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert!(!pairing_open(&f.dir));
    assert_eq!(progress(&f.dir, &opened.id), "cancelled:too_many_failures");
    let (code, _) = call(
        &f,
        Method::POST,
        "/v1/pair-code/start",
        None,
        None,
        serde_json::to_string(&PairStart {
            pairing: PAIRING_VERSION,
            salt: String::new(),
            msg: "00".repeat(33),
        })
        .unwrap(),
    )
    .await;
    assert!(
        code == StatusCode::GONE || code == StatusCode::TOO_MANY_REQUESTS,
        "{code}"
    );
    assert!(no_clients(&f));
}

#[tokio::test]
async fn real_tls_pairing_is_rate_limited_per_source() {
    let f = fixture("host").await;
    open_pairing_in(&f.dir, PAIR_CODE_LIFETIME).unwrap();
    let mut limited = false;
    for _ in 0..12 {
        let (code, body) = call(
            &f,
            Method::POST,
            "/v1/pair-code/start",
            None,
            None,
            serde_json::to_string(&PairStart {
                pairing: PAIRING_VERSION,
                salt: window(&f.dir).salt,
                msg: "00".repeat(33),
            })
            .unwrap(),
        )
        .await;
        if code == StatusCode::TOO_MANY_REQUESTS {
            assert_eq!(error_code(&body), "rate_limited");
            limited = true;
        }
    }
    assert!(limited);
}

#[tokio::test]
async fn real_tls_window_changes_mid_exchange_and_racing_finishes_enroll_at_most_once() {
    let f = fixture("host").await;
    let pin = host_pin(&f);
    // Replaced between start and confirm: the old code's proof opens nothing.
    let first = open_pairing_in(&f.dir, PAIR_CODE_LIFETIME).unwrap();
    let (_, x) = start(&f, &first.code).await;
    let second = open_pairing_in(&f.dir, PAIR_CODE_LIFETIME).unwrap();
    assert_eq!(confirm(&f, &x.unwrap(), &pin).await.0, StatusCode::GONE);
    assert_eq!(
        window(&f.dir).failures,
        0,
        "a stale proof is not a wrong guess"
    );
    // Expired between confirm and finish: no credential is issued.
    let (_, x) = start(&f, &second.code).await;
    let x = x.unwrap();
    assert_eq!(confirm(&f, &x, &pin).await.0, StatusCode::OK);
    let mut auth: Auth = read(&f.dir.join("discovery-auth.json")).unwrap().unwrap();
    auth.pairing.as_mut().unwrap().expires = now() - 1;
    write(
        &f.dir,
        "discovery-auth.json",
        &serde_json::to_vec(&auth).unwrap(),
    )
    .unwrap();
    assert_eq!(finish(&f, &x, None).await.0, StatusCode::GONE);
    assert!(no_clients(&f));
    assert!(progress(&f.dir, &second.id).contains("expired"));
    // Two confirmed exchanges racing to finish: exactly one enrolls.
    let third = open_pairing_in(&f.dir, PAIR_CODE_LIFETIME).unwrap();
    let (_, a) = start(&f, &third.code).await;
    let (_, b) = start(&f, &third.code).await;
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!(confirm(&f, &a, &pin).await.0, StatusCode::OK);
    assert_eq!(confirm(&f, &b, &pin).await.0, StatusCode::OK);
    let (ra, rb) = tokio::join!(finish(&f, &a, None), finish(&f, &b, None));
    let mut codes = [ra.0, rb.0];
    codes.sort();
    assert_eq!(codes, [StatusCode::OK, StatusCode::GONE]);
    let auth: Auth = read(&f.dir.join("discovery-auth.json")).unwrap().unwrap();
    assert_eq!(auth.clients.len(), 1);
}

#[tokio::test]
async fn paired_access_honors_an_aliases_upstream_settings() {
    let a = fixture("machine-a").await;
    let mut data: State = read(&a.dir.join("state.json")).unwrap().unwrap();
    data.upstreams.insert(
        "web".into(),
        crate::state::Upstream {
            tls: false,
            authority: Some("api.internal:8080".into()),
            insecure: false,
        },
    );
    write(&a.dir, "state.json", &serde_json::to_vec(&data).unwrap()).unwrap();
    let bearer = pair(&a).await;
    let (_, body) = call(
        &a,
        Method::GET,
        "/v1/catalog",
        Some(&bearer),
        None,
        String::new(),
    )
    .await;
    let catalog: Catalog = serde_json::from_slice(&body).unwrap();
    let id = &catalog.apps[0].id;
    // The backend sees the alias's own Host, as it does through the local proxy.
    let (status, body) = call(
        &a,
        Method::GET,
        &format!("/v1/apps/{id}/proxy/hello"),
        Some(&bearer),
        None,
        String::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "machine-a|api.internal:8080|none");
    // Its redirects to itself come back as the app's Porch URL.
    let stream = tls_stream(&a).await;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let response = sender
        .send_request(
            Request::builder()
                .uri(format!("/v1/apps/{id}/proxy/go"))
                .header("host", a.addr.to_string())
                .header("authorization", format!("Bearer {bearer}"))
                .body(http_body_util::Empty::<Bytes>::new())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(
        response.headers()["location"],
        "http://web.localhost:7462/login?next=1"
    );
}

/// Per-request checks read state files through a cache keyed on each file's
/// version, so revoking a credential, turning sharing off and registering an
/// app all apply to the very next request.
#[tokio::test]
async fn cached_state_still_applies_revocation_sharing_and_new_apps_at_once() {
    let f = fixture("machine-cache").await;
    let bearer = pair(&f).await;
    let catalog = |bearer: String| {
        let f = &f;
        async move {
            call(
                f,
                Method::GET,
                "/v1/catalog",
                Some(&bearer),
                None,
                String::new(),
            )
            .await
        }
    };
    let (status, body) = catalog(bearer.clone()).await;
    assert_eq!(status, StatusCode::OK);
    let first: Catalog = serde_json::from_slice(&body).unwrap();
    assert_eq!(first.apps.len(), 1);
    // A newly registered app is listed at once, not after the reuse window.
    let mut data: State = read(&f.dir.join("state.json")).unwrap().unwrap();
    data.routes.push(Route {
        name: "api".into(),
        target: "127.0.0.1:9".into(),
        pid: None,
    });
    write(&f.dir, "state.json", &serde_json::to_vec(&data).unwrap()).unwrap();
    let (_, body) = catalog(bearer.clone()).await;
    let second: Catalog = serde_json::from_slice(&body).unwrap();
    assert_eq!(second.apps.len(), 2);
    let path = format!("/v1/apps/{}/proxy/hello", first.apps[0].id);
    let relayed = |path: String| {
        let (f, bearer) = (&f, bearer.clone());
        async move {
            call(f, Method::GET, &path, Some(&bearer), None, String::new())
                .await
                .0
        }
    };
    assert_eq!(relayed(path.clone()).await, StatusCode::OK);
    // Sharing off: refused on the next request.
    write(
        &f.dir,
        "discovery-config.json",
        &serde_json::to_vec(&ServerConfig {
            bind: "127.0.0.1".into(),
            port: 7463,
            advertise: false,
            share_apps: false,
        })
        .unwrap(),
    )
    .unwrap();
    assert_eq!(relayed(path.clone()).await, StatusCode::FORBIDDEN);
    fs::remove_file(f.dir.join("discovery-config.json")).unwrap();
    assert_eq!(relayed(path.clone()).await, StatusCode::OK);
    // Revocation: refused on the next request.
    revoke_where(&f.dir, |_, _| true).unwrap();
    assert_eq!(catalog(bearer.clone()).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(relayed(path).await, StatusCode::UNAUTHORIZED);
}

/// Oversized uploads get a clear 413, and a route that points off this
/// machine is refused as unshared rather than failing as a server error.
#[tokio::test]
async fn relay_limits_answer_with_clear_statuses() {
    let f = fixture("machine-limits").await;
    let mut data: State = read(&f.dir.join("state.json")).unwrap().unwrap();
    data.routes.push(Route {
        name: "lan".into(),
        target: "http://10.0.0.5:8080".into(),
        pid: None,
    });
    write(&f.dir, "state.json", &serde_json::to_vec(&data).unwrap()).unwrap();
    let bearer = pair(&f).await;
    let web = route_id(&f.machine, "web");
    let lan = route_id(&f.machine, "lan");
    let (status, body) = call(
        &f,
        Method::POST,
        &format!("/v1/apps/{web}/proxy/upload"),
        Some(&bearer),
        None,
        "x".repeat(1024 * 1024),
    )
    .await;
    assert_eq!((status, &body[..]), (StatusCode::OK, &b"1048576"[..]));
    // Oversized uploads, declared and streamed, over HTTP/2 as the relay
    // sends them: the agent answers before reading the rest, and HTTP/2
    // delivers that answer however the platform treats the unread data.
    let mut roots = rustls::RootCertStore::empty();
    roots.add(f.cert.clone()).unwrap();
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec()];
    let stream = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(
            rustls::pki_types::ServerName::IpAddress(f.addr.ip().into()),
            tokio::net::TcpStream::connect(f.addr).await.unwrap(),
        )
        .await
        .unwrap();
    let (mut sender, connection) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(stream))
            .await
            .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let chunk = Bytes::from(vec![b'x'; 1024 * 1024]);
    for declared in [true, false] {
        let chunk = chunk.clone();
        let chunks = futures_util::stream::iter(
            (0..17).map(move |_| Ok::<_, Infallible>(hyper::body::Frame::data(chunk.clone()))),
        );
        let mut request = Request::post(format!("https://{}/v1/apps/{web}/proxy/upload", f.addr))
            .header("authorization", format!("Bearer {bearer}"));
        if declared {
            request = request.header("content-length", (17 * 1024 * 1024).to_string());
        }
        sender.ready().await.unwrap();
        let response = sender
            .send_request(
                request
                    .body(http_body_util::StreamBody::new(chunks))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::PAYLOAD_TOO_LARGE,
            "{declared}"
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("16 MB"));
    }
    let (status, _) = call(
        &f,
        Method::GET,
        &format!("/v1/apps/{lan}/proxy/"),
        Some(&bearer),
        None,
        String::new(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[test]
fn token_enrollment_records_the_machine_so_unpair_can_revoke_it() {
    let dir = temp();
    let invite = |dir: &Path| {
        let token = random().unwrap();
        let mut auth: Auth = read(&dir.join("discovery-auth.json"))
            .unwrap()
            .unwrap_or_default();
        auth.invitation = Some(Invitation {
            hash: digest(&token),
            expires: now() + 60,
        });
        write(
            dir,
            "discovery-auth.json",
            &serde_json::to_vec(&auth).unwrap(),
        )
        .unwrap();
        token
    };
    let laptop = enroll(&dir, &invite(&dir), Some("laptop-id"))
        .unwrap()
        .unwrap();
    let mini = enroll(&dir, &invite(&dir), Some("mini-id"))
        .unwrap()
        .unwrap();
    let revoked = revoke_machine_in(&dir, "laptop-id").unwrap();
    assert_eq!(
        revoked,
        Revoked {
            revoked: 1,
            unidentified: 0
        }
    );
    assert!(!paired(&dir, &laptop).unwrap());
    assert!(paired(&dir, &mini).unwrap());
    // An older client sent no machine: unpair can't tell whose it is and says so.
    let legacy = enroll(&dir, &invite(&dir), None).unwrap().unwrap();
    let pending = mint_client_in(&dir, PENDING_LABEL).unwrap();
    assert_eq!(
        revoke_machine_in(&dir, "laptop-id").unwrap(),
        Revoked {
            revoked: 0,
            unidentified: 2
        }
    );
    assert!(paired(&dir, &legacy).unwrap() && paired(&dir, &pending).unwrap());
    let _ = fs::remove_dir_all(dir);
}

#[tokio::test]
async fn token_enrollment_over_tls_labels_and_validates_the_machine() {
    let f = fixture("host").await;
    let enroll_as = |machine: Option<Machine>| {
        let token = random().unwrap();
        let mut auth: Auth = read(&f.dir.join("discovery-auth.json"))
            .unwrap()
            .unwrap_or_default();
        auth.invitation = Some(Invitation {
            hash: digest(&token),
            expires: now() + 60,
        });
        write(
            &f.dir,
            "discovery-auth.json",
            &serde_json::to_vec(&auth).unwrap(),
        )
        .unwrap();
        serde_json::to_string(&PairRequest { token, machine }).unwrap()
    };
    for bad in [
        f.machine.clone(),
        joiner("-x"),
        Machine {
            id: "ok-id".into(),
            name: "evil\u{202E}".into(),
        },
    ] {
        let body = enroll_as(Some(bad));
        let (status, _) = call(&f, Method::POST, "/v1/pair", None, None, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(no_clients(&f), "invalid machine spent the token");
    }
    let body = enroll_as(Some(joiner("laptop")));
    let (status, body) = call(&f, Method::POST, "/v1/pair", None, None, body).await;
    assert_eq!(status, StatusCode::OK);
    let bearer = serde_json::from_slice::<PairResponse>(&body).unwrap().token;
    let auth: Auth = read(&f.dir.join("discovery-auth.json")).unwrap().unwrap();
    assert_eq!(auth.labels.get(&digest(&bearer)).unwrap(), "laptop-id");
    // A 0.5.x client's request (no machine) still enrolls, unlabelled.
    let body = enroll_as(None);
    assert!(!body.contains("machine"));
    let (status, _) = call(&f, Method::POST, "/v1/pair", None, None, body).await;
    assert_eq!(status, StatusCode::OK);
}

fn mint_client_in(dir: &Path, label: &str) -> Result<String> {
    let mut auth: Auth = read(&dir.join("discovery-auth.json"))?.unwrap_or_default();
    let bearer = random()?;
    auth.clients.push(digest(&bearer));
    auth.labels.insert(digest(&bearer), label.into());
    write(dir, "discovery-auth.json", &serde_json::to_vec(&auth)?)?;
    Ok(bearer)
}

fn context_for(f: &Fixture) -> Arc<ContextState> {
    Arc::new(ContextState {
        dir: f.dir.clone(),
        machine: f.machine.clone(),
        share_apps: true,
        client: upstream(&f.dir).unwrap(),
        loopback: crate::upstream::client(tls_roots(&f.dir).unwrap()),
        loopback_insecure: crate::upstream::client(crate::upstream::insecure_tls().unwrap()),
        tunnels: Arc::new(tokio::sync::Semaphore::new(MAX_TUNNELS)),
        attempts: Arc::default(),
        sessions: Arc::default(),
        catalog: Arc::default(),
    })
}
fn acceptor_for(f: &Fixture) -> tokio_rustls::TlsAcceptor {
    let bytes = fs::read(f.dir.join("discovery-key.pem")).unwrap();
    let key = crate::pem::private_key(&mut bytes.as_slice())
        .unwrap()
        .unwrap();
    let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![f.cert.clone()], key)
    .unwrap();
    tls.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    tokio_rustls::TlsAcceptor::from(Arc::new(tls))
}
/// A second agent on the fixture's state, with its own limits and context.
async fn serve_with(
    f: &Fixture,
    context: Arc<ContextState>,
    slots: usize,
    limits: ConnectionLimits,
) -> (SocketAddr, tokio::task::JoinHandle<Result<()>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(connections_limited(
        listener,
        acceptor_for(f),
        context,
        Arc::new(tokio::sync::Semaphore::new(slots)),
        limits,
    ));
    (addr, task)
}
async fn tls_to(
    f: &Fixture,
    addr: SocketAddr,
) -> std::io::Result<tokio_rustls::client::TlsStream<tokio::net::TcpStream>> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(f.cert.clone()).unwrap();
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let tcp = tokio::net::TcpStream::connect(addr).await?;
    tokio::time::timeout(
        Duration::from_secs(3),
        tokio_rustls::TlsConnector::from(Arc::new(config)).connect(
            rustls::pki_types::ServerName::IpAddress(addr.ip().into()),
            tcp,
        ),
    )
    .await
    .unwrap_or_else(|_| Err(std::io::ErrorKind::TimedOut.into()))
}
async fn get_on(
    f: &Fixture,
    addr: SocketAddr,
    path: &str,
    token: Option<&str>,
    headers: &[(&str, &str)],
) -> (StatusCode, Bytes) {
    let stream = tls_to(f, addr).await.unwrap();
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let mut request = Request::builder()
        .uri(path)
        .header("host", addr.to_string());
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = sender
        .send_request(request.body(http_body_util::Empty::<Bytes>::new()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    (
        status,
        response.into_body().collect().await.unwrap().to_bytes(),
    )
}
async fn closed_by_server(
    stream: &mut tokio_rustls::client::TlsStream<tokio::net::TcpStream>,
) -> bool {
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 64];
    matches!(
        tokio::time::timeout(Duration::from_secs(3), stream.read(&mut buf)).await,
        Ok(Ok(0) | Err(_))
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unauthenticated_devices_cannot_take_every_connection() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
    let f = fixture("host").await;
    let bearer = pair(&f).await;
    let limits = ConnectionLimits {
        per_source: 16,
        untrusted: 3,
        per_untrusted_source: 2,
        idle: Duration::from_millis(300),
        lifetime: Duration::from_secs(30),
    };
    let (addr, task) = serve_with(&f, context_for(&f), 4, limits).await;
    // An unknown source gets a few connections, not every slot.
    let mut idle = vec![
        tls_to(&f, addr).await.unwrap(),
        tls_to(&f, addr).await.unwrap(),
    ];
    assert!(
        tls_to(&f, addr).await.is_err(),
        "third untrusted connection"
    );
    // Connections that never authenticate are closed once idle.
    for stream in &mut idle {
        assert!(closed_by_server(stream).await, "idle connection kept open");
    }
    drop(idle);
    tokio::time::sleep(Duration::from_millis(100)).await;
    // An authenticated request trusts its source from then on.
    let (status, _) = get_on(&f, addr, "/v1/catalog", Some(&bearer), &[]).await;
    assert_eq!(status, StatusCode::OK);
    let held: Vec<_> = futures_util::future::join_all((0..3).map(|_| tls_to(&f, addr))).await;
    assert!(
        held.iter().all(|s| s.is_ok()),
        "a trusted source has headroom"
    );
    drop(held);
    tokio::time::sleep(Duration::from_millis(100)).await;
    // WebSockets give their connection slot back when they upgrade, so more
    // of them than there are slots leave room for catalog requests.
    let id = route_id(&f.machine, "web");
    let mut sockets = Vec::new();
    for i in 0..6 {
        let mut request = format!("wss://{addr}/v1/apps/{id}/proxy/ws")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {bearer}").parse().unwrap());
        let stream = tls_to(&f, addr).await.unwrap();
        let (mut socket, _) = tokio_tungstenite::client_async(request, stream)
            .await
            .unwrap_or_else(|e| panic!("websocket {i}: {e}"));
        socket.send(Message::Text("hi".into())).await.unwrap();
        assert!(socket.next().await.unwrap().is_ok());
        sockets.push(socket);
    }
    let (status, _) = get_on(&f, addr, "/v1/catalog", Some(&bearer), &[]).await;
    assert_eq!(status, StatusCode::OK);
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_machine_cannot_take_over_a_paired_machines_identity() {
    let f = fixture("host").await;
    let pin = host_pin(&f);
    // laptop-id is paired with certificate 11…, and holds a credential here.
    let original = Peer {
        machine: joiner("laptop"),
        endpoint: "https://127.0.0.1:7464/".into(),
        fingerprint: "11".repeat(32),
        token: "d".repeat(64),
        sources: vec![Source::Explicit],
        last_seen: 0,
        slug: String::new(),
    };
    super::super::client::upsert_peer_in(&f.dir, original.clone()).unwrap();
    let laptop_bearer = mint_client_in(&f.dir, "laptop-id").unwrap();
    // Someone who knows the code claims laptop-id with another certificate.
    let opened = open_pairing_in(&f.dir, PAIR_CODE_LIFETIME).unwrap();
    let (_, x) = start(&f, &opened.code).await;
    let x = x.unwrap();
    assert_eq!(confirm(&f, &x, &pin).await.0, StatusCode::OK);
    let (code, body) = finish(&f, &x, None).await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert_eq!(error_code(&body), "already_paired");
    assert_eq!(progress(&f.dir, &opened.id), "cancelled:already_paired");
    let stored = super::super::client::peers_in(&f.dir).unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].fingerprint, original.fingerprint);
    assert_eq!(stored[0].endpoint, original.endpoint);
    assert!(
        paired(&f.dir, &laptop_bearer).unwrap(),
        "laptop lost access"
    );
    // The real laptop (same certificate) pairs again and replaces its credential.
    let mut auth: Auth = read(&f.dir.join("discovery-auth.json")).unwrap().unwrap();
    auth.pairing_result = None;
    write(
        &f.dir,
        "discovery-auth.json",
        &serde_json::to_vec(&auth).unwrap(),
    )
    .unwrap();
    let mut same = original.clone();
    same.fingerprint = "ab".repeat(32);
    super::super::client::remove_peer_in(&f.dir, "laptop-id").unwrap();
    super::super::client::upsert_peer_in(&f.dir, same).unwrap();
    let opened = open_pairing_in(&f.dir, PAIR_CODE_LIFETIME).unwrap();
    let (_, x) = start(&f, &opened.code).await;
    let x = x.unwrap();
    assert_eq!(confirm(&f, &x, &pin).await.0, StatusCode::OK);
    let (code, _) = finish(&f, &x, None).await;
    assert_eq!(code, StatusCode::OK);
    assert!(
        !paired(&f.dir, &laptop_bearer).unwrap(),
        "old credential replaced"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_confirmations_test_no_more_than_five_codes() {
    use super::super::pake;
    let f = fixture("host").await;
    let context = context_for(&f);
    // Every connection here comes from 127.0.0.1; let them all in.
    let limits = ConnectionLimits {
        per_untrusted_source: 64,
        ..ConnectionLimits::default()
    };
    let (addr, task) = serve_with(&f, context.clone(), 64, limits).await;
    let opened = open_pairing_in(&f.dir, PAIR_CODE_LIFETIME).unwrap();
    let w = window(&f.dir);
    let pin = host_pin(&f);
    // Sixteen exchanges, as if from sixteen addresses, all with wrong codes.
    let mut confirms = Vec::new();
    for i in 0..MAX_PAIR_SESSIONS {
        let wrong = format!(
            "{:06}",
            (opened.code.parse::<u32>().unwrap() + 1 + i as u32) % 1_000_000
        );
        let (state, msg) = pake::joiner_start(&pake::password(&w.salt, &wrong));
        let (host_msg, key) = pake::host_respond(&w.hash, &msg).unwrap();
        let joiner_key = pake::joiner_finish(state, &host_msg).unwrap();
        let id = random().unwrap();
        context.sessions().live.insert(
            id.clone(),
            PairSession {
                salt: w.salt.clone(),
                key,
                confirmed: false,
                started: Instant::now(),
                source: IpAddr::from([10, 0, 0, i as u8]),
            },
        );
        confirms.push(
            serde_json::to_string(&PairConfirm {
                session: id,
                confirm: joiner_key.joiner_confirm(&pin),
            })
            .unwrap(),
        );
    }
    let post = |body: String| {
        let f = &f;
        async move {
            let stream = tls_to(f, addr).await.unwrap();
            let (mut sender, connection) =
                hyper::client::conn::http1::handshake(TokioIo::new(stream))
                    .await
                    .unwrap();
            tokio::spawn(connection);
            sender
                .send_request(
                    Request::post("/v1/pair-code/confirm")
                        .header("host", addr.to_string())
                        .body(Full::new(Bytes::from(body)))
                        .unwrap(),
                )
                .await
                .unwrap()
                .status()
        }
    };
    let statuses = futures_util::future::join_all(confirms.into_iter().map(post)).await;
    let tested = statuses
        .iter()
        .filter(|s| **s == StatusCode::UNAUTHORIZED)
        .count();
    assert_eq!(tested, PAIR_CODE_MAX_FAILURES as usize, "{statuses:?}");
    assert_eq!(progress(&f.dir, &opened.id), "cancelled:too_many_failures");
    assert_eq!(context.sessions().checking, 0);
    task.abort();
}

#[tokio::test]
async fn abandoned_exchanges_cannot_fill_the_session_table() {
    use super::super::pake;
    let f = fixture("host").await;
    let context = context_for(&f);
    let (addr, task) = serve_with(&f, context.clone(), 64, ConnectionLimits::default()).await;
    let opened = open_pairing_in(&f.dir, PAIR_CODE_LIFETIME).unwrap();
    let w = window(&f.dir);
    let begin = || async {
        let (_, msg) = pake::joiner_start(&pake::password(&w.salt, &opened.code));
        let stream = tls_to(&f, addr).await.unwrap();
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .unwrap();
        tokio::spawn(connection);
        let response = sender
            .send_request(
                Request::post("/v1/pair-code/start")
                    .header("host", addr.to_string())
                    .body(Full::new(Bytes::from(
                        serde_json::to_string(&PairStart {
                            pairing: PAIRING_VERSION,
                            salt: w.salt.clone(),
                            msg,
                        })
                        .unwrap(),
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice::<PairStarted>(&body)
            .unwrap()
            .session
    };
    // One address keeps only its newest exchanges.
    let first = begin().await;
    let second = begin().await;
    let third = begin().await;
    {
        let all = context.sessions();
        assert!(!all.live.contains_key(&first));
        assert!(all.live.contains_key(&second) && all.live.contains_key(&third));
    }
    // A table full of other addresses' abandoned exchanges gives way to the oldest.
    let (_, key) = pake::host_respond(&w.hash, &pake::joiner_start("x").1).unwrap();
    let mut key = Some(key);
    {
        let mut all = context.sessions();
        all.live.clear();
        for i in 0..MAX_PAIR_SESSIONS {
            let key = match key.take() {
                Some(k) => k,
                None => {
                    pake::host_respond(&w.hash, &pake::joiner_start("x").1)
                        .unwrap()
                        .1
                }
            };
            all.live.insert(
                format!("old{i}"),
                PairSession {
                    salt: w.salt.clone(),
                    key,
                    confirmed: false,
                    started: Instant::now() - Duration::from_secs(1)
                        + Duration::from_millis(i as u64),
                    source: IpAddr::from([10, 0, 1, i as u8]),
                },
            );
        }
    }
    let fresh = begin().await;
    let all = context.sessions();
    assert!(all.live.contains_key(&fresh));
    assert!(!all.live.contains_key("old0"));
    assert_eq!(all.live.len(), MAX_PAIR_SESSIONS);
    drop(all);
    task.abort();
}

#[tokio::test]
async fn the_rate_limiter_survives_many_sources_and_a_poisoned_lock() {
    let f = fixture("host").await;
    let context = context_for(&f);
    {
        let mut all = context.attempts.lock().unwrap();
        for i in 0..MAX_ATTEMPT_SOURCES as u32 {
            all.insert(
                IpAddr::from((0x0a00_0000 + i).to_be_bytes()),
                VecDeque::from([Instant::now()]),
            );
        }
    }
    // Every slot taken by other addresses doesn't lock out a new one.
    assert!(context.allow_attempt(IpAddr::from([192, 168, 1, 20])));
    assert!(context.attempts.lock().unwrap().len() <= MAX_ATTEMPT_SOURCES);
    // Nor does a panic while the limiter was held.
    let attempts = context.attempts.clone();
    let _ = std::thread::spawn(move || {
        let _held = attempts.lock().unwrap();
        panic!("poison the limiter");
    })
    .join();
    assert!(context.attempts.is_poisoned());
    assert!(context.allow_attempt(IpAddr::from([192, 168, 1, 21])));
}

#[tokio::test]
async fn relayed_requests_carry_only_forwarding_headers_the_agent_set() {
    let app = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let app_addr = app.local_addr().unwrap();
    let router = axum::Router::new().fallback(|headers: axum::http::HeaderMap| async move {
        let mut seen: Vec<String> = headers
            .iter()
            .filter(|(k, _)| k.as_str() != "host" && k.as_str() != "user-agent")
            .map(|(k, v)| format!("{k}={}", v.to_str().unwrap()))
            .collect();
        seen.sort();
        seen.join("\n")
    });
    let app_task = tokio::spawn(async move { axum::serve(app, router).await });
    let f = fixture("host").await;
    let data = State {
        routes: vec![Route {
            name: "echo".into(),
            target: format!("http://{app_addr}"),
            pid: None,
        }],
        ..Default::default()
    };
    write(&f.dir, "state.json", &serde_json::to_vec(&data).unwrap()).unwrap();
    let bearer = pair(&f).await;
    let path = format!("/v1/apps/{}/proxy/", route_id(&f.machine, "echo"));
    let spoofed = [
        ("x-forwarded-for", "203.0.113.9"),
        ("x-forwarded-host", "admin.example"),
        ("x-forwarded-port", "8443"),
        ("x-forwarded-prefix", "/admin"),
        ("forwarded", "for=203.0.113.9;proto=https"),
        ("x-real-ip", "203.0.113.9"),
        ("x-client-ip", "203.0.113.9"),
        ("true-client-ip", "203.0.113.9"),
        ("x-original-url", "/admin"),
        ("x-rewrite-url", "/admin"),
        ("x-custom", "kept"),
    ];
    let (status, body) = get_on(&f, f.addr, &path, Some(&bearer), &spoofed).await;
    assert_eq!(status, StatusCode::OK);
    let seen = String::from_utf8(body.to_vec()).unwrap();
    assert_eq!(
        seen,
        [
            "x-custom=kept",
            "x-forwarded-for=127.0.0.1",
            "x-forwarded-host=echo.localhost:7462",
            "x-forwarded-port=7462",
            "x-forwarded-proto=http",
        ]
        .join("\n")
    );
    // What the paired machine's own proxy saw comes through its internal headers.
    let (_, body) = get_on(
        &f,
        f.addr,
        &path,
        Some(&bearer),
        &[
            ("x-porch-app-host", "echo.mini.localhost"),
            ("x-porch-app-proto", "https"),
            ("x-forwarded-host", "admin.example"),
        ],
    )
    .await;
    let seen = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        seen.contains("x-forwarded-host=echo.mini.localhost\n"),
        "{seen}"
    );
    assert!(seen.contains("x-forwarded-port=443\n"), "{seen}");
    assert!(seen.contains("x-forwarded-proto=https"), "{seen}");
    app_task.abort();
}

#[cfg(unix)]
#[test]
fn locking_writes_the_format_marker_only_once() {
    use std::os::unix::fs::MetadataExt;
    let dir = temp();
    drop(lock(&dir).unwrap());
    let marker = dir.join("discovery-format");
    let before = fs::metadata(&marker).unwrap().ino();
    let id = machine(&dir).unwrap();
    drop(lock(&dir).unwrap());
    assert_eq!(
        fs::metadata(&marker).unwrap().ino(),
        before,
        "marker rewritten"
    );
    assert_eq!(machine(&dir).unwrap(), id);
    fs::write(&marker, b"0\n").unwrap();
    drop(lock(&dir).unwrap());
    assert_eq!(fs::read(&marker).unwrap(), FORMAT_MARKER);
    let _ = fs::remove_dir_all(dir);
}
