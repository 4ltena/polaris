//! MCP controls change next-run admission; they never replace a running runtime.
use crate::{account::confirm, approver::KeyReader, render};
use polaris_core::{
    management_effect::{ManagementEffectStoreV1, mcp_change::McpRegistryChangeOwnerV1},
    mcp::{local::LocalHost, operation::ExternalCeiling},
    rewind_workspace::WorkspaceOwner,
};
use ratatui::{Terminal, backend::Backend, text::Line};
use std::{cell::RefCell, path::Path};

pub(crate) fn state_lines(
    host: &LocalHost,
    external_enabled: bool,
) -> Result<Vec<Line<'static>>, String> {
    let mut lines = vec![
        Line::from(format!(
            "外部MCPホスト上限: {}",
            if external_enabled { "許可" } else { "拒否" }
        )),
        Line::from(if external_enabled {
            "外部MCPを拒否するには --mcp-external を付けずに再起動してください。"
        } else {
            "外部MCPを許可するには --mcp-external を指定して再起動してください。"
        }),
        Line::from("以下は登録・入場状態です。接続成功や実行中runtimeの変更を意味しません。"),
    ];
    let states = host
        .state_report(ExternalCeiling::host_external(external_enabled))
        .map_err(|_| "MCPの登録・入場状態を読めません")?;
    let registry = match host.store() {
        Some(store) => store
            .load_registry()
            .map_err(|_| "MCP registryを読めません")?,
        None => None,
    };
    if registry.as_ref().is_some_and(|registry| {
        registry.servers.len() != states.len()
            || states.iter().any(|state| {
                !registry.servers.iter().any(|server| {
                    server.server_id == state.server_id
                        && server.enabled_projects.contains(host.project()) == state.enabled_here
                })
            })
    }) {
        return Err("閲覧中にMCP registryが変更されました。再表示してください".into());
    }
    if let Some(registry) = &registry {
        lines.push(Line::from(format!("登録revision {}", registry.revision)));
    }
    if states.is_empty() {
        lines.push(Line::from("登録されたMCPサーバーはありません。"));
        return Ok(lines);
    }
    lines.push(Line::from(format!(
        "MCPサーバー {}件（最大32件）",
        states.len()
    )));
    for state in states {
        let status = match (&state.quarantine, state.enabled_here) {
            (Some(reason), _) => format!("使えません（{}）", render::sanitize(reason)),
            (None, false) => "このプロジェクトでは無効".to_owned(),
            (None, true) => "有効".to_owned(),
        };
        lines.push(Line::from(format!(
            "{} — {status}",
            render::sanitize(&state.server_id)
        )));
        if let Some(server) = registry.as_ref().and_then(|registry| {
            registry
                .servers
                .iter()
                .find(|server| server.server_id == state.server_id)
        }) {
            lines.push(Line::from(format!(
                "  catalog={} resource={} prompt={} tool={} / timeout connect={}s catalog={}s operation={}s",
                server.capabilities.catalog_metadata, server.capabilities.resource_read,
                server.capabilities.prompt_selection, server.capabilities.tool_call,
                server.timeouts.connect_seconds, server.timeouts.catalog_seconds, server.timeouts.operation_seconds
            )));
        }
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_mcp::registration::{
        Capabilities, Destination, ProtocolProfile, Registry, Server, Timeouts,
    };
    use ratatui::{backend::TestBackend, crossterm::event::KeyCode};
    use std::collections::VecDeque;

    struct Keys(VecDeque<KeyCode>);
    impl KeyReader for Keys {
        fn read_key(&mut self) -> std::io::Result<KeyCode> {
            self.0
                .pop_front()
                .ok_or_else(|| std::io::Error::other("keys exhausted"))
        }
    }

    #[test]
    fn m3d_mcp_tui_registry_confirmation_ceiling_and_denial() {
        if std::env::var_os("POLARIS_M3D_TUI_CHILD").is_none() {
            let home = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "mcp_commands::tests::m3d_mcp_tui_registry_confirmation_ceiling_and_denial",
                    "--exact",
                    "--nocapture",
                ])
                .env("POLARIS_M3D_TUI_CHILD", "1")
                .env("HOME", home.path())
                .env_remove("POLARIS_DATA_DIR")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
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
        let host = LocalHost::open_for_test(&project, &home, true).unwrap();
        host.store()
            .unwrap()
            .save_registry(
                0,
                &Registry {
                    schema_version: 1,
                    revision: 1,
                    servers: vec![Server {
                        server_id: "fixture".into(),
                        enabled_projects: Default::default(),
                        transport: Destination::StreamableHttp {
                            url: "https://example.com/mcp".into(),
                            secret_headers: Default::default(),
                        },
                        protocol_profile: ProtocolProfile::default(),
                        capabilities: Capabilities::default(),
                        timeouts: Timeouts::default(),
                    }],
                },
            )
            .unwrap();
        let terminal = RefCell::new(Terminal::new(TestBackend::new(140, 25)).unwrap());
        let mut yes = Keys(VecDeque::from([KeyCode::Char('y')]));
        let output = handle_host(
            "enable fixture",
            &terminal,
            &mut yes,
            &workspace,
            &effects,
            &host,
            false,
            false,
        )
        .unwrap();
        assert!(output[0].to_string().contains("次の実行"));
        assert!(
            host.store()
                .unwrap()
                .load_registry()
                .unwrap()
                .unwrap()
                .servers[0]
                .enabled_projects
                .contains(host.project())
        );
        let mut no = Keys(VecDeque::from([KeyCode::Esc]));
        handle_host(
            "disable fixture",
            &terminal,
            &mut no,
            &workspace,
            &effects,
            &host,
            false,
            false,
        )
        .unwrap();
        assert!(
            host.store()
                .unwrap()
                .load_registry()
                .unwrap()
                .unwrap()
                .servers[0]
                .enabled_projects
                .contains(host.project())
        );
        let mut yes = Keys(VecDeque::from([KeyCode::Char('y')]));
        handle_host(
            "disable fixture",
            &terminal,
            &mut yes,
            &workspace,
            &effects,
            &host,
            false,
            false,
        )
        .unwrap();
        assert!(
            !host
                .store()
                .unwrap()
                .load_registry()
                .unwrap()
                .unwrap()
                .servers[0]
                .enabled_projects
                .contains(host.project())
        );
        let mut keys = Keys(VecDeque::new());
        assert!(
            handle_host(
                "enable missing",
                &terminal,
                &mut keys,
                &workspace,
                &effects,
                &host,
                false,
                false
            )
            .is_err()
        );
        assert!(
            handle_host(
                "enable fixture",
                &terminal,
                &mut keys,
                &workspace,
                &effects,
                &host,
                false,
                true
            )
            .is_err()
        );
        assert!(
            handle_host(
                "secret delete fixture",
                &terminal,
                &mut keys,
                &workspace,
                &effects,
                &host,
                false,
                false
            )
            .is_err()
        );
        assert!(
            handle_host(
                "check fixture",
                &terminal,
                &mut keys,
                &workspace,
                &effects,
                &host,
                false,
                false
            )
            .is_err()
        );
        let text = state_lines(&host, false)
            .unwrap()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("外部MCPホスト上限: 拒否"));
        assert!(text.contains("--mcp-external"));
        assert!(text.contains("fixture") && text.contains("最大32件"));
        let allowed = state_lines(&host, true)
            .unwrap()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(allowed.contains("外部MCPホスト上限: 許可"));
        assert!(effects.blocker().unwrap().is_none());
        std::fs::write(home.join(".polaris/mcp/servers.json"), b"not-json").unwrap();
        assert!(state_lines(&host, false).is_err());
    }
}

pub(crate) fn handle<B: Backend, R: KeyReader>(
    request: &str,
    terminal: &RefCell<Terminal<B>>,
    reader: &mut R,
    workspace: &WorkspaceOwner,
    effects: &ManagementEffectStoreV1,
    cwd: &Path,
    external_enabled: bool,
    has_pending: bool,
) -> Result<Vec<Line<'static>>, String> {
    let host = LocalHost::open(cwd, false).map_err(|_| "MCP保存領域を開けません")?;
    handle_host(
        request,
        terminal,
        reader,
        workspace,
        effects,
        &host,
        external_enabled,
        has_pending,
    )
}

pub(crate) fn handle_host<B: Backend, R: KeyReader>(
    request: &str,
    terminal: &RefCell<Terminal<B>>,
    reader: &mut R,
    workspace: &WorkspaceOwner,
    effects: &ManagementEffectStoreV1,
    host: &LocalHost,
    external_enabled: bool,
    has_pending: bool,
) -> Result<Vec<Line<'static>>, String> {
    let request = request.trim();
    if request.is_empty() || request == "list" || request == "status" {
        return state_lines(host, external_enabled);
    }
    if request.len() > 2048 || request.chars().any(char::is_control) {
        return Err("MCP操作は2048バイト以内の1行で指定してください".into());
    }
    let mut parts = request.split_whitespace();
    let command = parts.next().unwrap_or("");
    match command {
        "secret" => {
            return Err(
                "MCP credential ownerの安全な保存・復旧経路は未実装です。値を保存・削除できません。".into(),
            );
        }
        "check" | "setup" | "materialize" | "register" | "configure" | "delete" => {
            return Err(
                "このMCP操作の管理ownerは未実装です。登録・catalog・install領域を変更できません。"
                    .into(),
            );
        }
        "prompt" => {
            return Err(
                "/mcp prompt <server>/<prompt> [key=value ...] は次の入力の選択欄で扱います。"
                    .into(),
            );
        }
        "enable" | "disable" => {}
        _ => {
            return Err(
                "/mcp list|enable <server>|disable <server>|prompt <server>/<prompt>".into(),
            );
        }
    }
    let server_id = parts.next().ok_or("/mcp enable|disable <server>")?;
    if parts.next().is_some() {
        return Err("/mcp enable|disable <server> はserverを1件だけ指定してください".into());
    }
    if has_pending {
        return Err("送信待ちの入力があるためMCP登録を変更できません".into());
    }
    let lease = workspace
        .try_mutation()
        .map_err(|_| "実行中の操作があるためMCP登録を変更できません")?;
    polaris_core::sync::composite::C10CompositeBlockerV1::require_clear_with_mutation(
        workspace.project_root(),
        effects.state_dir(),
        &lease,
    )
    .map_err(|error| error.to_string())?;
    let owner = McpRegistryChangeOwnerV1::new(host, effects, &lease, workspace.project_root())?;
    let prepared = owner.prepare(server_id, command == "enable")?;
    let accepted = confirm(
        terminal,
        reader,
        if prepared.enabled() {
            "MCPをこのプロジェクトで有効化"
        } else {
            "MCPをこのプロジェクトで無効化"
        },
        prepared.server_id(),
        &format!(
            "registry revision {}。次の実行から反映します。実行中runtime・起動時の外部MCP上限は変更しません。巻き戻しでは取り消せません。",
            prepared.before_revision()
        ),
    );
    match owner.apply(&prepared, accepted)? {
        Some(revision) => Ok(vec![Line::from(format!(
            "{} をこのプロジェクトで{}にしました（revision {revision}）。次の実行から反映します。外部MCP上限: {}。",
            render::sanitize(prepared.server_id()),
            if prepared.enabled() {
                "有効"
            } else {
                "無効"
            },
            if external_enabled { "許可" } else { "拒否" },
        ))]),
        None => Ok(vec![Line::from("MCP登録の変更を取り消しました。")]),
    }
}
