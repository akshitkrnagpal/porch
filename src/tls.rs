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
fn write_private(path: &Path, value: &str) -> Result<()> {
    use std::io::Write;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(value.as_bytes())?;
    file.sync_all()?;
    crate::state::inherit_owner(path)?;
    Ok(())
}
fn certified(cert: &str, key: &str) -> Result<Arc<CertifiedKey>> {
    let chain = rustls_pemfile::certs(&mut BufReader::new(cert.as_bytes()))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let key = rustls_pemfile::private_key(&mut BufReader::new(key.as_bytes()))?
        .context("No private key in PEM")?;
    let provider = rustls::crypto::ring::default_provider();
    Ok(Arc::new(CertifiedKey::from_der(chain, key, &provider)?))
}
#[derive(Debug)]
struct LocalResolver {
    ca_pem: String,
    ca_key: String,
    cache: Mutex<HashMap<String, Arc<CertifiedKey>>>,
    settings: crate::state::ProxySettings,
    route_state: PathBuf,
}
impl LocalResolver {
    fn authorize(&self, name: &str) -> Result<()> {
        if !allowed_name(name, &self.settings, &[]) {
            let routes = match fs::read(&self.route_state) {
                Ok(bytes) => {
                    serde_json::from_slice::<crate::state::State>(&bytes)
                        .context("invalid route state for TLS issuance")?
                        .routes
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                Err(error) => return Err(error.into()),
            };
            if !allowed_name(name, &self.settings, &routes) {
                bail!("TLS name {name:?} is outside Porch's configured domains");
            }
        }
        Ok(())
    }
    fn issue(&self, name: &str) -> Result<Arc<CertifiedKey>> {
        self.authorize(name)?;
        let ca_key = KeyPair::from_pem(&self.ca_key)?;
        let ca = CertificateParams::from_ca_cert_pem(&self.ca_pem)?.self_signed(&ca_key)?;
        let key = KeyPair::generate()?;
        let mut params = CertificateParams::new(vec![name.to_owned()])?;
        set_validity(&mut params, 365);
        params.not_after = params.not_after.min(ca.params().not_after);
        if params.not_after <= SystemTime::now() {
            bail!("Local CA has expired; explicitly replace and trust a new CA");
        }
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, name);
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.use_authority_key_identifier_extension = true;
        let cert = params.signed_by(&key, &ca, &ca_key)?;
        certified(
            &format!("{}{}", cert.pem(), self.ca_pem),
            &key.serialize_pem(),
        )
    }
}
impl ResolvesServerCert for LocalResolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let name = hello
            .server_name()
            .unwrap_or("localhost")
            .to_ascii_lowercase();
        self.authorize(&name).ok()?;
        let mut cache = self.cache.lock().ok()?;
        if let Some(cert) = cache.get(&name) {
            return Some(cert.clone());
        }
        let cert = self.issue(&name).ok()?;
        if cache.len() >= 256 {
            cache.clear();
        }
        cache.insert(name, cert.clone());
        Some(cert)
    }
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
    let in_domain = |domain: &str| name == domain || name.ends_with(&format!(".{domain}"));
    name == "localhost"
        || settings
            .tlds
            .iter()
            .any(|tld| in_domain(tld.trim_start_matches('.')))
        || (settings.lan && in_domain("local"))
        || routes.iter().any(|route| {
            let domain = route.name.split('/').next().unwrap_or("");
            if let Some(suffix) = domain.strip_prefix("*.") {
                name.ends_with(&format!(".{suffix}"))
            } else {
                name == domain
            }
        })
}
fn set_validity(params: &mut CertificateParams, days: u64) {
    // A short backdate tolerates normal clock skew without extending the profile.
    let start = SystemTime::now() - Duration::from_secs(300);
    params.not_before = start.into();
    params.not_after = (start + Duration::from_secs(days * 86_400)).into();
}
fn ca_params() -> Result<CertificateParams> {
    let mut params = CertificateParams::new(Vec::<String>::new())?;
    set_validity(&mut params, 3650);
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "Porch Local CA");
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    Ok(params)
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
            if !cert_path.exists() && !key_path.exists() {
                let key = KeyPair::generate()?;
                let params = ca_params()?;
                let ca = params.self_signed(&key)?;
                write_private(&key_path, &key.serialize_pem())?;
                write_private(&cert_path, &ca.pem())?;
            }
            let resolver = LocalResolver {
                ca_pem: fs::read_to_string(cert_path)?,
                ca_key: fs::read_to_string(key_path)?,
                cache: Mutex::new(HashMap::new()),
                settings: settings.clone(),
                route_state: dir.join("state.json"),
            };
            resolver.issue("localhost").context("Invalid local CA")?;
            // There are no persisted leaf certificates: profile v2 takes effect on
            // every new handshake after restart. Preserve an existing root and key
            // byte-for-byte, since silently rotating them would invalidate trust.
            let profile = dir.join("tls-profile-version");
            if !profile.exists() {
                let ca = CertificateParams::from_ca_cert_pem(&resolver.ca_pem)?;
                if (ca.not_after - ca.not_before).whole_days() > 3653 {
                    eprintln!("Porch TLS: retained your existing CA to preserve system trust; new certificates use the restricted v2 server profile. Its original CA validity remains unchanged; replacing a trusted root requires an explicit cleanup and trust setup.");
                }
                write_private(&profile, "2\n")?;
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
    #[test]
    fn generates_signed_leaf() {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca = params.self_signed(&key).unwrap();
        let resolver = LocalResolver {
            ca_pem: ca.pem(),
            ca_key: key.serialize_pem(),
            cache: Mutex::new(HashMap::new()),
            settings: crate::state::ProxySettings::default(),
            route_state: std::env::temp_dir()
                .join(format!("porch-no-route-state-{}", std::process::id())),
        };
        assert!(resolver.issue("app.localhost").is_ok());
        assert!(resolver.issue("*.example.test").is_err());
    }
    fn test_resolver() -> LocalResolver {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca = params.self_signed(&key).unwrap();
        LocalResolver {
            ca_pem: ca.pem(),
            ca_key: key.serialize_pem(),
            cache: Mutex::new(HashMap::new()),
            settings: crate::state::ProxySettings::default(),
            route_state: std::env::temp_dir()
                .join(format!("porch-no-route-state-{}", std::process::id())),
        }
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
        let cert = resolver.issue("app.localhost").unwrap();
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
        assert!(allowed_name("registered.example.com", &settings, &routes));
        assert!(!allowed_name("other.example.com", &settings, &routes));
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
        // Install an old profile only in this temporary fixture; no system trust.
        let resolver = test_resolver();
        fs::write(path.join("ca.pem"), &resolver.ca_pem).unwrap();
        fs::write(path.join("ca-key.pem"), &resolver.ca_key).unwrap();
        fs::remove_file(path.join("tls-profile-version")).unwrap();
        config_in_dir(&settings, &path).unwrap();
        assert_eq!(
            fs::read_to_string(path.join("ca.pem")).unwrap(),
            resolver.ca_pem
        );
        assert_eq!(
            fs::read_to_string(path.join("ca-key.pem")).unwrap(),
            resolver.ca_key
        );
        assert_eq!(
            fs::read_to_string(path.join("tls-profile-version")).unwrap(),
            "2\n"
        );
        fs::remove_dir_all(path).unwrap();
    }
    #[tokio::test]
    async fn tls_policy_foreign_sni_handshake_rejected() {
        let mut resolver = test_resolver();
        let path = std::env::temp_dir().join(format!(
            "porch-tls-handshake-{}-{}.json",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        resolver.route_state = path.clone();
        let mut roots = rustls::RootCertStore::empty();
        for cert in rustls_pemfile::certs(&mut BufReader::new(resolver.ca_pem.as_bytes())) {
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
        fs::write(
            &path,
            serde_json::to_vec(&crate::state::State {
                routes: vec![crate::state::Route {
                    name: "registered.example.com".into(),
                    target: "http://127.0.0.1:3000".into(),
                    pid: None,
                }],
                ..Default::default()
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(handshake("registered.example.com").await, (true, true));
        fs::remove_file(path).unwrap();
        assert_eq!(
            handshake("registered.example.com").await,
            (false, false),
            "cached certificate must not survive route removal"
        );
    }
    #[test]
    fn rejects_invalid_pem() {
        assert!(certified("invalid", "invalid").is_err());
    }
}
