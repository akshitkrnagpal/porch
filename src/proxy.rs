//! Streaming HTTP/1.1, HTTP/2 and WebSocket reverse proxy.
use crate::state::ProxySettings;
use anyhow::{Context, Result};
use bytes::Bytes;
use http_body_util::{combinators::UnsyncBoxBody, BodyExt, Full};
use hyper::{body::Incoming, header, Method, Request, Response, StatusCode, Version};
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::{TokioExecutor, TokioIo, TokioTimer},
    server::conn::auto::Builder,
};
use std::{
    collections::HashMap,
    convert::Infallible,
    future::Future,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    pin::Pin,
    sync::{Arc, Mutex},
    task::Poll,
    time::Duration,
};
type BoxError = Box<dyn std::error::Error + Send + Sync>;
type Body = UnsyncBoxBody<Bytes, BoxError>;
/// Most client connections (WebSockets included) the proxy serves at once,
/// across all its listeners; fewer when the file-descriptor limit is lower.
const MAX_CONNECTIONS: usize = 4096;
/// Fewest connections a very low descriptor limit still allows.
#[cfg(any(unix, test))]
const MIN_CONNECTIONS: usize = 16;
/// Most connections one other device (LAN mode) may hold open at once.
const MAX_PER_CLIENT: usize = 64;
/// Time a client gets to send a complete HTTP/1 request head, and to start
/// its first request on a new connection.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// How long another device's connection may sit with no request in flight.
const LAN_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// How long another device may pause while sending a request body.
const BODY_READ_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a stopping proxy lets requests in flight finish.
/// `porch proxy stop` waits 5 s before it kills the process.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(3);
/// How long the dashboard reuses one listing of paired machines.
const PEERS_TTL: Duration = Duration::from_secs(5);
type Upstream = Client<hyper_rustls::HttpsConnector<HttpConnector>, Body>;
type LoopbackUpstream = crate::upstream::LoopbackClient<Body>;
type PeersCache = tokio::sync::Mutex<Option<(tokio::time::Instant, serde_json::Value)>>;
#[derive(Clone)]
struct ProxyContext {
    client: Upstream,
    /// Dials every name on loopback, for `--upstream` aliases: the URI host is
    /// the TLS server name, the connection still goes to this computer. Also
    /// used for apps Porch launched, which may listen on [::1] alone.
    loopback: LoopbackUpstream,
    /// The same, without certificate verification (`--insecure` aliases only).
    loopback_insecure: LoopbackUpstream,
    settings: Arc<ProxySettings>,
    routing: Arc<Routing>,
    token: Arc<String>,
    limits: Arc<Limits>,
    peers: Arc<PeersCache>,
    stopped: tokio::sync::watch::Receiver<bool>,
}
/// The TLDs a request's host is matched against, computed once per proxy:
/// longest first, so overlapping suffixes (`dev.example.com`, `example.com`)
/// resolve to the most specific.
struct Routing {
    tlds: Vec<String>,
    wildcard: bool,
}
impl Routing {
    fn new(settings: &ProxySettings) -> Self {
        let mut tlds = settings.tlds.clone();
        if settings.lan && !tlds.iter().any(|t| t == "local") {
            tlds.push("local".into());
        }
        tlds.sort_by_key(|t| std::cmp::Reverse(t.len()));
        Self {
            tlds,
            wildcard: settings.wildcard,
        }
    }
}
/// Connection slots shared by every listener, sized to the descriptor limit.
struct Limits {
    slots: Arc<tokio::sync::Semaphore>,
    clients: Arc<ClientSlots>,
}
impl Limits {
    fn new(connections: usize) -> Self {
        Self {
            slots: Arc::new(tokio::sync::Semaphore::new(connections)),
            clients: Arc::new(ClientSlots::new((connections / 4).clamp(4, MAX_PER_CLIENT))),
        }
    }
    /// A slot for a connection from `peer`, if it may open one: every client
    /// within the shared cap, another device also within its own share.
    fn acquire(&self, peer: IpAddr) -> Option<Slot> {
        let client = if peer.is_loopback() {
            None
        } else {
            Some(self.clients.acquire(peer)?)
        };
        let permit = self.slots.clone().try_acquire_owned().ok()?;
        Some(Slot {
            _permit: permit,
            _client: client,
        })
    }
}
struct Slot {
    _permit: tokio::sync::OwnedSemaphorePermit,
    _client: Option<ClientSlot>,
}
/// Open connections per other device, so one device can't take every slot.
struct ClientSlots {
    limit: usize,
    open: Mutex<HashMap<IpAddr, usize>>,
}
struct ClientSlot {
    slots: Arc<ClientSlots>,
    key: IpAddr,
}
impl ClientSlots {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            open: Mutex::new(HashMap::new()),
        }
    }
    /// One device: an IPv4 address, or an IPv6 /64 (a device picks new
    /// addresses within its prefix at will).
    fn key(ip: IpAddr) -> IpAddr {
        match ip.to_canonical() {
            IpAddr::V6(v6) => {
                let s = v6.segments();
                IpAddr::V6(std::net::Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
            }
            v4 => v4,
        }
    }
    fn acquire(self: &Arc<Self>, ip: IpAddr) -> Option<ClientSlot> {
        let key = Self::key(ip);
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        let count = open.entry(key).or_insert(0);
        if *count >= self.limit {
            return None;
        }
        *count += 1;
        Some(ClientSlot {
            slots: self.clone(),
            key,
        })
    }
}
impl Drop for ClientSlot {
    fn drop(&mut self) {
        let mut open = self.slots.open.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(count) = open.get_mut(&self.key) {
            *count -= 1;
            if *count == 0 {
                open.remove(&self.key);
            }
        }
    }
}
/// Raise the soft open-file limit toward the hard limit and return the limit
/// in force. Each proxied connection needs two descriptors (client and app),
/// and the usual defaults (256 on macOS, 1024 on Linux) would run out after a
/// few hundred connections.
#[cfg(unix)]
fn raise_fd_limit() -> u64 {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return 256;
    }
    #[allow(clippy::unnecessary_cast)]
    let (current, hard) = (limit.rlim_cur as u64, limit.rlim_max as u64);
    // macOS rejects values above kern.maxfilesperproc: step down until one fits.
    for want in [65536u64, 10240, 4096, 2048, 1024] {
        let want = want.min(hard);
        if want <= current {
            break;
        }
        let raised = libc::rlimit {
            rlim_cur: want as libc::rlim_t,
            rlim_max: limit.rlim_max,
        };
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raised) } == 0 {
            return want;
        }
    }
    current
}
/// Connections that fit in `fds` descriptors: two each, after a reserve for
/// listeners, state files, discovery and the apps' health checks.
#[cfg(any(unix, test))]
fn connections_for(fds: u64) -> usize {
    let reserve = (fds / 8).max(64);
    usize::try_from(fds.saturating_sub(reserve) / 2)
        .unwrap_or(usize::MAX)
        .clamp(MIN_CONNECTIONS, MAX_CONNECTIONS)
}
fn connection_limit() -> usize {
    #[cfg(unix)]
    {
        connections_for(raise_fd_limit())
    }
    #[cfg(not(unix))]
    {
        MAX_CONNECTIONS
    }
}
fn empty() -> Body {
    Full::new(Bytes::new())
        .map_err(|e: Infallible| match e {})
        .boxed_unsync()
}
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
const PAGE_CSS: &str = ":root{color-scheme:light dark;--bg:#faf8f2;--fg:#242421;--muted:#6b675e;--card:#fff;--line:#e6e1d6;--accent:#b4532a;--code:#efeadf}@media (prefers-color-scheme:dark){:root{--bg:#161513;--fg:#ece9e1;--muted:#a19c90;--card:#1f1e1b;--line:#33312c;--accent:#f08a5d;--code:#2a2925}}*{box-sizing:border-box}body{margin:0;font:16px/1.5 ui-sans-serif,system-ui,-apple-system,sans-serif;background:var(--bg);color:var(--fg)}main{max-width:46rem;margin:0 auto;padding:12vh 1rem 4rem}.status{font:600 .8rem ui-monospace,monospace;letter-spacing:.08em;text-transform:uppercase;color:var(--accent)}h1{font-size:1.75rem;line-height:1.2;margin:.4rem 0 1rem}p{margin:.6rem 0}code{font:.9em ui-monospace,SFMono-Regular,Menlo,monospace;background:var(--code);padding:.1rem .35rem;border-radius:4px}.muted{color:var(--muted)}h2{font-size:.95rem;margin:2.5rem 0 .75rem;color:var(--muted);font-weight:600}ul.apps{list-style:none;padding:0;margin:0;border:1px solid var(--line);border-radius:10px;overflow:hidden;background:var(--card)}ul.apps li{border-top:1px solid var(--line);display:flex;align-items:center}ul.apps li[hidden]{display:none}ul.apps li>a{flex:1;min-width:0}button.copy{font:.8rem inherit;margin-right:.75rem;padding:.25rem .6rem;border:1px solid var(--line);border-radius:6px;background:transparent;color:var(--muted);cursor:pointer}button.copy:hover,button.copy:focus-visible{color:var(--fg);border-color:var(--muted)}ul.apps li:first-child{border-top:0}ul.apps a{display:flex;justify-content:space-between;gap:1rem;padding:.75rem 1rem;color:inherit;text-decoration:none}ul.apps a:hover,ul.apps a:focus-visible{background:var(--code);outline:none}.name{font-weight:600}.target{color:var(--muted);font:.85em ui-monospace,monospace;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}input[type=search]{width:100%;font:inherit;padding:.55rem .75rem;margin:0 0 .75rem;border:1px solid var(--line);border-radius:8px;background:var(--card);color:inherit}.spinner{display:inline-block;width:.8em;height:.8em;border:2px solid var(--line);border-top-color:var(--accent);border-radius:50%;animation:spin 1s linear infinite;vertical-align:-.05em;margin-right:.4rem}@keyframes spin{to{transform:rotate(360deg)}}@media (prefers-reduced-motion:reduce){.spinner{animation:none}}footer{margin-top:3rem;font-size:.85rem;color:var(--muted)}.dot{display:inline-block;width:.55rem;height:.55rem;border-radius:50%;margin-right:.55rem;vertical-align:.08em}.dot.up{background:#2f9e44}.dot.down{background:#e0a100}";
/// Live data for the dashboard at the bare domain: this machine's apps with
/// health and, with `peers`, every paired machine's apps (loopback only).
async fn dashboard_json(settings: &ProxySettings, peers: Option<&PeersCache>) -> serde_json::Value {
    let state = crate::state::live_routes().unwrap_or_default();
    let routes = state.routes.clone();
    let health = tokio::task::spawn_blocking(move || {
        routes
            .iter()
            .map(|r| crate::health::target_listening_cached(&r.target))
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    let apps: Vec<_> = state
        .routes
        .iter()
        .zip(health)
        .map(|(r, up)| {
            serde_json::json!({
                "name": r.name,
                "host": settings.hostname(&r.name),
                "url": settings.url(&r.name),
                "target": r.target,
                "health": if up { "ok" } else { "not listening" },
                "private": state.private.contains(&r.name),
            })
        })
        .collect();
    let mut out = serde_json::json!({ "apps": apps });
    if let Some(cache) = peers {
        out["machines"] = peer_machines(settings, cache).await;
    }
    out
}
/// Every paired machine's apps for the dashboard. Each listing asks every
/// paired machine, so one runs at a time and its answer is reused briefly.
async fn peer_machines(settings: &ProxySettings, cache: &PeersCache) -> serde_json::Value {
    let mut cached = cache.lock().await;
    if let Some((at, machines)) = cached.as_ref() {
        if at.elapsed() < PEERS_TTL {
            return machines.clone();
        }
    }
    let snapshot = tokio::time::timeout(
        Duration::from_secs(10),
        crate::discovery::client::snapshot(false),
    )
    .await;
    let machines: Vec<_> = match snapshot {
        Ok(Ok(snap)) => snap
            .peers
            .iter()
            .map(|p| {
                let apps: Vec<_> = p
                    .catalog
                    .iter()
                    .flat_map(|c| c.apps.iter())
                    .map(|a| {
                        let url = (a.remote_access && !p.slug.is_empty())
                            .then(|| crate::discovery::gateway::hostname_app(&a.name))
                            .flatten()
                            .map(|n| settings.url(&format!("{n}.{}", p.slug)));
                        serde_json::json!({
                            "name": a.name,
                            "url": url,
                            "health": a.health,
                            "note": a.unavailable_reason,
                        })
                    })
                    .collect();
                serde_json::json!({
                    "machine": p.machine.name,
                    "error": p.error,
                    "apps": apps,
                })
            })
            .collect(),
        _ => vec![],
    };
    let machines = serde_json::json!(machines);
    *cached = Some((tokio::time::Instant::now(), machines.clone()));
    machines
}
/// A request a page on another site made (`Sec-Fetch-Site: cross-site` or
/// `same-site`). Browsers send this header on every request they make.
fn cross_site(req: &Request<Incoming>) -> bool {
    req.headers()
        .get("sec-fetch-site")
        .is_some_and(|v| !matches!(v.as_bytes(), b"same-origin" | b"none"))
}
const COPY_SCRIPT: &str = "document.addEventListener('click',e=>{const b=e.target.closest('button.copy');if(!b)return;navigator.clipboard.writeText(b.dataset.url).then(()=>{b.textContent='Copied';setTimeout(()=>b.textContent='Copy',1200)})});";
const DASHBOARD_SCRIPT: &str = r#"<script>
(()=>{const esc=s=>String(s??'').replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'})[c]);
const row=(url,label,up,detail)=>`<li><a href="${esc(url)}"><span class="name"><span class="dot ${up?'up':'down'}"></span>${esc(label)}</span><span class="target">${esc(detail)}</span></a><button class="copy" data-url="${esc(url)}" title="Copy URL" aria-label="Copy URL">Copy</button></li>`;
async function local(){const st=document.getElementById('status');try{const r=await fetch('/_porch/apps.json',{cache:'no-store'});if(!r.ok)throw 0;const d=await r.json();if(st)st.hidden=true;
const el=document.getElementById('local');if(!d.apps.length){el.dataset.empty='1';el.innerHTML='<p class="muted">No apps are running. Start one with <code>porch run pnpm dev</code>.</p>';return}
d.apps.sort((a,b)=>a.host.localeCompare(b.host));const html=d.apps.map(a=>row(a.url,a.host,a.health==='ok',a.health==='ok'?a.target:a.target+' · not listening')).join('');
let ul=el.querySelector('ul.apps');if(!ul||el.dataset.empty){delete el.dataset.empty;el.innerHTML=(d.apps.length>5?'<input type="search" placeholder="Filter apps" aria-label="Filter apps"><p id="nomatch" class="muted" role="status" hidden>No apps match.</p>':'')+'<ul class="apps"></ul>';ul=el.querySelector('ul.apps');const q=el.querySelector('input[type=search]');if(q)q.oninput=filter}
ul.innerHTML=html;filter()}catch(e){if(st)st.hidden=false}}
function filter(){const q=document.querySelector('#local input[type=search]');const f=q?q.value.toLowerCase():'';let n=0;for(const li of document.querySelectorAll('#local ul.apps li')){li.hidden=!!f&&!li.textContent.toLowerCase().includes(f);n+=!li.hidden}const nm=document.getElementById('nomatch');if(nm)nm.hidden=n>0}
async function peers(){try{const r=await fetch('/_porch/apps.json?peers=1',{cache:'no-store'});const d=await r.json();const el=document.getElementById('peers');
el.innerHTML=(d.machines||[]).map(m=>`<h2>${esc(m.machine)}${m.error?' · unreachable':''}</h2>`+(m.apps.length?'<ul class="apps">'+m.apps.map(a=>a.url?row(a.url,a.name,a.health==='healthy',''):`<li><a><span class="name"><span class="dot down"></span>${esc(a.name)}</span><span class="target">${esc(a.note||'not shared')}</span></a></li>`).join('')+'</ul>':'<p class="muted">No apps.</p>')).join('')}catch(e){}}
setInterval(local,3000);peers();setInterval(peers,30000);})();
</script>"#;
fn page(code: StatusCode, title: &str, body: &str, head: &str) -> String {
    let status = if code.is_success() {
        "<div class=\"status\">Porch</div>".to_owned()
    } else {
        format!(
            "<div class=\"status\">{} {}</div>",
            code.as_u16(),
            escape(code.canonical_reason().unwrap_or("Proxy error"))
        )
    };
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">{head}<title>{} · Porch</title><style>{PAGE_CSS}</style></head><body><main>{status}<h1>{}</h1>{body}<footer>Served by Porch · run <code>porch list</code> or <code>porch doctor</code></footer></main></body></html>",
        escape(title),
        escape(title),
    )
}
fn error(code: StatusCode, kind: &str, message: &str, host: &str, html: bool) -> Response<Body> {
    let body = if html {
        // An app that isn't listening yet is usually still starting: retry on
        // our own so the tab comes up as soon as the dev server does.
        let starting = kind == "upstream_unavailable";
        let title = if starting {
            "Waiting for the app to start"
        } else {
            message
        };
        let detail = if starting {
            format!("<p><span class=\"spinner\" aria-hidden=\"true\"></span><strong>{}</strong> is registered, but nothing is answering on its port yet. This page reloads by itself.</p><p class=\"muted\">{}</p>", escape(host), escape(message))
        } else {
            format!("<p class=\"muted\">Host: <code>{}</code></p>", escape(host))
        };
        let head = if starting {
            "<meta http-equiv=\"refresh\" content=\"2\">"
        } else {
            ""
        };
        page(code, title, &detail, head)
    } else {
        serde_json::json!({"error":kind,"upstream":host,"message":message}).to_string()
    };
    let mut r = Response::new(
        Full::new(Bytes::from(body))
            .map_err(|e: Infallible| match e {})
            .boxed_unsync(),
    );
    *r.status_mut() = code;
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static(if html {
            "text/html; charset=utf-8"
        } else {
            "application/json; charset=utf-8"
        }),
    );
    r.headers_mut()
        .insert(header::VARY, header::HeaderValue::from_static("Accept"));
    marker(&mut r);
    r
}
fn marker(r: &mut Response<Body>) {
    r.headers_mut()
        .insert("x-portless", header::HeaderValue::from_static("1"));
    r.headers_mut()
        .insert("x-porch", header::HeaderValue::from_static("1"));
}
fn strip(headers: &mut header::HeaderMap) {
    let named: Vec<String> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|s| s.trim().to_owned())
        .collect();
    for name in named {
        headers.remove(name);
    }
    for name in [
        "connection",
        "keep-alive",
        "proxy-connection",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        headers.remove(name);
    }
}
/// Headers that tell an app who the client is or which URL it asked for.
/// Only a client on this computer (another proxy, a tunnel) may set them;
/// another device could otherwise spoof its address, the scheme or the path
/// the app trusts.
fn client_forwarding_header(name: &str) -> bool {
    name.starts_with("x-forwarded-")
        || name.starts_with("x-original-")
        || matches!(
            name,
            "forwarded" | "x-real-ip" | "x-client-ip" | "true-client-ip" | "x-rewrite-url"
        )
}
fn strip_client_forwarding(headers: &mut header::HeaderMap) {
    let names: Vec<header::HeaderName> = headers
        .keys()
        .filter(|n| client_forwarding_header(n.as_str()))
        .cloned()
        .collect();
    for name in names {
        headers.remove(name);
    }
}
/// HTTP/2 lets a browser split cookies across several `cookie` fields; an
/// HTTP/1.1 app expects one. Join them with "; " (RFC 9113 section 8.2.3).
fn merge_cookies(headers: &mut header::HeaderMap) {
    let mut values = headers.get_all(header::COOKIE).iter();
    let Some(first) = values.next() else {
        return;
    };
    let mut joined = first.as_bytes().to_vec();
    let mut merged = false;
    for v in values {
        joined.extend_from_slice(b"; ");
        joined.extend_from_slice(v.as_bytes());
        merged = true;
    }
    if merged {
        if let Ok(v) = header::HeaderValue::from_bytes(&joined) {
            headers.insert(header::COOKIE, v);
        }
    }
}
/// How many times this request already passed through Porch. Only a proxy on
/// this computer can have forwarded it before; another device's hop count is
/// ignored so it can't force a 508.
fn hop_count(headers: &header::HeaderMap, trusted: bool) -> u32 {
    if !trusted {
        return 0;
    }
    ["x-porch-hops", "x-portless-hops"]
        .iter()
        .filter_map(|name| {
            headers
                .get(*name)?
                .to_str()
                .ok()?
                .trim()
                .parse::<u32>()
                .ok()
        })
        .max()
        .unwrap_or(0)
}
fn is_websocket(upgrade: &header::HeaderValue) -> bool {
    upgrade.to_str().is_ok_and(|v| {
        v.split(',')
            .any(|p| p.trim().eq_ignore_ascii_case("websocket"))
    })
}
/// The `Location` to send for a backend redirect. `absolute` is the redirect
/// already moved from the backend's own origin onto `public_origin`, if it
/// pointed there. An alias to `http://host/base` serves the backend's `/base`
/// at the Porch URL's root, so that prefix is dropped from its redirects.
fn redirect_location(
    location: &str,
    absolute: Option<String>,
    base: &str,
    public_origin: &str,
) -> Option<String> {
    let loc = absolute.clone().unwrap_or_else(|| location.to_owned());
    let (prefix, path) = if let Some(p) = loc.strip_prefix(public_origin) {
        (public_origin, p)
    } else if loc.starts_with('/') && !loc.starts_with("//") {
        ("", loc.as_str())
    } else {
        return absolute;
    };
    match path.strip_prefix(base) {
        Some(rest)
            if !base.is_empty() && (rest.is_empty() || rest.starts_with(['/', '?', '#'])) =>
        {
            // `/base//evil.example` must not become `//evil.example`, which a
            // browser reads as another site (as it does `/\evil.example`).
            let rest = if rest.starts_with('/') {
                format!("/{}", rest.trim_start_matches(['/', '\\']))
            } else {
                format!("/{rest}")
            };
            Some(format!("{prefix}{rest}"))
        }
        _ => absolute,
    }
}
/// `name.tld` (or `name` alone without a TLD) equals `host`, ignoring ASCII
/// case. Compared as bytes: a hand-edited name need not be ASCII.
fn is_name(host: &str, name: &str, tld: Option<&str>) -> bool {
    let (host, name) = (host.as_bytes(), name.as_bytes());
    match tld.map(str::as_bytes) {
        None => host.eq_ignore_ascii_case(name),
        Some(tld) => {
            host.len() == name.len() + 1 + tld.len()
                && host[name.len()] == b'.'
                && host[..name.len()].eq_ignore_ascii_case(name)
                && host[name.len() + 1..].eq_ignore_ascii_case(tld)
        }
    }
}
/// `host` is a subdomain of `name.tld` (or of `name`), ignoring ASCII case.
fn is_under(host: &str, name: &str, tld: Option<&str>) -> bool {
    let len = name.len() + tld.map_or(0, |t| t.len() + 1);
    let bytes = host.as_bytes();
    bytes.len() > len + 1
        && bytes[bytes.len() - len - 1] == b'.'
        && std::str::from_utf8(&bytes[bytes.len() - len..])
            .is_ok_and(|parent| is_name(parent, name, tld))
}
/// `host` ends with `.tld`, ignoring ASCII case.
fn has_tld(host: &str, tld: &str) -> bool {
    let (host, tld) = (host.as_bytes(), tld.as_bytes());
    host.len() > tld.len()
        && host[host.len() - tld.len() - 1] == b'.'
        && host[host.len() - tld.len()..].eq_ignore_ascii_case(tld)
}
/// How well route `name` matches `host`: an exact name beats a parent
/// (wildcard mode), then the longer matched TLD, then the longer name.
fn score(name: &str, host: &str, routing: &Routing) -> Option<(bool, usize, usize)> {
    let name = name.trim_end_matches('.');
    let matched = routing.tlds.iter().find(|t| has_tld(host, t));
    let matched_len = matched.map_or(0, |t| t.len());
    let candidate = |tld: Option<&str>| {
        let exact = is_name(host, name, tld);
        (exact || (routing.wildcard && is_under(host, name, tld))).then(|| {
            (
                exact,
                matched_len,
                name.len() + tld.map_or(0, |t| t.len() + 1),
            )
        })
    };
    if routing.tlds.iter().any(|t| has_tld(name, t)) {
        return candidate(None);
    }
    routing
        .tlds
        .iter()
        .filter(|t| matched.is_none_or(|m| m == *t))
        .filter_map(|t| candidate(Some(t)))
        .max()
}
fn authority(req: &Request<Incoming>) -> String {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok());
    if req.version() == Version::HTTP_2 {
        req.uri().authority().map(|a| a.as_str()).or(host)
    } else {
        host
    }
    .unwrap_or("")
    .to_owned()
}
fn local_authority(host: &str) -> bool {
    host.parse::<hyper::http::uri::Authority>()
        .ok()
        .is_some_and(|a| matches!(a.host(), "localhost" | "127.0.0.1" | "[::1]" | "::1"))
}
/// Paired machines and their apps, for the CLI and tray. The proxy fetches
/// them over the connections its relay keeps, so listing a machine's apps also
/// prepares opening one. Only Porch on this computer may ask: loopback, the
/// proxy's private token, and never a browser page.
async fn peers_request(
    req: &Request<Incoming>,
    peer: SocketAddr,
    host: &str,
    token: &str,
) -> Option<Response<Body>> {
    if req.uri().path() != PEERS_PATH || !local_authority(host) {
        return None;
    }
    let supplied = req
        .headers()
        .get(PEERS_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !peer.ip().is_loopback()
        || req.method() != Method::GET
        || req.headers().contains_key(header::ORIGIN)
        || req.headers().contains_key("sec-fetch-site")
        || !crate::integrations::constant_eq(token, supplied)
    {
        return Some(error(
            StatusCode::FORBIDDEN,
            "forbidden",
            "Paired machines are only listed for Porch on this computer",
            host,
            false,
        ));
    }
    let query = req.uri().query().unwrap_or("");
    let param = |key: &str| {
        query
            .split('&')
            .find_map(|p| p.strip_prefix(key)?.strip_prefix('='))
    };
    let body = match param("peer") {
        Some(id) => {
            async {
                let local = crate::discovery::server::local_catalog()?;
                let peer = crate::discovery::client::refresh_peer(&local.machine.id, id).await?;
                Ok::<_, anyhow::Error>(serde_json::to_vec(&peer)?)
            }
            .await
        }
        None => crate::discovery::client::snapshot(param("refresh") == Some("1"))
            .await
            .and_then(|s| Ok(serde_json::to_vec(&s)?)),
    };
    Some(match body {
        Ok(body) => {
            let mut r = Response::new(
                Full::new(Bytes::from(body))
                    .map_err(|e: Infallible| match e {})
                    .boxed_unsync(),
            );
            r.headers_mut().insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/json"),
            );
            r.headers_mut().insert(
                header::CACHE_CONTROL,
                header::HeaderValue::from_static("no-store"),
            );
            marker(&mut r);
            r
        }
        Err(e) => error(
            StatusCode::BAD_GATEWAY,
            "peers_unavailable",
            &format!("{e:#}"),
            host,
            false,
        ),
    })
}
/// Where the CLI asks the proxy for paired machines, and the header carrying
/// the proxy's private token (`proxy.hosts-sync-token`).
pub(crate) const PEERS_PATH: &str = "/_porch/peers.json";
pub(crate) const PEERS_TOKEN_HEADER: &str = "x-porch-token";
async fn hosts_request(
    req: &Request<Incoming>,
    peer: SocketAddr,
    host: &str,
    token: &str,
    unprivileged: bool,
) -> Option<Response<Body>> {
    let path = req.uri().path();
    let challenge = (req
        .headers()
        .get_all("x-porch-hosts-sync-challenge")
        .iter()
        .count()
        == 1)
        .then(|| {
            req.headers()
                .get("x-porch-hosts-sync-challenge")
                .and_then(|v| v.to_str().ok())
        })
        .flatten();
    if path != "/_porch/hosts-sync"
        && !(path == "/" && req.method() == Method::HEAD && challenge.is_some())
    {
        return None;
    }
    let safe = peer.ip().is_loopback()
        && local_authority(host)
        && !req.headers().contains_key(header::ORIGIN)
        && !req.headers().contains_key("sec-fetch-site");
    if !safe {
        return Some(error(
            StatusCode::FORBIDDEN,
            "forbidden",
            "Hosts synchronization is restricted to local authenticated clients",
            host,
            false,
        ));
    }
    if unprivileged {
        return Some(error(
            StatusCode::CONFLICT,
            "hosts_sync_disabled",
            "Hosts synchronization is disabled in unprivileged mode",
            host,
            false,
        ));
    }
    if req.method() == Method::HEAD {
        let mut r = Response::new(empty());
        marker(&mut r);
        if let Some(proof) = challenge.and_then(|c| crate::integrations::hosts_sync_proof(token, c))
        {
            if let Ok(v) = proof.parse() {
                r.headers_mut().insert("x-porch-hosts-sync-proof", v);
            }
        }
        r.headers_mut().insert(
            header::CACHE_CONTROL,
            header::HeaderValue::from_static("no-store"),
        );
        return Some(r);
    }
    if req.method() != Method::POST {
        return Some(error(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "Use POST for hosts synchronization",
            host,
            false,
        ));
    }
    let supplied = (req
        .headers()
        .get_all("x-porch-hosts-sync-token")
        .iter()
        .count()
        == 1)
        .then(|| {
            req.headers()
                .get("x-porch-hosts-sync-token")
                .and_then(|v| v.to_str().ok())
        })
        .flatten()
        .unwrap_or("");
    // The token is the one this proxy loaded at start (the challenge proof
    // above uses it too), and the hosts-file write blocks: keep both off the
    // runtime's threads.
    if !crate::integrations::constant_eq(token, supplied) {
        return Some(error(
            StatusCode::FORBIDDEN,
            "forbidden",
            &crate::integrations::InvalidHostsToken.to_string(),
            host,
            false,
        ));
    }
    let synced = tokio::task::spawn_blocking(crate::integrations::auto_hosts_sync)
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("hosts synchronization failed: {e}")));
    Some(match synced {
        Ok(()) => {
            let mut r = Response::new(empty());
            marker(&mut r);
            r
        }
        Err(e) => error(
            StatusCode::CONFLICT,
            "hosts_sync_unavailable",
            &e.to_string(),
            host,
            false,
        ),
    })
}
fn redirect(req: &Request<Incoming>, host: &str, port: u16) -> Response<Body> {
    let hostname = host
        .parse::<hyper::http::uri::Authority>()
        .map(|a| a.host().to_owned())
        .unwrap_or_else(|_| "localhost".into());
    let suffix = if port == 443 {
        String::new()
    } else {
        format!(":{port}")
    };
    let location = format!(
        "https://{hostname}{suffix}{}",
        req.uri().path_and_query().map_or("/", |p| p.as_str())
    );
    let mut r = Response::new(empty());
    *r.status_mut() = StatusCode::FOUND;
    if let Ok(v) = location.parse() {
        r.headers_mut().insert(header::LOCATION, v);
    }
    marker(&mut r);
    r
}
async fn forward(
    mut req: Request<Incoming>,
    peer: SocketAddr,
    secure: bool,
    redirect_plain: bool,
    context: ProxyContext,
) -> Result<Response<Body>, Infallible> {
    let ProxyContext {
        client,
        loopback,
        loopback_insecure,
        settings,
        routing,
        token,
        limits,
        peers,
        mut stopped,
    } = context;
    let host = authority(&req);
    let html = req
        .headers()
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().contains("text/html"));
    if let Some(r) = hosts_request(&req, peer, &host, &token, settings.unprivileged).await {
        return Ok(r);
    }
    if let Some(r) = peers_request(&req, peer, &host, &token).await {
        return Ok(r);
    }
    let hostname = host
        .parse::<hyper::http::uri::Authority>()
        .map(|a| a.host().trim_end_matches('.').to_ascii_lowercase())
        .unwrap_or_default();
    if hostname.is_empty() {
        return Ok(error(
            StatusCode::BAD_REQUEST,
            "missing_host",
            "Missing Host header",
            &host,
            html,
        ));
    }
    let dashboard_host = settings.tlds.contains(&hostname) || hostname == "porch.localhost";
    if dashboard_host && req.uri().path() == "/_porch/apps.json" {
        // Only the dashboard itself may ask: another site's page could
        // otherwise make the proxy query every paired machine in a loop.
        if !peer.ip().is_loopback() || cross_site(&req) {
            return Ok(error(
                StatusCode::FORBIDDEN,
                "forbidden",
                "The app list is only available on this computer",
                &host,
                false,
            ));
        }
        let with_peers = req
            .uri()
            .query()
            .is_some_and(|q| q.split('&').any(|p| p == "peers=1"));
        let body = dashboard_json(&settings, with_peers.then_some(&*peers))
            .await
            .to_string();
        let mut r = Response::new(
            Full::new(Bytes::from(body))
                .map_err(|e: Infallible| match e {})
                .boxed_unsync(),
        );
        r.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json"),
        );
        r.headers_mut().insert(
            header::CACHE_CONTROL,
            header::HeaderValue::from_static("no-store"),
        );
        marker(&mut r);
        return Ok(r);
    }
    let extended = req.method() == Method::CONNECT
        && req.version() == Version::HTTP_2
        && req
            .extensions()
            .get::<hyper::ext::Protocol>()
            .is_some_and(|p| p.as_str() == "websocket");
    let upgrade = req.headers().get(header::UPGRADE).cloned().filter(|_| {
        req.headers()
            .get(header::CONNECTION)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|s| {
                s.split(',')
                    .any(|v| v.trim().eq_ignore_ascii_case("upgrade"))
            })
    });
    // Only a WebSocket handshake can't follow a redirect to HTTPS; any other
    // upgrade (h2c, for one) would otherwise skip it.
    if redirect_plain && !upgrade.as_ref().is_some_and(is_websocket) {
        return Ok(redirect(&req, &host, settings.port));
    }
    if req.method() == Method::CONNECT && !extended {
        return Ok(error(
            StatusCode::NOT_IMPLEMENTED,
            "unsupported_connect",
            "CONNECT is only supported for WebSockets (RFC 8441)",
            &host,
            html,
        ));
    }
    let hops = hop_count(req.headers(), peer.ip().is_loopback());
    if hops >= 5 {
        return Ok(error(StatusCode::LOOP_DETECTED,"loop_detected","Request passed through Porch too many times. Add changeOrigin: true to your dev server proxy config.",&host,html));
    }
    let state = match crate::state::live_routes() {
        Ok(s) => s,
        Err(_) => {
            return Ok(error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "state_error",
                "Cannot load routes",
                &host,
                html,
            ))
        }
    };
    let route = state
        .routes
        .iter()
        .filter_map(|r| score(&r.name, &hostname, &routing).map(|s| (s, r)))
        .max_by_key(|(s, _)| *s)
        .map(|(_, r)| r)
        .or_else(|| {
            let requested_port = host
                .parse::<hyper::http::uri::Authority>()
                .ok()
                .and_then(|a| a.port_u16())
                .unwrap_or(if secure { 443 } else { 80 });
            state
                .routes
                .iter()
                .rev()
                .filter_map(|r| {
                    let score = state
                        .shares
                        .get(&r.name)?
                        .values()
                        .filter_map(|s| url::Url::parse(s).ok())
                        .filter(|url| {
                            url.host_str()
                                .is_some_and(|h| h.eq_ignore_ascii_case(&hostname))
                        })
                        .map(|url| {
                            usize::from(
                                url.port_or_known_default().unwrap_or(443) == requested_port,
                            )
                        })
                        .max()?;
                    Some((score, r))
                })
                .max_by_key(|(score, _)| *score)
                .map(|(_, r)| r)
        });
    // Paired machines: APP.MACHINE.<tld> is served by the discovery gateway.
    let remote = route
        .is_none()
        .then(|| {
            crate::discovery::gateway::target(&hostname, peer.ip().is_loopback(), &settings.tlds)
        })
        .flatten();
    let upstream_spec = route.and_then(|r| state.upstreams.get(&r.name)).cloned();
    let Some(target) = route.map(|r| r.target.clone()).or(remote) else {
        let local_name = settings
            .tlds
            .iter()
            .chain(settings.lan.then_some("local".to_owned()).iter())
            .any(|t| hostname == *t || hostname.ends_with(&format!(".{t}")));
        if !local_name
            && hostname
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_err()
        {
            return Ok(error(
                StatusCode::MISDIRECTED_REQUEST,
                "misdirected_request",
                &format!("DNS sent {hostname} to this computer. This is not the real site."),
                &host,
                html,
            ));
        }
        if dashboard_host && !html && peer.ip().is_loopback() {
            let body = dashboard_json(&settings, None).await.to_string();
            let mut r = Response::new(
                Full::new(Bytes::from(body))
                    .map_err(|e: Infallible| match e {})
                    .boxed_unsync(),
            );
            r.headers_mut().insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/json"),
            );
            r.headers_mut()
                .insert(header::VARY, header::HeaderValue::from_static("Accept"));
            marker(&mut r);
            return Ok(r);
        }
        let mut response = error(
            StatusCode::NOT_FOUND,
            "not_found",
            "No app is registered for this hostname. Start it with porch <name> <command>.",
            &host,
            html,
        );
        if html {
            // Other devices (LAN mode) see names only; upstream targets stay private.
            let show_targets = peer.ip().is_loopback();
            let targets: Vec<String> = state.routes.iter().map(|r| r.target.clone()).collect();
            let health = tokio::task::spawn_blocking(move || {
                targets
                    .iter()
                    .map(|t| crate::health::target_listening_cached(t))
                    .collect::<Vec<_>>()
            })
            .await
            .unwrap_or_default();
            let mut order: Vec<usize> = (0..state.routes.len()).collect();
            order.sort_by_key(|&i| settings.hostname(&state.routes[i].name));
            let links = order
                .into_iter()
                .map(|i| (i, &state.routes[i]))
                .map(|(i, r)| {
                    let up = health.get(i).copied().unwrap_or(true);
                    format!(
                        "<li><a href=\"{0}\"><span class=\"name\"><span class=\"dot {1}\" title=\"{2}\"></span>{3}</span>{4}</a><button class=\"copy\" data-url=\"{0}\" title=\"Copy URL\" aria-label=\"Copy URL\">Copy</button></li>",
                        escape(&settings.url(&r.name)),
                        if up { "up" } else { "down" },
                        if up { "answering" } else { "not listening" },
                        escape(&settings.hostname(&r.name)),
                        if show_targets {
                            format!("<span class=\"target\">{}</span>", escape(&r.target))
                        } else {
                            String::new()
                        }
                    )
                })
                .collect::<Vec<_>>();
            let list = if links.is_empty() {
                "<p class=\"muted\">No apps are running. Start one with <code>porch run pnpm dev</code> or <code>porch NAME COMMAND</code>.</p>".to_owned()
            } else {
                let filter = if links.len() > 5 {
                    "<input type=\"search\" placeholder=\"Filter apps\" aria-label=\"Filter apps\" oninput=\"let n=0;for(const li of document.querySelectorAll('#local ul.apps li, ul.apps li')){li.hidden=!li.textContent.toLowerCase().includes(this.value.toLowerCase());n+=!li.hidden}document.getElementById('nomatch').hidden=n>0\" autofocus><p id=\"nomatch\" class=\"muted\" role=\"status\" hidden>No apps match.</p>"
                } else {
                    ""
                };
                format!(
                    "{filter}<ul class=\"apps\">{}</ul><script>{COPY_SCRIPT}</script>",
                    links.join("")
                )
            };
            // The bare domain (http://localhost) is Porch's dashboard: a 200
            // page listing every app, rather than an error.
            let page = if dashboard_host {
                *response.status_mut() = StatusCode::OK;
                let live = if peer.ip().is_loopback() {
                    DASHBOARD_SCRIPT
                } else {
                    ""
                };
                page(
                    StatusCode::OK,
                    "Your apps",
                    &format!(
                        "<p class=\"muted\">Every app Porch is serving on this computer. This page updates by itself.</p><p id=\"status\" class=\"muted\" role=\"status\" hidden>Can't reach Porch's proxy right now; retrying…</p><div id=\"local\">{list}</div><div id=\"peers\"></div>{live}"
                    ),
                    "",
                )
            } else {
                let body = format!(
                    "<p>No app is registered for <code>{}</code>.</p><h2>Running apps</h2>{list}",
                    escape(&hostname)
                );
                page(StatusCode::NOT_FOUND, "No app at this address", &body, "")
            };
            *response.body_mut() = Full::new(Bytes::from(page))
                .map_err(|e: Infallible| match e {})
                .boxed_unsync();
        }
        return Ok(response);
    };
    let target = if target.contains("://") {
        target
    } else {
        format!("http://{target}")
    };
    let target = match target.parse::<hyper::Uri>() {
        Ok(u) if u.authority().is_some() => u,
        _ => {
            return Ok(error(
                StatusCode::BAD_GATEWAY,
                "invalid_target",
                "Invalid route target",
                &host,
                html,
            ))
        }
    };
    let path = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    // Dial the address the route records. An app Porch launched on
    // 127.0.0.1 may listen on [::1] alone; it's dialed there only when
    // nothing listens on 127.0.0.1 (see `upstream::ToLoopback`). A plain
    // alias never moves: another user could be listening on the other one.
    let launched = route.is_some_and(|r| r.pid.is_some())
        && target.host() == Some("127.0.0.1")
        && target.scheme_str() == Some("http");
    let uri = format!(
        "{}://{}{}{}",
        target.scheme_str().unwrap_or("http"),
        target.authority().unwrap(),
        target.path().trim_end_matches('/'),
        path
    );
    // `--upstream`: same loopback port, but our own scheme and TLS name.
    let uri = match &upstream_spec {
        Some(up) => crate::upstream::request(up, target.port_u16().unwrap_or(80), path).0,
        None => uri,
    };
    let Ok(uri) = uri.parse::<hyper::Uri>() else {
        return Ok(error(
            StatusCode::BAD_GATEWAY,
            "invalid_target",
            "Invalid upstream URI",
            &host,
            html,
        ));
    };
    let downstream = (upgrade.is_some() || extended).then(|| hyper::upgrade::on(&mut req));
    strip(req.headers_mut());
    let ws_key = extended.then(tokio_tungstenite::tungstenite::handshake::client::generate_key);
    if extended {
        *req.method_mut() = Method::GET;
        req.headers_mut().insert(
            header::UPGRADE,
            header::HeaderValue::from_static("websocket"),
        );
        req.headers_mut().insert(
            "sec-websocket-key",
            ws_key.as_ref().unwrap().parse().unwrap(),
        );
        req.headers_mut()
            .entry("sec-websocket-version")
            .or_insert(header::HeaderValue::from_static("13"));
    } else if let Some(v) = upgrade {
        req.headers_mut().insert(header::UPGRADE, v);
    }
    if downstream.is_some() {
        req.headers_mut().insert(
            header::CONNECTION,
            header::HeaderValue::from_static("upgrade"),
        );
    }
    // Only a client on this machine (another proxy, a tunnel) may set its own
    // forwarding headers. Anyone else on the LAN could otherwise spoof its
    // address, or the scheme, host or path the app sees.
    if !peer.ip().is_loopback() {
        strip_client_forwarding(req.headers_mut());
    }
    merge_cookies(req.headers_mut());
    let forwarded_for = req
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .map(|s| format!("{s}, {}", peer.ip()))
        .unwrap_or_else(|| peer.ip().to_string());
    for (name, value) in [
        ("x-forwarded-for", forwarded_for),
        ("x-forwarded-host", host.clone()),
        (
            "x-forwarded-proto",
            if secure { "https" } else { "http" }.into(),
        ),
        (
            "x-forwarded-port",
            host.parse::<hyper::http::uri::Authority>()
                .ok()
                .and_then(|a| a.port_u16())
                .unwrap_or(if secure { 443 } else { 80 })
                .to_string(),
        ),
        ("x-porch-hops", (hops + 1).to_string()),
        ("x-portless-hops", (hops + 1).to_string()),
    ] {
        if (name == "x-forwarded-for"
            || name == "x-portless-hops"
            || name == "x-porch-hops"
            || !req.headers().contains_key(name))
            && value.parse::<header::HeaderValue>().is_ok()
        {
            req.headers_mut().insert(name, value.parse().unwrap());
        }
    }
    // A remote alias (https://api.example.com) is a real site: it needs its own
    // name in Host (TLS SNI already uses it), and its redirects to itself are
    // rewritten to stay on the Porch URL. Local apps keep the browser's Host.
    let remote_origin = target
        .host()
        .map(|h| h.trim_matches(['[', ']']))
        .filter(|h| {
            !h.eq_ignore_ascii_case("localhost")
                && !h
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        })
        .map(|_| {
            format!(
                "{}://{}",
                target.scheme_str().unwrap_or("http"),
                target.authority().unwrap()
            )
        });
    let host_header =
        if let Some(authority) = upstream_spec.as_ref().and_then(|u| u.authority.clone()) {
            authority
        } else if remote_origin.is_some() {
            target.authority().unwrap().to_string()
        } else {
            host.clone()
        };
    if let Ok(v) = host_header.parse() {
        req.headers_mut().insert(header::HOST, v);
    }
    let public_origin = format!("{}://{host}", if secure { "https" } else { "http" });
    *req.uri_mut() = uri;
    *req.version_mut() = Version::HTTP_11;
    let (parts, body) = req.into_parts();
    let req = Request::from_parts(
        parts,
        if extended {
            empty()
        } else if peer.ip().is_loopback() {
            body.map_err(BoxError::from).boxed_unsync()
        } else {
            // Another device can't hold the request open by pausing its body.
            ReadTimeout::new(body, BODY_READ_TIMEOUT).boxed_unsync()
        },
    );
    let sent = match &upstream_spec {
        Some(up) if up.insecure => loopback_insecure.request(req).await,
        Some(_) => loopback.request(req).await,
        None if launched => loopback.request(req).await,
        None => client.request(req).await,
    };
    match sent {
        Ok(mut response) => {
            if response.status() == StatusCode::SWITCHING_PROTOCOLS {
                if let Some(key) = ws_key {
                    let expected = tokio_tungstenite::tungstenite::handshake::derive_accept_key(
                        key.as_bytes(),
                    );
                    if response
                        .headers()
                        .get("sec-websocket-accept")
                        .and_then(|v| v.to_str().ok())
                        != Some(&expected)
                    {
                        return Ok(error(
                            StatusCode::BAD_GATEWAY,
                            "invalid_websocket_handshake",
                            "Upstream sent an invalid WebSocket accept header",
                            &host,
                            html,
                        ));
                    }
                }
                if let Some(downstream) = downstream {
                    // Upgraded sockets outlive their connection task, so they
                    // take their own slot for their whole lifetime.
                    let Some(slot) = limits.acquire(peer.ip()) else {
                        return Ok(error(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "too_many_websockets",
                            "Too many open WebSocket connections through Porch",
                            &host,
                            html,
                        ));
                    };
                    let upstream = hyper::upgrade::on(&mut response);
                    tokio::spawn(async move {
                        let _slot = slot;
                        if let Ok((a, b)) = tokio::try_join!(downstream, upstream) {
                            let mut a = TokioIo::new(a);
                            let mut b = TokioIo::new(b);
                            tokio::select! {
                                _ = tokio::io::copy_bidirectional(&mut a, &mut b) => {},
                                _ = stopped.wait_for(|stop| *stop) => {},
                            };
                        }
                    });
                }
                if extended {
                    *response.status_mut() = StatusCode::OK;
                    strip(response.headers_mut());
                    response.headers_mut().remove("sec-websocket-accept");
                    let (parts, _) = response.into_parts();
                    let mut r = Response::from_parts(parts, empty());
                    marker(&mut r);
                    return Ok(r);
                }
            } else {
                strip(response.headers_mut());
                // Redirects a backend sends to its own address stay on the
                // Porch URL: remote aliases and `--upstream` authorities.
                let origins: Vec<String> = match (&upstream_spec, &remote_origin) {
                    (Some(up), _) => crate::upstream::origins(up),
                    (None, Some(_)) => crate::upstream::origins(&crate::state::Upstream {
                        tls: target.scheme_str() == Some("https"),
                        authority: target.authority().map(|a| a.to_string()),
                        insecure: false,
                    }),
                    (None, None) => vec![],
                };
                // Cookies a real backend scopes to its own domain would never
                // come back to the Porch name; keep them host-only instead.
                let backend_host = upstream_spec
                    .as_ref()
                    .and_then(|u| u.authority.as_deref())
                    .and_then(|a| a.parse::<hyper::http::uri::Authority>().ok())
                    .map(|a| a.host().to_owned())
                    .or_else(|| remote_origin.as_ref().and(target.host().map(str::to_owned)));
                if let Some(backend) = backend_host {
                    let cookies: Vec<_> = response
                        .headers()
                        .get_all(header::SET_COOKIE)
                        .iter()
                        .map(|v| {
                            v.to_str()
                                .ok()
                                .and_then(|c| crate::upstream::host_only_cookie(c, &backend))
                                .and_then(|c| header::HeaderValue::from_str(&c).ok())
                                .unwrap_or_else(|| v.clone())
                        })
                        .collect();
                    if !cookies.is_empty() {
                        response.headers_mut().remove(header::SET_COOKIE);
                        for c in cookies {
                            response.headers_mut().append(header::SET_COOKIE, c);
                        }
                    }
                }
                let base = target.path().trim_end_matches('/');
                let rewritten = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|l| {
                        let absolute =
                            crate::upstream::rewrite_location(l, &origins, &public_origin);
                        redirect_location(l, absolute, base, &public_origin)
                    })
                    .and_then(|l| header::HeaderValue::from_str(&l).ok());
                if let Some(v) = rewritten {
                    response.headers_mut().insert(header::LOCATION, v);
                }
            }
            let mut r = response.map(|b| b.map_err(BoxError::from).boxed_unsync());
            marker(&mut r);
            Ok(r)
        }
        Err(e) => {
            let (code, kind, message) =
                upstream_failure(&e, remote_origin.is_none(), upstream_spec.is_some());
            Ok(error(code, kind, message, &host, html))
        }
    }
}
/// Why a request to the app failed. Only an app on this computer that isn't
/// listening yet gets the self-reloading "Waiting for the app to start" page.
/// The messages name no target, since other devices (LAN mode) see them too.
fn upstream_failure(
    e: &hyper_util::client::legacy::Error,
    loopback_target: bool,
    upstream_alias: bool,
) -> (StatusCode, &'static str, &'static str) {
    use std::io::ErrorKind;
    if chain(e).any(|e| e.is::<BodyReadTimeout>()) {
        return (
            StatusCode::REQUEST_TIMEOUT,
            "request_timeout",
            "The request body stopped arriving.",
        );
    }
    if certificate_error(e) {
        return (
            StatusCode::BAD_GATEWAY,
            "upstream_certificate",
            if upstream_alias {
                "The app's TLS certificate isn't trusted for this name. Trust its CA, or re-register the alias with --insecure."
            } else {
                "The site's TLS certificate isn't trusted for its name. Check the alias's URL, or trust the CA that issued the certificate."
            },
        );
    }
    let gateway = |message| (StatusCode::BAD_GATEWAY, "upstream_error", message);
    if !e.is_connect() {
        return gateway("The app closed the connection before sending a complete response. Check its output: it may have crashed.");
    }
    match crate::upstream::io_error_kind(e) {
        Some(ErrorKind::ConnectionRefused) if loopback_target => (
            StatusCode::BAD_GATEWAY,
            "upstream_unavailable",
            "The target app is not accepting connections. Check its output and PORT setting.",
        ),
        Some(ErrorKind::ConnectionRefused) => gateway("The backend refused the connection."),
        Some(ErrorKind::TimedOut) => (
            StatusCode::GATEWAY_TIMEOUT,
            "upstream_timeout",
            "The backend didn't accept the connection in time.",
        ),
        _ if chain(e).any(|e| e.to_string().starts_with("dns error")) => {
            gateway("The backend's hostname could not be resolved.")
        }
        _ => gateway("Couldn't connect to the backend."),
    }
}
fn chain<'a>(
    e: &'a (dyn std::error::Error + 'static),
) -> impl Iterator<Item = &'a (dyn std::error::Error + 'static)> {
    std::iter::successors(Some(e), |e| e.source())
}
/// A pause longer than [`BODY_READ_TIMEOUT`] while sending a request body.
#[derive(Debug)]
struct BodyReadTimeout;
impl std::fmt::Display for BodyReadTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("request body read timed out")
    }
}
impl std::error::Error for BodyReadTimeout {}
/// A request body that fails when the client pauses longer than `limit`
/// between frames while the app is waiting for more.
struct ReadTimeout<B> {
    inner: B,
    limit: Duration,
    sleep: Option<Pin<Box<tokio::time::Sleep>>>,
}
impl<B> ReadTimeout<B> {
    fn new(inner: B, limit: Duration) -> Self {
        Self {
            inner,
            limit,
            sleep: None,
        }
    }
}
impl<B> hyper::body::Body for ReadTimeout<B>
where
    B: hyper::body::Body<Data = Bytes> + Unpin,
    B::Error: Into<BoxError>,
{
    type Data = Bytes;
    type Error = BoxError;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Result<hyper::body::Frame<Bytes>, BoxError>>> {
        let this = &mut *self;
        if let Poll::Ready(frame) = Pin::new(&mut this.inner).poll_frame(cx) {
            this.sleep = None;
            return Poll::Ready(frame.map(|f| f.map_err(Into::into)));
        }
        let limit = this.limit;
        let sleep = this
            .sleep
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(limit)));
        if sleep.as_mut().poll(cx).is_ready() {
            this.sleep = None;
            return Poll::Ready(Some(Err(Box::new(BodyReadTimeout))));
        }
        Poll::Pending
    }
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
    fn size_hint(&self) -> hyper::body::SizeHint {
        self.inner.size_hint()
    }
}
fn listener(addr: SocketAddr) -> Result<tokio::net::TcpListener> {
    let listener =
        crate::sockets::bind(addr, true).with_context(|| format!("binding proxy {addr}"))?;
    listener.set_nonblocking(true)?;
    Ok(tokio::net::TcpListener::from_std(listener)?)
}
fn unavailable_ipv6(addr: SocketAddr, error: &anyhow::Error) -> bool {
    #[cfg(unix)]
    let codes = [libc::EAFNOSUPPORT, libc::EADDRNOTAVAIL];
    // Winsock equivalents, confirmed against windows-sys WinSock bindings.
    #[cfg(windows)]
    let codes = [10047, 10049];
    #[cfg(not(any(unix, windows)))]
    let codes: [i32; 0] = [];
    addr.is_ipv6()
        && error
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::raw_os_error)
            .is_some_and(|code| codes.contains(&code))
}
fn listen_if_available(
    addr: SocketAddr,
    allow_ipv6_fallback: bool,
) -> Result<Option<tokio::net::TcpListener>> {
    match listener(addr) {
        Ok(listener) => Ok(Some(listener)),
        Err(error) if allow_ipv6_fallback && unavailable_ipv6(addr, &error) => {
            eprintln!("IPv6 unavailable on {addr}; retaining IPv4 listener");
            Ok(None)
        }
        Err(error) => Err(error),
    }
}
/// Requests in flight on one client connection, and when it was last busy.
struct Activity {
    state: Mutex<(usize, u64, tokio::time::Instant)>,
}
impl Activity {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new((0, 0, tokio::time::Instant::now())),
        })
    }
    fn begin(self: &Arc<Self>) -> InFlight {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.0 += 1;
        s.1 += 1;
        InFlight(self.clone())
    }
    /// (requests in flight, requests started, last busy)
    fn snapshot(&self) -> (usize, u64, tokio::time::Instant) {
        *self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
    /// Resolves once the connection has had no request in flight for too
    /// long: `first` until a request starts, then `after` (never, if `None`).
    async fn idle(&self, first: Duration, after: Option<Duration>) {
        loop {
            let (in_flight, started, last) = self.snapshot();
            let limit = match (started, after) {
                (0, _) => first,
                (_, Some(after)) => after,
                (_, None) => return std::future::pending().await,
            };
            if in_flight == 0 && last.elapsed() >= limit {
                return;
            }
            let wake = if in_flight == 0 {
                last + limit
            } else {
                tokio::time::Instant::now() + limit
            };
            tokio::time::sleep_until(wake).await;
        }
    }
}
/// One request in flight; it ends when its response body is dropped.
struct InFlight(Arc<Activity>);
impl Drop for InFlight {
    fn drop(&mut self) {
        let mut s = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        s.0 -= 1;
        s.2 = tokio::time::Instant::now();
    }
}
/// A response body that keeps its request counted as in flight.
struct Tracked {
    body: Body,
    _in_flight: InFlight,
}
impl hyper::body::Body for Tracked {
    type Data = Bytes;
    type Error = BoxError;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Result<hyper::body::Frame<Bytes>, BoxError>>> {
        Pin::new(&mut self.body).poll_frame(cx)
    }
    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }
    fn size_hint(&self) -> hyper::body::SizeHint {
        self.body.size_hint()
    }
}
trait Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Io for T {}
/// Probes a vanished client (LAN mode: a device that left without closing
/// its connections) so its connection, WebSocket included, is closed.
fn keepalive(stream: &tokio::net::TcpStream) {
    let keepalive = socket2::TcpKeepalive::new().with_time(Duration::from_secs(60));
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    let keepalive = keepalive.with_interval(Duration::from_secs(10));
    let _ = socket2::SockRef::from(stream).set_tcp_keepalive(&keepalive);
}
async fn stop_requested(stopped: &mut tokio::sync::watch::Receiver<bool>) {
    // A dropped sender means the proxy is gone: stop as well.
    let _ = stopped.wait_for(|stop| *stop).await;
}
async fn listen(
    listener: tokio::net::TcpListener,
    context: ProxyContext,
    acceptor: Option<tokio_rustls::TlsAcceptor>,
    redirect_only: bool,
) -> Result<()> {
    let mut connections = tokio::task::JoinSet::new();
    let mut stopped = context.stopped.clone();
    loop {
        let accepted = tokio::select! {
            result = listener.accept() => result,
            _ = connections.join_next(), if !connections.is_empty() => continue,
            _ = stop_requested(&mut stopped) => break,
        };
        let (stream, peer) = match accepted {
            Ok(connection) => connection,
            Err(e) => {
                eprintln!("Porch accept failed: {e}; retrying");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        // Wildcard low-port binding in unprivileged mode still rejects
        // non-loopback peers, before reading a byte.
        if !context.settings.lan && !peer.ip().is_loopback() {
            continue;
        }
        // Bound the work a flood of (or a LAN full of) idle clients can pin.
        let Some(slot) = context.limits.acquire(peer.ip()) else {
            continue;
        };
        keepalive(&stream);
        connections.spawn(serve_connection(
            stream,
            peer,
            context.clone(),
            acceptor.clone(),
            redirect_only,
            slot,
        ));
    }
    // Stop accepting at once, so a restarted proxy can bind the port, and let
    // requests in flight finish for a moment.
    drop(listener);
    let drained = tokio::time::timeout(DRAIN_TIMEOUT, async {
        while connections.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        connections.abort_all();
    }
    Ok(())
}
async fn serve_connection(
    stream: tokio::net::TcpStream,
    peer: SocketAddr,
    context: ProxyContext,
    acceptor: Option<tokio_rustls::TlsAcceptor>,
    redirect_only: bool,
    _slot: Slot,
) {
    let mut stopped = context.stopped.clone();
    let mut first = [0u8; 1];
    let tls = if acceptor.is_some() {
        matches!(tokio::time::timeout(Duration::from_secs(10),stream.peek(&mut first)).await,Ok(Ok(n)) if n>0 && first[0]==0x16)
    } else {
        false
    };
    let redirect_plain = redirect_only || (acceptor.is_some() && !tls);
    let io: Box<dyn Io> = match acceptor.filter(|_| tls) {
        Some(acceptor) => {
            match tokio::time::timeout(Duration::from_secs(10), acceptor.accept(stream)).await {
                Ok(Ok(stream)) => Box::new(stream),
                _ => return,
            }
        }
        None => Box::new(stream),
    };
    let activity = Activity::new();
    let requests = activity.clone();
    let service = hyper::service::service_fn(move |req| {
        let in_flight = requests.begin();
        let response = forward(req, peer, tls, redirect_plain, context.clone());
        async move {
            let response = response.await?;
            Ok::<_, Infallible>(response.map(|body| Tracked {
                body,
                _in_flight: in_flight,
            }))
        }
    });
    let mut builder = Builder::new(TokioExecutor::new());
    // Slow or silent clients can't hold a connection open forever
    // before sending a full request head.
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_READ_TIMEOUT);
    builder
        .http2()
        .timer(TokioTimer::new())
        .enable_connect_protocol()
        .keep_alive_interval(Some(Duration::from_secs(30)))
        .keep_alive_timeout(Duration::from_secs(20));
    let connection = builder.serve_connection_with_upgrades(TokioIo::new(io), service);
    tokio::pin!(connection);
    // Every client must start a request soon after connecting. Another device
    // (LAN mode) also can't keep a connection that is doing nothing, such as
    // an HTTP/2 connection that only answers pings; long-lived responses
    // (event streams, WebSockets) count as busy.
    let idle = activity.idle(
        HEADER_READ_TIMEOUT,
        (!peer.ip().is_loopback()).then_some(LAN_IDLE_TIMEOUT),
    );
    tokio::select! {
        _ = connection.as_mut() => return,
        _ = stop_requested(&mut stopped) => {},
        _ = idle => {},
    }
    // Finish the requests in flight, then close; the listener bounds how long.
    connection.as_mut().graceful_shutdown();
    let _ = connection.await;
}
/// Run `task` again whenever it fails or ends, logging why and waiting
/// longer after each quick failure. For helpers (LAN announcements) whose
/// failure must not take down a healthy proxy.
async fn supervise<F, Fut>(
    name: &'static str,
    mut task: F,
    first_delay: Duration,
    max_delay: Duration,
) where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    let mut delay = first_delay;
    loop {
        let began = tokio::time::Instant::now();
        let mut run = AbortOnDrop(tokio::spawn(task()));
        let reason = match (&mut run.0).await {
            Ok(Ok(())) => "it stopped".to_owned(),
            Ok(Err(e)) => format!("{e:#}"),
            Err(e) => format!("it crashed: {e}"),
        };
        if began.elapsed() >= max_delay {
            delay = first_delay;
        }
        eprintln!(
            "Porch {name} failed: {reason}; restarting in {}s",
            delay.as_secs_f32()
        );
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(max_delay);
    }
}
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);
impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}
/// Retained for embedding and existing callers; an explicit bind selects one interface.
pub async fn serve(
    bind: String,
    port: u16,
    tls: bool,
    cert: Option<PathBuf>,
    key: Option<PathBuf>,
) -> Result<()> {
    serve_config(ProxySettings {
        bind: Some(bind),
        port,
        tls,
        cert,
        key,
        redirect_port: None,
        ..ProxySettings::default()
    })
    .await
}
/// Serve until a listener fails.
pub async fn serve_config(settings: ProxySettings) -> Result<()> {
    serve_until(settings, std::future::pending::<Result<()>>()).await
}
/// Serve until a listener fails or `stop` completes. On stop, the proxy closes
/// its listeners at once and gives requests in flight a few seconds to finish;
/// then it returns `stop`'s error, if any.
pub async fn serve_until<T>(
    settings: ProxySettings,
    stop: impl Future<Output = Result<T>>,
) -> Result<()> {
    // Check every interface before opening our own listeners. A listener on an
    // unrelated family/interface must never be mistaken for this proxy.
    drop(crate::sockets::reserve(settings.port).context("proxy port unavailable")?);
    if let Some(port) = settings
        .redirect_port
        .filter(|p| settings.tls && *p != settings.port)
    {
        drop(crate::sockets::reserve(port).context("redirect port unavailable")?);
    }
    let acceptor = if settings.tls {
        Some(tokio_rustls::TlsAcceptor::from(
            crate::tls::config_for_settings(&settings)?,
        ))
    } else {
        None
    };
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    if let Ok(ca) = std::fs::read(crate::state::dir().join("ca.pem")) {
        for cert in crate::pem::certs(&mut std::io::BufReader::new(ca.as_slice())).flatten() {
            let _ = roots.add(cert);
        }
    }
    let tls_config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_root_certificates(roots)
    .with_no_client_auth();
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    http.set_connect_timeout(Some(Duration::from_secs(10)));
    http.set_happy_eyeballs_timeout(Some(Duration::from_millis(50)));
    let connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(tls_config.clone())
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .wrap_connector(http);
    let client = Client::builder(TokioExecutor::new()).build::<_, Body>(connector);
    let loopback = crate::upstream::client(tls_config.clone());
    let loopback_insecure = crate::upstream::client(crate::upstream::insecure_tls()?);
    let listeners = open_listeners(&settings)?;
    crate::state::atomic_write(
        "proxy.hosts-capability",
        &serde_json::to_vec(&serde_json::json!({
            "pid": std::process::id(), "writable": crate::integrations::hosts_sync_capable()
        }))?,
    )?;
    let token = Arc::new(crate::integrations::hosts_sync_token()?);
    let routing = Arc::new(Routing::new(&settings));
    let settings = Arc::new(settings);
    let mut tasks = tokio::task::JoinSet::new();
    let (shutdown, stopped) = tokio::sync::watch::channel(false);
    let context = ProxyContext {
        client,
        loopback,
        loopback_insecure,
        settings: settings.clone(),
        routing,
        token,
        // One budget for every listener and WebSocket: each holds two
        // descriptors (client and app) for as long as it is open.
        limits: Arc::new(Limits::new(connection_limit())),
        peers: Arc::new(tokio::sync::Mutex::new(None)),
        stopped,
    };
    for (socket, redirect_only) in listeners {
        tasks.spawn(listen(
            socket,
            context.clone(),
            if redirect_only {
                None
            } else {
                acceptor.clone()
            },
            redirect_only,
        ));
    }
    crate::discovery::gateway::start(settings.tlds.clone());
    crate::state::atomic_write("proxy.ready", std::process::id().to_string().as_bytes())?;
    // LAN announcements run beside the listeners: if mDNS fails, the proxy
    // keeps serving and the announcements restart.
    let _announcements = settings.lan.then(|| {
        let settings = (*settings).clone();
        AbortOnDrop(tokio::spawn(supervise(
            "LAN announcements",
            move || crate::integrations::lan_watch(settings.clone()),
            Duration::from_secs(1),
            Duration::from_secs(60),
        )))
    });
    let stopped = tokio::select! {
        ended = tasks.join_next() => match ended {
            Some(result) => { result??; anyhow::bail!("a proxy listener stopped") }
            None => anyhow::bail!("proxy has no listeners"),
        },
        stopped = stop => stopped,
    };
    let _ = shutdown.send(true);
    // Each listener bounds its own drain; this only backs that up.
    let _ = tokio::time::timeout(DRAIN_TIMEOUT + Duration::from_secs(1), async {
        while tasks.join_next().await.is_some() {}
    })
    .await;
    stopped.map(drop)
}
/// The proxy's listeners: (listener, serves only the HTTP-to-HTTPS redirect).
fn open_listeners(settings: &ProxySettings) -> Result<Vec<(tokio::net::TcpListener, bool)>> {
    let loopback = || vec!["127.0.0.1".to_owned(), "::1".to_owned()];
    let wildcard = || vec!["0.0.0.0".to_owned(), "::".to_owned()];
    let low_port = settings.unprivileged && settings.port < crate::ports::FIRST_UNPRIVILEGED_PORT;
    let choices = if let Some(bind) = &settings.bind {
        vec![vec![bind.clone()]]
    } else if settings.lan {
        vec![wildcard()]
    } else if low_port {
        // macOS lets any user bind ports below 1024 on the wildcard address
        // only. Elsewhere (Linux with a lowered ip_unprivileged_port_start,
        // Windows) loopback works and keeps the port off the network.
        vec![loopback(), wildcard()]
    } else {
        vec![loopback()]
    };
    let last = choices.len() - 1;
    for (i, binds) in choices.into_iter().enumerate() {
        match listeners_on(&binds, settings) {
            Ok(listeners) => return Ok(listeners),
            Err(e) if i < last && permission_denied(&e) => continue,
            Err(e) => return Err(e),
        }
    }
    unreachable!("the last choice returns")
}
fn permission_denied(e: &anyhow::Error) -> bool {
    e.downcast_ref::<std::io::Error>()
        .is_some_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied)
}
fn listeners_on(
    binds: &[String],
    settings: &ProxySettings,
) -> Result<Vec<(tokio::net::TcpListener, bool)>> {
    let mut listeners = Vec::new();
    for bind in binds {
        let ip: std::net::IpAddr = bind.parse().context("--bind must be an IP address")?;
        let Some(primary) =
            listen_if_available(SocketAddr::new(ip, settings.port), settings.bind.is_none())?
        else {
            continue;
        };
        listeners.push((primary, false));
        if settings.tls {
            if let Some(port) = settings.redirect_port.filter(|p| *p != settings.port) {
                if let Some(redirect) =
                    listen_if_available(SocketAddr::new(ip, port), settings.bind.is_none())?
                {
                    listeners.push((redirect, true));
                }
            }
        }
    }
    Ok(listeners)
}
/// Did the upstream TLS handshake fail on the backend's certificate?
fn certificate_error(e: &(dyn std::error::Error + 'static)) -> bool {
    let is_cert = |tls: &rustls::Error| {
        matches!(
            tls,
            rustls::Error::InvalidCertificate(_) | rustls::Error::NoCertificatesPresented
        )
    };
    let mut source = Some(e);
    while let Some(err) = source {
        if let Some(tls) = err.downcast_ref::<rustls::Error>() {
            return is_cert(tls);
        }
        // hyper-rustls nests the rustls error inside io::Error wrappers,
        // which don't expose it through source().
        let mut io = err.downcast_ref::<std::io::Error>();
        while let Some(inner) = io.and_then(|e| e.get_ref()) {
            if let Some(tls) = inner.downcast_ref::<rustls::Error>() {
                return is_cert(tls);
            }
            io = inner.downcast_ref::<std::io::Error>();
        }
        source = err.source();
    }
    false
}
#[cfg(test)]
mod tests {
    use super::*;
    fn routing_for(tlds: &[&str], lan: bool, wildcard: bool) -> Routing {
        Routing::new(&ProxySettings {
            tlds: tlds.iter().map(|t| (*t).to_owned()).collect(),
            lan,
            wildcard,
            ..ProxySettings::default()
        })
    }
    #[test]
    fn routing() {
        let tlds = ["example.com", "dev.example.com", "localhost"];
        let s = routing_for(&tlds, false, false);
        assert!(score("api.web", "api.web.localhost", &s).is_some());
        assert!(score("app", "app.dev.example.com", &s).is_some());
        assert!(score("app.dev", "app.dev.example.com", &s).is_none());
        assert!(score("app.localhost", "app.example.com", &s).is_none());
        assert!(score("app.localhost", "app.localhost", &s).is_some());
        assert!(score("App", "app.localhost", &s).is_some());
        assert!(score("app", "x.app.localhost", &s).is_none());
        assert!(score("app", "app.local", &s).is_none());
        assert!(score("app", "app.local", &routing_for(&tlds, true, false)).is_some());
        let s = routing_for(&tlds, false, true);
        assert!(score("app", "x.app.localhost", &s).is_some());
        assert!(score("app", "xapp.localhost", &s).is_none());
        assert!(
            score("api.app", "x.api.app.localhost", &s) > score("app", "x.api.app.localhost", &s)
        );
        assert!(score("app", "app.localhost", &s) > score("localhost", "app.localhost", &s));
        // A hand-edited name that isn't ASCII never matches, and never panics.
        assert!(score("é", "a.localhost", &s).is_none());
        assert!(score("aé.localhost", "b.localhost", &s).is_none());
    }
    #[test]
    fn longest_tld_wins_for_overlapping_suffixes() {
        let s = routing_for(&["example.com", "dev.example.com"], false, false);
        assert_eq!(s.tlds, ["dev.example.com", "example.com"]);
        // `app.dev.example.com` is app under dev.example.com, not app.dev
        // under example.com.
        assert!(score("app", "app.dev.example.com", &s).is_some());
        assert!(score("app.dev", "app.dev.example.com", &s).is_none());
    }
    #[test]
    fn other_devices_cannot_set_forwarding_headers() {
        let mut h = header::HeaderMap::new();
        for name in [
            "x-forwarded-for",
            "x-forwarded-host",
            "x-forwarded-proto",
            "x-forwarded-port",
            "x-forwarded-prefix",
            "x-forwarded-scheme",
            "x-forwarded-ssl",
            "x-forwarded-server",
            "forwarded",
            "x-real-ip",
            "x-client-ip",
            "true-client-ip",
            "x-original-url",
            "x-original-host",
            "x-rewrite-url",
        ] {
            h.insert(name, "spoofed".parse().unwrap());
        }
        h.insert("x-request-id", "kept".parse().unwrap());
        h.insert("cookie", "kept".parse().unwrap());
        strip_client_forwarding(&mut h);
        let mut kept: Vec<_> = h.keys().map(|k| k.as_str()).collect();
        kept.sort_unstable();
        assert_eq!(kept, ["cookie", "x-request-id"]);
    }
    #[test]
    fn split_cookies_are_joined() {
        let mut h = header::HeaderMap::new();
        h.append("cookie", "a=1".parse().unwrap());
        h.append("cookie", "b=2".parse().unwrap());
        h.append("cookie", "c=3".parse().unwrap());
        merge_cookies(&mut h);
        assert_eq!(
            h.get_all("cookie").iter().collect::<Vec<_>>(),
            ["a=1; b=2; c=3"]
        );
        let mut single = header::HeaderMap::new();
        single.insert("cookie", "a=1".parse().unwrap());
        merge_cookies(&mut single);
        assert_eq!(single["cookie"], "a=1");
        let mut none = header::HeaderMap::new();
        merge_cookies(&mut none);
        assert!(none.is_empty());
    }
    #[test]
    fn hop_counts() {
        let mut h = header::HeaderMap::new();
        h.insert("x-porch-hops", "junk".parse().unwrap());
        h.insert("x-portless-hops", "4".parse().unwrap());
        assert_eq!(hop_count(&h, true), 4);
        assert_eq!(hop_count(&h, false), 0);
        h.insert("x-porch-hops", "2".parse().unwrap());
        assert_eq!(hop_count(&h, true), 4);
        h.insert("x-porch-hops", "6".parse().unwrap());
        assert_eq!(hop_count(&h, true), 6);
        assert_eq!(hop_count(&header::HeaderMap::new(), true), 0);
    }
    #[test]
    fn only_websockets_skip_the_https_redirect() {
        for v in ["websocket", "WebSocket", "foo, websocket"] {
            assert!(is_websocket(&v.parse().unwrap()), "{v}");
        }
        for v in ["h2c", "websocketx", "TLS/1.2"] {
            assert!(!is_websocket(&v.parse().unwrap()), "{v}");
        }
    }
    #[test]
    fn redirects_drop_the_alias_base_path() {
        let origin = "http://app.localhost";
        let loc = |l: &str| redirect_location(l, None, "/base", origin);
        assert_eq!(loc("/base/login?x=1"), Some("/login?x=1".into()));
        assert_eq!(loc("/base"), Some("/".into()));
        assert_eq!(loc("/base?x"), Some("/?x".into()));
        assert_eq!(loc("/basement"), None);
        assert_eq!(loc("/other"), None);
        // Never a protocol-relative URL, which would leave the Porch name.
        assert_eq!(loc("/base//evil.example/"), Some("/evil.example/".into()));
        assert_eq!(loc("/base/\\evil.example/"), Some("/evil.example/".into()));
        assert_eq!(loc("/base///"), Some("/".into()));
        assert_eq!(loc("//evil.example/"), None);
        // Rewritten absolute redirects keep the public origin.
        assert_eq!(
            redirect_location(
                "https://backend/base//x",
                Some(format!("{origin}/base//x")),
                "/base",
                origin
            ),
            Some(format!("{origin}/x"))
        );
        assert_eq!(
            redirect_location("https://other/", None, "/base", origin),
            None
        );
        // No base: only a rewritten origin changes anything.
        assert_eq!(redirect_location("/a", None, "", origin), None);
    }
    #[test]
    fn connection_budget_follows_the_descriptor_limit() {
        assert_eq!(connections_for(64), MIN_CONNECTIONS);
        assert_eq!(connections_for(256), 96);
        assert_eq!(connections_for(1024), 448);
        assert_eq!(connections_for(1 << 20), MAX_CONNECTIONS);
        assert_eq!(connections_for(u64::MAX), MAX_CONNECTIONS);
    }
    #[cfg(unix)]
    #[test]
    fn descriptor_limit_is_raised() {
        let limit = raise_fd_limit();
        let mut now = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut now) }, 0);
        #[allow(clippy::unnecessary_cast)]
        let (current, hard) = (now.rlim_cur as u64, now.rlim_max as u64);
        assert_eq!(limit, current);
        assert!(current >= hard.min(1024), "{current} of {hard}");
    }
    #[test]
    fn one_device_cannot_take_every_slot() {
        let limits = Limits::new(MIN_CONNECTIONS);
        let lan: IpAddr = "192.168.1.20".parse().unwrap();
        let held: Vec<_> = std::iter::from_fn(|| limits.acquire(lan))
            .take(100)
            .collect();
        assert_eq!(held.len(), 4);
        // Other devices, and this computer, still get slots.
        let other = limits.acquire("192.168.1.21".parse().unwrap());
        assert!(other.is_some());
        let local: Vec<_> = std::iter::from_fn(|| limits.acquire(IpAddr::from([127, 0, 0, 1])))
            .take(100)
            .collect();
        assert_eq!(local.len(), MIN_CONNECTIONS - 5);
        drop(local);
        // An IPv6 device is one /64, whatever address it picks.
        let a = limits.acquire("2001:db8::1".parse().unwrap());
        let b: Vec<_> = std::iter::from_fn(|| limits.acquire("2001:db8::2:3".parse().unwrap()))
            .take(10)
            .collect();
        assert!(a.is_some());
        assert_eq!(b.len(), 3);
        drop(held);
        assert!(limits.acquire(lan).is_some());
        drop((a, b));
        assert_eq!(
            limits.clients.open.lock().unwrap().len(),
            1,
            "released devices are forgotten"
        );
    }
    // Real time, scaled down: tokio's paused clock needs its test-util feature.
    const TICK: Duration = Duration::from_millis(100);
    #[tokio::test]
    async fn idle_connections_time_out_but_busy_ones_do_not() {
        // No request at all: closed after the first-request deadline.
        let silent = Activity::new();
        let start = tokio::time::Instant::now();
        silent.idle(TICK, None).await;
        assert!(start.elapsed() >= TICK);
        // Another device: a request in flight (an event stream) keeps the
        // connection busy; once it ends, the idle limit applies.
        let lan = Activity::new();
        let request = lan.begin();
        let busy = tokio::time::timeout(TICK * 5, lan.idle(TICK, Some(TICK * 2))).await;
        assert!(busy.is_err());
        drop(request);
        let start = tokio::time::Instant::now();
        lan.idle(TICK, Some(TICK * 2)).await;
        assert!(start.elapsed() >= TICK * 2);
        // This computer: after the first request, never closed for idling.
        let local = Activity::new();
        drop(local.begin());
        let kept = tokio::time::timeout(TICK * 5, local.idle(TICK, None)).await;
        assert!(kept.is_err());
    }
    #[tokio::test]
    async fn request_bodies_from_other_devices_cannot_stall() {
        let (sender, rx) = tokio::sync::mpsc::channel::<Bytes>(1);
        let frames = futures_util::stream::unfold(rx, |mut rx| async move {
            let data = rx.recv().await?;
            Some((Ok::<_, Infallible>(hyper::body::Frame::data(data)), rx))
        });
        let body = http_body_util::StreamBody::new(Box::pin(frames));
        let mut body = ReadTimeout::new(body, TICK * 3);
        sender.send(Bytes::from_static(b"a")).await.unwrap();
        let frame = body.frame().await.unwrap().unwrap();
        assert_eq!(frame.into_data().unwrap(), "a");
        // Data that keeps arriving within the limit is fine...
        let late = sender.clone();
        tokio::spawn(async move {
            tokio::time::sleep(TICK).await;
            late.send(Bytes::from_static(b"b")).await.unwrap();
        });
        assert!(body.frame().await.unwrap().is_ok());
        // ...a pause longer than it is not.
        let start = tokio::time::Instant::now();
        let e = body.frame().await.unwrap().unwrap_err();
        assert!(e.is::<BodyReadTimeout>());
        assert!(start.elapsed() >= TICK * 3);
        drop(sender);
    }
    #[tokio::test]
    async fn a_failing_helper_is_restarted() {
        let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = runs.clone();
        let (running, mut third) = tokio::sync::mpsc::channel::<()>(1);
        let supervisor = tokio::spawn(supervise(
            "test helper",
            move || {
                let n = counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let running = running.clone();
                async move {
                    match n {
                        0 => anyhow::bail!("mDNS daemon failed"),
                        1 => panic!("helper panicked"),
                        _ => {
                            let _ = running.send(()).await;
                            std::future::pending().await
                        }
                    }
                }
            },
            Duration::from_millis(10),
            Duration::from_secs(60),
        ));
        tokio::time::timeout(Duration::from_secs(10), third.recv())
            .await
            .expect("helper restarted after an error and a panic");
        assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 3);
        assert!(!supervisor.is_finished());
        supervisor.abort();
    }
    async fn failure_for(port: u16) -> hyper_util::client::legacy::Error {
        let client = Client::builder(TokioExecutor::new()).build_http::<Body>();
        let req = Request::builder()
            .uri(format!("http://127.0.0.1:{port}/"))
            .body(empty())
            .unwrap();
        // Windows takes about two seconds to report a refused connection.
        tokio::time::timeout(Duration::from_secs(20), client.request(req))
            .await
            .unwrap()
            .unwrap_err()
    }
    #[tokio::test]
    async fn only_a_local_app_that_is_not_listening_gets_the_waiting_page() {
        // A private port nothing is bound to: connections are refused.
        let refused = crate::test_support::free_port();
        let e = failure_for(refused).await;
        assert_eq!(upstream_failure(&e, true, false).1, "upstream_unavailable");
        let (code, kind, _) = upstream_failure(&e, false, false);
        assert_eq!((code, kind), (StatusCode::BAD_GATEWAY, "upstream_error"));
        // An app that drops the connection mid-request: no auto-reload.
        let app = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = app.local_addr().unwrap().port();
        let crash = tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let (mut s, _) = app.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf).await;
        });
        let e = failure_for(port).await;
        crash.await.unwrap();
        let (code, kind, message) = upstream_failure(&e, true, false);
        assert_eq!((code, kind), (StatusCode::BAD_GATEWAY, "upstream_error"));
        assert!(message.contains("closed the connection"), "{message}");
        let page = error(code, kind, message, "app.localhost", true);
        let html = page.into_body().collect().await.unwrap().to_bytes();
        let html = String::from_utf8_lossy(&html);
        assert!(!html.contains("http-equiv=\"refresh\""), "{html}");
        let waiting = error(
            StatusCode::BAD_GATEWAY,
            "upstream_unavailable",
            "x",
            "app.localhost",
            true,
        );
        let html = waiting.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&html).contains("http-equiv=\"refresh\""));
    }
    #[test]
    fn ipv6_fallback_accepts_only_unavailable_address_family_errors() {
        let ipv6 = "[::1]:443".parse().unwrap();
        let ipv4 = "127.0.0.1:443".parse().unwrap();
        #[cfg(unix)]
        let unavailable = [libc::EAFNOSUPPORT, libc::EADDRNOTAVAIL];
        #[cfg(windows)]
        let unavailable = [10047, 10049];
        for code in unavailable {
            let error = anyhow::Error::new(std::io::Error::from_raw_os_error(code))
                .context("binding proxy");
            assert!(unavailable_ipv6(ipv6, &error));
            assert!(!unavailable_ipv6(ipv4, &error));
        }
        for kind in [
            std::io::ErrorKind::AddrInUse,
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::InvalidInput,
        ] {
            assert!(!unavailable_ipv6(
                ipv6,
                &anyhow::Error::new(std::io::Error::from(kind))
            ));
        }
    }
    #[tokio::test]
    async fn ipv6_bind_conflicts_are_not_hidden_by_fallback() {
        let hold = match listener("[::1]:0".parse().unwrap()) {
            Ok(listener) => listener,
            Err(error) if unavailable_ipv6("[::1]:0".parse().unwrap(), &error) => return,
            Err(error) => panic!("{error:#}"),
        };
        let error = listen_if_available(hold.local_addr().unwrap(), true).unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::AddrInUse
        );
    }
    #[test]
    fn connection_headers() {
        let mut h = header::HeaderMap::new();
        h.insert("connection", "x-private, keep-alive".parse().unwrap());
        h.insert("x-private", "secret".parse().unwrap());
        h.insert("content-type", "text/event-stream".parse().unwrap());
        strip(&mut h);
        assert!(!h.contains_key("x-private"));
        assert!(h.contains_key("content-type"));
    }
    #[test]
    fn html_escapes_untrusted_values() {
        let r = error(
            StatusCode::NOT_FOUND,
            "not_found",
            "<script>",
            "evil\"host",
            true,
        );
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        assert_eq!(escape("<script>\"'&"), "&lt;script&gt;&quot;&#39;&amp;");
    }
}
