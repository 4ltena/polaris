//! Verify actual-path certificate distrust and rejection before HTTP bytes.
use super::*;
use rustls::pki_types::{PrivateKeyDer, pem::PemObject};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn fixture_roots() -> RootCertStore {
    let mut roots = RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(include_bytes!("../fixtures/root.pem")) {
        roots.add(cert.unwrap()).unwrap();
    }
    roots
}

fn fixture_chain() -> Vec<CertificateDer<'static>> {
    CertificateDer::pem_slice_iter(include_bytes!("../fixtures/chain.pem"))
        .map(|cert| cert.unwrap())
        .collect()
}

#[test]
fn anchor_hash_uses_the_complete_der_spki() {
    let roots = fixture_roots();
    let der = CertificateDer::pem_slice_iter(include_bytes!("../fixtures/root.pem"))
        .next()
        .unwrap()
        .unwrap();
    let cert = webpki::EndEntityCert::try_from(&der).unwrap();
    assert_eq!(
        anchor_spki(roots.roots[0].subject_public_key_info.as_ref()),
        cert.subject_public_key_info().as_ref()
    );
}

#[test]
fn validity_and_hostname_are_required_even_for_a_trusted_chain() {
    let verifier = Verifier::new(fixture_roots()).unwrap();
    let chain = fixture_chain();
    let name = ServerName::try_from("foobar.com").unwrap();
    let now = UnixTime::now();
    verifier
        .verify_server_cert(&chain[0], &chain[1..], &name, &[], now)
        .unwrap();
    assert!(
        verifier
            .verify_server_cert(
                &chain[0],
                &chain[1..],
                &ServerName::try_from("wrong.invalid").unwrap(),
                &[],
                now
            )
            .is_err()
    );
    assert!(
        verifier
            .verify_server_cert(&CertificateDer::from(vec![0]), &[], &name, &[], now)
            .is_err()
    );
    let unknown = Verifier::new(RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    })
    .unwrap();
    assert!(
        unknown
            .verify_server_cert(&chain[0], &chain[1..], &name, &[], now)
            .is_err()
    );
}

#[tokio::test]
async fn a_rejected_verified_anchor_sends_no_http_bytes() {
    let chain = fixture_chain();
    let key = PrivateKeyDer::from_pem_slice(include_bytes!("../fixtures/end.key")).unwrap();
    let server = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(chain, key)
    .unwrap();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let reads = Arc::new(AtomicUsize::new(0));
    let observed = reads.clone();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        if let Ok(mut stream) = tokio_rustls::TlsAcceptor::from(Arc::new(server))
            .accept(stream)
            .await
        {
            let mut bytes = [0; 1024];
            let count = stream.read(&mut bytes).await.unwrap_or(0);
            observed.fetch_add(count, Ordering::SeqCst);
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
                .await;
        }
    });
    let roots = fixture_roots();
    let rejected = digest(&anchor_spki(
        roots.roots[0].subject_public_key_info.as_ref(),
    ));
    let mut verifier = Verifier::new(roots).unwrap();
    Arc::get_mut(&mut verifier)
        .unwrap()
        .additionally_rejected
        .push(rejected);
    let client = reqwest::Client::builder()
        .no_proxy()
        .resolve("foobar.com", address)
        .use_preconfigured_tls(configuration_with_verifier(verifier).unwrap())
        .build()
        .unwrap();
    assert!(client.get("https://foobar.com/").send().await.is_err());
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reads.load(Ordering::SeqCst), 0);
}

#[test]
fn known_and_reissued_compromised_keys_are_both_rejected() {
    for der in compromised_certificates() {
        let cert = CertificateDer::from(der);
        assert!(blocked_certificate(&cert).unwrap());
        // Changing only signature bytes changes the DER fingerprint, but not
        // the parsed public key. This tests identity rejection, not signature
        // validity; normal TLS verification is tested separately above.
        let mut altered = der.to_vec();
        *altered.last_mut().unwrap() ^= 1;
        assert!(!keys::BLOCKED_DER.contains(&digest(&altered).as_str()));
        assert!(blocked_certificate(&CertificateDer::from(altered)).unwrap());
    }
}

fn compromised_certificates() -> Vec<&'static [u8]> {
    vec![
        // Public incident certificates; never added to production trust roots.
        include_bytes!(
            "fixtures/0164d7ceae5bb909c272af151ff44b1177ae9b47d9424b94f1d9558c30934d19.der"
        ),
        include_bytes!(
            "fixtures/42c4accd12c4f59597925e1a08a7a90102608543809308d50ec427e118ba1493.der"
        ),
        include_bytes!(
            "fixtures/432ec25f0d28de9006d97bf752f17bbb2cd41f669933fcb072d9a1148d0261f3.der"
        ),
        include_bytes!(
            "fixtures/45a883f349aedfd5f3a649b5498ffb121712ba8b448f3f2a91dbc5a766fcddbe.der"
        ),
        include_bytes!(
            "fixtures/4b2460a466f95fd40113bcdcca2ceb29b3b093921de45474da7df48874c630b1.der"
        ),
        include_bytes!(
            "fixtures/579480aea166290bd3c643589e795fc0d0e54e86505d180f1dff99efd3199512.der"
        ),
        include_bytes!(
            "fixtures/5f1ded4dd590af74631290cf073b122a2aac6a1fa479778a0bd43579bd832699.der"
        ),
        include_bytes!(
            "fixtures/62b7f9868263f72f6911fc78ceaf72c1979158b7368bf648683bb2aa39b02e72.der"
        ),
        include_bytes!(
            "fixtures/7485b51c0ed192878fa1d2f948fb529ae1842cc5086c4b67f7be5b2039de9489.der"
        ),
        include_bytes!(
            "fixtures/84112972b8e59d14560ae7e7119654474aad119990cf3722bfc1e3b91615e1fd.der"
        ),
        include_bytes!(
            "fixtures/98b896625c0c230adeec3eb2cf6aad5001ecf9156a68644e82374ce5833bc25b.der"
        ),
        include_bytes!(
            "fixtures/9e39de2881273acc225a149c72ba3e0f51424963669ab633ee1dc7df5db067b4.der"
        ),
        include_bytes!(
            "fixtures/a496e23ac0cab8155fc9b117b97eae368efcbb9041aec8804d5a308d2d01c2d8.der"
        ),
        include_bytes!(
            "fixtures/cb6e15165ac802e9b3f993f16c25d41f4e81c21ebde71ae1b012ac00bf9c7c3f.der"
        ),
        include_bytes!(
            "fixtures/e7568fe6e12331f4fd7de61da4c7559d0f8eb192a218d4741a3ba75184333838.der"
        ),
        include_bytes!(
            "fixtures/fd9d5c2dbbd4995ab69b8d50c439d9527b00529199c2fd01b2174eb58f07021f.der"
        ),
    ]
}
