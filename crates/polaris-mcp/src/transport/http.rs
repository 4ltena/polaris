//! Restricted Streamable HTTP transport with explicit destination checks.

use crate::{Error, store::Secrets};
use futures_util::StreamExt;
use reqwest_mcp::{
    Client, Method, RequestBuilder, Response, StatusCode,
    header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue},
};
use rmcp::{
    model::ClientJsonRpcMessage,
    transport::{
        common::{
            client_side_sse::BoxedSseResponse,
            http_header::{HEADER_LAST_EVENT_ID, HEADER_SESSION_ID},
        },
        streamable_http_client::{
            StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
        },
    },
};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::Duration,
};

type Result<T> = std::result::Result<T, StreamableHttpError<Error>>;
use super::sensitive::SensitiveValues;
fn closed(error: Error) -> StreamableHttpError<Error> {
    StreamableHttpError::Client(error)
}

/// Exactly one registered destination; secrets never enter SDK config/Debug.
#[derive(Clone)]
pub(super) struct HttpClient {
    client: Client,
    destination: Arc<str>,
    headers: HeaderMap,
    request_timeout: Duration,
    secrets: SensitiveValues,
    failures: super::failure::Failures,
}

impl HttpClient {
    pub(super) fn new<'a>(
        destination: &str,
        references: &BTreeMap<String, String>,
        secrets: impl Into<Option<&'a Secrets>>,
        connect_timeout: Duration,
    ) -> std::result::Result<Self, Error> {
        let secrets = secrets.into();
        if crate::registration::normalize_http_url(destination)? != destination {
            return Err(Error::InvalidRegistration);
        }
        let mut headers = HeaderMap::new();
        let mut sensitive = Vec::new();
        for (name, reference) in references {
            let secrets = secrets.ok_or(Error::Auth)?;
            // Registry validation remains mandatory; defend the boundary too.
            if forbidden_secret_header(name) {
                return Err(Error::InvalidRegistration);
            }
            let name =
                HeaderName::from_bytes(name.as_bytes()).map_err(|_| Error::InvalidRegistration)?;
            let mut value =
                HeaderValue::from_str(secrets.resolve(reference)?).map_err(|_| Error::Auth)?;
            sensitive.push(secrets.resolve(reference)?.to_owned());
            value.set_sensitive(true);
            headers.insert(name, value);
        }
        let client = Client::builder()
            .redirect(reqwest_mcp::redirect::Policy::none())
            .retry(reqwest_mcp::retry::never())
            .no_proxy()
            .connect_timeout(connect_timeout)
            .build()
            .map_err(|_| Error::Transport)?;
        Ok(Self {
            client,
            destination: Arc::from(destination),
            headers,
            request_timeout: Duration::from_secs(120),
            secrets: SensitiveValues::new(sensitive),
            failures: super::failure::Failures::default(),
        })
    }

    pub(super) fn with_failures(mut self, failures: super::failure::Failures) -> Self {
        self.failures = failures;
        self
    }

    pub(super) fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    fn request(
        &self,
        method: Method,
        uri: &str,
        session: Option<&str>,
        auth: Option<String>,
        custom: HashMap<HeaderName, HeaderValue>,
    ) -> Result<RequestBuilder> {
        if uri != self.destination.as_ref() || auth.is_some() {
            return Err(closed(Error::InvalidRegistration));
        }
        let mut request = self
            .client
            .request(method, self.destination.as_ref())
            .timeout(self.request_timeout)
            .headers(self.headers.clone())
            .header(ACCEPT, "application/json, text/event-stream");
        // Only the SDK's protocol metadata may travel through this channel.
        for (name, value) in custom {
            if !matches!(
                name.as_str(),
                "mcp-protocol-version" | "mcp-method" | "mcp-name"
            ) && !name.as_str().starts_with("mcp-param-")
            {
                return Err(closed(Error::Protocol));
            }
            request = request.header(name, value);
        }
        if let Some(session) = session {
            request = request.header(HEADER_SESSION_ID, session);
        }
        Ok(request)
    }

    async fn send(request: RequestBuilder) -> Result<Response> {
        request.send().await.map_err(|_| closed(Error::Transport))
    }

    async fn body(mut response: Response) -> Result<Vec<u8>> {
        if response
            .content_length()
            .is_some_and(|n| n > super::MAX_MESSAGE_BYTES as u64)
        {
            return Err(closed(Error::Limit));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| closed(Error::Transport))?
        {
            if chunk.len() > super::MAX_MESSAGE_BYTES - bytes.len() {
                return Err(closed(Error::Limit));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }

    fn check_status(response: &Response, session: bool) -> Result<()> {
        match response.status() {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(closed(Error::Auth)),
            StatusCode::NOT_FOUND if session => Err(StreamableHttpError::SessionExpired),
            status if status.is_success() => Ok(()),
            StatusCode::REQUEST_TIMEOUT => Err(closed(Error::Timeout)),
            StatusCode::TOO_MANY_REQUESTS => Err(closed(Error::Transport)),
            status if status.is_redirection() || status.is_client_error() => {
                Err(closed(Error::Protocol))
            }
            // Do not read/log a reflected body, challenge, redirect, or URL.
            _ => Err(closed(Error::Transport)),
        }
    }

    fn is_mime(response: &Response, expected: &str) -> bool {
        response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(';').next())
            .is_some_and(|v| v.trim().eq_ignore_ascii_case(expected))
    }

    fn sse(&self, response: Response, limit: usize) -> BoxedSseResponse {
        // Drop raw reqwest errors before the SDK can format them.
        let bytes = response.bytes_stream().scan(
            (EventLimit::new(limit.min(super::MAX_MESSAGE_BYTES)), false),
            |(limiter, failed), next| {
                let next = if *failed {
                    None
                } else {
                    let result = next.map_err(|_| Error::Transport).and_then(|chunk| {
                        limiter.observe(&chunk)?;
                        Ok(chunk)
                    });
                    *failed = result.is_err();
                    Some(result)
                };
                std::future::ready(next)
            },
        );
        let secrets = self.secrets.clone();
        let failures = self.failures.clone();
        sse_stream::SseStream::from_bytes_stream(bytes)
            .map(move |event| {
                let event = event.map_err(|error| {
                    let class = match error {
                        sse_stream::Error::Body(error) => error
                            .downcast_ref::<Error>()
                            .copied()
                            .unwrap_or(Error::Protocol),
                        _ => Error::Protocol,
                    };
                    failures.record(class);
                    sse_stream::Error::Body(Box::new(class))
                })?;
                let checked = (|| {
                    if let Some(id) = &event.id {
                        secrets.text(id)?;
                    }
                    if let Some(name) = &event.event {
                        secrets.text(name)?;
                    }
                    if let Some(data) = &event.data {
                        secrets.json_bytes(data.as_bytes())?;
                    }
                    Ok::<(), Error>(())
                })();
                checked.map_err(|e| {
                    failures.record(e);
                    sse_stream::Error::Body(Box::new(e))
                })?;
                Ok(event)
            })
            .boxed()
    }
}

/// Limit raw bytes between SSE blank lines, including comments/unknown fields.
/// CRLF may span network chunks; neither chunking nor comments reset the limit.
struct EventLimit {
    limit: usize,
    bytes: usize,
    line: usize,
    cr: bool,
    ended_at_cr: bool,
}
impl EventLimit {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            bytes: 0,
            line: 0,
            cr: false,
            ended_at_cr: false,
        }
    }
    fn observe(&mut self, chunk: &[u8]) -> std::result::Result<(), Error> {
        for &byte in chunk {
            if self.cr && byte == b'\n' {
                self.bytes += 1;
                if self.bytes > self.limit {
                    return Err(Error::Limit);
                }
                if self.ended_at_cr {
                    self.bytes = 0;
                }
                self.cr = false;
                continue;
            }
            if self.cr && self.ended_at_cr {
                self.bytes = 0;
            }
            self.cr = false;
            self.bytes += 1;
            if self.bytes > self.limit {
                return Err(Error::Limit);
            }
            if matches!(byte, b'\r' | b'\n') {
                self.ended_at_cr = self.line == 0;
                if self.line == 0 && byte == b'\n' {
                    self.bytes = 0;
                }
                self.line = 0;
                self.cr = byte == b'\r';
            } else {
                self.line += 1;
            }
        }
        Ok(())
    }
}

fn forbidden_secret_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        "host"
            | "connection"
            | "content-length"
            | "transfer-encoding"
            | "content-type"
            | "accept"
            | "last-event-id"
            | "upgrade"
            | "te"
            | "trailer"
    ) || name.starts_with("mcp-")
        || name.starts_with("proxy-")
}

impl StreamableHttpClient for HttpClient {
    type Error = Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session: Option<Arc<str>>,
        auth: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse> {
        self.post_message_with_max_sse_event_size(
            uri,
            message,
            session,
            auth,
            headers,
            super::MAX_MESSAGE_BYTES,
        )
        .await
    }

    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session: Option<Arc<str>>,
        auth: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
        limit: usize,
    ) -> Result<StreamableHttpPostResponse> {
        let result = async {
            let request = self.request(Method::POST, &uri, session.as_deref(), auth, headers)?;
            let response = Self::send(request.json(&message)).await?;
            if let Err(error) = Self::check_status(&response, session.is_some()) {
                // A legacy server may report method-not-found for server/discover
                // using HTTP 400. Preserve only a bounded typed JSON-RPC error;
                // lifecycle fallback still requires the host's explicit profile.
                if !matches!(
                    response.status(),
                    StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
                ) && response.status().is_client_error()
                    && Self::is_mime(&response, "application/json")
                {
                    let body = Self::body(response).await?;
                    self.secrets.json_bytes(&body).map_err(closed)?;
                    if let Ok(message @ rmcp::model::JsonRpcMessage::Error(_)) =
                        serde_json::from_slice(&body)
                    {
                        return Ok(StreamableHttpPostResponse::Json(message, None));
                    }
                }
                return Err(error);
            }
            if matches!(
                response.status(),
                StatusCode::ACCEPTED | StatusCode::NO_CONTENT
            ) {
                return Ok(StreamableHttpPostResponse::Accepted);
            }
            let session = response
                .headers()
                .get(HEADER_SESSION_ID)
                .map(|v| {
                    let value = v.to_str().map_err(|_| closed(Error::Protocol))?;
                    self.secrets.text(value).map_err(closed)?;
                    if value.is_empty()
                        || value.len() > 1024
                        || !value.bytes().all(|b| b.is_ascii_graphic())
                    {
                        return Err(closed(Error::Protocol));
                    }
                    Ok(value.to_owned())
                })
                .transpose()?;
            if Self::is_mime(&response, "text/event-stream") {
                return Ok(StreamableHttpPostResponse::Sse(
                    self.sse(response, limit),
                    session,
                ));
            }
            if !Self::is_mime(&response, "application/json") {
                return Err(closed(Error::Protocol));
            }
            let body = Self::body(response).await?;
            self.secrets.json_bytes(&body).map_err(closed)?;
            let message = serde_json::from_slice(&body).map_err(|_| closed(Error::Protocol))?;
            Ok(StreamableHttpPostResponse::Json(message, session))
        }
        .await;
        if let Err(error) = &result {
            self.failures.record(super::failure::http(error));
        }
        result
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session: Arc<str>,
        auth: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<()> {
        let response =
            Self::send(self.request(Method::DELETE, &uri, Some(&session), auth, headers)?).await?;
        if matches!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_FOUND
        ) {
            return Ok(());
        }
        Self::check_status(&response, true)
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session: Option<Arc<str>>,
        last: Option<String>,
        auth: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxedSseResponse> {
        self.get_stream_with_max_sse_event_size(
            uri,
            session,
            last,
            auth,
            headers,
            super::MAX_MESSAGE_BYTES,
        )
        .await
    }

    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session: Option<Arc<str>>,
        last: Option<String>,
        auth: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
        limit: usize,
    ) -> Result<BoxedSseResponse> {
        let result = async {
            let mut request = self.request(Method::GET, &uri, session.as_deref(), auth, headers)?;
            if let Some(last) = last {
                request = request.header(HEADER_LAST_EVENT_ID, last);
            }
            let response = Self::send(request).await?;
            if response.status() == StatusCode::METHOD_NOT_ALLOWED {
                return Err(StreamableHttpError::ServerDoesNotSupportSse);
            }
            Self::check_status(&response, session.is_some())?;
            if !Self::is_mime(&response, "text/event-stream") {
                return Err(closed(Error::Protocol));
            }
            Ok(self.sse(response, limit))
        }
        .await;
        if let Err(error) = &result {
            self.failures.record(super::failure::http(error));
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    async fn fixture(response: Vec<u8>) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let uri = format!("http://{}/mcp", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut bytes = [0; 2048];
                let n = socket.read(&mut bytes).await.unwrap();
                assert!(n > 0);
                request.extend_from_slice(&bytes[..n]);
                assert!(request.len() < 32768);
                if let Some(offset) = request.windows(4).position(|v| v == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..offset]).to_ascii_lowercase();
                    let length: usize = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length: "))
                        .unwrap_or("0")
                        .parse()
                        .unwrap();
                    if request.len() >= offset + 4 + length {
                        break;
                    }
                }
            }
            // Limit tests deliberately close the connection before draining.
            let _ = socket.write_all(&response).await;
            String::from_utf8(request).unwrap()
        });
        (uri, task)
    }

    fn client(uri: &str) -> HttpClient {
        let secrets = Secrets::new(
            1,
            BTreeMap::from([("test".into(), "synthetic-secret".into())]),
        )
        .unwrap();
        HttpClient::new(
            uri,
            &BTreeMap::from([("x-test-token".into(), "test".into())]),
            &secrets,
            Duration::from_secs(2),
        )
        .unwrap()
    }
    fn message() -> ClientJsonRpcMessage {
        serde_json::from_str(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#).unwrap()
    }
    fn response(status: &str, mime: &str, body: &[u8]) -> Vec<u8> {
        [format!("HTTP/1.1 {status}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes(), body.to_vec()].concat()
    }
    async fn post(client: &HttpClient, uri: &str) -> Result<StreamableHttpPostResponse> {
        client
            .post_message(Arc::from(uri), message(), None, None, HashMap::new())
            .await
    }

    #[tokio::test]
    async fn bounded_json_positive_and_malformed_response_is_not_accepted() {
        for (body, valid) in [
            (br#"{"jsonrpc":"2.0","id":1,"result":{}}"#.as_slice(), true),
            (b"{}", false),
        ] {
            let (uri, task) =
                fixture(response("200 OK", "application/json; charset=utf-8", body)).await;
            let result = post(&client(&uri), &uri).await;
            if valid {
                assert!(matches!(
                    result.unwrap(),
                    StreamableHttpPostResponse::Json(..)
                ));
            } else {
                assert_eq!(
                    result.unwrap_err().to_string(),
                    "Client error: mcp_protocol"
                );
            }
            let request = task.await.unwrap();
            assert!(request.starts_with("POST /mcp HTTP/1.1"));
            assert!(request.contains("x-test-token: synthetic-secret"));
        }
    }

    #[tokio::test]
    async fn exact_destination_and_redacted_status_errors() {
        let (uri, task) = fixture(response(
            "401 Unauthorized",
            "text/plain",
            b"synthetic-secret",
        ))
        .await;
        let client = client(&uri);
        for altered in [format!("{uri}/other"), uri.replace("/mcp", "/other")] {
            assert_eq!(
                post(&client, &altered).await.unwrap_err().to_string(),
                "Client error: mcp_invalid_registration"
            );
        }
        assert_eq!(
            post(&client, &uri).await.unwrap_err().to_string(),
            "Client error: mcp_auth"
        );
        assert!(task.await.unwrap().starts_with("POST /mcp"));
    }

    #[tokio::test]
    async fn redirect_is_rejected_without_contacting_the_target() {
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let wire = format!(
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{}/stolen\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            target.local_addr().unwrap()
        );
        let (uri, task) = fixture(wire.into_bytes()).await;
        assert_eq!(
            post(&client(&uri), &uri).await.unwrap_err().to_string(),
            "Client error: mcp_protocol"
        );
        task.await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), target.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn ambient_proxy_is_ignored_with_a_working_proxy_positive_control() {
        let body = br#"{"jsonrpc":"2.0","id":1,"result":{}}"#;
        let (uri, direct) = fixture(response("200 OK", "application/json", body)).await;
        let (proxy, forwarded) = fixture(response("200 OK", "application/json", body)).await;
        for control in [false, true] {
            let output = tokio::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "transport::http::tests::proxy_environment_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env("POLARIS_MCP_FIXTURE_URL", &uri)
                .env("POLARIS_MCP_PROXY_CONTROL", if control { "1" } else { "0" })
                .env("HTTP_PROXY", &proxy)
                .env("HTTPS_PROXY", &proxy)
                .env("ALL_PROXY", &proxy)
                .env("http_proxy", &proxy)
                .env("https_proxy", &proxy)
                .env("all_proxy", &proxy)
                .env_remove("NO_PROXY")
                .env_remove("no_proxy")
                .env_remove("REQUEST_METHOD")
                .output()
                .await
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            if !control {
                assert!(!forwarded.is_finished(), "MCP contacted ambient proxy");
            }
        }
        assert!(direct.await.unwrap().starts_with("POST /mcp "));
        assert!(forwarded.await.unwrap().starts_with("POST http://"));
    }

    #[tokio::test]
    #[ignore = "child fixture for isolated proxy environment"]
    async fn proxy_environment_child() {
        let Ok(uri) = std::env::var("POLARIS_MCP_FIXTURE_URL") else {
            return;
        };
        if std::env::var("POLARIS_MCP_PROXY_CONTROL").as_deref() == Ok("1") {
            let response = Client::builder()
                .timeout(Duration::from_secs(3))
                .build()
                .unwrap()
                .post(&uri)
                .json(&message())
                .send()
                .await
                .unwrap();
            assert!(response.status().is_success());
        } else {
            assert!(matches!(
                post(&client(&uri), &uri).await.unwrap(),
                StreamableHttpPostResponse::Json(..)
            ));
        }
    }

    #[tokio::test]
    async fn json_limit_covers_content_length_and_chunked_bodies() {
        for chunked in [false, true] {
            let body = vec![b'x'; super::super::MAX_MESSAGE_BYTES + 1];
            let wire = if chunked {
                [b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec(), format!("{:x}\r\n", body.len()).into_bytes(), body, b"\r\n0\r\n\r\n".to_vec()].concat()
            } else {
                response("200 OK", "application/json", &body)
            };
            let (uri, task) = fixture(wire).await;
            assert_eq!(
                post(&client(&uri), &uri).await.unwrap_err().to_string(),
                "Client error: mcp_limit"
            );
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn sse_stream_is_bounded_before_parsing() {
        for (body, valid) in [
            (b"data: {}\n\n".to_vec(), true),
            ([b"data: ".as_slice(), &[b'x'; 128]].concat(), false),
        ] {
            let (uri, task) = fixture(response("200 OK", "text/event-stream", &body)).await;
            let client = client(&uri);
            let result = client
                .post_message_with_max_sse_event_size(
                    Arc::from(uri),
                    message(),
                    None,
                    None,
                    HashMap::new(),
                    64,
                )
                .await
                .unwrap();
            let StreamableHttpPostResponse::Sse(mut events, _) = result else {
                panic!("expected SSE");
            };
            let event = events.next().await.unwrap();
            if valid {
                assert_eq!(event.unwrap().data.as_deref(), Some("{}"));
            } else {
                assert_eq!(event.unwrap_err().to_string(), "body error: mcp_limit");
                assert_eq!(client.failures.resolve(Error::Transport), Error::Limit);
            }
            task.await.unwrap();
        }
    }

    #[test]
    fn sse_bound_survives_crlf_chunking_and_comments() {
        let mut limit = EventLimit::new(12);
        for bytes in [b"data: x\r".as_slice(), b"\n\r", b"\ndata: y\n\n"] {
            limit.observe(bytes).unwrap();
        }
        for bytes in [b":123\r".as_slice(), b"\n:456\r", b"\n:7"] {
            if limit.observe(bytes).is_err() {
                return;
            }
        }
        panic!("comment-only oversized event accepted");
    }
}
