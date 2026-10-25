//! Mandatory CA-key rejection for credential-free environment artifacts.
//! WebPKI checks each candidate *verified* path, including its trust anchor;
//! path building may choose another path only if it also passes this check.
//! Chain validity, DNS/IP identity, and TLS handshake signatures remain checked.
use crate::ClientConfigError;
use rustls::{
    DigitallySignedStruct, Error, RootCertStore, SignatureScheme,
    client::{
        WebPkiServerVerifier,
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    },
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use sha2::{Digest, Sha256};
use std::sync::Arc;

mod keys;

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// TrustAnchor stores the contents of SubjectPublicKeyInfo, not its outer DER
/// SEQUENCE. Restore that canonical wrapper before comparing full-SPKI hashes.
fn anchor_spki(contents: &[u8]) -> Vec<u8> {
    let mut der = vec![0x30];
    if contents.len() < 128 {
        der.push(contents.len() as u8);
    } else {
        let length = contents.len().to_be_bytes();
        let start = length
            .iter()
            .position(|byte| *byte != 0)
            .unwrap_or(length.len() - 1);
        der.push(0x80 | (length.len() - start) as u8);
        der.extend_from_slice(&length[start..]);
    }
    der.extend_from_slice(contents);
    der
}

fn blocked_key(spki: &[u8]) -> bool {
    keys::BLOCKED_SPKI.contains(&digest(spki).as_str())
}

fn blocked_certificate(cert: &CertificateDer<'_>) -> Result<bool, webpki::Error> {
    let parsed = webpki::EndEntityCert::try_from(cert)?;
    Ok(keys::BLOCKED_DER.contains(&digest(cert.as_ref()).as_str())
        || blocked_key(parsed.subject_public_key_info().as_ref()))
}

#[derive(Debug)]
struct Verifier {
    roots: Arc<RootCertStore>,
    provider: Arc<rustls::crypto::CryptoProvider>,
    signatures: Arc<WebPkiServerVerifier>,
    #[cfg(test)]
    additionally_rejected: Vec<String>,
}

impl Verifier {
    fn new(roots: RootCertStore) -> Result<Arc<Self>, ClientConfigError> {
        let roots = Arc::new(roots);
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let signatures =
            WebPkiServerVerifier::builder_with_provider(roots.clone(), provider.clone())
                .build()
                .map_err(|_| ClientConfigError::UnavailableVerifier)?;
        Ok(Arc::new(Self {
            roots,
            provider,
            signatures,
            #[cfg(test)]
            additionally_rejected: Vec::new(),
        }))
    }

    fn rejects_spki(&self, bytes: &[u8]) -> bool {
        if blocked_key(bytes) {
            return true;
        }
        #[cfg(test)]
        if self.additionally_rejected.contains(&digest(bytes)) {
            return true;
        }
        false
    }
}

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        let invalid =
            |_| Error::InvalidCertificate(rustls::CertificateError::ApplicationVerificationFailure);
        let cert = webpki::EndEntityCert::try_from(end_entity).map_err(invalid)?;
        let reject = |path: &webpki::VerifiedPath<'_>| {
            if blocked_certificate(end_entity)?
                || self.rejects_spki(&anchor_spki(path.anchor().subject_public_key_info.as_ref()))
            {
                return Err(webpki::Error::CertRevoked);
            }
            for intermediate in path.intermediate_certificates() {
                if blocked_certificate(&intermediate.der())?
                    || self.rejects_spki(intermediate.subject_public_key_info().as_ref())
                {
                    return Err(webpki::Error::CertRevoked);
                }
            }
            Ok(())
        };
        cert.verify_for_usage(
            self.provider.signature_verification_algorithms.all,
            &self.roots.roots,
            intermediates,
            now,
            webpki::KeyUsage::server_auth(),
            None,
            Some(&reject),
        )
        .map_err(invalid)?;
        cert.verify_is_valid_for_subject_name(server_name)
            .map_err(invalid)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.signatures.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.signatures.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.signatures.supported_verify_schemes()
    }
}

fn configuration(roots: RootCertStore) -> Result<rustls::ClientConfig, ClientConfigError> {
    let verifier = Verifier::new(roots)?;
    configuration_with_verifier(verifier)
}

fn configuration_with_verifier(
    verifier: Arc<Verifier>,
) -> Result<rustls::ClientConfig, ClientConfigError> {
    let mut config = rustls::ClientConfig::builder_with_provider(verifier.provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|_| ClientConfigError::UnavailableVerifier)?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    // Do not inherit TLS key logging or client certificates from the process.
    config.key_log = Arc::new(rustls::NoKeyLog);
    Ok(config)
}

pub fn client_builder() -> Result<reqwest::ClientBuilder, ClientConfigError> {
    let roots = RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    Ok(reqwest::Client::builder().use_preconfigured_tls(configuration(roots)?))
}

#[cfg(test)]
mod tests;
