//! Local CA and SNI certificates. This module never changes system trust.
use anyhow::{bail, Context, Result};
use fs2::FileExt;
use rcgen::{
    BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose,
};
use rustls::{
    server::{ClientHello, ResolvesServerCert},
    sign::CertifiedKey,
};
use std::{
    collections::HashMap,
    fs,
    io::BufReader,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};
/// Write a private file atomically: a temporary file in the same directory,
/// synced, then renamed over `path`. A crash or full disk leaves either the old
/// file or none, never a partial one.
fn write_private(path: &Path, value: &str) -> Result<()> {
    use std::io::Write;
    let name = path
        .file_name()
        .context("private file has no name")?
        .to_string_lossy();
    let temporary = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    let _ = fs::remove_file(&temporary);
    let result = (|| -> Result<()> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(value.as_bytes())?;
        file.sync_all()?;
        crate::state::inherit_owner(&temporary)?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.with_context(|| format!("writing {}", path.display()))
}
fn certified(cert: &str, key: &str) -> Result<Arc<CertifiedKey>> {
    let chain = crate::pem::certs(&mut BufReader::new(cert.as_bytes()))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let key = crate::pem::private_key(&mut BufReader::new(key.as_bytes()))?
        .context("No private key in PEM")?;
    let provider = rustls::crypto::ring::default_provider();
    Ok(Arc::new(CertifiedKey::from_der(chain, key, &provider)?))
}
/// Reissue a cached leaf this long before it expires.
const RENEW_BEFORE: Duration = Duration::from_secs(86_400);
/// At most this many cached leaves for names that serve nothing (typos, probes).
const UNKNOWN_CACHE: usize = 256;
/// A hard cap on cached leaves for real names, far above any real route count.
const KNOWN_CACHE: usize = 4096;
struct Cached {
    cert: Arc<CertifiedKey>,
    not_after: SystemTime,
    used: std::time::Instant,
    known: bool,
}
/// The local CA, parsed once: its certificate as sent in chains and the issuer
/// (key plus re-created certificate) that signs leaves.
struct Issuer {
    ca_pem: String,
    key: KeyPair,
    cert: rcgen::Certificate,
}
struct LocalResolver {
    issuer: Issuer,
    /// Issued certificates and when each was last used (least recently used
    /// entries for unknown names are evicted first).
    cache: Mutex<HashMap<String, Cached>>,
    settings: crate::state::ProxySettings,
    /// What a name serves: a local route or the dashboard, an app on a
    /// paired machine, or nothing.
    known: fn(&str, &crate::state::ProxySettings) -> NameKind,
    /// Each kind of name has its own budget, so a page cycling random names
    /// (which only reach the 404 page) can't stop your apps' certificates.
    known_budget: Budget,
    /// Any `NAME.MACHINE.<domain>` counts as a paired machine's app without
    /// asking that machine, so made-up ones draw from their own budget and
    /// can't stop local apps' or the dashboard's certificates either.
    peer_budget: Budget,
    unknown_budget: Budget,
}
/// What a TLS name serves, which decides the issuance budget it draws from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NameKind {
    /// The dashboard, a configured domain, or a local route.
    Local,
    /// A name under a paired machine (`APP.MACHINE.<domain>`).
    Peer,
    /// Nothing: typos, probes, random names.
    Unknown,
}
impl std::fmt::Debug for LocalResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalResolver")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}
impl LocalResolver {
    fn new(ca_pem: String, ca_key: &str, settings: crate::state::ProxySettings) -> Result<Self> {
        let key = KeyPair::from_pem(ca_key)?;
        let cert = CertificateParams::from_ca_cert_pem(&ca_pem)?.self_signed(&key)?;
        Ok(Self {
            issuer: Issuer { ca_pem, key, cert },
            cache: Mutex::new(HashMap::new()),
            settings,
            known: known_name,
            known_budget: Budget::new(),
            peer_budget: Budget::new(),
            unknown_budget: Budget::new(),
        })
    }
    fn authorize(&self, name: &str) -> Result<()> {
        // Route names never widen issuance (see allowed_name); routes only
        // decide which issuance budget a name draws from.
        if !allowed_name(name, &self.settings, &[]) {
            bail!("TLS name {name:?} is outside Porch's configured domains");
        }
        Ok(())
    }
    fn issue(&self, name: &str) -> Result<(Arc<CertifiedKey>, SystemTime)> {
        self.authorize(name)?;
        let key = KeyPair::generate()?;
        let mut params = CertificateParams::new(vec![name.to_owned()])?;
        set_validity(&mut params, 365);
        params.not_after = params.not_after.min(self.issuer.cert.params().not_after);
        let not_after: SystemTime = params.not_after.into();
        if not_after <= SystemTime::now() {
            bail!("Local CA has expired; explicitly replace and trust a new CA");
        }
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, name);
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.use_authority_key_identifier_extension = true;
        let cert = params.signed_by(&key, &self.issuer.cert, &self.issuer.key)?;
        Ok((
            certified(
                &format!("{}{}", cert.pem(), self.issuer.ca_pem),
                &key.serialize_pem(),
            )?,
            not_after,
        ))
    }
}
/// A sliding window of recent issuances: at most 30 new leaves per 10 seconds.
struct Budget(Mutex<std::collections::VecDeque<std::time::Instant>>);
impl Budget {
    fn new() -> Self {
        Self(Mutex::new(std::collections::VecDeque::new()))
    }
    fn take(&self) -> bool {
        use std::time::Instant;
        let Ok(mut recent) = self.0.lock() else {
            return false;
        };
        let now = Instant::now();
        while recent
            .front()
            .is_some_and(|t| now.duration_since(*t) > Duration::from_secs(10))
        {
            recent.pop_front();
        }
        if recent.len() >= 30 {
            return false;
        }
        recent.push_back(now);
        true
    }
}
impl ResolvesServerCert for LocalResolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let name = hello
            .server_name()
            .unwrap_or("localhost")
            .to_ascii_lowercase();
        self.resolve_name(&name)
    }
}
impl LocalResolver {
    fn resolve_name(&self, name: &str) -> Option<Arc<CertifiedKey>> {
        self.authorize(name).ok()?;
        let now = SystemTime::now();
        {
            let mut cache = self.cache.lock().ok()?;
            if let Some(entry) = cache.get_mut(name) {
                if entry.not_after > now + RENEW_BEFORE {
                    entry.used = std::time::Instant::now();
                    return Some(entry.cert.clone());
                }
            }
        }
        // Each new name costs a key generation: cap the rate, so a client
        // cycling random names can't burn CPU (names in use stay cached).
        let kind = (self.known)(name, &self.settings);
        let known = kind != NameKind::Unknown;
        let budget = match kind {
            NameKind::Local => &self.known_budget,
            NameKind::Peer => &self.peer_budget,
            NameKind::Unknown => &self.unknown_budget,
        };
        if !budget.take() {
            // Out of budget: an expiring (still valid) leaf beats none.
            let cache = self.cache.lock().ok()?;
            return cache
                .get(name)
                .filter(|e| e.not_after > now)
                .map(|e| e.cert.clone());
        }
        // Signing happens outside the lock: other handshakes keep using the cache.
        let (cert, not_after) = self.issue(name).ok()?;
        let mut cache = self.cache.lock().ok()?;
        let (count, cap) = if known {
            (cache.len(), KNOWN_CACHE)
        } else {
            (cache.values().filter(|e| !e.known).count(), UNKNOWN_CACHE)
        };
        if count >= cap && !cache.contains_key(name) {
            if let Some(oldest) = cache
                .iter()
                .filter(|(_, e)| known || !e.known)
                .min_by_key(|(_, e)| e.used)
                .map(|(k, _)| k.clone())
            {
                cache.remove(&oldest);
            }
        }
        cache.insert(
            name.to_owned(),
            Cached {
                cert: cert.clone(),
                not_after,
                used: std::time::Instant::now(),
                known,
            },
        );
        Some(cert)
    }
}
/// What `name` serves: the dashboard or a route, an app on a paired machine
/// (`APP.MACHINE.<domain>`), or nothing. Costs a `stat` of the route state
/// and, at most every few seconds, a read of the paired machines.
fn known_name(name: &str, settings: &crate::state::ProxySettings) -> NameKind {
    let routes = crate::state::live_routes()
        .map(|s| s.routes.iter().map(|r| r.name.clone()).collect::<Vec<_>>())
        .unwrap_or_default();
    known_in(name, settings, &routes, &peer_slugs())
}
fn peer_slugs() -> Vec<String> {
    use std::time::Instant;
    static SLUGS: Mutex<Option<(Instant, Vec<String>)>> = Mutex::new(None);
    let Ok(mut cached) = SLUGS.lock() else {
        return Vec::new();
    };
    if let Some((at, slugs)) = cached.as_ref() {
        if at.elapsed() < Duration::from_secs(5) {
            return slugs.clone();
        }
    }
    let slugs: Vec<String> = crate::discovery::client::peers()
        .map(|p| p.into_iter().map(|p| p.slug).collect())
        .unwrap_or_default();
    *cached = Some((Instant::now(), slugs.clone()));
    slugs
}
fn known_in(
    name: &str,
    settings: &crate::state::ProxySettings,
    routes: &[String],
    peers: &[String],
) -> NameKind {
    let mut domains: Vec<String> = settings
        .tlds
        .iter()
        .map(|t| t.trim_start_matches('.').to_ascii_lowercase())
        .collect();
    if settings.lan {
        domains.push("local".into());
    }
    if name == "localhost" || name == "porch.localhost" || domains.iter().any(|d| d == name) {
        return NameKind::Local;
    }
    let under =
        |host: &str| name == host || (settings.wildcard && name.ends_with(&format!(".{host}")));
    for route in routes {
        let route = route.trim_end_matches('.').to_ascii_lowercase();
        if domains.iter().any(|d| route.ends_with(&format!(".{d}"))) {
            if under(&route) {
                return NameKind::Local;
            }
        } else if domains.iter().any(|d| under(&format!("{route}.{d}"))) {
            return NameKind::Local;
        }
    }
    // Any app name under a paired machine counts: the peer's catalog isn't
    // consulted here, so these names get a budget of their own.
    if peers.iter().filter(|s| !s.is_empty()).any(|slug| {
        domains
            .iter()
            .any(|d| name.ends_with(&format!(".{slug}.{d}")))
    }) {
        return NameKind::Peer;
    }
    NameKind::Unknown
}
// Root DNS name constraints are deliberately not fixed to the initial settings: TLDs
// and registered domains can change without replacing a system-trusted root. All
// automatic issuance instead passes this policy on every handshake, including
// cache hits after a registered route is removed. Custom certificate/key pairs are supplied by the user.
fn allowed_name(
    name: &str,
    settings: &crate::state::ProxySettings,
    routes: &[crate::state::Route],
) -> bool {
    if name.len() > 253
        || name.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        })
    {
        return false;
    }
    // Only names under a domain Porch serves: the configured TLDs and, in LAN
    // mode, `.local`. A route's name never widens this: a route called
    // `github.com` is served as `github.com.localhost`, so the CA must never
    // sign `github.com` itself. Real domains need your own --cert/--key.
    let _ = routes;
    let in_domain = |domain: &str| name == domain || name.ends_with(&format!(".{domain}"));
    name == "localhost"
        || settings
            .tlds
            .iter()
            .filter(|tld| signable_domain(tld))
            .any(|tld| in_domain(tld.trim_start_matches('.')))
        || (settings.lan && in_domain("local"))
}
fn set_validity(params: &mut CertificateParams, days: u64) {
    // A short backdate tolerates normal clock skew without extending the profile.
    let start = SystemTime::now() - Duration::from_secs(300);
    params.not_before = start.into();
    params.not_after = (start + Duration::from_secs(days * 86_400)).into();
}
/// Domains a new local CA may ever sign for: reserved, never-public names plus
/// the TLDs configured when it is created. Browsers enforce these X.509 name
/// constraints, so even a leaked CA key can't impersonate a real website.
const RESERVED: [&str; 5] = ["localhost", "local", "test", "internal", "home.arpa"];
/// Can the local CA ever sign names under `tld`? Reserved, never-public names
/// can, and so can a domain with at least two labels (one you control, such as
/// `local.example.com`). A bare TLD like `com`, `dev` or `app` never can: a CA
/// allowed to sign all of `.com` could impersonate real sites.
pub fn signable_domain(tld: &str) -> bool {
    let t = tld.trim_start_matches('.').to_ascii_lowercase();
    if RESERVED
        .iter()
        .any(|r| t == *r || t.ends_with(&format!(".{r}")))
    {
        return true;
    }
    // Anything else must sit inside a registrable domain (a name someone can
    // own, such as example.com or example.co.uk), never be a public suffix
    // itself (com, co.uk, github.io) per the Public Suffix List.
    match (psl::suffix(t.as_bytes()), psl::domain(t.as_bytes())) {
        (Some(suffix), Some(_)) => suffix.as_bytes() != t.as_bytes(),
        _ => false,
    }
}
/// Domains a new CA permits: the reserved names, the configured TLDs, and
/// every domain the CA it replaces permitted (`previous`), so switching TLDs
/// back and forth doesn't make a new CA, and a new trust prompt, each time.
fn ca_permitted(settings: &crate::state::ProxySettings, previous: &[String]) -> Vec<String> {
    let mut out: Vec<String> = RESERVED.into_iter().map(str::to_owned).collect();
    for t in settings.tlds.iter().chain(previous) {
        let t = t.trim_start_matches('.').to_ascii_lowercase();
        if signable_domain(&t) && !out.iter().any(|p| t == *p || t.ends_with(&format!(".{p}"))) {
            out.push(t);
        }
    }
    out
}
/// The DNS names a CA's name constraints permit (empty for an unconstrained CA).
fn permitted_subtrees(ca_pem: &str) -> Vec<String> {
    CertificateParams::from_ca_cert_pem(ca_pem)
        .ok()
        .and_then(|ca| ca.name_constraints)
        .map(|c| {
            c.permitted_subtrees
                .iter()
                .filter_map(|s| match s {
                    rcgen::GeneralSubtree::DnsName(d) => {
                        Some(d.trim_start_matches('.').to_ascii_lowercase())
                    }
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}
/// Whether a CA is limited to server authentication by an extended key usage.
/// CAs made before Porch 0.5.3 aren't: see SECURITY.md.
pub fn ca_server_auth_only(ca_pem: &str) -> bool {
    CertificateParams::from_ca_cert_pem(ca_pem)
        .is_ok_and(|ca| ca.extended_key_usages == [ExtendedKeyUsagePurpose::ServerAuth])
}
/// The configured TLDs the CA's name constraints don't cover (none for an
/// older, unconstrained CA).
pub fn uncovered_tlds(ca_pem: &str, settings: &crate::state::ProxySettings) -> Vec<String> {
    let Ok(ca) = CertificateParams::from_ca_cert_pem(ca_pem) else {
        return vec![];
    };
    if ca.name_constraints.is_none() {
        return vec![];
    }
    let permitted = permitted_subtrees(ca_pem);
    settings
        .tlds
        .iter()
        .chain(settings.lan.then(|| "local".to_owned()).iter())
        .map(|t| t.trim_start_matches('.').to_ascii_lowercase())
        .filter(|t| signable_domain(t))
        .filter(|t| {
            !permitted
                .iter()
                .any(|p| t == p || t.ends_with(&format!(".{p}")))
        })
        .collect()
}
fn ca_params(
    settings: &crate::state::ProxySettings,
    previous: &[String],
) -> Result<CertificateParams> {
    let mut params = CertificateParams::new(Vec::<String>::new())?;
    params.name_constraints = Some(rcgen::NameConstraints {
        permitted_subtrees: ca_permitted(settings, previous)
            .into_iter()
            .map(rcgen::GeneralSubtree::DnsName)
            .collect(),
        // No IP addresses at all: Porch only ever signs DNS names.
        excluded_subtrees: vec![
            rcgen::GeneralSubtree::IpAddress(rcgen::CidrSubnet::V4([0; 4], [0; 4])),
            rcgen::GeneralSubtree::IpAddress(rcgen::CidrSubnet::V6([0; 16], [0; 16])),
        ],
    });
    set_validity(&mut params, 3650);
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "Porch Local CA");
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    // Name constraints don't limit S/MIME or code-signing certificates: the
    // extended key usage limits everything this CA signs to TLS servers.
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    Ok(params)
}
/// Shown once for a CA made before Porch limited new CAs to TLS servers.
pub const LEGACY_CA_NOTICE: &str = "Porch TLS: your local CA predates Porch 0.5.3 and isn't limited to TLS servers; Porch kept it so your trust stays in place. To replace it with a TLS-only CA: `porch proxy stop`, delete ca-key.pem from the Porch state folder, then `porch proxy start --https` and `porch trust` (which also removes the old CA's trust).";
/// Delete private keys kept by older Porch versions beside retired CAs.
fn remove_retired_keys(dir: &Path) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with("ca-key.pem.retired-")
            {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}
pub fn config(cert: Option<PathBuf>, key: Option<PathBuf>) -> Result<Arc<rustls::ServerConfig>> {
    let mut settings = crate::state::proxy_settings()?;
    settings.cert = cert;
    settings.key = key;
    config_for_settings(&settings)
}
pub fn config_for_settings(
    settings: &crate::state::ProxySettings,
) -> Result<Arc<rustls::ServerConfig>> {
    config_in_dir(settings, &crate::state::dir())
}
fn config_in_dir(
    settings: &crate::state::ProxySettings,
    dir: &Path,
) -> Result<Arc<rustls::ServerConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_no_client_auth();
    let mut config = match (&settings.cert, &settings.key) {
        (Some(cert), Some(key)) => {
            #[derive(Debug)]
            struct Fixed(Arc<CertifiedKey>);
            impl ResolvesServerCert for Fixed {
                fn resolve(&self, _: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
                    Some(self.0.clone())
                }
            }
            builder.with_cert_resolver(Arc::new(Fixed(certified(
                &fs::read_to_string(cert)?,
                &fs::read_to_string(key)?,
            )?)))
        }
        (None, None) => {
            fs::create_dir_all(dir)?;
            let mut options = fs::OpenOptions::new();
            options.create(true).truncate(false).read(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
            }
            let lock = options.open(dir.join("tls.lock"))?;
            crate::state::inherit_owner(&dir.join("tls.lock"))?;
            lock.lock_exclusive()?;
            let cert_path = dir.join("ca.pem");
            let key_path = dir.join("ca-key.pem");
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or_default();
            // Only a retired CA's certificate is needed (to remove its trust).
            remove_retired_keys(dir);
            let mut previous = Vec::new();
            match (cert_path.exists(), key_path.exists()) {
                (true, true) => {
                    // A name-constrained CA can't sign for domains added later.
                    // Retire it (kept beside, so `porch clean` can still remove
                    // its trust) and make a new one that also covers its domains.
                    let existing = fs::read_to_string(&cert_path)?;
                    let uncovered = uncovered_tlds(&existing, settings);
                    if !uncovered.is_empty() {
                        previous = permitted_subtrees(&existing);
                        fs::rename(&cert_path, dir.join(format!("ca.pem.retired-{stamp}")))?;
                        fs::remove_file(&key_path)?;
                        eprintln!(
                            "Porch made a new local CA, because the old one didn't cover {}. Browsers need it trusted once: run `porch trust` (no sudo).",
                            uncovered.join(", ")
                        );
                    }
                }
                (true, false) => {
                    // The key is gone, so this CA can never sign again. Keep the
                    // certificate for trust removal and start over.
                    let existing = fs::read_to_string(&cert_path).unwrap_or_default();
                    previous = permitted_subtrees(&existing);
                    fs::rename(&cert_path, dir.join(format!("ca.pem.retired-{stamp}")))?;
                    eprintln!("Porch's local CA key was missing, so Porch made a new CA. Browsers need it trusted once: run `porch trust` (no sudo).");
                }
                // An interrupted creation: the certificate is written last, so
                // nothing was ever trusted with this key.
                (false, true) => fs::remove_file(&key_path)?,
                (false, false) => {}
            }
            let fresh = !cert_path.exists();
            if fresh {
                let key = KeyPair::generate()?;
                let params = ca_params(settings, &previous)?;
                let ca = params.self_signed(&key)?;
                // Key first, certificate last: a certificate on disk always has its key.
                write_private(&key_path, &key.serialize_pem())?;
                write_private(&cert_path, &ca.pem())?;
            }
            let damaged = || {
                format!(
                    "Porch's local CA in {} is damaged. Move ca.pem and ca-key.pem out of that folder and start the proxy again to make a new one, then run `porch trust`",
                    dir.display()
                )
            };
            let ca_pem = fs::read_to_string(&cert_path)?;
            let resolver =
                LocalResolver::new(ca_pem, &fs::read_to_string(&key_path)?, settings.clone())
                    .with_context(damaged)?;
            resolver.issue("localhost").with_context(damaged)?;
            // There are no persisted leaf certificates: a new profile takes effect on
            // every new handshake after restart. Preserve an existing root and key
            // byte-for-byte, since silently rotating them would invalidate trust.
            let profile = dir.join("tls-profile-version");
            let seen = fs::read_to_string(&profile).unwrap_or_default();
            if seen.trim() != "3" {
                if !fresh && seen.is_empty() {
                    let ca = CertificateParams::from_ca_cert_pem(&resolver.issuer.ca_pem)?;
                    if (ca.not_after - ca.not_before).whole_days() > 3653 {
                        eprintln!("Porch TLS: retained your existing CA to preserve system trust; new certificates use the restricted v2 server profile. Its original CA validity remains unchanged; replacing a trusted root requires an explicit cleanup and trust setup.");
                    }
                }
                if !fresh && !ca_server_auth_only(&resolver.issuer.ca_pem) {
                    eprintln!("{LEGACY_CA_NOTICE}");
                }
                write_private(&profile, "3\n")?;
            }
            builder.with_cert_resolver(Arc::new(resolver))
        }
        _ => bail!("Both --cert and --key are required"),
    };
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}
#[cfg(test)]
mod tests {
    use super::*;
    fn unconstrained_ca() -> (String, String) {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca = params.self_signed(&key).unwrap();
        (ca.pem(), key.serialize_pem())
    }
    fn test_resolver() -> LocalResolver {
        let (pem, key) = unconstrained_ca();
        let mut resolver =
            LocalResolver::new(pem, &key, crate::state::ProxySettings::default()).unwrap();
        // Never read this machine's real routes or paired machines.
        resolver.known = |_, _| NameKind::Local;
        resolver
    }
    #[test]
    fn generates_signed_leaf() {
        let resolver = test_resolver();
        assert!(resolver.issue("app.localhost").is_ok());
        assert!(resolver.issue("*.example.test").is_err());
    }
    #[test]
    fn cached_leaf_is_reissued_before_it_expires() {
        let resolver = test_resolver();
        let first = resolver.resolve_name("app.localhost").unwrap();
        assert!(Arc::ptr_eq(
            &first,
            &resolver.resolve_name("app.localhost").unwrap()
        ));
        // Within a day of expiry, the next handshake gets a fresh leaf.
        resolver
            .cache
            .lock()
            .unwrap()
            .get_mut("app.localhost")
            .unwrap()
            .not_after = SystemTime::now() + Duration::from_secs(60);
        let renewed = resolver.resolve_name("app.localhost").unwrap();
        assert!(!Arc::ptr_eq(&first, &renewed));
        let entry_after = resolver.cache.lock().unwrap()["app.localhost"].not_after;
        assert!(entry_after > SystemTime::now() + Duration::from_secs(300 * 86_400));
    }
    #[test]
    fn random_names_cannot_starve_or_evict_real_ones() {
        let mut resolver = test_resolver();
        resolver.known = |name, _| {
            if name == "web.localhost" {
                NameKind::Local
            } else {
                NameKind::Unknown
            }
        };
        let web = resolver.resolve_name("web.localhost").unwrap();
        // A page cycling made-up names exhausts only their own budget.
        let mut refused = 0;
        for i in 0..40 {
            if resolver.resolve_name(&format!("r{i}.localhost")).is_none() {
                refused += 1;
            }
        }
        assert!(refused >= 10, "unknown names are rate limited");
        assert!(resolver.resolve_name("web.localhost").is_some());
        // ...and never evict a real name from the cache.
        {
            let mut cache = resolver.cache.lock().unwrap();
            for i in 0..UNKNOWN_CACHE + 10 {
                cache.insert(
                    format!("f{i}.localhost"),
                    Cached {
                        cert: web.clone(),
                        not_after: SystemTime::now() + Duration::from_secs(86_400 * 30),
                        used: std::time::Instant::now(),
                        known: false,
                    },
                );
            }
        }
        std::thread::sleep(Duration::from_millis(5));
        let _ = resolver.resolve_name("another.localhost");
        assert!(resolver.cache.lock().unwrap().contains_key("web.localhost"));
    }
    #[test]
    fn known_names_are_routes_dashboard_and_paired_apps() {
        let mut settings = crate::state::ProxySettings {
            tlds: vec!["localhost".into(), "dev.test".into()],
            ..Default::default()
        };
        let routes = vec!["web".to_owned(), "api.dev.test".to_owned()];
        let peers = vec!["laptop".to_owned()];
        let kind = |n: &str, s: &crate::state::ProxySettings| known_in(n, s, &routes, &peers);
        for name in [
            "localhost",
            "porch.localhost",
            "dev.test",
            "web.localhost",
            "web.dev.test",
            "api.dev.test",
        ] {
            assert_eq!(kind(name, &settings), NameKind::Local, "{name}");
        }
        assert_eq!(kind("shop.laptop.localhost", &settings), NameKind::Peer);
        for name in [
            "r1.localhost",
            "x.web.localhost",
            "api.localhost",
            "laptop.localhost",
        ] {
            assert_eq!(kind(name, &settings), NameKind::Unknown, "{name}");
        }
        settings.wildcard = true;
        assert_eq!(kind("x.web.localhost", &settings), NameKind::Local);
    }
    #[test]
    fn made_up_peer_names_cannot_starve_local_apps_or_the_dashboard() {
        let mut resolver = test_resolver();
        resolver.known = |n, s| known_in(n, s, &["web".to_owned()], &["mini".to_owned()]);
        // Any name under a paired machine looks like one of its apps, so a
        // flood of invented ones exhausts the peer budget...
        let refused = (0..40)
            .filter(|i| {
                resolver
                    .resolve_name(&format!("r{i}.mini.localhost"))
                    .is_none()
            })
            .count();
        assert!(refused >= 10, "peer names are rate limited");
        assert!(resolver.resolve_name("r99.mini.localhost").is_none());
        // ...but not the local one: first-time handshakes to a real app and
        // to the dashboard still get certificates.
        assert!(resolver.resolve_name("web.localhost").is_some());
        assert!(resolver.resolve_name("porch.localhost").is_some());
    }
    #[test]
    fn tls_policy_rejects_foreign_names() {
        let resolver = test_resolver();
        assert!(resolver.issue("app.localhost").is_ok());
        assert!(
            resolver.issue("www.google.com").is_err(),
            "foreign SNI must never receive a Porch certificate"
        );
    }
    #[test]
    fn tls_policy_leaf_has_server_profile() {
        let resolver = test_resolver();
        let (cert, _) = resolver.issue("app.localhost").unwrap();
        let params = CertificateParams::from_ca_cert_der(&cert.cert[0]).unwrap();
        assert!(
            params
                .extended_key_usages
                .contains(&rcgen::ExtendedKeyUsagePurpose::ServerAuth),
            "leaf must specify serverAuth"
        );
        assert!(
            (params.not_after - params.not_before).whole_days() <= 398,
            "leaf lifetime must be <=398 days"
        );
        assert_eq!(
            params.distinguished_name.get(&rcgen::DnType::CommonName),
            Some(&rcgen::DnValue::Utf8String("app.localhost".into()))
        );
    }
    #[test]
    fn tls_policy_configured_and_registered_domains() {
        let mut settings = crate::state::ProxySettings {
            tlds: vec!["localhost".into(), "dev.test".into()],
            ..Default::default()
        };
        assert!(allowed_name("web.dev.test", &settings, &[]));
        assert!(!allowed_name("dev.test.evil.com", &settings, &[]));
        assert!(!allowed_name("web.local", &settings, &[]));
        settings.lan = true;
        assert!(allowed_name("web.local", &settings, &[]));
        let routes = vec![crate::state::Route {
            name: "registered.example.com/api".into(),
            target: "http://127.0.0.1:3000".into(),
            pid: None,
        }];
        // A route's raw name is never a certificate name outside the TLDs.
        assert!(!allowed_name("registered.example.com", &settings, &routes));
        assert!(!allowed_name("other.example.com", &settings, &routes));
        let github = vec![crate::state::Route {
            name: "github.com".into(),
            target: "http://127.0.0.1:3000".into(),
            pid: None,
        }];
        assert!(!allowed_name("github.com", &settings, &github));
        assert!(allowed_name("github.com.localhost", &settings, &github));
        assert!(!allowed_name("registered.example.com", &settings, &[]));
        for invalid in [
            "*.localhost",
            "-bad.localhost",
            "bad_.localhost",
            "",
            "localhost.",
        ] {
            assert!(!allowed_name(invalid, &settings, &[]));
        }
    }
    #[test]
    fn tls_policy_new_ca_profile_and_legacy_identity_preserved() {
        let path = std::env::temp_dir().join(format!(
            "porch-tls-policy-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        let settings = crate::state::ProxySettings::default();
        config_in_dir(&settings, &path).unwrap();
        let cert = fs::read_to_string(path.join("ca.pem")).unwrap();
        let params = CertificateParams::from_ca_cert_pem(&cert).unwrap();
        assert!((params.not_after - params.not_before).whole_days() <= 3650);
        assert_eq!(params.is_ca, IsCa::Ca(BasicConstraints::Constrained(0)));
        // New CAs sign TLS server certificates only.
        assert!(ca_server_auth_only(&cert));
        // New CAs are name-constrained to never-public names and the TLDs.
        let permitted: Vec<_> = params
            .name_constraints
            .as_ref()
            .expect("new CA has name constraints")
            .permitted_subtrees
            .iter()
            .filter_map(|s| match s {
                rcgen::GeneralSubtree::DnsName(d) => Some(d.as_str()),
                _ => None,
            })
            .collect();
        assert!(permitted.contains(&"localhost") && permitted.contains(&"local"));
        assert!(!permitted.iter().any(|p| *p == "com" || p.is_empty()));
        let excluded = &params.name_constraints.as_ref().unwrap().excluded_subtrees;
        assert_eq!(excluded.len(), 2, "IPv4 and IPv6 are excluded");
        // A bare public TLD is never added to the CA, nor issued for.
        let risky = crate::state::ProxySettings {
            tlds: vec!["com".into(), "dev.example.com".into()],
            ..Default::default()
        };
        assert!(!ca_permitted(&risky, &[]).contains(&"com".to_owned()));
        for public in ["co.uk", "github.io", "com.au", "dev", "app"] {
            assert!(!signable_domain(public), "{public}");
        }
        for owned in [
            "example.co.uk",
            "local.example.com",
            "me.github.io",
            "test",
            "a.test",
        ] {
            assert!(signable_domain(owned), "{owned}");
        }
        assert!(ca_permitted(&risky, &[]).contains(&"dev.example.com".to_owned()));
        assert!(!allowed_name("github.com", &risky, &[]));
        assert!(allowed_name("web.dev.example.com", &risky, &[]));
        assert!(uncovered_tlds(&cert, &settings).is_empty());
        let other = crate::state::ProxySettings {
            tlds: vec!["dev.example.com".into()],
            ..Default::default()
        };
        assert_eq!(uncovered_tlds(&cert, &other), ["dev.example.com"]);
        // A new domain gets a new CA that covers it; the old one is retired.
        config_in_dir(&other, &path).unwrap();
        let rotated = fs::read_to_string(path.join("ca.pem")).unwrap();
        assert_ne!(rotated, cert);
        assert!(uncovered_tlds(&rotated, &other).is_empty());
        assert!(fs::read_dir(&path).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("ca.pem.retired-")));
        // Only the retired certificate is kept, never its private key.
        assert!(!fs::read_dir(&path).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("ca-key.pem.retired-")));
        // Switching to another domain carries the earlier ones over, so going
        // back needs no new CA.
        let third = crate::state::ProxySettings {
            tlds: vec!["b.example.com".into()],
            ..Default::default()
        };
        config_in_dir(&third, &path).unwrap();
        let carried = fs::read_to_string(path.join("ca.pem")).unwrap();
        assert!(permitted_subtrees(&carried).contains(&"dev.example.com".to_owned()));
        config_in_dir(&other, &path).unwrap();
        assert_eq!(fs::read_to_string(path.join("ca.pem")).unwrap(), carried);
        // Older versions kept retired keys: they are deleted.
        fs::write(path.join("ca-key.pem.retired-1"), "old key").unwrap();
        config_in_dir(&settings, &path).unwrap();
        assert!(!path.join("ca-key.pem.retired-1").exists());
        // Install an old profile only in this temporary fixture; no system trust.
        let (legacy_pem, legacy_key) = unconstrained_ca();
        fs::write(path.join("ca.pem"), &legacy_pem).unwrap();
        fs::write(path.join("ca-key.pem"), &legacy_key).unwrap();
        fs::remove_file(path.join("tls-profile-version")).unwrap();
        config_in_dir(&settings, &path).unwrap();
        assert_eq!(fs::read_to_string(path.join("ca.pem")).unwrap(), legacy_pem);
        assert_eq!(
            fs::read_to_string(path.join("ca-key.pem")).unwrap(),
            legacy_key
        );
        assert!(!ca_server_auth_only(&legacy_pem));
        assert_eq!(
            fs::read_to_string(path.join("tls-profile-version")).unwrap(),
            "3\n"
        );
        fs::remove_dir_all(path).unwrap();
    }
    #[tokio::test]
    async fn tls_policy_foreign_sni_handshake_rejected() {
        let resolver = test_resolver();
        let mut roots = rustls::RootCertStore::empty();
        for cert in crate::pem::certs(&mut BufReader::new(resolver.issuer.ca_pem.as_bytes())) {
            roots.add(cert.unwrap()).unwrap();
        }
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let server = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(resolver));
        let client = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        // Each handshake runs over its own in-memory pipe. A loopback listener
        // would accept any connection on its port, and a parallel test probing a
        // port it freed (and the OS reassigned here) once took the place of the
        // positive control below.
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server));
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client));
        let handshake = |name: &'static str| {
            let (acceptor, connector) = (acceptor.clone(), connector.clone());
            async move {
                let (client, server) = tokio::io::duplex(64 * 1024);
                let (accepted, connected) = tokio::join!(
                    acceptor.accept(server),
                    connector.connect(name.try_into().unwrap(), client)
                );
                (accepted.is_ok(), connected.is_ok())
            }
        };
        // Positive control proves this is policy rejection rather than a broken
        // root or leaf profile.
        assert_eq!(
            handshake("app.localhost").await,
            (true, true),
            "configured local name must complete TLS"
        );
        assert_eq!(handshake("www.google.com").await, (false, false));
        // A route named like a real domain doesn't get a certificate for it.
        assert_eq!(handshake("registered.example.com").await, (false, false));
    }
    #[test]
    fn incomplete_or_damaged_ca_pair_recovers() {
        let path = std::env::temp_dir().join(format!(
            "porch-tls-recover-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        let settings = crate::state::ProxySettings::default();
        // Interrupted creation: a key with no certificate is replaced.
        fs::write(path.join("ca-key.pem"), "partial").unwrap();
        config_in_dir(&settings, &path).unwrap();
        let first = fs::read_to_string(path.join("ca.pem")).unwrap();
        // A certificate whose key is gone is retired (for trust removal) and replaced.
        fs::remove_file(path.join("ca-key.pem")).unwrap();
        config_in_dir(&settings, &path).unwrap();
        assert_ne!(fs::read_to_string(path.join("ca.pem")).unwrap(), first);
        let retired: Vec<_> = fs::read_dir(&path)
            .unwrap()
            .filter_map(|e| {
                let n = e.unwrap().file_name().to_string_lossy().into_owned();
                n.starts_with("ca.pem.retired-").then_some(n)
            })
            .collect();
        assert_eq!(retired.len(), 1);
        assert_eq!(fs::read_to_string(path.join(&retired[0])).unwrap(), first);
        // A damaged pair says how to recover instead of a bare parse error.
        fs::write(path.join("ca-key.pem"), "garbage").unwrap();
        let err = config_in_dir(&settings, &path).unwrap_err();
        assert!(format!("{err:#}").contains("is damaged"), "{err:#}");
        // No temporary files are left behind.
        assert!(!fs::read_dir(&path).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")));
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn rejects_invalid_pem() {
        assert!(certified("invalid", "invalid").is_err());
    }
}
