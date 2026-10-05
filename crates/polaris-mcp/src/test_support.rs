//! Loopback-only, scripted MCP server. No credentials or production endpoint.
use crate::registration::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

pub async fn server_fixture(
    delay_method: &'static str,
) -> (Server, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
    server_fixture_with_tool_reply(
        delay_method,
        json!({"content":[{"type":"text","text":"known result ".repeat(3000)}]}),
    )
    .await
}

pub async fn server_fixture_with_tool_reply(
    delay_method: &'static str,
    tool_reply: Value,
) -> (Server, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
    server_fixture_with_replies(
        delay_method,
        tool_reply,
        json!([{"name":"fixture","inputSchema":{"type":"object"}}]),
    )
    .await
}

pub async fn server_fixture_with_replies(
    delay_method: &'static str,
    tool_reply: Value,
    tools: Value,
) -> (Server, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
    server_fixture_with_failures(delay_method, tool_reply, tools, "", Vec::new()).await
}

pub async fn server_fixture_with_failures(
    delay_method: &'static str,
    tool_reply: Value,
    tools: Value,
    fail_method: &'static str,
    failures: Vec<u16>,
) -> (Server, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let calls = Arc::new(Mutex::new(Vec::new()));
    let seen = calls.clone();
    let task = tokio::spawn(async move {
        let mut failures = failures.into_iter();
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
                let method = request["method"].as_str().unwrap();
                seen.lock().unwrap().push(method.to_string());
                if method == delay_method {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                let result = match method {
                    "server/discover" => {
                        json!({"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{},"resources":{},"prompts":{}},"ttlMs":0,"cacheScope":"private"})
                    }
                    "tools/list" => {
                        json!({"tools":tools})
                    }
                    "resources/list" => {
                        json!({"resources":[{"uri":"memory://note","name":"project note"}]})
                    }
                    "resources/templates/list" => {
                        json!({"resourceTemplates":[{"uriTemplate":"memory://note/{id}","name":"note template"}]})
                    }
                    "prompts/list" => json!({"prompts":[{"name":"prompt"}]}),
                    "prompts/get" => {
                        json!({"messages":[{"role":"user","content":{"type":"text","text":"selected prompt"}}]})
                    }
                    "resources/read" => {
                        json!({"contents":[{"uri":request["params"]["uri"],"text":"resource result","mimeType":"text/plain"}]})
                    }
                    "tools/call" => tool_reply.clone(),
                    _ => json!({}),
                };
                if let Some(status) = (method == fail_method).then(|| failures.next()).flatten() {
                    (format!("{status} Scripted failure"), Vec::new())
                } else if request.get("id").is_none() {
                    ("202 Accepted".to_owned(), Vec::new())
                } else {
                    (
                        "200 OK".to_owned(),
                        serde_json::to_vec(
                            &json!({"jsonrpc":"2.0","id":request["id"],"result":result}),
                        )
                        .unwrap(),
                    )
                }
            } else {
                ("405 Method Not Allowed".to_owned(), Vec::new())
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
            resource_read: true,
            prompt_selection: true,
        },
        timeouts: Timeouts {
            operation_seconds: 1,
            ..Default::default()
        },
    };
    (server, calls, task)
}
