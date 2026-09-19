//! Loads config files. Not existing is normal; being malformed is not.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The merged configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Strict history is opt-in and requires a pinned local embedding backend.
    pub history_mode: crate::conversation_state::HistoryMode,
    pub embedding: EmbeddingConfig,
    /// Built-in workflow skills are enabled unless configuration explicitly opts out.
    pub workflow: crate::workflow::WorkflowConfig,
    /// Whether root tool mutations automatically regenerate affected `files.md` maps.
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
    /// How large this project's build output may grow before the oldest of
    /// it is removed. TOML keys: `[build_cache] max_bytes`,
    /// `[build_cache] max_percent`. Unset means nothing is ever removed.
    pub build_cache: crate::build_cache::Cap,
    /// Which directory the cap applies to. `None` means `<project>/target`.
    pub build_cache_path: Option<PathBuf>,
}

/// The API key is deliberately not here. A project's configuration file is
/// committed and shared; a credential read from the environment is not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DecisionConfig {
    /// Off unless asked for. Every path that consults the judge works
    /// without one, so enabling it is a choice to spend a round trip.
    pub enabled: bool,
    pub base_url: Option<String>,
    pub model: Option<String>,
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
            history_mode: crate::conversation_state::HistoryMode::Legacy,
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
            files_md_auto_regenerate: true,
            skills_paths: Vec::new(),
            agents_paths: Vec::new(),
            spawn_concurrency: crate::spawn::DEFAULT_CONCURRENCY,
            spawn_write_concurrency: crate::spawn::DEFAULT_WRITE_CONCURRENCY,
            tiers: TierConfig::default(),
            decision: DecisionConfig::default(),
            build_cache: crate::build_cache::Cap::default(),
            build_cache_path: None,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    history_mode: Option<crate::conversation_state::HistoryMode>,
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
    build_cache: RawBuildCache,
}

/// Explicit, local-only embedding runtime. No downloads occur during a turn.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingConfig {
    pub runtime: Option<PathBuf>,
    pub model_path: Option<PathBuf>,
    pub revision: Option<String>,
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
    base_url: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

impl RawDecision {
    fn apply_to(self, merged: &mut DecisionConfig) -> Result<(), ConfigError> {
        if let Some(enabled) = self.enabled {
            merged.enabled = enabled;
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
        Ok(())
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
    #[error("[decision] のbase_urlとmodelに空の値は書けません")]
    EmptyDecisionValue,
    #[error("[build_cache] のmax_bytesを読めません。80GiB・500MB・バイト数のいずれかで書きます")]
    BuildCacheSize,
    #[error("[build_cache] のmax_percentは1から100までです")]
    BuildCacheShare,
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

/// Reads the global and project configs, letting the latter override the
/// former. Not existing is not a failure. Being unreadable or malformed is.
///
/// A file missing the `skills.paths` key inherits the prior stage's value
/// as-is. A file that explicitly gives `paths = []` overrides the prior
/// stage with an empty list — there needs to be a way to declare that when
/// personal skills should not be carried into a given project.
pub fn try_load_from(global: Option<&Path>, project: Option<&Path>) -> Result<Config, ConfigError> {
    let mut merged = Config::default();
    for path in [global, project].into_iter().flatten() {
        if let Some(raw) = read_one::<RawConfig>(path)? {
            if let Some(mode) = raw.history_mode {
                merged.history_mode = mode;
            }
            if raw.embedding.runtime.is_some() {
                merged.embedding.runtime = raw.embedding.runtime;
            }
            if raw.embedding.model_path.is_some() {
                merged.embedding.model_path = raw.embedding.model_path;
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
            raw.decision.apply_to(&mut merged.decision)?;
        }
    }
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
    Ok(merged)
}

/// Resolves and reads `~/.polaris/config.toml` and `<project-root>/.polaris/config.toml`.
pub fn load(project_root: &Path) -> Result<Config, ConfigError> {
    let global =
        std::env::var_os("HOME").map(|h| Path::new(&h).join(".polaris").join("config.toml"));
    let project = project_root.join(".polaris").join("config.toml");
    try_load_from(global.as_deref(), Some(&project))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_defaults_resolve_builtin_skills_for_every_phase() {
        use crate::workflow::{Phase, SessionWorkflow, WORKFLOW_TOKEN_LIMIT};
        let config = try_load_from(None, None).unwrap();
        assert!(config.workflow.enabled);
        assert_eq!(
            config.history_mode,
            crate::conversation_state::HistoryMode::Legacy
        );
        assert_eq!(config.embedding, EmbeddingConfig::default());
        assert_eq!(config.workflow.initial_phase, Phase::General);
        let runtime = SessionWorkflow::new(config.workflow);
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

    #[test]
    fn files_md_auto_regenerate_defaults_true_and_project_overrides_global() {
        let global = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let global_path = write(global.path(), "[files_md]\nauto_regenerate = false\n");
        let project_path = write(project.path(), "# inherit files_md\n");
        assert!(Config::default().files_md_auto_regenerate);
        assert!(
            !try_load_from(Some(&global_path), Some(&project_path))
                .unwrap()
                .files_md_auto_regenerate
        );
        let project_path = write(project.path(), "[files_md]\nauto_regenerate = true\n");
        assert!(
            try_load_from(Some(&global_path), Some(&project_path))
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
            "[tier.low]\nmodel = \"\"\n",
            "[tier.medium]\neffort = \"   \"\n",
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
            "[tier.low]\nmodle = \"gpt-6-astra\"\n",
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
