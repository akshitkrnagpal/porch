use futures_util::{SinkExt, StreamExt};
#[tokio::test]
async fn actual_http_tls_ws_sse() {
    let dir = std::env::temp_dir().join(format!("porch-network-{}", std::process::id()));
    std::env::set_var("PORCH_STATE_DIR", &dir);
    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up = upstream.local_addr().unwrap().port();
    let app = axum::Router::new()
        .route("/", axum::routing::get(|| async { "hello" }))
        .route(
            "/sse",
            axum::routing::get(|| async {
                axum::response::Sse::new(futures_util::stream::iter([
                    Ok::<_, std::convert::Infallible>(
                        axum::response::sse::Event::default().data("first"),
                    ),
                    Ok(axum::response::sse::Event::default().data("second")),
                ]))
            }),
        )
        .route(
            "/ws",
            axum::routing::get(|ws: axum::extract::WebSocketUpgrade| async {
                ws.on_upgrade(|mut s| async move {
                    while let Some(Ok(m)) = s.recv().await {
                        if s.send(m).await.is_err() {
                            break;
                        }
                    }
                })
            }),
        );
    let server = tokio::spawn(async move { axum::serve(upstream, app).await.unwrap() });
    porch::state::mutate(|s| {
        s.routes.push(porch::state::Route {
            name: "app".into(),
            target: format!("http://127.0.0.1:{up}"),
            pid: None,
        })
    })
    .unwrap();
    for tls in [false, true] {
        let reserve = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = reserve.local_addr().unwrap().port();
        drop(reserve);
        let proxy = tokio::spawn(porch::proxy::serve(
            "127.0.0.1".into(),
            port,
            tls,
            None,
            None,
        ));
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .no_proxy()
            .resolve(
                "app.localhost",
                format!("127.0.0.1:{port}").parse().unwrap(),
            )
            .build()
            .unwrap();
        let url = format!(
            "{}://app.localhost:{port}",
            if tls { "https" } else { "http" }
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Ok(r) = client.get(&url).send().await {
                assert_eq!(r.text().await.unwrap(), "hello");
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        let s = client.get(format!("{url}/sse")).send().await.unwrap();
        assert!(s.headers()["content-type"]
            .to_str()
            .unwrap()
            .contains("text/event-stream"));
        assert!(s.text().await.unwrap().contains("data: second"));
        if !tls {
            let (mut ws, _) = tokio_tungstenite::connect_async(
                format!("ws://127.0.0.1:{port}/ws").replace("127.0.0.1", "app.localhost"),
            )
            .await
            .unwrap();
            ws.send(tokio_tungstenite::tungstenite::Message::Text("echo".into()))
                .await
                .unwrap();
            assert_eq!(
                ws.next().await.unwrap().unwrap().into_text().unwrap(),
                "echo"
            );
        }
        proxy.abort();
    }
    server.abort();
    std::fs::remove_dir_all(dir).unwrap();
}
