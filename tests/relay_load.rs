//! Page loads through paired-machine URLs. Two real Porch installations on
//! loopback; machine B serves a Vite-like dev server (a module tree whose
//! imports are only discovered after each module arrives), and a browser model
//! loads it from B's own proxy and from A's proxy through the relay. A delay
//! line in front of B's agent simulates network round-trip time.
//!
//! The benchmark is ignored by default:
//! cargo test --release --test relay_load -- --ignored --nocapture
//! RELAY_BENCH_RTT_MS=20 RELAY_BENCH_BROWSER=h2 RELAY_BENCH_MODULES=400 cargo test --release ...
mod common;
use common::free_port;
use std::{
    path::Path,
    process::Stdio,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

const FANOUT: usize = 4;

fn env_number(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn porch(dir: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_porch"));
    command
        .env("PORCH_STATE_DIR", dir)
        .env("PORCH_CLEAN_URLS", "0")
        .env("PORCH_SYNC_HOSTS", "0")
        .env("HOSTNAME", dir.file_name().unwrap())
        .env("DISPLAY", "")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}
async fn ok(dir: &Path, args: &[&str]) -> String {
    let output = tokio::time::timeout(Duration::from_secs(30), porch(dir).args(args).output())
        .await
        .expect("CLI completes")
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

struct Nodes(Vec<std::path::PathBuf>);
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
                    .env("PORCH_CLEAN_URLS", "0")
                    .env("PORCH_SYNC_HOSTS", "0")
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Module `i` imports modules `i*FANOUT+1 ..= i*FANOUT+FANOUT`; its body lists
/// them, padded to a typical transformed-module size. Sent the way Vite sends
/// source modules: no-cache with an ETag, so every load reaches the server.
async fn vite_like(modules: usize) -> (u16, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = axum::Router::new().route(
        "/src/{file}",
        axum::routing::get(
            move |axum::extract::Path(file): axum::extract::Path<String>| async move {
                let i: usize = file.trim_end_matches(".js").parse().unwrap_or(usize::MAX);
                if i >= modules {
                    return (
                        axum::http::StatusCode::NOT_FOUND,
                        Default::default(),
                        String::new(),
                    );
                }
                let mut body = String::new();
                for c in (i * FANOUT + 1..=i * FANOUT + FANOUT).filter(|c| *c < modules) {
                    body.push_str(&format!("import \"/src/{c}.js\";\n"));
                }
                body.push_str(&format!("export const m{i} = \"{}\";\n", "x".repeat(6000)));
                let mut headers = axum::http::HeaderMap::new();
                headers.insert("content-type", "text/javascript".parse().unwrap());
                headers.insert("cache-control", "no-cache".parse().unwrap());
                headers.insert("etag", format!("W/\"{i}\"").parse().unwrap());
                (axum::http::StatusCode::OK, headers, body)
            },
        ),
    );
    (
        port,
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }),
    )
}

/// Forwards TCP to `target`, delivering every chunk `one_way` later in each
/// direction (order preserved), like a network link with that latency. Counts
/// the connections made across it.
async fn delay_line(
    target: u16,
    one_way: Duration,
) -> (u16, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let connections = Arc::new(AtomicUsize::new(0));
    let counter = connections.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    async fn pump(
        mut from: tokio::net::tcp::OwnedReadHalf,
        mut to: tokio::net::tcp::OwnedWriteHalf,
        one_way: Duration,
    ) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(Instant, Vec<u8>)>();
        let writer = tokio::spawn(async move {
            while let Some((at, bytes)) = rx.recv().await {
                // Timers tick in whole milliseconds and round up, which would
                // inflate every delay; sleep most of the way, then yield until due.
                if let Some(early) = at.checked_sub(Duration::from_millis(2)) {
                    if early > Instant::now() {
                        tokio::time::sleep_until(early.into()).await;
                    }
                }
                while Instant::now() < at {
                    tokio::task::yield_now().await;
                }
                if to.write_all(&bytes).await.is_err() {
                    break;
                }
            }
            let _ = to.shutdown().await;
        });
        let mut buf = vec![0u8; 64 * 1024];
        while let Ok(n) = from.read(&mut buf).await {
            if n == 0
                || tx
                    .send((Instant::now() + one_way, buf[..n].to_vec()))
                    .is_err()
            {
                break;
            }
        }
        drop(tx);
        let _ = writer.await;
    }
    let task = tokio::spawn(async move {
        loop {
            let (inbound, _) = listener.accept().await.unwrap();
            counter.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let Ok(outbound) = tokio::net::TcpStream::connect(("127.0.0.1", target)).await
                else {
                    return;
                };
                let _ = inbound.set_nodelay(true);
                let _ = outbound.set_nodelay(true);
                let (ir, iw) = inbound.into_split();
                let (or, ow) = outbound.into_split();
                tokio::join!(pump(ir, ow, one_way), pump(or, iw, one_way));
            });
        }
    });
    (port, connections, task)
}

/// Loads the module tree the way a browser does, each module's imports
/// requested once it arrives. Over HTTP/1.1 at most six requests are in flight
/// per host; over HTTP/2 (an https Porch proxy) they share one connection.
/// An `https` base must name the CA that signs the proxy's certificates.
async fn load_page(base: &str, modules: usize, h2: bool) -> (Duration, usize) {
    load_page_with(base, modules, h2, None).await
}
async fn load_page_with(
    base: &str,
    modules: usize,
    h2: bool,
    ca: Option<&Path>,
) -> (Duration, usize) {
    let parsed = url::Url::parse(base).unwrap();
    let builder = reqwest::Client::builder().no_proxy();
    let builder = match (h2, ca) {
        // Over TLS the protocol is negotiated (ALPN), as a browser does.
        (true, Some(ca)) => builder.add_root_certificate(
            reqwest::Certificate::from_pem(&std::fs::read(ca).unwrap()).unwrap(),
        ),
        (true, None) => builder.http2_prior_knowledge(),
        (false, _) => builder.http1_only().pool_max_idle_per_host(6),
    };
    let client = builder
        .resolve(
            parsed.host_str().unwrap(),
            std::net::SocketAddr::from(([127, 0, 0, 1], parsed.port().unwrap())),
        )
        .build()
        .unwrap();
    let slots = Arc::new(tokio::sync::Semaphore::new(if h2 { 1000 } else { 6 }));
    let started = Instant::now();
    let mut pending = tokio::task::JoinSet::new();
    let fetch = |i: usize| {
        let client = client.clone();
        let slots = slots.clone();
        let url = format!("{base}/src/{i}.js");
        async move {
            let _slot = slots.acquire_owned().await.unwrap();
            let response = client.get(&url).send().await.unwrap();
            if h2 {
                assert_eq!(response.version(), reqwest::Version::HTTP_2, "{url}");
            }
            let status = response.status();
            if status != 200 {
                panic!(
                    "{url}: {status} {}",
                    response.text().await.unwrap_or_default()
                );
            }
            let body = response.text().await.unwrap();
            body.lines()
                .filter_map(|l| l.strip_prefix("import \"/src/"))
                .filter_map(|l| l.strip_suffix(".js\";"))
                .map(|n| n.parse::<usize>().unwrap())
                .collect::<Vec<_>>()
        }
    };
    pending.spawn(fetch(0));
    let mut loaded = 0;
    while let Some(children) = pending.join_next().await {
        loaded += 1;
        for c in children.unwrap() {
            pending.spawn(fetch(c));
        }
    }
    assert_eq!(loaded, modules);
    (started.elapsed(), loaded)
}

/// Two paired nodes; B shares a Vite-like app and A reaches B's agent through
/// a delay line with the given round-trip time.
struct Pair {
    a: std::path::PathBuf,
    /// A's and B's proxy ports, and B's agent port.
    pa: String,
    pb: String,
    agent_b: String,
    link_port: u16,
    link_connections: Arc<AtomicUsize>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    _nodes: Nodes,
}
impl Drop for Pair {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}
impl Pair {
    fn local(&self) -> String {
        format!("http://web.localhost:{}", self.pb)
    }
    fn remote(&self) -> String {
        format!("http://web.node-b.localhost:{}", self.pa)
    }
}
async fn pair(name: &str, modules: usize, rtt: Duration) -> Pair {
    pair_with(name, modules, rtt, false).await
}
/// `https_a` serves machine A's proxy over HTTPS (its CA is not trusted
/// system-wide; clients load `ca.pem` from A's state directory).
async fn pair_with(name: &str, modules: usize, rtt: Duration, https_a: bool) -> Pair {
    let root = std::env::temp_dir().join(format!("porch-{name}-{}", std::process::id()));
    let (a, b) = (root.join("node-a"), root.join("node-b"));
    let nodes = Nodes(vec![a.clone(), b.clone()]);
    let (app_port, app) = vite_like(modules).await;
    let (pa, pb) = (free_port().to_string(), free_port().to_string());
    for (node, port, https) in [(&a, &pa, https_a), (&b, &pb, false)] {
        let mut args = vec!["proxy", "start", "--unprivileged", "--port", port];
        args.extend(["--bind", "127.0.0.1"]);
        if https {
            args.extend(["--https", "--skip-trust"]);
        } else {
            args.push("--no-tls");
        }
        ok(node, &args).await;
    }
    ok(&b, &["alias", "web", &app_port.to_string()]).await;
    let (agent_a, agent_b) = (free_port().to_string(), free_port().to_string());
    let mut host = porch(&b)
        .args(["pair", "--bind", "127.0.0.1", "--port", &agent_b])
        .spawn()
        .unwrap();
    let mut lines = tokio::io::BufReader::new(host.stdout.take().unwrap()).lines();
    let line = tokio::time::timeout(Duration::from_secs(20), lines.next_line())
        .await
        .expect("code shown")
        .unwrap()
        .unwrap();
    let code = line.strip_prefix("Pairing code: ").expect(&line).to_owned();
    ok(
        &a,
        &[
            "pair",
            &code,
            "--host",
            &format!("127.0.0.1:{agent_b}"),
            "--bind",
            "127.0.0.1",
            "--port",
            &agent_a,
        ],
    )
    .await;
    let _ = tokio::time::timeout(Duration::from_secs(20), host.wait()).await;
    // Route A's relay through the simulated link (the pin still authenticates B).
    let (link_port, link_connections, link) = delay_line(agent_b.parse().unwrap(), rtt / 2).await;
    let peers_file = a.join("discovery-peers.json");
    let peers = std::fs::read_to_string(&peers_file).unwrap();
    let peers = peers.replace(
        &format!("127.0.0.1:{agent_b}"),
        &format!("127.0.0.1:{link_port}"),
    );
    std::fs::write(&peers_file, peers).unwrap();
    Pair {
        a,
        pa,
        pb,
        agent_b,
        link_port,
        link_connections,
        tasks: vec![app, link],
        _nodes: nodes,
    }
}

/// An https Porch proxy speaks HTTP/2 to the browser, which then requests a
/// page's modules all at once. The relay must carry that burst on one agent
/// connection: one TLS connection per request ran into the agent's connection
/// limit and failed modules with 502.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_carries_a_module_burst_on_one_agent_connection() {
    let modules = 400;
    let pair = pair("relay-burst", modules, Duration::ZERO).await;
    for _ in 0..2 {
        load_page(&pair.remote(), modules, true).await;
    }
    let connections = pair.link_connections.load(Ordering::SeqCst);
    assert!(connections <= 2, "{connections} agent connections");
}

/// The same burst from a real browser's point of view: an HTTPS proxy, HTTP/2
/// negotiated over TLS, and the URL `porch open --peer` prints.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn https_peer_url_loads_a_module_burst_over_http2() {
    let modules = 400;
    let pair = pair_with("relay-https", modules, Duration::ZERO, true).await;
    let url = ok(&pair.a, &["open", "web", "--peer", "node-b", "--url-only"]).await;
    let url = url.trim();
    assert_eq!(url, format!("https://web.node-b.localhost:{}", pair.pa));
    load_page_with(url, modules, true, Some(&pair.a.join("ca.pem"))).await;
    let connections = pair.link_connections.load(Ordering::SeqCst);
    assert!(connections <= 2, "{connections} agent connections");
}

/// Listing a machine's apps (the tray runs `porch list --all`) and opening one
/// go through the proxy, so they warm the connection the page then uses: the
/// whole sequence crosses the network on a single connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn listing_apps_warms_the_connection_the_page_uses() {
    let modules = 50;
    let pair = pair("relay-warm", modules, Duration::ZERO).await;
    let listed: serde_json::Value =
        serde_json::from_str(&ok(&pair.a, &["list", "--all", "--json"]).await).unwrap();
    let apps = &listed["peers"][0]["catalog"]["apps"];
    assert_eq!(apps[0]["name"], "web", "{listed}");
    assert_eq!(pair.link_connections.load(Ordering::SeqCst), 1);
    let url = ok(&pair.a, &["open", "web", "--peer", "node-b", "--url-only"]).await;
    assert_eq!(url.trim(), pair.remote());
    load_page(&pair.remote(), modules, false).await;
    assert_eq!(pair.link_connections.load(Ordering::SeqCst), 1);
}

/// The proxy lists paired machines only for Porch on this computer: the
/// private token is required, and a browser page can never read the list.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_proxy_peer_list_needs_the_local_token_and_refuses_browsers() {
    let pair = pair("relay-peers-auth", 1, Duration::ZERO).await;
    let token = std::fs::read_to_string(pair.a.join("proxy.hosts-sync-token")).unwrap();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let url = format!("http://127.0.0.1:{}/_porch/peers.json", pair.pa);
    let status = |r: reqwest::RequestBuilder| async move { r.send().await.unwrap().status() };
    assert_eq!(status(client.get(&url)).await, 403);
    assert_eq!(
        status(client.get(&url).header("x-porch-token", "0".repeat(64))).await,
        403
    );
    assert_eq!(
        status(
            client
                .get(&url)
                .header("x-porch-token", token.trim())
                .header("origin", "http://evil.example")
        )
        .await,
        403
    );
    assert_eq!(
        status(
            client
                .get(&url)
                .header("x-porch-token", token.trim())
                .header("sec-fetch-site", "cross-site")
        )
        .await,
        403
    );
    // Only the proxy's own name: an app's hostname is routed to the app as usual.
    let app = format!("http://web.localhost:{}/_porch/peers.json", pair.pa);
    let routed = reqwest::Client::builder()
        .no_proxy()
        .resolve(
            "web.localhost",
            format!("127.0.0.1:{}", pair.pa).parse().unwrap(),
        )
        .build()
        .unwrap()
        .get(&app)
        .header("x-porch-token", token.trim())
        .send()
        .await
        .unwrap();
    assert_ne!(routed.status(), 200);
    let listed: serde_json::Value = client
        .get(&url)
        .header("x-porch-token", token.trim())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listed["peers"][0]["slug"], "node-b", "{listed}");
    assert!(!listed.to_string().contains(token.trim()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "benchmark; run with --ignored --nocapture"]
async fn relayed_vite_page_load_vs_local() {
    let modules = env_number("RELAY_BENCH_MODULES", 300) as usize;
    let rtt = Duration::from_millis(env_number("RELAY_BENCH_RTT_MS", 0));
    let runs = env_number("RELAY_BENCH_RUNS", 5);
    let h2 = std::env::var("RELAY_BENCH_BROWSER").as_deref() == Ok("h2");
    let pair = pair("relay-bench", modules, rtt).await;
    if std::env::var("RELAY_BENCH_HOPS").is_ok() {
        hops(&pair).await;
    }
    println!(
        "\n{modules} modules, fan-out {FANOUT}, simulated RTT {rtt:?}, browser {}, {runs} runs",
        if h2 { "HTTP/2" } else { "HTTP/1.1" }
    );
    for (label, base) in [("local ", pair.local()), ("remote", pair.remote())] {
        // The first load opens connections and fetches the catalog.
        let (cold, _) = load_page(&base, modules, h2).await;
        let mut times = Vec::new();
        for _ in 0..runs {
            times.push(load_page(&base, modules, h2).await.0);
        }
        times.sort();
        let median = times[times.len() / 2];
        println!(
            "{label}  cold {:>8.1?}  median {:>8.1?}  min {:>8.1?}  max {:>8.1?}",
            cold,
            median,
            times[0],
            times[times.len() - 1]
        );
    }
}

/// Sequential single-request latency at each hop, to locate per-request cost.
async fn hops(pair: &Pair) {
    let (agent_b, link) = (&pair.agent_b, pair.link_port);
    let peers: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(pair.a.join("discovery-peers.json")).unwrap(),
    )
    .unwrap();
    let token = peers[0]["token"].as_str().unwrap().to_owned();
    let catalog: serde_json::Value = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap()
        .get(format!("https://127.0.0.1:{agent_b}/v1/catalog"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let app_id = catalog["apps"][0]["id"].as_str().unwrap().to_owned();
    let agent = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .http1_only()
        .build()
        .unwrap();
    let time = |label: &'static str, request: reqwest::RequestBuilder| async move {
        let mut times = Vec::new();
        for _ in 0..200 {
            let r = request.try_clone().unwrap();
            let t = Instant::now();
            let response = r.send().await.unwrap();
            assert_eq!(response.status(), 200, "{label}");
            response.bytes().await.unwrap();
            times.push(t.elapsed());
        }
        times.sort();
        println!(
            "{label:<28} p50 {:>9.1?}  p90 {:>9.1?}",
            times[100], times[180]
        );
    };
    let via = |url: &str| {
        let parsed = url::Url::parse(url).unwrap();
        reqwest::Client::builder()
            .no_proxy()
            .http1_only()
            .resolve(
                parsed.host_str().unwrap(),
                std::net::SocketAddr::from(([127, 0, 0, 1], parsed.port().unwrap())),
            )
            .build()
            .unwrap()
            .get(format!("{url}/src/5.js"))
    };
    time("B proxy (local)", via(&pair.local())).await;
    time(
        "B agent (direct, TLS)",
        agent
            .get(format!(
                "https://127.0.0.1:{agent_b}/v1/apps/{app_id}/proxy/src/5.js"
            ))
            .bearer_auth(&token),
    )
    .await;
    time(
        "B agent (via link)",
        agent
            .get(format!(
                "https://127.0.0.1:{link}/v1/apps/{app_id}/proxy/src/5.js"
            ))
            .bearer_auth(&token),
    )
    .await;
    time("A proxy -> relay -> B", via(&pair.remote())).await;
}
