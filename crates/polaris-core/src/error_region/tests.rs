//! Each test names one property of the selection and fails if that property
//! is removed; none of them would pass against a head-of-text stub.
use super::*;

/// The line counts a gap marker claims, and the lines actually printed.
fn accounting(out: &str) -> (usize, usize) {
    let omitted = |line: &str| {
        let count = line.strip_prefix("… ")?.strip_suffix(" 行省略 …")?;
        count.replace(',', "").parse::<usize>().ok()
    };
    let claimed: usize = out.lines().filter_map(omitted).sum();
    let printed = out.lines().filter(|line| omitted(line).is_none()).count();
    (printed, claimed)
}

#[test]
fn a_marker_drags_its_context_lines_along_with_it() {
    let mut text = vec!["noise".to_owned(); 200];
    text[99] = "   Compiling polaris-core v1.0.0".into();
    text[100] = "error[E0308]: mismatched types".into();
    text[101] = "  --> src/main.rs:5:9".into();
    text[104] = "   = note: expected `u8`".into();
    let out = select(&text.join("\n"), 600);
    assert!(out.contains("error[E0308]"), "{out}");
    assert!(out.contains("--> src/main.rs:5:9"), "位置行が落ちた: {out}");
    assert!(
        out.contains("= note: expected"),
        "後続の説明が落ちた: {out}"
    );
    assert!(
        out.contains("Compiling polaris-core"),
        "直前の行が落ちた: {out}"
    );
}

/// One line in the middle of a quiet log, once per marker: a marker that
/// stopped being recognised would leave that line in the omitted middle.
#[test]
fn each_failure_marker_pulls_its_line_out_of_the_middle() {
    for marker in [
        "error[E0308]: mismatched types",
        "thread 'main' panicked at src/lib.rs:1:1",
        "test result: FAILED. 0 passed",
        "1 test failed",
        "assertion `left == right` failed",
        "Traceback (most recent call last):",
        "fatal: not a git repository",
        "E/AndroidRuntime( 1234): FATAL EXCEPTION: main",
        "java.lang.NullPointerException: null",
    ] {
        let mut text = vec!["noise".to_owned(); 400];
        text[200] = marker.to_owned();
        let out = select(&text.join("\n"), 400);
        assert!(out.contains(marker), "{marker} を見落とした: {out}");
    }
    // `E/` is a severity column, not a substring: a path must not look like a
    // failure just because a directory name ends in `e`.
    let mut text = vec!["noise".to_owned(); 400];
    text[200] = "   reading /Users/who/File/projects/x".into();
    let out = select(&text.join("\n"), 400);
    assert!(
        !out.contains("/Users/who/File"),
        "パスを障害と誤認した: {out}"
    );
}

#[test]
fn a_warning_counts_only_when_nothing_stronger_appears() {
    let mut quiet = vec!["noise".to_owned(); 400];
    quiet[200] = "warning: unused variable `x`".into();
    let out = select(&quiet.join("\n"), 400);
    assert!(out.contains("warning: unused variable"), "{out}");

    let mut loud = quiet.clone();
    loud[380] = "error: aborting due to 1 previous error".into();
    let out = select(&loud.join("\n"), 400);
    assert!(out.contains("error: aborting"), "{out}");
    assert!(!out.contains("warning: unused variable"), "{out}");
}

#[test]
fn the_verdict_on_the_last_line_survives_a_pile_of_earlier_failures() {
    let mut text: Vec<String> = (0..400)
        .map(|i| format!("thread '{i}' panicked at src/lib.rs:1:1"))
        .collect();
    text.extend((0..20).map(|_| "   note: run with RUST_BACKTRACE=1".to_owned()));
    text.push("exit code 101".into());
    let out = select(&text.join("\n"), 800);
    assert!(out.ends_with("exit code 101"), "{out}");
    assert!(out.contains("panicked at"), "{out}");
}

#[test]
fn overlapping_windows_are_emitted_once() {
    let mut text = vec!["noise".to_owned(); 100];
    text[90] = "error one".into();
    text[91] = "shared context".into();
    text[92] = "error two".into();
    let out = select(&text.join("\n"), 300);
    assert_eq!(out.matches("shared context").count(), 1, "{out}");
    assert_eq!(out.matches("error one").count(), 1, "{out}");
    assert!(!out.contains("… 1 行省略 …"), "隣接領域が割れた: {out}");
}

#[test]
fn every_dropped_line_is_counted_in_a_gap_marker() {
    let mut text = vec!["noise".to_owned(); 2000];
    text[1990] = "fatal: out of memory".into();
    let out = select(&text.join("\n"), 400);
    let first = out.lines().next().unwrap();
    assert!(
        first.starts_with("… ") && first.ends_with(" 行省略 …"),
        "冒頭の省略が示されていない: {first}"
    );
    assert!(first.contains(','), "四桁以上は桁区切り: {first}");
    let (printed, claimed) = accounting(&out);
    assert_eq!(printed + claimed, 2000, "行数の申告が合わない: {out}");
}

#[test]
fn the_last_failure_wins_when_the_markers_alone_overrun_the_budget() {
    let text: Vec<String> = (0..300).map(|i| format!("error: 障害 {i} 番")).collect();
    let out = select(&text.join("\n"), 500);
    assert!(out.len() <= 500, "{} bytes", out.len());
    assert!(out.contains("障害 299 番"), "{out}");
    assert!(!out.contains("障害 0 番"), "{out}");
    let (printed, claimed) = accounting(&out);
    assert_eq!(
        printed + claimed,
        300,
        "捨てた行数が申告されていない: {out}"
    );
}

#[test]
fn a_cut_lands_on_a_character_boundary_inside_the_budget() {
    let mut text: Vec<String> = (0..50).map(|i| format!("実行 {i} 番の記録")).collect();
    text.push("障".repeat(200)); // One line far longer than the whole budget.
    let out = select(&text.join("\n"), 101);
    assert!(out.len() <= 101, "{} bytes", out.len());
    assert!(out.ends_with('障'), "末尾が落ちた: {out}");
    assert!(!out.contains("実行 0 番"), "先頭を返している: {out}");
    assert!(
        out.chars().all(|c| c == '障' || c == '…' || c == '\n'),
        "{out}"
    );
}

#[test]
fn a_log_that_never_fails_still_shows_its_head_and_its_tail() {
    let text: Vec<String> = (0..500).map(|i| format!("line {i}")).collect();
    let out = select(&text.join("\n"), 300);
    assert!(out.len() <= 300, "{} bytes", out.len());
    assert!(out.starts_with("line 0\n"), "{out}");
    assert!(out.ends_with("line 499"), "{out}");
    assert!(out.contains("行省略"), "{out}");
    let (printed, claimed) = accounting(&out);
    assert_eq!(printed + claimed, 500, "{out}");
}

#[test]
fn text_that_already_fits_comes_back_byte_for_byte() {
    let text = "error: 失敗\n続きの行\n";
    assert_eq!(select(text, text.len()), text);
    assert_eq!(select(text, 4096), text);
}

/// Answers every line with the same probability, and counts the asking.
/// `None` fails instead of answering.
struct Judge {
    yes: Option<f64>,
    asked: std::sync::Mutex<usize>,
}

#[async_trait::async_trait]
impl polaris_provider::decision::Decider for Judge {
    async fn decide(
        &self,
        _state: &serde_json::Value,
        questions: &std::collections::BTreeMap<String, polaris_provider::decision::Question>,
    ) -> Result<polaris_provider::decision::Decision, polaris_provider::decision::DecisionError>
    {
        *self.asked.lock().unwrap() += 1;
        let Some(yes) = self.yes else {
            return Err(polaris_provider::decision::DecisionError::Unavailable);
        };
        Ok(polaris_provider::decision::Decision {
            answers: questions
                .keys()
                .map(|key| {
                    // Only the piece that names the thing asked about is
                    // useful; everything else is noise. Judged on the text
                    // rather than on an index, so the answer does not depend
                    // on whether the unit is a line or a block.
                    let names_it = questions.get(key).is_some_and(|question| match question {
                        polaris_provider::decision::Question::Noul { instructions } => {
                            instructions.contains("retry_budget")
                        }
                        _ => false,
                    });
                    (
                        key.clone(),
                        polaris_provider::decision::Answer {
                            noul: Some(if names_it { yes } else { 0.0 }),
                            ..Default::default()
                        },
                    )
                })
                .collect(),
            input_tokens: None,
            output_tokens: None,
        })
    }
}

fn judge(yes: Option<f64>) -> std::sync::Arc<Judge> {
    std::sync::Arc::new(Judge {
        yes,
        asked: std::sync::Mutex::new(0),
    })
}

/// 180 lines with nothing a marker scan can aim at, and one line that answers
/// the request. Under the judged-line bound so the judge is consulted.
fn unmarked() -> String {
    let mut text = vec!["ordinary output".to_owned(); 180];
    text[120] = "the setting that controls the retry budget is retry_budget".into();
    text[179] = "done".into();
    text.join("\n")
}

/// The judge aims at a block, and the whole block arrives.
///
/// The budget is the real one — `tool_memory::EXCERPT_BYTES` — rather than
/// the 400 this used when the unit was a single line. A block carries ten
/// lines and `assemble` adds its own context around them, so a squeeze that
/// fitted one line cannot fit a region, and testing against a budget no
/// caller uses would only measure the squeeze.
#[tokio::test(flavor = "current_thread")]
async fn a_judged_line_becomes_what_the_excerpt_aims_at() {
    const BUDGET: usize = 1024;
    let held = judge(Some(1.0));
    let out = select_judged(
        Some(held.as_ref()),
        "retryの予算はどこで決まるか",
        &unmarked(),
        BUDGET,
    )
    .await;
    assert_eq!(
        *held.asked.lock().unwrap(),
        1,
        "the judge was asked more than once"
    );
    assert!(out.contains("retry_budget"), "{out}");
    // The scan alone cannot find it: nothing in this text is a failure marker.
    assert!(!select(&unmarked(), BUDGET).contains("retry_budget"));
}

/// A text the scan can already answer is not re-decided. Paying a round trip
/// to agree with a measured rule buys nothing.
#[tokio::test(flavor = "current_thread")]
async fn a_strong_marker_settles_it_without_asking() {
    let held = judge(Some(1.0));
    let mut text = vec!["ordinary output".to_owned(); 180];
    text[120] = "error[E0308]: mismatched types".into();
    let out = select_judged(Some(held.as_ref()), "問い", &text.join("\n"), 400).await;
    assert_eq!(*held.asked.lock().unwrap(), 0);
    assert!(out.contains("E0308"));
}

/// No judge, one that cannot answer, and one that excluded every line all
/// leave the scan's answer standing.
#[tokio::test(flavor = "current_thread")]
async fn without_a_usable_answer_the_scan_still_answers() {
    let text = unmarked();
    let plain = select(&text, 400);
    assert_eq!(select_judged(None, "問い", &text, 400).await, plain);
    let failing = judge(None);
    assert_eq!(
        select_judged(Some(failing.as_ref()), "問い", &text, 400).await,
        plain
    );
    assert_eq!(*failing.asked.lock().unwrap(), 1);
    let excluding = judge(Some(0.0));
    assert_eq!(
        select_judged(Some(excluding.as_ref()), "問い", &text, 400).await,
        plain
    );
}

/// A log is unbounded and a question per line is not.
#[tokio::test(flavor = "current_thread")]
async fn past_the_line_bound_the_scan_answers_alone() {
    let held = judge(Some(1.0));
    let text = vec!["ordinary output".to_owned(); MAX_JUDGED_LINES + 1].join("\n");
    let out = select_judged(Some(held.as_ref()), "問い", &text, 400).await;
    assert_eq!(*held.asked.lock().unwrap(), 0);
    assert_eq!(out, select(&text, 400));
}

/// Whatever decided the aim, the excerpt still accounts for every line.
#[tokio::test(flavor = "current_thread")]
async fn a_judged_excerpt_reports_every_line_it_left_out() {
    let held = judge(Some(1.0));
    let text = unmarked();
    let out = select_judged(Some(held.as_ref()), "問い", &text, 400).await;
    let (printed, claimed) = accounting(&out);
    assert_eq!(printed + claimed, text.lines().count(), "{out}");
}
