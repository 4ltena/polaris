//! A fake decider stands in for the real one; nothing is sent.
use super::super::*;
use super::*;
use polaris_provider::decision::DecisionError;
use polaris_provider::decision::{Answer, Decision};
use std::collections::BTreeMap;

/// Answers each question with whatever it was told to, without looking at
/// the questions. The checks under test are the host's, so the fake must be
/// able to give a wrong answer as easily as a right one.
struct Fake {
    /// By candidate index. Anything absent answers 0.0.
    yes: Result<std::collections::BTreeMap<usize, f64>, DecisionError>,
    /// Leave this many answers out of the reply.
    omit: usize,
    /// Answer with a probability outside 0–1.
    out_of_range: bool,
    /// Tokens in the whole request the host actually handed over: the state
    /// plus every question. The narrowing exists to keep this inside one
    /// request's budget, so the fake is where that can be checked.
    seen_request_tokens: std::sync::Mutex<Option<usize>>,
}

impl Fake {
    fn new(yes: &[(usize, f64)]) -> Self {
        Self {
            yes: Ok(yes.iter().copied().collect()),
            omit: 0,
            out_of_range: false,
            seen_request_tokens: std::sync::Mutex::new(None),
        }
    }
    fn failing(error: DecisionError) -> Self {
        Self {
            yes: Err(error),
            omit: 0,
            out_of_range: false,
            seen_request_tokens: std::sync::Mutex::new(None),
        }
    }
    fn request_tokens(&self) -> Option<usize> {
        *self.seen_request_tokens.lock().expect("the record")
    }
}

#[async_trait::async_trait]
impl Decider for Fake {
    fn capabilities(&self) -> polaris_provider::decision::DecisionCapabilities {
        // This synthetic fixture exercises the historical numerical policy.
        polaris_provider::decision::DecisionCapabilities {
            band_calibration: polaris_provider::decision::BandCalibration::LegacyJev,
            ..Default::default()
        }
    }

    async fn decide(
        &self,
        state: &serde_json::Value,
        questions: &BTreeMap<String, Question>,
    ) -> Result<Decision, DecisionError> {
        let questions_text: usize = questions
            .values()
            .map(|held| match held {
                Question::Noul { instructions } => crate::budget::count_tokens(instructions),
                _ => 0,
            })
            .sum();
        *self.seen_request_tokens.lock().expect("the record") =
            Some(crate::budget::count_tokens(&state.to_string()) + questions_text);
        let yes = self.yes.clone()?;
        let mut answers = BTreeMap::new();
        for key in questions.keys().skip(self.omit) {
            let index: usize = key
                .strip_prefix("t-")
                .and_then(|held| held.parse().ok())
                .expect("a candidate key");
            let held = *yes.get(&index).unwrap_or(&0.0);
            answers.insert(
                key.clone(),
                Answer {
                    noul: Some(if self.out_of_range { 1.5 } else { held }),
                    confidence: Some(1.0),
                    ..Default::default()
                },
            );
        }
        Ok(Decision {
            answers,
            input_tokens: Some(10),
            output_tokens: Some(0),
        })
    }
}

fn tool(name: &str, description: &str) -> polaris_mcp::catalog::index::IndexedTool {
    polaris_mcp::catalog::index::IndexedTool {
        key: name.into(),
        name: name.into(),
        description: description.into(),
        summary_hash: format!("summary-{name}"),
        schema_hash: format!("schema-{name}"),
    }
}

fn index(tools: &[(&str, &str)]) -> BTreeMap<String, ServerIndex> {
    let mut held = BTreeMap::new();
    held.insert(
        "server".to_owned(),
        ServerIndex {
            destination_hash: "destination".into(),
            partial: false,
            tools: tools
                .iter()
                .map(|(name, description)| tool(name, description))
                .collect(),
        },
    );
    held
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a runtime")
}

/// Stands in for the light model. Records what it was shown, so a test can
/// check that the middle band is all it ever sees.
#[derive(Default)]
struct Arbiter {
    reply: String,
    seen: std::sync::Mutex<Vec<polaris_provider::CompletionRequest>>,
    fail: bool,
}

impl Arbiter {
    fn choosing(reply: &str) -> Self {
        Self {
            reply: reply.into(),
            ..Default::default()
        }
    }
    fn shown(&self) -> Vec<String> {
        let seen = self.seen.lock().unwrap();
        let body: serde_json::Value =
            serde_json::from_str(&seen[0].messages[0].content).expect("the message is JSON");
        body["tools"]
            .as_array()
            .expect("a tool list")
            .iter()
            .map(|tool| tool["name"].as_str().unwrap_or_default().to_owned())
            .collect()
    }
}

#[async_trait::async_trait]
impl polaris_provider::Provider for Arbiter {
    async fn complete(
        &self,
        request: polaris_provider::CompletionRequest,
    ) -> Result<polaris_provider::CompletionResponse, polaris_provider::ProviderError> {
        self.seen.lock().unwrap().push(request);
        if self.fail {
            return Err(polaris_provider::ProviderError::Http("fixture".into()));
        }
        Ok(polaris_provider::CompletionResponse {
            text: self.reply.clone(),
            ..Default::default()
        })
    }
}

/// The typed stage alone. Nothing arbitrates, so a middle band is left
/// unjudged and says so.
fn run(fake: &Fake, tools: &[(&str, &str)], query: &str) -> (Vec<Pick>, Status) {
    let held = decided(fake, tools, query);
    (held.picks, held.status)
}

fn decided(fake: &Fake, tools: &[(&str, &str)], query: &str) -> Decided {
    decided_with(fake, None, tools, query)
}

fn decided_with(
    fake: &Fake,
    arbiter: Option<&dyn polaris_provider::Provider>,
    tools: &[(&str, &str)],
    query: &str,
) -> Decided {
    let index = index(tools);
    runtime().block_on(select_by_decision(
        fake,
        arbiter,
        Ok(Some(&index)),
        query,
        "plan",
        std::future::pending(),
    ))
}

const THREE: &[(&str, &str)] = &[
    ("search", "ファイルを検索する"),
    ("write", "ファイルを書き込む"),
    ("delete", "ファイルを削除する"),
];

#[test]
fn the_confident_are_taken_on_the_typed_answer_alone() {
    // The whole point of asking independently: several tools can each be
    // useful, and each is judged on its own.
    let fake = Fake::new(&[(0, 0.95), (1, 0.85), (2, 0.9)]);
    let held = decided(&fake, THREE, "ファイルを扱いたい");
    assert_eq!(held.status, Status::Selected);
    assert_eq!(
        held.picks
            .iter()
            .map(|p| p.key.as_str())
            .collect::<Vec<_>>(),
        vec!["search", "delete", "write"],
        "most confident first"
    );
    assert_eq!(held.bands.adopted, 3);
    assert_eq!(held.bands.middle, 0);
    // Nothing was left undecided, so no light model was needed — and none
    // was given, which would otherwise have been reported.
    assert_eq!(held.bands.excluded, 0);
}

#[test]
fn many_confident_candidates_do_not_take_the_decision_away_from_one_another() {
    // This is what a `Choice` could not do. Its probabilities sum to 1, so
    // five equally good tools would land near 0.2 each and every one would
    // fall below an adoption edge that a single good tool clears at 0.9.
    // Independent answers have no such coupling: 0.85 stays 0.85 however
    // many other tools also fit.
    let many: Vec<(&str, &str)> = vec![
        ("a", "ファイルを検索する"),
        ("b", "ファイルを書き込む"),
        ("c", "ファイルを削除する"),
        ("d", "ファイルを移動する"),
        ("e", "ファイルを複製する"),
    ];
    let fake = Fake::new(&[(0, 0.85), (1, 0.85), (2, 0.85), (3, 0.85), (4, 0.85)]);
    let held = decided(&fake, &many, "ファイルを扱いたい");
    assert_eq!(held.status, Status::Selected);
    assert_eq!(held.bands.adopted, 5);
    assert_eq!(
        held.picks.len(),
        PICKS,
        "all five cleared; the cap decides how many fit, not the edge"
    );
}

#[test]
fn the_two_edges_decide_which_band_a_candidate_lands_in() {
    let above = Fake::new(&[(0, 0.81)]);
    let held = decided(&above, THREE, "検索したい");
    assert_eq!(held.status, Status::Selected);
    assert_eq!(held.picks.len(), 1);
    assert_eq!(held.picks[0].key, "search");
    assert_eq!(held.picks[0].summary_hash, "summary-search");
    assert_eq!(held.bands.excluded, 2, "the two unanswered sit at 0.0");

    // Exactly on an edge belongs to the decided side of it. Adoption and
    // exclusion are both closed intervals, so no answer falls between them.
    assert_eq!(
        decided(&Fake::new(&[(0, ADOPT)]), THREE, "検索したい")
            .bands
            .adopted,
        1
    );
    assert_eq!(
        decided(&Fake::new(&[(0, EXCLUDE)]), THREE, "検索したい")
            .bands
            .excluded,
        3
    );

    let under = decided(&Fake::new(&[(0, EXCLUDE + 0.01)]), THREE, "検索したい");
    assert_eq!(
        under.bands.middle, 1,
        "just above the lower edge is undecided"
    );
    assert!(under.picks.is_empty(), "undecided is not selected");
}

#[test]
fn only_the_undecided_band_is_shown_to_the_light_model() {
    // Showing it what was adopted or dropped would spend tokens on
    // decisions that are made, and invite it to reopen them.
    let fake = Fake::new(&[(0, 0.95), (1, 0.6), (2, 0.1)]);
    let arbiter = Arbiter::choosing(r#"{"tools":[0]}"#);
    let held = decided_with(&fake, Some(&arbiter), THREE, "ファイルを扱いたい");
    assert_eq!(held.status, Status::Selected);
    assert_eq!(
        held.bands,
        super::super::BandCounts {
            adopted: 1,
            middle: 1,
            excluded: 1
        }
    );
    assert_eq!(
        arbiter.shown(),
        vec!["write"],
        "the adopted and the dropped must not be shown again"
    );
    assert_eq!(
        held.picks
            .iter()
            .map(|p| p.key.as_str())
            .collect::<Vec<_>>(),
        vec!["search", "write"],
        "the adopted come first, then what the comparison added"
    );
}

#[test]
fn a_full_slate_never_asks_the_light_model() {
    // Three adopted and one still undecided: the band is not empty, so
    // only the full slate can be what stops the second request.
    let fake = Fake::new(&[(0, 0.95), (1, 0.9), (2, 0.85), (3, 0.6)]);
    let mut many = THREE.to_vec();
    many.push(("extra", "ファイルを移動する"));
    let arbiter = Arbiter::choosing(r#"{"tools":[0]}"#);
    let held = decided_with(&fake, Some(&arbiter), &many, "ファイルを扱いたい");
    assert_eq!(held.picks.len(), PICKS);
    assert_eq!(held.bands.middle, 1, "something was left undecided");
    assert!(
        arbiter.seen.lock().unwrap().is_empty(),
        "there is nothing left to award, so nothing is worth asking"
    );
    assert_eq!(held.status, Status::Selected);
}

#[test]
fn what_the_typed_stage_settled_survives_a_light_model_that_fails() {
    let fake = Fake::new(&[(0, 0.95), (1, 0.6)]);
    let arbiter = Arbiter {
        fail: true,
        ..Default::default()
    };
    let held = decided_with(&fake, Some(&arbiter), THREE, "ファイルを扱いたい");
    assert_eq!(
        held.picks
            .iter()
            .map(|p| p.key.as_str())
            .collect::<Vec<_>>(),
        vec!["search"],
        "an adopted tool is not lost because the rest could not be compared"
    );
    assert_eq!(held.status, Status::ProviderFailed, "and the run says so");
}

#[test]
fn an_undecided_band_with_nobody_to_judge_it_is_reported_not_hidden() {
    let fake = Fake::new(&[(0, 0.95), (1, 0.6)]);
    let held = decided(&fake, THREE, "ファイルを扱いたい");
    assert_eq!(
        held.picks
            .iter()
            .map(|p| p.key.as_str())
            .collect::<Vec<_>>(),
        vec!["search"]
    );
    assert_eq!(
        held.status,
        Status::UnsupportedBackend,
        "leaving a band unjudged is a different outcome from having judged it"
    );
}

#[test]
fn ties_keep_the_order_the_candidates_arrived_in() {
    let fake = Fake::new(&[(0, 0.9), (1, 0.9), (2, 0.9)]);
    let (picks, _) = run(&fake, THREE, "何か");
    assert_eq!(
        picks.iter().map(|p| p.key.as_str()).collect::<Vec<_>>(),
        vec!["search", "write", "delete"],
        "the result must not depend on sort details"
    );
}

#[test]
fn a_reply_that_leaves_a_candidate_unanswered_is_refused() {
    // A narrower reply is not a narrower answer; it answers a different set.
    let mut fake = Fake::new(&[(1, 0.9), (2, 0.9)]);
    fake.omit = 1;
    let (picks, status) = run(&fake, THREE, "何か");
    assert!(picks.is_empty());
    assert_eq!(status, Status::InvalidOutput);
}

#[test]
fn a_probability_outside_the_range_is_refused() {
    let mut fake = Fake::new(&[(0, 0.9)]);
    fake.out_of_range = true;
    let (picks, status) = run(&fake, THREE, "何か");
    assert!(picks.is_empty());
    assert_eq!(status, Status::InvalidOutput);
}

#[test]
fn not_getting_an_answer_is_never_read_as_no_tool_fits() {
    for (error, expected) in [
        (DecisionError::Unavailable, Status::ProviderFailed),
        (DecisionError::Auth, Status::ProviderFailed),
        (DecisionError::TooLarge, Status::ProviderFailed),
        (DecisionError::Cancelled, Status::Cancelled),
        (DecisionError::InvalidAnswer, Status::InvalidOutput),
    ] {
        let (picks, status) = run(&Fake::failing(error.clone()), THREE, "何か");
        assert!(picks.is_empty());
        assert_eq!(status, expected, "{error:?}");
        assert_ne!(status, Status::NoneSelected);
    }
}

#[test]
fn a_judge_that_fails_mid_run_leaves_the_path_that_needs_no_judge_to_answer() {
    // Failing to build a judge already leaves the run to do what it would
    // have done without one. A judge that falls over mid-run is the same
    // state, and refusing instead takes every external tool away from the
    // run over one transient error.
    let arbiter = Arbiter::choosing(r#"{"tools":[0]}"#);
    let held = decided_with(
        &Fake::failing(DecisionError::Unavailable),
        Some(&arbiter),
        THREE,
        "ファイルを扱いたい",
    );
    assert_eq!(
        held.picks
            .iter()
            .map(|pick| pick.key.as_str())
            .collect::<Vec<_>>(),
        ["search"]
    );
    // The failure is still what the record says. A run that fell back is
    // not a run that selected cleanly, and nothing was judged.
    assert_eq!(held.status, Status::ProviderFailed);
    assert_eq!(held.bands, BandCounts::default());
    // The lexical path sees the catalogue, not a band.
    assert_eq!(arbiter.shown(), ["search", "write", "delete"]);
}

#[test]
fn a_judge_that_fails_with_no_light_model_behind_it_still_refuses() {
    let held = decided(&Fake::failing(DecisionError::Unavailable), THREE, "何か");
    assert!(held.picks.is_empty());
    assert_eq!(held.status, Status::ProviderFailed);
}

#[test]
fn a_cancelled_judge_is_not_worked_around() {
    // Every other failure means "nobody could judge". This one means the
    // person stopped the run, and a stopped run does not continue by
    // another route.
    let arbiter = Arbiter::choosing(r#"{"tools":[0]}"#);
    let held = decided_with(
        &Fake::failing(DecisionError::Cancelled),
        Some(&arbiter),
        THREE,
        "ファイルを扱いたい",
    );
    assert!(held.picks.is_empty());
    assert_eq!(held.status, Status::Cancelled);
    assert!(arbiter.seen.lock().unwrap().is_empty());
}

#[test]
fn a_cancellation_before_the_request_stops_without_asking() {
    let index = index(THREE);
    let held = runtime().block_on(select_by_decision(
        &Fake::new(&[(0, 0.9)]),
        None,
        Ok(Some(&index)),
        "何か",
        "plan",
        std::future::ready(()),
    ));
    assert!(held.picks.is_empty());
    assert_eq!(held.status, Status::Cancelled);
}

#[test]
fn the_whole_catalogue_is_asked_about_when_it_fits() {
    // A tool whose wording shares nothing with the request is still judged.
    // A lexical filter would drop it before any judgement of meaning.
    let tools: Vec<(&str, &str)> = vec![
        ("unrelated", "天気を調べる"),
        ("resize", "resize and optimize raster assets"),
    ];
    let fake = Fake::new(&[(1, 0.9)]);
    let (picks, status) = run(&fake, &tools, "画像を小さくしたい");
    assert_eq!(status, Status::Selected);
    assert_eq!(
        picks[0].key, "resize",
        "a lexical filter would have removed this candidate"
    );
}

#[test]
fn a_catalogue_too_large_for_one_request_is_narrowed_rather_than_refused() {
    // 512 is the indexed-tool ceiling; descriptions at their own ceiling
    // put the whole catalogue past what one request may carry.
    let description = "あ".repeat(160);
    let names: Vec<String> = (0..512).map(|held| format!("tool{held:04}")).collect();
    let tools: Vec<(&str, &str)> = names
        .iter()
        .map(|name| (name.as_str(), description.as_str()))
        .collect();
    // The request shares no vocabulary with any description, so the lexical
    // pass ranks nothing. Candidates must still be asked about: lexical
    // order decides which come first, never which are eligible.
    let fake = Fake::new(&[(0, 0.9)]);
    let (picks, status) = run(&fake, &tools, "何かしたい");
    assert_ne!(
        status,
        Status::NoLexicalMatch,
        "a catalogue too large to send whole is not a catalogue with no candidates"
    );
    assert!(
        matches!(status, Status::Selected | Status::NoneSelected),
        "{status:?}"
    );
    assert!(picks.len() <= PICKS);
    // And the run says so, because the guarantee that no tool was dropped
    // for its wording did not hold here.
    assert!(
        !decided(&fake, &tools, "何かしたい").considered_whole_catalogue,
        "a narrowed run must not claim the whole catalogue was judged"
    );
    // Narrowing has to reach a state one request can carry. Sending it
    // oversized would come back as `TooLarge` and select nothing at all.
    let sent = fake.request_tokens().expect("the request was made");
    assert!(
        sent <= polaris_provider::decision::MAX_STATE_TOKENS,
        "narrowed to {sent} tokens, which one request cannot carry"
    );
}

#[test]
fn a_catalogue_that_fits_is_reported_as_judged_whole() {
    let held = decided(&Fake::new(&[(0, 0.9)]), THREE, "検索したい");
    assert!(
        held.considered_whole_catalogue,
        "nothing was dropped, so the guarantee held"
    );
}

#[test]
fn the_candidate_narrowing_answers_the_same_way_it_always_did() {
    let fake = Fake::new(&[]);
    let empty: BTreeMap<String, ServerIndex> = BTreeMap::new();
    let held = runtime();
    assert_eq!(
        held.block_on(select_by_decision(
            &fake,
            None,
            Ok(Some(&empty)),
            "何か",
            "plan",
            std::future::pending()
        ))
        .status,
        Status::NoIndex
    );
    let index = index(THREE);
    assert_eq!(
        held.block_on(select_by_decision(
            &fake,
            None,
            Ok(Some(&index)),
            "   ",
            "plan",
            std::future::pending()
        ))
        .status,
        Status::NoLexicalMatch,
        "an empty request is not asked about"
    );
    assert_eq!(
        held.block_on(select_by_decision(
            &fake,
            None,
            Err(Status::IndexBusy),
            "何か",
            "plan",
            std::future::pending()
        ))
        .status,
        Status::IndexBusy
    );
}

#[test]
fn the_light_model_is_told_what_it_may_add_not_the_total() {
    // Telling it the total while showing it only the undecided band
    // invites a reply that overshoots by exactly the number already
    // adopted. Measured on 2026-09-17: two of five tasks came back over
    // the cap and lost their whole arbitration.
    let fake = Fake::new(&[(0, 0.95), (1, 0.6), (2, 0.6)]);
    let arbiter = Arbiter::choosing(r#"{"tools":[0]}"#);
    let held = decided_with(&fake, Some(&arbiter), THREE, "ファイルを扱いたい");
    assert_eq!(held.bands.adopted, 1);
    let seen = arbiter.seen.lock().unwrap();
    let body: serde_json::Value =
        serde_json::from_str(&seen[0].messages[0].content).expect("the message is JSON");
    assert_eq!(
        body["limit"], 2,
        "one of three slots is already spent: {body}"
    );
    assert_eq!(
        body["adopted"].as_array().expect("a list").len(),
        1,
        "and it is told which one, so it does not repeat it"
    );
}

#[test]
fn a_refused_reply_leaves_nothing_behind() {
    // A caller told "invalid output" while holding half of the reply
    // cannot tell what it is holding. Here the second id repeats the
    // first, so the reply is refused as a whole.
    let fake = Fake::new(&[(0, 0.95), (1, 0.6), (2, 0.6)]);
    let arbiter = Arbiter::choosing(r#"{"tools":[0,0]}"#);
    let held = decided_with(&fake, Some(&arbiter), THREE, "ファイルを扱いたい");
    assert_eq!(held.status, Status::InvalidOutput);
    assert_eq!(
        held.picks
            .iter()
            .map(|p| p.key.as_str())
            .collect::<Vec<_>>(),
        vec!["search"],
        "only what the typed stage settled survives; nothing from the refused reply"
    );
}
