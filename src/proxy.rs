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
use std::{convert::Infallible, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
type Body = UnsyncBoxBody<Bytes, hyper::Error>;
/// Open client connections the proxy serves at once; more are refused.
const MAX_CONNECTIONS: usize = 4096;
/// Time a client gets to send a complete HTTP/1 request head.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);
type Upstream = Client<hyper_rustls::HttpsConnector<HttpConnector>, Body>;
type LoopbackUpstream = crate::upstream::LoopbackClient<Body>;
#[derive(Clone)]
struct ProxyContext {
    client: Upstream,
    /// Dials every name on loopback, for `--upstream` aliases: the URI host is
    /// the TLS server name, the connection still goes to this computer.
    loopback: LoopbackUpstream,
    /// The same, without certificate verification (`--insecure` aliases only).
    loopback_insecure: LoopbackUpstream,
    settings: Arc<ProxySettings>,
    token: Arc<String>,
    stopped: tokio::sync::watch::Receiver<bool>,
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
async fn dashboard_json(settings: &ProxySettings, peers: bool) -> serde_json::Value {
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
    if peers {
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
        out["machines"] = serde_json::json!(machines);
    }
    out
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
/// Score includes the matched TLD length so overlapping suffixes are unambiguous.
fn score(
    name: &str,
    host: &str,
    path: &str,
    settings: &ProxySettings,
) -> Option<(usize, usize, usize, usize)> {
    let (domain, prefix) = name
        .split_once('/')
        .map(|(h, p)| (h, format!("/{p}")))
        .unwrap_or((name, String::new()));
    if !(prefix.is_empty()
        || path == prefix
        || path
            .strip_prefix(&prefix)
            .is_some_and(|s| s.starts_with('/')))
    {
        return None;
    }
    let domain = domain.trim_end_matches('.').to_ascii_lowercase();
    let mut tlds = settings.tlds.clone();
    if settings.lan {
        tlds.push("local".into());
    }
    tlds.sort_by_key(|t| std::cmp::Reverse(t.len()));
    let matched_tld = tlds.iter().find(|t| host.ends_with(&format!(".{t}")));
    let explicit = tlds.iter().any(|t| domain.ends_with(&format!(".{t}")));
    let candidates = if explicit || domain.starts_with("*.") {
        vec![domain.clone()]
    } else {
        tlds.iter()
            .filter(|t| matched_tld.is_none_or(|m| m == *t))
            .map(|t| format!("{domain}.{t}"))
            .collect()
    };
    candidates
        .into_iter()
        .filter_map(|candidate| {
            let wildcard = candidate.strip_prefix("*.");
            let exact = candidate == host;
            let parent = wildcard.unwrap_or(&candidate);
            let fallback =
                (wildcard.is_some() || settings.wildcard) && host.ends_with(&format!(".{parent}"));
            (exact || fallback).then_some((
                usize::from(exact),
                matched_tld.map_or(0, |t| t.len()),
                parent.len(),
                prefix.len(),
            ))
        })
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
fn hosts_request(
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
    Some(
        match crate::integrations::authenticated_hosts_sync(supplied) {
            Ok(()) => {
                let mut r = Response::new(empty());
                marker(&mut r);
                r
            }
            Err(e) => error(
                if e.downcast_ref::<crate::integrations::InvalidHostsToken>()
                    .is_some()
                {
                    StatusCode::FORBIDDEN
                } else {
                    StatusCode::CONFLICT
                },
                if e.downcast_ref::<crate::integrations::InvalidHostsToken>()
                    .is_some()
                {
                    "forbidden"
                } else {
                    "hosts_sync_unavailable"
                },
                &e.to_string(),
                host,
                false,
            ),
        },
    )
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
        token,
        mut stopped,
    } = context;
    let host = authority(&req);
    let html = req
        .headers()
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().contains("text/html"));
    if let Some(r) = hosts_request(&req, peer, &host, &token, settings.unprivileged) {
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
        if !peer.ip().is_loopback() {
            return Ok(error(
                StatusCode::FORBIDDEN,
                "forbidden",
                "The app list is only available on this computer",
                &host,
                false,
            ));
        }
        let peers = req
            .uri()
            .query()
            .is_some_and(|q| q.split('&').any(|p| p == "peers=1"));
        let body = dashboard_json(&settings, peers).await.to_string();
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
    if redirect_plain && upgrade.is_none() {
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
    // Only a proxy on this machine can have forwarded this request before; a
    // LAN client's hop count is ignored so it can't force a 508.
    let hops = req
        .headers()
        .get("x-porch-hops")
        .or_else(|| req.headers().get("x-portless-hops"))
        .filter(|_| peer.ip().is_loopback())
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(0);
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
        .filter_map(|r| score(&r.name, &hostname, req.uri().path(), &settings).map(|s| (s, r)))
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
            let body = dashboard_json(&settings, false).await.to_string();
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
    // Dial the address the route records. Only for an app Porch launched on
    // 127.0.0.1 that turned out to listen on ::1 alone does the proxy use ::1
    // (an alias never moves: another user could be listening there).
    let launched = route.is_some_and(|r| r.pid.is_some());
    let target_authority = match (target.host(), target.port_u16()) {
        (Some("127.0.0.1"), Some(port))
            if launched && target.scheme_str() == Some("http") && ipv6_only(port) =>
        {
            format!("[::1]:{port}")
        }
        _ => target.authority().unwrap().to_string(),
    };
    let uri = format!(
        "{}://{}{}{}",
        target.scheme_str().unwrap_or("http"),
        target_authority,
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
    // forwarding headers. Anyone else on the LAN could otherwise spoof the
    // scheme or host the app sees.
    let trust_forwarded = peer.ip().is_loopback();
    if !trust_forwarded {
        for name in [
            "x-forwarded-for",
            "x-forwarded-host",
            "x-forwarded-proto",
            "x-forwarded-port",
            "forwarded",
        ] {
            req.headers_mut().remove(name);
        }
    }
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
        } else {
            body.boxed_unsync()
        },
    );
    let sent = match &upstream_spec {
        Some(up) if up.insecure => loopback_insecure.request(req).await,
        Some(_) => loopback.request(req).await,
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
                    // count against their own cap for their whole lifetime.
                    let Ok(permit) = upgrade_slots().clone().try_acquire_owned() else {
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
                        let _permit = permit;
                        if let Ok((a, b)) = tokio::try_join!(downstream, upstream) {
                            let mut a = TokioIo::new(a);
                            let mut b = TokioIo::new(b);
                            tokio::select! { _ = tokio::io::copy_bidirectional(&mut a, &mut b) => {}, _ = stopped.changed() => {} };
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
                // An alias to `http://host/base` serves the backend's `/base` at
                // the Porch URL's root: drop that prefix from its redirects.
                let base = target.path().trim_end_matches('/').to_owned();
                let rewritten = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|l| {
                        let absolute = crate::upstream::rewrite_location(l, &origins, &public_origin);
                        let loc = absolute.clone().unwrap_or_else(|| l.to_owned());
                        let (prefix, path) = if let Some(p) = loc.strip_prefix(&public_origin) {
                            (public_origin.as_str(), p)
                        } else if loc.starts_with('/') && !loc.starts_with("//") {
                            ("", loc.as_str())
                        } else {
                            return absolute;
                        };
                        match path.strip_prefix(base.as_str()) {
                            Some(rest)
                                if !base.is_empty()
                                    && (rest.is_empty() || rest.starts_with(['/', '?', '#'])) =>
                            {
                                let rest = if rest.starts_with('/') { rest.to_owned() } else { format!("/{rest}") };
                                Some(format!("{prefix}{rest}"))
                            }
                            _ => absolute,
                        }
                    })
                    .and_then(|l| header::HeaderValue::from_str(&l).ok());
                if let Some(v) = rewritten {
                    response.headers_mut().insert(header::LOCATION, v);
                }
            }
            let mut r = response.map(|b| b.boxed_unsync());
            marker(&mut r);
            Ok(r)
        }
        Err(e) if upstream_spec.as_ref().is_some_and(|u| u.tls) && certificate_error(&e) =>
        {
            Ok(error(
                StatusCode::BAD_GATEWAY,
                "upstream_certificate",
                "The app's TLS certificate isn't trusted for this name. Trust its CA, or re-register the alias with --insecure.",
                &host,
                html,
            ))
        }
        Err(_) => Ok(error(
            StatusCode::BAD_GATEWAY,
            "upstream_unavailable",
            "The target app is not accepting connections. Check its output and PORT setting.",
            &host,
            html,
        )),
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
async fn listen(
    listener: tokio::net::TcpListener,
    context: ProxyContext,
    acceptor: Option<tokio_rustls::TlsAcceptor>,
    redirect_only: bool,
) -> Result<()> {
    let mut connections = tokio::task::JoinSet::new();
    loop {
        let accepted = tokio::select! { result=listener.accept()=>result, _=connections.join_next(), if !connections.is_empty()=>continue };
        let (stream, peer) = match accepted {
            Ok(connection) => connection,
            Err(e) => {
                eprintln!("Porch accept failed: {e}; retrying");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        // Bound the work a flood of (or a LAN full of) idle clients can pin.
        if connections.len() >= MAX_CONNECTIONS {
            drop(stream);
            continue;
        }
        let context = context.clone();
        let acceptor = acceptor.clone();
        connections.spawn(async move {
            // Wildcard low-port binding in unprivileged mode still rejects non-loopback peers.
            if !context.settings.lan && !peer.ip().is_loopback() {
                return;
            }
            let mut first = [0u8; 1];
            let tls = if acceptor.is_some() {
                matches!(tokio::time::timeout(Duration::from_secs(10),stream.peek(&mut first)).await,Ok(Ok(n)) if n>0 && first[0]==0x16)
            } else {
                false
            };
            let redirect_plain = redirect_only || (acceptor.is_some() && !tls);
            let service = hyper::service::service_fn(move |req| {
                forward(req,peer,tls,redirect_plain,context.clone())
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
            if tls {
                if let Some(acceptor) = acceptor {
                    if let Ok(Ok(stream)) =
                        tokio::time::timeout(Duration::from_secs(10), acceptor.accept(stream)).await
                    {
                        let _ = builder
                            .serve_connection_with_upgrades(TokioIo::new(stream), service)
                            .await;
                    }
                }
            } else {
                let _ = builder
                    .serve_connection_with_upgrades(TokioIo::new(stream), service)
                    .await;
            }
        });
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
pub async fn serve_config(settings: ProxySettings) -> Result<()> {
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
    let binds = if let Some(bind) = &settings.bind {
        vec![bind.clone()]
    } else if settings.lan
        || (settings.unprivileged && settings.port < crate::ports::FIRST_UNPRIVILEGED_PORT)
    {
        // Wildcard binds permit unprivileged low ports on macOS; high ports stay on loopback.
        vec!["0.0.0.0".into(), "::".into()]
    } else {
        vec!["127.0.0.1".into(), "::1".into()]
    };
    let mut listeners = Vec::new();
    for bind in &binds {
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
    crate::state::atomic_write(
        "proxy.hosts-capability",
        &serde_json::to_vec(&serde_json::json!({
            "pid": std::process::id(), "writable": crate::integrations::hosts_sync_capable()
        }))?,
    )?;
    let token = Arc::new(crate::integrations::hosts_sync_token()?);
    let settings = Arc::new(settings);
    let mut tasks = tokio::task::JoinSet::new();
    let (_shutdown, stopped) = tokio::sync::watch::channel(false);
    let context = ProxyContext {
        client,
        loopback,
        loopback_insecure,
        settings: settings.clone(),
        token,
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
    if settings.lan {
        let settings = (*settings).clone();
        tasks.spawn(crate::integrations::lan_watch(settings));
    }
    match tasks.join_next().await {
        Some(result) => result??,
        None => anyhow::bail!("proxy has no listeners"),
    };
    Ok(())
}
/// Open upgraded (WebSocket) connections across all listeners.
fn upgrade_slots() -> &'static Arc<tokio::sync::Semaphore> {
    static SLOTS: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    SLOTS.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS)))
}
/// Is a launched app on `port` reachable on ::1 but not 127.0.0.1? Probed at
/// most every two seconds per port.
fn ipv6_only(port: u16) -> bool {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::Instant;
    static SEEN: Mutex<Option<HashMap<u16, (bool, Instant)>>> = Mutex::new(None);
    if let Ok(mut seen) = SEEN.lock() {
        if let Some((v6, at)) = seen.get_or_insert_with(HashMap::new).get(&port) {
            if at.elapsed() < Duration::from_secs(2) {
                return *v6;
            }
        }
    }
    let v6 = crate::ports::accepts_ipv6_only(port);
    if let Ok(mut seen) = SEEN.lock() {
        let map = seen.get_or_insert_with(HashMap::new);
        if map.len() > 4096 {
            map.clear();
        }
        map.insert(port, (v6, Instant::now()));
    }
    v6
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
    #[test]
    fn routing() {
        let mut s = ProxySettings {
            tlds: vec![
                "example.com".into(),
                "dev.example.com".into(),
                "localhost".into(),
            ],
            ..ProxySettings::default()
        };
        assert!(score("api.web", "api.web.localhost", "/", &s).is_some());
        assert!(score("app", "app.dev.example.com", "/", &s).is_some());
        assert!(score("app.dev", "app.dev.example.com", "/", &s).is_none());
        assert!(score("app.localhost", "app.example.com", "/", &s).is_none());
        assert!(score("*.example.test", "x.example.test", "/", &s).is_some());
        assert!(score("app", "x.app.localhost", "/", &s).is_none());
        s.wildcard = true;
        assert!(score("app", "x.app.localhost", "/", &s).is_some());
        assert!(
            score("api.app", "x.api.app.localhost", "/", &s)
                > score("app", "x.api.app.localhost", "/", &s)
        );
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
