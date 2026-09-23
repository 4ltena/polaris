//! Loads config files. Not existing is normal; being malformed is not.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The merged configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// User-owned presentation preferences. Project configuration is not
    /// allowed to change these values for a shared checkout.
    pub presentation: PresentationConfig,
    /// User-owned final answer policy. `Enforce` is only a request here; the
    /// runtime must still prove the benchmark activation lock for the exact
    /// turn snapshot before it may publish through that path.
    pub answer_finalizer: AnswerFinalizerConfig,
    /// Strict history is opt-in and requires a pinned local embedding backend.
    pub history_mode: crate::conversation_state::HistoryMode,
    /// Reversible preparation for eligible new root inputs. Off unless explicitly set.
    pub input_preparation: crate::input_preparation::InputPreparationMode,
    pub embedding: EmbeddingConfig,
    /// Built-in workflow skills are enabled unless configuration explicitly opts out.
    pub workflow: crate::workflow::WorkflowConfig,
    /// Whether root tool mutations automatically regenerate affected
    /// `files.md` maps.
    ///
    /// Off unless asked for. It was on, and a 2026-09-19 audit found the
    /// output has no reader: nothing loads a `files.md` into a request, the
    /// writer gitignores what it produces (`files_md.rs`), and
    /// `docs/filemap.md` — which the system prompt does point at — is built
    /// from `git ls-files --exclude-standard`, so the two are mutually
    /// invisible. What remained was a subagent run per changed directory,
    /// up to 32 per call, producing files nobody read.
    pub files_md_auto_regenerate: bool,
    /// Additional places to look for skills. Not included in the 2 default locations.
    pub skills_paths: Vec<PathBuf>,
    /// Additional places to look for subagent types. Not included in the 2 default locations
    /// (`<project_root>/agents`, `<HOME>/.polaris/agents`).
    pub agents_paths: Vec<PathBuf>,
    /// How many `spawn` tasks a single wave may run concurrently. TOML key:
    /// `[spawn] concurrency`. Defaults to
    /// `polaris_spawn::DEFAULT_CONCURRENCY` (8).
    pub spawn_concurrency: usize,
    /// Among a wave's concurrently-running tasks, how many may hold a
    /// `write_root` at once — tighter than `spawn_concurrency` because
    /// writes contend for I/O and disk in a way reads don't. TOML key:
    /// `[spawn] write_concurrency`. Defaults to
    /// `polaris_spawn::DEFAULT_WRITE_CONCURRENCY` (4).
    pub spawn_write_concurrency: usize,
    /// Which model and reasoning effort each declared weight is worth. TOML
    /// keys: `[tier.dwarf]`, `[tier.subgiant]`, `[tier.giant]`,
    /// `[tier.supergiant]`. Everything
    /// unset inherits the parent run's own model, which is what happens
    /// when the table is absent entirely.
    pub tiers: TierConfig,
    /// The typed judge, used both to preselect tools and to weigh delegated
    /// tasks. One setting for both, because both ask the same service the
    /// same kind of question and a run that trusts it for one has no reason
    /// to withhold the other.
    pub decision: DecisionConfig,
    /// Route a high-confidence distinct input into one empty sidechat when a
    /// typed decider is configured. The classifier-unavailable path always
    /// remains in the current conversation.
    pub sidechat_auto: bool,
    /// How large this project's build output may grow before the oldest of
    /// it is removed. TOML keys: `[build_cache] max_bytes`,
    /// `[build_cache] max_percent`. Unset means nothing is ever removed.
    pub build_cache: crate::build_cache::Cap,
    /// Which directory the cap applies to. `None` means `<project>/target`.
    pub build_cache_path: Option<PathBuf>,
    /// How much of `--approval always`'s band the person has agreed in
    /// advance to stop being asked about. TOML key: `[approval] auto`.
    ///
    /// **`off` unless written down.** This is the only setting here that
    /// widens what runs without a person answering, so it does not acquire
    /// a value from a judge becoming reachable, from a default that seemed
    /// convenient, or from any other configuration key.
    pub approval_auto: crate::approval::AutoApproval,
    /// The bundled `llama-server`, when a project pinned one. TOML table:
    /// `[llama_cpp]` with `server`, `models_dir` and `build`.
    ///
    /// `None` starts nothing. There is no default path and no search:
    /// which binary to execute is the one thing this must never guess.
    pub llama_cpp: Option<crate::llama_server::LlamaCppConfig>,
}

/// The language Polaris should use when presenting final answers. Absence is
/// distinct from automatic detection: it means no translation suggestion may
/// be derived from the OS locale or from message contents.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PresentationLanguage {
    #[default]
    Unspecified,
    Ja,
    En,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PresentationConfig {
    pub language: PresentationLanguage,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerFinalizerMode {
    #[default]
    Off,
    Shadow,
    Enforce,
}

impl AnswerFinalizerMode {
    /// Configuration never constitutes benchmark approval. This explicit
    /// predicate prevents a downstream owner from treating a decoded
    /// `enforce` spelling as an activation decision.
    pub fn requires_benchmark_lock(self) -> bool {
        self == Self::Enforce
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum JudgeProfile {
    #[serde(rename = "fulfillment-v1")]
    FulfillmentV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum TranslationProfile {
    #[serde(rename = "cat-translate-3.3b")]
    CatTranslate3_3b,
}

impl TranslationProfile {
    pub const fn id(self) -> &'static str {
        match self {
            Self::CatTranslate3_3b => "cat-translate-3.3b",
        }
    }

    /// The path is owned by the managed installer/runtime contract. Config
    /// selects this named profile and cannot supply an arbitrary project path.
    pub fn managed_model_path(self, home: &Path) -> PathBuf {
        match self {
            Self::CatTranslate3_3b => home
                .join(".polaris/models/cat-translate-3.3b")
                .join(crate::translation::CAT_TRANSLATE_REVISION),
        }
    }
}

/// A saved request to use translation. It is not proof that the model,
/// licence, identity, privacy receipt, or benchmark decision is valid.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TranslationActivation {
    #[default]
    Disabled,
    Requested,
}

impl TranslationActivation {
    pub fn is_requested(self) -> bool {
        self == Self::Requested
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TranslationConfig {
    pub activation: TranslationActivation,
    pub profile: Option<TranslationProfile>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnswerFinalizerConfig {
    pub mode: AnswerFinalizerMode,
    pub judge_profile: Option<JudgeProfile>,
    /// Explicit user consent to disclose the current prompt, candidate, and
    /// closed tool evidence to the remote finalization judge.  A shadow
    /// finalizer is inert without this separate opt-in.
    pub external_disclosure: bool,
    pub translation: TranslationConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalizerActivationRequirement {
    None,
    JudgeBenchmark,
    TranslationBenchmark,
    CompositeBenchmark,
}

impl AnswerFinalizerConfig {
    /// Describes evidence a runtime owner must validate. The user config only
    /// requests a path; it cannot manufacture a passed benchmark decision.
    pub fn activation_requirement(&self) -> FinalizerActivationRequirement {
        match (
            self.mode.requires_benchmark_lock(),
            self.translation.activation.is_requested(),
        ) {
            (false, false) => FinalizerActivationRequirement::None,
            (true, false) => FinalizerActivationRequirement::JudgeBenchmark,
            (false, true) => FinalizerActivationRequirement::TranslationBenchmark,
            (true, true) => FinalizerActivationRequirement::CompositeBenchmark,
        }
    }
}

/// The API key is deliberately not here. A project's configuration file is
/// committed and shared; a credential read from the environment is not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DecisionConfig {
    /// Off unless asked for. Every path that consults the judge works
    /// without one, so enabling it is a choice to spend a round trip.
    pub enabled: bool,
    /// Which judge answers. Named rather than inferred: a run that picks
    /// its decider from whichever resource happens to be present is a run
    /// whose evidence nobody can attribute.
    pub backend: DecisionBackend,
    pub base_url: Option<String>,
    pub model: Option<String>,
    /// The SemIf helper, absolute. Only meaningful for that backend, and
    /// rejected under the other one rather than ignored — a key that does
    /// nothing reads as a key that did something.
    pub helper: Option<PathBuf>,
}

/// The two judges. Unknown spellings are refused at load, so a typo stops
/// the run instead of silently selecting the default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DecisionBackend {
    #[default]
    Jev,
    SemIf,
}

impl DecisionBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Jev => "jev",
            Self::SemIf => "semif",
        }
    }
}

/// One entry per weight `polaris_skills::Tier` declares, so a new weight
/// cannot be added to the enum without a place here to say what it costs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TierConfig {
    pub dwarf: TierSettings,
    pub subgiant: TierSettings,
    pub giant: TierSettings,
    pub supergiant: TierSettings,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TierSettings {
    /// `None` keeps the parent run's model rather than naming one.
    pub model: Option<String>,
    pub effort: Option<String>,
}

impl TierConfig {
    pub fn for_tier(&self, tier: polaris_skills::Tier) -> &TierSettings {
        match tier {
            polaris_skills::Tier::Dwarf => &self.dwarf,
            polaris_skills::Tier::Subgiant => &self.subgiant,
            polaris_skills::Tier::Giant => &self.giant,
            polaris_skills::Tier::Supergiant => &self.supergiant,
        }
    }

    /// Whether any weight names anything at all. A run with none of them
    /// set has nothing to route and can skip building per-weight providers.
    pub fn is_empty(&self) -> bool {
        [&self.dwarf, &self.subgiant, &self.giant, &self.supergiant]
            .into_iter()
            .all(|held| held.model.is_none() && held.effort.is_none())
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            presentation: PresentationConfig::default(),
            answer_finalizer: AnswerFinalizerConfig::default(),
            history_mode: crate::conversation_state::HistoryMode::Legacy,
            input_preparation: crate::input_preparation::InputPreparationMode::Off,
            embedding: EmbeddingConfig::default(),
            workflow: crate::workflow::WorkflowConfig {
                enabled: true,
                always: vec!["builtin:workflow-core".into()],
                phase_skills: crate::workflow::Phase::ALL
                    .into_iter()
                    .filter(|phase| *phase != crate::workflow::Phase::General)
                    .map(|phase| (phase, vec![format!("builtin:{phase}")]))
                    .collect(),
                ..Default::default()
            },
            files_md_auto_regenerate: false,
            skills_paths: Vec::new(),
            agents_paths: Vec::new(),
            spawn_concurrency: crate::spawn::DEFAULT_CONCURRENCY,
            spawn_write_concurrency: crate::spawn::DEFAULT_WRITE_CONCURRENCY,
            tiers: TierConfig::default(),
            decision: DecisionConfig::default(),
            sidechat_auto: true,
            build_cache: crate::build_cache::Cap::default(),
            build_cache_path: None,
            approval_auto: crate::approval::AutoApproval::default(),
            llama_cpp: None,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    #[serde(default)]
    presentation: Option<RawPresentation>,
    #[serde(default)]
    answer_finalizer: Option<RawAnswerFinalizer>,
    history_mode: Option<crate::conversation_state::HistoryMode>,
    input_preparation: Option<crate::input_preparation::InputPreparationMode>,
    #[serde(default)]
    embedding: EmbeddingConfig,
    #[serde(default)]
    workflow: RawWorkflow,
    #[serde(default)]
    files_md: RawFilesMd,
    #[serde(default)]
    skills: RawSkills,
    #[serde(default)]
    agents: RawAgents,
    #[serde(default)]
    spawn: RawSpawn,
    #[serde(default)]
    tier: RawTier,
    #[serde(default)]
    decision: RawDecision,
    #[serde(default)]
    sidechat: RawSidechat,
    #[serde(default)]
    build_cache: RawBuildCache,
    #[serde(default)]
    approval: RawApproval,
    #[serde(default)]
    llama_cpp: RawLlamaCpp,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPresentation {
    #[serde(default)]
    language: Option<RawPresentationLanguage>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawPresentationLanguage {
    Ja,
    En,
}

impl From<RawPresentationLanguage> for PresentationLanguage {
    fn from(value: RawPresentationLanguage) -> Self {
        match value {
            RawPresentationLanguage::Ja => Self::Ja,
            RawPresentationLanguage::En => Self::En,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAnswerFinalizer {
    #[serde(default)]
    mode: Option<AnswerFinalizerMode>,
    #[serde(default)]
    judge_profile: Option<JudgeProfile>,
    #[serde(default)]
    external_disclosure: Option<bool>,
    #[serde(default)]
    translation: Option<RawTranslation>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTranslation {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    profile: Option<TranslationProfile>,
}

impl RawAnswerFinalizer {
    fn apply_to(self, merged: &mut AnswerFinalizerConfig) {
        if let Some(mode) = self.mode {
            merged.mode = mode;
        }
        if let Some(profile) = self.judge_profile {
            merged.judge_profile = Some(profile);
        }
        if let Some(external_disclosure) = self.external_disclosure {
            merged.external_disclosure = external_disclosure;
        }
        if let Some(translation) = self.translation {
            if let Some(enabled) = translation.enabled {
                merged.translation.activation = if enabled {
                    TranslationActivation::Requested
                } else {
                    TranslationActivation::Disabled
                };
            }
            if let Some(profile) = translation.profile {
                merged.translation.profile = Some(profile);
            }
        }
    }

    fn settled(merged: &AnswerFinalizerConfig) -> Result<(), ConfigError> {
        if merged.mode != AnswerFinalizerMode::Off && merged.judge_profile.is_none() {
            return Err(ConfigError::MissingFinalizerJudgeProfile);
        }
        if merged.translation.activation.is_requested() && merged.translation.profile.is_none() {
            return Err(ConfigError::MissingTranslationProfile);
        }
        if merged.external_disclosure && merged.mode != AnswerFinalizerMode::Shadow {
            return Err(ConfigError::ExternalFinalizerDisclosureMode);
        }
        Ok(())
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLlamaCpp {
    #[serde(default)]
    server: Option<PathBuf>,
    #[serde(default)]
    models_dir: Option<PathBuf>,
    #[serde(default)]
    build: Option<String>,
}

impl RawLlamaCpp {
    /// All three or none. Two of three is not a partial configuration to
    /// fill in with a default — it is a pin somebody stopped writing, and
    /// completing it here would execute a binary nobody named.
    fn resolve(self) -> Result<Option<crate::llama_server::LlamaCppConfig>, ConfigError> {
        match (self.server, self.models_dir, self.build) {
            (None, None, None) => Ok(None),
            (Some(server), Some(models_dir), Some(build)) => {
                Ok(Some(crate::llama_server::LlamaCppConfig {
                    server,
                    models_dir,
                    build,
                }))
            }
            _ => Err(ConfigError::IncompleteLlamaCpp),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawApproval {
    #[serde(default)]
    auto: Option<crate::approval::AutoApproval>,
}

/// Explicit, local-only embedding runtime. No downloads occur during a turn.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingConfig {
    pub runtime: Option<PathBuf>,
    pub model_path: Option<PathBuf>,
    pub revision: Option<String>,
    /// The e5-base snapshot the preselection ranking stage uses (M10 L2).
    ///
    /// Separate from `model_path` because they are different models and
    /// both may be present: strict10 pins e5-small and the ranking stage
    /// pins e5-base. One field would force a project to choose, and
    /// whichever it chose the other subsystem would silently compare
    /// vectors from the wrong space. `runtime` is shared — it is the same
    /// interpreter running the same helper.
    pub preselect_model_path: Option<PathBuf>,
    pub preselect_revision: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWorkflow {
    enabled: Option<bool>,
    mode: Option<crate::workflow::WorkflowMode>,
    initial_phase: Option<crate::workflow::Phase>,
    always: Option<Vec<String>>,
    phase_skills: Option<std::collections::BTreeMap<crate::workflow::Phase, Vec<String>>>,
}

impl RawWorkflow {
    fn apply_to(self, merged: &mut crate::workflow::WorkflowConfig) {
        if let Some(enabled) = self.enabled {
            merged.enabled = enabled;
        }
        if let Some(mode) = self.mode {
            merged.mode = mode;
        }
        if let Some(phase) = self.initial_phase {
            merged.initial_phase = phase;
        }
        if let Some(always) = self.always {
            merged.always = always;
        }
        if let Some(phases) = self.phase_skills {
            // An explicit empty map clears all inherited stage bindings.
            // Nonempty maps override their named phases only, including [].
            if phases.is_empty() {
                merged.phase_skills.clear();
            } else {
                merged.phase_skills.extend(phases);
            }
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFilesMd {
    /// `None` inherits the prior stage; the default remains enabled.
    auto_regenerate: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct RawSkills {
    /// `None` means the key itself is absent (= inherit the prior stage's
    /// value), and `Some(vec![])` means `paths = []` was given explicitly
    /// (= override the prior stage with an empty list). Leaving this as a
    /// plain `Vec` would make the two indistinguishable, taking away the
    /// project's ability to deliberately empty out the global list.
    #[serde(default)]
    paths: Option<Vec<PathBuf>>,
}

#[derive(Debug, Default, Deserialize)]
struct RawAgents {
    /// Same `None` vs `Some(vec![])` distinction as `RawSkills::paths`.
    #[serde(default)]
    paths: Option<Vec<PathBuf>>,
}

/// What each declared weight is worth in models. Absent tables and absent
/// keys both mean "inherit", so a project file can set `[tier.dwarf] model`
/// without restating the global file's `effort`.
/// `max_bytes` is written the way a person says it: `"80GiB"`, `"500MB"`,
/// or a bare number of bytes. A unit nobody uses for disk is refused rather
/// than guessed — being wrong about the unit here deletes the wrong amount.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBuildCache {
    #[serde(default)]
    max_bytes: Option<String>,
    #[serde(default)]
    max_percent: Option<u8>,
    /// Which directory holds this project's build output. Relative paths are
    /// read against the project root. Unset means `target`.
    #[serde(default)]
    path: Option<PathBuf>,
}

impl RawBuildCache {
    fn apply_to(self, merged: &mut crate::build_cache::Cap) -> Result<(), ConfigError> {
        if let Some(written) = self.max_bytes {
            merged.bytes = Some(parse_size(&written).ok_or(ConfigError::BuildCacheSize)?);
        }
        if let Some(percent) = self.max_percent {
            if !(1..=100).contains(&percent) {
                return Err(ConfigError::BuildCacheShare);
            }
            merged.percent = Some(percent);
        }
        Ok(())
    }
}

/// `"80GiB"` and the rest. Binary units only where they are written as such;
/// `GB` is the decimal one, because that is what the person who wrote it
/// meant and silently treating it as 2^30 overstates the cap by 7%.
fn parse_size(written: &str) -> Option<u64> {
    let trimmed = written.trim();
    let split = trimmed
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(trimmed.len());
    let (number, unit) = trimmed.split_at(split);
    let number: f64 = number.parse().ok()?;
    if !number.is_finite() || number < 0.0 {
        return None;
    }
    let scale: u64 = match unit.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1,
        "KB" => 1_000,
        "MB" => 1_000_000,
        "GB" => 1_000_000_000,
        "TB" => 1_000_000_000_000,
        "KIB" => 1 << 10,
        "MIB" => 1 << 20,
        "GIB" => 1 << 30,
        "TIB" => 1u64 << 40,
        _ => return None,
    };
    let bytes = number * scale as f64;
    (bytes.is_finite() && bytes >= 0.0 && bytes <= u64::MAX as f64).then_some(bytes as u64)
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDecision {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    backend: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    helper: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSidechat {
    #[serde(default)]
    enabled: Option<bool>,
}

impl RawDecision {
    fn apply_to(self, merged: &mut DecisionConfig) -> Result<(), ConfigError> {
        if let Some(enabled) = self.enabled {
            merged.enabled = enabled;
        }
        if let Some(backend) = self.backend {
            merged.backend = match backend.trim() {
                "jev" => DecisionBackend::Jev,
                "semif" => DecisionBackend::SemIf,
                _ => return Err(ConfigError::UnknownDecisionBackend(backend)),
            };
        }
        for (value, held) in [
            (self.base_url, &mut merged.base_url),
            (self.model, &mut merged.model),
        ] {
            let Some(value) = value else { continue };
            if value.trim().is_empty() {
                return Err(ConfigError::EmptyDecisionValue);
            }
            *held = Some(value);
        }
        if let Some(helper) = self.helper {
            if helper.trim().is_empty() {
                return Err(ConfigError::EmptyDecisionValue);
            }
            let helper = PathBuf::from(helper);
            if !helper.is_absolute() {
                return Err(ConfigError::RelativeDecisionHelper(helper));
            }
            merged.helper = Some(helper);
        }
        Ok(())
    }

    /// Checked after every layer has been applied, because the conflict is
    /// between the final values and not between one file's lines.
    fn settled(merged: &DecisionConfig) -> Result<(), ConfigError> {
        match (merged.backend, merged.helper.is_some()) {
            // A half-written pin: the backend that needs a helper, without
            // one. Refused rather than started and found wanting later.
            (DecisionBackend::SemIf, false) => Err(ConfigError::MissingDecisionHelper),
            // A helper under the remote backend names a local program this
            // run will never start. Ambiguous, so it stops.
            (DecisionBackend::Jev, true) => Err(ConfigError::UnusedDecisionHelper),
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTier {
    #[serde(default)]
    dwarf: Option<RawTierSettings>,
    #[serde(default)]
    subgiant: Option<RawTierSettings>,
    #[serde(default)]
    giant: Option<RawTierSettings>,
    #[serde(default)]
    supergiant: Option<RawTierSettings>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTierSettings {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
}

impl RawTier {
    fn apply_to(self, merged: &mut TierConfig) -> Result<(), ConfigError> {
        for (raw, settings) in [
            (self.dwarf, &mut merged.dwarf),
            (self.subgiant, &mut merged.subgiant),
            (self.giant, &mut merged.giant),
            (self.supergiant, &mut merged.supergiant),
        ] {
            let Some(raw) = raw else { continue };
            for (value, held) in [
                (raw.model, &mut settings.model),
                (raw.effort, &mut settings.effort),
            ] {
                let Some(value) = value else { continue };
                // A blank is a typo, not a choice: it would be carried all
                // the way to the provider and refused there, or worse,
                // accepted as a model named "". Whether the name is one
                // the backend knows is settled when the provider is built,
                // where the backend is known; here only emptiness is.
                if value.trim().is_empty() {
                    return Err(ConfigError::EmptyTierValue);
                }
                *held = Some(value);
            }
        }
        Ok(())
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawSpawn {
    /// `None` here means "inherit the prior stage's value" — unlike
    /// `RawSkills`/`RawAgents::paths`, there is no meaningful "override
    /// with an explicit empty" state for a single number, so this stays a
    /// plain `Option<usize>`.
    #[serde(default)]
    concurrency: Option<usize>,
    #[serde(default)]
    write_concurrency: Option<usize>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("cannot parse TOML in {path}: {source}")]
    Parse {
        path: String,
        source: toml::de::Error,
    },
    #[error("[tier] のmodelとeffortに空の値は書けません")]
    EmptyTierValue,
    #[error("[decision] backend は jev か semif のどちらかです（{0} は不明）")]
    UnknownDecisionBackend(String),
    #[error("[decision] backend = \"semif\" には helper への絶対pathが必要です")]
    MissingDecisionHelper,
    #[error("[decision] helper は backend = \"semif\" のときだけ指定できます")]
    UnusedDecisionHelper,
    #[error("[decision] helper は絶対pathで書いてください（{0} は相対）")]
    RelativeDecisionHelper(PathBuf),
    #[error("[decision] のbase_urlとmodelに空の値は書けません")]
    EmptyDecisionValue,
    #[error("[build_cache] のmax_bytesを読めません。80GiB・500MB・バイト数のいずれかで書きます")]
    BuildCacheSize,
    #[error("[build_cache] のmax_percentは1から100までです")]
    BuildCacheShare,
    #[error("[llama_cpp] には server・models_dir・build の3つすべてが要ります")]
    IncompleteLlamaCpp,
    #[error(
        "[presentation] と [answer_finalizer] はOSユーザーの ~/.polaris/config.toml にだけ保存できます"
    )]
    UserOnlyFinalizerSetting,
    #[error("answer_finalizer.mode = \"shadow\" または \"enforce\" には judge_profile が必要です")]
    MissingFinalizerJudgeProfile,
    #[error("answer_finalizer.translation.enabled = true には固定profileが必要です")]
    MissingTranslationProfile,
    #[error("managed-preferences.toml の answer_finalizer.mode は off か shadow だけです")]
    ManagedFinalizerMode,
    #[error("answer_finalizer.external_disclosure = true は shadow モードでのみ指定できます")]
    ExternalFinalizerDisclosureMode,
    #[error(
        "managed-preferences.toml の translation は enabled=true と固定CAT profileを同時に指定します"
    )]
    ManagedTranslation,
}

fn read_one<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, ConfigError> {
    let body = match std::fs::read_to_string(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(ConfigError::Io {
                path: path.display().to_string(),
                source: e,
            });
        }
    };
    toml::from_str(&body)
        .map(Some)
        .map_err(|e| ConfigError::Parse {
            path: path.display().to_string(),
            source: e,
        })
}

/// Reads the app-owned preferences file without following aliases. The file
/// is an input to the production answer path, so an unexpected object or an
/// unbounded document is a startup error rather than an absent preference.
fn read_managed_preferences<T: serde::de::DeserializeOwned>(
    path: &Path,
) -> Result<Option<T>, ConfigError> {
    const MAX_BYTES: u64 = 64 * 1024;
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ConfigError::Io {
                path: path.display().to_string(),
                source,
            });
        }
    };
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_file()
        || metadata.len() > MAX_BYTES
    {
        return Err(ConfigError::Io {
            path: path.display().to_string(),
            source: std::io::Error::other("managed preferences must be a bounded regular file"),
        });
    }
    read_one(path)
}

/// Reads the global and project configs, letting the latter override the
/// former. Not existing is not a failure. Being unreadable or malformed is.
///
/// A file missing the `skills.paths` key inherits the prior stage's value
/// as-is. A file that explicitly gives `paths = []` overrides the prior
/// stage with an empty list — there needs to be a way to declare that when
/// personal skills should not be carried into a given project.
pub fn try_load_from(global: Option<&Path>, project: Option<&Path>) -> Result<Config, ConfigError> {
    try_load_layers(&[global, project])
}

fn try_load_layers(paths: &[Option<&Path>]) -> Result<Config, ConfigError> {
    let mut merged = Config::default();
    let user_index = paths.len().checked_sub(2);
    for (index, path) in paths.iter().copied().enumerate() {
        let Some(path) = path else { continue };
        if let Some(raw) = read_one::<RawConfig>(path)? {
            let has_user_only = raw.presentation.is_some() || raw.answer_finalizer.is_some();
            if has_user_only && Some(index) != user_index {
                return Err(ConfigError::UserOnlyFinalizerSetting);
            }
            if Some(index) == user_index {
                if let Some(presentation) = raw.presentation
                    && let Some(language) = presentation.language
                {
                    merged.presentation.language = language.into();
                }
                if let Some(answer_finalizer) = raw.answer_finalizer {
                    answer_finalizer.apply_to(&mut merged.answer_finalizer);
                }
            }
            if let Some(mode) = raw.history_mode {
                merged.history_mode = mode;
            }
            if let Some(mode) = raw.input_preparation {
                merged.input_preparation = mode;
            }
            if raw.embedding.runtime.is_some() {
                merged.embedding.runtime = raw.embedding.runtime;
            }
            if raw.embedding.model_path.is_some() {
                merged.embedding.model_path = raw.embedding.model_path;
            }
            if raw.embedding.preselect_model_path.is_some() {
                merged.embedding.preselect_model_path = raw.embedding.preselect_model_path;
            }
            if raw.embedding.preselect_revision.is_some() {
                merged.embedding.preselect_revision = raw.embedding.preselect_revision;
            }
            if raw.embedding.revision.is_some() {
                merged.embedding.revision = raw.embedding.revision;
            }
            raw.workflow.apply_to(&mut merged.workflow);
            if let Some(auto_regenerate) = raw.files_md.auto_regenerate {
                merged.files_md_auto_regenerate = auto_regenerate;
            }
            if let Some(paths) = raw.skills.paths {
                merged.skills_paths = paths;
            }
            if let Some(paths) = raw.agents.paths {
                merged.agents_paths = paths;
            }
            // Floored to 1: `spawn::run_wave` builds a `Semaphore::new(n)`
            // from these directly, and a semaphore with 0 permits means
            // every task in the wave parks on `acquire()` forever — a
            // silent hang, not a config error a user would ever see.
            // Flooring rather than rejecting keeps a config file with
            // `concurrency = 0` usable (as "as serialized as this build
            // allows") instead of turning it into a hard failure at
            // startup.
            if let Some(n) = raw.spawn.concurrency {
                merged.spawn_concurrency = n.max(1);
            }
            if let Some(n) = raw.spawn.write_concurrency {
                merged.spawn_write_concurrency = n.max(1);
            }
            raw.tier.apply_to(&mut merged.tiers)?;
            let path = raw.build_cache.path.clone();
            raw.build_cache.apply_to(&mut merged.build_cache)?;
            if path.is_some() {
                merged.build_cache_path = path;
            }
            if let Some(auto) = raw.approval.auto {
                merged.approval_auto = auto;
            }
            if let Some(llama_cpp) = raw.llama_cpp.resolve()? {
                merged.llama_cpp = Some(llama_cpp);
            }
            raw.decision.apply_to(&mut merged.decision)?;
            if let Some(enabled) = raw.sidechat.enabled {
                merged.sidechat_auto = enabled;
            }
        }
    }
    RawDecision::settled(&merged.decision)?;
    RawAnswerFinalizer::settled(&merged.answer_finalizer)?;
    Ok(merged)
}

/// Reads only workflow configuration. Other feature settings belong to their
/// own loaders, so disabled memory cannot prevent a legacy owner from starting.
/// Malformed TOML and invalid workflow fields still fail closed.
pub fn try_load_workflow_from(
    global: Option<&Path>,
    project: Option<&Path>,
) -> Result<crate::workflow::WorkflowConfig, ConfigError> {
    #[derive(Deserialize)]
    struct WorkflowOnly {
        #[serde(default)]
        workflow: RawWorkflow,
    }
    let mut merged = Config::default().workflow;
    for path in [global, project].into_iter().flatten() {
        if let Some(raw) = read_one::<WorkflowOnly>(path)? {
            raw.workflow.apply_to(&mut merged);
        }
    }
    Ok(merged)
}

/// Reads only input preparation so desktop startup can freeze the same
/// top-level setting without coupling it to optional workflow state.
pub fn try_load_input_preparation_from(
    global: Option<&Path>,
    project: Option<&Path>,
) -> Result<crate::input_preparation::InputPreparationMode, ConfigError> {
    #[derive(Deserialize)]
    struct InputPreparationOnly {
        input_preparation: Option<crate::input_preparation::InputPreparationMode>,
    }
    let mut mode = crate::input_preparation::InputPreparationMode::Off;
    for path in [global, project].into_iter().flatten() {
        if let Some(raw) = read_one::<InputPreparationOnly>(path)?
            && let Some(written) = raw.input_preparation
        {
            mode = written;
        }
    }
    Ok(mode)
}

/// Reads only the judge's configuration, so an owner that cannot start
/// feature X does not lose the judge with it — the same reason
/// `try_load_workflow_from` exists.
pub fn try_load_decision_from(
    global: Option<&Path>,
    project: Option<&Path>,
) -> Result<DecisionConfig, ConfigError> {
    #[derive(Deserialize)]
    struct DecisionOnly {
        #[serde(default)]
        decision: RawDecision,
    }
    let mut merged = Config::default().decision;
    for path in [global, project].into_iter().flatten() {
        if let Some(raw) = read_one::<DecisionOnly>(path)? {
            raw.decision.apply_to(&mut merged)?;
        }
    }
    RawDecision::settled(&merged)?;
    Ok(merged)
}

/// Reads only the user-owned presentation/finalizer settings. Project files
/// are inspected solely to reject an attempted override; their values are
/// never merged into the returned turn policy.
pub fn try_load_answer_finalizer_from(
    user: Option<&Path>,
    project: Option<&Path>,
) -> Result<(PresentationConfig, AnswerFinalizerConfig), ConfigError> {
    try_load_answer_finalizer_with_managed_from(None, user, project)
}

/// Reads the app-generated preferences first, then lets the explicit user
/// config override individual values. A checkout is never allowed to set
/// presentation or finalizer policy.
pub fn try_load_answer_finalizer_with_managed_from(
    managed: Option<&Path>,
    user: Option<&Path>,
    project: Option<&Path>,
) -> Result<(PresentationConfig, AnswerFinalizerConfig), ConfigError> {
    #[derive(Default, Deserialize)]
    struct FinalizerOnly {
        #[serde(default)]
        presentation: Option<RawPresentation>,
        #[serde(default)]
        answer_finalizer: Option<RawAnswerFinalizer>,
    }
    #[derive(Default, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ManagedFinalizerOnly {
        #[serde(default)]
        presentation: Option<RawPresentation>,
        #[serde(default)]
        answer_finalizer: Option<RawAnswerFinalizer>,
    }

    fn apply(
        raw_presentation: Option<RawPresentation>,
        raw_finalizer: Option<RawAnswerFinalizer>,
        presentation: &mut PresentationConfig,
        answer_finalizer: &mut AnswerFinalizerConfig,
    ) {
        if let Some(raw) = raw_presentation
            && let Some(language) = raw.language
        {
            presentation.language = language.into();
        }
        if let Some(raw) = raw_finalizer {
            raw.apply_to(answer_finalizer);
        }
    }

    let mut presentation = PresentationConfig::default();
    let mut answer_finalizer = AnswerFinalizerConfig::default();
    if let Some(path) = managed
        && let Some(raw) = read_managed_preferences::<ManagedFinalizerOnly>(path)?
    {
        if raw
            .answer_finalizer
            .as_ref()
            .and_then(|finalizer| finalizer.mode)
            == Some(AnswerFinalizerMode::Enforce)
        {
            return Err(ConfigError::ManagedFinalizerMode);
        }
        if let Some(translation) = raw
            .answer_finalizer
            .as_ref()
            .and_then(|finalizer| finalizer.translation.as_ref())
            && (translation.enabled != Some(true)
                || translation.profile != Some(TranslationProfile::CatTranslate3_3b))
        {
            return Err(ConfigError::ManagedTranslation);
        }
        apply(
            raw.presentation,
            raw.answer_finalizer,
            &mut presentation,
            &mut answer_finalizer,
        );
    }
    if let Some(path) = user
        && let Some(raw) = read_one::<FinalizerOnly>(path)?
    {
        apply(
            raw.presentation,
            raw.answer_finalizer,
            &mut presentation,
            &mut answer_finalizer,
        );
    }
    if let Some(path) = project
        && let Some(raw) = read_one::<FinalizerOnly>(path)?
        && (raw.presentation.is_some() || raw.answer_finalizer.is_some())
    {
        return Err(ConfigError::UserOnlyFinalizerSetting);
    }
    RawAnswerFinalizer::settled(&answer_finalizer)?;
    Ok((presentation, answer_finalizer))
}

/// Resolves the installer-managed resource pins, then the user and project
/// configs. Explicit user configuration remains authoritative.
pub fn load(project_root: &Path) -> Result<Config, ConfigError> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let managed = home
        .as_ref()
        .map(|h| h.join(".polaris").join("managed-resources.toml"));
    let global = home
        .as_ref()
        .map(|h| h.join(".polaris").join("config.toml"));
    let project = project_root.join(".polaris").join("config.toml");
    try_load_layers(&[managed.as_deref(), global.as_deref(), Some(&project)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finalizer_preferences_default_off_and_legacy_config_stays_compatible() {
        let directory = tempfile::tempdir().unwrap();
        let global = write(directory.path(), "[workflow]\nenabled = false\n");
        let config = try_load_from(Some(&global), None).unwrap();

        assert_eq!(
            config.presentation.language,
            PresentationLanguage::Unspecified
        );
        assert_eq!(config.answer_finalizer, AnswerFinalizerConfig::default());
        assert_eq!(
            config.answer_finalizer.activation_requirement(),
            FinalizerActivationRequirement::None
        );
    }

    #[test]
    fn user_finalizer_preferences_decode_to_named_profiles_and_managed_path() {
        let directory = tempfile::tempdir().unwrap();
        let global = write(
            directory.path(),
            "[presentation]\nlanguage = 'ja'\n\
             [answer_finalizer]\nmode = 'enforce'\njudge_profile = 'fulfillment-v1'\n\
             [answer_finalizer.translation]\nenabled = true\nprofile = 'cat-translate-3.3b'\n",
        );
        let config = try_load_from(Some(&global), None).unwrap();

        assert_eq!(config.presentation.language, PresentationLanguage::Ja);
        assert_eq!(config.answer_finalizer.mode, AnswerFinalizerMode::Enforce);
        assert_eq!(
            config.answer_finalizer.judge_profile,
            Some(JudgeProfile::FulfillmentV1)
        );
        assert_eq!(
            config.answer_finalizer.translation.activation,
            TranslationActivation::Requested
        );
        let profile = config.answer_finalizer.translation.profile.unwrap();
        assert_eq!(profile.id(), "cat-translate-3.3b");
        assert_eq!(
            profile.managed_model_path(Path::new("/Users/example")),
            PathBuf::from("/Users/example/.polaris/models/cat-translate-3.3b")
                .join(crate::translation::CAT_TRANSLATE_REVISION)
        );
        assert_eq!(
            config.answer_finalizer.activation_requirement(),
            FinalizerActivationRequirement::CompositeBenchmark
        );
    }

    #[test]
    fn project_cannot_set_or_override_user_finalizer_preferences() {
        let user = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let global = write(
            user.path(),
            "[presentation]\nlanguage = 'ja'\n[answer_finalizer]\nmode = 'off'\n",
        );

        for body in [
            "[presentation]\nlanguage = 'en'\n",
            "[answer_finalizer]\nmode = 'shadow'\njudge_profile = 'fulfillment-v1'\n",
            "[answer_finalizer.translation]\nprofile = 'cat-translate-3.3b'\n",
        ] {
            let project_path = write(project.path(), body);
            assert!(matches!(
                try_load_from(Some(&global), Some(&project_path)),
                Err(ConfigError::UserOnlyFinalizerSetting)
            ));
            assert!(matches!(
                try_load_answer_finalizer_from(Some(&global), Some(&project_path)),
                Err(ConfigError::UserOnlyFinalizerSetting)
            ));
        }
    }

    #[test]
    fn managed_finalizer_preferences_are_defaults_below_explicit_user_config() {
        let directory = tempfile::tempdir().unwrap();
        let managed = directory.path().join("managed-preferences.toml");
        std::fs::write(
            &managed,
            "[presentation]\nlanguage='ja'\n[answer_finalizer]\nmode='shadow'\njudge_profile='fulfillment-v1'\n[answer_finalizer.translation]\nenabled=true\nprofile='cat-translate-3.3b'\n",
        )
        .unwrap();
        let user = directory.path().join("config.toml");
        std::fs::write(
            &user,
            "[presentation]\nlanguage='en'\n[answer_finalizer]\nmode='off'\n[answer_finalizer.translation]\nenabled=false\n",
        )
        .unwrap();
        let (presentation, finalizer) =
            try_load_answer_finalizer_with_managed_from(Some(&managed), Some(&user), None).unwrap();
        assert_eq!(presentation.language, PresentationLanguage::En);
        assert_eq!(finalizer.mode, AnswerFinalizerMode::Off);
        assert_eq!(
            finalizer.translation.activation,
            TranslationActivation::Disabled
        );
    }

    #[test]
    fn managed_finalizer_preferences_fail_closed_on_unsafe_or_invalid_files() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let managed = directory.path().join("managed-preferences.toml");
        let target = directory.path().join("target.toml");
        std::fs::write(&target, "[presentation]\nlanguage='ja'\n").unwrap();
        symlink(&target, &managed).unwrap();
        assert!(try_load_answer_finalizer_with_managed_from(Some(&managed), None, None).is_err());

        std::fs::remove_file(&managed).unwrap();
        std::fs::create_dir(&managed).unwrap();
        assert!(try_load_answer_finalizer_with_managed_from(Some(&managed), None, None).is_err());

        std::fs::remove_dir(&managed).unwrap();
        std::fs::write(&managed, vec![b'x'; 64 * 1024 + 1]).unwrap();
        assert!(try_load_answer_finalizer_with_managed_from(Some(&managed), None, None).is_err());

        std::fs::write(&managed, "[presentation]\nlanguage='ja'\nunknown=true\n").unwrap();
        assert!(try_load_answer_finalizer_with_managed_from(Some(&managed), None, None).is_err());

        for body in [
            "[answer_finalizer]\nmode='enforce'\njudge_profile='fulfillment-v1'\n",
            "[answer_finalizer.translation]\nenabled=false\nprofile='cat-translate-3.3b'\n",
            "[answer_finalizer.translation]\nenabled=true\n",
        ] {
            std::fs::write(&managed, body).unwrap();
            assert!(
                try_load_answer_finalizer_with_managed_from(Some(&managed), None, None).is_err(),
                "{body}"
            );
        }
    }

    #[test]
    fn finalizer_config_rejects_unknown_or_incomplete_activation() {
        let directory = tempfile::tempdir().unwrap();
        for body in [
            "[presentation]\nlanguage = 'auto'\n",
            "[presentation]\nlanguage = 'unspecified'\n",
            "[answer_finalizer]\nmode = 'observe'\n",
            "[answer_finalizer]\nmode = 'shadow'\n",
            "[answer_finalizer]\nmode = 'off'\nexternal_disclosure = true\n",
            "[answer_finalizer]\nmode = 'enforce'\njudge_profile = 'fulfillment-v1'\nexternal_disclosure = true\n",
            "[answer_finalizer]\nmode = 'off'\njudge_profile = 'fulfillment-v2'\n",
            "[answer_finalizer.translation]\nenabled = true\n",
            "[answer_finalizer.translation]\nprofile = 'project-model'\n",
            "[answer_finalizer.translation]\nprofile = 'cat-translate-3.3b'\npath = './weights'\n",
        ] {
            let global = write(directory.path(), body);
            assert!(try_load_from(Some(&global), None).is_err(), "{body}");
        }
    }

    #[test]
    fn external_finalizer_disclosure_is_user_only_shadow_opt_in() {
        let directory = tempfile::tempdir().unwrap();
        let user = write(
            directory.path(),
            "[answer_finalizer]\nmode = 'shadow'\njudge_profile = 'fulfillment-v1'\nexternal_disclosure = true\n",
        );
        let (_, finalizer) = try_load_answer_finalizer_from(Some(&user), None).unwrap();
        assert!(finalizer.external_disclosure);

        let project = tempfile::tempdir().unwrap();
        let project_path = write(
            project.path(),
            "[answer_finalizer]\nmode = 'shadow'\njudge_profile = 'fulfillment-v1'\nexternal_disclosure = true\n",
        );
        assert!(matches!(
            try_load_answer_finalizer_from(Some(&user), Some(&project_path)),
            Err(ConfigError::UserOnlyFinalizerSetting)
        ));
    }

    #[test]
    fn enforce_is_a_request_that_retains_a_typed_benchmark_requirement() {
        let mut config = AnswerFinalizerConfig {
            mode: AnswerFinalizerMode::Enforce,
            judge_profile: Some(JudgeProfile::FulfillmentV1),
            ..AnswerFinalizerConfig::default()
        };
        assert!(config.mode.requires_benchmark_lock());
        assert_eq!(
            config.activation_requirement(),
            FinalizerActivationRequirement::JudgeBenchmark
        );
        config.translation = TranslationConfig {
            activation: TranslationActivation::Requested,
            profile: Some(TranslationProfile::CatTranslate3_3b),
        };
        assert_eq!(
            config.activation_requirement(),
            FinalizerActivationRequirement::CompositeBenchmark
        );
    }

    #[test]
    fn managed_resource_pins_are_defaults_below_user_and_project_config() {
        let directory = tempfile::tempdir().unwrap();
        let managed = write(
            directory.path(),
            "[embedding]\nruntime='/managed/python'\nmodel_path='/managed/small'\nrevision='small-fixed'\npreselect_model_path='/managed/base'\npreselect_revision='base-fixed'\n",
        );
        let global = directory.path().join("global.toml");
        std::fs::write(&global, "[embedding]\nruntime='/user/python'\n").unwrap();
        let project = directory.path().join("project.toml");
        std::fs::write(&project, "[embedding]\nrevision='project-small'\n").unwrap();

        let config = try_load_layers(&[Some(&managed), Some(&global), Some(&project)]).unwrap();
        assert_eq!(
            config.embedding.runtime,
            Some(PathBuf::from("/user/python"))
        );
        assert_eq!(
            config.embedding.model_path,
            Some(PathBuf::from("/managed/small"))
        );
        assert_eq!(config.embedding.revision.as_deref(), Some("project-small"));
        assert_eq!(
            config.embedding.preselect_model_path,
            Some(PathBuf::from("/managed/base"))
        );
        assert_eq!(
            config.embedding.preselect_revision.as_deref(),
            Some("base-fixed")
        );
    }

    /// `is_empty` is what the CLI keys its "the ladder is inert" warning
    /// off, so it has to mean exactly "no weight buys anything different".
    /// A weight naming only an effort still counts: the same model told to
    /// think harder is a real rung.
    #[test]
    fn a_ladder_where_no_weight_names_anything_is_reported_as_empty() {
        use crate::config::TierSettings;
        assert!(
            TierConfig::default().is_empty(),
            "a bare default is not reported as empty"
        );

        let effort_only = TierConfig {
            subgiant: TierSettings {
                model: None,
                effort: Some("medium".into()),
            },
            ..TierConfig::default()
        };
        assert!(
            !effort_only.is_empty(),
            "an effort-only rung was treated as nothing"
        );

        let model_only = TierConfig {
            supergiant: TierSettings {
                model: Some("sol".into()),
                effort: None,
            },
            ..TierConfig::default()
        };
        assert!(!model_only.is_empty());

        // The default really does leave every weight on the parent, which
        // is the condition the warning exists for.
        for tier in polaris_skills::Tier::ALL {
            let settings = TierConfig::default().for_tier(tier).clone();
            assert_eq!(settings, TierSettings::default(), "{tier}");
        }
    }

    /// The only setting that widens what runs without a person answering.
    /// It has to be written down, and a value nobody defined has to be
    /// rejected rather than rounded to the nearest one.
    #[test]
    fn automatic_approval_is_off_unless_the_project_wrote_a_level_down() {
        use crate::approval::AutoApproval;
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        let load = |body: &str| {
            std::fs::write(&path, body).expect("write");
            try_load_from(None, Some(&path))
        };

        assert_eq!(
            try_load_from(None, None).unwrap().approval_auto,
            AutoApproval::Off
        );
        assert_eq!(load("").unwrap().approval_auto, AutoApproval::Off);
        assert_eq!(
            load("[approval]\n").unwrap().approval_auto,
            AutoApproval::Off
        );
        for (body, expected) in [
            ("[approval]\nauto = \"off\"\n", AutoApproval::Off),
            ("[approval]\nauto = \"low\"\n", AutoApproval::Low),
            ("[approval]\nauto = \"medium\"\n", AutoApproval::Medium),
        ] {
            assert_eq!(load(body).unwrap().approval_auto, expected, "{body}");
        }
        // `high` would mean "stop asking about anything", which is not a
        // level anyone may set here; the other spellings are typos that
        // must not silently become `off`.
        for refused in [
            "[approval]\nauto = \"high\"\n",
            "[approval]\nauto = \"Low\"\n",
            "[approval]\nauto = \"all\"\n",
            "[approval]\nauto = true\n",
            "[approval]\nautomatic = \"low\"\n",
        ] {
            assert!(load(refused).is_err(), "{refused}");
        }
    }

    #[test]
    fn input_preparation_is_off_by_default_and_project_value_overrides() {
        use crate::input_preparation::InputPreparationMode;
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        assert_eq!(
            try_load_from(None, None).unwrap().input_preparation,
            InputPreparationMode::Off
        );
        std::fs::write(&path, "input_preparation = \"repeat_refs\"\n").unwrap();
        assert_eq!(
            try_load_from(None, Some(&path)).unwrap().input_preparation,
            InputPreparationMode::RepeatRefs
        );
        let project = tempfile::tempdir().unwrap();
        let project_path = write(project.path(), "input_preparation = 'off'\n");
        assert_eq!(
            try_load_from(Some(&path), Some(&project_path))
                .unwrap()
                .input_preparation,
            InputPreparationMode::Off
        );
        assert_eq!(
            try_load_input_preparation_from(Some(&path), Some(&project_path)).unwrap(),
            InputPreparationMode::Off
        );
        std::fs::write(&path, "input_preparation = \"unknown\"\n").unwrap();
        assert!(try_load_from(None, Some(&path)).is_err());
    }

    #[test]
    fn input_preparation_only_loader_ignores_unrelated_configuration_fields() {
        use crate::input_preparation::InputPreparationMode;
        let directory = tempfile::tempdir().unwrap();
        let path = write(
            directory.path(),
            "input_preparation = 'repeat_refs'\n[workflow]\nunknown_option = true\n[embedding]\nprovider = 42\n",
        );
        assert_eq!(
            try_load_input_preparation_from(None, Some(&path)).unwrap(),
            InputPreparationMode::RepeatRefs
        );
        let path = write(directory.path(), "input_preparation = 'unknown'\n");
        assert!(try_load_input_preparation_from(None, Some(&path)).is_err());
    }

    /// Nothing starts unless all three are written down. A default path
    /// or a partial fill-in here would execute a binary nobody named.
    #[test]
    fn the_bundled_runtime_is_absent_unless_all_three_fields_are_pinned() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        let load = |body: &str| {
            std::fs::write(&path, body).expect("write");
            try_load_from(None, Some(&path))
        };

        assert!(try_load_from(None, None).unwrap().llama_cpp.is_none());
        assert!(load("").unwrap().llama_cpp.is_none());
        assert!(load("[llama_cpp]\n").unwrap().llama_cpp.is_none());

        let full = load(
            "[llama_cpp]\nserver = \"/r/llama-server\"\nmodels_dir = \"/r/models\"\nbuild = \"b11026-x\"\n",
        )
        .unwrap()
        .llama_cpp
        .expect("a complete pin");
        assert_eq!(full.server, std::path::PathBuf::from("/r/llama-server"));
        assert_eq!(full.models_dir, std::path::PathBuf::from("/r/models"));
        assert_eq!(full.build, "b11026-x");

        for partial in [
            "[llama_cpp]\nserver = \"/r/llama-server\"\n",
            "[llama_cpp]\nserver = \"/r/llama-server\"\nmodels_dir = \"/r/models\"\n",
            "[llama_cpp]\nmodels_dir = \"/r/models\"\nbuild = \"b11026-x\"\n",
        ] {
            assert!(
                matches!(load(partial), Err(ConfigError::IncompleteLlamaCpp)),
                "{partial}"
            );
        }
        assert!(load("[llama_cpp]\nbinary = \"/r/llama-server\"\n").is_err());
    }

    #[test]
    fn workflow_defaults_resolve_builtin_skills_for_every_phase() {
        use crate::workflow::{
            Phase, SessionWorkflow, WORKFLOW_TOKEN_LIMIT, WorkflowGatesV1, WorkflowStateV1,
        };
        let config = try_load_from(None, None).unwrap();
        assert!(config.workflow.enabled);
        assert_eq!(
            config.history_mode,
            crate::conversation_state::HistoryMode::Legacy
        );
        assert_eq!(config.embedding, EmbeddingConfig::default());
        assert_eq!(config.workflow.initial_phase, Phase::General);
        let runtime = SessionWorkflow::restore(
            config.workflow,
            WorkflowGatesV1 {
                phase_gate_schema_version: None,
                ..WorkflowGatesV1::default()
            },
            WorkflowStateV1::default(),
        )
        .unwrap();
        for phase in Phase::ALL {
            runtime.request_phase(phase).unwrap();
            let turn = runtime.begin_turn(&[]).unwrap();
            let mut expected = vec!["builtin:workflow-core".to_string()];
            if phase != Phase::General {
                expected.push(format!("builtin:{phase}"));
            }
            assert_eq!(turn.ids, expected);
            assert!(turn.tokens <= WORKFLOW_TOKEN_LIMIT);
        }
    }

    #[test]
    fn workflow_opt_out_is_inherited_and_can_be_overridden() {
        let global = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let gp = write(global.path(), "[workflow]\nenabled = false\n");
        let pp = write(project.path(), "[workflow]\ninitial_phase = 'review'\n");
        assert!(
            !try_load_from(Some(&gp), Some(&pp))
                .unwrap()
                .workflow
                .enabled
        );
        let pp = write(project.path(), "[workflow]\nenabled = true\n");
        assert!(
            try_load_from(Some(&gp), Some(&pp))
                .unwrap()
                .workflow
                .enabled
        );
        let pp = write(project.path(), "[workflow]\nenabled = false\n");
        assert!(!try_load_from(None, Some(&pp)).unwrap().workflow.enabled);
        let pp = write(
            project.path(),
            "[workflow]\nalways = []\nphase_skills = {}\n",
        );
        let cleared = try_load_from(None, Some(&pp)).unwrap();
        assert!(cleared.workflow.enabled);
        assert!(cleared.workflow.always.is_empty());
        assert!(cleared.workflow.phase_skills.is_empty());
    }

    #[test]
    fn workflow_mode_defaults_to_manual_and_inherits_independently() {
        use crate::workflow::WorkflowMode;
        let global = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let gp = write(global.path(), "[workflow]\nmode = 'adaptive'\n");
        let pp = write(project.path(), "[workflow]\ninitial_phase = 'review'\n");
        let inherited = try_load_from(Some(&gp), Some(&pp)).unwrap();
        assert_eq!(inherited.workflow.mode, WorkflowMode::Adaptive);
        let pp = write(project.path(), "[workflow]\nmode = 'manual'\n");
        assert_eq!(
            try_load_from(Some(&gp), Some(&pp)).unwrap().workflow.mode,
            WorkflowMode::Manual
        );
        assert_eq!(Config::default().workflow.mode, WorkflowMode::Manual);
    }

    #[test]
    fn workflow_inherits_missing_keys_and_clears_explicit_empty_values() {
        use crate::workflow::Phase;
        let global = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let gp = write(
            global.path(),
            "[workflow]\nenabled = true\ninitial_phase = 'implement'\nalways = ['core']\n[workflow.phase_skills]\nreview = ['reviewer']\nverify = ['tests']\n",
        );
        let pp = write(
            project.path(),
            "[workflow]\nalways = []\n[workflow.phase_skills]\nreview = []\n",
        );
        let config = try_load_from(Some(&gp), Some(&pp)).unwrap();
        assert!(config.workflow.enabled);
        assert_eq!(config.workflow.initial_phase, Phase::Implement);
        assert!(config.workflow.always.is_empty());
        assert!(config.workflow.phase_skills[&Phase::Review].is_empty());
        assert_eq!(config.workflow.phase_skills[&Phase::Verify], vec!["tests"]);
        let pp = write(
            project.path(),
            "[workflow]\nenabled = false\nphase_skills = {}\n",
        );
        let config = try_load_from(Some(&gp), Some(&pp)).unwrap();
        assert!(!config.workflow.enabled);
        assert!(config.workflow.phase_skills.is_empty());
        assert_eq!(config.workflow.always, vec!["core"]);
    }

    #[test]
    fn workflow_rejects_unknown_phase_or_misspelled_fields() {
        let dir = tempfile::tempdir().unwrap();
        for body in [
            "[workflow]\ninitial_phase = 'auto'",
            "[workflow]\nenabeld = true",
            "[workflow.phase_skills]\nauto = []",
        ] {
            let path = write(dir.path(), body);
            assert!(try_load_from(None, Some(&path)).is_err());
        }
    }

    /// The default is off: the maps it writes have no reader, so the only
    /// thing left on by default was a subagent run per changed directory.
    /// Asking for it still works, and a project still overrides the global
    /// file in both directions.
    #[test]
    fn files_md_auto_regenerate_defaults_false_and_project_overrides_global() {
        let global = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        assert!(!Config::default().files_md_auto_regenerate);

        // Silence anywhere in the chain leaves it off.
        let global_path = write(global.path(), "# nothing about files_md\n");
        let project_path = write(project.path(), "# inherit files_md\n");
        assert!(
            !try_load_from(Some(&global_path), Some(&project_path))
                .unwrap()
                .files_md_auto_regenerate
        );

        // Asked for globally, inherited by a project that says nothing.
        let global_path = write(global.path(), "[files_md]\nauto_regenerate = true\n");
        assert!(
            try_load_from(Some(&global_path), Some(&project_path))
                .unwrap()
                .files_md_auto_regenerate
        );

        // And a project turns it back off.
        let project_path = write(project.path(), "[files_md]\nauto_regenerate = false\n");
        assert!(
            !try_load_from(Some(&global_path), Some(&project_path))
                .unwrap()
                .files_md_auto_regenerate
        );
    }

    #[test]
    fn a_weight_the_project_leaves_alone_keeps_what_the_global_file_gave_it() {
        let global = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let global_path = write(
            global.path(),
            "[tier.dwarf]\nmodel = \"gpt-5.4-mini\"\neffort = \"low\"\n\n[tier.supergiant]\nmodel = \"gpt-6-astra\"\n",
        );
        // Only `low`'s model is restated, so `low`'s effort and the whole
        // of `ultra` have to survive. Restating one key must not blank the
        // rest of the weight.
        let project_path = write(project.path(), "[tier.dwarf]\nmodel = \"gpt-5.6-luna\"\n");
        let held = try_load_from(Some(&global_path), Some(&project_path)).unwrap();
        assert_eq!(held.tiers.dwarf.model.as_deref(), Some("gpt-5.6-luna"));
        assert_eq!(held.tiers.dwarf.effort.as_deref(), Some("low"));
        assert_eq!(held.tiers.supergiant.model.as_deref(), Some("gpt-6-astra"));
        assert_eq!(held.tiers.subgiant, TierSettings::default());
        assert_eq!(
            held.tiers
                .for_tier(polaris_skills::Tier::Dwarf)
                .model
                .as_deref(),
            Some("gpt-5.6-luna")
        );
        assert!(!held.tiers.is_empty());
    }

    #[test]
    fn no_tier_table_leaves_every_weight_on_the_parent_model() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "# nothing about weights\n");
        let held = try_load_from(None, Some(&path)).unwrap();
        assert!(held.tiers.is_empty());
        assert_eq!(held.tiers, Config::default().tiers);
    }

    #[test]
    fn a_blank_model_or_effort_is_refused_rather_than_carried() {
        let dir = tempfile::tempdir().unwrap();
        for body in [
            "[tier.dwarf]\nmodel = \"\"\n",
            "[tier.subgiant]\neffort = \"   \"\n",
            "[tier.giant]\nmodel = \"\"\n",
            "[tier.supergiant]\neffort = \"\"\n",
        ] {
            let path = write(dir.path(), body);
            assert!(
                matches!(
                    try_load_from(None, Some(&path)),
                    Err(ConfigError::EmptyTierValue)
                ),
                "{body}"
            );
        }
        // A weight nobody has heard of, or a key nobody has heard of, is a
        // typo in the same way — and unlike a blank it would otherwise be
        // dropped in silence.
        for body in [
            "[tier.heavy]\nmodel = \"gpt-6-astra\"\n",
            "[tier.dwarf]\nmodle = \"gpt-6-astra\"\n",
        ] {
            let path = write(dir.path(), body);
            assert!(
                matches!(
                    try_load_from(None, Some(&path)),
                    Err(ConfigError::Parse { .. })
                ),
                "{body}"
            );
        }
    }

    /// The judge is named, never inferred. Every shape that would leave
    /// "which decider answered" ambiguous is refused at load, because the
    /// alternative is a run whose evidence cannot be attributed.
    #[test]
    fn the_decision_backend_is_named_and_its_resources_must_match_it() {
        let dir = tempfile::tempdir().unwrap();
        let helper = dir.path().join("semif-helper");
        std::fs::write(&helper, "#!/bin/sh\n").unwrap();
        let helper = helper.display();

        assert_eq!(
            Config::default().decision.backend,
            DecisionBackend::Jev,
            "the default moved"
        );

        let ok = write(
            dir.path(),
            &format!("[decision]\nenabled = true\nbackend = \"semif\"\nhelper = \"{helper}\"\n"),
        );
        let held = try_load_from(None, Some(&ok)).expect("a valid semif配置");
        assert_eq!(held.decision.backend, DecisionBackend::SemIf);
        assert_eq!(
            held.decision.helper.as_deref(),
            Some(Path::new(&format!("{helper}")))
        );
        // The decision-only loader answers the same, or a host that reads
        // just this section would start a different judge.
        let only = try_load_decision_from(None, Some(&ok)).expect("a read");
        assert_eq!(only.backend, DecisionBackend::SemIf);

        for (body, expected) in [
            ("[decision]\nbackend = \"semiff\"\n".to_owned(), "unknown"),
            ("[decision]\nbackend = \"semif\"\n".to_owned(), "missing"),
            (format!("[decision]\nhelper = \"{helper}\"\n"), "unused"),
            (
                "[decision]\nbackend = \"semif\"\nhelper = \"semif-helper\"\n".to_owned(),
                "relative",
            ),
        ] {
            let path = write(dir.path(), &body);
            let error = try_load_from(None, Some(&path)).expect_err(&body);
            let matched = matches!(
                (expected, &error),
                ("unknown", ConfigError::UnknownDecisionBackend(_))
                    | ("missing", ConfigError::MissingDecisionHelper)
                    | ("unused", ConfigError::UnusedDecisionHelper)
                    | ("relative", ConfigError::RelativeDecisionHelper(_))
            );
            assert!(matched, "{body} gave {error:?}, wanted {expected}");
        }
    }

    #[test]
    fn the_judge_is_off_unless_asked_for_and_never_reads_a_key_from_the_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!Config::default().decision.enabled);
        let path = write(dir.path(), "# nothing about the judge\n");
        assert!(!try_load_from(None, Some(&path)).unwrap().decision.enabled);

        let path = write(
            dir.path(),
            "[decision]\nenabled = true\nmodel = \"jev-latest\"\n",
        );
        let held = try_load_from(None, Some(&path)).unwrap();
        assert!(held.decision.enabled);
        assert_eq!(held.decision.model.as_deref(), Some("jev-latest"));
        assert_eq!(held.decision.base_url, None, "unset falls back in the host");

        // A credential does not belong in a file that is committed and
        // shared, so the key has no name here at all.
        let path = write(
            dir.path(),
            "[decision]\nenabled = true\napi_key = \"secret\"\n",
        );
        assert!(matches!(
            try_load_from(None, Some(&path)),
            Err(ConfigError::Parse { .. })
        ));

        let path = write(dir.path(), "[decision]\nmodel = \"\"\n");
        assert!(matches!(
            try_load_from(None, Some(&path)),
            Err(ConfigError::EmptyDecisionValue)
        ));
    }

    #[test]
    fn sidechat_auto_defaults_on_and_can_be_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "[sidechat]\nenabled = false\n");
        assert!(Config::default().sidechat_auto);
        assert!(!try_load_from(None, Some(&path)).unwrap().sidechat_auto);
    }

    #[test]
    fn a_judge_only_read_ignores_everything_it_is_not_asked_about() {
        // Read on its own so an owner that cannot start one feature does
        // not lose the judge with it — the same reason the workflow-only
        // read exists. Fields belonging to other tables are not this
        // reader's to reject.
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "[workflow]\nenabled = false\n\n[decision]\nenabled = true\n",
        );
        let held = try_load_decision_from(None, Some(&path)).expect("a read");
        assert!(held.enabled);

        // The project file overrides the global one field by field, the
        // same way the whole configuration does.
        let global = write(dir.path(), "[decision]\nenabled = true\nmodel = \"a\"\n");
        let project = tempfile::tempdir().unwrap();
        let project = write(project.path(), "[decision]\nmodel = \"b\"\n");
        let held = try_load_decision_from(Some(&global), Some(&project)).expect("a read");
        assert!(held.enabled, "what the project does not restate survives");
        assert_eq!(held.model.as_deref(), Some("b"));

        // Malformed still fails here; whether that stops the caller is the
        // caller's decision, and the desktop owner chooses to continue
        // without a judge rather than without a session.
        let broken = write(dir.path(), "invalid TOML [");
        assert!(try_load_decision_from(None, Some(&broken)).is_err());
    }

    fn write(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        let p = dir.join("config.toml");
        std::fs::write(&p, body).expect("could not write");
        p
    }

    #[test]
    fn returns_empty_config_when_neither_file_exists() {
        let dir = tempfile::tempdir().expect("temp directory");
        let c = try_load_from(None, Some(&dir.path().join("missing.toml")))
            .expect("not existing is not a failure");
        assert!(c.skills_paths.is_empty());
    }

    #[test]
    fn reads_skills_paths_from_a_single_file() {
        let dir = tempfile::tempdir().expect("temp directory");
        let p = write(dir.path(), "[skills]\npaths = [\"/a\", \"/b\"]\n");
        let c = try_load_from(Some(&p), None).expect("should be readable");
        assert_eq!(
            c.skills_paths,
            vec![
                std::path::PathBuf::from("/a"),
                std::path::PathBuf::from("/b")
            ]
        );
    }

    #[test]
    fn project_overrides_global() {
        let g = tempfile::tempdir().expect("temp directory");
        let pj = tempfile::tempdir().expect("temp directory");
        let gp = write(g.path(), "[skills]\npaths = [\"/global\"]\n");
        let pp = write(pj.path(), "[skills]\npaths = [\"/project\"]\n");
        let c = try_load_from(Some(&gp), Some(&pp)).expect("should be readable");
        assert_eq!(c.skills_paths, vec![std::path::PathBuf::from("/project")]);
    }

    #[test]
    fn malformed_toml_is_reported_not_swallowed() {
        let dir = tempfile::tempdir().expect("temp directory");
        let p = write(dir.path(), "[skills\npaths = ");
        let err = try_load_from(Some(&p), None).expect_err("malformed TOML should be reported");
        assert!(
            err.to_string().contains("config.toml"),
            "path is missing: {err}"
        );
    }

    #[test]
    fn project_can_clear_global_skills_paths_with_an_empty_list() {
        // When personal skills should not be carried into a given project,
        // explicitly giving `paths = []` lets the global list be overridden
        // with an empty one.
        let g = tempfile::tempdir().expect("temp directory");
        let pj = tempfile::tempdir().expect("temp directory");
        let gp = write(g.path(), "[skills]\npaths = [\"/global\"]\n");
        let pp = write(pj.path(), "[skills]\npaths = []\n");
        let c = try_load_from(Some(&gp), Some(&pp)).expect("should be readable");
        assert!(
            c.skills_paths.is_empty(),
            "explicit empty list did not override the global one: {:?}",
            c.skills_paths
        );
    }

    #[test]
    fn agents_paths_defaults_to_empty_when_the_key_is_absent() {
        let cfg = Config::default();
        assert!(cfg.agents_paths.is_empty());
    }

    #[test]
    fn project_without_skills_section_inherits_global() {
        // A project config missing the `[skills]` section entirely should
        // "inherit", not "clear". If this isn't kept distinct in behavior
        // from an explicit empty list (the previous test), the two would
        // collapse into being identical, just as they stand today.
        let g = tempfile::tempdir().expect("temp directory");
        let pj = tempfile::tempdir().expect("temp directory");
        let gp = write(g.path(), "[skills]\npaths = [\"/global\"]\n");
        let pp = write(pj.path(), "# no skills section\n");
        let c = try_load_from(Some(&gp), Some(&pp)).expect("should be readable");
        assert_eq!(
            c.skills_paths,
            vec![std::path::PathBuf::from("/global")],
            "did not inherit global despite having no skills section: {:?}",
            c.skills_paths
        );
    }

    #[test]
    fn spawn_concurrency_defaults_to_the_documented_constants_when_absent() {
        let cfg = Config::default();
        assert_eq!(cfg.spawn_concurrency, crate::spawn::DEFAULT_CONCURRENCY);
        assert_eq!(
            cfg.spawn_write_concurrency,
            crate::spawn::DEFAULT_WRITE_CONCURRENCY
        );
    }

    #[test]
    fn reads_spawn_concurrency_from_a_single_file() {
        let dir = tempfile::tempdir().expect("temp directory");
        let p = write(
            dir.path(),
            "[spawn]\nconcurrency = 16\nwrite_concurrency = 2\n",
        );
        let c = try_load_from(Some(&p), None).expect("should be readable");
        assert_eq!(c.spawn_concurrency, 16);
        assert_eq!(c.spawn_write_concurrency, 2);
    }

    #[test]
    fn project_without_spawn_section_inherits_global_concurrency() {
        let g = tempfile::tempdir().expect("temp directory");
        let pj = tempfile::tempdir().expect("temp directory");
        let gp = write(g.path(), "[spawn]\nconcurrency = 16\n");
        let pp = write(pj.path(), "# no spawn section\n");
        let c = try_load_from(Some(&gp), Some(&pp)).expect("should be readable");
        assert_eq!(c.spawn_concurrency, 16);
    }

    #[test]
    fn a_declared_concurrency_of_zero_is_floored_to_one_not_left_as_a_deadlock() {
        // `Semaphore::new(0)` means every task in a wave parks on
        // `acquire()` forever — this has to be caught here, at config
        // load, rather than surfacing as a silent hang deep inside
        // `spawn::run_wave`.
        let dir = tempfile::tempdir().expect("temp directory");
        let p = write(
            dir.path(),
            "[spawn]\nconcurrency = 0\nwrite_concurrency = 0\n",
        );
        let c = try_load_from(Some(&p), None).expect("should be readable");
        assert_eq!(c.spawn_concurrency, 1, "concurrency = 0 was not floored");
        assert_eq!(
            c.spawn_write_concurrency, 1,
            "write_concurrency = 0 was not floored"
        );
    }
}
