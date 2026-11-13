//! Readable remote URLs. The local proxy forwards `APP.MACHINE.<tld>` (for a
//! paired machine's slug) to this loopback-only listener inside the proxy
//! process, which relays over the pinned, authenticated agent connection.
//! URLs therefore stay stable across restarts: they depend only on the proxy
//! port and the stored peer slug, never on a per-relay random port.
use super::{client, types::*};
use axum::body::Body;
use hyper::{body::Incoming, header, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use std::{
    collections::{HashMap, HashSet},
    convert::Infallible,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

const PREFIX: &str = "/_porch_peer/";
/// A catalog this young is used as is.
const CATALOG_TTL: Duration = Duration::from_secs(10);
/// An older one (up to this age) still answers at once while a single
/// background refresh replaces it, so page loads never wait on the catalog.
/// It only maps names to app IDs: the agent checks every request itself.
const CATALOG_STALE: Duration = Duration::from_secs(300);
/// A name missing from a catalog at least this old triggers one fresh fetch.
const CATALOG_RECHECK: Duration = Duration::from_secs(1);
/// A failed fetch answers requests for this long, so a page's burst against an
/// unreachable machine waits for one attempt instead of one each.
const CATALOG_FAILURE: Duration = Duration::from_secs(2);
static PORT: OnceLock<u16> = OnceLock::new();
/// The gateway of this process, when it is the proxy.
static GATEWAY: OnceLock<Arc<Gateway>> = OnceLock::new();

struct Gateway {
    tlds: Vec<String>,
    catalogs: Mutex<HashMap<String, (Instant, String, Catalog)>>,
    refreshing: Mutex<HashSet<String>>,
    /// One catalog fetch per machine at a time; the others wait for its result.
    fetching: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// The last failed fetch per machine: when, for which pairing, and why.
    failures: Mutex<HashMap<String, (Instant, String, String)>>,
    clients: Mutex<HashMap<String, client::RelayClient>>,
}

fn label_ok(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 63
        && label
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}
/// `web.mac-mini.localhost` -> (`web`, `mac-mini`). App names may contain dots.
pub fn split_host(hostname: &str, tlds: &[String]) -> Option<(String, String)> {
    let hostname = hostname.trim_end_matches('.').to_ascii_lowercase();
    let mut tlds: Vec<&String> = tlds.iter().collect();
    tlds.sort_by_key(|t| std::cmp::Reverse(t.len()));
    let rest = tlds.iter().find_map(|t| {
        hostname
            .strip_suffix(t.trim_start_matches('.'))
            .and_then(|r| r.strip_suffix('.'))
    })?;
    let (app, slug) = rest.rsplit_once('.')?;
    (label_ok(slug) && app.split('.').all(label_ok)).then(|| (app.to_owned(), slug.to_owned()))
}
/// A canonical app name that can appear in a hostname.
pub fn hostname_app(name: &str) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    lower.split('.').all(label_ok).then_some(lower)
}

/// Start the listener once per proxy process. Failure only disables remote names.
pub fn start(tlds: Vec<String>) {
    if PORT.get().is_some() {
        return;
    }
    let listener = match std::net::TcpListener::bind("127.0.0.1:0").and_then(|l| {
        l.set_nonblocking(true)?;
        Ok(l)
    }) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Porch: paired-machine URLs unavailable: {e}");
            return;
        }
    };
    let (Ok(addr), Ok(listener)) = (
        listener.local_addr(),
        tokio::net::TcpListener::from_std(listener),
    ) else {
        return;
    };
    if PORT.set(addr.port()).is_err() {
        return;
    }
    let gateway = Arc::new(Gateway {
        tlds,
        catalogs: Mutex::default(),
        refreshing: Mutex::default(),
        fetching: Mutex::default(),
        failures: Mutex::default(),
        clients: Mutex::default(),
    });
    let _ = GATEWAY.set(gateway.clone());
    tokio::spawn(accept(listener, gateway));
}

/// A peer's catalog fetched by this proxy's gateway, when this process is the
/// proxy: over the connection its relay uses, which the fetch keeps warm, and
/// into the cache a browser's first request then finds. `None` elsewhere.
pub(super) async fn fetch_shared(peer: &Peer, endpoint: &str) -> Option<anyhow::Result<Catalog>> {
    let gateway = GATEWAY.get()?.clone();
    let mut peer = peer.clone();
    peer.endpoint = endpoint.to_owned();
    Some(match gateway.client(&peer.fingerprint) {
        Ok(relay) => gateway.fetch(&peer, &relay).await,
        Err(e) => Err(e),
    })
}

/// Called by the proxy when no local route matches. Only loopback browsers may
/// reach paired machines; LAN clients of a LAN-mode proxy never chain through.
pub fn target(hostname: &str, loopback_client: bool, tlds: &[String]) -> Option<String> {
    if !loopback_client {
        return None;
    }
    let port = PORT.get()?;
    let (app, slug) = split_host(hostname, tlds)?;
    client::shared_peers()
        .ok()?
        .iter()
        .any(|p| p.slug == slug)
        .then(|| format!("http://127.0.0.1:{port}{PREFIX}{slug}/{app}"))
}

async fn accept(listener: tokio::net::TcpListener, gateway: Arc<Gateway>) {
    let permits = Arc::new(tokio::sync::Semaphore::new(256));
    loop {
        let Ok((stream, remote)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        if !remote.ip().is_loopback() {
            continue;
        }
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            continue;
        };
        let gateway = gateway.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let service = hyper::service::service_fn(move |req| handle(req, gateway.clone()));
            let mut builder = hyper::server::conn::http1::Builder::new();
            builder
                .header_read_timeout(Duration::from_secs(10))
                .timer(TokioTimer::new())
                .max_buf_size(32768);
            let _ = builder
                .serve_connection(TokioIo::new(stream), service)
                .with_upgrades()
                .await;
        });
    }
}

fn page(status: StatusCode, message: &str) -> Response<Body> {
    let mut r = Response::new(Body::from(message.to_owned()));
    *r.status_mut() = status;
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    r.headers_mut()
        .insert("x-porch", header::HeaderValue::from_static("1"));
    r
}

fn catalog_key(peer: &Peer) -> String {
    format!("{}\0{}\0{}", peer.endpoint, peer.fingerprint, peer.token)
}
impl Gateway {
    fn cached(&self, peer: &Peer) -> Option<(Catalog, Duration)> {
        let key = catalog_key(peer);
        self.catalogs.lock().ok().and_then(|cache| {
            cache
                .get(&peer.machine.id)
                .filter(|(_, k, _)| *k == key)
                .map(|(at, _, c)| (c.clone(), at.elapsed()))
        })
    }
    /// The peer's catalog and its age. Fresh or recently stale copies return
    /// at once (stale ones start a background refresh); otherwise this fetches.
    async fn catalog(
        self: &Arc<Self>,
        peer: &Peer,
        relay: &client::RelayClient,
    ) -> anyhow::Result<(Catalog, Duration)> {
        match self.cached(peer) {
            Some((catalog, age)) if age < CATALOG_TTL => Ok((catalog, age)),
            Some((catalog, age)) if age < CATALOG_STALE => {
                let first = self
                    .refreshing
                    .lock()
                    .is_ok_and(|mut r| r.insert(peer.machine.id.clone()));
                if first {
                    let (gateway, peer, relay) = (self.clone(), peer.clone(), relay.clone());
                    tokio::spawn(async move {
                        // Unreachable now: drop the copy so the next request
                        // reports why instead of relaying to a dead link.
                        if gateway
                            .fetch_once(&peer, &relay, Duration::ZERO)
                            .await
                            .is_err()
                        {
                            if let Ok(mut cache) = gateway.catalogs.lock() {
                                cache.remove(&peer.machine.id);
                            }
                        }
                        if let Ok(mut r) = gateway.refreshing.lock() {
                            r.remove(&peer.machine.id);
                        }
                    });
                }
                Ok((catalog, age))
            }
            _ => self.fetch_once(peer, relay, CATALOG_TTL).await,
        }
    }
    /// Fetch the catalog unless a copy younger than `newer_than` exists,
    /// which a concurrent fetch this call waited for may just have stored.
    /// Concurrent callers share one fetch, and a recent failure is reported
    /// again without another attempt.
    async fn fetch_once(
        &self,
        peer: &Peer,
        relay: &client::RelayClient,
        newer_than: Duration,
    ) -> anyhow::Result<(Catalog, Duration)> {
        let lock = {
            let mut all = self
                .fetching
                .lock()
                .map_err(|_| anyhow::anyhow!("catalog fetch lock poisoned"))?;
            if all.len() > 256 {
                all.retain(|_, l| Arc::strong_count(l) > 1);
            }
            all.entry(peer.machine.id.clone()).or_default().clone()
        };
        let _fetching = lock.lock().await;
        if let Some((catalog, age)) = self.cached(peer) {
            if age < newer_than {
                return Ok((catalog, age));
            }
        }
        let key = catalog_key(peer);
        let failed = self.failures.lock().ok().and_then(|f| {
            f.get(&peer.machine.id)
                .filter(|(at, k, _)| at.elapsed() < CATALOG_FAILURE && *k == key)
                .map(|(_, _, e)| e.clone())
        });
        if let Some(e) = failed {
            anyhow::bail!(e);
        }
        match self.fetch(peer, relay).await {
            Ok(catalog) => Ok((catalog, Duration::ZERO)),
            Err(e) => {
                if let Ok(mut f) = self.failures.lock() {
                    if f.len() > 256 {
                        f.clear();
                    }
                    f.insert(
                        peer.machine.id.clone(),
                        (Instant::now(), key, format!("{e:#}")),
                    );
                }
                Err(e)
            }
        }
    }
    async fn fetch(&self, peer: &Peer, relay: &client::RelayClient) -> anyhow::Result<Catalog> {
        let catalog = client::relay_catalog(peer, relay).await?;
        if let Ok(mut failures) = self.failures.lock() {
            failures.remove(&peer.machine.id);
        }
        if let Ok(mut cache) = self.catalogs.lock() {
            if cache.len() > 256 {
                cache.clear();
            }
            cache.insert(
                peer.machine.id.clone(),
                (Instant::now(), catalog_key(peer), catalog.clone()),
            );
        }
        Ok(catalog)
    }
    fn client(&self, pin: &str) -> anyhow::Result<client::RelayClient> {
        let mut clients = self
            .clients
            .lock()
            .map_err(|_| anyhow::anyhow!("relay client cache poisoned"))?;
        if let Some(c) = clients.get(pin) {
            return Ok(c.clone());
        }
        let c = client::relay_client(pin)?;
        if clients.len() > 256 {
            clients.clear();
        }
        clients.insert(pin.to_owned(), c.clone());
        Ok(c)
    }
}

async fn handle(
    mut req: Request<Incoming>,
    gateway: Arc<Gateway>,
) -> Result<Response<Body>, Infallible> {
    let Some(rest) = req.uri().path().strip_prefix(PREFIX) else {
        return Ok(page(StatusCode::NOT_FOUND, "Unknown Porch gateway path"));
    };
    let (slug, rest) = rest.split_once('/').unwrap_or((rest, ""));
    let (app, path) = rest.split_once('/').unwrap_or((rest, ""));
    let (slug, app, path) = (slug.to_owned(), app.to_owned(), format!("/{path}"));
    // The Host must be the readable name itself; a rebinding DNS name aimed at
    // this port cannot select a paired machine.
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let hostname = host
        .parse::<hyper::http::uri::Authority>()
        .map(|a| a.host().to_owned())
        .unwrap_or_default();
    if split_host(&hostname, &gateway.tlds) != Some((app.clone(), slug.clone())) {
        return Ok(page(
            StatusCode::MISDIRECTED_REQUEST,
            "Use the paired machine's APP.MACHINE URL",
        ));
    }
    let Some(peer) = client::shared_peers()
        .ok()
        .and_then(|all| all.iter().find(|p| p.slug == slug).cloned())
    else {
        return Ok(page(
            StatusCode::NOT_FOUND,
            &format!("No paired machine is called {slug}. Run porch peers list."),
        ));
    };
    let Ok(relay) = gateway.client(&peer.fingerprint) else {
        return Ok(page(StatusCode::BAD_GATEWAY, "Cannot build relay client"));
    };
    let unreachable = |e: anyhow::Error| {
        page(
            StatusCode::BAD_GATEWAY,
            &format!("{} is unreachable: {e:#}", peer.machine.name),
        )
    };
    let shared = |catalog: &Catalog| {
        catalog
            .apps
            .iter()
            .find(|a| hostname_app(&a.name).as_deref() == Some(app.as_str()))
            .filter(|a| a.remote_access)
            .cloned()
    };
    let catalog = match gateway.catalog(&peer, &relay).await {
        Ok((c, age)) => {
            // An app started (or shared) since this copy was taken is reachable at once.
            if shared(&c).is_none() && age >= CATALOG_RECHECK {
                match gateway.fetch_once(&peer, &relay, CATALOG_RECHECK).await {
                    Ok((c, _)) => c,
                    Err(e) => return Ok(unreachable(e)),
                }
            } else {
                c
            }
        }
        Err(e) => return Ok(unreachable(e)),
    };
    let found = match shared(&catalog) {
        Some(found) => found,
        None => {
            let Some(found) = catalog
                .apps
                .iter()
                .find(|a| hostname_app(&a.name).as_deref() == Some(app.as_str()))
            else {
                let names: Vec<_> = catalog.apps.iter().map(|a| a.name.as_str()).collect();
                return Ok(page(
                    StatusCode::NOT_FOUND,
                    &format!(
                        "No app named {app} on {}. Apps: {}",
                        peer.machine.name,
                        if names.is_empty() {
                            "none".into()
                        } else {
                            names.join(", ")
                        }
                    ),
                ));
            };
            return Ok(page(
                StatusCode::FORBIDDEN,
                found
                    .unavailable_reason
                    .as_deref()
                    .unwrap_or("App sharing is off on that machine"),
            ));
        }
    };
    let scheme = match req
        .headers()
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
    {
        Some("https") => "https",
        _ => "http",
    };
    let (Ok(relay_url), Ok(local_app)) = (
        url::Url::parse(&format!("{scheme}://{host}")),
        url::Url::parse(&found.local_url),
    ) else {
        return Ok(page(StatusCode::BAD_REQUEST, "Invalid request host"));
    };
    let path_and_query = match req.uri().query() {
        Some(q) => format!("{path}?{q}"),
        None => path,
    };
    let Ok(uri) = path_and_query.parse() else {
        return Ok(page(StatusCode::BAD_REQUEST, "Invalid request path"));
    };
    *req.uri_mut() = uri;
    let origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok());
    let sibling = sibling_origin(origin, &relay_url, &slug, &gateway.tlds, &catalog);
    let context = client::RelayContext {
        app_id: found.id.clone(),
        peer,
        client: relay,
        relay_url,
        local_app,
        sibling,
    };
    client::relay_request(req, context).await
}

/// A page of another app on the same paired machine (`web.mac-mini` calling
/// `api.mac-mini`) may call this one, as it could on that machine: its origin
/// and the one it has there. Other machines' apps and foreign sites cannot.
fn sibling_origin(
    origin: Option<&str>,
    relay_url: &url::Url,
    slug: &str,
    tlds: &[String],
    catalog: &Catalog,
) -> Option<(String, String)> {
    let origin = origin?;
    let parsed = url::Url::parse(origin).ok()?;
    if parsed.scheme() != relay_url.scheme()
        || parsed.port_or_known_default() != relay_url.port_or_known_default()
        || parsed.origin().ascii_serialization() != origin
    {
        return None;
    }
    let (app, machine) = split_host(parsed.host_str()?, tlds)?;
    if machine != slug {
        return None;
    }
    let theirs = catalog
        .apps
        .iter()
        .find(|a| hostname_app(&a.name).as_deref() == Some(app.as_str()))?;
    let local = url::Url::parse(&theirs.local_url).ok()?;
    Some((origin.to_owned(), local.origin().ascii_serialization()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// A paired agent whose catalog takes a moment to build. Counts catalog
    /// requests; answers 500 while `failing` is set.
    async fn catalog_agent() -> (Peer, Arc<AtomicUsize>, Arc<AtomicBool>) {
        let cert = rcgen::generate_simple_self_signed(vec!["peer.local".into()]).unwrap();
        let pin = client::hex_pin(&sha2::Sha256::digest(cert.cert.der()).into());
        let key = rustls::pki_types::PrivateKeyDer::Pkcs8(
            rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()),
        );
        let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.cert.der().clone()], key)
        .unwrap();
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let machine = Machine {
            id: "machine-b".into(),
            name: "Mac mini".into(),
        };
        let catalog = Catalog {
            version: PROTOCOL_VERSION,
            machine: machine.clone(),
            apps: vec![],
            proxy_running: true,
            generated_at: 1,
        };
        let peer = Peer {
            machine,
            endpoint: format!("https://{}/", listener.local_addr().unwrap()),
            fingerprint: pin,
            token: "a".repeat(64),
            sources: vec![Source::Explicit],
            last_seen: 0,
            slug: "mac-mini".into(),
        };
        let (hits, failing) = (
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicBool::new(false)),
        );
        let (counter, fail) = (hits.clone(), failing.clone());
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let (acceptor, catalog) = (acceptor.clone(), catalog.clone());
                let (counter, fail) = (counter.clone(), fail.clone());
                tokio::spawn(async move {
                    let Ok(stream) = acceptor.accept(stream).await else {
                        return;
                    };
                    let service = hyper::service::service_fn(move |_req: Request<Incoming>| {
                        counter.fetch_add(1, Ordering::SeqCst);
                        let (catalog, fail) = (catalog.clone(), fail.load(Ordering::SeqCst));
                        async move {
                            tokio::time::sleep(Duration::from_millis(200)).await;
                            let mut r =
                                Response::new(Body::from(serde_json::to_vec(&catalog).unwrap()));
                            if fail {
                                *r.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
                            }
                            Ok::<_, Infallible>(r)
                        }
                    });
                    let _ = hyper_util::server::conn::auto::Builder::new(
                        hyper_util::rt::TokioExecutor::new(),
                    )
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
                });
            }
        });
        (peer, hits, failing)
    }
    #[test]
    fn only_apps_of_the_same_machine_count_as_siblings() {
        let tlds = vec!["localhost".to_owned()];
        let relay = url::Url::parse("http://web.mac-mini.localhost:7462").unwrap();
        let app = |name: &str, url: &str| App {
            id: name.into(),
            name: name.into(),
            local_url: url.into(),
            health: Health::Healthy,
            remote_access: true,
            unavailable_reason: None,
        };
        let catalog = Catalog {
            version: PROTOCOL_VERSION,
            machine: Machine {
                id: "m".into(),
                name: "Mac mini".into(),
            },
            apps: vec![
                app("web", "http://web.localhost:7462"),
                app("api", "https://api.localhost:8443/"),
            ],
            proxy_running: true,
            generated_at: 1,
        };
        let check = |o: &str| sibling_origin(Some(o), &relay, "mac-mini", &tlds, &catalog);
        assert_eq!(
            check("http://api.mac-mini.localhost:7462"),
            Some((
                "http://api.mac-mini.localhost:7462".into(),
                "https://api.localhost:8443".into()
            ))
        );
        for refused in [
            "http://api.other-box.localhost:7462",
            "http://api.mac-mini.localhost:7463",
            "https://api.mac-mini.localhost:7462",
            "http://docs.mac-mini.localhost:7462",
            "http://api.mac-mini.localhost:7462/path",
            "http://evil.example",
            "null",
        ] {
            assert_eq!(check(refused), None, "{refused}");
        }
    }
    fn gateway() -> Arc<Gateway> {
        Arc::new(Gateway {
            tlds: vec!["localhost".into()],
            catalogs: Mutex::default(),
            refreshing: Mutex::default(),
            fetching: Mutex::default(),
            failures: Mutex::default(),
            clients: Mutex::default(),
        })
    }

    /// A page's first burst (no catalog yet, or one past its stale window)
    /// fetches the catalog once, and a failure answers the burst at once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_requests_share_one_catalog_fetch() {
        let (peer, hits, failing) = catalog_agent().await;
        let gateway = gateway();
        let relay = gateway.client(&peer.fingerprint).unwrap();
        let burst = |n| {
            let (gateway, peer, relay) = (gateway.clone(), peer.clone(), relay.clone());
            futures_util::future::join_all((0..n).map(move |_| {
                let (gateway, peer, relay) = (gateway.clone(), peer.clone(), relay.clone());
                async move { gateway.catalog(&peer, &relay).await.map(|_| ()) }
            }))
        };
        assert!(burst(30).await.iter().all(|r| r.is_ok()));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        // A missing app rechecks once for the whole burst, too.
        gateway.catalogs.lock().unwrap().clear();
        failing.store(true, Ordering::SeqCst);
        assert!(burst(30).await.iter().all(|r| r.is_err()));
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        // The failure is reported again briefly without another attempt...
        assert!(burst(5).await.iter().all(|r| r.is_err()));
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        // ...and then retried.
        failing.store(false, Ordering::SeqCst);
        tokio::time::sleep(CATALOG_FAILURE).await;
        assert!(burst(5).await.iter().all(|r| r.is_ok()));
        assert_eq!(hits.load(Ordering::SeqCst), 3);
        // Re-pairing (a new token) is never answered from the old failure.
        failing.store(true, Ordering::SeqCst);
        gateway.catalogs.lock().unwrap().clear();
        assert!(burst(1).await[0].is_err());
        failing.store(false, Ordering::SeqCst);
        let mut repaired = peer.clone();
        repaired.token = "b".repeat(64);
        assert!(gateway.catalog(&repaired, &relay).await.is_ok());
        assert_eq!(hits.load(Ordering::SeqCst), 5);
    }
    #[test]
    fn readable_hosts_split_into_app_and_machine() {
        let tlds = vec!["localhost".to_owned(), "dev.example.com".to_owned()];
        let split = |h: &str| split_host(h, &tlds);
        assert_eq!(
            split("web.mac-mini.localhost"),
            Some(("web".into(), "mac-mini".into()))
        );
        assert_eq!(
            split("API.Web.Mac-Mini.localhost."),
            Some(("api.web".into(), "mac-mini".into()))
        );
        assert_eq!(
            split("web.box.dev.example.com"),
            Some(("web".into(), "box".into()))
        );
        for bad in [
            "web.localhost",
            "mac-mini.localhost",
            "web.mac-mini.evil.com",
            "we_b.mac-mini.localhost",
            ".mac-mini.localhost",
        ] {
            assert_eq!(split(bad), None, "{bad}");
        }
        assert_eq!(hostname_app("Web"), Some("web".into()));
        assert_eq!(hostname_app("web/docs"), None);
        assert_eq!(target("web.mac-mini.localhost", false, &tlds), None);
    }
}
