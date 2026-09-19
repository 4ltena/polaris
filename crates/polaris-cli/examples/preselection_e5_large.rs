//! Does a better candidate list make the light model choose better?
//!
//! The lexical stage finds nothing when the request is Japanese and the
//! descriptions are English, and `select` then gives up before the light
//! model is asked at all. So the recorded zero is not a model failing to
//! choose; it is a model never consulted. Replacing the lexical stage is
//! only worth doing if the light model, handed a good list, actually picks
//! the right tools — and that has never been measured.
//!
//! Four arms over the same 162-tool corpus, each narrowed to the same
//! `CANDIDATES` so only the *choice* of candidates differs:
//!
//! - `head20` — the catalogue's own order. The floor, and the thing any
//!   narrowing has to beat. It costs nothing and needs no model.
//! - `e5b20` — multilingual-e5-base's top 20, read from a file because the
//!   embedding runs outside this workspace.
//! - `oracle20` — all 19 labelled tools plus one filler. The ceiling: it
//!   answers "given a perfect list, can the light model choose?", which is
//!   the question a bad result would otherwise leave ambiguous.
//! - `e5top3` — e5's own top 3, taken directly. Sends nothing, so it says
//!   whether the light model's request is earning its place.
//!
//! **Sending is opt-in.** With no arguments this contacts nothing and
//! reports what it would run. `--light` spends real requests against real
//! credentials: three arms × five tasks per pass.
//!
//!   cargo run -p polaris-cli --example preselection_e5_large
//!   cargo run -p polaris-cli --example preselection_e5_large -- --light --passes 3
use std::collections::{BTreeMap, BTreeSet};
use std::process::ExitCode;
use std::sync::Mutex;

use polaris_core::mcp::preselection::CANDIDATES;
use polaris_core::preselection_corpus::{Corpus, Row};
use polaris_mcp::catalog::index::ServerIndex;

const CORPUS: &[u8] =
    include_bytes!("../../polaris-core/tests/fixtures/preselection_corpus_large.json");
const ORDER: &[u8] =
    include_bytes!("../../../.polaris/notes/checks/2026-09-18-e5-luna-large/e5_order.json");

struct Ordering {
    /// Which corpus the ordering was measured against. An ordering paired
    /// with a corpus it did not come from scores a different question.
    corpus_digest: String,
    model: String,
    passage_form: String,
    order: BTreeMap<String, Vec<String>>,
}

impl Ordering {
    /// Read without a derive: `serde` is not a dependency of this crate, and
    /// one field per line is cheaper than adding one.
    fn parse(bytes: &[u8]) -> Result<Self, String> {
        let held: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
        let text = |key: &str| {
            held.get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| format!("{key}がありません"))
        };
        let order = held
            .get("order")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| "orderがありません".to_owned())?
            .iter()
            .map(|(task, keys)| {
                let keys = keys
                    .as_array()
                    .ok_or_else(|| format!("課題{task}の順位が配列ではありません"))?
                    .iter()
                    .map(|key| {
                        key.as_str()
                            .map(str::to_owned)
                            .ok_or_else(|| format!("課題{task}の順位に文字列でない要素があります"))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok((task.clone(), keys))
            })
            .collect::<Result<BTreeMap<_, _>, String>>()?;
        Ok(Self {
            corpus_digest: text("corpus_digest")?,
            model: text("model")?,
            passage_form: text("passage_form")?,
            order,
        })
    }
}

/// Records what was sent and what came back. `arbitrate` parses the reply
/// and drops it, so without this a rejected reply and a reply that chose
/// badly are the same row.
struct Watched {
    inner: std::sync::Arc<dyn polaris_provider::Provider>,
    seen: Mutex<Vec<(String, String)>>,
}

#[async_trait::async_trait]
impl polaris_provider::Provider for Watched {
    async fn complete(
        &self,
        request: polaris_provider::CompletionRequest,
    ) -> Result<polaris_provider::CompletionResponse, polaris_provider::ProviderError> {
        let sent = request
            .messages
            .first()
            .map(|message| message.content.clone())
            .unwrap_or_default();
        let held = self.inner.complete(request).await;
        let got = match &held {
            Ok(response) => response.text.clone(),
            Err(error) => format!("<error> {error}"),
        };
        if let Ok(mut seen) = self.seen.lock() {
            seen.push((sent, got));
        }
        held
    }
}

/// The corpus index, keeping only `keep`. Servers left empty are dropped:
/// an empty server would still be an entry the run has to reason about.
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

fn main() -> ExitCode {
    let mut light = false;
    let mut passes = 1usize;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--light" => light = true,
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
    let ordering = match Ordering::parse(ORDER) {
        Ok(ordering) => ordering,
        Err(error) => {
            eprintln!("順位ファイルを読めません: {error}");
            return ExitCode::FAILURE;
        }
    };
    if ordering.corpus_digest != corpus.digest() {
        eprintln!(
            "順位ファイルは別のコーパスに対するものです: {} ではなく {}",
            &corpus.digest()[..16],
            &ordering.corpus_digest[..16]
        );
        return ExitCode::FAILURE;
    }
    let index = corpus.index();
    eprintln!(
        "コーパス {} tools / {} tasks / digest {} / 順位 {} ({})",
        corpus.tool_count(),
        corpus.tasks.len(),
        &corpus.digest()[..16],
        ordering.model,
        ordering.passage_form
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

    let provider = light.then(build_light).flatten().map(|inner| {
        std::sync::Arc::new(Watched {
            inner,
            seen: Mutex::new(Vec::new()),
        })
    });
    if light && provider.is_none() {
        eprintln!("--light を指定しましたが軽量モデルを作れません。送信する腕は未測定になります");
    }

    let mut rows = Vec::new();
    for pass in 0..passes {
        for task in &corpus.tasks {
            let Some(ranked) = ordering.order.get(&task.id) else {
                eprintln!("順位に課題 {} がありません", task.id);
                return ExitCode::FAILURE;
            };
            // Every arm narrows to the same count, so what differs between
            // rows is which candidates were chosen and nothing else.
            let head: BTreeSet<&str> = index
                .values()
                .flat_map(|held| held.tools.iter())
                .map(|tool| tool.key.as_str())
                .take(CANDIDATES)
                .collect();
            let e5: BTreeSet<&str> = ranked.iter().take(CANDIDATES).map(String::as_str).collect();
            let mut oracle: BTreeSet<&str> = task.useful.iter().map(String::as_str).collect();
            for key in ranked {
                if oracle.len() >= CANDIDATES {
                    break;
                }
                oracle.insert(key.as_str());
            }

            for (arm, keep) in [("head20", &head), ("e5b20", &e5), ("oracle20", &oracle)] {
                rows.push(match &provider {
                    Some(provider) => {
                        let pruned = prune(&index, keep);
                        let held: usize = pruned.values().map(|server| server.tools.len()).sum();
                        // Over the cap the lexical stage runs, finds nothing
                        // against a Japanese request, and the run is refused
                        // before the model is asked. That would be measuring
                        // the giving-up branch, not the arm.
                        if held > CANDIDATES {
                            eprintln!("{arm}: 候補が{held}件で上限{CANDIDATES}を超えています");
                            return ExitCode::FAILURE;
                        }
                        let (picks, status) =
                            runtime.block_on(polaris_core::mcp::preselection::select(
                                Some(provider.as_ref()),
                                Ok(Some(&pruned)),
                                &task.query,
                                &task.phase,
                                std::future::pending(),
                            ));
                        Row::score(
                            &corpus,
                            &format!("{arm}-p{pass}"),
                            task,
                            status.as_str(),
                            picks.into_iter().map(|pick| pick.key).collect(),
                            None,
                            true,
                        )
                    }
                    None => Row::score(
                        &corpus,
                        &format!("{arm}-p{pass}"),
                        task,
                        "not_run",
                        vec![],
                        None,
                        false,
                    ),
                });
            }

            // Sends nothing, so it is measured on every pass regardless of
            // whether the light model was built, and is identical each time.
            rows.push(Row::score(
                &corpus,
                &format!("e5top3-p{pass}"),
                task,
                "selected",
                ranked
                    .iter()
                    .take(polaris_core::mcp::preselection::PICKS)
                    .cloned()
                    .collect(),
                None,
                true,
            ));
        }
    }

    for row in &rows {
        match serde_json::to_string(row) {
            Ok(line) => println!("{line}"),
            Err(error) => {
                eprintln!("行を書けません: {error}");
                return ExitCode::FAILURE;
            }
        }
    }
    if let Some(Ok(seen)) = provider.as_ref().map(|provider| provider.seen.lock()) {
        for (sent, got) in seen.iter() {
            let trace = serde_json::json!({ "sent": sent, "got": got });
            eprintln!("TRACE {trace}");
        }
    }
    let measured = rows.iter().filter(|row| row.measured).count();
    eprintln!("{measured}/{} 行を実測した", rows.len());
    ExitCode::SUCCESS
}

/// The stored Codex registration, read the way the CLI reads it. Mirrors
/// `preselection_compare.rs` rather than inventing a second way in: an arm
/// measured through a different credential path is not measuring the path a
/// run takes.
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

fn build_light() -> Option<std::sync::Arc<dyn polaris_provider::Provider>> {
    if let Ok(key) = std::env::var("POLARIS_API_KEY") {
        let base = std::env::var("POLARIS_BASE_URL")
            .unwrap_or_else(|_| "https://api.openai.com/v1".to_owned());
        return polaris_provider::openai::OpenAiProvider::new(
            base,
            key,
            polaris_core::mcp::preselection::MODEL.to_owned(),
        )
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
        polaris_core::mcp::preselection::MODEL.to_owned(),
        std::sync::Arc::new(StoredTokens { store }),
    )
    .ok()
    .map(|provider| std::sync::Arc::new(provider) as std::sync::Arc<dyn polaris_provider::Provider>)
}
