//! `polaris mcp` against a synthetic home; no server or model is contacted.
#[path = "support/product_binary.rs"]
mod product_binary;

use std::io::Write;
use std::process::{Output, Stdio};

fn polaris(
    home: &std::path::Path,
    project: &std::path::Path,
    args: &[&str],
    input: &str,
) -> Output {
    // The child's synthetic HOME must not redirect project identity upward.
    std::fs::create_dir_all(project.join(".git")).unwrap();
    let mut child = product_binary::command()
        .env("HOME", home)
        .env_remove("POLARIS_DATA_DIR")
        .current_dir(project)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn registry_commands_keep_values_out_of_output_and_setup_needs_a_terminal() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let text = |output: &Output| {
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    };
    let listed = polaris(home.path(), project.path(), &["mcp", "list"], "");
    assert!(listed.status.success(), "{}", text(&listed));
    assert!(text(&listed).contains("登録済みのサーバーはありません"));

    let saved = polaris(
        home.path(),
        project.path(),
        &["mcp", "secret-set", "docs-token"],
        "synthetic-value\n",
    );
    assert!(saved.status.success(), "{}", text(&saved));
    assert!(!text(&saved).contains("synthetic-value"));
    let secrets = home.path().join(".polaris/mcp/secrets.json");
    assert!(
        std::fs::read_to_string(&secrets)
            .unwrap()
            .contains("docs-token")
    );

    let missing = polaris(
        home.path(),
        project.path(),
        &["mcp", "enable", "missing"],
        "",
    );
    assert!(!missing.status.success());
    let setup = polaris(home.path(), project.path(), &["mcp", "setup"], "y\n");
    assert!(!setup.status.success());
    assert!(text(&setup).contains("対話端末"));
    assert!(!home.path().join(".polaris/mcp/servers.json").exists());
}

/// A loopback Streamable HTTP server with one tool; GET streams are refused.
/// Returns its URL and the number of `tools/call` requests it answered.
fn serve_one_tool() -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::io::Read;
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = calls.clone();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut bytes = Vec::new();
            let mut part = [0; 8192];
            let body = loop {
                let n = stream.read(&mut part).unwrap_or(0);
                if n == 0 {
                    break None;
                }
                bytes.extend_from_slice(&part[..n]);
                if let Some(offset) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..offset]).to_ascii_lowercase();
                    let length: usize = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length: "))
                        .unwrap_or("0")
                        .parse()
                        .unwrap();
                    if bytes.len() >= offset + 4 + length {
                        break (length > 0).then(|| {
                            serde_json::from_slice::<serde_json::Value>(
                                &bytes[offset + 4..offset + 4 + length],
                            )
                            .unwrap()
                        });
                    }
                }
            };
            let (status, output) = match body {
                Some(request) if request.get("id").is_some() => {
                    let result = match request["method"].as_str().unwrap_or_default() {
                        "server/discover" => {
                            serde_json::json!({"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{},"resources":{},"prompts":{}},"ttlMs":0,"cacheScope":"private"})
                        }
                        "tools/list" => {
                            serde_json::json!({"tools":[{"name":"lookup","inputSchema":{"type":"object"}}]})
                        }
                        "resources/list" => serde_json::json!({"resources":[]}),
                        "resources/templates/list" => serde_json::json!({"resourceTemplates":[]}),
                        "prompts/list" => serde_json::json!({"prompts":[]}),
                        "tools/call" => {
                            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            serde_json::json!({"content":[{"type":"text","text":"found"}]})
                        }
                        _ => serde_json::json!({}),
                    };
                    (
                        "200 OK",
                        serde_json::to_vec(&serde_json::json!({"jsonrpc":"2.0","id":request["id"],"result":result})).unwrap(),
                    )
                }
                Some(_) => ("202 Accepted", Vec::new()),
                None => ("405 Method Not Allowed", Vec::new()),
            };
            let header = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                output.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(&output);
        }
    });
    (url, calls)
}

#[test]
fn list_and_check_show_the_saved_server_and_its_catalog_state() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let project_id = polaris_core::project::project_id(&project.path().canonicalize().unwrap());
    let store = polaris_mcp::store::Store::open(&home.path().canonicalize().unwrap()).unwrap();
    let server: polaris_mcp::registration::Server = serde_json::from_value(serde_json::json!({
        "server_id": "docs",
        "enabled_projects": [project_id],
        "transport": {"kind":"streamable_http","url":serve_one_tool().0,"secret_headers":{}},
        "capabilities": {"catalog_metadata":true,"resource_read":false,"prompt_selection":false,"tool_call":true}
    }))
    .unwrap();
    store
        .save_registry(
            0,
            &polaris_mcp::registration::Registry {
                schema_version: 1,
                revision: 1,
                servers: vec![server],
            },
        )
        .unwrap();
    let text = |output: &Output| String::from_utf8_lossy(&output.stdout).into_owned();
    let listed = polaris(home.path(), project.path(), &["mcp", "list"], "");
    assert!(
        text(&listed).contains("docs\t有効\tカタログ未確認"),
        "{}",
        text(&listed)
    );
    let checked = polaris(home.path(), project.path(), &["mcp", "check", "docs"], "");
    assert!(checked.status.success(), "{:?}", checked);
    assert!(text(&checked).contains("docs: ツール1件"));
    let listed = polaris(home.path(), project.path(), &["mcp", "list"], "");
    assert!(
        text(&listed).contains("docs\t有効\tツール1件"),
        "{}",
        text(&listed)
    );
    let other = tempfile::tempdir().unwrap();
    let elsewhere = polaris(home.path(), other.path(), &["mcp", "list"], "");
    assert!(text(&elsewhere).contains("docs\t無効"));
}

/// An OpenAI-compatible provider that searches, selects, calls the MCP tool,
/// then answers. The next step is read from the latest tool result.
fn serve_scripted_model() -> String {
    serve_recorded_model().0
}

fn serve_recorded_model() -> (
    String,
    std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
) {
    use std::io::Read;
    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut bytes = Vec::new();
            let mut part = [0; 65536];
            let request = loop {
                let n = stream.read(&mut part).unwrap_or(0);
                if n == 0 {
                    break None;
                }
                bytes.extend_from_slice(&part[..n]);
                if let Some(offset) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..offset]).to_ascii_lowercase();
                    let length: usize = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length: "))
                        .unwrap_or("0")
                        .parse()
                        .unwrap();
                    if bytes.len() >= offset + 4 + length {
                        break serde_json::from_slice::<serde_json::Value>(
                            &bytes[offset + 4..offset + 4 + length],
                        )
                        .ok();
                    }
                }
            };
            let Some(request) = request else { continue };
            recorded.lock().unwrap().push(request.clone());
            let selection = request["model"] == "gpt-5.6-luna";
            let last = request["messages"]
                .as_array()
                .and_then(|messages| messages.last())
                .cloned()
                .unwrap_or_default();
            let content = last["content"].as_str().unwrap_or_default();
            let call = |name: &str, arguments: serde_json::Value| serde_json::json!({"role":"assistant","content":null,"tool_calls":[{"id":format!("call-{name}"),"type":"function","function":{"name":name,"arguments":arguments.to_string()}}]});
            let alias = request["tools"].as_array().and_then(|tools| {
                tools
                    .iter()
                    .filter_map(|tool| tool["function"]["name"].as_str())
                    .find(|name| {
                        name.starts_with("mcp_") && *name != "mcp_catalog" && *name != "mcp_setup"
                    })
            });
            let message = if selection {
                serde_json::json!({"role":"assistant","content":"{\"tools\":[0]}"})
            } else if last["role"] != "tool" && alias.is_some() {
                call(alias.unwrap(), serde_json::json!({}))
            } else if last["role"] != "tool" {
                call(
                    "mcp_catalog",
                    serde_json::json!({"op":"search","q":"lookup","server":"docs"}),
                )
            } else if content.contains("\"candidates\"") {
                let found: serde_json::Value = serde_json::from_str(content).unwrap();
                let candidate = &found["candidates"][0];
                call(
                    "mcp_catalog",
                    serde_json::json!({"op":"select","server":"docs","kind":"tool","key":candidate["key"],"generation":candidate["generation"]}),
                )
            } else if content.contains("selected_for_next_request") {
                let alias = request["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|tool| tool["function"]["name"].as_str())
                    .find(|name| {
                        name.starts_with("mcp_") && *name != "mcp_catalog" && *name != "mcp_setup"
                    })
                    .unwrap()
                    .to_owned();
                call(&alias, serde_json::json!({}))
            } else {
                serde_json::json!({"role":"assistant","content":format!("done {content}")})
            };
            let usage = if selection {
                serde_json::json!({"prompt_tokens":7,"completion_tokens":11,"total_tokens":18})
            } else {
                serde_json::json!({"prompt_tokens":1,"completion_tokens":1,"total_tokens":2})
            };
            let output = serde_json::to_vec(&serde_json::json!({
                "choices":[{"message":message}],
                "usage":usage
            }))
            .unwrap();
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                output.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(&output);
        }
    });
    (url, requests)
}

#[test]
fn one_shot_preselects_once_with_an_independent_model_and_counts_its_usage() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let home_path = home.path().canonicalize().unwrap();
    let project_id = polaris_core::project::project_id(&project.path().canonicalize().unwrap());
    let (tool_url, calls) = serve_one_tool();
    let store = polaris_mcp::store::Store::open(&home_path).unwrap();
    let server = serde_json::from_value(serde_json::json!({
        "server_id":"docs", "enabled_projects":[project_id],
        "transport":{"kind":"streamable_http","url":tool_url,"secret_headers":{}},
        "capabilities":{"catalog_metadata":true,"tool_call":true,"resource_read":false,"prompt_selection":false}
    })).unwrap();
    store
        .save_registry(
            0,
            &polaris_mcp::registration::Registry {
                schema_version: 1,
                revision: 1,
                servers: vec![server],
            },
        )
        .unwrap();
    let checked = polaris(&home_path, project.path(), &["mcp", "check", "docs"], "");
    assert!(checked.status.success(), "{checked:?}");
    let (url, requests) = serve_recorded_model();
    let mut child = product_binary::command()
        .args(["--mcp-external", "-p", "look it up"])
        .current_dir(project.path())
        .env("HOME", &home_path)
        .env_remove("POLARIS_DATA_DIR")
        .env_remove("POLARIS_PROVIDER")
        .env("POLARIS_API_KEY", "synthetic-key")
        .env("POLARIS_BASE_URL", url)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"y\n").unwrap();
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{output:?}");
    let requests = requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r["model"] == "gpt-5.6-luna")
            .count(),
        1,
        "the fixed selector must run once before the main model"
    );
    assert_eq!(requests.len(), 3, "selector, main tool call, main answer");
    assert_eq!(requests[0]["model"], "gpt-5.6-luna");
    assert_eq!(requests[1]["model"], requests[2]["model"]);
    assert_ne!(requests[1]["model"], "gpt-5.6-luna");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(stderr.contains("/ total 22 "), "{stderr}");
}

#[test]
fn one_shot_runs_ask_at_the_terminal_before_an_mcp_tool_call() {
    for (answer, sent) in [("n\n", 0), ("y\n", 1)] {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join(".git")).unwrap();
        let project_id = polaris_core::project::project_id(&project.path().canonicalize().unwrap());
        let (tool_url, calls) = serve_one_tool();
        let store = polaris_mcp::store::Store::open(&home.path().canonicalize().unwrap()).unwrap();
        let server: polaris_mcp::registration::Server =
            serde_json::from_value(serde_json::json!({
                "server_id": "docs",
                "enabled_projects": [project_id],
                "transport": {"kind":"streamable_http","url":tool_url,"secret_headers":{}},
                "capabilities": {"catalog_metadata":true,"resource_read":false,"prompt_selection":false,"tool_call":true}
            }))
            .unwrap();
        store
            .save_registry(
                0,
                &polaris_mcp::registration::Registry {
                    schema_version: 1,
                    revision: 1,
                    servers: vec![server],
                },
            )
            .unwrap();
        let mut child = product_binary::command()
            .args(["--mcp-external", "-p", "look it up"])
            .current_dir(project.path())
            .env("HOME", home.path().canonicalize().unwrap())
            .env_remove("POLARIS_DATA_DIR")
            .env_remove("POLARIS_PROVIDER")
            .env("POLARIS_API_KEY", "synthetic-key")
            .env("POLARIS_BASE_URL", serve_scripted_model())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(answer.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stdout}\n{stderr}");
        assert!(stderr.contains("docs"), "{stderr}");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            sent,
            "{stdout}"
        );
        if sent == 0 {
            assert!(stdout.contains("mcp_call_denied"), "{stdout}");
        } else {
            assert!(stdout.contains("completed"), "{stdout}");
        }
    }
}
