//! Loads config files. Not existing is normal; being malformed is not.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The merged configuration.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Config {
    /// Additional places to look for skills. Not included in the 2 default locations.
    pub skills_paths: Vec<PathBuf>,
    /// Additional places to look for subagent types. Not included in the 2 default locations
    /// (`<project_root>/agents`, `<HOME>/.polaris/agents`).
    pub agents_paths: Vec<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    #[serde(default)]
    skills: RawSkills,
    #[serde(default)]
    agents: RawAgents,
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
            if let Some(paths) = raw.skills.paths {
                merged.skills_paths = paths;
            }
            if let Some(paths) = raw.agents.paths {
                merged.agents_paths = paths;
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
}
