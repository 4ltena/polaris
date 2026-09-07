//! subagent 型の定義（`agents/<type>/SKILL.md`）の解析と discovery。
//! `frontmatter.rs` の `name`/`description` 検証規則を再利用しつつ、
//! subagent 固有の `allowed-tools`/`metadata` を独自に解析する。

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use crate::frontmatter::{self, SkillError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentAccess {
    Read,
    ReadWrite,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentType {
    pub name: String,
    pub description: String,
    pub body: String,
    pub path: PathBuf,
    pub allowed_tools: Vec<String>,
    pub access: AgentAccess,
    pub tier: String,
    pub wall_seconds: u32,
    pub max_turns: u32,
    /// Explicit role focus from the local type definition, never task prose.
    pub workflow_phase: Option<String>,
    pub continuation: bool,
    pub output_schema: PathBuf,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AgentTypeError {
    #[error(transparent)]
    Skill(#[from] SkillError),
    #[error("{agent}'s SKILL.md is missing the required metadata key {key}")]
    MissingMetadataKey { agent: String, key: &'static str },
    #[error("{agent}'s SKILL.md has an invalid value for {key}: {value}")]
    InvalidMetadataValue {
        agent: String,
        key: &'static str,
        value: String,
    },
    #[error("{agent}'s SKILL.md has a malformed metadata line: {line}")]
    MalformedMetadataLine { agent: String, line: String },
    #[error(
        "{agent}'s polaris-output ({value}) points outside the agent's own directory. It must be a relative path to a file under agents/{agent}/"
    )]
    OutputSchemaEscapesAgentDirectory { agent: String, value: String },
}

/// Resolves `.` and `..` components without touching the filesystem.
///
/// `canonicalize` cannot be used for the containment check below: at parse
/// time the declared schema file need not exist yet, and `dir_path` is
/// whatever discovery walked with — possibly relative to a working
/// directory this process never had. A purely lexical normalization is
/// enough for what is being checked, which is that the *declared* path
/// cannot name somewhere else. (A symlink inside the agent's own directory
/// could still redirect the read; that is the type author's own directory
/// pointing at their own choice of file, not an escape granted by the
/// declaration.)
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(Component::ParentDir);
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// `metadata:` ブロック配下の、2スペースインデントされた `key: value` 行
/// だけを読む。値がダブルクォートで囲われていれば剥がす。トップレベルの
/// `name`/`description` は既存の `frontmatter::field` が担うため、ここで
/// は扱わない——`field` はフラットなトップレベルキーだけを想定しており、
/// ネストしたブロックの構文は扱えないため、専用の小さなパーサを別に持つ。
fn parse_metadata_block(
    front_lines: &[&str],
    agent: &str,
) -> Result<BTreeMap<String, String>, AgentTypeError> {
    let mut map = BTreeMap::new();
    let mut in_block = false;
    for line in front_lines {
        if !in_block {
            if line.trim() == "metadata:" {
                in_block = true;
            }
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }
        if !line.starts_with("  ") {
            break;
        }
        let trimmed = line.trim_start();
        let (key, raw_value) =
            trimmed
                .split_once(':')
                .ok_or_else(|| AgentTypeError::MalformedMetadataLine {
                    agent: agent.to_string(),
                    line: trimmed.to_string(),
                })?;
        let value = raw_value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or(value);
        map.insert(key.trim().to_string(), value.to_string());
    }
    Ok(map)
}

fn required_metadata<'a>(
    map: &'a BTreeMap<String, String>,
    agent: &str,
    key: &'static str,
) -> Result<&'a str, AgentTypeError> {
    map.get(key)
        .map(String::as_str)
        .ok_or(AgentTypeError::MissingMetadataKey {
            agent: agent.to_string(),
            key,
        })
}

/// `text` は `SKILL.md` 全文。`dir_name` は親ディレクトリ名
/// （`name` と一致する必要がある——`frontmatter::parse` と同じ規則）。
/// `dir_path` は discovery が知っている、このディレクトリの実パス。
/// `output_schema` を相対パスのまま持つか絶対化するかは呼び出し側の
/// 都合によるため、ここでは `dir_path` に対して相対結合するところまで
/// 行い、絶対化はしない。
pub fn parse(text: &str, dir_name: &str, dir_path: &Path) -> Result<AgentType, AgentTypeError> {
    // frontmatter の外枠（`---` の位置、`name`/`description` の抽出と
    // 検証）は `frontmatter::parse` とまったく同じ規則を踏む必要がある
    // ため、専用の再実装はせず、まずそのまま呼ぶ。返ってくる `body` は
    // subagent のシステムプロンプトとしてそのまま使う。
    let (name, description, body) = frontmatter::parse_fields(text, dir_name)?;

    // `allowed-tools`/`metadata` は `frontmatter::parse` が返さないため、
    // 同じ frontmatter 行群をもう一度読む。`frontmatter::parse_fields` は
    // フロントマター本体の行配列も返す拡張が必要——Step 3a 参照。
    let front_lines = frontmatter::front_lines(text, dir_name)?;

    let allowed_tools = frontmatter::field(&front_lines, "allowed-tools", dir_name)?
        .map(|s| s.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();

    let metadata = parse_metadata_block(&front_lines, dir_name)?;

    let access = match required_metadata(&metadata, dir_name, "polaris-access")? {
        "read" => AgentAccess::Read,
        "read-write" => AgentAccess::ReadWrite,
        other => {
            return Err(AgentTypeError::InvalidMetadataValue {
                agent: dir_name.to_string(),
                key: "polaris-access",
                value: other.to_string(),
            });
        }
    };
    let tier = required_metadata(&metadata, dir_name, "polaris-tier")?.to_string();
    let wall_seconds = required_metadata(&metadata, dir_name, "polaris-wall-seconds")?
        .parse::<u32>()
        .map_err(|_| AgentTypeError::InvalidMetadataValue {
            agent: dir_name.to_string(),
            key: "polaris-wall-seconds",
            value: metadata["polaris-wall-seconds"].clone(),
        })?;
    let max_turns = required_metadata(&metadata, dir_name, "polaris-max-turns")?
        .parse::<u32>()
        .map_err(|_| AgentTypeError::InvalidMetadataValue {
            agent: dir_name.to_string(),
            key: "polaris-max-turns",
            value: metadata["polaris-max-turns"].clone(),
        })?;
    let continuation = match required_metadata(&metadata, dir_name, "polaris-continuation")? {
        "denied" => false,
        "allowed" => true,
        other => {
            return Err(AgentTypeError::InvalidMetadataValue {
                agent: dir_name.to_string(),
                key: "polaris-continuation",
                value: other.to_string(),
            });
        }
    };
    let workflow_phase = metadata.get("polaris-phase").cloned();
    if let Some(phase) = &workflow_phase
        && ![
            "general",
            "brainstorm",
            "specify",
            "implement",
            "review",
            "verify",
            "deliver",
        ]
        .contains(&phase.as_str())
    {
        return Err(AgentTypeError::InvalidMetadataValue {
            agent: dir_name.into(),
            key: "polaris-phase",
            value: phase.clone(),
        });
    }
    // `polaris-output` names a file the *parent* process later reads and
    // feeds to a JSON Schema validator, so where it is allowed to point is
    // a trust boundary, not a convenience. `Path::join` replaces the whole
    // path when the right-hand side is absolute, so `polaris-output:
    // /etc/passwd` would silently resolve to `/etc/passwd`; `../` walks out
    // just as easily. Confine it to the agent's own directory.
    let declared_output = required_metadata(&metadata, dir_name, "polaris-output")?;
    let output_schema = dir_path.join(declared_output);
    let normalized_dir = normalize_lexically(dir_path);
    let normalized_output = normalize_lexically(&output_schema);
    if !normalized_output.starts_with(&normalized_dir) || normalized_output == normalized_dir {
        return Err(AgentTypeError::OutputSchemaEscapesAgentDirectory {
            agent: dir_name.to_string(),
            value: declared_output.to_string(),
        });
    }

    Ok(AgentType {
        name,
        description,
        body,
        path: dir_path.to_path_buf(),
        allowed_tools,
        access,
        tier,
        wall_seconds,
        max_turns,
        workflow_phase,
        continuation,
        output_schema,
    })
}

#[derive(Debug, thiserror::Error)]
pub enum SkipCauseAgentType {
    #[error("cannot read SKILL.md: {0}")]
    Unreadable(std::io::Error),
    #[error(transparent)]
    Invalid(AgentTypeError),
}

#[derive(Debug, thiserror::Error)]
#[error("{dir_name}: {cause}")]
pub struct SkippedAgentType {
    pub dir_name: String,
    #[source]
    pub cause: SkipCauseAgentType,
}

#[derive(Debug, Default)]
pub struct DiscoveredAgentTypes {
    pub agent_types: Vec<AgentType>,
    pub skipped: Vec<SkippedAgentType>,
}

/// `polaris_skills::discovery::discover_in` と同じ形の歩行を行う。
/// 実装を共有せず複製したのは、両者の戻り値の要素型（`Skill` と
/// `AgentType`）が違い、無理に共有すると型消去かジェネリック化が要る
/// ためである。歩行ロジック自体は30行程度であり、複製の方が
/// `discover_in`（既存の skill 読み込み経路、挙動を変えたくない）を
/// 一切変更せずに済む分だけ安全である。
pub fn discover_agent_types_in(dirs: &[PathBuf]) -> DiscoveredAgentTypes {
    let mut out = DiscoveredAgentTypes::default();
    for dir in dirs {
        let Ok(mut entries) = std::fs::read_dir(dir).map(|it| it.collect::<Vec<_>>()) else {
            continue;
        };
        entries.sort_by_key(|e| e.as_ref().ok().map(std::fs::DirEntry::file_name));
        for entry in entries.into_iter().flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let dir_name = entry.file_name().to_string_lossy().to_string();
            if out.agent_types.iter().any(|a| a.name == dir_name) {
                continue;
            }
            let skill_md = path.join("SKILL.md");
            if !skill_md.exists() {
                continue;
            }
            match std::fs::read_to_string(&skill_md) {
                Ok(text) => match parse(&text, &dir_name, &path) {
                    Ok(agent) => out.agent_types.push(agent),
                    Err(e) => out.skipped.push(SkippedAgentType {
                        dir_name,
                        cause: SkipCauseAgentType::Invalid(e),
                    }),
                },
                Err(e) => out.skipped.push(SkippedAgentType {
                    dir_name,
                    cause: SkipCauseAgentType::Unreadable(e),
                }),
            }
        }
    }
    out
}

/// `<project_root>/agents` と `<HOME>/.polaris/agents` に加えて設定由来の
/// 追加パスを、この順で歩く。`.polaris/skills` と対になる規約。
pub fn discover_agent_types(project_root: &Path, extra_paths: &[PathBuf]) -> DiscoveredAgentTypes {
    let mut dirs = vec![project_root.join("agents")];
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(Path::new(&home).join(".polaris").join("agents"));
    }
    dirs.extend_from_slice(extra_paths);
    discover_agent_types_in(&dirs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_text() -> &'static str {
        "---\nname: file-inspector\ndescription: 単一ファイルを読み取り専用で棚卸しし、責務、入出力、対応するテストを返す。\nallowed-tools: read\nmetadata:\n  polaris-access: read\n  polaris-tier: low\n  polaris-wall-seconds: \"360\"\n  polaris-max-turns: \"12\"\n  polaris-continuation: \"denied\"\n  polaris-output: references/result.schema.json\n---\n本文がそのまま subagent のシステムプロンプトとなる。\n"
    }

    #[test]
    fn a_valid_agent_type_parses_every_field() {
        let a = parse(
            valid_text(),
            "file-inspector",
            Path::new("agents/file-inspector"),
        )
        .unwrap();
        assert_eq!(a.name, "file-inspector");
        assert_eq!(a.allowed_tools, vec!["read".to_string()]);
        assert_eq!(a.access, AgentAccess::Read);
        assert_eq!(a.tier, "low");
        assert_eq!(a.wall_seconds, 360);
        assert_eq!(a.max_turns, 12);
        assert!(!a.continuation);
        assert_eq!(
            a.output_schema,
            Path::new("agents/file-inspector").join("references/result.schema.json")
        );
        assert_eq!(
            a.body.trim(),
            "本文がそのまま subagent のシステムプロンプトとなる。"
        );
    }

    #[test]
    fn a_missing_metadata_key_is_rejected() {
        let text =
            "---\nname: bad\ndescription: 説明。\nmetadata:\n  polaris-tier: low\n---\n本文。\n";
        let err = parse(text, "bad", Path::new("agents/bad")).unwrap_err();
        assert!(matches!(
            err,
            AgentTypeError::MissingMetadataKey {
                key: "polaris-access",
                ..
            }
        ));
    }

    #[test]
    fn an_invalid_access_value_is_rejected() {
        let text = "---\nname: bad\ndescription: 説明。\nmetadata:\n  polaris-access: sudo\n  polaris-tier: low\n  polaris-wall-seconds: \"1\"\n  polaris-max-turns: \"1\"\n  polaris-continuation: \"denied\"\n  polaris-output: r.json\n---\n本文。\n";
        let err = parse(text, "bad", Path::new("agents/bad")).unwrap_err();
        assert!(matches!(
            err,
            AgentTypeError::InvalidMetadataValue {
                key: "polaris-access",
                ..
            }
        ));
    }

    /// The same valid frontmatter, with `polaris-output` swapped for
    /// whatever the caller wants to try declaring.
    fn text_with_output(value: &str) -> String {
        valid_text().replace("references/result.schema.json", value)
    }

    #[test]
    fn an_ordinary_relative_output_schema_resolves_under_the_agent_directory() {
        // The control for the two rejection tests below: the normal case
        // is unaffected by the containment check.
        let a = parse(
            &text_with_output("references/result.schema.json"),
            "file-inspector",
            Path::new("agents/file-inspector"),
        )
        .unwrap();
        assert_eq!(
            a.output_schema,
            Path::new("agents/file-inspector/references/result.schema.json")
        );
    }

    #[test]
    fn an_absolute_output_schema_is_rejected() {
        // `Path::join` discards the left-hand side entirely when the right
        // is absolute, so without the check this type's `output_schema`
        // would simply *be* `/etc/passwd` — a file the unsandboxed parent
        // then reads and hands to the schema validator.
        let err = parse(
            &text_with_output("/etc/passwd"),
            "file-inspector",
            Path::new("agents/file-inspector"),
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                AgentTypeError::OutputSchemaEscapesAgentDirectory { .. }
            ),
            "an absolute polaris-output was accepted: {err:?}"
        );
    }

    #[test]
    fn an_output_schema_escaping_via_parent_components_is_rejected() {
        let err = parse(
            &text_with_output("../../../etc/passwd"),
            "file-inspector",
            Path::new("agents/file-inspector"),
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                AgentTypeError::OutputSchemaEscapesAgentDirectory { .. }
            ),
            "a ../-escaping polaris-output was accepted: {err:?}"
        );
    }

    #[test]
    fn a_parent_component_that_stays_inside_the_agent_directory_is_still_accepted() {
        // The check rejects escaping, not `..` as a character sequence.
        // `references/../result.schema.json` never leaves the directory, so
        // refusing it would be over-broad.
        let a = parse(
            &text_with_output("references/../result.schema.json"),
            "file-inspector",
            Path::new("agents/file-inspector"),
        )
        .unwrap();
        assert_eq!(
            a.output_schema,
            Path::new("agents/file-inspector/references/../result.schema.json")
        );
    }

    #[test]
    fn discover_agent_types_in_finds_a_valid_type_and_skips_an_invalid_one() {
        let root = tempfile::tempdir().unwrap();
        let good = root.path().join("file-inspector");
        std::fs::create_dir_all(&good).unwrap();
        std::fs::write(good.join("SKILL.md"), valid_text()).unwrap();

        let bad = root.path().join("broken");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("SKILL.md"), "not frontmatter at all").unwrap();

        let d = discover_agent_types_in(&[root.path().to_path_buf()]);
        assert_eq!(d.agent_types.len(), 1);
        assert_eq!(d.agent_types[0].name, "file-inspector");
        assert_eq!(d.skipped.len(), 1);
        assert_eq!(d.skipped[0].dir_name, "broken");
    }

    #[test]
    fn discover_agent_types_in_finds_the_files_md_writer_type() {
        let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let agents_dir = repo_root.join("agents");
        let d = discover_agent_types_in(&[agents_dir]);
        assert!(
            d.agent_types.iter().any(|a| a.name == "files-md-writer"),
            "files-md-writer not found among: {:?}",
            d.agent_types.iter().map(|a| &a.name).collect::<Vec<_>>()
        );
        assert!(d.skipped.is_empty(), "unexpected skips: {:?}", d.skipped);
    }
}
