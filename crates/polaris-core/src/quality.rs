//! Checks the harness makes when the root agent is about to stop, and
//! corrections it makes before an error loop ends a turn.
//!
//! Each is a switch, off unless named in `POLARIS_QUALITY` (a comma list of
//! `completion`, `loop`, `blocker` and `data`), because neither is shown to help yet: they are
//! measured against runs without them before they are kept. They apply to the
//! root only; a subagent finishes when it finishes.
//!
//! Both only decide from the message history and add one fixed user message;
//! nothing here calls a model.

use std::path::Path;

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

const DATA: &str = "These keys appear in the input data files but nowhere in your code: {KEYS}. For each, decide from the task text and the key's name whether leaving it out is correct; if it names something the task asks for (a time, a cost, a limit, a step, a rule), implement it. Do not write new test suites.";

const LOOP: &str = "The same tool error has now happened three times in a row. Do not repeat it. State in one sentence why it keeps failing, then try a different approach; if nothing else can work, stop and explain what blocks you.";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Gates {
    pub completion: bool,
    pub loop_nudge: bool,
    pub blocker: bool,
    pub data: bool,
}

impl Gates {
    pub fn parse(list: &str) -> Self {
        let mut gates = Self::default();
        for name in list.split(',').map(str::trim) {
            match name {
                "completion" => gates.completion = true,
                "loop" => gates.loop_nudge = true,
                "blocker" => gates.blocker = true,
                "data" => gates.data = true,
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
    let failed = edited_turn(messages)?;
    let tail = if failed {
        format!(" {COMPLETION_AFTER_FAILURE}")
    } else {
        String::new()
    };
    Some(format!("{MARK}{COMPLETION}{tail}"))
}

/// The one message that replaces a final answer when it needs a second look:
/// it declares the task blocked (`blocker`), or the turn changed files and
/// input data keys exist that the code never mentions (`data`). Both go in a
/// single message, so the model reconsiders once, not twice. `dir` is where
/// the data and code are looked for.
pub fn review_request(
    gates: Gates,
    answer: &str,
    messages: &[Message],
    dir: &Path,
) -> Option<String> {
    let mut parts = Vec::new();
    if gates.blocker && declares_blocked(answer) {
        parts.push(BLOCKER.to_owned());
    }
    if gates.data && edited_turn(messages).is_some() {
        let keys = unused_keys(dir);
        if !keys.is_empty() {
            parts.push(DATA.replace("{KEYS}", &keys.join(", ")));
        }
    }
    (!parts.is_empty()).then(|| format!("{MARK}{}", parts.join("\n\n")))
}

const SKIP_DIRS: [&str; 9] = [
    ".git",
    "target",
    "node_modules",
    "output",
    "__pycache__",
    ".polaris",
    "tools",
    "tmp",
    ".venv",
];
const CODE_EXTENSIONS: [&str; 14] = [
    "py", "rs", "js", "ts", "go", "java", "rb", "c", "cc", "cpp", "h", "sh", "kt", "swift",
];
const MAX_KEYS: usize = 30;
const MAX_CODE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_DATA_BYTES: u64 = 256 * 1024;

/// Lowercase identifier keys of the JSON files under `dir` that no code file
/// there mentions. Keys written in capitals are identifiers of things (an
/// airport, a product), not fields, and a key starting with `_` is a note.
pub fn unused_keys(dir: &Path) -> Vec<String> {
    let (mut data, mut code) = (Vec::new(), String::new());
    let mut spent = 0u64;
    let mut stack = vec![(dir.to_path_buf(), 0usize)];
    while let Some((path, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&path) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let (name, file) = (entry.file_name(), entry.path());
            if polaris_tools::path_policy::is_denied(&file) {
                continue;
            }
            if kind.is_dir() {
                if depth < 4 && !SKIP_DIRS.iter().any(|skip| name == *skip) {
                    stack.push((file, depth + 1));
                }
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if !kind.is_file() {
                continue;
            }
            match file.extension().and_then(|e| e.to_str()) {
                Some("json") if meta.len() <= MAX_DATA_BYTES => {
                    if let Ok(text) = std::fs::read_to_string(&file) {
                        data.push(text);
                    }
                }
                Some(ext)
                    if CODE_EXTENSIONS.contains(&ext) && spent + meta.len() <= MAX_CODE_BYTES =>
                {
                    if let Ok(text) = std::fs::read_to_string(&file) {
                        spent += meta.len();
                        code.push_str(&text);
                    }
                }
                _ => {}
            }
        }
    }
    let mut keys = std::collections::BTreeSet::new();
    fn collect(value: &serde_json::Value, keys: &mut std::collections::BTreeSet<String>) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, inner) in map {
                    keys.insert(key.clone());
                    collect(inner, keys);
                }
            }
            serde_json::Value::Array(items) => items.iter().for_each(|item| collect(item, keys)),
            _ => {}
        }
    }
    for text in &data {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
            collect(&value, &mut keys);
        }
    }
    keys.into_iter()
        .filter(|key| {
            key.len() >= 3
                && key.starts_with(|c: char| c.is_ascii_lowercase())
                && key
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                && !code.contains(key.as_str())
        })
        .take(MAX_KEYS)
        .collect()
}

/// `Some(failed)` when the turn since the user's last input changed files
/// (`write` or `edit`), where `failed` says the last command after the last
/// edit failed; `None` when it changed none.
fn edited_turn(messages: &[Message]) -> Option<bool> {
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
    Some(failed)
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
    fn keys_the_code_never_mentions_are_listed_and_identifiers_and_notes_are_not() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("aircraft.json"),
            r#"{"fuel_flow_gph": 60, "turnaround_time_min": 25, "_note": "x", "NAN": {"elevation_ft": 5}}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("plan.py"), "rate = a['fuel_flow_gph']").unwrap();
        std::fs::write(
            dir.path().join("credentials.json"),
            r#"{"private_credential_field": 1}"#,
        )
        .unwrap();
        // Output and tooling directories are not read.
        std::fs::create_dir(dir.path().join("output")).unwrap();
        std::fs::write(
            dir.path().join("output/plan.json"),
            r#"{"turnaround_time_min": 1, "extra_key": 1}"#,
        )
        .unwrap();
        assert_eq!(
            unused_keys(dir.path()),
            ["elevation_ft", "turnaround_time_min"]
        );
        std::fs::write(
            dir.path().join("plan.py"),
            "a['fuel_flow_gph'] + a['turnaround_time_min'] + a['elevation_ft']",
        )
        .unwrap();
        assert!(unused_keys(dir.path()).is_empty());
    }

    #[test]
    fn one_review_message_carries_the_blocked_challenge_and_the_unused_keys() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("d.json"), r#"{"turnaround_time_min": 25}"#).unwrap();
        let edited = vec![
            Message::user("fix it"),
            call("a", "edit"),
            Message::tool_result("a", "ok"),
        ];
        let both = Gates {
            blocker: true,
            data: true,
            ..Default::default()
        };
        let ask =
            review_request(both, "Blocker: infeasible.", &edited, dir.path()).expect("both apply");
        assert!(ask.contains("a choice of your own") && ask.contains("turnaround_time_min"));
        assert_eq!(ask.matches(MARK).count(), 1, "one message, not two");
        // Only the keys, when the answer is not blocked; only the challenge, when no key is unused.
        let keys_only = review_request(both, "Done.", &edited, dir.path()).unwrap();
        assert!(
            !keys_only.contains("a choice of your own")
                && keys_only.contains("turnaround_time_min")
        );
        std::fs::write(dir.path().join("c.py"), "x = 'turnaround_time_min'").unwrap();
        assert!(review_request(both, "Done.", &edited, dir.path()).is_none());
        assert!(
            review_request(both, "Infeasible.", &edited, dir.path())
                .unwrap()
                .contains("a choice of your own")
        );
        // A turn that changed nothing is not asked about keys.
        std::fs::remove_file(dir.path().join("c.py")).unwrap();
        assert!(review_request(both, "Done.", &[Message::user("explain")], dir.path()).is_none());
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
