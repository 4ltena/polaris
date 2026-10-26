//! Platform-independent startup defaults and explicit selection precedence.

/// Resolve both providers identically; an account plan cannot replace an
/// explicit or default model.
pub(super) fn selected_model(
    explicit: Option<String>,
    environment: Option<String>,
    interactive: bool,
) -> String {
    explicit.or(environment).unwrap_or_else(|| {
        if interactive {
            polaris_tui::render::RECOMMENDED_MODEL
        } else {
            polaris_provider::codex::DEFAULT_MODEL
        }
        .to_string()
    })
}

/// Interactive turns let Jev choose effort. One-shot runs keep the historical
/// xhigh default so benchmarks and automation remain fixed unless explicitly
/// overridden.
pub(super) fn selected_effort(explicit: Option<&str>, has_prompt: bool) -> Option<&str> {
    explicit.or(has_prompt.then_some("xhigh"))
}

/// When `POLARIS_PROVIDER` isn't set, decides which provider arm to try
/// first. Prefers `openai` if a key is available (env var or the saved
/// `api_key.json`), since that's the arm onboarding itself lives on. If no
/// openai key exists but the user already signed in with ChatGPT (saved
/// `~/.polaris/auth.json`, from `polaris login` or onboarding, or a registered
/// account), prefer
/// `codex` instead of re-triggering onboarding from scratch. With neither
/// credential present, default to `openai` so onboarding fires.
pub(super) fn default_provider_name(
    has_openai_key: bool,
    has_saved_codex_credentials: bool,
) -> &'static str {
    if !has_openai_key && has_saved_codex_credentials {
        "codex"
    } else {
        "openai"
    }
}
