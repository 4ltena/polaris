//! 段階1: the arms that can be measured without a new adapter.
//!
//! Two arms run against the fixed corpus: the lexical + light-model path
//! that every run uses today, and the three-stage path with the typed
//! judge. The local arm is deliberately absent — it needs the adapter L2
//! builds, and a row that cannot be measured is written as
//! `measured: false` rather than omitted, so a later comparison cannot
//! read a missing arm as an arm that chose nothing.
//!
//! **Sending is opt-in per arm.** With no arguments this contacts nothing
//! and prints what it would run. `--light` uses the configured light model
//! and `--judge` the configured typed judge; both cost real requests
//! against real credentials, so neither happens unless it is asked for.
//!
//!   cargo run -p polaris-cli --example preselection_compare
//!   set -a; . ./.env.local; set +a
//!   cargo run -p polaris-cli --example preselection_compare -- --judge
use std::process::ExitCode;

use polaris_core::preselection_corpus::{Corpus, Row};

const CORPUS: &[u8] = include_bytes!("../../polaris-core/tests/fixtures/preselection_corpus.json");

fn main() -> ExitCode {
    let mut light = false;
    let mut judge = false;
    for argument in std::env::args().skip(1) {
        match argument.as_str() {
            "--light" => light = true,
            "--judge" => judge = true,
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
    let index = corpus.index();
    eprintln!(
        "コーパス {} tools / {} tasks / digest {}",
        corpus.tool_count(),
        corpus.tasks.len(),
        &corpus.digest()[..16]
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

    // Built once and shared, so an arm's cost is the arm's and not the
    // setup's. Absent means that arm is reported unmeasured.
    let selector = light.then(build_light).flatten();
    let decider = judge.then(build_judge).flatten();
    if light && selector.is_none() {
        eprintln!("--light を指定しましたが軽量モデルを作れません。legacy腕は未測定になります");
    }
    if judge && decider.is_none() {
        eprintln!("--judge を指定しましたが判定器を作れません。typed腕は未測定になります");
    }

    let mut rows = Vec::new();
    for task in &corpus.tasks {
        rows.push(match &selector {
            Some(provider) => {
                let (picks, status) = runtime.block_on(polaris_core::mcp::preselection::select(
                    Some(provider.as_ref()),
                    Ok(Some(&index)),
                    &task.query,
                    &task.phase,
                    std::future::pending(),
                ));
                Row::score(
                    &corpus,
                    "legacy",
                    task,
                    status.as_str(),
                    picks.into_iter().map(|pick| pick.key).collect(),
                    None,
                    true,
                )
            }
            None => Row::score(&corpus, "legacy", task, "not_run", vec![], None, false),
        });

        rows.push(match &decider {
            Some(decider) => {
                let held = runtime.block_on(polaris_core::mcp::preselection::select_by_decision(
                    decider.as_ref(),
                    selector.as_deref(),
                    Ok(Some(&index)),
                    &task.query,
                    &task.phase,
                    std::future::pending(),
                ));
                Row::score(
                    &corpus,
                    "typed",
                    task,
                    held.status.as_str(),
                    held.picks.into_iter().map(|pick| pick.key).collect(),
                    Some([held.bands.adopted, held.bands.middle, held.bands.excluded]),
                    true,
                )
            }
            None => Row::score(&corpus, "typed", task, "not_run", vec![], None, false),
        });

        // The arm L2 makes possible. Written, never guessed.
        rows.push(Row::score(
            &corpus,
            "local_typed",
            task,
            "adapter_missing",
            vec![],
            None,
            false,
        ));
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
    let measured = rows.iter().filter(|row| row.measured).count();
    eprintln!("{measured}/{} 行を実測した", rows.len());
    ExitCode::SUCCESS
}

/// The stored Codex registration, read the way the CLI reads it.
///
/// Mirrors `polaris-cli`'s own `AuthTokens` rather than inventing a second
/// way in: an arm measured through a different credential path is not
/// measuring the path a run takes, and the reasoning effort the plan
/// implies is part of what is being compared.
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
    // The same independently constructed selector a run uses, built the
    // same way, so the arm measures the run's path and not a variant.
    // The API-key form is tried first because it needs no stored
    // credential; the stored Codex registration is the fallback and, on
    // this machine, the only one that exists.
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

fn build_judge() -> Option<std::sync::Arc<dyn polaris_provider::decision::Decider>> {
    polaris_provider::decision::jev::JevDecider::from_environment()
        .ok()
        .map(|decider| {
            std::sync::Arc::new(decider) as std::sync::Arc<dyn polaris_provider::decision::Decider>
        })
}
