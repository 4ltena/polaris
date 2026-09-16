//! Owner request projection never rewrites raw or substitutes the search query.
use super::*;

fn next(writer: &mut Writer, text: &str) -> RunTarget {
    let revision = writer.snapshot().unwrap().state.draft.draft_revision.get();
    writer
        .apply(&draft("next-draft", revision, text), None)
        .unwrap();
    let target = RunTarget {
        run_id: RunId::new("next-run").unwrap(),
        attempt_id: AttemptId::new("next-attempt").unwrap(),
    };
    writer
        .apply(&start("next-start", revision + 1), Some(target.clone()))
        .unwrap();
    target
}

#[test]
fn request_history_joins_only_proven_prompt_and_preserves_actual_query_and_raw() {
    let (_root, mut writer, intent) = begun();
    assert!(writer.project_request_history(&target()).is_err());
    let publication = writer
        .prepare_mcp_prompt_terminal(&intent, McpTerminalInput::Reply(&reply()))
        .unwrap();
    writer.publish_mcp_prompt_terminal(&publication).unwrap();
    let saved = writer.snapshot().unwrap();
    let history = writer.project_request_history(&target()).unwrap();
    assert_eq!(history.query(), saved.state.runs[0].input.text);
    assert_eq!(history.target(), &target());
    assert_eq!(history.marker(), &saved.marker);
    assert_eq!(history.messages().len(), 1);
    assert!(
        history.messages()[0]
            .content
            .starts_with(&saved.raw[0].message.content)
    );
    assert!(
        history.messages()[0]
            .content
            .ends_with(&saved.raw[1].message.content)
    );
    unchanged(&saved, &writer.snapshot().unwrap());
    let imitation = saved.raw[1].message.content.clone();
    writer
        .finish(&target(), Observation::Succeeded, result())
        .unwrap();
    let next = next(&mut writer, &imitation);
    let history = writer.project_request_history(&next).unwrap();
    assert_eq!(history.query(), imitation);
    assert_eq!(history.messages().len(), 2);
    assert_eq!(history.messages()[1].content, imitation);
    assert!(writer.project_request_history(&target()).is_err());
}

#[test]
fn request_history_blocks_unconfirmed_current_prompt_but_preserves_failed_historical_receipts() {
    let (_root, writer) = accepted();
    assert!(writer.project_request_history(&target()).is_err());
    for disposition in [
        McpDisposition::BeforeSend,
        McpDisposition::InputRequired,
        McpDisposition::OutcomeUnknown,
    ] {
        let (root, mut writer, intent) = begun();
        let mut response = reply();
        response.status = "input_required";
        response.parts = vec![ProjectedPart::InputRequired {
            request: json!({"question":"fixture"}),
        }];
        let input = match disposition {
            McpDisposition::BeforeSend => McpTerminalInput::Failure(OperationFailure::BeforeSend(
                polaris_mcp::Error::Cancelled,
            )),
            McpDisposition::OutcomeUnknown => McpTerminalInput::Failure(
                OperationFailure::OutcomeUnknown(polaris_mcp::Error::Transport),
            ),
            _ => McpTerminalInput::Reply(&response),
        };
        let publication = writer.prepare_mcp_prompt_terminal(&intent, input).unwrap();
        writer.publish_mcp_prompt_terminal(&publication).unwrap();
        assert!(writer.project_request_history(&target()).is_err());
        drop(writer);
        let mut writer = root.open(&project(), &session()).unwrap();
        let next = next(&mut writer, "new actual query");
        let before = writer.snapshot().unwrap();
        let history = writer.project_request_history(&next).unwrap();
        assert_eq!(history.query(), "new actual query");
        assert_eq!(history.messages().len(), 2);
        assert!(history.messages()[0].content.contains("\"disposition\":"));
        unchanged(&before, &writer.snapshot().unwrap());
    }
}
