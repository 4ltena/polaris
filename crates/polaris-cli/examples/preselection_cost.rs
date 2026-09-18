//! What each preselection route costs, and who pays it.
//!
//! Accuracy was settled on 2026-09-18. This asks the other half of the same
//! question: how many tokens the main model spends under each route, and how
//! long the route takes. Four routes over the same 162-tool corpus and the
//! same five tasks, counted with the host's own tokenizer on the bytes the
//! shipped code actually builds — no request shape is reconstructed here.
//!
//! - `none` — no preselection at all. Nothing is selected, so every tool is
//!   carried on every turn. The thing preselection exists to avoid.
//! - `codex` — the selector slot filled by the main model instead of the
//!   light one, which is what the stage cost before a light model had it.
//!   Same request, same narrowing, different price.
//! - `jev` — the typed judge. One request, one question per tool, and the
//!   main model is not asked anything.
//! - `e5_luna` — e5-base narrows locally, the light model chooses. The
//!   embedding runs outside this workspace, so its ordering is read from the
//!   file the accuracy measurement used.
//!
//! Two costs are separated because they are paid at different rates. The
//! selection request is paid **once per run**: `Context::prepare` returns
//! early unless the selection is pending. What the picks weigh is paid on
//! **every turn**, because the resolved schemas go into `sending_tools` each
//! time round the loop. A route that halves the first and leaves the second
//! alone is not halving anything that matters.
//!
//! **Sending is opt-in.** With no arguments this contacts nothing: every
//! request is built through the real path and measured, and the reply is
//! canned. `--send` spends real requests against real credentials — five per
//! sending route — which is what latency and the service's own reported
//! usage need. The typed judge needs `POLARIS_JEV_API_KEY`; without it that
//! route is counted but not timed.
//!
//!   cargo run -p polaris-cli --example preselection_cost
//!   cargo run -p polaris-cli --example preselection_cost -- --send
use std::collections::{BTreeMap, BTreeSet};
use std::process::ExitCode;
use std::sync::Mutex;
use std::time::Instant;

use polaris_core::mcp::preselection::{CANDIDATES, PICKS};
use polaris_core::preselection_corpus::Corpus;
use polaris_mcp::catalog::index::ServerIndex;
use polaris_provider::decision::{Answer, Decider, Decision, DecisionError, Question};

const CORPUS: &[u8] =
    include_bytes!("../../polaris-core/tests/fixtures/preselection_corpus_large.json");
const ORDER: &[u8] =
    include_bytes!("../../../docs/superpowers/checks/2026-09-18-e5-luna-large/e5_order.json");

/// The model the selector slot holds when the stage is not given a cheaper
/// one. It is the same model the run itself uses, which is the whole point
/// of the comparison.
const MAIN_MODEL: &str = "gpt-6-astra";

/// Records the request the shipped code built, then either answers it from
/// a fixture or forwards it and times the round trip.
struct Selector {
    inner: Option<std::sync::Arc<dyn polaris_provider::Provider>>,
    seen: Mutex<Vec<Sent>>,
}

struct Sent {
    tokens: usize,
    /// How many candidates the request carried. The routes reach the light
    /// model with different numbers of them, and tokens alone cannot say
    /// whether that is a long description or a longer list.
    candidates: usize,
    elapsed_ms: Option<u128>,
    input: Option<u32>,
    output: Option<u32>,
}

#[async_trait::async_trait]
impl polaris_provider::Provider for Selector {
    async fn complete(
        &self,
        request: polaris_provider::CompletionRequest,
    ) -> Result<polaris_provider::CompletionResponse, polaris_provider::ProviderError> {
        // The whole request is counted, not the candidate list inside it:
        // the framing is sent too, and a comparison that drops it flatters
        // whichever route has the most framing per candidate.
        let tokens: usize = request
            .messages
            .iter()
            .map(|message| polaris_core::budget::count_tokens(&message.content))
            .sum::<usize>()
            + polaris_core::budget::count_tokens(&request.system);
        let candidates = request
            .messages
            .first()
            .and_then(|message| serde_json::from_str::<serde_json::Value>(&message.content).ok())
            .and_then(|body| {
                body.get("tools")
                    .and_then(|held| held.as_array())
                    .map(Vec::len)
            })
            .unwrap_or_default();
        let Some(inner) = &self.inner else {
            self.record(Sent {
                tokens,
                candidates,
                elapsed_ms: None,
                input: None,
                output: None,
            });
            // A reply naming as many candidates as the request could bear.
            // Naming `PICKS` unconditionally would be rejected as
            // out-of-range whenever the lexical stage found fewer, and the
            // route would be recorded as a malformed reply rather than as
            // the short candidate list it actually was.
            let ids: Vec<usize> = (0..PICKS.min(candidates)).collect();
            return Ok(polaris_provider::CompletionResponse {
                text: serde_json::json!({ "tools": ids }).to_string(),
                ..Default::default()
            });
        };
        let started = Instant::now();
        let held = inner.complete(request).await;
        let elapsed = started.elapsed().as_millis();
        let usage = held.as_ref().ok().and_then(|reply| reply.usage.as_ref());
        self.record(Sent {
            tokens,
            candidates,
            elapsed_ms: Some(elapsed),
            input: usage.map(|usage| usage.input_tokens),
            output: usage.map(|usage| usage.output_tokens),
        });
        held
    }
}

impl Selector {
    fn record(&self, sent: Sent) {
        if let Ok(mut seen) = self.seen.lock() {
            seen.push(sent);
        }
    }

    fn take(&self) -> Option<Sent> {
        self.seen.lock().ok().and_then(|mut seen| seen.pop())
    }
}

/// Records the typed request the shipped code built. Without a key it
/// answers from a fixture: the first `PICKS` candidates clear the adoption
/// edge and the rest fall below the exclusion edge, so the adopted band
/// fills the slate and no light model is asked anything. That is the shape
/// a real judged run takes when the answers are confident, and it keeps the
/// route's cost to the one request this measures.
struct Judge {
    inner: Option<polaris_provider::decision::jev::JevDecider>,
    seen: Mutex<Vec<Sent>>,
}

#[async_trait::async_trait]
impl Decider for Judge {
    async fn decide(
        &self,
        state: &serde_json::Value,
        questions: &BTreeMap<String, Question>,
    ) -> Result<Decision, DecisionError> {
        let body = serde_json::json!({ "state": state, "questions": questions });
        let tokens = polaris_core::budget::count_tokens(&body.to_string());
        let Some(inner) = &self.inner else {
            let answers = questions
                .keys()
                .enumerate()
                .map(|(rank, key)| {
                    (
                        key.clone(),
                        Answer {
                            noul: Some(if rank < PICKS { 1.0 } else { 0.0 }),
                            ..Default::default()
                        },
                    )
                })
                .collect();
            self.record(Sent {
                tokens,
                candidates: questions.len(),
                elapsed_ms: None,
                input: None,
                output: None,
            });
            return Ok(Decision {
                answers,
                input_tokens: None,
                output_tokens: None,
            });
        };
        let started = Instant::now();
        let held = inner.decide(state, questions).await;
        let elapsed = started.elapsed().as_millis();
        let usage = held.as_ref().ok();
        self.record(Sent {
            tokens,
            candidates: questions.len(),
            elapsed_ms: Some(elapsed),
            input: usage
                .and_then(|held| held.input_tokens)
                .map(|held| held as u32),
            output: usage
                .and_then(|held| held.output_tokens)
                .map(|held| held as u32),
        });
        held
    }
}

impl Judge {
    fn record(&self, sent: Sent) {
        if let Ok(mut seen) = self.seen.lock() {
            seen.push(sent);
        }
    }

    fn take(&self) -> Option<Sent> {
        self.seen.lock().ok().and_then(|mut seen| seen.pop())
    }
}

/// The e5 ordering, paired with the corpus it was measured against. An
/// ordering from another corpus scores a different question, so the digest
/// is checked rather than trusted.
fn ordering(corpus_digest: &str) -> Result<BTreeMap<String, Vec<String>>, String> {
    let held: serde_json::Value = serde_json::from_slice(ORDER).map_err(|e| e.to_string())?;
    let digest = held
        .get("corpus_digest")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "corpus_digestがありません".to_owned())?;
    if digest != corpus_digest {
        return Err(format!(
            "順位ファイルは別のコーパスに対するものです: {digest}"
        ));
    }
    held.get("order")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| "orderがありません".to_owned())?
        .iter()
        .map(|(task, keys)| {
            let keys = keys
                .as_array()
                .ok_or_else(|| format!("課題{task}の順位が配列ではありません"))?
                .iter()
                .filter_map(|key| key.as_str().map(str::to_owned))
                .collect();
            Ok((task.clone(), keys))
        })
        .collect()
}

/// The corpus index, keeping only `keep`. Servers left empty are dropped.
fn prune(
    index: &BTreeMap<String, ServerIndex>,
    keep: &BTreeSet<&str>,
) -> BTreeMap<String, ServerIndex> {
    index
        .iter()
        .filter_map(|(server_id, held)| {
            let tools: Vec<_> = held
                .tools
                .iter()
                .filter(|tool| keep.contains(tool.key.as_str()))
                .cloned()
                .collect();
            (!tools.is_empty()).then(|| {
                (
                    server_id.clone(),
                    ServerIndex {
                        tools,
                        ..held.clone()
                    },
                )
            })
        })
        .collect()
}

/// What the named tools weigh on every turn once their schemas are in the
/// request. The corpus carries names and descriptions but no input schemas,
/// so this is a **floor**: a real schema only adds to it, and it adds to
/// the route that carries the most tools fastest.
fn per_turn_tokens(index: &BTreeMap<String, ServerIndex>, keys: &BTreeSet<String>) -> usize {
    let specs: Vec<polaris_tools::ToolSpec> = index
        .values()
        .flat_map(|held| held.tools.iter())
        .filter(|tool| keys.contains(&tool.key))
        .map(|tool| polaris_tools::ToolSpec {
            name: tool.name.clone(),
            description: tool.description.clone(),
            parameters: serde_json::json!({}),
        })
        .collect();
    polaris_core::budget::always_on_tokens("", &specs)
}

fn main() -> ExitCode {
    let mut send = false;
    let mut passes = 1usize;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--send" => send = true,
            // Latency is the one figure here that a single reading cannot
            // settle: the same request measured twice differs by seconds.
            "--passes" => match arguments.next().and_then(|held| held.parse().ok()) {
                Some(held) => passes = held,
                None => {
                    eprintln!("--passes には回数が要ります");
                    return ExitCode::FAILURE;
                }
            },
            other => {
                eprintln!("知らない引数です: {other}");
                return ExitCode::FAILURE;
            }
        }
    }

    let corpus = match Corpus::parse(CORPUS) {
        Ok(corpus) => corpus,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let order = match ordering(&corpus.digest()) {
        Ok(order) => order,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let index = corpus.index();
    let all: BTreeSet<String> = index
        .values()
        .flat_map(|held| held.tools.iter())
        .map(|tool| tool.key.clone())
        .collect();
    eprintln!(
        "コーパス {} tools / {} tasks / digest {} / 送信 {}",
        corpus.tool_count(),
        corpus.tasks.len(),
        &corpus.digest()[..16],
        if send { "する" } else { "しない" }
    );

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("runtimeを作れません: {error}");
            return ExitCode::FAILURE;
        }
    };

    let luna = Selector {
        inner: send
            .then(|| build(polaris_core::mcp::preselection::MODEL))
            .flatten(),
        seen: Mutex::new(Vec::new()),
    };
    // The judged route still asks the light model about whatever the typed
    // answers left in the middle band, and that request is part of what the
    // route costs. Recording it on the `Selector` the `e5_luna` row reads
    // would credit it to the wrong route, so the arbiter gets its own
    // recorder over the same provider.
    let arbiter = Selector {
        inner: luna.inner.clone(),
        seen: Mutex::new(Vec::new()),
    };
    let astra = Selector {
        inner: send.then(|| build(MAIN_MODEL)).flatten(),
        seen: Mutex::new(Vec::new()),
    };
    let judge = Judge {
        inner: send
            .then(polaris_provider::decision::jev::JevDecider::from_environment)
            .and_then(Result::ok),
        seen: Mutex::new(Vec::new()),
    };
    if send {
        for (name, built) in [
            ("軽量モデル", luna.inner.is_some()),
            ("主モデル", astra.inner.is_some()),
            ("判定器", judge.inner.is_some()),
        ] {
            if !built {
                eprintln!("{name}を作れません。その経路は送信せず、要求量だけを数えます");
            }
        }
    }

    let mut rows: Vec<serde_json::Value> = Vec::new();
    for pass in 0..passes {
        for task in &corpus.tasks {
            let Some(ranked) = order.get(&task.id) else {
                eprintln!("順位に課題 {} がありません", task.id);
                return ExitCode::FAILURE;
            };
            let e5: BTreeSet<&str> = ranked.iter().take(CANDIDATES).map(String::as_str).collect();

            // No preselection: nothing is selected and nothing is asked, so the
            // whole catalogue rides on every turn.
            rows.push(at(
                pass,
                row(
                    &corpus,
                    task,
                    "none",
                    "—",
                    None,
                    per_turn_tokens(&index, &all),
                    &all,
                ),
            ));

            for (route, model, selector, held) in [
                ("codex", MAIN_MODEL, &astra, index.clone()),
                (
                    "e5_luna",
                    polaris_core::mcp::preselection::MODEL,
                    &luna,
                    prune(&index, &e5),
                ),
            ] {
                let (picks, status) = runtime.block_on(polaris_core::mcp::preselection::select(
                    Some(selector),
                    Ok(Some(&held)),
                    &task.query,
                    &task.phase,
                    std::future::pending(),
                ));
                let keys: BTreeSet<String> = picks.iter().map(|pick| pick.key.clone()).collect();
                let mut held = at(
                    pass,
                    row(
                        &corpus,
                        task,
                        route,
                        model,
                        selector.take(),
                        per_turn_tokens(&index, &keys),
                        &keys,
                    ),
                );
                held["status"] = status.as_str().into();
                rows.push(held);
            }

            let decided = runtime.block_on(polaris_core::mcp::preselection::select_by_decision(
                &judge,
                Some(&arbiter),
                Ok(Some(&index)),
                &task.query,
                &task.phase,
                std::future::pending(),
            ));
            let keys: BTreeSet<String> =
                decided.picks.iter().map(|pick| pick.key.clone()).collect();
            let mut held = at(
                pass,
                row(
                    &corpus,
                    task,
                    "jev",
                    "jev-latest",
                    judge.take(),
                    per_turn_tokens(&index, &keys),
                    &keys,
                ),
            );
            held["status"] = decided.status.as_str().into();
            held["considered_whole_catalogue"] = decided.considered_whole_catalogue.into();
            held["bands"] = serde_json::json!({
                "adopted": decided.bands.adopted,
                "middle": decided.bands.middle,
                "excluded": decided.bands.excluded,
            });
            // Absent when the typed answers settled everything. Present is
            // not a failure: it is the second stage the route is allowed.
            if let Some(sent) = arbiter.take() {
                held["arbiter_request_tokens"] = serde_json::json!(sent.tokens);
                held["arbiter_candidates"] = serde_json::json!(sent.candidates);
                held["arbiter_elapsed_ms"] = serde_json::json!(sent.elapsed_ms);
                held["arbiter_reported_input"] = serde_json::json!(sent.input);
                held["arbiter_reported_output"] = serde_json::json!(sent.output);
            }
            rows.push(held);
        }
    }

    for held in &rows {
        println!("{held}");
    }
    ExitCode::SUCCESS
}

/// Which pass a row was taken on. Stamped rather than carried through
/// `row`'s arguments, the same way the status is.
fn at(pass: usize, mut held: serde_json::Value) -> serde_json::Value {
    held["pass"] = pass.into();
    held
}

/// One measured route on one task. `main_model_tokens` is the part the run's
/// own model pays, which is the number the comparison is about: it is the
/// selection request only when the selector *is* that model.
fn row(
    corpus: &Corpus,
    task: &polaris_core::preselection_corpus::CorpusTask,
    route: &str,
    model: &str,
    sent: Option<Sent>,
    per_turn: usize,
    picks: &BTreeSet<String>,
) -> serde_json::Value {
    let request = sent.as_ref().map_or(0, |sent| sent.tokens);
    serde_json::json!({
        "corpus_digest": corpus.digest(),
        "task": task.id,
        "route": route,
        "selector_model": model,
        "selector_request_tokens": request,
        "selector_candidates": sent.as_ref().map_or(0, |sent| sent.candidates),
        "main_model_tokens": if model == MAIN_MODEL { request } else { 0 },
        "picks": picks.len(),
        "picked": picks,
        "per_turn_tool_tokens": per_turn,
        "elapsed_ms": sent.as_ref().and_then(|sent| sent.elapsed_ms),
        "reported_input": sent.as_ref().and_then(|sent| sent.input),
        "reported_output": sent.as_ref().and_then(|sent| sent.output),
        "sent": sent.as_ref().is_some_and(|sent| sent.elapsed_ms.is_some()),
    })
}

fn build(model: &str) -> Option<std::sync::Arc<dyn polaris_provider::Provider>> {
    if let Ok(key) = std::env::var("POLARIS_API_KEY") {
        let base = std::env::var("POLARIS_BASE_URL")
            .unwrap_or_else(|_| "https://api.openai.com/v1".to_owned());
        return polaris_provider::openai::OpenAiProvider::new(base, key, model.to_owned())
            .ok()
            .map(|provider| {
                std::sync::Arc::new(provider) as std::sync::Arc<dyn polaris_provider::Provider>
            });
    }
    let store = polaris_auth::store::default_path().ok()?;
    if !store.exists() {
        return None;
    }
    polaris_provider::codex::CodexProvider::new(
        polaris_provider::codex::ENDPOINT_BASE.to_owned(),
        model.to_owned(),
        std::sync::Arc::new(StoredTokens { store }),
    )
    .ok()
    .map(|provider| std::sync::Arc::new(provider) as std::sync::Arc<dyn polaris_provider::Provider>)
}

/// The stored Codex registration, read the way the CLI reads it. Mirrors
/// `preselection_e5_large.rs` rather than inventing a second way in.
struct StoredTokens {
    store: std::path::PathBuf,
}

#[async_trait::async_trait]
impl polaris_provider::TokenSource for StoredTokens {
    async fn token(&self) -> Result<polaris_provider::Token, polaris_provider::ProviderError> {
        let held = polaris_auth::ensure_fresh(polaris_auth::ISSUER, &self.store)
            .await
            .map_err(|error| polaris_provider::ProviderError::Auth(error.to_string()))?;
        Ok(into_token(held))
    }

    async fn refreshed(&self) -> Result<polaris_provider::Token, polaris_provider::ProviderError> {
        let held = polaris_auth::force_refresh(polaris_auth::ISSUER, &self.store)
            .await
            .map_err(|error| polaris_provider::ProviderError::Auth(error.to_string()))?;
        Ok(into_token(held))
    }
}

fn into_token(held: polaris_auth::Credentials) -> polaris_provider::Token {
    let effort = polaris_auth::effort_for_plan_type(
        polaris_auth::token::plan_type_from_access_token(&held.access_token).as_deref(),
    )
    .map(str::to_owned);
    polaris_provider::Token {
        access_token: held.access_token,
        account_id: held.account_id,
        effort,
    }
}
