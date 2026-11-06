//! Streaming HTTP/1.1, HTTP/2 and WebSocket reverse proxy.
use crate::state::ProxySettings;
use anyhow::{Context, Result};
use bytes::Bytes;
use http_body_util::{combinators::UnsyncBoxBody, BodyExt, Full};
use hyper::{body::Incoming, header, Method, Request, Response, StatusCode, Version};
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::{TokioExecutor, TokioIo},
    server::conn::auto::Builder,
};
use std::{convert::Infallible, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
type Body = UnsyncBoxBody<Bytes, hyper::Error>;
type Upstream = Client<hyper_rustls::HttpsConnector<HttpConnector>, Body>;
#[derive(Clone)]
struct ProxyContext {
    client: Upstream,
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
fn error(code: StatusCode, kind: &str, message: &str, host: &str, html: bool) -> Response<Body> {
    let body = if html {
        format!("<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\"><title>{} · Porch</title><style>body{{font:16px system-ui;background:#faf8f2;color:#242421;max-width:52rem;margin:12vh auto;padding:2rem}}h1{{font-size:2rem}}code{{background:#eee9df;padding:.2rem}}</style><h1>{} {}</h1><p>{}</p><p>Host: <code>{}</code></p><p>Run <code>porch list</code> to inspect registered apps.</p></html>", code.as_u16(),code.as_u16(),escape(code.canonical_reason().unwrap_or("Proxy error")), escape(message),escape(host))
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
                StatusCode::FORBIDDEN,
                "hosts_sync_failed",
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
    let hops = req
        .headers()
        .get("x-portless-hops")
        .or_else(|| req.headers().get("x-porch-hops"))
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(0);
    if hops >= 5 {
        return Ok(error(StatusCode::LOOP_DETECTED,"loop_detected","Request passed through Porch too many times. Add changeOrigin: true to your dev server proxy config.",&host,html));
    }
    let state = match crate::state::load() {
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
        .filter(|r| crate::state::route_alive(&state, r))
        .filter_map(|r| score(&r.name, &hostname, req.uri().path(), &settings).map(|s| (s, r)))
        .max_by_key(|(s, _)| *s)
        .map(|(_, r)| r)
        .or_else(|| {
            let requested_port = host
                .parse::<hyper::http::uri::Authority>()
                .ok()
                .and_then(|a| a.port_u16())
                .unwrap_or(443);
            state
                .routes
                .iter()
                .rev()
                .filter(|r| crate::state::route_alive(&state, r))
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
    let Some(route) = route else {
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
        let mut response = error(
            StatusCode::NOT_FOUND,
            "not_found",
            "No app is registered for this hostname. Start it with porch <name> <command>.",
            &host,
            html,
        );
        if html {
            let links = state
                .routes
                .iter()
                .filter(|r| crate::state::route_alive(&state, r))
                .map(|r| {
                    format!(
                        "<li><a href=\"{}\">{}</a> → <code>{}</code></li>",
                        escape(&settings.url(&r.name)),
                        escape(&settings.hostname(&r.name)),
                        escape(&r.target)
                    )
                })
                .collect::<Vec<_>>()
                .join("");
            let page=format!("<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><title>404 · Porch</title><style>body{{font:16px system-ui;max-width:52rem;margin:10vh auto;padding:2rem;background:#faf8f2;color:#242421}}li{{margin:1rem 0}}</style><h1>404 Not Found</h1><p>No app registered for <strong>{}</strong>.</p><h2>Active apps</h2><ul>{}</ul><p>Start an app with <code>porch &lt;name&gt; &lt;command&gt;</code>.</p></html>",escape(&hostname),links);
            *response.body_mut() = Full::new(Bytes::from(page))
                .map_err(|e: Infallible| match e {})
                .boxed_unsync();
        }
        return Ok(response);
    };
    let target = if route.target.contains("://") {
        route.target.clone()
    } else {
        format!("http://{}", route.target)
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
    let target_authority = if target.scheme_str() == Some("http")
        && target
            .host()
            .is_some_and(|h| matches!(h, "127.0.0.1" | "[::1]" | "::1"))
    {
        format!("localhost:{}", target.port_u16().unwrap_or(80))
    } else {
        target.authority().unwrap().to_string()
    };
    let uri = format!(
        "{}://{}{}{}",
        target.scheme_str().unwrap_or("http"),
        target_authority,
        target.path().trim_end_matches('/'),
        path
    );
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
        ("x-portless-hops", (hops + 1).to_string()),
    ] {
        if (name == "x-forwarded-for"
            || name == "x-portless-hops"
            || !req.headers().contains_key(name))
            && value.parse::<header::HeaderValue>().is_ok()
        {
            req.headers_mut().insert(name, value.parse().unwrap());
        }
    }
    if let Ok(v) = host.parse() {
        req.headers_mut().insert(header::HOST, v);
    }
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
    match client.request(req).await {
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
                    let upstream = hyper::upgrade::on(&mut response);
                    tokio::spawn(async move {
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
            }
            let mut r = response.map(|b| b.boxed_unsync());
            marker(&mut r);
            Ok(r)
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
    let domain = if addr.is_ipv6() {
        socket2::Domain::IPV6
    } else {
        socket2::Domain::IPV4
    };
    let socket = socket2::Socket::new(domain, socket2::Type::STREAM, Some(socket2::Protocol::TCP))?;
    socket.set_reuse_address(true)?;
    if addr.is_ipv6() {
        socket.set_only_v6(true)?;
    }
    socket.set_nonblocking(true)?;
    socket
        .bind(&addr.into())
        .with_context(|| format!("binding proxy {addr}"))?;
    socket.listen(1024)?;
    Ok(tokio::net::TcpListener::from_std(socket.into())?)
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
        let (stream, peer) = tokio::select! { result=listener.accept()=>result?, _=connections.join_next(), if !connections.is_empty()=>continue };
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
            builder.http2().enable_connect_protocol();
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
    let acceptor = if settings.tls {
        Some(tokio_rustls::TlsAcceptor::from(crate::tls::config(
            settings.cert.clone(),
            settings.key.clone(),
        )?))
    } else {
        None
    };
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    if let Ok(ca) = std::fs::read(crate::state::dir().join("ca.pem")) {
        for cert in rustls_pemfile::certs(&mut std::io::BufReader::new(ca.as_slice())).flatten() {
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
        .with_tls_config(tls_config)
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .wrap_connector(http);
    let client = Client::builder(TokioExecutor::new()).build::<_, Body>(connector);
    let binds = if let Some(bind) = &settings.bind {
        vec![bind.clone()]
    } else if settings.lan || settings.unprivileged {
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
    let token = Arc::new(crate::integrations::hosts_sync_token()?);
    let settings = Arc::new(settings);
    let mut tasks = tokio::task::JoinSet::new();
    let (_shutdown, stopped) = tokio::sync::watch::channel(false);
    let context = ProxyContext {
        client,
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
