//! PEM decoding for certificates and keys, on rustls-pki-types' own parser
//! (the separate rustls-pemfile crate is unmaintained). Same shape as the
//! rustls-pemfile functions it replaces.
use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
use std::io::{self, BufRead};

fn read_all(rd: &mut dyn BufRead) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    rd.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Every certificate in the PEM input.
pub fn certs(rd: &mut dyn BufRead) -> impl Iterator<Item = io::Result<CertificateDer<'static>>> {
    let items: Vec<io::Result<CertificateDer<'static>>> = match read_all(rd) {
        Ok(bytes) => CertificateDer::pem_slice_iter(&bytes)
            .map(|c| c.map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string())))
            .collect(),
        Err(e) => vec![Err(e)],
    };
    items.into_iter()
}

/// The first private key in the PEM input, if any.
pub fn private_key(rd: &mut dyn BufRead) -> io::Result<Option<PrivateKeyDer<'static>>> {
    let bytes = read_all(rd)?;
    match PrivateKeyDer::from_pem_slice(&bytes) {
        Ok(key) => Ok(Some(key)),
        Err(rustls::pki_types::pem::Error::NoItemsFound) => Ok(None),
        Err(e) => Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_certs_and_keys() {
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["a.localhost".into()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        let pem = format!("{}{}", cert.pem(), cert.pem());
        let certs: Vec<_> = super::certs(&mut pem.as_bytes()).collect();
        assert_eq!(certs.len(), 2);
        assert!(certs.iter().all(|c| c.is_ok()));
        assert!(super::private_key(&mut key.serialize_pem().as_bytes())
            .unwrap()
            .is_some());
        assert!(super::private_key(&mut cert.pem().as_bytes())
            .unwrap()
            .is_none());
    }
}
