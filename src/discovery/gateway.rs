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
    collections::HashMap,
    convert::Infallible,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

const PREFIX: &str = "/_porch_peer/";
const CATALOG_TTL: Duration = Duration::from_secs(10);
static PORT: OnceLock<u16> = OnceLock::new();

struct Gateway {
    tlds: Vec<String>,
    catalogs: Mutex<HashMap<String, (Instant, String, Catalog)>>,
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
        clients: Mutex::default(),
    });
    tokio::spawn(accept(listener, gateway));
}

/// Called by the proxy when no local route matches. Only loopback browsers may
/// reach paired machines; LAN clients of a LAN-mode proxy never chain through.
pub fn target(hostname: &str, loopback_client: bool, tlds: &[String]) -> Option<String> {
    if !loopback_client {
        return None;
    }
    let port = PORT.get()?;
    let (app, slug) = split_host(hostname, tlds)?;
    client::peers()
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

impl Gateway {
    async fn catalog(&self, peer: &Peer) -> anyhow::Result<Catalog> {
        let key = format!("{}\0{}\0{}", peer.endpoint, peer.fingerprint, peer.token);
        if let Ok(cache) = self.catalogs.lock() {
            if let Some((at, k, c)) = cache.get(&peer.machine.id) {
                if *k == key && at.elapsed() < CATALOG_TTL {
                    return Ok(c.clone());
                }
            }
        }
        let catalog = client::fetch_catalog(peer, &peer.endpoint).await?;
        if let Ok(mut cache) = self.catalogs.lock() {
            if cache.len() > 256 {
                cache.clear();
            }
            cache.insert(
                peer.machine.id.clone(),
                (Instant::now(), key, catalog.clone()),
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
    let Some(peer) = client::peers()
        .ok()
        .and_then(|all| all.into_iter().find(|p| p.slug == slug))
    else {
        return Ok(page(
            StatusCode::NOT_FOUND,
            &format!("No paired machine is called {slug}. Run porch peers list."),
        ));
    };
    let catalog = match gateway.catalog(&peer).await {
        Ok(c) => c,
        Err(e) => {
            return Ok(page(
                StatusCode::BAD_GATEWAY,
                &format!("{} is unreachable: {e:#}", peer.machine.name),
            ))
        }
    };
    let found = catalog
        .apps
        .iter()
        .find(|a| hostname_app(&a.name).as_deref() == Some(app.as_str()));
    let Some(found) = found else {
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
    if !found.remote_access {
        return Ok(page(
            StatusCode::FORBIDDEN,
            found
                .unavailable_reason
                .as_deref()
                .unwrap_or("App sharing is off on that machine"),
        ));
    }
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
    let Ok(relay) = gateway.client(&peer.fingerprint) else {
        return Ok(page(StatusCode::BAD_GATEWAY, "Cannot build relay client"));
    };
    let context = client::RelayContext {
        app_id: found.id.clone(),
        peer,
        client: relay,
        relay_url,
        local_app,
    };
    client::relay_request(req, context).await
}

#[cfg(test)]
mod tests {
    use super::*;
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
