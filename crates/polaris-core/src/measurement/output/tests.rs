//! Counting only; nothing is sent and no transcript is read from disk.
use super::super::Reported;
use super::*;

#[test]
fn the_body_and_the_tool_arguments_are_counted_apart() {
    let held = parts(
        "この変更で対応します。",
        &[r#"{"path":"src/main.rs","old":"a","new":"b"}"#.into()],
    );
    assert!(held.body_tokens > 0);
    assert!(held.tool_argument_tokens > 0);
    assert_eq!(held.tool_calls, 1);
    assert_eq!(
        held.visible_tokens(),
        held.body_tokens + held.tool_argument_tokens,
        "a diff lives inside the arguments and is not counted a second time"
    );

    // A turn that only calls a tool has no body, and the reverse.
    assert_eq!(parts("", &["{}".into()]).body_tokens, 0);
    assert_eq!(parts("text", &[]).tool_argument_tokens, 0);
    assert_eq!(parts("text", &[]).tool_calls, 0);
}

#[test]
fn what_the_provider_billed_and_what_can_be_seen_are_not_reconciled_by_force() {
    let held = Parts {
        body_tokens: 40,
        tool_argument_tokens: 60,
        tool_calls: 2,
    };
    assert_eq!(
        recount(&held, Reported::Known { tokens: 100 }),
        Recount::Agrees { tokens: 100 }
    );
    // Reasoning that was never shown is the usual reason for a gap. It stays
    // whole instead of being spread over the body and the arguments.
    assert_eq!(
        recount(&held, Reported::Known { tokens: 340 }),
        Recount::Differs {
            reported: 340,
            visible: 100,
            unexplained: 240,
        }
    );
    // The visible text can also exceed the billed count, when the arguments
    // were re-encoded on the way out.
    assert_eq!(
        recount(&held, Reported::Known { tokens: 90 }),
        Recount::Differs {
            reported: 90,
            visible: 100,
            unexplained: -10,
        }
    );
    assert_eq!(recount(&held, Reported::Unknown), Recount::NoReportedOutput);
}
