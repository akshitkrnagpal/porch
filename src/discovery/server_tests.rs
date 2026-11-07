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
    assert!(!serde_json::to_string(&remote).unwrap().contains("9000"));
    let _ = fs::remove_dir_all(dir);
}
#[test]
fn catalog_health_matches_porch_list() {
    let dir = temp();
    let m = machine(&dir).unwrap();
    let up = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let up_port = up.local_addr().unwrap().port();
    let down_port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
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
    let CodeOutcome::Accepted(bearer) = try_code(&dir, &opened.code, &joiner("laptop")).unwrap()
    else {
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
        try_code(&dir, &opened.code, &joiner("laptop")).unwrap(),
        CodeOutcome::Closed
    ));
    // Re-pairing the same machine replaces its old credential.
    let again = open_pairing_in(&dir, PAIR_CODE_LIFETIME).unwrap();
    let CodeOutcome::Accepted(second) = try_code(&dir, &again.code, &joiner("laptop")).unwrap()
    else {
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
            matches!(
                try_code(&dir, wrong, &joiner("attacker")).unwrap(),
                CodeOutcome::Wrong
            ),
            "attempt {attempt}"
        );
        assert_eq!(pairing_open(&dir), attempt < PAIR_CODE_MAX_FAILURES);
    }
    assert!(matches!(
        try_code(&dir, &opened.code, &joiner("laptop")).unwrap(),
        CodeOutcome::Closed
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
        try_code(&dir, &opened.code, &joiner("laptop")).unwrap(),
        CodeOutcome::Closed
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
    let CodeOutcome::Accepted(bearer) = try_code(&dir, &third.code, &joiner("laptop")).unwrap()
    else {
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
        try_code(&dir, &opened.code, &joiner("laptop")).unwrap(),
        CodeOutcome::Full
    ));
    assert_eq!(progress(&dir, &opened.id), "cancelled:host_full");
    let auth: Auth = read(&dir.join("discovery-auth.json")).unwrap().unwrap();
    assert_eq!(auth.clients.len(), MAX_CLIENTS);
    // Storing the joiner can also fail after acceptance; that takes it back.
    auth_reset(&dir);
    let opened = open_pairing_in(&dir, PAIR_CODE_LIFETIME).unwrap();
    let CodeOutcome::Accepted(bearer) = try_code(&dir, &opened.code, &joiner("laptop")).unwrap()
    else {
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
#[tokio::test]
async fn real_tls_pair_code_binds_certificate_rate_limits_and_stores_joiner() {
    let f = fixture("host").await;
    let status = |body: Bytes| serde_json::from_slice::<PairingStatus>(&body).unwrap().open;
    let (code, body) = call(&f, Method::GET, "/v1/pairing", None, None, String::new()).await;
    assert_eq!(code, StatusCode::OK);
    assert!(!status(body));
    let opened = open_pairing_in(&f.dir, PAIR_CODE_LIFETIME).unwrap();
    let (_, body) = call(&f, Method::GET, "/v1/pairing", None, None, String::new()).await;
    assert!(status(body));
    let own = fingerprint_in(&f.dir).unwrap();
    let request = |code: &str, seen: &str| {
        serde_json::to_string(&PairCodeRequest {
            version: PROTOCOL_VERSION,
            code: code.into(),
            host_fingerprint: seen.into(),
            machine: joiner("laptop"),
            endpoint: "https://127.0.0.1:7463/".into(),
            fingerprint: "ab".repeat(32),
            token: "c".repeat(64),
        })
        .unwrap()
    };
    // An interceptor presenting another certificate never gets the code checked.
    let (code, _) = call(
        &f,
        Method::POST,
        "/v1/pair-code",
        None,
        None,
        request(&opened.code, &"0".repeat(64)),
    )
    .await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert!(pairing_open(&f.dir));
    let error_code = |body: &Bytes| {
        serde_json::from_slice::<PairCodeError>(body)
            .unwrap()
            .error
            .code
    };
    let wrong = if opened.code == "000000" {
        "111111"
    } else {
        "000000"
    };
    let (code, body) = call(
        &f,
        Method::POST,
        "/v1/pair-code",
        None,
        None,
        request(wrong, &own),
    )
    .await;
    assert_eq!(code, StatusCode::UNAUTHORIZED);
    assert_eq!(error_code(&body), "wrong_code");
    let (code, body) = call(
        &f,
        Method::POST,
        "/v1/pair-code",
        None,
        None,
        request(&opened.code, &own),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let response: PairResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(response.machine.id, f.machine.id);
    assert_eq!(
        pairing_progress_in(&f.dir, &opened.id).unwrap().state,
        PairState::Accepted
    );
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
    // The new peer's first authenticated request completes the pairing.
    assert_eq!(
        pairing_progress_in(&f.dir, &opened.id).unwrap().state,
        PairState::Paired
    );
    // Mutual: the host stored the joiner with the credentials it supplied.
    let stored = super::super::client::peers_in(&f.dir).unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].machine.name, "laptop");
    assert_eq!(stored[0].token, "c".repeat(64));
    assert_eq!(stored[0].slug, "laptop");
    let (code, body) = call(
        &f,
        Method::POST,
        "/v1/pair-code",
        None,
        None,
        request(&opened.code, &own),
    )
    .await;
    assert_eq!(code, StatusCode::GONE, "code is single-use");
    assert_eq!(error_code(&body), "not_waiting");
    // Per-source rate limit (10/minute) applies on top of the 5-failure cap.
    let mut limited = false;
    for _ in 0..12 {
        let (code, body) = call(
            &f,
            Method::POST,
            "/v1/pair-code",
            None,
            None,
            request("123456", &own),
        )
        .await;
        if code == StatusCode::TOO_MANY_REQUESTS {
            assert_eq!(error_code(&body), "rate_limited");
            limited = true;
        }
    }
    assert!(limited);
}
