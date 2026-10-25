//! End-to-end TUI coverage for the MCP management owners.

use super::*;
use crate::approver::SecretInputEvent;
use polaris_core::mcp::credentials::{
    McpCredentialOwnerV1, McpSecretReferenceV1, McpSecretSetRequestV1,
};
use polaris_mcp::{
    registration::{Capabilities, Destination, ProtocolProfile, Registry, Server, Timeouts},
    store::Store,
};
use ratatui::{backend::TestBackend, crossterm::event::KeyCode};
use std::{
    cell::RefCell,
    collections::VecDeque,
    io::{Read, Write},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use zeroize::Zeroizing;

const CHILD: &str = "POLARIS_MCP_MANAGEMENT_TUI_CHILD";

struct Keys(VecDeque<KeyCode>);

impl KeyReader for Keys {
    fn read_key(&mut self) -> std::io::Result<KeyCode> {
        self.0
            .pop_front()
            .ok_or_else(|| std::io::Error::other("keys exhausted"))
    }
}

fn in_child(name: &str) -> bool {
    if std::env::var_os(CHILD).is_some() {
        return true;
    }
    let home = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([name, "--exact", "--nocapture"])
        .env(CHILD, "1")
        .env("HOME", home.path())
        .env_remove("POLARIS_DATA_DIR")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    false
}

async fn fixture() -> (
    std::path::PathBuf,
    std::path::PathBuf,
    WorkspaceOwner,
    ManagementEffectStoreV1,
) {
    let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap())
        .canonicalize()
        .unwrap();
    polaris_auth::protection::use_trusted_home(&home).unwrap();
    let project = home.join("project");
    std::fs::create_dir(&project).unwrap();
    let state = polaris_core::project::state_dir(&project).unwrap();
    let workspace = WorkspaceOwner::open(
        &project,
        &state.join("rewind-workspace"),
        Default::default(),
    )
    .unwrap();
    let lease = workspace.try_mutation().unwrap();
    let effects = ManagementEffectStoreV1::open_at_state_dir(&project, &state, &lease).unwrap();
    drop(lease);
    (home, project, workspace, effects)
}

fn terminal() -> RefCell<Terminal<TestBackend>> {
    RefCell::new(Terminal::new(TestBackend::new(140, 35)).unwrap())
}

fn screen(terminal: &RefCell<Terminal<TestBackend>>) -> String {
    let terminal = terminal.borrow();
    let buffer = terminal.backend().buffer();
    buffer.content.iter().map(|cell| cell.symbol()).collect()
}

/// The TUI draws immediately before each secret read.  This reader ensures
/// that neither the bytes nor their decimal length reaches the frame.
struct MaskedKeys<'a> {
    terminal: &'a RefCell<Terminal<TestBackend>>,
    keys: VecDeque<KeyCode>,
    secret: &'a str,
}

impl KeyReader for MaskedKeys<'_> {
    fn read_key(&mut self) -> std::io::Result<KeyCode> {
        self.keys
            .pop_front()
            .ok_or_else(|| std::io::Error::other("keys exhausted"))
    }

    fn read_secret_key(&mut self) -> std::io::Result<KeyCode> {
        let frame = screen(self.terminal);
        assert!(!frame.contains(self.secret));
        assert!(!frame.contains(&self.secret.chars().count().to_string()));
        self.read_key()
    }
}

struct SecretEvents(VecDeque<SecretInputEvent>);

impl KeyReader for SecretEvents {
    fn read_key(&mut self) -> std::io::Result<KeyCode> {
        panic!("the dedicated secret reader must be used")
    }

    fn read_secret_input(&mut self) -> std::io::Result<SecretInputEvent> {
        self.0
            .pop_front()
            .ok_or_else(|| std::io::Error::other("secret events exhausted"))
    }
}

#[test]
fn tui_mcp_secret_overflow_drains_queued_input_before_returning() {
    use polaris_mcp::secret_input::MAX_SECRET_INPUT_BYTES_V1;

    const TAIL: &str = "synthetic-tail-must-never-reach-the-composer";
    for pasted in [false, true] {
        for boundary in [KeyCode::Enter, KeyCode::Esc] {
            let terminal = RefCell::new(Terminal::new(TestBackend::new(12, 3)).unwrap());
            let value = "🦀".repeat(MAX_SECRET_INPUT_BYTES_V1 / 4 + 1);
            let mut events = VecDeque::new();
            if pasted {
                events.push_back(SecretInputEvent::Paste(Zeroizing::new(value)));
            } else {
                events.extend(
                    value
                        .chars()
                        .map(|c| SecretInputEvent::Key(KeyCode::Char(c))),
                );
            }
            events.extend(
                TAIL.chars()
                    .map(|c| SecretInputEvent::Key(KeyCode::Char(c))),
            );
            events.push_back(SecretInputEvent::Paste(Zeroizing::new(TAIL.into())));
            events.push_back(SecretInputEvent::Key(boundary));
            let mut reader = SecretEvents(events);
            let error = secret::read_value(&terminal, &mut reader).unwrap_err();
            assert_eq!(error, "MCP secret input exceeds the limit");
            assert!(
                reader.0.is_empty(),
                "secret tail and boundary must be consumed"
            );
            assert!(!screen(&terminal).contains(TAIL));
            assert!(!screen(&terminal).contains("🦀"));
            assert!(!screen(&terminal).contains(&MAX_SECRET_INPUT_BYTES_V1.to_string()));
        }
    }
}

#[test]
fn tui_mcp_secret_paste_moves_only_through_the_typed_owner() {
    let terminal = terminal();
    let secret = "synthetic-pasted-token";
    let mut pasted = SecretEvents(VecDeque::from([
        SecretInputEvent::Paste(Zeroizing::new(secret.into())),
        SecretInputEvent::Key(KeyCode::Enter),
    ]));
    let input = secret::read_value(&terminal, &mut pasted).unwrap().unwrap();
    let home = tempfile::tempdir().unwrap();
    let store = Store::open(home.path()).unwrap();
    let changed = McpCredentialOwnerV1::new(&store)
        .set(McpSecretSetRequestV1::new(
            0,
            McpSecretReferenceV1::parse("pasted").unwrap(),
            input,
        ))
        .unwrap();
    assert_eq!(changed.secret_revision, 1);
    assert_eq!(
        store
            .load_secrets()
            .unwrap()
            .unwrap()
            .resolve("pasted")
            .unwrap(),
        secret
    );
    assert!(!screen(&terminal).contains(secret));

    let forbidden = "synthetic-paste-with-newline\n";
    let mut invalid = SecretEvents(VecDeque::from([SecretInputEvent::Paste(Zeroizing::new(
        forbidden.into(),
    ))]));
    let error = match secret::read_value(&terminal, &mut invalid) {
        Err(error) => error,
        Ok(_) => panic!("a control character pasted into a secret must be rejected"),
    };
    assert!(!error.contains(forbidden));
    assert!(!screen(&terminal).contains(forbidden));

    let mut cancelled = SecretEvents(VecDeque::from([SecretInputEvent::Key(KeyCode::Esc)]));
    assert!(
        secret::read_value(&terminal, &mut cancelled)
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn tui_mcp_secret_lifecycle_is_masked_versioned_and_irreversible() {
    const NAME: &str = "mcp_commands::management_tests::tui_mcp_secret_lifecycle_is_masked_versioned_and_irreversible";
    if !in_child(NAME) {
        return;
    }
    let (home, project, workspace, effects) = fixture().await;
    let terminal = terminal();
    let secret = "synthetic-super-secret";
    let baseline = {
        let lease = workspace.try_mutation().unwrap();
        ManagementEffectStoreV1::rewind_checkpoint_for_project_at_state_dir(
            &project,
            effects.state_dir(),
            &lease,
        )
        .unwrap()
    };
    let host = LocalHost::open_for_test(&project, &home, false).unwrap();
    let mut never = Keys(VecDeque::from([KeyCode::Char('x')]));
    assert!(
        handle_host(
            "secret set",
            &terminal,
            &mut never,
            (&workspace, &effects),
            &host,
            (false, ApprovalPolicy::Never),
            false,
        )
        .await
        .is_err()
    );
    assert_eq!(never.0.len(), 1, "Never must not open a secret input frame");
    assert!(!Store::exists(&home));
    drop(host);
    let mut set = MaskedKeys {
        terminal: &terminal,
        keys: "docs_token"
            .chars()
            .map(KeyCode::Char)
            .chain(std::iter::once(KeyCode::Enter))
            .chain(secret.chars().map(KeyCode::Char))
            .chain([KeyCode::Enter, KeyCode::Char('y')])
            .collect(),
        secret,
    };
    let host = LocalHost::open_for_test(&project, &home, true).unwrap();
    let saved = handle_host(
        "secret set",
        &terminal,
        &mut set,
        (&workspace, &effects),
        &host,
        (false, ApprovalPolicy::Always),
        false,
    )
    .await
    .unwrap();
    let text = saved.iter().map(ToString::to_string).collect::<String>();
    assert!(text.contains("revision 1"));
    assert!(!text.contains(secret));
    assert!(!screen(&terminal).contains(secret));
    drop(host);

    // A fresh host observes the durable store created by the set route.
    let host = LocalHost::open_for_test(&project, &home, false).unwrap();
    let secrets = host.store().unwrap().load_secrets().unwrap().unwrap();
    assert_eq!(secrets.revision(), 1);
    assert_eq!(secrets.resolve("docs_token").unwrap(), secret);
    let listed = state_lines(&host, false)
        .unwrap()
        .iter()
        .map(ToString::to_string)
        .collect::<String>();
    assert!(listed.contains("Credential reference: docs_token"));
    assert!(!listed.contains(secret));
    let lease = workspace.try_mutation().unwrap();
    let through = ManagementEffectStoreV1::rewind_checkpoint_for_project_at_state_dir(
        &project,
        effects.state_dir(),
        &lease,
    )
    .unwrap();
    assert!(matches!(
        ManagementEffectStoreV1::verify_rewind_crossing_for_project_at_state_dir(
            &project,
            effects.state_dir(),
            &lease,
            &baseline,
            &through,
            true,
        )
        .unwrap(),
        polaris_core::management_effect::ManagementRewindCrossingV1::Blocked { inventory, .. }
            if inventory.encoded() == "C24"
    ));
    drop(lease);
    let mut delete = Keys(VecDeque::from([KeyCode::Char('y')]));
    let deleted = handle_host(
        "secret delete docs_token",
        &terminal,
        &mut delete,
        (&workspace, &effects),
        &host,
        (false, ApprovalPolicy::Always),
        false,
    )
    .await
    .unwrap();
    assert!(deleted[0].to_string().contains("revision 2"));
    let secrets = host.store().unwrap().load_secrets().unwrap().unwrap();
    assert_eq!(secrets.revision(), 2);
    assert!(secrets.resolve("docs_token").is_err());
    assert!(effects.blocker().unwrap().is_none());

    let mut cancelled = Keys(VecDeque::from([KeyCode::Esc]));
    assert_eq!(
        handle_host(
            "secret set",
            &terminal,
            &mut cancelled,
            (&workspace, &effects),
            &host,
            (false, ApprovalPolicy::Always),
            false,
        )
        .await
        .unwrap()[0]
            .to_string(),
        "MCP secret input cancelled"
    );
    for request in ["secret set inline", "secret delete", "secret delete absent"] {
        let mut keys = Keys(VecDeque::new());
        assert!(
            handle_host(
                request,
                &terminal,
                &mut keys,
                (&workspace, &effects),
                &host,
                (false, ApprovalPolicy::Always),
                false,
            )
            .await
            .is_err(),
            "{request}"
        );
    }
}

fn write_detectable_config(home: &std::path::Path, token: &str) {
    let config = home.join(".codex/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(
        config,
        format!(
            "[mcp_servers.docs]\nurl = \"https://docs.example.com/mcp\"\nhttp_headers = {{ authorization = \"Bearer {token}\" }}\n"
        ),
    )
    .unwrap();
}

struct MutateAtConfirmation {
    keys: VecDeque<KeyCode>,
    config: std::path::PathBuf,
}

impl KeyReader for MutateAtConfirmation {
    fn read_key(&mut self) -> std::io::Result<KeyCode> {
        let key = self
            .keys
            .pop_front()
            .ok_or_else(|| std::io::Error::other("keys exhausted"))?;
        if key == KeyCode::Char('y') {
            std::fs::write(
                &self.config,
                "[mcp_servers.docs]\nurl = \"https://docs.example.com/mcp\"\nhttp_headers = { authorization = \"Bearer changed-after-card\" }\n",
            )
            .unwrap();
        }
        Ok(key)
    }
}

#[tokio::test]
async fn tui_mcp_register_requires_ceiling_approval_and_fresh_credential_source() {
    const NAME: &str = "mcp_commands::management_tests::tui_mcp_register_requires_ceiling_approval_and_fresh_credential_source";
    if !in_child(NAME) {
        return;
    }
    let (home, project, workspace, effects) = fixture().await;
    write_detectable_config(&home, "initial-token");
    let terminal = terminal();

    for (external_enabled, policy) in [
        (false, ApprovalPolicy::Always),
        (true, ApprovalPolicy::Never),
    ] {
        let host = LocalHost::open_for_test(&project, &home, false).unwrap();
        let mut keys = Keys(VecDeque::new());
        let result = handle_host(
            "register docs",
            &terminal,
            &mut keys,
            (&workspace, &effects),
            &host,
            (external_enabled, policy),
            false,
        )
        .await;
        assert!(result.is_err());
        assert!(!Store::exists(&home));
        assert!(effects.blocker().unwrap().is_none());
    }

    let host = LocalHost::open_for_test(&project, &home, false).unwrap();
    let mut cancelled = Keys(VecDeque::from([KeyCode::End, KeyCode::Esc]));
    assert!(
        handle_host(
            "register docs",
            &terminal,
            &mut cancelled,
            (&workspace, &effects),
            &host,
            (true, ApprovalPolicy::Always),
            false,
        )
        .await
        .unwrap()[0]
            .to_string()
            .contains("cancelled")
    );
    assert!(!Store::exists(&home));

    let host = LocalHost::open_for_test(&project, &home, false).unwrap();
    let mut stale = MutateAtConfirmation {
        keys: VecDeque::from([KeyCode::End, KeyCode::Char('y')]),
        config: home.join(".codex/config.toml"),
    };
    let result = handle_host(
        "register docs",
        &terminal,
        &mut stale,
        (&workspace, &effects),
        &host,
        (true, ApprovalPolicy::Always),
        false,
    )
    .await;
    assert!(result.is_err());
    let store = Store::open(&home).unwrap();
    assert!(store.load_registry().unwrap().is_none());
    assert!(store.load_secrets().unwrap().is_none());
    assert!(
        effects.blocker().unwrap().is_some(),
        "accepted uncertain registration is retained"
    );
}

#[tokio::test]
async fn tui_mcp_register_persists_server_and_bound_secret_after_confirmation() {
    const NAME: &str = "mcp_commands::management_tests::tui_mcp_register_persists_server_and_bound_secret_after_confirmation";
    if !in_child(NAME) {
        return;
    }
    let (home, project, workspace, effects) = fixture().await;
    write_detectable_config(&home, "registered-token");
    let terminal = terminal();
    let host = LocalHost::open_for_test(&project, &home, false).unwrap();
    let mut keys = Keys(VecDeque::from([KeyCode::End, KeyCode::Char('y')]));
    let output = handle_host(
        "register docs",
        &terminal,
        &mut keys,
        (&workspace, &effects),
        &host,
        (true, ApprovalPolicy::Always),
        false,
    )
    .await
    .unwrap();
    assert!(output[0].to_string().contains("Registered docs"));
    drop(host);

    let reopened = LocalHost::open_for_test(&project, &home, false).unwrap();
    let registry = reopened.store().unwrap().load_registry().unwrap().unwrap();
    assert_eq!(registry.revision, 1);
    assert_eq!(registry.servers.len(), 1);
    let server = &registry.servers[0];
    assert!(server.server_id.starts_with("docs-"));
    assert_eq!(server.server_id.len(), "docs-".len() + 16);
    assert!(server.enabled_projects.contains(reopened.project()));
    let reference = match &server.transport {
        Destination::StreamableHttp { secret_headers, .. } => &secret_headers["authorization"],
        _ => panic!("the detected HTTP configuration must remain HTTP"),
    };
    assert_eq!(
        reopened
            .store()
            .unwrap()
            .load_secrets()
            .unwrap()
            .unwrap()
            .resolve(reference)
            .unwrap(),
        "Bearer registered-token"
    );
    assert!(effects.blocker().unwrap().is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn tui_mcp_materialize_pins_a_fixture_without_package_output() {
    use std::os::unix::fs::PermissionsExt;

    const NAME: &str =
        "mcp_commands::management_tests::tui_mcp_materialize_pins_a_fixture_without_package_output";
    const MARKER: &str = "fixture-package-output-must-not-reach-tui";
    if std::env::var_os(CHILD).is_none() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().canonicalize().unwrap();
        std::fs::create_dir(home.join(".codex")).unwrap();
        std::fs::write(
            home.join(".codex/config.toml"),
            "[mcp_servers.docs]\ncommand = 'npx'\nargs = ['@fixture/docs@1.2.3']\n",
        )
        .unwrap();
        let bin = home.join("fixture-bin");
        std::fs::create_dir(&bin).unwrap();
        let npm = bin.join("npm");
        std::fs::write(&npm, format!("#!/bin/sh\nset -eu\nprintf '%s\\n' '{MARKER}'\nprintf '%s\\n' '{MARKER}' >&2\n/bin/mkdir -p \"$3/node_modules/@fixture/docs\"\nprintf '%s' '{{\"name\":\"@fixture/docs\",\"version\":\"1.2.3\",\"bin\":\"index.js\"}}' > \"$3/node_modules/@fixture/docs/package.json\"\nprintf '%s' 'fixture-entry' > \"$3/node_modules/@fixture/docs/index.js\"\n")).unwrap();
        std::fs::set_permissions(&npm, std::fs::Permissions::from_mode(0o700)).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([NAME, "--exact", "--nocapture"])
            .env(CHILD, "materialize")
            .env("HOME", &home)
            .env("PATH", &bin)
            .env_remove("POLARIS_DATA_DIR")
            .output()
            .unwrap();
        let visible = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.status.success(), "{visible}");
        assert!(!visible.contains(MARKER));
        return;
    }
    let (home, project, workspace, effects) = fixture().await;
    let terminal = terminal();
    let host = LocalHost::open_for_test(&project, &home, false).unwrap();
    let mut keys = Keys(VecDeque::from([KeyCode::End, KeyCode::Char('y')]));
    let output = handle_host(
        "materialize docs --runtime /bin/sh",
        &terminal,
        &mut keys,
        (&workspace, &effects),
        &host,
        (true, ApprovalPolicy::Always),
        false,
    )
    .await
    .unwrap();
    assert!(output[0].to_string().contains("@fixture/docs 1.2.3"));
    let directory = std::fs::read_dir(home.join(".polaris/mcp/servers"))
        .unwrap()
        .map(Result::unwrap)
        .map(|entry| entry.path())
        .find(|path| path.is_dir())
        .unwrap();
    let expected = polaris_core::mcp::setup::launcher::Pinned {
        schema_version: 1,
        package: "@fixture/docs".into(),
        version: "1.2.3".into(),
        runtime: "/bin/sh".into(),
        entry: directory
            .join("node_modules/@fixture/docs/index.js")
            .canonicalize()
            .unwrap(),
    };
    assert_eq!(
        std::fs::read(directory.join("pinned.json")).unwrap(),
        serde_json::to_vec_pretty(&expected).unwrap()
    );
    assert!(effects.blocker().unwrap().is_none());
}

fn serve_catalog() -> (String, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            counted.fetch_add(1, Ordering::SeqCst);
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

#[tokio::test]
async fn tui_mcp_check_persists_catalog_and_denied_ceiling_never_connects() {
    const NAME: &str = "mcp_commands::management_tests::tui_mcp_check_persists_catalog_and_denied_ceiling_never_connects";
    if !in_child(NAME) {
        return;
    }
    let (home, project, workspace, effects) = fixture().await;
    let (url, requests) = serve_catalog();
    let host = LocalHost::open_for_test(&project, &home, true).unwrap();
    host.store()
        .unwrap()
        .save_registry(
            0,
            &Registry {
                schema_version: 1,
                revision: 1,
                servers: vec![Server {
                    server_id: "docs".into(),
                    enabled_projects: [host.project().to_owned()].into(),
                    transport: Destination::StreamableHttp {
                        url,
                        secret_headers: Default::default(),
                    },
                    protocol_profile: ProtocolProfile::default(),
                    capabilities: Capabilities {
                        catalog_metadata: true,
                        ..Capabilities::default()
                    },
                    timeouts: Timeouts::default(),
                }],
            },
        )
        .unwrap();
    let terminal = terminal();
    let mut denied = Keys(VecDeque::new());
    assert!(
        handle_host(
            "check docs",
            &terminal,
            &mut denied,
            (&workspace, &effects),
            &host,
            (false, ApprovalPolicy::Always),
            false,
        )
        .await
        .is_err()
    );
    assert_eq!(requests.load(Ordering::SeqCst), 0);

    let cursor_before = effects.cursor().unwrap();
    let catalog_before = host.store().unwrap().load_catalog_index().unwrap();
    let mut never = Keys(VecDeque::new());
    assert!(
        handle_host(
            "check docs",
            &terminal,
            &mut never,
            (&workspace, &effects),
            &host,
            (true, ApprovalPolicy::Never),
            false,
        )
        .await
        .is_err()
    );
    assert_eq!(requests.load(Ordering::SeqCst), 0);
    assert_eq!(effects.cursor().unwrap(), cursor_before);
    assert_eq!(
        host.store().unwrap().load_catalog_index().unwrap(),
        catalog_before
    );

    let mut allowed = Keys(VecDeque::from([KeyCode::End, KeyCode::Char('y')]));
    let output = handle_host(
        "check docs",
        &terminal,
        &mut allowed,
        (&workspace, &effects),
        &host,
        (true, ApprovalPolicy::Always),
        false,
    )
    .await
    .unwrap();
    assert!(output[0].to_string().contains("1 tools"));
    assert!(requests.load(Ordering::SeqCst) > 0);
    let index = host.store().unwrap().load_catalog_index().unwrap().unwrap();
    assert_eq!(index.servers["docs"].tools[0].name, "lookup");
    assert!(effects.blocker().unwrap().is_none());
}
