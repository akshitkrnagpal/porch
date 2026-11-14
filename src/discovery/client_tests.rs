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
    // Stored without the trailing slash: requests are built from the parsed URL.
    let peer = fixture_peer(format!("https://{}", upstream.local_addr().unwrap()), pin);
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
                    } else if req.uri().path().ends_with("/utf8") {
                        // Bytes beyond ASCII, as apps send UTF-8 in these headers.
                        *response.status_mut() = StatusCode::FOUND;
                        let location: &[u8] = match req.uri().query() {
                            Some("loopback") => b"http://127.0.0.1:9999/caf\xc3\xa9",
                            _ => b"http://web.localhost:7462/caf\xc3\xa9?q=\xe2\x9c\x93",
                        };
                        response.headers_mut().insert(
                            header::LOCATION,
                            header::HeaderValue::from_bytes(location).unwrap(),
                        );
                        response.headers_mut().insert(
                            header::SET_COOKIE,
                            header::HeaderValue::from_bytes(
                                b"name=J\xc3\xbcrgen; domain=web.localhost; Path=/",
                            )
                            .unwrap(),
                        );
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
    let context = RelayContext {
        client: relay_client(&peer.fingerprint).unwrap(),
        peer,
        app_id: "web".into(),
        relay_url: relay_url.clone(),
        local_app: Url::parse(&fixture_catalog().apps[0].local_url).unwrap(),
        sibling: Some((
            format!("http://api-machine-b.localhost:{}", address.port()),
            "http://api.localhost:7462".into(),
        )),
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
#[test]
fn rewrite_location_rejects_every_loopback_spelling() {
    let local_app = Url::parse("http://web.localhost:7462").unwrap();
    let relay_url = Url::parse("http://app-machine-b.localhost:9000").unwrap();
    for location in [
        "http://[::1]:9999/",
        "http://[0:0:0:0:0:0:0:1]:9999/admin",
        "https://[::1]/",
        "//[::1]:9999/x",
        "http://[::ffff:127.0.0.1]:9999/",
        "http://[::ffff:7f00:1]/",
        "http://[::]:9999/",
        "http://127.0.0.1:9999/",
        "http://127.8.9.10/",
        "http://0x7f.1:9999/",
        "http://2130706433:9999/",
        "http://0.0.0.0:9999/",
        "http://localhost:9999/",
        "http://LOCALHOST:9999/",
        "http://localhost.:9999/",
        "http://other.localhost:7462/",
        "http://Other.LocalHost.:7462/",
        "http://web.localhost:7463/",
        "https://web.localhost:7462/",
        "//localhost:9999/",
        "http://[::127.0.0.1]:9999/",
        "http://[::ffff:0.0.0.0]:9999/",
        "\\\\[::1]:9999/",
        "/\\localhost:9999/",
        "\\/127.0.0.1:9999/",
        " //[::1]:9999/",
        "\t//localhost:9999/",
        "http:\\\\[::1]:9999/",
        "https:/localhost:9999/",
    ] {
        assert!(
            rewrite_location(location, &local_app, &relay_url).is_err(),
            "{location} should be rejected"
        );
    }
}
#[test]
fn rewrite_location_keeps_same_app_relative_and_external_redirects() {
    let local_app = Url::parse("http://web.localhost:7462").unwrap();
    let relay_url = Url::parse("http://app-machine-b.localhost:9000").unwrap();
    let rewrite = |v: &str| rewrite_location(v, &local_app, &relay_url).unwrap();
    assert_eq!(
        rewrite("http://web.localhost:7462/done?q=1"),
        "http://app-machine-b.localhost:9000/done?q=1"
    );
    assert_eq!(
        rewrite("http://WEB.localhost:7462/a"),
        "http://app-machine-b.localhost:9000/a"
    );
    for location in [
        "//web.localhost:7462/b",
        "\\\\web.localhost:7462/b",
        "/\\web.localhost:7462/b",
    ] {
        assert_eq!(rewrite(location), "http://app-machine-b.localhost:9000/b");
    }
    for location in [
        "/relative?x=1",
        "relative/path",
        "?q=only",
        "https://example.com/login",
        "https://[2001:db8::1]/",
        "http://192.168.1.10:8080/",
        "https://localhost.example.com/",
        "https://notlocalhost/",
        "http://app-machine-b.localhost:9000/self",
        "mailto:someone@example.com",
    ] {
        assert_eq!(rewrite(location), location);
    }
}
#[test]
fn rewrite_location_with_https_relay_only_rewrites_explicit_app_authority() {
    let local_app = Url::parse("http://web.localhost:7462").unwrap();
    let relay_url = Url::parse("https://app-machine-b.localhost:9443").unwrap();
    let rewrite = |v: &str| rewrite_location(v, &local_app, &relay_url);
    for location in [
        "http:example.com/login",
        "http:/example.com/login",
        "/relative",
        "https://example.com/",
    ] {
        assert_eq!(rewrite(location).unwrap(), location);
    }
    for location in [
        "//web.localhost:7462/path",
        "http://web.localhost:7462/path",
        "http:\\\\web.localhost:7462/path",
        "http:web.localhost:7462/path",
        "http:/web.localhost:7462/path",
    ] {
        assert_eq!(
            rewrite(location).unwrap(),
            "https://app-machine-b.localhost:9443/path"
        );
    }
    for location in [
        "http:localhost:9999/",
        "http:/[::1]:9999/",
        "//localhost:9999/",
    ] {
        assert!(rewrite(location).is_err(), "{location} should be rejected");
    }
}
fn temp_state() -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "porch-discovery-client-{}",
        super::super::server::random().unwrap()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}
/// A paired machine that accepts TCP but never completes TLS: if anything tried
/// it, the caller would wait out the request timeout. Counts every connection.
async fn silent_peer() -> (Peer, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut peer = fixture_peer(
        format!("https://{}/", listener.local_addr().unwrap()),
        "0".repeat(64),
    );
    peer.machine = Machine {
        id: "machine-offline".into(),
        name: "Offline Box".into(),
    };
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = hits.clone();
    let task = tokio::spawn(async move {
        let mut held = Vec::new();
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            seen.fetch_add(1, Ordering::Relaxed);
            held.push(stream);
        }
    });
    (peer, hits, task)
}
#[tokio::test]
async fn opening_one_peer_never_contacts_an_unreachable_other_peer() {
    let dir = temp_state();
    let (reachable, reachable_hits, good_task) = tls_fixture(
        serde_json::to_vec(&fixture_catalog()).unwrap(),
        StatusCode::OK,
    )
    .await;
    let (offline, offline_hits, offline_task) = silent_peer().await;
    // The offline peer is enrolled first, so a query over every peer meets it first.
    upsert_peer_in(&dir, offline.clone()).unwrap();
    upsert_peer_in(&dir, reachable.clone()).unwrap();

    let result = refresh_peer_in(&dir, "machine-local", "machine-b")
        .await
        .unwrap();
    assert_eq!(result.machine.id, "machine-b");
    assert_eq!(result.catalog.unwrap().apps[0].name, "web");
    assert_eq!(reachable_hits.load(Ordering::Relaxed), 1);
    assert_eq!(
        offline_hits.load(Ordering::Relaxed),
        0,
        "opening machine-b must not wait on another paired peer"
    );
    // Like a snapshot, the authenticated refresh is persisted for the selected
    // peer only; the untouched peer keeps its stored record.
    let stored = peers_in(&dir).unwrap();
    let b = stored.iter().find(|p| p.machine.id == "machine-b").unwrap();
    assert!(b.last_seen > 0);
    assert_eq!(b.endpoint, reachable.endpoint);
    let off = stored
        .iter()
        .find(|p| p.machine.id == "machine-offline")
        .unwrap();
    assert_eq!(off.last_seen, 0);
    assert_eq!(off.endpoint, offline.endpoint);

    good_task.abort();
    offline_task.abort();
    let _ = fs::remove_dir_all(&dir);
}
#[tokio::test]
async fn refreshing_a_peer_refuses_unknown_and_local_machine_ids() {
    let dir = temp_state();
    let (offline, hits, task) = silent_peer().await;
    upsert_peer_in(&dir, offline).unwrap();
    for (local, wanted) in [
        ("machine-local", "machine-missing"),
        ("machine-offline", "machine-offline"),
    ] {
        let error = refresh_peer_in(&dir, local, wanted).await.unwrap_err();
        assert!(error.to_string().contains("paired peer unavailable"));
    }
    assert_eq!(hits.load(Ordering::Relaxed), 0);
    task.abort();
    let _ = fs::remove_dir_all(&dir);
}

/// A paired agent as the real one runs it: TLS offering h2 and http/1.1, the
/// auto (HTTP/1.1 + HTTP/2) server, and at most `slots` connections at once
/// (the rest are dropped, as the agent drops connections over its limit).
/// `/big` streams `big` bytes; anything else answers "ok". Counts connections.
async fn agent_fixture(
    slots: usize,
    big: usize,
    alpn: Vec<Vec<u8>>,
) -> (Peer, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let cert = rcgen::generate_simple_self_signed(vec!["porch-peer.local".into()]).unwrap();
    let pin = hex_pin(&Sha256::digest(cert.cert.der()).into());
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
        cert.key_pair.serialize_der(),
    ));
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![cert.cert.der().clone()], key)
    .unwrap();
    config.alpn_protocols = alpn;
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer = fixture_peer(format!("https://{}/", listener.local_addr().unwrap()), pin);
    let connections = Arc::new(AtomicUsize::new(0));
    let counter = connections.clone();
    let slots = Arc::new(tokio::sync::Semaphore::new(slots));
    let task = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            counter.fetch_add(1, Ordering::SeqCst);
            let Ok(permit) = slots.clone().try_acquire_owned() else {
                drop(stream);
                continue;
            };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let Ok(stream) = acceptor.accept(stream).await else {
                    return;
                };
                let service =
                    hyper::service::service_fn(move |req: Request<Incoming>| async move {
                        if req.uri().path().ends_with("/big") {
                            let chunk = bytes::Bytes::from(vec![b'x'; 64 * 1024]);
                            let chunks = big.div_ceil(chunk.len());
                            let body = stream::iter(
                                (0..chunks).map(move |_| Ok::<_, Infallible>(chunk.clone())),
                            );
                            return Ok::<_, Infallible>(Response::new(Body::from_stream(body)));
                        }
                        if req.uri().path().ends_with("/upload") {
                            use http_body_util::BodyExt;
                            let n = req.into_body().collect().await.map(|b| b.to_bytes().len());
                            return Ok(Response::new(Body::from(format!("{}", n.unwrap_or(0)))));
                        }
                        if req.uri().path().ends_with("/ignore-upload") {
                            // Answers without reading the body, holding it open.
                            let body = req.into_body();
                            tokio::spawn(async move {
                                tokio::time::sleep(Duration::from_secs(30)).await;
                                drop(body);
                            });
                            return Ok(Response::new(Body::from("ignored")));
                        }
                        Ok(Response::new(Body::from("ok")))
                    });
                let mut builder =
                    hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
                super::super::server::http2_settings(builder.http2());
                let _ = builder
                    .serve_connection_with_upgrades(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    (peer, connections, task)
}
fn proxy_uri(peer: &Peer, path: &str) -> hyper::Uri {
    format!("{}v1/apps/web/proxy{path}", peer.endpoint)
        .parse()
        .unwrap()
}
fn get(peer: &Peer, path: &str) -> Request<Body> {
    Request::get(proxy_uri(peer, path))
        .header(header::AUTHORIZATION, format!("Bearer {}", peer.token))
        .body(Body::empty())
        .unwrap()
}

/// A page's burst on a cold connection (a restarted peer, a woken laptop, an
/// idle timeout) waits for one connection instead of dialing one per request:
/// the agent's connection limit would drop the extra dials as 502s.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cold_burst_shares_one_agent_connection() {
    let (peer, connections, task) =
        agent_fixture(8, 0, vec![b"h2".to_vec(), b"http/1.1".to_vec()]).await;
    let client = relay_client(&peer.fingerprint).unwrap();
    let burst = (0..100).map(|i| {
        let client = client.clone();
        let request = get(&peer, &format!("/m{i}.js"));
        async move { client.send(request).await.map(|r| r.status()) }
    });
    let results = futures_util::future::join_all(burst).await;
    let failed = results
        .iter()
        .filter(|r| !matches!(r, Ok(s) if *s == StatusCode::OK))
        .count();
    assert_eq!(failed, 0, "{failed} of 100 requests failed");
    let opened = connections.load(Ordering::SeqCst);
    assert_eq!(opened, 1, "{opened} connections for one burst");
    task.abort();
}

/// Responses the browser stopped reading (a paused video or download) must not
/// stall every other request to that machine on the shared connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stalled_responses_do_not_block_the_shared_connection() {
    let (peer, connections, task) = agent_fixture(
        8,
        8 * 1024 * 1024,
        vec![b"h2".to_vec(), b"http/1.1".to_vec()],
    )
    .await;
    let client = relay_client(&peer.fingerprint).unwrap();
    // Several large responses whose bodies nobody reads.
    let mut stalled = Vec::new();
    for _ in 0..4 {
        let response = client.send(get(&peer, "/big")).await.unwrap();
        assert_eq!(response.version(), hyper::Version::HTTP_2);
        stalled.push(response);
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let fetch = async {
        use http_body_util::BodyExt;
        let response = client.send(get(&peer, "/small")).await.unwrap();
        response.into_body().collect().await.unwrap().to_bytes()
    };
    let body = tokio::time::timeout(Duration::from_secs(3), fetch)
        .await
        .expect("a small response arrives while others are stalled");
    assert_eq!(&body[..], b"ok");
    assert_eq!(connections.load(Ordering::SeqCst), 1);
    drop(stalled);
    task.abort();
}

/// Uploads an app is not reading must not stall other uploads to that
/// machine: the agent's connection window is far above its per-request one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unread_upload_does_not_block_other_uploads() {
    let (peer, _, task) = agent_fixture(8, 0, vec![b"h2".to_vec(), b"http/1.1".to_vec()]).await;
    let client = relay_client(&peer.fingerprint).unwrap();
    let post = |path: &str, size: usize| {
        Request::post(proxy_uri(&peer, path))
            .header(header::AUTHORIZATION, format!("Bearer {}", peer.token))
            .body(Body::from(vec![b'u'; size]))
            .unwrap()
    };
    let mut held = Vec::new();
    for _ in 0..3 {
        let client = client.clone();
        let request = post("/ignore-upload", 4 * 1024 * 1024);
        held.push(tokio::spawn(async move { client.send(request).await }));
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let upload = async {
        use http_body_util::BodyExt;
        let response = client.send(post("/upload", 512 * 1024)).await.unwrap();
        response.into_body().collect().await.unwrap().to_bytes()
    };
    let body = tokio::time::timeout(Duration::from_secs(3), upload)
        .await
        .expect("an upload completes while others are unread");
    assert_eq!(&body[..], b"524288");
    for h in held {
        h.abort();
    }
    task.abort();
}

/// Agents from before HTTP/2 keep working over HTTP/1.1.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agents_without_http2_are_relayed_over_http1() {
    let (peer, _, task) = agent_fixture(64, 0, vec![b"http/1.1".to_vec()]).await;
    let client = relay_client(&peer.fingerprint).unwrap();
    for _ in 0..2 {
        let burst = (0..20).map(|i| {
            let client = client.clone();
            let request = get(&peer, &format!("/m{i}.js"));
            async move {
                client
                    .send(request)
                    .await
                    .map(|r| (r.status(), r.version()))
            }
        });
        for result in futures_util::future::join_all(burst).await {
            assert_eq!(result.unwrap(), (StatusCode::OK, hyper::Version::HTTP_11));
        }
    }
    task.abort();
}

/// A burst against a machine that is down fails together after one attempt,
/// not one connection timeout per request.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_burst_against_an_unreachable_agent_fails_fast() {
    // Accepts TCP but never answers TLS.
    let (peer, hits, task) = silent_peer().await;
    let client = relay_client(&"0".repeat(64)).unwrap();
    let started = std::time::Instant::now();
    let burst = (0..50).map(|i| {
        let client = client.clone();
        let request = get(&peer, &format!("/m{i}.js"));
        async move { client.send(request).await }
    });
    let results = futures_util::future::join_all(burst).await;
    assert!(results.iter().all(|r| r.is_err()));
    assert_eq!(hits.load(Ordering::Relaxed), 1);
    assert!(started.elapsed() < DIAL_TIMEOUT + Duration::from_secs(3));
    // The machine's other address is still tried at once.
    let (other, other_hits, other_task) = silent_peer().await;
    let _ = tokio::time::timeout(
        Duration::from_millis(500),
        client.send(get(&other, "/m.js")),
    )
    .await;
    assert_eq!(other_hits.load(Ordering::Relaxed), 1);
    task.abort();
    other_task.abort();
}

/// Redirects and cookies with UTF-8 bytes: the redirect is still checked (and
/// rewritten), and the cookie keeps its value without its Domain attribute.
#[tokio::test]
async fn relay_handles_non_ascii_redirects_and_cookies() {
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
    let response = client.get(url.join("/utf8").unwrap()).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(
        response.headers()[header::LOCATION],
        url.join("/caf%C3%A9?q=%E2%9C%93").unwrap().as_str()
    );
    assert_eq!(
        response.headers()[header::SET_COOKIE].as_bytes(),
        b"name=J\xc3\xbcrgen; Path=/"
    );
    // A loopback redirect is refused however it is spelled.
    let response = client
        .get(url.join("/utf8?loopback").unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    relay.abort();
    backend.abort();
}

/// Another app on the same machine may call this one (the app sees that
/// app's origin on its own machine); other origins stay refused.
#[tokio::test]
async fn relay_accepts_sibling_apps_of_the_same_machine_only() {
    let (url, relay, backend) = relay_fixture().await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .resolve(
            url.host_str().unwrap(),
            SocketAddr::from(([127, 0, 0, 1], url.port().unwrap())),
        )
        .build()
        .unwrap();
    let sibling = format!("http://api-machine-b.localhost:{}", url.port().unwrap());
    let data: serde_json::Value = client
        .get(url.join("/where").unwrap())
        .header(header::ORIGIN, &sibling)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(data["app_origin"], "http://api.localhost:7462");
    for foreign in [
        format!("http://api-machine-c.localhost:{}", url.port().unwrap()),
        format!("http://api-machine-b.localhost:{}", url.port().unwrap() + 1),
        "https://evil.example".to_owned(),
        "null".to_owned(),
    ] {
        let status = client
            .get(url.clone())
            .header(header::ORIGIN, &foreign)
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status, StatusCode::FORBIDDEN, "{foreign}");
    }
    relay.abort();
    backend.abort();
}

/// The response may start long after the request did, as long as the wait
/// since the last upload data stays under the limit.
#[tokio::test]
async fn response_timeout_counts_from_the_last_upload_data() {
    use http_body_util::BodyExt;
    let idle = Duration::from_millis(300);
    // No upload: a response after 100 ms arrives; one that never comes ends.
    let (_, last) = Watched::new(Body::empty());
    let quick = within_idle(tokio::time::sleep(Duration::from_millis(100)), &last, idle).await;
    assert!(quick.is_some());
    let (_, last) = Watched::new(Body::empty());
    let started = tokio::time::Instant::now();
    let never = within_idle(std::future::pending::<()>(), &last, idle).await;
    assert!(never.is_none());
    assert!(started.elapsed() >= idle);
    // A slow upload keeps a slow response alive well past the idle limit.
    let chunks = stream::iter(0..8).then(|_| async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        Ok::<_, Infallible>(bytes::Bytes::from_static(b"x"))
    });
    let (mut body, last) = Watched::new(Body::from_stream(chunks));
    let upload = async move {
        while body.frame().await.is_some() {}
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let started = tokio::time::Instant::now();
    assert!(within_idle(upload, &last, idle).await.is_some());
    assert!(started.elapsed() > idle * 3);
}

#[test]
fn stored_endpoints_are_normalized() {
    let dir = temp_state();
    let (pin, token) = ("0".repeat(64), "a".repeat(64));
    let mut peer = fixture_peer("https://10.0.0.5:7463".into(), pin);
    peer.token = token;
    assert_eq!(
        upsert_peer_in(&dir, peer.clone()).unwrap().endpoint,
        "https://10.0.0.5:7463/"
    );
    // Files written before normalization are read in the canonical form.
    let mut raw: Vec<serde_json::Value> =
        serde_json::from_slice(&fs::read(dir.join(PEERS_FILE)).unwrap()).unwrap();
    raw[0]["endpoint"] = "https://10.0.0.5".into();
    fs::write(dir.join(PEERS_FILE), serde_json::to_vec(&raw).unwrap()).unwrap();
    assert_eq!(peers_in(&dir).unwrap()[0].endpoint, "https://10.0.0.5/");
    let _ = fs::remove_dir_all(&dir);
}

/// The cached list follows every change to the peers file, including a
/// re-pairing that only changes the token (same file length).
#[test]
fn cached_peers_follow_every_change_to_the_file() {
    let dir = temp_state();
    let mut peer = fixture_peer("https://10.0.0.5:7463/".into(), "0".repeat(64));
    upsert_peer_in(&dir, peer.clone()).unwrap();
    let first = shared_peers_in(dir.clone()).unwrap();
    assert_eq!(first[0].token, "a".repeat(64));
    assert!(Arc::ptr_eq(&first, &shared_peers_in(dir.clone()).unwrap()));
    peer.token = "b".repeat(64);
    upsert_peer_in(&dir, peer).unwrap();
    assert_eq!(
        shared_peers_in(dir.clone()).unwrap()[0].token,
        "b".repeat(64)
    );
    remove_peer_in(&dir, "machine-b").unwrap();
    assert!(shared_peers_in(dir.clone()).unwrap().is_empty());
    let _ = fs::remove_dir_all(&dir);
}
