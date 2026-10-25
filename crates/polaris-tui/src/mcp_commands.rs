//! MCP controls change next-run admission; they never replace a running runtime.
use crate::{account::confirm, approver::KeyReader, render};
use polaris_core::{
    approval::ApprovalPolicy,
    management_effect::{ManagementEffectStoreV1, mcp_change::McpRegistryChangeOwnerV1},
    mcp::{local::LocalHost, operation::ExternalCeiling},
    rewind_workspace::WorkspaceOwner,
};
use ratatui::{Terminal, backend::Backend, text::Line};
use std::{cell::RefCell, path::Path, sync::Arc};

mod journal;
#[cfg(test)]
mod management_tests;
mod operations;
pub(crate) use operations::confirm_card as confirm_environment_card;
mod secret;

pub(crate) fn state_lines(
    host: &LocalHost,
    external_enabled: bool,
) -> Result<Vec<Line<'static>>, String> {
    let mut lines = vec![
        Line::from(format!(
            "External MCP host ceiling: {}",
            if external_enabled {
                "allowed"
            } else {
                "denied"
            }
        )),
        Line::from(if external_enabled {
            "To deny external MCP, restart without --mcp-external."
        } else {
            "To allow external MCP, restart with --mcp-external."
        }),
        Line::from(
            "Registration and admission state follows. This does not establish connection success or changes to the active runtime.",
        ),
        Line::from(
            "/mcp setup: discover and review registrations / /mcp check <server>: verify catalog",
        ),
        Line::from(
            "/mcp materialize <name>: download and pin a package (registration confirmed separately)",
        ),
        Line::from(
            "/mcp secret set: dedicated secret input / /mcp secret delete <reference>: delete credentials",
        ),
    ];
    let states = host
        .state_report(ExternalCeiling::host_external(external_enabled))
        .map_err(|_| "Cannot read MCP registration and admission state")?;
    let registry = match host.store() {
        Some(store) => store
            .load_registry()
            .map_err(|_| "Cannot read the MCP registry")?,
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
        return Err("MCP registry changed while viewing. Redisplay it".into());
    }
    if let Some(registry) = &registry {
        lines.push(Line::from(format!(
            "Registry revision {}",
            registry.revision
        )));
    }
    if let Some(store) = host.store()
        && let Some(secrets) = store
            .load_secrets()
            .map_err(|_| "Cannot read MCP credential reference names")?
    {
        for reference in secrets.references() {
            lines.push(Line::from(format!(
                "Credential reference: {}",
                render::sanitize(reference)
            )));
        }
    }
    if states.is_empty() {
        lines.push(Line::from("No registered MCP servers."));
        return Ok(lines);
    }
    lines.push(Line::from(format!(
        "{} MCP servers (maximum 32)",
        states.len()
    )));
    for state in states {
        let status = match (&state.quarantine, state.enabled_here) {
            (Some(reason), _) => format!("unavailable ({})", render::sanitize(reason)),
            (None, false) => "disabled for this project".to_owned(),
            (None, true) => "enabled".to_owned(),
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

pub(crate) async fn handle<B: Backend, R: KeyReader>(
    request: &str,
    terminal: &RefCell<Terminal<B>>,
    reader: &mut R,
    management: (&WorkspaceOwner, &ManagementEffectStoreV1),
    cwd: &Path,
    (external_enabled, approval_policy): (bool, ApprovalPolicy),
    has_pending: bool,
) -> Result<Vec<Line<'static>>, String> {
    let host = LocalHost::open(cwd, false).map_err(|_| "Cannot open MCP storage")?;
    handle_host(
        request,
        terminal,
        reader,
        management,
        &host,
        (external_enabled, approval_policy),
        has_pending,
    )
    .await
}

pub(crate) async fn handle_host<B: Backend, R: KeyReader>(
    request: &str,
    terminal: &RefCell<Terminal<B>>,
    reader: &mut R,
    (workspace, effects): (&WorkspaceOwner, &ManagementEffectStoreV1),
    host: &LocalHost,
    (external_enabled, approval_policy): (bool, ApprovalPolicy),
    has_pending: bool,
) -> Result<Vec<Line<'static>>, String> {
    let request = request.trim();
    if request.is_empty() || request == "list" || request == "status" {
        return state_lines(host, external_enabled);
    }
    if request.len() > 2048 || request.chars().any(char::is_control) {
        return Err("Specify an MCP operation as one line within 2048 bytes".into());
    }
    let parts: Vec<_> = request.split_whitespace().collect();
    let command = parts[0];
    match command {
        "configure" | "delete" => {
            return Err(
                "This MCP operation is not provided. Manage detected registrations with /mcp setup."
                    .into(),
            );
        }
        "prompt" => {
            return Err(
                "/mcp prompt <server>/<prompt> [key=value ...] is handled by the next-input selection screen."
                    .into(),
            );
        }
        "enable" | "disable" | "secret" | "check" | "setup" | "register" | "materialize" => {}
        _ => {
            return Err(
                "/mcp list|enable|disable|check <server>, setup, register [name], materialize <name>, secret set|delete, prompt <server>/<prompt>".into(),
            );
        }
    }
    if has_pending {
        return Err("Cannot change MCP registrations while input is queued".into());
    }
    let lease = Arc::new(
        workspace
            .try_mutation()
            .map_err(|_| "Cannot change MCP registrations while an operation is active")?,
    );
    polaris_core::sync::composite::C10CompositeBlockerV1::require_clear_with_mutation(
        workspace.project_root(),
        effects.state_dir(),
        &lease,
    )
    .map_err(|error| error.to_string())?;
    if effects.project_scope_hash()
        != polaris_core::management_effect::ManagementEffectHashV1::digest(
            workspace.project_root().as_os_str().as_encoded_bytes(),
        )
        || host.project() != polaris_core::project::project_id(workspace.project_root())
        || lease.store_root() != effects.state_dir().join("rewind-workspace")
    {
        return Err("MCP management owner project mismatch".into());
    }
    let journal = journal::Journal {
        effects,
        lease: &lease,
    };
    match parts.as_slice() {
        ["secret", rest @ ..] => return secret::handle(rest, terminal, reader, host, &journal, approval_policy),
        ["check", server] => {
            return operations::check(server, terminal, reader, host, (ExternalCeiling::host_external(external_enabled), approval_policy), &journal).await;
        }
        ["setup"] | ["register"] => {
            return operations::setup(None, terminal, reader, host, (ExternalCeiling::host_external(external_enabled), approval_policy), &journal).await;
        }
        ["register", name] => {
            return operations::setup(Some(name), terminal, reader, host, (ExternalCeiling::host_external(external_enabled), approval_policy), &journal).await;
        }
        ["materialize", rest @ ..] => return operations::materialize(rest, terminal, reader, host, (external_enabled, approval_policy), (&journal, &lease)).await,
        ["enable" | "disable", _] => {}
        _ => return Err("Invalid MCP operation arguments. Specify /mcp setup, check <server>, enable|disable <server>, or secret set|delete".into()),
    }
    let server_id = parts[1];
    let owner = McpRegistryChangeOwnerV1::new(host, effects, &lease, workspace.project_root())?;
    let prepared = owner.prepare(server_id, command == "enable")?;
    let accepted = confirm(
        terminal,
        reader,
        if prepared.enabled() {
            "Enable MCP for this project"
        } else {
            "Disable MCP for this project"
        },
        prepared.server_id(),
        &format!(
            "registry revision {}. Effective from the next run. The active runtime and startup external MCP ceiling are unchanged. Rewind cannot undo this.",
            prepared.before_revision()
        ),
    );
    match owner.apply(&prepared, accepted)? {
        Some(revision) => Ok(vec![Line::from(format!(
            "{} is now {} for this project (revision {revision}). Effective from the next run. External MCP ceiling: {}.",
            render::sanitize(prepared.server_id()),
            if prepared.enabled() {
                "enabled"
            } else {
                "disabled"
            },
            if external_enabled {
                "allowed"
            } else {
                "denied"
            },
        ))]),
        None => Ok(vec![Line::from("MCP registration change cancelled.")]),
    }
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

    #[tokio::test]
    async fn m3d_mcp_tui_registry_confirmation_ceiling_and_denial() {
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
            (&workspace, &effects),
            &host,
            (false, ApprovalPolicy::Always),
            false,
        )
        .await
        .unwrap();
        assert!(output[0].to_string().contains("next run"));
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
            (&workspace, &effects),
            &host,
            (false, ApprovalPolicy::Always),
            false,
        )
        .await
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
            (&workspace, &effects),
            &host,
            (false, ApprovalPolicy::Always),
            false,
        )
        .await
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
                (&workspace, &effects),
                &host,
                (false, ApprovalPolicy::Always),
                false
            )
            .await
            .is_err()
        );
        assert!(
            handle_host(
                "enable fixture",
                &terminal,
                &mut keys,
                (&workspace, &effects),
                &host,
                (false, ApprovalPolicy::Always),
                true
            )
            .await
            .is_err()
        );
        assert!(
            handle_host(
                "secret delete fixture",
                &terminal,
                &mut keys,
                (&workspace, &effects),
                &host,
                (false, ApprovalPolicy::Always),
                false
            )
            .await
            .is_err()
        );
        assert!(
            handle_host(
                "check fixture",
                &terminal,
                &mut keys,
                (&workspace, &effects),
                &host,
                (false, ApprovalPolicy::Always),
                false
            )
            .await
            .is_err()
        );
        let text = state_lines(&host, false)
            .unwrap()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("External MCP host ceiling: denied"));
        assert!(text.contains("--mcp-external"));
        assert!(text.contains("fixture") && text.contains("maximum 32"));
        let allowed = state_lines(&host, true)
            .unwrap()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(allowed.contains("External MCP host ceiling: allowed"));
        assert!(effects.blocker().unwrap().is_none());
        std::fs::write(home.join(".polaris/mcp/servers.json"), b"not-json").unwrap();
        assert!(state_lines(&host, false).is_err());
    }
}
