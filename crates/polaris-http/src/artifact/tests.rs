//! Local TLS fixtures only; no models, user credentials or external services.
use super::*;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::Notify,
    task::JoinHandle,
};
use tokio_rustls::TlsAcceptor;

async fn server(
    responses: Vec<String>,
    stall_body: bool,
) -> (SocketAddr, Arc<Notify>, JoinHandle<Vec<String>>) {
    let chain = CertificateDer::pem_slice_iter(include_bytes!("../fixtures/chain.pem"))
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let key = PrivateKeyDer::from_pem_slice(include_bytes!("../fixtures/end.key")).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(chain, key)
    .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let received = Arc::new(Notify::new());
    let signal = received.clone();
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for response in responses {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = acceptor.accept(stream).await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                assert!(request.len() < 8192);
                request.push(stream.read_u8().await.unwrap());
            }
            requests.push(String::from_utf8(request).unwrap());
            signal.notify_one();
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.flush().await.unwrap();
            if stall_body {
                std::future::pending::<()>().await;
            }
            stream.shutdown().await.unwrap();
        }
        requests
    });
    (address, received, task)
}

fn origin(address: SocketAddr) -> String {
    format!("https://foobar.com:{}", address.port())
}
fn tls_client(address: SocketAddr) -> reqwest::Client {
    client(
        reqwest::Client::builder()
            .resolve("foobar.com", address)
            .add_root_certificate(
                reqwest::Certificate::from_pem(include_bytes!("../fixtures/root.pem")).unwrap(),
            ),
    )
    .unwrap()
}
fn response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}
fn no_credentials(request: &str) {
    let request = request.to_lowercase();
    assert!(!request.contains("\r\nauthorization:"));
    assert!(!request.contains("\r\ncookie:"));
    assert!(!request.contains("\r\nproxy-authorization:"));
    assert!(!request.contains("\r\nreferer:"));
    assert!(request.contains("accept-encoding: identity"));
}

#[test]
fn sources_have_bounded_exact_https_origins_and_no_stored_query_credentials() {
    for url in [
        "http://example.invalid/model",
        "file:///model",
        "https://name:secret@example.invalid/model",
        "https://example.invalid/model?secret=dummy",
        "https://example.invalid/model#part",
        "https://example.invalid/a b",
    ] {
        assert!(ArtifactSource::new(url, &[], 5).is_err());
    }
    for allowed in [
        "https://example.invalid/",
        "http://example.invalid",
        "https://example.invalid/path",
        "https://secret@example.invalid",
    ] {
        assert!(
            ArtifactSource::new("https://example.invalid/model", &[allowed.into()], 5).is_err()
        );
    }
    assert!(ArtifactSource::new("https://example.invalid/model", &[], 0).is_err());
    assert!(
        ArtifactSource::new(
            "https://example.invalid/model",
            &vec!["https://example.invalid".into(); 17],
            5
        )
        .is_err()
    );
    assert!(
        ArtifactSource::new(
            "https://example.invalid/model",
            &["https://cdn.example.invalid".into()],
            5
        )
        .is_ok()
    );
}

#[tokio::test]
async fn fixed_tls_payload_reaches_the_sink_without_authentication_or_model_dispatch() {
    let (address, _, server) = server(vec![response("dummy")], false).await;
    let source = ArtifactSource::new(&format!("{}/artifact", origin(address)), &[], 5).unwrap();
    let mut bytes = Vec::new();
    fetch_with_client(
        &tls_client(address),
        &source,
        |part| {
            bytes.extend_from_slice(part);
            Ok(())
        },
        || false,
    )
    .await
    .unwrap();
    assert_eq!(bytes, b"dummy");
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("GET /artifact HTTP/1.1"));
    no_credentials(&requests[0]);
}

#[tokio::test]
async fn only_pinned_redirect_origins_are_followed_and_signed_queries_are_not_errors() {
    let (second, _, second_server) = server(vec![response("dummy")], false).await;
    let redirect = format!(
        "HTTP/1.1 302 Found\r\nLocation: {}/payload?signature=dummy-only\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        origin(second)
    );
    let (first, _, first_server) = server(vec![redirect], false).await;
    let source =
        ArtifactSource::new(&format!("{}/artifact", origin(first)), &[origin(second)], 5).unwrap();
    let mut bytes = Vec::new();
    fetch_with_client(
        &tls_client(first),
        &source,
        |part| {
            bytes.extend_from_slice(part);
            Ok(())
        },
        || false,
    )
    .await
    .unwrap();
    assert_eq!(bytes, b"dummy");
    no_credentials(&first_server.await.unwrap()[0]);
    let second = second_server.await.unwrap();
    no_credentials(&second[0]);
    assert!(second[0].starts_with("GET /payload?signature=dummy-only HTTP/1.1"));
    let (address, _, server) = server(vec!["HTTP/1.1 302 Found\r\nLocation: https://unapproved.invalid/file?secret=dummy-only\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()], false).await;
    let source = ArtifactSource::new(&format!("{}/artifact", origin(address)), &[], 5).unwrap();
    let error = fetch_with_client(
        &tls_client(address),
        &source,
        |_| panic!("unapproved redirect wrote bytes"),
        || false,
    )
    .await
    .unwrap_err();
    assert_eq!(error, Error::Policy);
    assert!(!error.to_string().contains("dummy-only"));
    assert_eq!(server.await.unwrap().len(), 1);
}

#[tokio::test]
async fn header_chunked_size_and_encoding_failures_never_overrun_the_sink() {
    for reply in [response("excess"),
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n3\r\nabc\r\n3\r\ndef\r\n0\r\n\r\n".into(),
        "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nabc".into(),
        "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: 5\r\nConnection: close\r\n\r\ndummy".into()] {
        let (address, _, server) = server(vec![reply], false).await;
        let source = ArtifactSource::new(&format!("{}/artifact", origin(address)), &[], 5).unwrap();
        let mut bytes = Vec::new();
        let result = fetch_with_client(&tls_client(address), &source, |part| { bytes.extend_from_slice(part); Ok(()) }, || false).await;
        assert_eq!(result, Err(Error::Size));
        assert!(bytes.len() <= 5);
        assert_eq!(server.await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn rejected_status_and_sink_failure_are_not_retried() {
    for status in [true, false] {
        let reply = if status {
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
        } else {
            response("dummy")
        };
        let (address, _, server) = server(vec![reply], false).await;
        let source = ArtifactSource::new(&format!("{}/artifact", origin(address)), &[], 5).unwrap();
        let result = fetch_with_client(&tls_client(address), &source, |_| Err(()), || false).await;
        assert_eq!(
            result,
            Err(if status {
                Error::Status(401)
            } else {
                Error::Sink
            })
        );
        assert_eq!(server.await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn cancellation_interrupts_a_pending_body_and_redirects_have_a_fixed_cap() {
    let (address, received, body_server) = server(
        vec!["HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n".into()],
        true,
    )
    .await;
    let cancelled = Arc::new(AtomicBool::new(false));
    let flag = cancelled.clone();
    let cancel = tokio::spawn(async move {
        received.notified().await;
        flag.store(true, Ordering::Release);
    });
    let source = ArtifactSource::new(&format!("{}/artifact", origin(address)), &[], 5).unwrap();
    let client = tls_client(address);
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        fetch_with_client(
            &client,
            &source,
            |_| Ok(()),
            || cancelled.load(Ordering::Acquire),
        ),
    )
    .await
    .unwrap();
    assert_eq!(result, Err(Error::Cancelled));
    cancel.await.unwrap();
    body_server.abort();
    let redirect =
        "HTTP/1.1 302 Found\r\nLocation: /again\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_owned();
    let (address, _, server) = server(vec![redirect; MAX_REDIRECTS + 1], false).await;
    let source = ArtifactSource::new(&format!("{}/artifact", origin(address)), &[], 5).unwrap();
    assert_eq!(
        fetch_with_client(&tls_client(address), &source, |_| Ok(()), || false).await,
        Err(Error::Policy)
    );
    assert_eq!(server.await.unwrap().len(), MAX_REDIRECTS + 1);
}
