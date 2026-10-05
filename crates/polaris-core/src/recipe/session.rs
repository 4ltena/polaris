//! M7c-3b-1: the durable session a bundled setting command writes into.
//!
//! The ledger lives where every other run record lives, so opening it means
//! opening the same store the desktop service opens. One session per
//! project, so a second `polaris setting` in the same project finds what
//! the first one finished and does not repeat it.
use super::super::desktop_store::{DesktopRoot, InitialState, StoreError, StoreResult, Writer};
use polaris_desktop_protocol::{
    ids::{DecimalU64, ProjectId, SessionId},
    snapshot::{Configuration, Draft},
};
use std::path::Path;

/// Why a session could not be opened, in words a person reading a terminal
/// can act on.
pub fn refusal(error: &StoreError) -> &'static str {
    match error {
        // The same project is already being set up somewhere else. Waiting
        // is the answer; a second writer would be a second ledger.
        StoreError::Busy => {
            "この project の設定は別の場所で実行中です。終わってから実行してください。"
        }
        StoreError::RecoveryRequired => {
            "前回の設定が中断しています。もう一度実行すると続きから始めます。"
        }
        StoreError::Deleted => "この project の設定記録は削除されています。",
        StoreError::UnsupportedPlatform => "この OS では設定記録を保存できません。",
        _ => "設定記録を読み書きできません。",
    }
}

/// The session a project's setting commands share. Derived from the
/// project, never from where the command was launched, so the same project
/// reached from a subdirectory is the same session.
fn session_id(project: &ProjectId) -> StoreResult<SessionId> {
    SessionId::new(format!("setting-{}", project.as_str())).map_err(Into::into)
}

/// Opens, or creates, the ledger session for the project `cwd` belongs to.
///
/// Nothing is run and nothing is installed here: this only makes the place
/// the steps will be recorded. A project already being set up elsewhere
/// comes back as `Busy` rather than opening a second writer.
pub fn open(cwd: &Path) -> StoreResult<(Writer, ProjectId, SessionId)> {
    let root = super::super::project::resolve_root(cwd);
    let project = ProjectId::new(super::super::project::project_id(&root))?;
    let session = session_id(&project)?;
    let store = DesktopRoot::open_owned(&super::super::project::desktop_store_root()?)?;
    let writer = store.open_or_create(
        project.clone(),
        session.clone(),
        InitialState {
            // A setting command has no conversation: the draft stays empty
            // and no model is configured, so nothing here can send anything.
            draft: Draft {
                draft_revision: DecimalU64::new(0),
                text: String::new(),
                attachment_ids: Vec::new(),
            },
            configuration: Configuration {
                history_mode: Default::default(),
                configuration_revision: DecimalU64::new(0),
                provider: "none".into(),
                model: String::new(),
                effort: "none".into(),
            },
            policy_revision: DecimalU64::new(0),
        },
    )?;
    Ok((writer, project, session))
}

/// The operation a step's records hang on, named by the step itself.
pub fn operation(step_id: &str) -> StoreResult<polaris_desktop_protocol::ids::OperationId> {
    polaris_desktop_protocol::ids::OperationId::new(step_id).map_err(Into::into)
}

/// Accepts one run on the session, so the steps have somewhere to be
/// recorded. The request id identifies this invocation: the same one twice
/// is the same run, not a second one.
pub fn start_run(
    writer: &mut Writer,
    session: &SessionId,
    request_id: &str,
) -> StoreResult<polaris_desktop_protocol::request::RunTarget> {
    start_run_with_attempt(writer, session, request_id, "1")
}

/// A distinct invocation has a distinct attempt; captured revisions retain
/// the store's CAS checks after an earlier setting consumed its empty draft.
pub fn start_run_with_attempt(
    writer: &mut Writer,
    session: &SessionId,
    request_id: &str,
    attempt_id: &str,
) -> StoreResult<polaris_desktop_protocol::request::RunTarget> {
    use polaris_desktop_protocol::{ids::*, request::*};
    let captured = writer.snapshot()?;
    let target = RunTarget {
        run_id: RunId::new(request_id)?,
        attempt_id: AttemptId::new(attempt_id)?,
    };
    writer.apply_bound(
        &Request {
            protocol_version: Default::default(),
            client_id: ClientId::new("polaris-setting")?,
            request_id: RequestId::new(request_id)?,
            body: RequestBody::RunStart(
                session.clone(),
                RunStart {
                    mcp_prompt: None,
                    expected_draft_revision: captured.state.draft.draft_revision,
                    expected_configuration_revision: captured
                        .state
                        .configuration
                        .configuration_revision,
                    expected_policy_revision: captured.state.policy_revision,
                },
            ),
        },
        Some(target.clone()),
        None,
    )?;
    Ok(target)
}

#[cfg(test)]
mod tests;
