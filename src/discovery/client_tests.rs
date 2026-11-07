use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

fn fixture_catalog() -> Catalog {
    Catalog {
        version: PROTOCOL_VERSION,
        machine: Machine {
            id: "machine-b".into(),
            name: "Remote Mac".into(),
        },
        apps: vec![App {
            id: "web".into(),
            name: "web".into(),
            local_url: "http://web.localhost:7462".into(),
            health: Health::Healthy,
            remote_access: true,
            unavailable_reason: None,
        }],
        proxy_running: true,
        generated_at: 1,
    }
}
fn fixture_peer(endpoint: String, fingerprint: String) -> Peer {
    Peer {
        machine: fixture_catalog().machine,
        endpoint,
        fingerprint,
        token: "a".repeat(64),
        sources: vec![Source::Explicit],
        last_seen: 0,
        slug: String::new(),
    }
}
async fn tls_fixture(
    body: Vec<u8>,
    status: StatusCode,
) -> (Peer, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let cert = rcgen::generate_simple_self_signed(vec!["porch-peer.local".into()]).unwrap();
    let pin = hex_pin(&Sha256::digest(cert.cert.der()).into());
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
        cert.key_pair.serialize_der(),
    ));
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![cert.cert.der().clone()], key)
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer = fixture_peer(format!("https://{}/", listener.local_addr().unwrap()), pin);
    let hits = Arc::new(AtomicUsize::new(0));
    let received = hits.clone();
    let task = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let acceptor = acceptor.clone();
            let body = body.clone();
            let hits = received.clone();
            tokio::spawn(async move {
                let Ok(stream) = acceptor.accept(stream).await else {
                    return;
                };
                let service = hyper::service::service_fn(move |req: Request<Incoming>| {
                    hits.fetch_add(1, Ordering::Relaxed);
                    let authorized = req.headers().get(header::AUTHORIZATION).is_some_and(|h| {
                        h.as_bytes() == format!("Bearer {}", "a".repeat(64)).as_bytes()
                    });
                    let mut response = Response::new(Body::from(if authorized {
                        body.clone()
                    } else {
                        b"unauthorized".to_vec()
                    }));
                    *response.status_mut() = if authorized {
                        status
                    } else {
                        StatusCode::UNAUTHORIZED
                    };
                    async move { Ok::<_, Infallible>(response) }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    (peer, hits, task)
}

#[test]
fn endpoints_reject_dns_public_credentials_redirect_paths_and_mapped_addresses() {
    for u in [
        "http://127.0.0.1:7463",
        "https://porch.local:7463",
        "https://example.com",
        "https://8.8.8.8",
        "https://0.0.0.0:7463",
        "https://[::]:7463",
        "https://[::ffff:127.0.0.1]:7463",
        "https://name:secret@192.168.1.4:7463",
        "https://192.168.1.4:7463/v1/catalog",
        "https://192.168.1.4?url=http://169.254.169.254",
        "https://192.168.1.4/#secret",
    ] {
        assert!(validate_endpoint(u).is_err(), "accepted {u}");
    }
    for u in [
        "https://127.0.0.2:1234",
        "https://192.168.1.4:7463",
        "https://10.0.0.2:7463",
        "https://172.16.0.2:7463",
        "https://100.64.1.2:7463",
        "https://[::1]:7463",
        "https://[fd7a:115c:a1e0::2]:7463",
    ] {
        assert!(validate_endpoint(u).is_ok(), "rejected {u}");
    }
}
#[test]
fn tailscale_adapter_filters_offline_self_unroutable_and_backend() {
    let fixture = serde_json::json!({"BackendState":"Running","Self":{"ID":"self","PublicKey":"self-key"},"Peer":{
        "self-key":{"ID":"self","HostName":"Me","Online":true,"TailscaleIPs":["100.64.1.1"]},
        "offline":{"ID":"offline","HostName":"Old","Online":false,"TailscaleIPs":["100.64.1.2"]},
        "online":{"ID":"other","HostName":"Linux","Online":true,"TailscaleIPs":["100.92.2.3","fd7a:115c:a1e0::2","8.8.8.8","127.0.0.1","10.0.0.2"]}
    }});
    let all = tailscale_candidates(&serde_json::to_vec(&fixture).unwrap(), 42).unwrap();
    assert_eq!(all.len(), 2);
    assert!(all
        .iter()
        .all(|c| c.name == "Linux" && c.last_seen == 42 && c.source == Source::Tailscale));
    assert_eq!(all[0].endpoint, "https://100.92.2.3:7463/");
    assert!(all.iter().all(|c| c.node.as_deref() == Some("other")));
    assert!(
        tailscale_candidates(br#"{"BackendState":"Stopped","Peer":{}}"#, 42)
            .unwrap()
            .is_empty()
    );
    assert!(tailscale_candidates(&vec![b' '; MAX_BODY + 1], 0).is_err());
}
#[test]
fn goodbye_and_stale_expiry_remove_candidates_without_retaining_apps() {
    let c = Candidate {
        machine_id: Some("b".into()),
        name: "B".into(),
        endpoint: "https://192.168.0.3:7463/".into(),
        source: Source::Lan,
        last_seen: 100,
        node: None,
    };
    let mut cache = CandidateCache::default();
    cache.insert("b._porch._tcp.local.".into(), vec![c.clone()]);
    cache.expire(190);
    assert_eq!(cache.all().len(), 1);
    cache.expire(191);
    assert!(cache.all().is_empty());
    cache.insert("b._porch._tcp.local.".into(), vec![c]);
    cache.remove("b._porch._tcp.local.");
    assert!(cache.all().is_empty());
}
#[test]
fn protocol_validation_rejects_machine_spoofing_and_malformed_app_ids() {
    let mut c = fixture_catalog();
    assert!(validate_catalog(&c, "machine-a").is_err());
    c.apps[0].id = "../admin".into();
    assert!(validate_catalog(&c, "machine-b").is_err());
    c.apps[0].id = "web".into();
    c.apps.push(c.apps[0].clone());
    assert!(validate_catalog(&c, "machine-b").is_err());
    c.apps.pop();
    c.apps[0].local_url = "file:///etc/passwd".into();
    assert!(validate_catalog(&c, "machine-b").is_err());
}
#[tokio::test]
async fn actual_tls_catalog_requires_exact_pin_and_token_before_returning_apps() {
    let (mut peer, hits, task) = tls_fixture(
        serde_json::to_vec(&fixture_catalog()).unwrap(),
        StatusCode::OK,
    )
    .await;
    let good = fetch_catalog(&peer, &peer.endpoint).await.unwrap();
    assert_eq!(good.machine.id, "machine-b");
    assert_eq!(good.apps[0].name, "web");
    assert_eq!(hits.load(Ordering::Relaxed), 1);
    peer.fingerprint = "0".repeat(64);
    assert!(fetch_catalog(&peer, &peer.endpoint).await.is_err());
    assert_eq!(
        hits.load(Ordering::Relaxed),
        1,
        "token must not reach a server with the wrong certificate"
    );
    task.abort();
    let (mut peer, _, task) = tls_fixture(
        serde_json::to_vec(&fixture_catalog()).unwrap(),
        StatusCode::OK,
    )
    .await;
    peer.token = "b".repeat(64);
    assert!(fetch_catalog(&peer, &peer.endpoint).await.is_err());
    task.abort();
}
#[tokio::test]
async fn actual_tls_catalog_bounds_bad_peer_body_and_identity() {
    let (peer, _, task) = tls_fixture(vec![b' '; MAX_BODY + 1], StatusCode::OK).await;
    assert!(fetch_catalog(&peer, &peer.endpoint)
        .await
        .unwrap_err()
        .to_string()
        .contains("size limit"));
    task.abort();
    let mut catalog = fixture_catalog();
    catalog.machine.id = "imposter".into();
    let (peer, _, task) = tls_fixture(serde_json::to_vec(&catalog).unwrap(), StatusCode::OK).await;
    assert!(fetch_catalog(&peer, &peer.endpoint).await.is_err());
    task.abort();
}
#[tokio::test]
async fn authenticated_lan_and_tailnet_hints_collapse_to_one_machine_catalog() {
    let (peer, _, task) = tls_fixture(
        serde_json::to_vec(&fixture_catalog()).unwrap(),
        StatusCode::OK,
    )
    .await;
    let candidates = vec![
        Candidate {
            machine_id: Some("machine-b".into()),
            name: "B".into(),
            endpoint: peer.endpoint.clone(),
            source: Source::Lan,
            last_seen: now(),
            node: None,
        },
        Candidate {
            machine_id: None,
            name: "B".into(),
            endpoint: peer.endpoint.clone(),
            source: Source::Tailscale,
            last_seen: now(),
            node: Some("node-b".into()),
        },
    ];
    let result = query_peer(peer, &candidates).await;
    assert_eq!(result.machine.id, "machine-b");
    assert!(result.catalog.is_some());
    assert!(result.sources.contains(&Source::Lan));
    assert!(result.sources.contains(&Source::Tailscale));
    task.abort();
}
#[test]
#[ignore = "requires local multicast; run explicitly to observe real _porch._tcp browsing"]
fn real_local_mdns_registration_and_browse() {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
    socket.connect("192.0.2.1:9").unwrap();
    let ip = if std::env::var_os("PORCH_TEST_MDNS_LOOPBACK").is_some() {
        IpAddr::from([127, 0, 0, 1])
    } else {
        socket.local_addr().unwrap().ip()
    };
    let daemon = mdns_sd::ServiceDaemon::new().unwrap();
    daemon.disable_interface(mdns_sd::IfKind::All).unwrap();
    daemon.enable_interface(mdns_sd::IfKind::Addr(ip)).unwrap();
    let id = format!("test-{}", std::process::id());
    let hostname = format!("{id}.local.");
    let properties = [("id", id.as_str()), ("version", "1")];
    let info = mdns_sd::ServiceInfo::new(SERVICE_TYPE, &id, &hostname, ip, 23456, &properties[..])
        .unwrap();
    let monitor = daemon.monitor().unwrap();
    daemon.register(info).unwrap();
    eprintln!(
        "opaque registration monitor on {ip}: {:?}",
        monitor.recv_timeout(Duration::from_secs(2))
    );
    let cache = browse_lan(CandidateCache::default()).unwrap();
    let all = cache.all();
    let _ = daemon.shutdown();
    assert!(
        all.iter()
            .any(|c| c.machine_id.as_deref() == Some(id.as_str())
                && c.endpoint == format!("https://{ip}:23456/")),
        "real local browse did not resolve registration: {all:?}"
    );
}

async fn relay_fixture() -> (
    Url,
    tokio::task::JoinHandle<()>,
    tokio::task::JoinHandle<()>,
) {
    let cert = rcgen::generate_simple_self_signed(vec!["porch-peer.local".into()]).unwrap();
    let pin = hex_pin(&Sha256::digest(cert.cert.der()).into());
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
        cert.key_pair.serialize_der(),
    ));
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![cert.cert.der().clone()], key)
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer = fixture_peer(format!("https://{}/", upstream.local_addr().unwrap()), pin);
    let backend = tokio::spawn(async move {
        loop {
            let (stream, _) = upstream.accept().await.unwrap();
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(stream) = acceptor.accept(stream).await else {
                    return;
                };
                let service = hyper::service::service_fn(|mut req: Request<Incoming>| async move {
                    let mut response = Response::new(Body::empty());
                    if req
                        .headers()
                        .get(header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        != Some(format!("Bearer {}", "a".repeat(64)).as_str())
                    {
                        *response.status_mut() = StatusCode::UNAUTHORIZED;
                    } else if req.uri().path() == "/v1/apps/web/proxy/socket" {
                        let accept = tokio_tungstenite::tungstenite::handshake::derive_accept_key(
                            req.headers().get("sec-websocket-key").unwrap().as_bytes(),
                        );
                        let upgraded = hyper::upgrade::on(&mut req);
                        tokio::spawn(async move {
                            use futures_util::SinkExt;
                            let stream = upgraded.await.unwrap();
                            let mut ws = tokio_tungstenite::WebSocketStream::from_raw_socket(
                                TokioIo::new(stream),
                                tokio_tungstenite::tungstenite::protocol::Role::Server,
                                None,
                            )
                            .await;
                            let message = ws.next().await.unwrap().unwrap();
                            assert_eq!(message.to_text().unwrap(), "hello-machine-b");
                            ws.send(tokio_tungstenite::tungstenite::Message::Text(
                                "machine-b-echo".into(),
                            ))
                            .await
                            .unwrap();
                        });
                        *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
                        response
                            .headers_mut()
                            .insert(header::CONNECTION, "upgrade".parse().unwrap());
                        response
                            .headers_mut()
                            .insert(header::UPGRADE, "websocket".parse().unwrap());
                        response
                            .headers_mut()
                            .insert("sec-websocket-accept", accept.parse().unwrap());
                    } else if req.uri().path().ends_with("/redirect")
                        || req.uri().path().ends_with("/protocol-relative")
                        || req.uri().path().ends_with("/foreign")
                    {
                        *response.status_mut() = StatusCode::FOUND;
                        let location = if req.uri().path().ends_with("/foreign") {
                            "http://other.localhost:9999/wrong-machine"
                        } else if req.uri().path().ends_with("/protocol-relative") {
                            "//web.localhost:7462/done?q=remote"
                        } else {
                            "http://web.localhost:7462/done?q=remote"
                        };
                        response
                            .headers_mut()
                            .insert(header::LOCATION, location.parse().unwrap());
                        response.headers_mut().insert(
                            header::SET_COOKIE,
                            "session=value; Domain=web.localhost; Secure; HttpOnly; SameSite=Lax"
                                .parse()
                                .unwrap(),
                        );
                    } else {
                        let value = serde_json::json!({"uri":req.uri().to_string(),"origin":req.headers().get(header::ORIGIN).and_then(|v|v.to_str().ok()),"app_origin":req.headers().get("x-porch-app-origin").and_then(|v|v.to_str().ok()),"app_auth":req.headers().get("x-porch-app-authorization").and_then(|v|v.to_str().ok()),"app_host":req.headers().get("x-porch-app-host").and_then(|v|v.to_str().ok())});
                        *response.body_mut() = Body::from(value.to_string());
                    }
                    Ok::<_, Infallible>(response)
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .with_upgrades()
                    .await;
            });
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let relay_url = Url::parse(&format!(
        "http://app-machine-b.localhost:{}",
        address.port()
    ))
    .unwrap();
    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(tls_config(&peer.fingerprint).unwrap())
        .https_only()
        .enable_http1()
        .build();
    let context = RelayContext {
        peer,
        app_id: "web".into(),
        client: Client::builder(TokioExecutor::new()).build(https),
        relay_url: relay_url.clone(),
        local_app: Url::parse(&fixture_catalog().apps[0].local_url).unwrap(),
    };
    let relay = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let context = context.clone();
            tokio::spawn(async move {
                let service =
                    hyper::service::service_fn(move |req| relay_request(req, context.clone()));
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .with_upgrades()
                    .await;
            });
        }
    });
    (relay_url, relay, backend)
}
#[tokio::test]
async fn actual_relay_preserves_route_and_app_auth_but_rejects_browser_cross_origin_and_host() {
    let (url, relay, backend) = relay_fixture().await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .resolve(
            url.host_str().unwrap(),
            SocketAddr::from(([127, 0, 0, 1], url.port().unwrap())),
        )
        .build()
        .unwrap();
    let data: serde_json::Value = client
        .get(url.join("/where?q=yes").unwrap())
        .header(header::ORIGIN, url.origin().ascii_serialization())
        .header(header::AUTHORIZATION, "Basic app-credential")
        .header("x-porch-app-origin", "https://evil.example")
        .header("x-porch-app-authorization", "Bearer attacker-token")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(data["uri"]
        .as_str()
        .unwrap()
        .ends_with("/v1/apps/web/proxy/where?q=yes"));
    assert!(data["origin"].is_null());
    assert_eq!(data["app_origin"], "http://web.localhost:7462");
    assert_eq!(data["app_auth"], "Basic app-credential");
    assert_eq!(
        data["app_host"],
        format!("{}:{}", url.host_str().unwrap(), url.port().unwrap())
    );
    assert_eq!(
        client
            .get(url.clone())
            .header(header::ORIGIN, "https://evil.example")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        client
            .get(url.clone())
            .header(header::HOST, "evil.example")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::MISDIRECTED_REQUEST
    );
    relay.abort();
    backend.abort();
}
#[tokio::test]
async fn actual_relay_translates_remote_localhost_redirect_and_isolates_cookies() {
    let (url, relay, backend) = relay_fixture().await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .resolve(
            url.host_str().unwrap(),
            SocketAddr::from(([127, 0, 0, 1], url.port().unwrap())),
        )
        .build()
        .unwrap();
    for path in ["/redirect", "/protocol-relative"] {
        let response = client.get(url.join(path).unwrap()).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::FOUND);
        assert_eq!(
            response.headers()[header::LOCATION],
            url.join("/done?q=remote").unwrap().as_str()
        );
        assert_eq!(
            response.headers()[header::SET_COOKIE],
            "session=value; Secure; HttpOnly; SameSite=Lax"
        );
    }
    assert_eq!(
        client
            .get(url.join("/foreign").unwrap())
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_GATEWAY
    );
    relay.abort();
    backend.abort();
}
#[tokio::test]
async fn actual_browser_websocket_passes_through_pinned_tls_relay_to_machine_b() {
    use futures_util::SinkExt;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let (url, relay, backend) = relay_fixture().await;
    let mut ws_url = url.join("/socket").unwrap();
    ws_url.set_scheme("ws").unwrap();
    let mut request = ws_url.as_str().into_client_request().unwrap();
    request.headers_mut().insert(
        header::ORIGIN,
        url.origin().ascii_serialization().parse().unwrap(),
    );
    let stream =
        tokio::net::TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], url.port().unwrap())))
            .await
            .unwrap();
    let (mut ws, _) = tokio_tungstenite::client_async(request, stream)
        .await
        .unwrap();
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        "hello-machine-b".into(),
    ))
    .await
    .unwrap();
    let message = tokio::time::timeout(Duration::from_secs(3), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(message.to_text().unwrap(), "machine-b-echo");
    relay.abort();
    backend.abort();
}
#[test]
fn slugs_are_readable_unique_and_stable() {
    assert_eq!(slug("Akshits-Mac-mini.local"), "akshits-mac-mini");
    assert_eq!(slug("Mac mini"), "mac-mini");
    assert_eq!(slug("  !!  "), "peer");
    let named = |id: &str, name: &str, s: &str| {
        let mut p = fixture_peer("https://127.0.0.1:7463/".into(), "a".repeat(64));
        p.machine = Machine {
            id: id.into(),
            name: name.into(),
        };
        p.slug = s.into();
        p
    };
    let mut all = vec![
        named("1", "Mac mini", ""),
        named("2", "Mac mini", ""),
        named("3", "Bad", "Has Spaces"),
    ];
    assign_slugs(&mut all);
    let slugs: Vec<_> = all.iter().map(|p| p.slug.clone()).collect();
    assert_eq!(slugs, ["mac-mini", "mac-mini-2", "bad"]);
    // Removing the first machine never renames the second; a newcomer gets a free name.
    let mut rest = vec![all[1].clone(), named("4", "Mac mini", "")];
    assign_slugs(&mut rest);
    assert_eq!(rest[0].slug, "mac-mini-2");
    assert_eq!(rest[1].slug, "mac-mini");
}
#[test]
fn paired_machines_are_not_listed_again_as_discovered() {
    let peer = |id: &str, endpoint: &str| PeerCatalog {
        machine: Machine {
            id: id.into(),
            name: id.into(),
        },
        slug: id.into(),
        endpoint: endpoint.into(),
        sources: vec![Source::Explicit],
        last_seen: 0,
        catalog: None,
        error: None,
        fingerprint: String::new(),
    };
    let candidate =
        |id: Option<&str>, endpoint: &str, source: Source, node: Option<&str>| Candidate {
            machine_id: id.map(Into::into),
            name: "hint".into(),
            endpoint: endpoint.into(),
            source,
            last_seen: 0,
            node: node.map(Into::into),
        };
    // Paired on a custom port via its tailnet IPv4; Tailscale hints guess 7463
    // on both of that node's addresses.
    let peers = vec![peer("mini", "https://100.101.77.128:1356/")];
    let all = vec![
        candidate(
            None,
            "https://100.101.77.128:7463/",
            Source::Tailscale,
            Some("n1"),
        ),
        candidate(
            None,
            "https://[fd7a:115c:a1e0::eb34:4d80]:7463/",
            Source::Tailscale,
            Some("n1"),
        ),
        candidate(
            Some("mini"),
            "https://192.168.1.165:1356/",
            Source::Lan,
            None,
        ),
        candidate(
            None,
            "https://100.92.2.3:7463/",
            Source::Tailscale,
            Some("n2"),
        ),
        candidate(
            Some("other"),
            "https://192.168.1.20:7463/",
            Source::Lan,
            None,
        ),
    ];
    let left: Vec<_> = unpaired_candidates(all, &peers)
        .into_iter()
        .map(|c| c.endpoint)
        .collect();
    assert_eq!(
        left,
        ["https://100.92.2.3:7463/", "https://192.168.1.20:7463/"]
    );
}
