//! 保存済みsession、memory、MCPのlocal command実行をまとめる。
//! 入力確定、owner確認、管理操作のclearance、履歴とqueueはcallerが所有する。

use crate::{
    HistoryBackendFactory, active_saved_session_id, approver::KeyReader, mcp_commands,
    memory_commands, render::Status, session_commands, slash,
};
use polaris_core::{
    approval::ApprovalPolicy, management_effect::ManagementEffectStoreV1,
    rewind_workspace::WorkspaceOwner, session::Session, session_settings::SavedSessionSettingsV1,
};
use polaris_provider::Provider;
use ratatui::{Terminal, backend::Backend, text::Line};
use std::{cell::RefCell, path::Path};

pub(super) struct CommandDispatchResult {
    pub(super) lines: Vec<Line<'static>>,
    pub(super) status: Status,
    pub(super) settings_changed: bool,
    pub(super) invalidate_mcp_prompt: bool,
}

impl CommandDispatchResult {
    fn success(lines: Vec<Line<'static>>, notice: &'static str) -> Self {
        Self {
            lines,
            status: Status::Notice(notice.into()),
            settings_changed: false,
            invalidate_mcp_prompt: false,
        }
    }

    fn error(error: String) -> Self {
        Self {
            lines: Vec::new(),
            status: Status::Error(error),
            settings_changed: false,
            invalidate_mcp_prompt: false,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn dispatch<B: Backend, R: KeyReader>(
    action: &slash::Action,
    terminal: &RefCell<Terminal<B>>,
    reader: &mut R,
    session: &mut Session,
    session_dependencies: (
        &SavedSessionSettingsV1,
        &dyn Provider,
        Option<&HistoryBackendFactory>,
    ),
    management: (&WorkspaceOwner, &ManagementEffectStoreV1),
    paths: (&Path, &Path),
    embedding: Option<&(String, String)>,
    mcp: (bool, ApprovalPolicy),
    has_pending: bool,
) -> Option<CommandDispatchResult> {
    let (initial, provider, history_factory) = session_dependencies;
    let (workspace, effects) = management;
    let (cwd, state_dir) = paths;
    match action {
        slash::Action::Environment(request) => {
            #[cfg(target_os = "macos")]
            let result = crate::environment_commands::handle(
                request,
                cwd,
                terminal,
                reader,
                workspace,
                effects,
                mcp.1,
                has_pending,
            )
            .await;
            #[cfg(not(target_os = "macos"))]
            let result: Result<Vec<Line<'static>>, String> = {
                let _ = request;
                Err("Environment setup currently supports macOS arm64".into())
            };
            Some(match result {
                Ok(lines) => CommandDispatchResult::success(lines, "Environment result displayed"),
                Err(error) => CommandDispatchResult::error(error),
            })
        }
        slash::Action::SessionControl(request) => Some(
            match session_commands::handle(
                request,
                terminal,
                reader,
                session,
                initial,
                workspace,
                effects,
                provider,
                history_factory,
                has_pending,
            ) {
                Ok(result) => CommandDispatchResult {
                    lines: result.lines,
                    status: Status::Notice("Session operation result displayed".into()),
                    settings_changed: result.settings_changed,
                    invalidate_mcp_prompt: false,
                },
                Err(error) => CommandDispatchResult::error(error),
            },
        ),
        slash::Action::Memory(request) => {
            let id = match active_saved_session_id(session) {
                Ok(id) => id,
                Err(error) => return Some(CommandDispatchResult::error(error)),
            };
            Some(
                match memory_commands::handle(
                    request,
                    terminal,
                    reader,
                    workspace,
                    effects,
                    cwd,
                    state_dir,
                    id.as_deref(),
                    embedding.cloned(),
                    has_pending,
                )
                .await
                {
                    Ok(lines) => {
                        CommandDispatchResult::success(lines, "Memory operation result displayed")
                    }
                    Err(error) => CommandDispatchResult::error(error),
                },
            )
        }
        slash::Action::Mcp(request) if request.split_whitespace().next() != Some("prompt") => Some(
            match mcp_commands::handle(
                request,
                terminal,
                reader,
                (workspace, effects),
                cwd,
                mcp,
                has_pending,
            )
            .await
            {
                Ok(lines) => {
                    let mut result =
                        CommandDispatchResult::success(lines, "MCP operation result displayed");
                    result.invalidate_mcp_prompt = matches!(
                        request.split_whitespace().next(),
                        Some(
                            "enable"
                                | "disable"
                                | "check"
                                | "setup"
                                | "register"
                                | "materialize"
                                | "secret"
                        )
                    );
                    result
                }
                Err(error) => CommandDispatchResult::error(error),
            },
        ),
        _ => None,
    }
}
