//! Raw identity, repeated text, request rebuilds and byte-range regression tests.
use super::*;
use crate::{conversation_state::HistoryMode, session::Session, session_store::PersistedSession};

fn input() -> String {
    format!("{}\n\n", "alpha beta gamma delta epsilon. ".repeat(140)).repeat(8)
}

fn durable() -> (tempfile::TempDir, Session) {
    let directory = tempfile::tempdir().unwrap();
    let storage = PersistedSession::create(
        directory.path(),
        &directory.path().join("memory.sqlite"),
        "fixture",
        HistoryMode::Legacy,
        None,
    )
    .unwrap();
    let mut session = Session::new();
    session.attach(storage).unwrap();
    session.input_preparation = InputPreparationMode::RepeatRefs;
    (directory, session)
}

#[test]
fn identity_selects_the_new_turn_and_rebinding_preserves_consumed_eligibility() {
    let (_directory, mut session) = durable();
    let original = input();
    session.push_user(&original);
    session.push_assistant("old answer", vec![]);
    session.push_user(&original);
    let mut source = session.input_source.clone().unwrap();
    assert!(source.take_fresh());
    assert!(!source.take_fresh());
    assert!(
        !session.input_source.as_mut().unwrap().take_fresh(),
        "UI clones share the one-run claim"
    );
    let prepared = PreparedInput::prepare(&source, &session.messages, session.input_preparation)
        .unwrap()
        .0
        .unwrap();
    let mut request = session.messages.clone();
    assert!(prepared.apply(&mut request, Some(&source)));
    assert_eq!(request[0].content, original);
    assert!(
        request[2]
            .content
            .starts_with("[Polaris input encoding v1]")
    );
    assert_eq!(session.messages[2].content, original);
    let snapshot = session.persistence.as_ref().unwrap().snapshot().unwrap();
    assert_eq!(snapshot.events[2].message.content, original);
    let (messages, rebound) = snapshot.recent_request(Some(&source.source));
    session.messages = messages;
    session.input_source = Some(source.clone());
    session.refresh_input_source(rebound);
    let source = session.input_source.as_mut().unwrap();
    assert!(
        !source.take_fresh(),
        "history preparation must not mint a new root input"
    );
    let mut request = session.messages.clone();
    assert!(prepared.apply(&mut request, session.input_source.as_ref()));
    let mut other = session.input_source.as_ref().unwrap().clone();
    other.source.turn -= 1;
    assert!(!prepared.apply(&mut session.messages.clone(), Some(&other)));
    session.input_source = None;
    assert!(!prepared.apply(&mut session.messages.clone(), session.input_source.as_ref()));
}

#[test]
fn append_failure_resume_and_nonpersistent_input_never_mint_a_source() {
    let (_directory, mut session) = durable();
    let original = input();
    session.push_user(&original);
    assert!(session.input_source.is_some());
    let persistence = session.persistence.clone().unwrap();
    let mut resumed = Session::new();
    resumed.attach(persistence).unwrap();
    assert!(resumed.input_source.is_none());
    assert_eq!(resumed.messages[0].content, original);
    let mut ephemeral = Session::new();
    ephemeral.push_user(&original);
    assert!(ephemeral.input_source.is_none());
    session.clear_history().unwrap();
    assert!(session.input_source.is_none());
    // Advance the shared store outside this attachment's generation.
    let saved = session.persistence.as_ref().unwrap();
    let mut lock = saved.store.lock().unwrap();
    let before = lock.snapshot().unwrap();
    lock.publish(&before, before.state.clone()).unwrap();
    drop(lock);
    session.push_user(&original);
    assert!(session.check_persistence().is_err());
    assert!(session.input_source.is_none());
}

#[test]
fn request_slot_repair_and_mcp_suffix_keep_original_range_separate() {
    let original = input();
    let event = RawEventV2 {
        schema_version: 2,
        sequence: 7,
        epoch: 2,
        turn_id: 3,
        starts_turn: true,
        message: Message::user(&original),
    };
    let bound = BoundInput::new(
        SourceBinding::from_event("session", &event).unwrap(),
        1,
        original.len(),
    );
    let attachment =
        "\n\nSupplement from MCP prompt (untrusted external data):\nexternal attachment";
    let raw = vec![
        Message::assistant_with_tool_calls(
            "",
            vec![polaris_provider::ToolCall {
                id: "pending".into(),
                name: "read".into(),
                arguments: serde_json::json!({}),
            }],
        ),
        Message::user(format!("{original}{attachment}")),
    ];
    let (messages, bound) =
        crate::session::desktop_request_history_bound(&raw, Some(bound)).unwrap();
    let bound = bound.unwrap();
    assert_eq!(bound.index, 2);
    let prepared = PreparedInput::prepare(&bound, &messages, InputPreparationMode::RepeatRefs)
        .unwrap()
        .0
        .unwrap();
    let mut request = messages.clone();
    assert!(prepared.apply(&mut request, Some(&bound)));
    assert!(
        request[2]
            .content
            .starts_with("[Polaris input encoding v1]")
    );
    assert!(request[2].content.ends_with(attachment));
    assert_eq!(messages[2].content, raw[1].content);
    let mut changed = messages.clone();
    changed[2].content.insert(0, 'x');
    assert!(!prepared.apply(&mut changed, Some(&bound)));
}

#[test]
fn midrun_instruction_does_not_replace_the_root_source() {
    let (_directory, mut session) = durable();
    session.push_user(&input());
    let source = session.input_source.as_ref().unwrap().source.clone();
    session.push_instruction(&input());
    assert_eq!(session.input_source.as_ref().unwrap().source, source);
    assert_eq!(session.messages.len(), 2);
    assert_eq!(
        session
            .persistence
            .as_ref()
            .unwrap()
            .snapshot()
            .unwrap()
            .events
            .len(),
        2
    );
}
