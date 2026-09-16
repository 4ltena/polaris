//! Synthetic lifecycle and isolation checks for MCP transports.

use super::*;
use crate::registration::{Capabilities, Timeouts};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
mod apps;

pub(super) fn server(transport: Destination, profile: ProtocolProfile) -> Server {
    Server {
        server_id: "fixture".into(),
        enabled_projects: BTreeSet::from(["project".into()]),
        transport,
        protocol_profile: profile,
        capabilities: Capabilities {
            catalog_metadata: true,
            ..Default::default()
        },
        timeouts: Timeouts::default(),
    }
}
pub(super) fn secrets() -> Secrets {
    Secrets::new(1, BTreeMap::new()).unwrap()
}

fn answer(request: &Value, legacy: bool) -> Value {
    let result = match request["method"].as_str().unwrap() {
        "server/discover" if legacy => {
            return json!({"jsonrpc":"2.0", "id":request["id"], "error":{"code":-32601,"message":"legacy fixture"}});
        }
        "server/discover" => {
            json!({"resultType":"complete", "supportedVersions":["2026-07-28"], "capabilities":{"tools":{}, "resources":{}, "prompts":{}}, "ttlMs":0,"cacheScope":"private"})
        }
        "initialize" => {
            json!({"protocolVersion":"2025-11-25", "capabilities":{"tools":{},"resources":{},"prompts":{}}, "serverInfo":{"name":"fixture","version":"1"}})
        }
        "tools/list" => json!({"tools":[{"name":"fixture","inputSchema":{"type":"object"}}]}),
        "resources/list" => json!({"resources":[]}),
        "resources/templates/list" => json!({"resourceTemplates":[]}),
        "prompts/list" => json!({"prompts":[]}),
        _ => json!({}),
    };
    json!({"jsonrpc":"2.0", "id":request["id"], "result":result})
}

pub(super) async fn fixture(
    legacy: bool,
) -> (
    String,
    Arc<std::sync::Mutex<Vec<String>>>,
    tokio::task::JoinHandle<()>,
) {
    fixture_failure(legacy, None).await
}

#[derive(Clone, Copy)]
enum Failure {
    Expired,
    Disconnect,
    Timeout,
}

async fn fixture_failure(
    legacy: bool,
    failure: Option<Failure>,
) -> (
    String,
    Arc<std::sync::Mutex<Vec<String>>>,
    tokio::task::JoinHandle<()>,
) {
    fixture_response(legacy, failure, None).await
}

pub(super) struct OperationFixture {
    pub reply: Value,
    pub delay: bool,
    pub disconnect: bool,
    /// Replaces the listed tools, e.g. Unreal's three meta-tools.
    pub tools: Option<Value>,
}

pub(super) async fn operation_fixture(
    operation: OperationFixture,
) -> (
    String,
    Arc<std::sync::Mutex<Vec<String>>>,
    tokio::task::JoinHandle<()>,
) {
    fixture_response(false, None, Some(operation)).await
}

async fn fixture_response(
    legacy: bool,
    failure: Option<Failure>,
    operation: Option<OperationFixture>,
) -> (
    String,
    Arc<std::sync::Mutex<Vec<String>>>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}/mcp", listener.local_addr().unwrap());
    let methods = Arc::new(std::sync::Mutex::new(Vec::new()));
    let record = methods.clone();
    let task = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let body = loop {
                let mut part = [0; 8192];
                let n = stream.read(&mut part).await.unwrap();
                if n == 0 {
                    break None;
                }
                bytes.extend_from_slice(&part[..n]);
                assert!(bytes.len() < 65536);
                if let Some(offset) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..offset]).to_ascii_lowercase();
                    let length: usize = headers
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length: "))
                        .unwrap_or("0")
                        .parse()
                        .unwrap();
                    if bytes.len() >= offset + 4 + length {
                        if length == 0 {
                            break None;
                        }
                        break Some(
                            serde_json::from_slice::<Value>(
                                &bytes[offset + 4..offset + 4 + length],
                            )
                            .unwrap(),
                        );
                    }
                }
            };
            let (status, output) = if let Some(request) = body {
                record
                    .lock()
                    .unwrap()
                    .push(request["method"].as_str().unwrap().into());
                if let Some(operation) = &operation {
                    let method = request["method"].as_str().unwrap();
                    let result = match method {
                        "tools/list" if operation.tools.is_some() => operation.tools.clone(),
                        "resources/templates/list" => Some(
                            json!({"resourceTemplates":[{"name":"template","uriTemplate":"fixture://resource/{name}{?lang}"}]}),
                        ),
                        "resources/list" => Some(
                            json!({"resources":[{"name":"resource","uri":"fixture://resource"}]}),
                        ),
                        "prompts/list" => Some(json!({"prompts":[{"name":"prompt"}]})),
                        "tools/call" | "resources/read" | "prompts/get" => {
                            if method == "resources/read"
                                && let Some(uri) = operation
                                    .reply
                                    .pointer("/contents/0/uri")
                                    .and_then(Value::as_str)
                            {
                                assert_eq!(
                                    request.pointer("/params/uri").and_then(Value::as_str),
                                    Some(uri)
                                );
                            }
                            if operation.delay {
                                tokio::time::sleep(Duration::from_secs(2)).await;
                            }
                            if operation.disconnect {
                                continue;
                            }
                            Some(operation.reply.clone())
                        }
                        _ => None,
                    };
                    if let Some(result) = result {
                        let output = serde_json::to_vec(
                            &json!({"jsonrpc":"2.0","id":request["id"],"result":result}),
                        )
                        .unwrap();
                        let header = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            output.len()
                        );
                        let _ = stream.write_all(header.as_bytes()).await;
                        let _ = stream.write_all(&output).await;
                        continue;
                    }
                }
                if request["method"] == "tools/list"
                    && let Some(failure) = failure
                {
                    assert!(
                        String::from_utf8_lossy(&bytes)
                            .to_ascii_lowercase()
                            .contains("mcp-session-id: fixture-session")
                    );
                    match failure {
                        Failure::Disconnect => continue,
                        Failure::Timeout => {
                            tokio::time::sleep(Duration::from_secs(2)).await;
                            continue;
                        }
                        Failure::Expired => {
                            let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                            continue;
                        }
                    }
                }
                if request.get("id").is_none() {
                    ("202 Accepted", Vec::new())
                } else {
                    (
                        if legacy && request["method"] == "server/discover" {
                            "400 Bad Request"
                        } else {
                            "200 OK"
                        },
                        serde_json::to_vec(&answer(&request, legacy)).unwrap(),
                    )
                }
            } else {
                ("405 Method Not Allowed", Vec::new())
            };
            let header = format!(
                "HTTP/1.1 {status}\r\nMcp-Session-Id: fixture-session\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                output.len()
            );
            let _ = stream.write_all(header.as_bytes()).await;
            let _ = stream.write_all(&output).await;
        }
    });
    (uri, methods, task)
}

#[tokio::test]
async fn sent_request_failure_never_reinitializes_or_replays() {
    for failure in [Failure::Expired, Failure::Disconnect, Failure::Timeout] {
        let (url, methods, task) = fixture_failure(true, Some(failure)).await;
        let mut server = server(
            Destination::StreamableHttp {
                url,
                secret_headers: BTreeMap::new(),
            },
            ProtocolProfile::Auto2025Compat,
        );
        server.timeouts.operation_seconds = 1;
        let mut connection =
            Connection::prepare(&server, "project", &secrets(), None, || true).unwrap();
        connection
            .initialize(std::future::pending(), || true)
            .await
            .unwrap();
        let call = connection.service.as_ref().unwrap().list_tools(None);
        assert!(
            tokio::time::timeout(Duration::from_secs(4), call)
                .await
                .unwrap()
                .is_err()
        );
        connection.shutdown().await.unwrap();
        let sent = methods.lock().unwrap().clone();
        for method in ["server/discover", "initialize", "tools/list"] {
            assert_eq!(
                sent.iter().filter(|m| m.as_str() == method).count(),
                1,
                "{sent:?}"
            );
        }
        task.abort();
        let _ = task.await;
    }
}

async fn assert_catalog(connection: &mut Connection) {
    let catalog = connection
        .refresh_catalog(1, std::future::pending(), || true)
        .await
        .unwrap();
    assert_eq!(catalog.items.len(), 1);
    assert_eq!(catalog.items[0].name, "fixture");
    assert_eq!(catalog.rejected_items, 0);
    assert!(catalog.items[0].input_schema.is_some());
    assert_eq!(connection.catalog(1).unwrap().items.len(), 1);
    assert!(connection.catalog(2).is_err());
}

#[tokio::test]
async fn http_discover_and_explicit_compatibility_do_not_silently_downgrade() {
    for (legacy, profile, success) in [
        (false, ProtocolProfile::Discover2026, true),
        (true, ProtocolProfile::Discover2026, false),
        (true, ProtocolProfile::Auto2025Compat, true),
    ] {
        let (url, methods, task) = fixture(legacy).await;
        let server = server(
            Destination::StreamableHttp {
                url,
                secret_headers: BTreeMap::new(),
            },
            profile,
        );
        let mut connection =
            Connection::prepare(&server, "project", &secrets(), None, || true).unwrap();
        let result = connection.initialize(std::future::pending(), || true).await;
        assert_eq!(result.is_ok(), success, "{result:?}");
        if success {
            assert!(connection.is_connected());
            assert_catalog(&mut connection).await;
        }
        connection.shutdown().await.unwrap();
        assert!(!connection.is_connected());
        let methods = methods.lock().unwrap().clone();
        assert_eq!(
            methods.iter().filter(|m| *m == "initialize").count(),
            usize::from(legacy && success)
        );
        assert_eq!(
            methods.iter().filter(|m| *m == "server/discover").count(),
            1
        );
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
    }
}

#[tokio::test]
async fn cancellation_and_authorization_prevent_handshake_sends() {
    for revoked in [false, true] {
        let (url, methods, task) = fixture(false).await;
        let server = server(
            Destination::StreamableHttp {
                url,
                secret_headers: BTreeMap::new(),
            },
            ProtocolProfile::Discover2026,
        );
        let mut connection =
            Connection::prepare(&server, "project", &secrets(), None, || true).unwrap();
        let result = connection
            .initialize(std::future::ready(()), || !revoked)
            .await;
        assert_eq!(result, Err(Error::Cancelled));
        connection.shutdown().await.unwrap();
        assert!(methods.lock().unwrap().is_empty());
        task.abort();
        let _ = task.await;
    }
}

#[tokio::test]
async fn shutdown_failure_stays_unconfirmed_on_later_calls() {
    for failure in [false, true] {
        let server = server(
            Destination::StreamableHttp {
                url: "http://127.0.0.1:1/mcp".into(),
                secret_headers: BTreeMap::new(),
            },
            ProtocolProfile::Discover2026,
        );
        let mut connection =
            Connection::prepare(&server, "project", &secrets(), None, || true).unwrap();
        connection.closing = Some(tokio::spawn(async move {
            if failure {
                Err(Error::StopUnconfirmed)
            } else {
                Ok(())
            }
        }));
        let expected = if failure {
            Err(Error::StopUnconfirmed)
        } else {
            Ok(())
        };
        assert_eq!(connection.shutdown().await, expected);
        assert_eq!(connection.shutdown().await, expected);
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn stdio_refuses_unisolated_or_mismatched_sandbox_before_spawn() {
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().canonicalize().unwrap();
    let server = server(
        Destination::Stdio {
            command: "/usr/bin/true".into(),
            args: vec![],
            cwd: cwd.clone(),
            sandbox_profile: "read-only".into(),
            secret_env: BTreeMap::new(),
            readable_roots: vec![],
            executable_sha256: None,
            script: None,
            script_sha256: None,
        },
        ProtocolProfile::Discover2026,
    );
    let legacy =
        polaris_sandbox::SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[]).unwrap();
    assert!(matches!(
        Connection::prepare(&server, "project", &secrets(), Some(&legacy), || panic!(
            "unisolated launch reached authorization"
        )),
        Err(Error::Protection)
    ));
    let home = cwd.join("home");
    let tmp = cwd.join("tmp");
    std::fs::create_dir(&home).unwrap();
    std::fs::create_dir(&tmp).unwrap();
    let mismatch = polaris_sandbox::SandboxPolicy::isolated(
        polaris_sandbox::SandboxMode::WorkspaceWrite,
        &cwd,
        &[],
    )
    .unwrap()
    .with_isolated_environment(&home, &tmp)
    .unwrap();
    assert!(matches!(
        Connection::prepare(&server, "project", &secrets(), Some(&mismatch), || panic!(
            "mismatched launch reached authorization"
        )),
        Err(Error::Protection)
    ));
}

#[cfg(target_os = "macos")]
/// A Python fixture under the same isolated stdio launch as a registered server.
pub(super) struct PythonStdio {
    _temp: tempfile::TempDir,
    pub server: Server,
    policy: polaris_sandbox::SandboxPolicy,
}

impl PythonStdio {
    pub fn connect(&self) -> Connection {
        Connection::prepare(
            &self.server,
            "project",
            &secrets(),
            Some(&self.policy),
            || true,
        )
        .unwrap()
    }
}

pub(super) fn python_stdio(fixture: &str, extra: &[&str], profile: ProtocolProfile) -> PythonStdio {
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().canonicalize().unwrap();
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(fixture)
        .canonicalize()
        .unwrap();
    // /usr/bin/python3 is an xcrun shim that consults host developer settings.
    // Resolve the actual trusted runtime before entering the isolated child.
    let selected = std::process::Command::new("/usr/bin/xcrun")
        .args(["--find", "python3"])
        .output()
        .unwrap();
    assert!(selected.status.success());
    let selected = std::path::PathBuf::from(String::from_utf8(selected.stdout).unwrap().trim());
    let developer = selected
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .canonicalize()
        .unwrap();
    let command = selected.canonicalize().unwrap();
    let script_root = script.parent().unwrap().to_owned();
    let mut server = server(
        Destination::Stdio {
            command,
            args: ["-I", "-u", script.to_str().unwrap()]
                .into_iter()
                .chain(extra.iter().copied())
                .map(Into::into)
                .collect(),
            cwd: cwd.clone(),
            sandbox_profile: "read-only".into(),
            secret_env: BTreeMap::new(),
            readable_roots: vec![],
            executable_sha256: None,
            script: None,
            script_sha256: None,
        },
        profile,
    );
    server.timeouts.connect_seconds = 8;
    server.timeouts.catalog_seconds = 8;
    server.timeouts.operation_seconds = 8;
    server.capabilities.tool_call = true;
    let home = cwd.join("home");
    let scratch = cwd.join("tmp");
    std::fs::create_dir(&home).unwrap();
    std::fs::create_dir(&scratch).unwrap();
    let runtimes = [
        script_root,
        "/System".into(),
        "/usr/bin".into(),
        "/usr/lib".into(),
        "/bin".into(),
        developer,
    ];
    let policy = polaris_sandbox::SandboxPolicy::isolated(
        polaris_sandbox::SandboxMode::ReadOnly,
        &cwd,
        &runtimes,
    )
    .unwrap()
    .with_isolated_environment(&home, &scratch)
    .unwrap();
    PythonStdio {
        _temp: temp,
        server,
        policy,
    }
}

#[tokio::test]
async fn native_stdio_discovers_the_same_catalog_and_confirms_shutdown() {
    // Only the explicit stdio profile accepts a server that speaks 2024-11-05.
    for (fixture, profile, success) in [
        (
            "tests/fixtures/stdio_server.py",
            ProtocolProfile::Discover2026,
            true,
        ),
        (
            "tests/fixtures/stdio_2024_server.py",
            ProtocolProfile::Auto2024Compat,
            true,
        ),
        (
            "tests/fixtures/stdio_2024_server.py",
            ProtocolProfile::Auto2025Compat,
            false,
        ),
        (
            "tests/fixtures/stdio_2024_server.py",
            ProtocolProfile::Discover2026,
            false,
        ),
    ] {
        let fixture_server = python_stdio(fixture, &[], profile);
        let mut connection = fixture_server.connect();
        let initialized = connection.initialize(std::future::pending(), || true).await;
        assert_eq!(
            initialized.is_ok(),
            success,
            "{fixture} {profile:?}: {initialized:?}"
        );
        if !success {
            connection.shutdown().await.unwrap();
            continue;
        }
        assert_catalog(&mut connection).await;
        let catalog = connection.catalog(1).unwrap();
        let operation = connection
            .prepare_operation(1, &catalog.generation, &catalog.items[0].id, json!({}))
            .unwrap();
        let reply = connection
            .perform_once(operation, std::future::pending(), |_| true)
            .await
            .unwrap();
        assert_eq!(reply.status, crate::content::Status::Complete);
        assert!(
            matches!(&reply.parts[..], [crate::content::Part::Text { text, .. }] if text == "done")
        );
        connection.shutdown().await.unwrap();
        assert!(connection.take_pending_cleanup().is_none());
    }
}

/// A 2024 HTTP+SSE server refuses Streamable HTTP POSTs and offers an SSE
/// endpoint on GET. Neither profile may fall back to that endpoint.
#[tokio::test]
async fn legacy_http_sse_servers_are_refused_without_an_sse_fallback() {
    for profile in [
        ProtocolProfile::Discover2026,
        ProtocolProfile::Auto2025Compat,
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let lines = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let record = lines.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut part = [0; 8192];
                while !bytes.windows(4).any(|v| v == b"\r\n\r\n") {
                    let n = stream.read(&mut part).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&part[..n]);
                }
                let line = String::from_utf8_lossy(&bytes)
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_owned();
                let response: &[u8] = if line.starts_with("GET") {
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\nevent: endpoint\ndata: /messages\n\n"
                } else {
                    b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                };
                record.lock().unwrap().push(line);
                let _ = stream.write_all(response).await;
            }
        });
        let server = server(
            Destination::StreamableHttp {
                url,
                secret_headers: BTreeMap::new(),
            },
            profile,
        );
        let mut connection =
            Connection::prepare(&server, "project", &secrets(), None, || true).unwrap();
        assert!(
            connection
                .initialize(std::future::pending(), || true)
                .await
                .is_err()
        );
        connection.shutdown().await.unwrap();
        let lines = lines.lock().unwrap().clone();
        assert!(!lines.is_empty());
        assert!(
            lines.iter().all(|line| line.starts_with("POST ")),
            "{lines:?}"
        );
        task.abort();
        let _ = task.await;
    }
}

/// Every kind is split across two pages; the host follows each cursor once.
#[tokio::test]
async fn http_catalog_follows_cursors_for_every_kind() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let requests = Arc::new(std::sync::Mutex::new(Vec::<(String, Option<String>)>::new()));
    let record = requests.clone();
    let task = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let body = loop {
                let mut part = [0; 8192];
                let n = stream.read(&mut part).await.unwrap();
                if n == 0 {
                    break None;
                }
                bytes.extend_from_slice(&part[..n]);
                if let Some(offset) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..offset]).to_ascii_lowercase();
                    let length: usize = headers
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length: "))
                        .unwrap_or("0")
                        .parse()
                        .unwrap();
                    if bytes.len() >= offset + 4 + length {
                        break (length > 0).then(|| {
                            serde_json::from_slice::<Value>(&bytes[offset + 4..offset + 4 + length])
                                .unwrap()
                        });
                    }
                }
            };
            let (status, output) = match body {
                Some(request) if request.get("id").is_some() => {
                    let method = request["method"].as_str().unwrap().to_owned();
                    let cursor = request
                        .pointer("/params/cursor")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    record
                        .lock()
                        .unwrap()
                        .push((method.clone(), cursor.clone()));
                    let page = |key: &str, first: Value, second: Value| {
                        if cursor.is_none() {
                            json!({ key: [first], "nextCursor": "second" })
                        } else {
                            json!({ key: [second] })
                        }
                    };
                    let result = match method.as_str() {
                        "server/discover" => {
                            json!({"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{},"resources":{},"prompts":{}},"ttlMs":0,"cacheScope":"private"})
                        }
                        "tools/list" => page(
                            "tools",
                            json!({"name":"first","inputSchema":{"type":"object"}}),
                            json!({"name":"second","inputSchema":{"type":"object"}}),
                        ),
                        "resources/list" => page(
                            "resources",
                            json!({"name":"one","uri":"fixture://one"}),
                            json!({"name":"two","uri":"fixture://two"}),
                        ),
                        "resources/templates/list" => page(
                            "resourceTemplates",
                            json!({"name":"one","uriTemplate":"fixture://one/{id}"}),
                            json!({"name":"two","uriTemplate":"fixture://two/{id}"}),
                        ),
                        "prompts/list" => {
                            page("prompts", json!({"name":"one"}), json!({"name":"two"}))
                        }
                        _ => json!({}),
                    };
                    (
                        "200 OK",
                        serde_json::to_vec(
                            &json!({"jsonrpc":"2.0","id":request["id"],"result":result}),
                        )
                        .unwrap(),
                    )
                }
                Some(_) => ("202 Accepted", Vec::new()),
                None => ("405 Method Not Allowed", Vec::new()),
            };
            let header = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                output.len()
            );
            let _ = stream.write_all(header.as_bytes()).await;
            let _ = stream.write_all(&output).await;
        }
    });
    let mut registration = server(
        Destination::StreamableHttp {
            url,
            secret_headers: BTreeMap::new(),
        },
        ProtocolProfile::Discover2026,
    );
    registration.capabilities.resource_read = true;
    registration.capabilities.prompt_selection = true;
    let mut connection =
        Connection::prepare(&registration, "project", &secrets(), None, || true).unwrap();
    connection
        .initialize(std::future::pending(), || true)
        .await
        .unwrap();
    let catalog = connection
        .refresh_catalog(1, std::future::pending(), || true)
        .await
        .unwrap();
    let mut kinds = BTreeMap::new();
    for item in &catalog.items {
        *kinds.entry(format!("{:?}", item.id.kind)).or_insert(0) += 1;
    }
    assert_eq!(catalog.rejected_items, 0);
    assert_eq!(
        kinds,
        BTreeMap::from([
            ("Prompt".to_owned(), 2),
            ("Resource".to_owned(), 2),
            ("ResourceTemplate".to_owned(), 2),
            ("Tool".to_owned(), 2),
        ])
    );
    connection.shutdown().await.unwrap();
    let requests = requests.lock().unwrap().clone();
    for method in [
        "tools/list",
        "resources/list",
        "resources/templates/list",
        "prompts/list",
    ] {
        let pages: Vec<_> = requests
            .iter()
            .filter(|(m, _)| m == method)
            .map(|(_, cursor)| cursor.clone())
            .collect();
        assert_eq!(pages, [None, Some("second".to_owned())], "{method}");
    }
    task.abort();
    let _ = task.await;
}
