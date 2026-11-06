//! Streaming reverse proxy. Routes are reloaded for every request.
use anyhow::Result;
use bytes::Bytes;
use http_body_util::{combinators::UnsyncBoxBody, BodyExt, Full};
use hyper::{body::Incoming, header, Request, Response, StatusCode};
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::{TokioExecutor, TokioIo},
    server::conn::auto::Builder,
};
use std::{convert::Infallible, net::SocketAddr, path::PathBuf};
type Body = UnsyncBoxBody<Bytes, hyper::Error>;
fn error(code: StatusCode, message: &str) -> Response<Body> {
    let mut response = Response::new(
        Full::new(Bytes::copy_from_slice(message.as_bytes()))
            .map_err(|e: Infallible| match e {})
            .boxed_unsync(),
    );
    *response.status_mut() = code;
    response
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
fn score(name: &str, host: &str, path: &str) -> Option<(usize, usize)> {
    let (domain, prefix) = name
        .split_once('/')
        .map(|(h, p)| (h, format!("/{p}")))
        .unwrap_or((name, String::new()));
    let domain = domain.trim_end_matches('.').to_ascii_lowercase();
    let exact = domain == host;
    let wildcard = domain.strip_prefix("*.").is_some_and(|suffix| {
        host.ends_with(&format!(".{suffix}")) && host.len() > suffix.len() + 1
    });
    let short = !domain.contains('.') && host == format!("{domain}.localhost");
    if !(exact || wildcard || short)
        || !(prefix.is_empty()
            || path == prefix
            || path
                .strip_prefix(&prefix)
                .is_some_and(|s| s.starts_with('/')))
    {
        return None;
    }
    Some((if wildcard { 0 } else { 1 }, prefix.len()))
}
async fn forward(
    mut req: Request<Incoming>,
    client: Client<hyper_rustls::HttpsConnector<HttpConnector>, Incoming>,
    peer: SocketAddr,
    secure: bool,
) -> Result<Response<Body>, Infallible> {
    let host = req
        .uri()
        .authority()
        .map(|a| a.as_str())
        .or_else(|| {
            req.headers()
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
        })
        .unwrap_or("")
        .to_owned();
    let hostname = host
        .parse::<hyper::http::uri::Authority>()
        .map(|a| a.host().trim_end_matches('.').to_ascii_lowercase())
        .unwrap_or_default();
    let state = match crate::state::load() {
        Ok(s) => s,
        Err(_) => {
            return Ok(error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Cannot load routes",
            ))
        }
    };
    let route = state
        .routes
        .iter()
        .filter_map(|r| score(&r.name, &hostname, req.uri().path()).map(|s| (s, r)))
        .max_by_key(|(s, _)| *s)
        .map(|(_, r)| r);
    let Some(route) = route else {
        return Ok(error(
            StatusCode::NOT_FOUND,
            "No route for this host and path",
        ));
    };
    let target = if route.target.contains("://") {
        route.target.clone()
    } else {
        format!("http://{}", route.target)
    };
    let target = match target.parse::<hyper::Uri>() {
        Ok(u) if u.authority().is_some() => u,
        _ => return Ok(error(StatusCode::BAD_GATEWAY, "Invalid route target")),
    };
    let path = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let base = target.path().trim_end_matches('/');
    let uri = format!(
        "{}://{}{}{}",
        target.scheme_str().unwrap_or("http"),
        target.authority().unwrap(),
        base,
        path
    );
    let Ok(uri) = uri.parse() else {
        return Ok(error(StatusCode::BAD_GATEWAY, "Invalid upstream URI"));
    };
    let upgrade = req.headers().get(header::UPGRADE).cloned().filter(|_| {
        req.headers()
            .get(header::CONNECTION)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|s| {
                s.split(',')
                    .any(|v| v.trim().eq_ignore_ascii_case("upgrade"))
            })
    });
    let downstream = upgrade.as_ref().map(|_| hyper::upgrade::on(&mut req));
    strip(req.headers_mut());
    if let Some(value) = upgrade {
        req.headers_mut().insert(header::UPGRADE, value);
        req.headers_mut().insert(
            header::CONNECTION,
            header::HeaderValue::from_static("upgrade"),
        );
    }
    for (name, value) in [
        ("x-forwarded-for", peer.ip().to_string()),
        ("x-forwarded-host", host),
        (
            "x-forwarded-proto",
            if secure { "https" } else { "http" }.to_owned(),
        ),
    ] {
        if let Ok(value) = value.parse() {
            req.headers_mut().insert(name, value);
        }
    }
    *req.uri_mut() = uri;
    *req.version_mut() = hyper::Version::HTTP_11;
    match client.request(req).await {
        Ok(mut response) => {
            if response.status() == StatusCode::SWITCHING_PROTOCOLS {
                if let Some(downstream) = downstream {
                    let upstream = hyper::upgrade::on(&mut response);
                    tokio::spawn(async move {
                        if let Ok((a, b)) = tokio::try_join!(downstream, upstream) {
                            let mut a = TokioIo::new(a);
                            let mut b = TokioIo::new(b);
                            let _ = tokio::io::copy_bidirectional(&mut a, &mut b).await;
                        }
                    });
                }
            } else {
                strip(response.headers_mut());
            }
            Ok(response.map(|b| b.boxed_unsync()))
        }
        Err(_) => Ok(error(StatusCode::BAD_GATEWAY, "Upstream connection failed")),
    }
}
pub async fn serve(
    bind: String,
    port: u16,
    tls: bool,
    cert: Option<PathBuf>,
    key: Option<PathBuf>,
) -> Result<()> {
    let connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_native_roots()?
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .build();
    let client = Client::builder(TokioExecutor::new()).build::<_, Incoming>(connector);
    let acceptor = if tls {
        Some(tokio_rustls::TlsAcceptor::from(crate::tls::config(
            cert, key,
        )?))
    } else {
        None
    };
    let addr: SocketAddr = format!(
        "{}:{}",
        if bind.contains(':') {
            format!("[{bind}]")
        } else {
            bind
        },
        port
    )
    .parse()?;
    let socket = if addr.is_ipv6() {
        let s = socket2::Socket::new(
            socket2::Domain::IPV6,
            socket2::Type::STREAM,
            Some(socket2::Protocol::TCP),
        )?;
        s.set_only_v6(true)?;
        s.set_nonblocking(true)?;
        tokio::net::TcpSocket::from_std_stream(s.into())
    } else {
        tokio::net::TcpSocket::new_v4()?
    };
    socket.bind(addr)?;
    let listener = socket.listen(1024)?;
    loop {
        let (stream, peer) = listener.accept().await?;
        let client = client.clone();
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            let service =
                hyper::service::service_fn(move |req| forward(req, client.clone(), peer, tls));
            if let Some(acceptor) = acceptor {
                if let Ok(stream) = acceptor.accept(stream).await {
                    let _ = Builder::new(TokioExecutor::new())
                        .serve_connection_with_upgrades(TokioIo::new(stream), service)
                        .await;
                }
            } else {
                let _ = Builder::new(TokioExecutor::new())
                    .serve_connection_with_upgrades(TokioIo::new(stream), service)
                    .await;
            }
        });
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn routing() {
        assert!(score("app", "app.localhost", "/").is_some());
        assert!(score("*.example.test", "x.example.test", "/").is_some());
        assert!(score("*.example.test", "example.test", "/").is_none());
        assert!(score("app.localhost/api", "app.localhost", "/api/v1").is_some());
        assert!(score("app.localhost/api", "app.localhost", "/apix").is_none());
        assert!(
            score("app.localhost", "app.localhost", "/")
                > score("*.localhost", "app.localhost", "/")
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
}
