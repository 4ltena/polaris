//! Model inventory and selection using the existing provider lifetime.
use super::*;

#[derive(Clone, Copy)]
enum ModelMenuChoice {
    Hosted(&'static str),
    Local,
}

struct ConnectedLocalModel {
    runtime: Runtime,
    adapter: Arc<LocalAdapter>,
    observed: ModelMetadata,
}

impl ConnectedLocalModel {
    fn provider_name(&self) -> &'static str {
        match self.runtime {
            Runtime::Ollama => "ollama",
            Runtime::LmStudio => "lmstudio",
            Runtime::LlamaCpp => "llamacpp",
        }
    }

    fn runtime_label(&self) -> &'static str {
        match self.runtime {
            Runtime::Ollama => "Ollama",
            Runtime::LmStudio => "LM Studio",
            Runtime::LlamaCpp => "llama.cpp",
        }
    }

    fn display_name(&self) -> String {
        format!("{} · {}", self.runtime_label(), self.observed.id())
    }
}

async fn discover_local_runtime(runtime: Runtime, endpoint: &str) -> Vec<ConnectedLocalModel> {
    let Ok(endpoint) = Endpoint::parse(endpoint) else {
        return Vec::new();
    };
    let Ok(adapter) = LocalAdapter::new(runtime, endpoint) else {
        return Vec::new();
    };
    let adapter = Arc::new(adapter);
    let Ok(models) = adapter.inventory().await else {
        return Vec::new();
    };
    models
        .into_iter()
        .filter(|model| model.capabilities().completion != Capability::Unsupported)
        .map(|observed| ConnectedLocalModel {
            runtime,
            adapter: adapter.clone(),
            observed,
        })
        .collect()
}

async fn discover_connected_local_models() -> Vec<ConnectedLocalModel> {
    let (ollama, lmstudio) = tokio::join!(
        discover_local_runtime(Runtime::Ollama, "http://127.0.0.1:11434/"),
        discover_local_runtime(Runtime::LmStudio, "http://127.0.0.1:1234/"),
    );
    ollama.into_iter().chain(lmstudio).collect()
}

/// Runs the `/model` picker — three hosted choices plus one local-runtime
/// entry. Reasoning effort is intentionally absent because routing owns it.
fn run_model_picker<B: ratatui::backend::Backend, R: approver::KeyReader>(
    terminal: &RefCell<ratatui::Terminal<B>>,
    reader: &mut R,
    current: &str,
    current_is_local: bool,
) -> Option<ModelMenuChoice> {
    let catalog = render::MODEL_CATALOG;
    let mut selected = if current_is_local {
        catalog.len()
    } else {
        catalog.iter().position(|m| *m == current).unwrap_or(0)
    };
    let count = catalog.len() + 1;
    loop {
        if terminal
            .borrow_mut()
            .draw(|f| render::render_model_picker(f, current, current_is_local, selected))
            .is_err()
        {
            return None;
        }
        use ratatui::crossterm::event::KeyCode;
        match reader.read_key() {
            Ok(KeyCode::Down) => selected = (selected + 1) % count,
            Ok(KeyCode::Up) => selected = (selected + count - 1) % count,
            Ok(KeyCode::Enter) if selected == catalog.len() => {
                return Some(ModelMenuChoice::Local);
            }
            Ok(KeyCode::Enter) => return Some(ModelMenuChoice::Hosted(catalog[selected])),
            Ok(KeyCode::Esc) => return None,
            Ok(_) => continue,
            Err(_) => return None,
        }
    }
}

fn run_local_model_picker<B: ratatui::backend::Backend, R: approver::KeyReader>(
    terminal: &RefCell<ratatui::Terminal<B>>,
    reader: &mut R,
    models: &[ConnectedLocalModel],
    current_provider: &str,
    current_model: &str,
) -> Option<usize> {
    if models.is_empty() {
        return None;
    }
    let labels: Vec<String> = models
        .iter()
        .map(ConnectedLocalModel::display_name)
        .collect();
    let current = models.iter().position(|model| {
        model.provider_name() == current_provider && model.observed.id() == current_model
    });
    let current_label = current.map(|index| labels[index].as_str());
    let mut selected = current.unwrap_or(0);
    loop {
        if terminal
            .borrow_mut()
            .draw(|f| render::render_local_model_picker(f, &labels, current_label, selected))
            .is_err()
        {
            return None;
        }
        use ratatui::crossterm::event::KeyCode;
        match reader.read_key() {
            Ok(KeyCode::Down) => selected = (selected + 1) % models.len(),
            Ok(KeyCode::Up) => selected = (selected + models.len() - 1) % models.len(),
            Ok(KeyCode::Enter) => return Some(selected),
            Ok(KeyCode::Esc) => return None,
            Ok(_) => continue,
            Err(_) => return None,
        }
    }
}

/// Handles `/model`. Hosted choices reuse the authenticated provider. The
/// local row discovers only the same two loopback runtimes as the desktop,
/// then installs an immutable local binding for the chosen observed model.
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_model<B: ratatui::backend::Backend, R: approver::KeyReader>(
    terminal: &RefCell<ratatui::Terminal<B>>,
    reader: &mut R,
    provider: &mut Arc<dyn Provider>,
    cloud_provider: &Arc<dyn Provider>,
    usage_meter: &polaris_provider::UsageMeter,
    local_router: &polaris_core::local_execution::LocalRouter,
    provider_name: &mut String,
    cloud_provider_name: &str,
    model_name: &mut String,
    cloud_effort_name: &str,
    effort_name: &mut String,
    status: &mut Status,
) {
    loop {
        #[cfg(target_os = "linux")]
        if signal::pending_linux_process_shutdown().is_some() {
            return;
        }
        let current_is_local = is_local_provider(provider_name);
        let Some(choice) = run_model_picker(terminal, reader, model_name, current_is_local) else {
            return;
        };
        #[cfg(target_os = "linux")]
        if signal::pending_linux_process_shutdown().is_some() {
            return;
        }
        match choice {
            ModelMenuChoice::Hosted(picked_model) => {
                if !matches!(cloud_provider_name, "codex" | "openai") {
                    *status = Status::Error(
                        "hosted models are unavailable for this launch; restart with a hosted provider"
                            .to_string(),
                    );
                    return;
                }
                cloud_provider.set_model(picked_model);
                *provider = cloud_provider.clone();
                *provider_name = cloud_provider_name.to_string();
                *model_name = picked_model.to_string();
                *effort_name = cloud_effort_name.to_string();
                *status = Status::Notice(format!("model set to {picked_model}"));
                return;
            }
            ModelMenuChoice::Local => {
                let models = discover_connected_local_models().await;
                #[cfg(target_os = "linux")]
                if signal::pending_linux_process_shutdown().is_some() {
                    return;
                }
                let Some(index) =
                    run_local_model_picker(terminal, reader, &models, provider_name, model_name)
                else {
                    if models.is_empty() {
                        *status = Status::Notice("no connected local models".to_string());
                        return;
                    }
                    continue;
                };
                #[cfg(target_os = "linux")]
                if signal::pending_linux_process_shutdown().is_some() {
                    return;
                }
                let picked = &models[index];
                let selection = match picked.adapter.select(&picked.observed).await {
                    Ok(selection) => selection,
                    Err(error) => {
                        *status = Status::Error(format!("local model is unavailable: {error}"));
                        return;
                    }
                };
                #[cfg(target_os = "linux")]
                if signal::pending_linux_process_shutdown().is_some() {
                    return;
                }
                if selection.model().capabilities().tools != Capability::Supported {
                    *status = Status::Error(
                        "local model does not report the tool support required by polaris"
                            .to_string(),
                    );
                    return;
                }
                let binding = match local_router.bind(selection) {
                    Ok(binding) => binding,
                    Err(error) => {
                        *status = Status::Error(format!("local model cannot be used: {error}"));
                        return;
                    }
                };
                *provider = Arc::new(usage_meter.wrap(binding));
                *provider_name = picked.provider_name().to_string();
                *model_name = picked.observed.id().to_string();
                effort_name.clear();
                *status = Status::Notice(format!(
                    "local model set to {} · {}",
                    picked.runtime_label(),
                    picked.observed.id()
                ));
                return;
            }
        }
    }
}
