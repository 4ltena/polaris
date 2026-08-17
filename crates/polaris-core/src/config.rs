//! 設定ファイルの読み込み。存在しないことは正常だが、壊れていることは正常ではない。

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// 統合後の設定。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Config {
    /// skill を追加で探す場所。既定の 2 箇所には含まれない。
    pub skills_paths: Vec<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    #[serde(default)]
    skills: RawSkills,
}

#[derive(Debug, Default, Deserialize)]
struct RawSkills {
    /// `None` はキー自体が無いこと（＝前段の値を引き継ぐ）を表し、
    /// `Some(vec![])` は `paths = []` と明示されたこと（＝前段を空で
    /// 上書きする）を表す。`Vec` のままでは両者が区別できず、
    /// プロジェクト側が意図的にグローバルの一覧を空にする手段が無くなる。
    #[serde(default)]
    paths: Option<Vec<PathBuf>>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{path} を読めない: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("{path} の TOML を解釈できない: {source}")]
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

/// グローバルとプロジェクトの設定を読み、後者で前者を上書きする。
/// 存在しないことは失敗ではない。読めないことと壊れていることは失敗である。
///
/// `skills.paths` キーが無いファイルは前段の値をそのまま引き継ぐ。
/// `paths = []` と明示されたファイルは前段を空で上書きする —
/// 個人の skill をあるプロジェクトへ持ち込みたくない場合に、それを
/// 表明する手段が要る。
pub fn try_load_from(global: Option<&Path>, project: Option<&Path>) -> Result<Config, ConfigError> {
    let mut merged = Config::default();
    for path in [global, project].into_iter().flatten() {
        if let Some(raw) = read_one(path)?
            && let Some(paths) = raw.skills.paths
        {
            merged.skills_paths = paths;
        }
    }
    Ok(merged)
}

/// `~/.polaris/config.toml` と `<project-root>/.polaris/config.toml` を解決して読む。
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
        std::fs::write(&p, body).expect("書けない");
        p
    }

    #[test]
    fn returns_empty_config_when_neither_file_exists() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let c = try_load_from(None, Some(&dir.path().join("missing.toml")))
            .expect("存在しないことは失敗ではない");
        assert!(c.skills_paths.is_empty());
    }

    #[test]
    fn reads_skills_paths_from_a_single_file() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let p = write(dir.path(), "[skills]\npaths = [\"/a\", \"/b\"]\n");
        let c = try_load_from(Some(&p), None).expect("読めるはず");
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
        let g = tempfile::tempdir().expect("一時ディレクトリ");
        let pj = tempfile::tempdir().expect("一時ディレクトリ");
        let gp = write(g.path(), "[skills]\npaths = [\"/global\"]\n");
        let pp = write(pj.path(), "[skills]\npaths = [\"/project\"]\n");
        let c = try_load_from(Some(&gp), Some(&pp)).expect("読めるはず");
        assert_eq!(c.skills_paths, vec![std::path::PathBuf::from("/project")]);
    }

    #[test]
    fn malformed_toml_is_reported_not_swallowed() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let p = write(dir.path(), "[skills\npaths = ");
        let err = try_load_from(Some(&p), None).expect_err("壊れた TOML は報告されるべき");
        assert!(
            err.to_string().contains("config.toml"),
            "パスが含まれない: {err}"
        );
    }

    #[test]
    fn project_can_clear_global_skills_paths_with_an_empty_list() {
        // 個人の skill をあるプロジェクトへ持ち込みたくない場合、
        // `paths = []` と明示すればグローバルの一覧を空で上書きできる。
        let g = tempfile::tempdir().expect("一時ディレクトリ");
        let pj = tempfile::tempdir().expect("一時ディレクトリ");
        let gp = write(g.path(), "[skills]\npaths = [\"/global\"]\n");
        let pp = write(pj.path(), "[skills]\npaths = []\n");
        let c = try_load_from(Some(&gp), Some(&pp)).expect("読めるはず");
        assert!(
            c.skills_paths.is_empty(),
            "空リストの明示がグローバルを上書きしていない: {:?}",
            c.skills_paths
        );
    }

    #[test]
    fn project_without_skills_section_inherits_global() {
        // `[skills]` 節そのものが無いプロジェクト設定は「引き継ぐ」であって
        // 「空にする」ではない。空リストの明示（前のテスト）と挙動が
        // 分かれていなければ、この2つは今日のように同一になってしまう。
        let g = tempfile::tempdir().expect("一時ディレクトリ");
        let pj = tempfile::tempdir().expect("一時ディレクトリ");
        let gp = write(g.path(), "[skills]\npaths = [\"/global\"]\n");
        let pp = write(pj.path(), "# skills セクション無し\n");
        let c = try_load_from(Some(&gp), Some(&pp)).expect("読めるはず");
        assert_eq!(
            c.skills_paths,
            vec![std::path::PathBuf::from("/global")],
            "skills 節が無いのにグローバルを引き継いでいない: {:?}",
            c.skills_paths
        );
    }
}
