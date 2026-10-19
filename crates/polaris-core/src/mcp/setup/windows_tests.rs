//! Windows MCP setup refusal tests that preserve storage and avoid approval requests.
use super::*;
use crate::approval::{ApprovalPolicy, Approver, Decision, Gate};
use crate::isolated_workspace::Limits;
use crate::mcp::operation::ExternalCeiling;
use serde_json::json;

struct Unasked;

impl Approver for Unasked {
    fn ask(&mut self, _reason: &str) -> Decision {
        panic!("unsupported setup must not request approval")
    }
}

fn tool(home: PathBuf, runtime: PathBuf) -> SetupTool {
    SetupTool::new(
        home.clone(),
        vec![runtime],
        vec![home],
        Limits {
            max_entries: 32,
            max_files: 16,
            max_file_bytes: 4096,
            max_total_bytes: 8192,
            max_depth: 4,
        },
        "project".into(),
        Authorization::Local(ExternalCeiling::Allowed),
    )
}

async fn assert_unavailable(tool: &SetupTool) {
    let mut gate = Gate::new(ApprovalPolicy::Always);
    let mut approver = Unasked;
    for arguments in [
        json!({ "op": "detect" }),
        json!({ "op": "register", "candidate": "fixture" }),
        json!({ "op": "unknown" }),
        json!({}),
        json!(null),
    ] {
        let cancelled = std::future::poll_fn::<(), _>(|_| {
            panic!("unsupported setup must not poll cancellation")
        });
        let error = tool
            .call(&arguments, cancelled, &mut gate, &mut approver)
            .await
            .err()
            .expect("unsupported setup must fail");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&error).unwrap(),
            json!({ "error": "unavailable" })
        );
    }
}

fn names(path: &Path) -> Vec<std::ffi::OsString> {
    let mut names: Vec<_> = std::fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn windows_mcp_setup_unavailable_does_not_create_home_or_runtime() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("absent-home");
    let runtime = root.path().join("absent-runtime");
    let before = names(root.path());
    let tool = tool(home.clone(), runtime.clone());
    assert!(matches!(
        protect_sources(&home),
        Err(polaris_mcp::Error::Protection)
    ));
    assert_unavailable(&tool).await;
    assert!(!home.exists());
    assert!(!runtime.exists());
    assert_eq!(names(root.path()), before);
}

#[tokio::test]
async fn windows_mcp_setup_unavailable_preserves_source_store_and_runtime() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let codex = home.join(".codex");
    let polaris = home.join(".polaris");
    let mcp = polaris.join("mcp");
    let runtime = root.path().join("runtime");
    let nested = runtime.join("nested");
    std::fs::create_dir_all(&codex).unwrap();
    std::fs::create_dir_all(&mcp).unwrap();
    std::fs::create_dir_all(&nested).unwrap();
    let files = [
        codex.join("config.toml"),
        home.join(".claude.json"),
        mcp.join("registry.json"),
        nested.join("server.exe"),
    ];
    let contents: [&[u8]; 4] = [
        b"[mcp_servers.fixture]\ncommand = 'public-fixture'\n",
        b"{\"mcpServers\":{\"fixture\":{\"command\":\"public-fixture\"}}}",
        b"{\"schema_version\":1,\"revision\":1,\"servers\":[]}",
        b"retained\0\xffserver",
    ];
    for (path, bytes) in files.iter().zip(contents) {
        std::fs::write(path, bytes).unwrap();
    }
    let directories = [
        root.path(),
        &home,
        &codex,
        &polaris,
        &mcp,
        &runtime,
        &nested,
    ];
    let before: Vec<_> = directories.iter().map(|path| names(path)).collect();
    let tool = tool(home.clone(), runtime.clone());
    assert!(matches!(
        protect_sources(&home),
        Err(polaris_mcp::Error::Protection)
    ));
    assert_unavailable(&tool).await;
    for (path, bytes) in files.iter().zip(contents) {
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
    let after: Vec<_> = directories.iter().map(|path| names(path)).collect();
    assert_eq!(after, before);
}
