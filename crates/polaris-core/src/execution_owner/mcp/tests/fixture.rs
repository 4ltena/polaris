//! Loopback-only, scripted MCP server. No credentials or production endpoint.
use super::*;
use polaris_mcp::{registration::*, store::Secrets};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

pub(super) async fn ready(
    reply: Value,
    delay: bool,
) -> (
    Connection,
    Arc<std::sync::atomic::AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = calls.clone();
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
                        .find_map(|line| line.strip_prefix("content-length: "))
                        .unwrap_or("0")
                        .parse()
                        .unwrap();
                    if bytes.len() >= offset + 4 + length {
                        break (length != 0).then(|| {
                            serde_json::from_slice::<Value>(&bytes[offset + 4..offset + 4 + length])
                                .unwrap()
                        });
                    }
                }
            };
            let (status, output) = if let Some(request) = body {
                let result = match request["method"].as_str().unwrap() {
                    "server/discover" => {
                        json!({"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{},"prompts":{}},"ttlMs":0,"cacheScope":"private"})
                    }
                    "tools/list" => {
                        json!({"tools":[{"name":"fixture","inputSchema":{"type":"object"}}]})
                    }
                    "prompts/list" => json!({"prompts":[{"name":"prompt"}]}),
                    "prompts/get" | "tools/call" => {
                        seen.fetch_add(1, Ordering::AcqRel);
                        if delay {
                            tokio::time::sleep(Duration::from_secs(2)).await;
                        }
                        reply.clone()
                    }
                    _ => json!({}),
                };
                if request.get("id").is_none() {
                    ("202 Accepted", Vec::new())
                } else {
                    (
                        "200 OK",
                        serde_json::to_vec(
                            &json!({"jsonrpc":"2.0","id":request["id"],"result":result}),
                        )
                        .unwrap(),
                    )
                }
            } else {
                ("405 Method Not Allowed", Vec::new())
            };
            let header = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                output.len()
            );
            let _ = stream.write_all(header.as_bytes()).await;
            let _ = stream.write_all(&output).await;
        }
    });
    let server = Server {
        server_id: "fixture".into(),
        enabled_projects: BTreeSet::from(["project".into()]),
        transport: Destination::StreamableHttp {
            url,
            secret_headers: BTreeMap::new(),
        },
        protocol_profile: ProtocolProfile::Discover2026,
        capabilities: Capabilities {
            catalog_metadata: true,
            tool_call: true,
            prompt_selection: true,
            ..Default::default()
        },
        timeouts: Timeouts {
            operation_seconds: 1,
            ..Default::default()
        },
    };
    let mut connection = Connection::prepare(
        &server,
        "project",
        &Secrets::new(1, BTreeMap::new()).unwrap(),
        None,
        || true,
    )
    .unwrap();
    connection
        .initialize(std::future::pending(), || true)
        .await
        .unwrap();
    connection
        .refresh_catalog(1, std::future::pending(), || true)
        .await
        .unwrap();
    (connection, calls, task)
}
