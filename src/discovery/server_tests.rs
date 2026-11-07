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
        ..Default::default()
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
    // Only the apps can carry a route's target. The whole catalog also holds
    // a timestamp and hashed ids, where the digits "9000" turn up by chance.
    let apps = serde_json::to_string(&remote.apps).unwrap();
    assert!(!apps.contains("127.0.0.1"), "{apps}");
    assert!(!apps.contains(":9000"), "{apps}");
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
    let apps = catalog(&dir, m, true).unwrap().apps;
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
        attempts: Arc::default(),
        sessions: Arc::default(),
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
    match code_accepted(dir, &w.salt, peer).unwrap() {
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
        code_accepted(&dir, &old.salt, &joiner("laptop")).unwrap(),
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
