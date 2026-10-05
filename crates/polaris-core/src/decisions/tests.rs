//! What the ledger must not lose, and what the route must not claim.
use std::collections::BTreeMap;
use std::sync::Mutex;

use polaris_provider::decision::{Answer, Decider, Decision, DecisionError, Question};

use super::*;

fn entry(at: &str, what: &str) -> Entry {
    Entry {
        at: at.into(),
        what: what.into(),
    }
}

/// A one-line decision is prose, and prose has commas in it. Splitting on
/// `,` would cut the sentence in half and store the tail as a third column
/// that nothing reads.
#[test]
fn a_decision_containing_commas_quotes_and_newlines_survives_the_round_trip() {
    let dir = tempfile::tempdir().expect("dir");
    let awkward = "判定器はoff、e5へ倒す。理由は\"順位\"であって分類ではないから\nという2行目";
    append(dir.path(), "2026-09-19T10:00:00+09:00", awkward).expect("append");

    let read = load(dir.path());
    assert_eq!(read.len(), 1, "{read:?}");
    assert_eq!(read[0].what, awkward);
    assert_eq!(read[0].at, "2026-09-19T10:00:00+09:00");
}

#[test]
fn a_decision_preserves_leading_and_trailing_whitespace() {
    let dir = tempfile::tempdir().expect("dir");
    let exact = " \t判定器は既定offのまま　 ";
    append(dir.path(), "2026-09-19", exact).expect("append");

    let read = load(dir.path());
    assert_eq!(read.len(), 1, "{read:?}");
    assert_eq!(read[0].what, exact);
}

/// Hand-editing is expected. One bad line must not take the ledger with it.
#[test]
fn a_malformed_row_is_skipped_rather_than_failing_the_whole_ledger() {
    let text =
        "at,what\n2026-09-19,決めたこと\nこの行は1列しかない\n,空のat\n2026-09-20,もう一件\n";
    let read = parse(text);
    assert_eq!(read.len(), 2, "{read:?}");
    assert_eq!(read[0].what, "決めたこと");
    assert_eq!(read[1].what, "もう一件");
}

/// A ledger that has settled nothing is the ordinary state of a new
/// project, not a failure to read.
#[test]
fn a_missing_ledger_is_empty_not_an_error() {
    let dir = tempfile::tempdir().expect("dir");
    assert!(load(dir.path()).is_empty());
}

/// Appending must not rewrite what is already there — the ledger's whole
/// purpose is to say what was decided at the time.
#[test]
fn appending_keeps_every_earlier_line() {
    let dir = tempfile::tempdir().expect("dir");
    for n in 0..3 {
        append(
            dir.path(),
            &format!("2026-09-{:02}", 19 + n),
            &format!("件{n}"),
        )
        .expect("append");
    }
    let read = load(dir.path());
    assert_eq!(read.len(), 3);
    assert_eq!(read[0].what, "件0", "the first line must still be first");
    assert_eq!(read[2].what, "件2");
}

#[test]
fn an_empty_decision_is_refused_rather_than_stored() {
    let dir = tempfile::tempdir().expect("dir");
    for blank in ["   ", "\t \n　"] {
        assert_eq!(
            append(dir.path(), "2026-09-19", blank)
                .expect_err("whitespace-only decision must be rejected")
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
    }
    assert!(load(dir.path()).is_empty());
    assert!(!dir.path().join(PATH).exists());
}

struct Picks {
    wanted: &'static str,
    asked: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl Decider for Picks {
    async fn decide(
        &self,
        _state: &serde_json::Value,
        questions: &BTreeMap<String, Question>,
    ) -> Result<Decision, DecisionError> {
        let mut answers = BTreeMap::new();
        for (key, question) in questions {
            let text = match question {
                Question::Noul { instructions } => instructions.clone(),
                _ => String::new(),
            };
            self.asked.lock().unwrap().push(text.clone());
            answers.insert(
                key.clone(),
                Answer {
                    noul: Some(if text.contains(self.wanted) { 1.0 } else { 0.0 }),
                    ..Default::default()
                },
            );
        }
        Ok(Decision {
            answers,
            input_tokens: None,
            output_tokens: None,
        })
    }
}

/// Every entry is asked about. A ledger only grows, so the one that
/// matters may be the oldest; a lexical pre-filter would drop it, and a
/// Japanese request produces no lexical terms at all.
#[tokio::test(flavor = "current_thread")]
async fn every_entry_is_asked_about_even_when_the_ledger_needs_several_requests() {
    let padding = "決めたこと".repeat(400);
    let entries: Vec<Entry> = (0..400)
        .map(|i| {
            entry(
                &format!("2026-09-{:02}", i % 28 + 1),
                &format!("{padding}{i}"),
            )
        })
        .collect();
    let judge = Picks {
        wanted: "399",
        asked: Mutex::new(Vec::new()),
    };
    let matched = route(&judge, &entries, "判定器をoffにするか")
        .await
        .expect("the route was refused");
    assert_eq!(matched.len(), 1, "{}", matched.len());
    let asked = judge.asked.lock().unwrap();
    assert_eq!(
        asked.len(),
        entries.len(),
        "every entry must be asked about"
    );
}

/// Nothing matched is not an empty heading. A heading that says nothing
/// costs tokens on every iteration of the run.
#[test]
fn nothing_matched_produces_no_section() {
    assert_eq!(section(&[]), None);
}

#[test]
fn the_section_is_cut_between_entries_and_never_shows_more_than_the_cap() {
    let many: Vec<Entry> = (0..20)
        .map(|i| entry("2026-09-19", &format!("十分に長い決定の一行 {i}")))
        .collect();
    let picks: Vec<&Entry> = many.iter().collect();
    let body = section(&picks).expect("a section was expected");
    assert!(crate::budget::count_tokens(&body) <= SECTION_LIMIT);
    assert!(body.lines().count() <= MAX_SHOWN);
    for line in body.lines() {
        assert!(line.starts_with("- "), "a line was cut mid-entry: {line}");
    }
}

/// The wording is this module's own. Reusing `decision_bands::question`
/// would ask 「役に立つか」, which is a different question — a past
/// decision can be useful without being the same one.
#[tokio::test(flavor = "current_thread")]
async fn the_question_asks_whether_it_is_the_same_one_not_whether_it_is_useful() {
    let entries = vec![entry("2026-09-19", "判定器は既定offのまま")];
    let judge = Picks {
        wanted: "同じ問いか",
        asked: Mutex::new(Vec::new()),
    };
    let matched = route(&judge, &entries, "判定器の既定を変えるか")
        .await
        .expect("route");
    assert_eq!(matched.len(), 1);
    let asked = judge.asked.lock().unwrap();
    assert!(
        !asked[0].contains("役に立つか"),
        "the calibrated wording must not leak in here: {}",
        asked[0]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn an_empty_request_or_an_empty_ledger_asks_nothing() {
    let judge = Picks {
        wanted: "x",
        asked: Mutex::new(Vec::new()),
    };
    assert!(
        route(&judge, &[], "依頼")
            .await
            .expect("empty ledger")
            .is_empty()
    );
    let entries = vec![entry("2026-09-19", "何か")];
    assert!(
        route(&judge, &entries, "  ")
            .await
            .expect("empty query")
            .is_empty()
    );
    assert!(judge.asked.lock().unwrap().is_empty());
}
