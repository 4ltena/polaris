//! Desktop inputs are created by a real writer, including retained tool turns.
use super::*;
use crate::desktop_store::{InitialState, PrototypeRoot, Writer};
use polaris_desktop_protocol::{
    ids::*,
    request::*,
    run_state::Observation,
    snapshot::{Configuration, Draft, HistoryMode},
};
use polaris_provider::Message;

fn request(id: &str, body: RequestBody) -> Request {
    Request {
        protocol_version: Default::default(),
        client_id: ClientId::new("desktop-fixture").unwrap(),
        request_id: RequestId::new(id).unwrap(),
        body,
    }
}

pub(super) fn writer_for(
    run: RunId,
    text: &str,
    prompt: Option<McpPromptChoice>,
) -> (PrototypeRoot, Writer, RunTarget) {
    writer_with_workflow(run, text, prompt, None)
}

pub(super) fn writer_with_workflow(
    run: RunId,
    text: &str,
    prompt: Option<McpPromptChoice>,
    workflow: Option<crate::desktop_store::SavedWorkflow>,
) -> (PrototypeRoot, Writer, RunTarget) {
    let root = PrototypeRoot::new().unwrap();
    let session = SessionId::new("desktop-session").unwrap();
    let mut writer = root
        .create(
            ProjectId::new("project").unwrap(),
            session.clone(),
            InitialState {
                draft: Draft {
                    draft_revision: DecimalU64::new(0),
                    text: text.into(),
                    attachment_ids: vec![],
                },
                configuration: Configuration {
                    configuration_revision: DecimalU64::new(0),
                    provider: "fixture".into(),
                    model: "fixture".into(),
                    effort: "medium".into(),
                    history_mode: HistoryMode::Legacy,
                },
                policy_revision: DecimalU64::new(0),
            },
        )
        .unwrap();
    let target = RunTarget {
        run_id: run,
        attempt_id: AttemptId::new("attempt").unwrap(),
    };
    if workflow.is_some() {
        writer
            .configure_workflow(DecimalU64::new(0), workflow)
            .unwrap();
    }
    start(&mut writer, target.clone(), prompt);
    (root, writer, target)
}

fn start(writer: &mut Writer, target: RunTarget, prompt: Option<McpPromptChoice>) {
    let saved = writer.snapshot().unwrap();
    writer
        .apply(
            &request(
                target.run_id.as_str(),
                RequestBody::RunStart(
                    saved.marker.session_id,
                    RunStart {
                        mcp_prompt: prompt,
                        expected_draft_revision: saved.state.draft.draft_revision,
                        expected_configuration_revision: saved
                            .state
                            .configuration
                            .configuration_revision,
                        expected_policy_revision: saved.state.policy_revision,
                    },
                ),
            ),
            Some(target.clone()),
        )
        .unwrap();
    writer
        .record_intent(
            &target,
            OperationId::new(format!("main-{}", target.run_id.as_str())).unwrap(),
        )
        .unwrap();
}

pub(super) fn continue_after(input: &mut DesktopRunInput, text: &str, mut messages: Vec<Message>) {
    let (_root, mut writer, old) = writer_for(RunId::new("old-run").unwrap(), "old user", None);
    messages.push(Message::assistant("old completion"));
    writer
        .finish_with_messages_and_workflow(
            &old,
            &OperationId::new("main-old-run").unwrap(),
            Observation::Succeeded,
            ResultId::new("old-result").unwrap(),
            messages,
            None,
        )
        .unwrap();
    let saved = writer.snapshot().unwrap();
    writer
        .apply(
            &request(
                "next-draft",
                RequestBody::DraftUpdate(
                    saved.marker.session_id,
                    DraftUpdate {
                        expected_draft_revision: saved.state.draft.draft_revision,
                        text: text.into(),
                        attachment_ids: vec![],
                    },
                ),
            ),
            None,
        )
        .unwrap();
    let target = RunTarget {
        run_id: input.target.run_id.clone(),
        attempt_id: AttemptId::new("next-attempt").unwrap(),
    };
    start(&mut writer, target.clone(), None);
    input.target = target.clone();
    input.session.messages = writer
        .snapshot()
        .unwrap()
        .raw
        .into_iter()
        .map(|e| e.message)
        .collect();
    input.request_context =
        DesktopRequestContext::Prepared(writer.project_request_history(&target).unwrap());
}
