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
    initial_phase: Option<crate::workflow::Phase>,
    always: Option<Vec<String>>,
    phase_skills: Option<std::collections::BTreeMap<crate::workflow::Phase, Vec<String>>>,
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
}

fn read_one(path: &Path) -> Result<Option<RawConfig>, ConfigError> {
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
        if let Some(raw) = read_one(path)? {
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
            if let Some(enabled) = raw.workflow.enabled {
                merged.workflow.enabled = enabled;
            }
            if let Some(phase) = raw.workflow.initial_phase {
                merged.workflow.initial_phase = phase;
            }
            if let Some(always) = raw.workflow.always {
                merged.workflow.always = always;
            }
            if let Some(phases) = raw.workflow.phase_skills {
                // An explicit empty map clears all inherited stage bindings.
                // Nonempty maps override their named phases only, including [].
                if phases.is_empty() {
                    merged.workflow.phase_skills.clear();
                } else {
                    merged.workflow.phase_skills.extend(phases);
                }
            }
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
