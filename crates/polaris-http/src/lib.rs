//! Shared HTTP client setup.
//!
//! An explicitly supplied `SSL_CERT_FILE` augments, rather than replaces,
//! reqwest's built-in roots. TLS chain and hostname verification stay enabled.

use std::path::Path;

/// Builds a reqwest client builder with any explicitly supplied CA bundle.
///
/// When `SSL_CERT_FILE` is absent, reqwest's normal trust configuration is
/// preserved. When it is present, it must name a non-empty, valid PEM bundle;
/// otherwise client construction fails before a request can be made.
pub fn client_builder() -> Result<reqwest::ClientBuilder, ClientConfigError> {
    match std::env::var_os("SSL_CERT_FILE") {
        Some(path) if path.is_empty() => Err(ClientConfigError::EmptyPath),
        Some(path) => client_builder_with_ca_file(Path::new(&path)),
        None => Ok(reqwest::Client::builder()),
    }
}

/// Builds a client builder augmented with every certificate in `path`.
///
/// This is public so callers that obtain an explicit compatibility bundle by
/// another configuration route can retain the same fail-closed behavior.
pub fn client_builder_with_ca_file(
    path: impl AsRef<Path>,
) -> Result<reqwest::ClientBuilder, ClientConfigError> {
    let pem = std::fs::read(path).map_err(|_| ClientConfigError::UnreadableBundle)?;
    if pem.is_empty() {
        return Err(ClientConfigError::EmptyBundle);
    }

    let certificates = reqwest::Certificate::from_pem_bundle(&pem)
        .map_err(|_| ClientConfigError::InvalidBundle)?;
    if certificates.is_empty() {
        return Err(ClientConfigError::EmptyBundle);
    }

    Ok(certificates
        .into_iter()
        .fold(reqwest::Client::builder(), |builder, certificate| {
            builder.add_root_certificate(certificate)
        }))
}

/// Fail-closed configuration errors for an explicitly supplied CA bundle.
#[derive(Debug, thiserror::Error)]
pub enum ClientConfigError {
    #[error("SSL_CERT_FILE is empty")]
    EmptyPath,
    #[error("SSL_CERT_FILE could not be read")]
    UnreadableBundle,
    #[error("SSL_CERT_FILE contains no certificates")]
    EmptyBundle,
    #[error("SSL_CERT_FILE is not a valid PEM certificate bundle")]
    InvalidBundle,
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::Arc;

    use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;

    use super::*;

    const ROOT: &str = include_str!("fixtures/root.pem");
    const CHAIN: &str = include_str!("fixtures/chain.pem");
    const KEY: &str = include_str!("fixtures/end.key");

    async fn tls_server() -> SocketAddr {
        let chain = CertificateDer::pem_slice_iter(CHAIN.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .expect("test fixture certificate chain");
        let key = PrivateKeyDer::from_pem_slice(KEY.as_bytes()).expect("test fixture private key");
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .expect("matching test fixture key");
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind local TLS fixture");
        let address = listener.local_addr().expect("local TLS fixture address");

        tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(mut stream) = acceptor.accept(stream).await else {
                return;
            };
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).await;
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
                .await;
        });

        address
    }

    #[tokio::test]
    async fn unknown_ca_is_denied_and_bundle_trusts_every_certificate() {
        let address = tls_server().await;
        let untrusted = reqwest::Client::builder()
            .resolve("foobar.com", address)
            .build()
            .expect("client");
        let error = untrusted
            .get("https://foobar.com/")
            .send()
            .await
            .expect_err("the fixture CA must not be trusted by default");
        assert!(
            error.is_connect(),
            "expected TLS connection failure: {error}"
        );

        let address = tls_server().await;
        let directory = tempfile::tempdir().expect("temporary CA bundle directory");
        let bundle_path = directory.path().join("ca-bundle.pem");
        // The first two certificates do not anchor the fixture chain. The
        // root comes last, so this catches an implementation that reads only
        // the first PEM block instead of the complete supplied bundle.
        std::fs::write(&bundle_path, format!("{CHAIN}{ROOT}")).expect("write test CA bundle");
        let trusted = client_builder_with_ca_file(&bundle_path)
            .expect("valid CA bundle")
            .resolve("foobar.com", address)
            .build()
            .expect("client");
        let response = trusted
            .get("https://foobar.com/")
            .send()
            .await
            .expect("supplied CA bundle should be trusted");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
    }

    #[test]
    fn invalid_or_empty_explicit_bundle_fails_closed() {
        let directory = tempfile::tempdir().expect("temporary CA bundle directory");
        let empty = directory.path().join("empty.pem");
        let invalid = directory.path().join("invalid.pem");
        std::fs::write(&empty, b"").expect("write empty fixture");
        std::fs::write(&invalid, b"not a PEM certificate").expect("write invalid fixture");

        assert!(matches!(
            client_builder_with_ca_file(&empty),
            Err(ClientConfigError::EmptyBundle)
        ));
        assert!(matches!(
            client_builder_with_ca_file(&invalid),
            Err(ClientConfigError::EmptyBundle | ClientConfigError::InvalidBundle)
        ));
        assert!(matches!(
            client_builder_with_ca_file(directory.path().join("missing.pem")),
            Err(ClientConfigError::UnreadableBundle)
        ));
    }
}
