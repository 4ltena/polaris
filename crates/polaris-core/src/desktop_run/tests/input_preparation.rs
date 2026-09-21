//! Desktop request preparation preserves accepted raw input across worker and reopen boundaries.
use super::*;
use crate::desktop_store::Writer;
use crate::input_preparation::{InputPreparationMode, prepare};
use polaris_desktop_protocol::{
    ids::{AttemptId, ClientId, OperationId, RequestId, ResultId},
    request::{DraftUpdate, Request, RequestBody, RunStart},
    run_state::Observation,
};
use polaris_provider::{CompletionRequest, CompletionResponse, Message, ProviderError};

#[derive(Default)]
struct Capture(std::sync::Mutex<Vec<CompletionRequest>>);

#[async_trait::async_trait]
impl Provider for Capture {
    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        self.0.lock().unwrap().push(request);
        Ok(CompletionResponse {
            text: "prepared completion".into(),
            ..Default::default()
        })
    }
}

fn repeated_input() -> String {
    let original = format!("{}\n\n", "alpha beta gamma delta epsilon. ".repeat(140)).repeat(8);
    assert!((4096..=32_000).contains(&crate::budget::count_tokens(&original)));
    original
}

fn bind_input(input: &mut DesktopRunInput, writer: &Writer, capture: Arc<Capture>) {
    input.session.messages = writer
        .snapshot()
        .unwrap()
        .raw
        .into_iter()
        .map(|event| event.message)
        .collect();
    input.session.input_preparation = InputPreparationMode::RepeatRefs;
    input.request_context =
        DesktopRequestContext::Prepared(writer.project_request_history(&input.target).unwrap());
    input.provider = capture.clone();
    input.provider_pool = capture;
}

#[test]
fn accepted_repeated_input_is_prepared_once_without_changing_durable_raw() {
    let (_temp, mut input, mut executions, _) = fixture(Behavior::Complete);
    let original = repeated_input();
    let projection = prepare(&original, InputPreparationMode::RepeatRefs)
        .projection
        .expect("fixture is eligible for input preparation");
    assert_eq!(projection.reconstruct().as_deref(), Some(original.as_str()));
    let (root, mut writer, target) =
        history::writer_for(input.target.run_id.clone(), &original, None);
    input.target = target.clone();
    let before = writer.snapshot().unwrap();
    let raw_before = serde_json::to_value(&before.raw).unwrap();
    let capture = Arc::new(Capture::default());
    bind_input(&mut input, &writer, capture.clone());

    let (mut worker, _events) = DesktopRun::start(input).unwrap();
    let completion = joined(&mut worker);
    assert_eq!(
        completion.result.as_ref().unwrap().text,
        "prepared completion"
    );
    assert_eq!(completion.session.messages[0].content, original);
    assert_eq!(
        serde_json::to_value(writer.snapshot().unwrap().raw).unwrap(),
        raw_before,
    );
    assert!(executions.try_recv().is_err());
    let captured = capture.0.lock().unwrap();
    assert_eq!(
        captured.len(),
        1,
        "preparation must not add a model request"
    );
    assert!(
        !captured[0].tools.is_empty(),
        "the shared agent made this request"
    );
    assert_eq!(captured[0].messages.len(), 1);
    assert!(
        captured[0].messages[0]
            .content
            .starts_with("[Polaris input encoding v1]\n")
    );
    assert_eq!(captured[0].messages[0].content, projection.encoded);

    writer
        .finish_with_messages_and_workflow(
            &target,
            &OperationId::new(format!("main-{}", target.run_id.as_str())).unwrap(),
            Observation::Succeeded,
            ResultId::new("prepared-result").unwrap(),
            completion.turn_suffix,
            None,
        )
        .unwrap();
    drop(writer);
    let reopened = root
        .open(&before.marker.project_id, &before.marker.session_id)
        .unwrap();
    let saved = reopened.snapshot().unwrap();
    assert_eq!(serde_json::to_value(&saved.raw[..1]).unwrap(), raw_before);
    assert_eq!(saved.raw[0].message.content, original);
    assert_eq!(saved.raw[1].message.content, "prepared completion");
}

fn request(id: &str, body: RequestBody) -> Request {
    Request {
        protocol_version: Default::default(),
        client_id: ClientId::new("input-preparation-fixture").unwrap(),
        request_id: RequestId::new(id).unwrap(),
        body,
    }
}

#[test]
fn reopened_desktop_history_keeps_eligible_old_input_raw_on_the_next_request() {
    let (_temp, mut input, mut executions, _) = fixture(Behavior::Complete);
    let original = repeated_input();
    assert!(
        prepare(&original, InputPreparationMode::RepeatRefs)
            .projection
            .is_some()
    );
    let (root, mut writer, old) =
        history::writer_for(RunId::new("previous-run").unwrap(), &original, None);
    writer
        .finish_with_messages_and_workflow(
            &old,
            &OperationId::new("main-previous-run").unwrap(),
            Observation::Succeeded,
            ResultId::new("previous-result").unwrap(),
            vec![Message::assistant("previous completion")],
            None,
        )
        .unwrap();
    let saved = writer.snapshot().unwrap();
    let old_raw = serde_json::to_value(&saved.raw).unwrap();
    drop(writer);
    let mut writer = root
        .open(&saved.marker.project_id, &saved.marker.session_id)
        .unwrap();
    assert_eq!(
        serde_json::to_value(writer.snapshot().unwrap().raw).unwrap(),
        old_raw,
    );
    let next_text = "Continue with the accepted history.";
    writer
        .apply(
            &request(
                "next-draft",
                RequestBody::DraftUpdate(
                    saved.marker.session_id.clone(),
                    DraftUpdate {
                        expected_draft_revision: saved.state.draft.draft_revision,
                        text: next_text.into(),
                        attachment_ids: vec![],
                    },
                ),
            ),
            None,
        )
        .unwrap();
    let accepted = writer.snapshot().unwrap();
    let target = RunTarget {
        run_id: input.target.run_id.clone(),
        attempt_id: AttemptId::new("next-attempt").unwrap(),
    };
    writer
        .apply(
            &request(
                "next-start",
                RequestBody::RunStart(
                    accepted.marker.session_id,
                    RunStart {
                        mcp_prompt: None,
                        expected_draft_revision: accepted.state.draft.draft_revision,
                        expected_configuration_revision: accepted
                            .state
                            .configuration
                            .configuration_revision,
                        expected_policy_revision: accepted.state.policy_revision,
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
    input.target = target;
    let capture = Arc::new(Capture::default());
    bind_input(&mut input, &writer, capture.clone());
    let raw_before = serde_json::to_value(writer.snapshot().unwrap().raw).unwrap();

    let (mut worker, _events) = DesktopRun::start(input).unwrap();
    let completion = joined(&mut worker);
    assert_eq!(
        completion.result.as_ref().unwrap().text,
        "prepared completion"
    );
    assert_eq!(completion.session.messages[0].content, original);
    assert_eq!(completion.session.messages[2].content, next_text);
    let captured = capture.0.lock().unwrap();
    assert_eq!(captured.len(), 1, "reopening must not add a model request");
    assert!(!captured[0].tools.is_empty());
    assert_eq!(captured[0].messages.len(), 3);
    assert_eq!(captured[0].messages[0].content, original);
    assert_eq!(captured[0].messages[1].content, "previous completion");
    assert_eq!(captured[0].messages[2].content, next_text);
    assert!(executions.try_recv().is_err());
    assert_eq!(
        serde_json::to_value(writer.snapshot().unwrap().raw).unwrap(),
        raw_before,
    );
}
