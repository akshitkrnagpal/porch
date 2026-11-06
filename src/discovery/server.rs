//! Opt-in authenticated discovery agent. This is separate from the app proxy lifecycle.
use super::types::*;
use crate::state::{self, ProcessIdentity, ProxySettings, Route, State};
use anyhow::{bail, Context, Result};
use bytes::Bytes;
use fs2::FileExt;
use http_body_util::{combinators::UnsyncBoxBody, BodyExt, Full, Limited};
use hyper::{body::Incoming, header, Method, Request, Response, StatusCode, Version};
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::{TokioExecutor, TokioIo, TokioTimer},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    convert::Infallible,
    fs::{self, OpenOptions},
    io::Write,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type Body = UnsyncBoxBody<Bytes, BoxError>;
type Upstream = Client<hyper_rustls::HttpsConnector<HttpConnector>, Body>;
const MAX_CLIENTS: usize = 64;
const MAX_ROUTES: usize = 512;
const MAX_UPLOAD: usize = 16 * 1024 * 1024;
const PAIR_LIFETIME: u64 = 600;

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|v| format!("{v:02x}")).collect()
}
fn random() -> Result<String> {
    let mut value = [0u8; 32];
    getrandom::fill(&mut value).map_err(|e| anyhow::anyhow!("secure random generation: {e}"))?;
    Ok(hex(&value))
}
fn digest(value: &str) -> String {
    hex(&Sha256::digest(value.as_bytes()))
}
fn equal(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |n, (x, y)| n | (x ^ y)) == 0
}
fn read<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => {
            if bytes.len() > MAX_BODY {
                bail!("discovery state exceeds size limit")
            }
            Ok(Some(
                serde_json::from_slice(&bytes).context("invalid discovery state")?,
            ))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
/// Private atomic files do not reuse proxy lifecycle state or modify trust stores.
fn write(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    if fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_symlink()) {
        bail!("discovery state directory cannot be a symlink")
    }
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    let path = dir.join(format!("{name}.{}.{}.tmp", std::process::id(), random()?));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(&path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(path, dir.join(name))?;
    fs::File::open(dir)?.sync_all()?;
    Ok(())
}
fn lock(dir: &Path) -> Result<fs::File> {
    // Preparing the directory through an atomic marker ensures private permissions.
    write(dir, "discovery-format", b"1\n")?;
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(dir.join("discovery-auth.lock"))?;
    file.lock_exclusive()?;
    Ok(file)
}
#[derive(Default, Serialize, Deserialize)]
struct Auth {
    invitation: Option<Invitation>,
    #[serde(default)]
    clients: Vec<String>,
}
#[derive(Serialize, Deserialize)]
struct Invitation {
    hash: String,
    expires: u64,
}
fn machine(dir: &Path) -> Result<Machine> {
    let _lock = lock(dir)?;
    if let Some(value) = read(&dir.join("discovery-machine.json"))? {
        return Ok(value);
    }
    let name = std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            let mut buf = [0u8; 256];
            #[cfg(unix)]
            {
                if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len() - 1) } == 0 {
                    return Some(
                        String::from_utf8_lossy(
                            &buf[..buf.iter().position(|b| *b == 0).unwrap_or(buf.len())],
                        )
                        .into_owned(),
                    );
                }
            }
            None
        })
        .unwrap_or_else(|| "Porch machine".into());
    let value = Machine {
        id: random()?,
        name: name.chars().filter(|c| !c.is_control()).take(128).collect(),
    };
    write(dir, "discovery-machine.json", &serde_json::to_vec(&value)?)?;
    Ok(value)
}
pub fn identity() -> Result<Machine> {
    machine(&state::dir())
}
/// A literal private, CGNAT, ULA, link-local or loopback address is mandatory.
pub fn validate_bind(bind: &str) -> Result<IpAddr> {
    let ip: IpAddr = bind
        .parse()
        .context("discovery --bind requires an IP address on the intended interface")?;
    let allowed = match ip {
        IpAddr::V4(v) => {
            v.is_private()
                || v.is_loopback()
                || v.is_link_local()
                || (v.octets()[0] == 100 && (64..=127).contains(&v.octets()[1]))
        }
        IpAddr::V6(v) => v.is_loopback() || v.is_unique_local() || v.is_unicast_link_local(),
    };
    if !allowed || ip.is_unspecified() || ip.is_multicast() {
        bail!("discovery must bind one explicit private LAN, tailnet or loopback IP; wildcard/public binds are refused")
    }
    Ok(ip)
}
pub fn configure(value: ServerConfig) -> Result<()> {
    validate_bind(&value.bind)?;
    if value.port == 0 {
        bail!("discovery port must be 1-65535")
    }
    identity()?;
    ensure_certificate(&state::dir(), &value.bind)?;
    write(
        &state::dir(),
        "discovery-config.json",
        &serde_json::to_vec_pretty(&value)?,
    )
}
pub fn config() -> Result<Option<ServerConfig>> {
    read(&state::dir().join("discovery-config.json"))
}
pub fn disable() -> Result<()> {
    match fs::remove_file(state::dir().join("discovery-config.json")) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}
fn ensure_certificate(dir: &Path, bind: &str) -> Result<()> {
    let _lock = lock(dir)?;
    let cert = dir.join("discovery-cert.pem");
    let key = dir.join("discovery-key.pem");
    if cert.exists() && key.exists() {
        return Ok(());
    }
    if cert.exists() || key.exists() {
        bail!("discovery certificate/key incomplete; restore both files or explicitly reset discovery identity")
    }
    let pair = rcgen::generate_simple_self_signed(vec![bind.into(), "porch-agent.local".into()])?;
    write(
        dir,
        "discovery-key.pem",
        pair.key_pair.serialize_pem().as_bytes(),
    )?;
    write(dir, "discovery-cert.pem", pair.cert.pem().as_bytes())?;
    Ok(())
}
fn cert_der(dir: &Path) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    let bytes = fs::read(dir.join("discovery-cert.pem"))?;
    Ok(rustls_pemfile::certs(&mut bytes.as_slice()).collect::<std::io::Result<_>>()?)
}
pub fn fingerprint() -> Result<String> {
    let bind = config()?
        .context("discovery is disabled; enable it with an explicit bind first")?
        .bind;
    ensure_certificate(&state::dir(), &bind)?;
    let cert = cert_der(&state::dir())?;
    Ok(hex(&Sha256::digest(
        cert.first()
            .context("missing discovery certificate")?
            .as_ref(),
    )))
}
pub fn create_pairing_token() -> Result<String> {
    config()?.context("discovery is disabled")?;
    let dir = state::dir();
    let _lock = lock(&dir)?;
    let token = random()?;
    let mut auth: Auth = read(&dir.join("discovery-auth.json"))?.unwrap_or_default();
    auth.invitation = Some(Invitation {
        hash: digest(&token),
        expires: now() + PAIR_LIFETIME,
    });
    write(&dir, "discovery-auth.json", &serde_json::to_vec(&auth)?)?;
    Ok(token)
}
pub fn revoke_clients() -> Result<()> {
    let dir = state::dir();
    let _lock = lock(&dir)?;
    write(
        &dir,
        "discovery-auth.json",
        &serde_json::to_vec(&Auth::default())?,
    )
}
fn paired(dir: &Path, token: &str) -> Result<bool> {
    if token.len() != 64 {
        return Ok(false);
    }
    let hash = digest(token);
    let auth: Auth = read(&dir.join("discovery-auth.json"))?.unwrap_or_default();
    Ok(auth.clients.iter().any(|v| equal(v, &hash)))
}
fn enroll(dir: &Path, token: &str) -> Result<Option<String>> {
    if token.len() != 64 {
        return Ok(None);
    }
    let _lock = lock(dir)?;
    let mut auth: Auth = read(&dir.join("discovery-auth.json"))?.unwrap_or_default();
    if !auth
        .invitation
        .as_ref()
        .is_some_and(|v| v.expires > now() && equal(&v.hash, &digest(token)))
    {
        return Ok(None);
    }
    if auth.clients.len() >= MAX_CLIENTS {
        bail!("client enrollment limit reached; revoke old clients locally")
    }
    let bearer = random()?;
    auth.clients.push(digest(&bearer));
    auth.invitation = None;
    write(dir, "discovery-auth.json", &serde_json::to_vec(&auth)?)?;
    Ok(Some(bearer))
}
fn running(dir: &Path) -> bool {
    read::<ProcessIdentity>(&dir.join("proxy.identity"))
        .ok()
        .flatten()
        .is_some_and(|p| {
            state::matches(&p)
                && fs::read_to_string(dir.join("proxy.ready"))
                    .ok()
                    .is_some_and(|v| v.trim() == p.pid.to_string())
        })
}
fn route_id(machine: &Machine, name: &str) -> String {
    digest(&format!("{}\0{name}", machine.id))
}
fn route_url(settings: &ProxySettings, name: &str) -> String {
    let (name, path) = name.split_once('/').map_or((name, ""), |(n, p)| (n, p));
    let url = settings.url(name);
    if path.is_empty() {
        url
    } else {
        format!("{url}/{path}")
    }
}
fn catalog(dir: &Path, machine: Machine, share: bool) -> Result<Catalog> {
    let data: State = read(&dir.join("state.json"))?.unwrap_or_default();
    let settings: ProxySettings = read(&dir.join("proxy.json"))?.unwrap_or_default();
    let proxy_running = running(dir);
    let apps = data
        .routes
        .iter()
        .take(MAX_ROUTES)
        .map(|route| {
            let alive = state::route_alive(&data, route);
            let supported = route_target(route).is_ok();
            let remote_access = share && alive && supported;
            App {
                id: route_id(&machine, &route.name),
                name: route.name.clone(),
                local_url: route_url(&settings, &route.name),
                health: if !alive {
                    Health::Stopped
                } else {
                    Health::Unknown
                },
                remote_access,
                unavailable_reason: if !alive {
                    Some("App process is stopped".into())
                } else if !share {
                    Some("Local only; remote app transport is disabled on this machine".into())
                } else if !supported {
                    Some("Route target is not a supported HTTP URL".into())
                } else {
                    None
                },
            }
        })
        .collect();
    Ok(Catalog {
        version: PROTOCOL_VERSION,
        machine,
        apps,
        proxy_running,
        generated_at: now(),
    })
}
pub fn local_catalog() -> Result<Catalog> {
    let dir = state::dir();
    catalog(
        &dir,
        machine(&dir)?,
        config()?.is_some_and(|c| c.share_apps),
    )
}
fn response(status: StatusCode, message: impl Into<Bytes>) -> Response<Body> {
    let mut r = Response::new(
        Full::new(message.into())
            .map_err(|e: Infallible| match e {})
            .boxed_unsync(),
    );
    *r.status_mut() = status;
    r.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    r
}
fn json<T: Serialize>(value: &T) -> Response<Body> {
    match serde_json::to_vec(value) {
        Ok(bytes) if bytes.len() <= MAX_BODY => {
            let mut r = response(StatusCode::OK, bytes);
            r.headers_mut().insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/json"),
            );
            r
        }
        _ => response(StatusCode::INTERNAL_SERVER_ERROR, "catalog unavailable"),
    }
}
fn route_target(route: &Route) -> Result<url::Url> {
    let target = if route.target.contains("://") {
        route.target.clone()
    } else {
        format!("http://{}", route.target)
    };
    let target = url::Url::parse(&target)?;
    if !matches!(target.scheme(), "http" | "https")
        || target.host().is_none()
        || !target.username().is_empty()
        || target.password().is_some()
    {
        bail!("unsupported route target")
    };
    Ok(target)
}
fn strip(headers: &mut header::HeaderMap) {
    let named: Vec<_> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|v| v.trim().to_owned())
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
#[derive(Clone)]
struct ContextState {
    dir: PathBuf,
    machine: Machine,
    share_apps: bool,
    client: Upstream,
    tunnels: Arc<tokio::sync::Semaphore>,
}
async fn request(mut req: Request<Incoming>, context: Arc<ContextState>) -> Result<Response<Body>> {
    if req.headers().contains_key(header::ORIGIN) {
        return Ok(response(
            StatusCode::FORBIDDEN,
            "browser origins are not allowed on the agent API",
        ));
    }
    if req.uri().path() == "/v1/pair" && req.method() == Method::POST {
        let body = match tokio::time::timeout(
            Duration::from_secs(5),
            Limited::new(req.into_body(), 1024).collect(),
        )
        .await
        {
            Ok(Ok(v)) => v.to_bytes(),
            _ => return Ok(response(StatusCode::BAD_REQUEST, "invalid pairing request")),
        };
        let input: PairRequest = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => return Ok(response(StatusCode::BAD_REQUEST, "invalid pairing request")),
        };
        return Ok(match enroll(&context.dir, &input.token) {
            Ok(Some(token)) => json(&PairResponse {
                version: PROTOCOL_VERSION,
                machine: context.machine.clone(),
                token,
            }),
            Ok(None) => response(StatusCode::UNAUTHORIZED, "invalid or expired pairing token"),
            Err(_) => response(
                StatusCode::CONFLICT,
                "enrollment unavailable; check local agent state",
            ),
        });
    }
    let token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "));
    if !token.is_some_and(|t| paired(&context.dir, t).unwrap_or(false)) {
        return Ok(response(
            StatusCode::UNAUTHORIZED,
            "pair this machine before requesting its catalog",
        ));
    }
    if req.uri().path() == "/v1/catalog" && req.method() == Method::GET {
        return Ok(json(&catalog(
            &context.dir,
            context.machine.clone(),
            context.share_apps,
        )?));
    }
    let Some((app_id, app_path)) = req
        .uri()
        .path()
        .strip_prefix("/v1/apps/")
        .and_then(|v| v.split_once("/proxy"))
    else {
        return Ok(response(StatusCode::NOT_FOUND, "unknown agent endpoint"));
    };
    if !app_path.is_empty() && !app_path.starts_with('/') {
        return Ok(response(StatusCode::NOT_FOUND, "unknown app endpoint"));
    }
    if !context.share_apps {
        return Ok(response(
            StatusCode::FORBIDDEN,
            "remote app transport is disabled",
        ));
    }
    if app_id.len() != 64 {
        return Ok(response(StatusCode::NOT_FOUND, "unknown app"));
    }
    let data: State = read(&context.dir.join("state.json"))?.unwrap_or_default();
    let Some(route) = data
        .routes
        .iter()
        .find(|r| route_id(&context.machine, &r.name) == app_id && state::route_alive(&data, r))
    else {
        return Ok(response(
            StatusCode::NOT_FOUND,
            "app is no longer registered",
        ));
    };
    let settings: ProxySettings = read(&context.dir.join("proxy.json"))?.unwrap_or_default();
    let mut target = route_target(route)?;
    let base = target.path().trim_end_matches('/');
    let app_path = if app_path.is_empty() { "/" } else { app_path };
    let path = format!("{base}{app_path}");
    target.set_path(&path);
    target.set_query(req.uri().query());
    // Url::set_path escapes existing percent encodings. Use the validated target authority plus raw app path.
    let uri = format!(
        "{}://{}{}{}",
        target.scheme(),
        &target[url::Position::BeforeHost..url::Position::AfterPort],
        path,
        req.uri()
            .query()
            .map(|q| format!("?{q}"))
            .unwrap_or_default()
    );
    let uri: hyper::Uri = uri.parse().context("invalid route URI")?;
    let websocket = req
        .headers()
        .get(header::UPGRADE)
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"));
    let tunnel_permit = if websocket {
        match context.tunnels.clone().try_acquire_owned() {
            Ok(permit) => Some(permit),
            Err(_) => {
                return Ok(response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "app tunnel limit reached",
                ))
            }
        }
    } else {
        None
    };
    let downstream = websocket.then(|| hyper::upgrade::on(&mut req));
    let origin = req.headers_mut().remove("x-porch-app-origin");
    let auth = req.headers_mut().remove("x-porch-app-authorization");
    let original_host = req.headers_mut().remove("x-porch-app-host");
    strip(req.headers_mut());
    req.headers_mut().remove(header::AUTHORIZATION);
    // Never let a bearer or arbitrary internal control header reach a development app.
    let private: Vec<_> = req
        .headers()
        .keys()
        .filter(|h| h.as_str().starts_with("x-porch-app-"))
        .cloned()
        .collect();
    for h in private {
        req.headers_mut().remove(h);
    }
    if let Some(v) = origin {
        req.headers_mut().insert(header::ORIGIN, v);
    }
    if let Some(v) = auth {
        req.headers_mut().insert(header::AUTHORIZATION, v);
    }
    if let Some(v) = original_host {
        req.headers_mut().insert("x-forwarded-host", v);
    }
    let host = route_url(&settings, &route.name);
    let host = url::Url::parse(&host)?;
    let host = &host[url::Position::BeforeHost..url::Position::AfterPort];
    req.headers_mut().insert(header::HOST, host.parse()?);
    req.headers_mut().insert(
        "x-forwarded-proto",
        header::HeaderValue::from_static("http"),
    );
    if websocket {
        req.headers_mut().insert(
            header::UPGRADE,
            header::HeaderValue::from_static("websocket"),
        );
        req.headers_mut().insert(
            header::CONNECTION,
            header::HeaderValue::from_static("upgrade"),
        );
    }
    *req.uri_mut() = uri;
    *req.version_mut() = Version::HTTP_11;
    let (parts, body) = req.into_parts();
    let req = Request::from_parts(
        parts,
        Limited::new(body, MAX_UPLOAD)
            .map_err(|e| -> BoxError { e })
            .boxed_unsync(),
    );
    let mut upstream =
        match tokio::time::timeout(Duration::from_secs(15), context.client.request(req)).await {
            Ok(Ok(v)) => v,
            _ => {
                return Ok(response(
                    StatusCode::BAD_GATEWAY,
                    "registered app is unavailable",
                ))
            }
        };
    if upstream.status() == StatusCode::SWITCHING_PROTOCOLS {
        if let Some(downstream) = downstream {
            let remote = hyper::upgrade::on(&mut upstream);
            tokio::spawn(async move {
                let _permit = tunnel_permit;
                if let Ok((a, b)) = tokio::try_join!(downstream, remote) {
                    let mut a = TokioIo::new(a);
                    let mut b = TokioIo::new(b);
                    let _ = tokio::io::copy_bidirectional(&mut a, &mut b).await;
                }
            });
        } else {
            return Ok(response(StatusCode::BAD_GATEWAY, "unexpected app upgrade"));
        }
    } else {
        strip(upstream.headers_mut());
    }
    Ok(upstream.map(|body| body.map_err(|e| -> BoxError { Box::new(e) }).boxed_unsync()))
}
fn upstream(dir: &Path) -> Result<Upstream> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    if let Ok(bytes) = fs::read(dir.join("ca.pem")) {
        for cert in rustls_pemfile::certs(&mut bytes.as_slice()).flatten() {
            let _ = roots.add(cert);
        }
    }
    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_root_certificates(roots)
    .with_no_client_auth();
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    http.set_connect_timeout(Some(Duration::from_secs(5)));
    let connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(tls)
        .https_or_http()
        .enable_http1()
        .wrap_connector(http);
    Ok(Client::builder(TokioExecutor::new()).build(connector))
}
struct Advertisement {
    daemon: mdns_sd::ServiceDaemon,
    fullname: String,
}
impl Drop for Advertisement {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}
fn advertise(machine: &Machine, ip: IpAddr, port: u16) -> Result<Advertisement> {
    let daemon = mdns_sd::ServiceDaemon::new()?;
    daemon.disable_interface(mdns_sd::IfKind::All)?;
    daemon.enable_interface(mdns_sd::IfKind::Addr(ip))?;
    let properties = [("id", machine.id.as_str()), ("version", "1")];
    let info = mdns_sd::ServiceInfo::new(
        SERVICE_TYPE,
        &format!("Porch-{}", &machine.id[..12]),
        &format!("porch-{}.local.", &machine.id[..12]),
        ip,
        port,
        &properties[..],
    )?;
    let fullname = info.get_fullname().to_owned();
    daemon.register(info)?;
    Ok(Advertisement { daemon, fullname })
}
pub async fn serve() -> Result<()> {
    let config =
        config()?.context("discovery is disabled; run porch discovery enable --bind IP")?;
    let ip = validate_bind(&config.bind)?;
    let dir = state::dir();
    let machine = machine(&dir)?;
    ensure_certificate(&dir, &config.bind)?;
    let certificates = cert_der(&dir)?;
    let bytes = fs::read(dir.join("discovery-key.pem"))?;
    let key =
        rustls_pemfile::private_key(&mut bytes.as_slice())?.context("missing discovery key")?;
    let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(certificates, key)?;
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    let listener = tokio::net::TcpListener::bind(SocketAddr::new(ip, config.port))
        .await
        .context("binding discovery interface")?;
    let _advertisement = if config.advertise {
        match advertise(&machine, ip, config.port) {
            Ok(v) => Some(v),
            Err(e) => {
                eprintln!("Porch discovery advertisement unavailable: {e}");
                None
            }
        }
    } else {
        None
    };
    let context = Arc::new(ContextState {
        dir: dir.clone(),
        machine,
        share_apps: config.share_apps,
        client: upstream(&dir)?,
        tunnels: Arc::new(tokio::sync::Semaphore::new(64)),
    });
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
    let slots = Arc::new(tokio::sync::Semaphore::new(64));
    state::atomic_write("discovery.ready", std::process::id().to_string().as_bytes())?;
    connections(listener, acceptor, context, slots).await
}
async fn connections(
    listener: tokio::net::TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
    context: Arc<ContextState>,
    slots: Arc<tokio::sync::Semaphore>,
) -> Result<()> {
    loop {
        let (stream, _peer) = listener.accept().await?;
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            drop(stream);
            continue;
        };
        let acceptor = acceptor.clone();
        let context = context.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let Ok(Ok(stream)) =
                tokio::time::timeout(Duration::from_secs(5), acceptor.accept(stream)).await
            else {
                return;
            };
            let service = hyper::service::service_fn(move |req| {
                let context = context.clone();
                async move {
                    Ok::<_, Infallible>(match request(req, context).await {
                        Ok(v) => v,
                        Err(_) => {
                            response(StatusCode::INTERNAL_SERVER_ERROR, "agent request failed")
                        }
                    })
                }
            });
            let mut builder = hyper::server::conn::http1::Builder::new();
            builder
                .timer(TokioTimer::new())
                .header_read_timeout(Duration::from_secs(10))
                .max_headers(64)
                .max_buf_size(32 * 1024);
            let _ = builder
                .serve_connection(TokioIo::new(stream), service)
                .with_upgrades()
                .await;
        });
    }
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
