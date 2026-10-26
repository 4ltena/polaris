
use super::*;
#[test]
fn m17_live_memory_matches_common_terminal_projection_and_escapes_uri_controls() {
    let observation = polaris_core::conversation_memory::observation::MemoryObservation {
            mode: polaris_core::conversation_state::HistoryMode::Structured10,
            retrieval_status: Some(polaris_core::conversation_memory::structured::packing::RetrievalStatus::PendingReconfirmation),
            source_uris: vec!["conversation://fixture?start=1&end=1\n\u{1b}[2J".into()],
            reference_tokens: 40, related_omission: None,
        };
    let expected = observation.display_lines().join("\n");
    let lines = format_event_for_live_print(&AgentEvent::MemoryPrepared(observation));
    let actual = lines
        .iter()
        .map(|line| line.line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(actual, expected);
    assert!(actual.contains("recheck required") && actual.contains("40 reference tokens"));
    assert!(!actual.contains('\u{1b}'));
}
