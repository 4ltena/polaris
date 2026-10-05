//! TUI initialization and cloned run ownership preserve request-only input preparation.

use super::*;
use polaris_core::{
    approval::{Approver, Decision},
    input_preparation::{InputPreparationMode, prepare},
    session_store::PersistedSession,
};
use polaris_provider::{CompletionRequest, CompletionResponse, ProviderError, Role};
use std::sync::Mutex;

#[derive(Default)]
struct Capture(Mutex<Vec<CompletionRequest>>);

#[async_trait::async_trait]
impl Provider for Capture {
    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        self.0.lock().unwrap().push(request);
        Ok(CompletionResponse {
            text: "recorded".into(),
            ..Default::default()
        })
    }
}

struct NoApproval;
impl Approver for NoApproval {
    fn ask(&mut self, _: &str) -> Decision {
        panic!("the request-only fixture must not invoke tools or approvals")
    }
}

#[tokio::test]
async fn tui_input_preparation_changes_only_the_sent_copy_and_survives_reopen() {
    let original = format!("{}\n\n", "alpha beta gamma delta epsilon. ".repeat(140)).repeat(8);
    let projection = prepare(&original, InputPreparationMode::RepeatRefs)
        .projection
        .expect("positive fixture must be eligible");
    assert_eq!(projection.reconstruct().as_deref(), Some(original.as_str()));
    for mode in [InputPreparationMode::Off, InputPreparationMode::RepeatRefs] {
        let dir = tempfile::tempdir().unwrap();
        let config = polaris_core::config::Config {
            input_preparation: mode,
            files_md_auto_regenerate: false,
            ..Default::default()
        };
        let sessions_dir = dir.path().join("sessions");
        let mut session = new_workflow_session(WorkflowSessionArgs {
            config: &config.workflow,
            initial_phase: None,
            history_mode: config.history_mode,
            input_preparation: config.input_preparation,
            cwd: dir.path(),
            state_dir: dir.path(),
            sessions_dir: &sessions_dir,
            strict_history: None,
            summary_usage: None,
            history_backend_factory: None,
            files_md_auto_regenerate: config.files_md_auto_regenerate,
            approval_auto: config.approval_auto,
            continuation: None,
            workflow_mode_overridden: false,
            skills: &[],
            #[cfg(unix)]
            management_effects: None,
            #[cfg(unix)]
            project_mutation: None,
        })
        .await
        .unwrap();
        session.push_user(&original);
        let persistence = session.persistence.as_ref().unwrap().clone();
        let before = persistence.snapshot().unwrap();
        let raw_before = serde_json::to_value(&before.events).unwrap();
        // The TUI passes a clone to its running task and receives it back.
        let mut running = session.clone();
        let provider = Arc::new(Capture::default());
        let sandbox = SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[]).unwrap();
        let mut gate = Gate::new(ApprovalPolicy::Never);
        let mut approver = NoApproval;
        let mut context = ToolContext {
            sandbox: &sandbox,
            helper: std::path::Path::new("/unused-tui-input-preparation-helper"),
            gate: &mut gate,
            approver: &mut approver,
        };
        let outcome = agent::run(
            provider.as_ref(),
            &mut running,
            Arc::new(tokio::sync::Mutex::new(
                AuditLog::open(&dir.path().join("audit.jsonl")).unwrap(),
            )),
            &mut StopTracker::new(6),
            &polaris_core::prompt::assemble_always_on("", "", &[]),
            &[],
            &[],
            provider.clone(),
            1,
            1,
            None,
            &mut context,
        )
        .await
        .unwrap();
        assert_eq!(outcome.text, "recorded");
        let sent = provider.0.lock().unwrap();
        assert_eq!(sent.len(), 1, "preparation must add no model requests");
        let users: Vec<_> = sent[0]
            .messages
            .iter()
            .filter(|message| message.role == Role::User)
            .collect();
        assert_eq!(users.len(), 1);
        assert_eq!(
            users[0].content,
            if mode == InputPreparationMode::Off {
                &original
            } else {
                &projection.encoded
            }
            .as_str()
        );
        assert_eq!(session.messages[0].content, original);
        assert_eq!(running.messages[0].content, original);
        let reopened = PersistedSession::open(
            &persistence.data_root,
            &persistence.database,
            &before.state.project_id,
            &before.state.session_id,
        )
        .unwrap()
        .snapshot()
        .unwrap();
        assert_eq!(
            serde_json::to_value(&reopened.events[..before.events.len()]).unwrap(),
            raw_before
        );
        assert_eq!(reopened.events[0].message.content, original);
    }
}
