//! The rules that decide which files the model is pointed at, and what the
//! map is allowed to lose on the way in.
use std::collections::BTreeMap;
use std::sync::Mutex;

use polaris_provider::decision::{Answer, Decider, Decision, DecisionError, Question};

use super::*;

const MAP: &str = "\
# File map

Preamble that names no file.

## `.`

- `README.md` — polaris

## `crates/polaris-core/src`

- `agent.rs` — The loop that runs one turn — tools included.
- `budget.rs` — Token ceilings.
";

/// The line carries a bare file name; the path only exists once the
/// heading above it is known. An implementation that took the line at face
/// value would hand the model `agent.rs`, which `read` cannot open from the
/// repository root.
#[test]
fn the_path_is_rebuilt_from_the_heading_the_line_sits_under() {
    let entries = parse(MAP);
    let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "README.md",
            "crates/polaris-core/src/agent.rs",
            "crates/polaris-core/src/budget.rs",
        ]
    );
}

/// The root section is spelled `.`, and joining it like any other directory
/// would produce `./README.md`. That still opens, so no test would catch it
/// by failing to read — it would only ever show up as a path the model was
/// handed that does not match the one the repository uses anywhere else.
#[test]
fn the_root_section_does_not_become_a_dot_prefix() {
    let entries = parse(MAP);
    assert_eq!(entries[0].path, "README.md");
    assert!(!entries[0].path.starts_with("./"));
}

/// A `- ` line above the first heading has no directory to belong to.
/// Defaulting it to the root would invent a path the map never claimed.
#[test]
fn an_entry_before_any_heading_is_dropped_rather_than_rooted() {
    let orphan = "- `stray.rs` — belongs to no directory\n\n## `src`\n\n- `real.rs` — real\n";
    let entries = parse(orphan);
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0].path, "src/real.rs");
}

/// Several real summaries contain an em dash of their own. Splitting on the
/// last one, or on all of them, truncates the summary at that point and
/// silently narrows what the judge is shown.
#[test]
fn a_summary_that_contains_an_em_dash_survives_whole() {
    let entries = parse(MAP);
    let agent = &entries[1];
    assert_eq!(
        agent.summary,
        "The loop that runs one turn — tools included."
    );
}

/// A line the generator did not produce means the two have diverged.
/// Repairing it here would hide that.
#[test]
fn a_line_the_generator_could_not_have_written_is_skipped() {
    let broken = "## `src`\n\n- no backticks — summary\n- `named.rs` no dash\n- `` — empty name\n\
                  - `ok.rs` — kept\n";
    let entries = parse(broken);
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0].path, "src/ok.rs");
}

fn entry(path: &str, summary: &str) -> Entry {
    Entry {
        path: path.into(),
        summary: summary.into(),
    }
}

/// Truncation is by whole entries. A path cut in half reads to the model as
/// a real path, and the `read` that follows fails the way a missing file
/// does.
#[test]
fn the_section_is_cut_between_entries_never_inside_one() {
    let long: Vec<Entry> = (0..40)
        .map(|i| {
            entry(
                &format!("crates/polaris-core/src/very/long/path/number/{i}/module.rs"),
                "A summary long enough that eight of these cannot fit the ceiling at once.",
            )
        })
        .collect();
    let picks: Vec<&Entry> = long.iter().collect();
    let body = section(&picks).expect("a section was expected");

    assert!(crate::budget::count_tokens(&body) <= SECTION_LIMIT);
    for line in body.lines() {
        let path = line
            .strip_prefix("- `")
            .and_then(|rest| rest.split_once("` — "))
            .map(|(path, _)| path)
            .unwrap_or_else(|| panic!("a line was cut inside an entry: {line}"));
        assert!(
            long.iter().any(|held| held.path == path),
            "the section names a path the map does not have: {path}"
        );
    }
    assert!(!body.is_empty());
}

/// Nothing adopted is not an empty section; it is no section at all. An
/// empty heading spends tokens every iteration to say nothing.
#[test]
fn nothing_adopted_produces_no_section() {
    assert_eq!(section(&[]), None);
}

#[test]
fn the_section_never_names_more_than_the_cap() {
    let many: Vec<Entry> = (0..40)
        .map(|i| entry(&format!("a{i}.rs"), "short"))
        .collect();
    let picks: Vec<&Entry> = many.iter().collect();
    let body = section(&picks).expect("a section was expected");
    assert_eq!(body.lines().count(), MAX_PICKS);
}

/// A judge that says yes to whatever path it is asked about, and records
/// every path it saw.
struct Everything {
    seen: Mutex<Vec<String>>,
    requests: Mutex<usize>,
}

impl Everything {
    fn new() -> Self {
        Self {
            seen: Mutex::new(Vec::new()),
            requests: Mutex::new(0),
        }
    }
}

fn yes() -> Answer {
    Answer {
        noul: Some(1.0),
        ..Default::default()
    }
}

#[async_trait::async_trait]
impl Decider for Everything {
    fn capabilities(&self) -> polaris_provider::decision::DecisionCapabilities {
        // This synthetic fixture exercises the historical numerical policy.
        polaris_provider::decision::DecisionCapabilities {
            band_calibration: polaris_provider::decision::BandCalibration::LegacyJev,
            ..Default::default()
        }
    }

    async fn decide(
        &self,
        _state: &serde_json::Value,
        questions: &BTreeMap<String, Question>,
    ) -> Result<Decision, DecisionError> {
        *self.requests.lock().unwrap() += 1;
        let mut answers = BTreeMap::new();
        for (key, question) in questions {
            if let Question::Noul { instructions } = question {
                self.seen.lock().unwrap().push(instructions.clone());
            }
            answers.insert(key.clone(), yes());
        }
        Ok(Decision {
            answers,
            input_tokens: None,
            output_tokens: None,
        })
    }
}

/// The whole point of the typed route: nothing is dropped before meaning is
/// judged. A map too large for one request is split across several, and an
/// entry in the last chunk is asked about exactly as the first one is.
///
/// This is the rule a lexical pre-filter would break, and the reason
/// `rank_by_bm25` is not used here — it produces no terms at all for a
/// Japanese request, so it would drop every entry before this point.
#[tokio::test(flavor = "current_thread")]
async fn every_entry_is_asked_about_even_when_the_map_needs_several_requests() {
    // Long enough summaries that the set cannot be one request.
    let padding = "説明".repeat(400);
    let entries: Vec<Entry> = (0..400)
        .map(|i| entry(&format!("crates/x/src/m{i}.rs"), &padding))
        .collect();

    let judge = Everything::new();
    let picked = route(&judge, &entries, "登録でsymlinkが拒否される", "implement")
        .await
        .expect("the route was refused");

    assert!(
        *judge.requests.lock().unwrap() > 1,
        "the map fitted one request, so this test proves nothing"
    );
    assert_eq!(
        picked.len(),
        entries.len(),
        "a judge that adopted everything must leave nothing behind"
    );
    let seen = judge.seen.lock().unwrap();
    for entry in &entries {
        assert!(
            seen.iter().any(|asked| asked.contains(&entry.path)),
            "{} was never asked about",
            entry.path
        );
    }
}

/// A judge that fails on its first request and answers afterwards.
struct FailsOnce {
    failed: Mutex<bool>,
}

#[async_trait::async_trait]
impl Decider for FailsOnce {
    fn capabilities(&self) -> polaris_provider::decision::DecisionCapabilities {
        // This synthetic fixture exercises the historical numerical policy.
        polaris_provider::decision::DecisionCapabilities {
            band_calibration: polaris_provider::decision::BandCalibration::LegacyJev,
            ..Default::default()
        }
    }

    async fn decide(
        &self,
        _state: &serde_json::Value,
        questions: &BTreeMap<String, Question>,
    ) -> Result<Decision, DecisionError> {
        let mut failed = self.failed.lock().unwrap();
        if !*failed {
            *failed = true;
            return Err(DecisionError::Unavailable);
        }
        Ok(Decision {
            answers: questions.keys().map(|key| (key.clone(), yes())).collect(),
            input_tokens: None,
            output_tokens: None,
        })
    }
}

/// One chunk failing is not the route failing. A shorter list of real files
/// is still worth more than the whole-map read it replaces.
#[tokio::test(flavor = "current_thread")]
async fn one_failed_chunk_does_not_discard_the_answered_ones() {
    let padding = "説明".repeat(400);
    let entries: Vec<Entry> = (0..400)
        .map(|i| entry(&format!("crates/x/src/m{i}.rs"), &padding))
        .collect();

    let judge = FailsOnce {
        failed: Mutex::new(false),
    };
    let picked = route(&judge, &entries, "登録でsymlinkが拒否される", "implement")
        .await
        .expect("a partial answer must not refuse the route");
    assert!(!picked.is_empty(), "every chunk was discarded");
    assert!(
        picked.len() < entries.len(),
        "the failing chunk was answered anyway, so this test proves nothing"
    );
}

/// A judge that answers nothing at all leaves the run where it was, and the
/// caller has to be able to tell that apart from "no file fits".
#[tokio::test(flavor = "current_thread")]
async fn a_judge_that_never_answers_refuses_rather_than_reporting_no_match() {
    struct Never;
    #[async_trait::async_trait]
    impl Decider for Never {
        fn capabilities(&self) -> polaris_provider::decision::DecisionCapabilities {
            Everything::new().capabilities()
        }

        async fn decide(
            &self,
            _state: &serde_json::Value,
            _questions: &BTreeMap<String, Question>,
        ) -> Result<Decision, DecisionError> {
            Err(DecisionError::Unavailable)
        }
    }
    let entries = vec![entry("src/a.rs", "one")];
    assert!(route(&Never, &entries, "依頼", "implement").await.is_err());
}

/// An empty request has nothing to judge against, and asking anyway spends
/// a request to compare every file to the empty string.
#[tokio::test(flavor = "current_thread")]
async fn an_empty_request_asks_nothing() {
    let judge = Everything::new();
    let entries = vec![entry("src/a.rs", "one")];
    assert!(
        route(&judge, &entries, "   ", "implement")
            .await
            .expect("an empty request is not a failure")
            .is_empty()
    );
    assert_eq!(*judge.requests.lock().unwrap(), 0);
}

/// The real map is the input this exists for. If the generator's format
/// drifts, every rule above still passes against the fixture and the route
/// silently returns nothing.
#[test]
fn the_repositorys_own_map_parses() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/filemap.md")
        .canonicalize();
    let Ok(path) = path else {
        return; // Not every checkout carries the generated map.
    };
    let body = std::fs::read_to_string(&path).expect("the map exists but cannot be read");
    let entries = parse(&body);
    assert!(
        entries.len() > 400,
        "the map parsed to {} entries, so the format has drifted",
        entries.len()
    );
    for entry in &entries {
        assert!(
            !entry.path.starts_with('.') && !entry.path.ends_with('/'),
            "a path was rebuilt wrong: {}",
            entry.path
        );
    }
}

struct Unmeasured;
#[async_trait::async_trait]
impl Decider for Unmeasured {
    async fn decide(
        &self,
        _: &serde_json::Value,
        _: &BTreeMap<String, Question>,
    ) -> Result<Decision, DecisionError> {
        panic!("unmeasured probabilities must not be requested for numerical bands")
    }
}

#[tokio::test]
async fn an_unmeasured_backend_retains_context_candidates_in_original_order() {
    let entries = vec![entry("first.rs", "first"), entry("second.rs", "second")];
    let picked = route(&Unmeasured, &entries, "query", "implement")
        .await
        .unwrap();
    assert_eq!(
        picked.iter().map(|entry| &entry.path).collect::<Vec<_>>(),
        entries.iter().map(|entry| &entry.path).collect::<Vec<_>>()
    );
}
