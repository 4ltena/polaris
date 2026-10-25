//! Provider construction for the native CLI run after continuation preflight.

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use polaris_provider::openai::OpenAiProvider;

use crate::{Args, Shared, TierWeights, account, tier_providers};

/// Provider handles that must stay together for the whole native turn.
///
/// The local server is deliberately retained here after the providers are
/// handed to the rest of the run: dropping it would terminate `llama-server`.
pub(super) struct PreparedProviders {
    pub(super) provider: Shared,
    pub(super) mcp_selection_provider: Option<Shared>,
    pub(super) tier_weights: TierWeights,
    pub(super) provider_name: String,
    pub(super) model_name: String,
    _llama_server: Option<polaris_core::llama_server::LlamaServer>,
}

async fn onboard(
    store: &Path,
    accounts: account::StartupAccounts,
) -> Result<polaris_tui::onboarding::Outcome, ExitCode> {
    let api_key_path = polaris_auth::api_key::default_path().map_err(|error| {
        eprintln!("Can't determine where to store the API key: {error}");
        ExitCode::FAILURE
    })?;
    let outcome = match accounts {
        account::StartupAccounts::LoggedOut(owner) => {
            polaris_tui::onboarding::run_registered(owner, &api_key_path).await
        }
        _ => polaris_tui::onboarding::run(store, &api_key_path).await,
    };
    outcome.map_err(|error| match error {
        polaris_tui::onboarding::OnboardingError::Cancelled => ExitCode::SUCCESS,
        other => {
            eprintln!("{other}");
            ExitCode::FAILURE
        }
    })
}

fn startup_accounts() -> Result<account::StartupAccounts, ExitCode> {
    account::startup().map_err(|error| {
        eprintln!("{error}");
        ExitCode::FAILURE
    })
}

/// Construct the primary, selector, and tier providers after preflight.
///
/// Errors are reported at the existing provider boundary so callers preserve
/// the native command's exit behavior without learning credential details.
pub(super) async fn prepare(
    args: &Args,
    config: &polaris_core::config::Config,
    project_root: &Path,
    model: String,
    initial_effort_name: Option<&str>,
    attempt_ledger: polaris_provider::attempts::AttemptLedger,
    mut provider_name: String,
) -> Result<PreparedProviders, ExitCode> {
    let model_name: String;
    let mut llama_server = None;
    let (provider, mcp_selection_provider, tier_weights): (Shared, Option<Shared>, TierWeights) = loop {
        match provider_name.as_str() {
            "openai" => {
                if args.account.is_some() {
                    eprintln!(
                        "--accountはcodexのみで使えます。POLARIS_PROVIDER=codexを指定してください。"
                    );
                    return Err(ExitCode::FAILURE);
                }
                let base = std::env::var("POLARIS_BASE_URL")
                    .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
                let key = std::env::var("POLARIS_API_KEY").ok().or_else(|| {
                    polaris_auth::api_key::default_path().ok().and_then(|p| {
                        match polaris_auth::api_key::load_from(&p) {
                            Ok(k) => k,
                            Err(e) => {
                                eprintln!("Can't read the saved API key: {e}");
                                None
                            }
                        }
                    })
                });
                let key = match key {
                    Some(k) => k,
                    None if args.prompt.is_none() => {
                        let auth_store_path = match polaris_auth::store::default_path() {
                            Ok(p) => p,
                            Err(e) => {
                                eprintln!("Can't determine where to store credentials: {e}");
                                return Err(ExitCode::FAILURE);
                            }
                        };
                        match onboard(&auth_store_path, startup_accounts()?).await {
                            Ok(polaris_tui::onboarding::Outcome::ApiKeySaved) => continue,
                            Ok(polaris_tui::onboarding::Outcome::CodexLoggedIn) => {
                                provider_name = "codex".to_string();
                                continue;
                            }
                            Err(status) => return Err(status),
                        }
                    }
                    None => {
                        eprintln!("POLARIS_API_KEY is not set");
                        return Err(ExitCode::FAILURE);
                    }
                };
                model_name = model.clone();
                let selector = OpenAiProvider::new(
                    base.clone(),
                    key.clone(),
                    polaris_core::mcp::preselection::MODEL.into(),
                )
                .ok()
                .map(|provider| {
                    Arc::new(provider.with_attempt_ledger(attempt_ledger.clone()))
                        as Arc<dyn polaris_provider::Provider>
                });
                let weights = tier_providers(&config.tiers, &model, initial_effort_name, |named| {
                    OpenAiProvider::new(base.clone(), key.clone(), named.to_owned())
                        .map(|provider| {
                            Arc::new(provider.with_attempt_ledger(attempt_ledger.clone()))
                                as Arc<dyn polaris_provider::Provider>
                        })
                        .map_err(|error| error.to_string())
                });
                let weights = match weights {
                    Ok(weights) => weights,
                    Err(message) => {
                        eprintln!("{message}");
                        return Err(ExitCode::FAILURE);
                    }
                };
                match OpenAiProvider::new(base, key, model.clone())
                    .map(|provider| provider.with_attempt_ledger(attempt_ledger.clone()))
                {
                    Ok(p) => break (Arc::new(p), selector, weights),
                    Err(e) => {
                        eprintln!("Can't build the client: {e}");
                        return Err(ExitCode::FAILURE);
                    }
                }
            }
            "codex" => {
                let store = match polaris_auth::store::default_path() {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("Can't determine where to store credentials: {e}");
                        return Err(ExitCode::FAILURE);
                    }
                };
                if args.prompt.is_none() && args.account.is_none() {
                    let accounts = startup_accounts()?;
                    let needs_login = match &accounts {
                        account::StartupAccounts::LoggedOut(_) => true,
                        account::StartupAccounts::Legacy => polaris_auth::store::load_from(&store)
                            .map_err(|error| {
                                eprintln!("Can't read credentials: {error}");
                                ExitCode::FAILURE
                            })?
                            .is_none(),
                        account::StartupAccounts::Registered => false,
                    };
                    if needs_login {
                        match onboard(&store, accounts).await? {
                            polaris_tui::onboarding::Outcome::ApiKeySaved => {
                                provider_name = "openai".into();
                            }
                            polaris_tui::onboarding::Outcome::CodexLoggedIn => {}
                        }
                        continue;
                    }
                }
                model_name = model.clone();
                if let Err(message) = account::bind(&model, args.account.as_deref()) {
                    eprintln!("{message}");
                    return Err(ExitCode::FAILURE);
                }
                let tokens = match account::require(store) {
                    Ok(tokens) => tokens,
                    Err(message) => {
                        eprintln!("{message}");
                        return Err(ExitCode::FAILURE);
                    }
                };
                let selector = polaris_provider::codex::CodexProvider::new(
                    polaris_provider::codex::ENDPOINT_BASE.to_string(),
                    polaris_core::mcp::preselection::MODEL.into(),
                    tokens.clone(),
                )
                .ok()
                .map(|provider| {
                    Arc::new(provider.with_attempt_ledger(attempt_ledger.clone()))
                        as Arc<dyn polaris_provider::Provider>
                });
                let weights = tier_providers(&config.tiers, &model, initial_effort_name, |named| {
                    polaris_provider::codex::CodexProvider::new(
                        polaris_provider::codex::ENDPOINT_BASE.to_string(),
                        named.to_owned(),
                        tokens.clone(),
                    )
                    .map(|provider| {
                        Arc::new(provider.with_attempt_ledger(attempt_ledger.clone()))
                            as Arc<dyn polaris_provider::Provider>
                    })
                    .map_err(|error| error.to_string())
                });
                let weights = match weights {
                    Ok(weights) => weights,
                    Err(message) => {
                        eprintln!("{message}");
                        return Err(ExitCode::FAILURE);
                    }
                };
                match polaris_provider::codex::CodexProvider::new(
                    polaris_provider::codex::ENDPOINT_BASE.to_string(),
                    model.clone(),
                    tokens,
                )
                .map(|provider| provider.with_attempt_ledger(attempt_ledger.clone()))
                {
                    Ok(provider) => break (Arc::new(provider), selector, weights),
                    Err(error) => {
                        eprintln!("Can't build the client: {error}");
                        return Err(ExitCode::FAILURE);
                    }
                }
            }
            "llamacpp" => {
                let Some(llama) = config.llama_cpp.clone() else {
                    eprintln!(
                        "POLARIS_PROVIDER=llamacpp には `[llama_cpp]` の server・models_dir・build が要ります。"
                    );
                    return Err(ExitCode::FAILURE);
                };
                let state = match polaris_core::project::state_dir(project_root) {
                    Ok(directory) => directory,
                    Err(error) => {
                        eprintln!("状態ディレクトリを特定できません: {error}");
                        return Err(ExitCode::FAILURE);
                    }
                };
                let started = async {
                    let server =
                        polaris_core::llama_server::LlamaServer::start(&llama, &state).await?;
                    let adapter = polaris_core::llama_server::adapter(&server)
                        .map_err(|_| std::io::Error::other("問い合わせ口を作成できません"))?;
                    let inventory = adapter
                        .inventory_with_load_state()
                        .await
                        .map_err(|error| std::io::Error::other(format!("{error}")))?;
                    let observed = inventory
                        .iter()
                        .find(|held| held.id() == model)
                        .ok_or_else(|| {
                            let names: Vec<&str> = inventory.iter().map(|held| held.id()).collect();
                            std::io::Error::other(format!(
                                "{model} が {} にありません。あるのは {names:?}",
                                llama.models_dir.display()
                            ))
                        })?;
                    let selection = adapter
                        .select(observed)
                        .await
                        .map_err(|error| std::io::Error::other(format!("{error}")))?;
                    let provider =
                        polaris_provider::local_inference::LocalInferenceProvider::with_token(
                            selection,
                            server.token(),
                        )
                        .map_err(|error| std::io::Error::other(format!("{error}")))?;
                    Ok::<_, std::io::Error>((server, provider))
                }
                .await;
                let (server, provider) = match started {
                    Ok(started) => started,
                    Err(error) => {
                        eprintln!("llama-serverを起動できません: {error}");
                        return Err(ExitCode::FAILURE);
                    }
                };
                llama_server = Some(server);
                model_name = model.clone();
                break (Arc::new(provider), None, TierWeights::new());
            }
            other => {
                eprintln!(
                    "POLARIS_PROVIDER is an unknown value {other}. Specify openai, codex or llamacpp"
                );
                return Err(ExitCode::FAILURE);
            }
        }
    };

    if let Some(effort) = initial_effort_name {
        provider.set_effort(Some(effort));
    }

    Ok(PreparedProviders {
        provider,
        mcp_selection_provider,
        tier_weights,
        provider_name,
        model_name,
        _llama_server: llama_server,
    })
}
