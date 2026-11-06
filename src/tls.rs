//! Local CA and SNI certificates. This module never changes system trust.
use anyhow::{bail, Context, Result};
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair, KeyUsagePurpose};
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
    options.open(path)?.write_all(value.as_bytes())?;
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
}
impl LocalResolver {
    fn issue(&self, name: &str) -> Result<Arc<CertifiedKey>> {
        let ca_key = KeyPair::from_pem(&self.ca_key)?;
        let ca = CertificateParams::from_ca_cert_pem(&self.ca_pem)?.self_signed(&ca_key)?;
        let key = KeyPair::generate()?;
        let params = CertificateParams::new(vec![name.to_owned()])?;
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
pub fn config(cert: Option<PathBuf>, key: Option<PathBuf>) -> Result<Arc<rustls::ServerConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_no_client_auth();
    let mut config = match (cert, key) {
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
            let dir = crate::state::dir();
            fs::create_dir_all(&dir)?;
            let cert_path = dir.join("ca.pem");
            let key_path = dir.join("ca-key.pem");
            if !cert_path.exists() && !key_path.exists() {
                let key = KeyPair::generate()?;
                let mut params = CertificateParams::new(Vec::<String>::new())?;
                params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
                params
                    .distinguished_name
                    .push(rcgen::DnType::CommonName, "Porch Local CA");
                params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
                let ca = params.self_signed(&key)?;
                write_private(&key_path, &key.serialize_pem())?;
                write_private(&cert_path, &ca.pem())?;
            }
            let resolver = LocalResolver {
                ca_pem: fs::read_to_string(cert_path)?,
                ca_key: fs::read_to_string(key_path)?,
                cache: Mutex::new(HashMap::new()),
            };
            resolver.issue("localhost").context("Invalid local CA")?;
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
        };
        assert!(resolver.issue("app.localhost").is_ok());
        assert!(resolver.issue("*.example.test").is_ok());
    }
    #[test]
    fn rejects_invalid_pem() {
        assert!(certified("invalid", "invalid").is_err());
    }
}
