use std::fs;
use std::io::{self, BufWriter, Read, Write};
use std::path::Path;
use std::{fs::File, io::BufReader};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use sha2::{Digest, Sha256};
use thiserror::Error;
use webrtc_dtls::crypto::Certificate;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Dtls(#[from] webrtc_dtls::Error),
}

pub fn generate_fingerprint(cert: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(cert);
    let bytes = hash
        .finalize()
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect::<Vec<_>>();
    bytes.join(":").to_lowercase()
}

pub fn certificate_fingerprint(cert: &Certificate) -> String {
    let certificate = cert.certificate.first().expect("certificate missing");
    generate_fingerprint(certificate)
}

/// load certificate from file
pub fn load_certificate(path: &Path) -> Result<Certificate, Error> {
    let f = File::open(path)?;

    let mut reader = BufReader::new(f);
    let mut pem = String::new();
    reader.read_to_string(&mut pem)?;
    Ok(Certificate::from_pem(pem.as_str())?)
}

pub(crate) fn load_or_generate_key_and_cert(path: &Path) -> Result<Certificate, Error> {
    if path.exists() && path.is_file() {
        Ok(load_certificate(path)?)
    } else {
        generate_key_and_cert(path)
    }
}

/// Load the same on-disk PEM that DTLS uses and return the cert chain +
/// private key in the shapes rustls (and therefore quinn) expects. Used by the
/// QUIC file-transfer side channel so it shares one identity + fingerprint
/// with the main DTLS listener.
#[cfg(feature = "file_drop")]
pub(crate) fn load_rustls_cert_and_key(
    path: &Path,
) -> Result<
    (
        Vec<rustls::pki_types::CertificateDer<'static>>,
        rustls::pki_types::PrivateKeyDer<'static>,
    ),
    Error,
> {
    // webrtc-dtls emits PEM blocks with non-standard labels (`PRIVATE_KEY`
    // with an underscore, not the `PRIVATE KEY` rustls_pemfile recognizes),
    // so parse blocks manually via the `pem` crate that webrtc-dtls also
    // uses, then re-wrap into rustls types. PKCS#8 is what webrtc-dtls/rcgen
    // produces, so treating any "key-ish" block as PKCS#8 is sound.
    let mut raw = Vec::new();
    File::open(path)?.read_to_end(&mut raw)?;
    let blocks = pem::parse_many(&raw)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("pem parse: {e}")))?;

    let mut certs: Vec<rustls::pki_types::CertificateDer<'static>> = Vec::new();
    let mut key: Option<rustls::pki_types::PrivateKeyDer<'static>> = None;
    for block in blocks {
        let tag = block.tag().to_ascii_uppercase();
        let contents = block.contents().to_vec();
        if tag.contains("CERTIFICATE") {
            certs.push(rustls::pki_types::CertificateDer::from(contents));
        } else if tag.contains("PRIVATE") && tag.contains("KEY") && key.is_none() {
            let pkcs8 = rustls::pki_types::PrivatePkcs8KeyDer::from(contents);
            key = Some(rustls::pki_types::PrivateKeyDer::Pkcs8(pkcs8));
        }
    }

    if certs.is_empty() {
        return Err(Error::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "no certificates found in PEM",
        )));
    }
    let key = key
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no private key found in PEM"))?;
    Ok((certs, key))
}

pub(crate) fn generate_key_and_cert(path: &Path) -> Result<Certificate, Error> {
    let cert = Certificate::generate_self_signed(["ignored".to_owned()])?;
    let serialized = cert.serialize_pem();
    let parent = path.parent().expect("is a path");
    fs::create_dir_all(parent)?;
    let f = File::create(path)?;
    #[cfg(unix)]
    {
        let mut perm = f.metadata()?.permissions();
        perm.set_mode(0o400); /* r-- --- --- */
        f.set_permissions(perm)?;
    }
    /* FIXME windows permissions */
    let mut writer = BufWriter::new(f);
    writer.write_all(serialized.as_bytes())?;
    Ok(cert)
}
