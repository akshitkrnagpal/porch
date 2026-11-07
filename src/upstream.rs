//! `--upstream` aliases: dial this computer's loopback port while speaking the
//! alias's own scheme, Host and TLS server name. Shared by the local proxy and
//! the paired-machine relay, so both reach the backend the same way.
use crate::state::Upstream;
use hyper::Uri;
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::TokioExecutor,
};
use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

/// A connector that always dials 127.0.0.1 on the URI's port, whatever host
/// the URI names (a DNS name or an IP literal). The TLS layer above it still
/// sees the original URI, so the server name and certificate check follow the
/// alias, while the connection can never leave this computer.
#[derive(Clone)]
pub struct ToLoopback(HttpConnector);

impl tower_service::Service<Uri> for ToLoopback {
    type Response = <HttpConnector as tower_service::Service<Uri>>::Response;
    type Error = <HttpConnector as tower_service::Service<Uri>>::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }
    fn call(&mut self, uri: Uri) -> Self::Future {
        let https = uri.scheme_str() == Some("https");
        let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });
        // Plain TCP to loopback, IPv4 first and then IPv6 (an app may listen
        // on only one); the TLS layer wraps this stream itself.
        let v4: Uri = format!("http://127.0.0.1:{port}")
            .parse()
            .expect("loopback URI");
        let v6: Uri = format!("http://[::1]:{port}")
            .parse()
            .expect("loopback URI");
        let mut connector = self.0.clone();
        Box::pin(async move {
            match connector.call(v4).await {
                Ok(stream) => Ok(stream),
                Err(_) => connector.call(v6).await,
            }
        })
    }
}

pub type LoopbackClient<B> = Client<hyper_rustls::HttpsConnector<ToLoopback>, B>;

/// A client for `--upstream` aliases, with the given TLS policy.
pub fn client<B>(tls: rustls::ClientConfig) -> LoopbackClient<B>
where
    B: hyper::body::Body + Send + 'static + Unpin,
    B::Data: Send,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    http.set_connect_timeout(Some(Duration::from_secs(10)));
    let connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(tls)
        .https_or_http()
        .enable_http1()
        .wrap_connector(ToLoopback(http));
    Client::builder(TokioExecutor::new()).build(connector)
}

/// TLS that accepts any backend certificate, for `--insecure` aliases only.
/// Their connections still go only to loopback (see [`ToLoopback`]).
pub fn insecure_tls() -> anyhow::Result<rustls::ClientConfig> {
    Ok(rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(NoVerify))
    .with_no_client_auth())
}

/// The request URI and Host header for an `--upstream` alias on `port`.
/// The URI host is the TLS server name; [`ToLoopback`] decides where it dials.
pub fn request(up: &Upstream, port: u16, path_and_query: &str) -> (String, Option<String>) {
    let name = up
        .authority
        .as_deref()
        .and_then(|a| a.parse::<hyper::http::uri::Authority>().ok())
        .map(|a| a.host().to_owned())
        .unwrap_or_else(|| "127.0.0.1".to_owned());
    let path = if path_and_query.starts_with('/') {
        path_and_query.to_owned()
    } else {
        format!("/{path_and_query}")
    };
    (
        format!(
            "{}://{}:{port}{path}",
            if up.tls { "https" } else { "http" },
            name
        ),
        up.authority.clone(),
    )
}

/// The origins an `--upstream` backend may redirect to that belong to it:
/// its own scheme and authority, with and without the default port.
pub fn origins(up: &Upstream) -> Vec<String> {
    let Some(authority) = &up.authority else {
        return vec![];
    };
    let scheme = if up.tls { "https" } else { "http" };
    let mut out = vec![format!("{scheme}://{authority}")];
    let default = if up.tls { ":443" } else { ":80" };
    if let Some(bare) = authority.strip_suffix(default) {
        out.push(format!("{scheme}://{bare}"));
    } else if !authority.contains(':') || authority.ends_with(']') {
        out.push(format!("{scheme}://{authority}{default}"));
    }
    out
}

/// Rewrite a redirect that points at one of `origins` onto `public_origin`.
pub fn rewrite_location(location: &str, origins: &[String], public_origin: &str) -> Option<String> {
    origins.iter().find_map(|origin| {
        let rest = location
            .get(..origin.len())
            .filter(|p| p.eq_ignore_ascii_case(origin))
            .map(|_| &location[origin.len()..])?;
        (rest.is_empty() || rest.starts_with(['/', '?', '#']))
            .then(|| format!("{public_origin}{rest}"))
    })
}

/// Make a cookie the backend scoped to its own domain (`Domain=example.com`)
/// host-only, so the browser keeps it for the Porch name it's actually on.
/// Returns `None` when the cookie needs no change.
pub fn host_only_cookie(set_cookie: &str, backend_host: &str) -> Option<String> {
    let backend = backend_host.trim_matches(['[', ']']).to_ascii_lowercase();
    let mut changed = false;
    let parts: Vec<&str> = set_cookie
        .split(';')
        .filter(|part| {
            let Some((key, value)) = part.trim().split_once('=') else {
                return true;
            };
            if !key.trim().eq_ignore_ascii_case("domain") {
                return true;
            }
            let domain = value.trim().trim_start_matches('.').to_ascii_lowercase();
            let ours = backend == domain || backend.ends_with(&format!(".{domain}"));
            changed |= ours;
            !ours
        })
        .collect();
    changed.then(|| parts.join(";"))
}

#[derive(Debug)]
struct NoVerify;
impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn requests_and_redirect_origins() {
        let up = Upstream {
            tls: true,
            authority: Some("api.internal".into()),
            insecure: false,
        };
        assert_eq!(
            request(&up, 8443, "/x?y=1"),
            (
                "https://api.internal:8443/x?y=1".into(),
                Some("api.internal".into())
            )
        );
        let o = origins(&up);
        assert_eq!(
            rewrite_location("https://api.internal/login?a=1", &o, "http://api.localhost"),
            Some("http://api.localhost/login?a=1".into())
        );
        assert_eq!(
            rewrite_location("https://api.internal:443/", &o, "http://api.localhost"),
            Some("http://api.localhost/".into())
        );
        assert_eq!(
            rewrite_location("https://api.internal.evil/", &o, "http://x"),
            None
        );
        assert_eq!(rewrite_location("https://other/", &o, "http://x"), None);
        let keep = Upstream {
            tls: true,
            authority: None,
            insecure: true,
        };
        assert_eq!(request(&keep, 9, "/").0, "https://127.0.0.1:9/");
        assert!(origins(&keep).is_empty());
    }
    #[test]
    fn backend_scoped_cookies_become_host_only() {
        assert_eq!(
            host_only_cookie(
                "sid=1; Domain=.example.com; Path=/; Secure",
                "staging.example.com"
            ),
            Some("sid=1; Path=/; Secure".into())
        );
        assert_eq!(
            host_only_cookie("sid=1; domain=staging.example.com", "staging.example.com"),
            Some("sid=1".into())
        );
        assert_eq!(
            host_only_cookie("sid=1; Path=/", "staging.example.com"),
            None
        );
        // Another site's domain is left for the browser to reject as usual.
        assert_eq!(
            host_only_cookie("sid=1; Domain=other.com", "staging.example.com"),
            None
        );
    }

    #[tokio::test]
    async fn ipv6_only_backends_are_reached() {
        let Ok(listener) = tokio::net::TcpListener::bind("[::1]:0").await else {
            return; // no IPv6 loopback here
        };
        let port = listener.local_addr().unwrap().port();
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_err() {
            return; // something else holds the IPv4 side
        }
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf).await;
            let _ = s
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nv6")
                .await;
        });
        let up = Upstream {
            tls: false,
            authority: Some("backend.internal".into()),
            insecure: false,
        };
        let (uri, host) = request(&up, port, "/");
        let c: LoopbackClient<http_body_util::Empty<bytes::Bytes>> = client(
            rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth(),
        );
        let req = hyper::Request::builder()
            .uri(uri)
            .header("host", host.unwrap())
            .body(http_body_util::Empty::new())
            .unwrap();
        let resp = tokio::time::timeout(Duration::from_secs(5), c.request(req))
            .await
            .unwrap()
            .unwrap();
        use http_body_util::BodyExt;
        assert_eq!(
            &resp.into_body().collect().await.unwrap().to_bytes()[..],
            b"v6"
        );
    }
    #[tokio::test]
    async fn ip_literal_authority_still_dials_loopback() {
        // A backend on loopback answers; the alias names a non-loopback IP.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let n = s.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let body = req
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("host:"))
                .unwrap_or("")
                .to_owned();
            let _ = s
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await;
        });
        let up = Upstream {
            tls: false,
            authority: Some("203.0.113.7".into()),
            insecure: false,
        };
        let (uri, host) = request(&up, port, "/");
        let c: LoopbackClient<http_body_util::Empty<bytes::Bytes>> = client(
            rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth(),
        );
        let req = hyper::Request::builder()
            .uri(uri)
            .header("host", host.unwrap())
            .body(http_body_util::Empty::new())
            .unwrap();
        let resp = tokio::time::timeout(Duration::from_secs(5), c.request(req))
            .await
            .expect("dialed somewhere other than loopback")
            .unwrap();
        use http_body_util::BodyExt;
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"host: 203.0.113.7");
        server.await.unwrap();
    }
}
