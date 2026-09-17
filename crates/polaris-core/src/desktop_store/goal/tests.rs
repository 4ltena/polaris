//! Bounds and identity only; no store and no I/O.
use super::*;

fn instruction(id: &str, text: &str) -> SavedInstruction {
    SavedInstruction {
        instruction_id: id.into(),
        text: text.into(),
        received_revision: DecimalU64::new(1),
        applied_to: None,
    }
}

#[test]
fn a_goal_is_refused_when_it_is_empty_or_unnumbered_or_too_long() {
    let goal = |text: &str, revision: u64, active: bool| SavedGoal {
        text: text.into(),
        revision: DecimalU64::new(revision),
        active,
        stopped: false,
        automatic_used: DecimalU64::new(0),
    };
    assert!(goal("進める", 1, true).validate().is_ok());
    // A cleared goal may keep an empty text; an active one may not.
    assert!(goal("", 1, false).validate().is_ok());
    assert!(goal("  ", 1, true).validate().is_err());
    assert!(goal("進める", 0, true).validate().is_err());
    assert!(
        goal(&"x".repeat(MAX_GOAL_BYTES + 1), 1, true)
            .validate()
            .is_err()
    );
}

#[test]
fn instructions_need_an_identity_and_never_repeat_one() {
    assert!(instruction("i1", "追加の指示").validate().is_ok());
    assert!(instruction("", "追加の指示").validate().is_err());
    assert!(instruction("i 1", "追加の指示").validate().is_err());
    assert!(instruction("i1", "   ").validate().is_err());
    assert!(
        instruction("i1", &"x".repeat(MAX_INSTRUCTION_BYTES + 1))
            .validate()
            .is_err()
    );
    let duplicated = vec![instruction("i1", "one"), instruction("i1", "two")];
    assert!(validate(None, &duplicated).is_err());
    assert!(validate(None, &duplicated[..1]).is_ok());
    let many: Vec<_> = (0..=MAX_INSTRUCTIONS)
        .map(|n| instruction(&format!("i{n}"), "指示"))
        .collect();
    assert!(validate(None, &many).is_err());
}
