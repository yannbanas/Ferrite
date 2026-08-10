//! Transport security.
//!
//! TLS is on by default and is not a feature flag: [`TlsMode::Disabled`]
//! has to be named explicitly, which is the only way to get a listener
//! that accepts a cleartext session. When TLS is required, a client that
//! skips `SSLRequest` and sends `StartupMessage` straight away is refused
//! with a fatal `ErrorResponse` — the password mechanism in
//! [`crate::auth`] assumes the channel is encrypted.

use std::path::Path;
use std::sync::Arc;

use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::ServerConfig as RustlsServerConfig;
use tokio_rustls::TlsAcceptor;

use crate::error::{ProtocolError, Result};

/// Whether a listener speaks TLS.
#[derive(Clone)]
pub enum TlsMode {
    /// The default. `SSLRequest` is accepted and a cleartext startup is
    /// refused.
    Required(TlsAcceptor),
    /// Cleartext only, for tests and for a listener already behind a
    /// trusted local transport. Passwords cross the wire unprotected.
    Disabled,
}

impl std::fmt::Debug for TlsMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TlsMode::Required(_) => f.write_str("TlsMode::Required"),
            TlsMode::Disabled => f.write_str("TlsMode::Disabled"),
        }
    }
}

impl TlsMode {
    /// Builds a TLS-required mode from a certificate chain and private key
    /// already in memory.
    pub fn from_der(
        chain: Vec<CertificateDer<'static>>,
        key: PrivateKeyDer<'static>,
    ) -> Result<Self> {
        // Pinned to *ring* rather than the process-default provider, so a
        // downstream crate enabling a second rustls backend cannot change
        // which one Ferrite's listener ends up using.
        let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
        let config = RustlsServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| ProtocolError::Tls(e.to_string()))?
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .map_err(|e| ProtocolError::Tls(e.to_string()))?;
        Ok(TlsMode::Required(TlsAcceptor::from(Arc::new(config))))
    }

    /// Builds a TLS-required mode from raw DER, so a caller does not need a
    /// rustls dependency of its own just to name the key type. The key must
    /// be PKCS#8.
    pub fn from_pkcs8_der(chain: Vec<Vec<u8>>, key: Vec<u8>) -> Result<Self> {
        Self::from_der(
            chain.into_iter().map(CertificateDer::from).collect(),
            PrivateKeyDer::Pkcs8(key.into()),
        )
    }

    /// Loads a PEM certificate chain and private key off disk.
    pub fn from_pem_files(cert: impl AsRef<Path>, key: impl AsRef<Path>) -> Result<Self> {
        let chain = load_certs(cert.as_ref())?;
        let key = load_key(key.as_ref())?;
        Self::from_der(chain, key)
    }

    pub fn is_required(&self) -> bool {
        matches!(self, TlsMode::Required(_))
    }

    pub(crate) fn acceptor(&self) -> Option<&TlsAcceptor> {
        match self {
            TlsMode::Required(acceptor) => Some(acceptor),
            TlsMode::Disabled => None,
        }
    }
}

fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    // `rustls_pemfile` is gone (RUSTSEC-2025-0134, unmaintained): PEM
    // parsing for certificates/keys lives in `rustls-pki-types` itself
    // now, already a transitive dependency of `rustls`.
    let certs: std::result::Result<Vec<_>, _> = CertificateDer::pem_file_iter(path)
        .map_err(|err| ProtocolError::Tls(format!("{}: {err}", path.display())))?
        .collect();
    let certs = certs.map_err(|err| ProtocolError::Tls(format!("{}: {err}", path.display())))?;
    if certs.is_empty() {
        return Err(ProtocolError::Tls(format!(
            "{} contains no certificate",
            path.display()
        )));
    }
    Ok(certs)
}

fn load_key(path: &Path) -> Result<PrivateKeyDer<'static>> {
    PrivateKeyDer::from_pem_file(path)
        .map_err(|err| ProtocolError::Tls(format!("{}: {err}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tls_is_required_unless_explicitly_disabled() {
        assert!(!TlsMode::Disabled.is_required());
        assert!(TlsMode::Disabled.acceptor().is_none());
    }

    #[test]
    fn missing_pem_files_are_reported_not_panicked() {
        let err = TlsMode::from_pem_files("no-such.crt", "no-such.key");
        assert!(err.is_err());
    }
}
