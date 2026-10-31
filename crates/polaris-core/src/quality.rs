//! Checks the harness makes when the root agent is about to stop, and
//! corrections it makes before an error loop ends a turn.
//!
//! Each is a switch, off unless named in `POLARIS_QUALITY` (a comma list of
//! `completion`, `loop` and `blocker`), because neither is shown to help yet: they are
//! measured against runs without them before they are kept. They apply to the
//! root only; a subagent finishes when it finishes.
//!
//! Both only decide from the message history and add one fixed user message;
//! nothing here calls a model.

use polaris_provider::{Message, Role};

/// Marks a message the harness wrote, so a later check can tell it from the
/// user's own input.
const MARK: &str = "[harness] ";

const COMPLETION: &str = "You changed files this turn and are about to finish. Before you do, re-read the task statement and list each requirement and constraint it states. For each one, run something that shows the current result satisfies it (the program on real input, the tests, a direct query) instead of relying on your reading of the code. Fix what fails. If a requirement cannot be checked here, say so in the final answer. Then give the final answer.";

const COMPLETION_AFTER_FAILURE: &str =
    "The last command you ran after your edit failed. Resolve that first.";

const BLOCKER: &str = "Your answer says the task cannot be satisfied or is blocked. The task asks for a correct result, so a contradiction like that usually comes from a choice of your own, not from the data. Do not change anything the data or the task gives as fixed (settings, limits, speeds, altitudes, capacities, prescribed values). Look instead at what you decided yourself: a quantity you held fixed that could be adjusted within the limits (an amount to load or carry, a margin, a reserve, an order), or a requirement you added that the task does not state. For each such choice, test whether adjusting it removes the contradiction, and apply the one that does. Be economical: reuse the checks you already have instead of writing new test suites. If every limit truly cannot be met by any choice of yours, say so with the one check that shows it.";

/// Words by which a final answer declares the whole task blocked.
const BLOCKED_WORDS: [&str; 10] = [
    "blocker",
    "infeasible",
    "no feasible",
    "not feasible",
    "impossible",
    "ブロッカー",
    "実行不能",
    "実行不可能",
    "不可能",
    "成立しない",
];

const LOOP: &str = "The same tool error has now happened three times in a row. Do not repeat it. State in one sentence why it keeps failing, then try a different approach; if nothing else can work, stop and explain what blocks you.";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Gates {
    pub completion: bool,
    pub loop_nudge: bool,
    pub blocker: bool,
}

impl Gates {
    pub fn parse(list: &str) -> Self {
        let mut gates = Self::default();
        for name in list.split(',').map(str::trim) {
            match name {
                "completion" => gates.completion = true,
                "loop" => gates.loop_nudge = true,
                "blocker" => gates.blocker = true,
                _ => {}
            }
        }
        gates
    }

    pub fn from_env() -> Self {
        std::env::var("POLARIS_QUALITY")
            .map(|v| Self::parse(&v))
            .unwrap_or_default()
    }
}

/// The message to send instead of ending the turn, when the final answer
/// declares the task blocked or infeasible.
pub fn blocker_request(answer: &str) -> Option<String> {
    declares_blocked(answer).then(|| format!("{MARK}{BLOCKER}"))
}

fn declares_blocked(answer: &str) -> bool {
    let lowered = answer.to_lowercase();
    BLOCKED_WORDS.iter().any(|word| lowered.contains(word))
}

pub fn loop_request() -> String {
    format!("{MARK}{LOOP}")
}

/// The message to send instead of ending the turn, when the turn changed files
/// (`write` or `edit`) and so has something to check. `None` when it changed
/// none. The user's input starts the turn; the harness's own messages do not.
pub fn completion_request(messages: &[Message]) -> Option<String> {
    let start = messages
        .iter()
        .rposition(|m| m.role == Role::User && !m.content.starts_with(MARK))
        .unwrap_or(0);
    let turn = &messages[start..];
    let last_edit = turn.iter().rposition(|m| {
        m.role == Role::Assistant
            && m.tool_calls
                .iter()
                .any(|c| c.name == "write" || c.name == "edit")
    })?;
    // A command after the last edit whose result says it failed.
    let mut failed = false;
    let mut commands = std::collections::HashSet::new();
    for m in &turn[last_edit..] {
        if m.role == Role::Assistant {
            commands = m
                .tool_calls
                .iter()
                .filter(|c| c.name == "bash")
                .map(|c| c.id.as_str())
                .collect();
        } else if m.role == Role::Tool
            && m.tool_call_id
                .as_deref()
                .is_some_and(|id| commands.contains(id))
        {
            failed = m.content.starts_with("the command failed with exit code");
        }
    }
    let tail = if failed {
        format!(" {COMPLETION_AFTER_FAILURE}")
    } else {
        String::new()
    };
    Some(format!("{MARK}{COMPLETION}{tail}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_provider::ToolCall;

    fn call(id: &str, name: &str) -> Message {
        Message::assistant_with_tool_calls(
            "",
            vec![ToolCall {
                id: id.into(),
                name: name.into(),
                arguments: serde_json::json!({}),
            }],
        )
    }

    #[test]
    fn gates_are_named_in_a_comma_list_and_unknown_names_do_nothing() {
        assert_eq!(
            Gates::parse("completion, loop"),
            Gates {
                completion: true,
                loop_nudge: true,
                ..Default::default()
            }
        );
        assert_eq!(
            Gates::parse("loop"),
            Gates {
                completion: false,
                loop_nudge: true,
                ..Default::default()
            }
        );
        assert_eq!(Gates::parse("nonsense"), Gates::default());
        assert_eq!(Gates::parse(""), Gates::default());
    }

    #[test]
    fn only_an_answer_that_declares_the_task_blocked_is_challenged() {
        assert!(Gates::parse("blocker").blocker);
        let ask =
            blocker_request("Operational blocker: all 24 tours are infeasible.").expect("blocked");
        assert!(ask.starts_with(MARK) && ask.contains("a choice of your own"));
        assert!(blocker_request("No feasible tour exists at this altitude.").is_some());
        assert!(blocker_request("Done. All 24 tests pass and the plan is feasible.").is_none());
        assert!(blocker_request("").is_none());
    }

    #[test]
    fn a_turn_that_changed_nothing_has_nothing_to_check() {
        let messages = vec![
            Message::user("explain"),
            call("a", "read"),
            Message::tool_result("a", "text"),
            Message::assistant("done"),
        ];
        assert!(completion_request(&messages).is_none());
    }

    #[test]
    fn only_edits_after_the_last_real_user_input_count() {
        let messages = vec![
            Message::user("fix it"),
            call("a", "edit"),
            Message::tool_result("a", "ok"),
            Message::assistant("done"),
            Message::user("now explain it"),
            Message::assistant("explained"),
        ];
        assert!(completion_request(&messages).is_none());
    }

    #[test]
    fn an_edited_turn_is_asked_to_check_its_requirements_and_the_ask_is_marked_as_the_harness() {
        let messages = vec![
            Message::user("fix it"),
            call("a", "write"),
            Message::tool_result("a", "ok"),
        ];
        let ask = completion_request(&messages).expect("edited");
        assert!(ask.starts_with(MARK) && ask.contains("each requirement"));
        assert!(!ask.contains("failed"));
        // The harness's own message does not start a new turn: the edit still counts.
        let mut more = messages;
        more.push(Message::user(ask));
        assert!(completion_request(&more).is_some());
    }

    #[test]
    fn a_command_that_failed_after_the_edit_is_named_in_the_ask() {
        let messages = vec![
            Message::user("fix it"),
            call("a", "edit"),
            Message::tool_result("a", "ok"),
            call("b", "bash"),
            Message::tool_result(
                "b",
                "the command failed with exit code 1. policy: x. output: boom",
            ),
        ];
        assert!(completion_request(&messages).unwrap().contains("failed"));
        let mut fixed = messages;
        fixed.push(call("c", "bash"));
        fixed.push(Message::tool_result("c", "all passed"));
        assert!(!completion_request(&fixed).unwrap().contains("failed"));
    }
}
