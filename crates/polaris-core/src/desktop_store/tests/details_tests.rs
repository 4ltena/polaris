//! Run details show the saved turn of exactly the accepted run.
use super::*;
use polaris_desktop_protocol::run_details::RunDetailKind;
use polaris_provider::{Message, ToolCall};
use serde_json::json;

fn run(n: u8) -> RunTarget {
    RunTarget {
        run_id: RunId::new(format!("run-{n}")).unwrap(),
        attempt_id: AttemptId::new(format!("attempt-{n}")).unwrap(),
    }
}

fn operation(n: u8) -> OperationId {
    OperationId::new(format!("operation-{n}")).unwrap()
}

fn tool_turn(output: &str) -> Vec<Message> {
    vec![
        Message::assistant_with_tool_calls(
            "確認します",
            vec![ToolCall {
                id: "call-1".into(),
                name: "read".into(),
                arguments: json!({"path":"a.txt","note":null,"items":[1,"二","\"quoted\""]}),
            }],
        ),
        Message::tool_result("call-1", output),
        Message::assistant("完了"),
    ]
}

fn accept(writer: &mut Writer, n: u8, text: &str) {
    let revision = writer.snapshot().unwrap().state.draft.draft_revision.get();
    writer
        .apply(&draft(&format!("draft-{n}"), revision, text), None)
        .unwrap();
    writer
        .apply(&start(&format!("start-{n}"), revision + 1), Some(run(n)))
        .unwrap();
}

fn two_runs() -> (PrototypeRoot, Writer) {
    let (root, mut writer) = setup();
    for (n, output) in [(1, "first output"), (2, "second output")] {
        accept(&mut writer, n, "同じ指示");
        writer.record_intent(&run(n), operation(n)).unwrap();
        writer
            .finish_with_messages(
                &run(n),
                &operation(n),
                Observation::Succeeded,
                ResultId::new(format!("result-{n}")).unwrap(),
                tool_turn(output),
            )
            .unwrap();
    }
    (root, writer)
}

fn turn_len(writer: &Writer) -> usize {
    let raw = writer.snapshot().unwrap().raw;
    let bytes = serde_json::to_vec(&raw[1..].iter().map(|e| &e.message).collect::<Vec<_>>());
    let bytes = bytes.unwrap().len();
    assert!(bytes > 4 * 1024 * 1024 - 1024, "{bytes}");
    raw.len() - 1
}

fn kinds(details: &RunDetails) -> Vec<RunDetailKind> {
    details.entries.iter().map(|entry| entry.kind).collect()
}

#[test]
fn identical_inputs_project_their_own_tool_turns_without_side_effects() {
    let (root, writer) = two_runs();
    let before = writer.snapshot().unwrap();
    let first = writer.project_run_details(&run(1)).unwrap();
    let second = writer.project_run_details(&run(2)).unwrap();
    assert_eq!(
        kinds(&first),
        [
            RunDetailKind::UserInput,
            RunDetailKind::Assistant,
            RunDetailKind::ToolCall,
            RunDetailKind::ToolResult,
            RunDetailKind::Assistant,
        ]
    );
    assert_eq!(first.entries[0].text, "同じ指示");
    assert_eq!(second.entries[0].text, "同じ指示");
    assert!(first.entries[3].text.contains("first output"));
    assert!(second.entries[3].text.contains("second output"));
    let call: serde_json::Value = serde_json::from_str(&first.entries[2].text).unwrap();
    assert_eq!(
        call["arguments"],
        json!({"path":"a.txt","note":null,"items":[1,"二","\"quoted\""]})
    );
    assert!(first.accepted_revision < second.accepted_revision);
    assert_eq!(first.run_state, RunState::Succeeded);
    assert!(!first.history_gap);
    unchanged(&before, &writer.snapshot().unwrap());
    drop(writer);
    let reopened = root.open(&project(), &session()).unwrap();
    assert_eq!(reopened.project_run_details(&run(2)).unwrap(), second);
}

#[test]
fn reordered_missing_duplicated_or_reversed_acceptance_is_refused() {
    let (_root, writer) = two_runs();
    let saved = writer.snapshot().unwrap();
    let refused = |published: &Published| {
        matches!(
            super::super::writer::details::project(published, &run(1)),
            Err(StoreError::Corrupt(_))
        )
    };
    let acceptance = |published: &mut Published, n: u8| -> usize {
        published
            .state
            .requests
            .iter()
            .position(|record| {
                matches!(&record.result, RequestResult::RunAccepted { run_id, .. }
                    if *run_id == run(n).run_id)
            })
            .unwrap()
    };
    let mut swapped = saved.clone();
    swapped.state.runs.swap(0, 1);
    assert!(refused(&swapped), "records swapped under identical inputs");
    let mut missing = saved.clone();
    let index = acceptance(&mut missing, 2);
    missing.state.requests.remove(index);
    assert!(refused(&missing));
    let mut duplicated = saved.clone();
    let first = acceptance(&mut duplicated, 1);
    let second = acceptance(&mut duplicated, 2);
    duplicated.state.requests[second].result = duplicated.state.requests[first].result.clone();
    assert!(refused(&duplicated));
    let mut reversed = saved.clone();
    let second = acceptance(&mut reversed, 2);
    reversed.state.requests[second].accepted_revision = DecimalU64::new(0);
    assert!(refused(&reversed));
    let mut changed = saved.clone();
    changed.state.runs[0].input.text = "別の指示".into();
    assert!(refused(&changed));
    assert!(matches!(
        writer.project_run_details(&RunTarget {
            run_id: run(1).run_id,
            attempt_id: AttemptId::new("other").unwrap(),
        }),
        Err(StoreError::NotFound)
    ));
}

#[test]
fn unsaved_results_gaps_and_cancelled_runs_are_reported_as_such() {
    let (_root, mut writer) = setup();
    accept(&mut writer, 1, "途中で止まる");
    writer.record_intent(&run(1), operation(1)).unwrap();
    writer
        .record_history_gap(&run(1), HistoryGap::OutputRejected)
        .unwrap();
    let mut turn = tool_turn("unused");
    turn.truncate(1);
    writer
        .finish_with_messages(
            &run(1),
            &operation(1),
            Observation::Interrupted,
            ResultId::new("result-1").unwrap(),
            turn,
        )
        .unwrap();
    let interrupted = writer.project_run_details(&run(1)).unwrap();
    assert!(interrupted.history_gap);
    assert_eq!(
        interrupted.entries.last().unwrap().kind,
        RunDetailKind::ToolResultUnsaved
    );
    assert_eq!(
        interrupted.entries.last().unwrap().id,
        format!("{}-unsaved", interrupted.entries[2].id)
    );

    accept(&mut writer, 2, "開始前に取り消す");
    writer
        .apply(
            &request("cancel-2", RequestBody::RunCancel(session(), run(2))),
            None,
        )
        .unwrap();
    let cancelled = writer.project_run_details(&run(2)).unwrap();
    assert_eq!(cancelled.run_state, RunState::Cancelled);
    assert_eq!(kinds(&cancelled), [RunDetailKind::UserInput]);
}

#[test]
fn a_suffix_near_the_storage_limit_projects_every_byte() {
    let (_root, mut writer) = setup();
    accept(&mut writer, 1, "大きな結果");
    writer.record_intent(&run(1), operation(1)).unwrap();
    // validate_turn_suffix admits at most 256 messages and 4 MiB of JSON.
    let calls: Vec<ToolCall> = (0..254)
        .map(|i| ToolCall {
            id: format!("call-{i}"),
            name: "read".into(),
            arguments: json!({"part": i}),
        })
        .collect();
    let mut turn = vec![Message::assistant_with_tool_calls("", calls)];
    for i in 0..254 {
        turn.push(Message::tool_result(
            format!("call-{i}"),
            format!("{i:03}{}", "x".repeat(16_398)),
        ));
    }
    turn.push(Message::assistant("まとめ"));
    writer
        .finish_with_messages(
            &run(1),
            &operation(1),
            Observation::Succeeded,
            ResultId::new("result-1").unwrap(),
            turn,
        )
        .unwrap();
    let details = writer.project_run_details(&run(1)).unwrap();
    let results: Vec<_> = details
        .entries
        .iter()
        .filter(|entry| entry.kind == RunDetailKind::ToolResult)
        .collect();
    assert_eq!(turn_len(&writer), 256);
    assert_eq!(results.len(), 254);
    for (i, entry) in results.iter().enumerate() {
        let value: serde_json::Value = serde_json::from_str(&entry.text).unwrap();
        assert_eq!(value["content"].as_str().unwrap().len(), 16_401);
        assert!(
            value["content"]
                .as_str()
                .unwrap()
                .starts_with(&format!("{i:03}"))
        );
    }
    assert_eq!(details.entries.last().unwrap().text, "まとめ");
}
