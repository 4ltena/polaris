# polaris M3a Skills ローダと skill ツール Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Agent Skills 仕様に準拠した skill をディスクから読み込み、`skill` ツールで検索と読み出しができるようにする。

**Architecture:** 設定ファイルを `polaris-core::config` に置き、skill の探索と検証を新クレート `polaris-skills` が担う。`skill` ツールは 1 引数で、与えられた語が skill 名に完全一致すれば本文を返し、そうでなければ名前と説明を検索して候補を返す。常時コンテキストに skill の一覧は載せない。

**Tech Stack:** Rust 1.96.0 / edition 2024、serde、toml、既存の polaris-core / polaris-tools

**Spec:** `docs/superpowers/specs/2026-08-16-polaris-harness-design.md`

## Global Constraints

- Rust edition は `2024`、`rust-toolchain.toml` の channel は `1.96.0`
- 常時コンテキストは 990 トークン以下。基準トークナイザは `tiktoken_rs::o200k_base()`
- 常時提供するツールは 6 本を超えない
- クレート名の接頭辞は `polaris-`
- Agent Skills 仕様に準拠し、独自のフロントマターを追加しない
- `name` は 64 文字以内、小文字英数字とハイフンのみ、先頭と末尾にハイフンを置かず、連続ハイフンを含まず、親ディレクトリ名と一致する
- `description` は 1 文字以上 1024 文字以内
- 監査ログへ書く文字列は `AuditLog::record` が伏字化を通す
- 新しい `.rs` ファイルには `//!` のモジュールドキュメントを付ける。`docs/filemap.md` のスナップショットテストが名指しで落とす
- コミットのタイトルは英語、本文は日本語可。末尾に空行 1 行を挟んで `Co-Authored-By: Claude <noreply@anthropic.com>` を付ける

## 前提となる既存の API

- `polaris_tools::{ToolSpec { name, description, parameters }, ToolError::{PathDenied, Io}, all_specs() -> Vec<ToolSpec>}`
- `polaris_tools::read::read(path: &Path, offset: usize, limit: usize) -> Result<String, ToolError>`
- `polaris_core::budget::{BUDGET_LIMIT = 990, MAX_TOOLS = 6, count_tokens(&str) -> usize, always_on_tokens(&str, &[ToolSpec]) -> usize}`
- `polaris_core::prompt::{SYSTEM_PROMPT, build_system(constitution: &str, environment: &str) -> String}`
- `polaris_core::constitution::{CONSTITUTION_LIMIT = 150, ENVIRONMENT_LIMIT = 200, load(project_root: &Path) -> String, environment_block(cwd: &Path, branch: Option<&str>) -> String}`
- `polaris_core::agent::run(provider: &dyn Provider, session: &mut Session, audit: &mut AuditLog, stop: &mut StopTracker, system: &str) -> Result<String, AgentError>` と、その中の `fn dispatch(call: &ToolCall) -> Result<String, String>`
- 常時コンテキストの現在値は下限 166 トークン、最悪ケース 342 トークン。ツールは `read` の 1 本

## ファイル構成

```
crates/
├── polaris-core/src/
│   └── config.rs                設定ファイルの読み込みと統合
└── polaris-skills/
    ├── Cargo.toml
    └── src/
        ├── lib.rs               Skill 型と読み込みの入口
        ├── frontmatter.rs       SKILL.md のフロントマター解析と検証
        └── discovery.rs         探索パスの走査と名前衝突の解決
```

---

### Task 1: 設定ファイルの読み込み

**Files:**
- Create: `crates/polaris-core/src/config.rs`
- Modify: `crates/polaris-core/src/lib.rs`
- Modify: `crates/polaris-core/Cargo.toml`
- Modify: `Cargo.toml`（`[workspace.dependencies]` に `toml`）

**Interfaces:**
- Consumes: なし
- Produces: `polaris_core::config::{Config { skills_paths: Vec<PathBuf> }, ConfigError}`、`config::try_load_from(global: Option<&Path>, project: Option<&Path>) -> Result<Config, ConfigError>`、`config::load(project_root: &Path) -> Result<Config, ConfigError>`

- [ ] **Step 1: 依存を足してクレートの骨格を用意する**

`Cargo.toml` の `[workspace.dependencies]` へ追加する。

```toml
toml = "0.8"
```

`crates/polaris-core/Cargo.toml` の `[dependencies]` へ追加する。

```toml
toml = { workspace = true }
```

`crates/polaris-core/src/config.rs` を空で作り、`crates/polaris-core/src/lib.rs` へ `pub mod config;` を追加する。

- [ ] **Step 2: 失敗するテストを書く**

`crates/polaris-core/src/config.rs` の末尾に置く。

```rust
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
        let c = load_from(None, Some(&dir.path().join("missing.toml")));
        assert!(c.skills_paths.is_empty());
    }

    #[test]
    fn reads_skills_paths_from_a_single_file() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let p = write(dir.path(), "[skills]\npaths = [\"/a\", \"/b\"]\n");
        let c = load_from(Some(&p), None);
        assert_eq!(
            c.skills_paths,
            vec![std::path::PathBuf::from("/a"), std::path::PathBuf::from("/b")]
        );
    }

    #[test]
    fn project_overrides_global() {
        let g = tempfile::tempdir().expect("一時ディレクトリ");
        let pj = tempfile::tempdir().expect("一時ディレクトリ");
        let gp = write(g.path(), "[skills]\npaths = [\"/global\"]\n");
        let pp = write(pj.path(), "[skills]\npaths = [\"/project\"]\n");
        let c = load_from(Some(&gp), Some(&pp));
        assert_eq!(c.skills_paths, vec![std::path::PathBuf::from("/project")]);
    }

    #[test]
    fn malformed_toml_is_reported_not_swallowed() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let p = write(dir.path(), "[skills\npaths = ");
        let err = try_load_from(Some(&p), None).expect_err("壊れた TOML は報告されるべき");
        assert!(err.to_string().contains("config.toml"), "パスが含まれない: {err}");
    }
}
```

4 つ目のテストが要点である。設定が壊れているのを黙って空扱いにすると、利用者は skill が読まれない理由を永久に知れない。読めなかったことと、書かれていなかったことは違う。

- [ ] **Step 3: テストが失敗することを確認する**

Run: `cargo test -p polaris-core config`
Expected: コンパイルエラー。`load_from` と `try_load_from` が未定義

- [ ] **Step 4: 実装を書く**

`crates/polaris-core/src/config.rs` の先頭に置く。

```rust
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
    #[serde(default)]
    paths: Vec<PathBuf>,
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
pub fn try_load_from(
    global: Option<&Path>,
    project: Option<&Path>,
) -> Result<Config, ConfigError> {
    let mut merged = Config::default();
    for path in [global, project].into_iter().flatten() {
        if let Some(raw) = read_one(path)? {
            if !raw.skills.paths.is_empty() {
                merged.skills_paths = raw.skills.paths;
            }
        }
    }
    Ok(merged)
}

/// 失敗を空の設定に潰す版。呼び出し側が診断を出せない場面でのみ使う。
pub fn load_from(global: Option<&Path>, project: Option<&Path>) -> Config {
    try_load_from(global, project).unwrap_or_default()
}

/// `~/.polaris/config.toml` と `<project-root>/.polaris/config.toml` を解決して読む。
pub fn load(project_root: &Path) -> Result<Config, ConfigError> {
    let global = std::env::var_os("HOME")
        .map(|h| Path::new(&h).join(".polaris").join("config.toml"));
    let project = project_root.join(".polaris").join("config.toml");
    try_load_from(global.as_deref(), Some(&project))
}
```

- [ ] **Step 5: テストが通ることを確認する**

Run: `cargo test -p polaris-core config`
Expected: 4 件とも PASS

- [ ] **Step 6: 壊れた設定を黙って無視しないことを変異で確かめる**

`try_load_from` の `Parse` を返す箇所を一時的に `Ok(None)` に置き換え、`malformed_toml_is_reported_not_swallowed` が落ちることを確認してから戻す。観測した出力を報告に書く。

- [ ] **Step 7: コミットする**

```bash
git add Cargo.toml crates/polaris-core docs/filemap.md
git commit -F - <<'MSG'
feat(core): load layered configuration

~/.polaris/config.toml と <project>/.polaris/config.toml を読み、後者で
前者を上書きする。存在しないことは正常として空を返すが、読めないことと
TOML が壊れていることは報告する。黙って空扱いにすると、利用者は skill が
読まれない理由を永久に知れない。

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

### Task 2: SKILL.md のフロントマター解析と検証

**Files:**
- Create: `crates/polaris-skills/Cargo.toml`
- Create: `crates/polaris-skills/src/lib.rs`
- Create: `crates/polaris-skills/src/frontmatter.rs`

**Interfaces:**
- Consumes: なし
- Produces: `polaris_skills::frontmatter::{SkillError, parse(text: &str, dir_name: &str) -> Result<(String, String, String), SkillError>}`。`Skill` 型は Task 3 で定義する

- [ ] **Step 1: クレートの骨格を作る**

`crates/polaris-skills/Cargo.toml`

```toml
[package]
name = "polaris-skills"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true

[dependencies]
serde = { workspace = true }
thiserror = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }
```

`crates/polaris-skills/src/lib.rs`

```rust
//! Agent Skills 仕様に準拠した skill の読み込み。独自のフロントマターは足さない。

pub mod frontmatter;
```

`crates/polaris-skills/src/frontmatter.rs` は空で作る。

- [ ] **Step 2: 失敗するテストを書く**

`crates/polaris-skills/src/frontmatter.rs` の末尾に置く。

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "---\nname: git-commit\ndescription: コミットを作る。\n---\n\n本文。\n";

    #[test]
    fn parses_name_description_and_body() {
        let (name, desc, body) = parse(GOOD, "git-commit").expect("読めるべき");
        assert_eq!(name, "git-commit");
        assert_eq!(desc, "コミットを作る。");
        assert_eq!(body.trim(), "本文。");
    }

    #[test]
    fn rejects_a_name_that_does_not_match_the_directory() {
        let err = parse(GOOD, "other-dir").expect_err("親ディレクトリ名と不一致は拒否");
        assert!(matches!(err, SkillError::NameMismatch { .. }));
    }

    #[test]
    fn rejects_names_violating_the_specification() {
        for bad in ["Git-Commit", "-lead", "trail-", "double--hyphen", "under_score"] {
            let text = format!("---\nname: {bad}\ndescription: x\n---\n本文\n");
            assert!(parse(&text, bad).is_err(), "{bad} は拒否されるべき");
        }
    }

    #[test]
    fn rejects_an_empty_or_oversized_description() {
        let empty = "---\nname: a\ndescription: \"\"\n---\n本文\n";
        assert!(parse(empty, "a").is_err(), "空の description は拒否");

        let long = format!("---\nname: a\ndescription: \"{}\"\n---\n本文\n", "x".repeat(1025));
        assert!(parse(&long, "a").is_err(), "1024 文字超の description は拒否");
    }

    #[test]
    fn rejects_a_file_without_frontmatter() {
        assert!(parse("# 見出しだけ\n", "a").is_err());
    }

    #[test]
    fn accepts_optional_fields_without_complaint() {
        let text = "---\nname: a\ndescription: x\nlicense: MIT\nallowed-tools: read bash\n---\n本文\n";
        let (name, _, _) = parse(text, "a").expect("任意フィールドは許容される");
        assert_eq!(name, "a");
    }
}
```

最後のテストが重要である。仕様は `license`、`compatibility`、`metadata`、`allowed-tools` を任意フィールドとして認めている。知らないフィールドで落ちる実装は、仕様準拠を名乗れない。

- [ ] **Step 3: テストが失敗することを確認する**

Run: `cargo test -p polaris-skills`
Expected: コンパイルエラー。`parse` と `SkillError` が未定義

- [ ] **Step 4: 実装を書く**

`crates/polaris-skills/src/frontmatter.rs` の先頭に置く。

```rust
//! SKILL.md のフロントマター解析。仕様が定める制約だけを検証し、独自の制約を足さない。

/// 仕様が `name` に課す上限。
const NAME_MAX: usize = 64;
/// 仕様が `description` に課す上限。
const DESCRIPTION_MAX: usize = 1024;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SkillError {
    #[error("フロントマターが無い")]
    NoFrontmatter,
    #[error("必須フィールド {field} が無い")]
    MissingField { field: &'static str },
    #[error("name {name} が仕様の命名規則に反する")]
    InvalidName { name: String },
    #[error("name {name} が親ディレクトリ名 {dir} と一致しない")]
    NameMismatch { name: String, dir: String },
    #[error("description の長さ {len} が範囲外")]
    InvalidDescription { len: usize },
}

/// 仕様の命名規則。小文字英数字とハイフンのみ、先頭末尾にハイフンなし、連続ハイフンなし、1 から 64 文字。
fn name_is_valid(name: &str) -> bool {
    if name.is_empty() || name.chars().count() > NAME_MAX {
        return false;
    }
    if name.starts_with('-') || name.ends_with('-') || name.contains("--") {
        return false;
    }
    name.chars()
        .all(|c| c == '-' || c.is_ascii_digit() || c.is_ascii_lowercase())
}

/// `key: value` の平坦な走査で `name` と `description` を取り出す。
/// 知らないキーは黙って読み飛ばす。仕様が任意フィールドを認めているため、
/// 未知のキーで落ちる実装は仕様準拠を名乗れない。
fn field(front: &str, key: &str) -> Option<String> {
    for line in front.lines() {
        let Some(rest) = line.strip_prefix(key) else {
            continue;
        };
        let Some(rest) = rest.strip_prefix(':') else {
            continue;
        };
        let v = rest.trim();
        let v = v
            .strip_prefix('"')
            .and_then(|x| x.strip_suffix('"'))
            .unwrap_or(v);
        return Some(v.to_string());
    }
    None
}

/// フロントマターを解析し、`(name, description, body)` を返す。
pub fn parse(text: &str, dir_name: &str) -> Result<(String, String, String), SkillError> {
    let rest = text.strip_prefix("---").ok_or(SkillError::NoFrontmatter)?;
    let rest = rest.trim_start_matches(['\r', '\n']);
    let end = rest.find("\n---").ok_or(SkillError::NoFrontmatter)?;
    let front = &rest[..end];
    let body = rest[end + 4..].trim_start_matches(['\r', '\n']).to_string();

    let name = field(front, "name").ok_or(SkillError::MissingField { field: "name" })?;
    let description =
        field(front, "description").ok_or(SkillError::MissingField { field: "description" })?;

    if !name_is_valid(&name) {
        return Err(SkillError::InvalidName { name });
    }
    if name != dir_name {
        return Err(SkillError::NameMismatch {
            name,
            dir: dir_name.to_string(),
        });
    }
    let len = description.chars().count();
    if len == 0 || len > DESCRIPTION_MAX {
        return Err(SkillError::InvalidDescription { len });
    }

    Ok((name, description, body))
}
```

- [ ] **Step 5: テストが通ることを確認する**

Run: `cargo test -p polaris-skills`
Expected: 6 件とも PASS

- [ ] **Step 6: 命名規則の各条件を変異で確かめる**

`name_is_valid` の連続ハイフン検査、先頭末尾検査、文字種検査を 1 つずつ外し、そのたびに `rejects_names_violating_the_specification` が落ちることを確認して戻す。3 回とも観測した出力を報告に書く。1 つの検査を外しても落ちないなら、そのテストはその条件を守っていない。

- [ ] **Step 7: コミットする**

```bash
git add crates/polaris-skills docs/filemap.md
git commit -F - <<'MSG'
feat(skills): parse and validate SKILL.md frontmatter

Agent Skills 仕様が定める制約だけを検証する。name の命名規則、親ディレクトリ
名との一致、description の長さ。license や allowed-tools などの任意フィールドは
黙って読み飛ばす。未知のキーで落ちる実装は仕様準拠を名乗れない。

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

### Task 3: skill の探索と名前衝突の解決

**Files:**
- Create: `crates/polaris-skills/src/discovery.rs`
- Modify: `crates/polaris-skills/src/lib.rs`

**Interfaces:**
- Consumes: `polaris_skills::frontmatter::parse`
- Produces: `polaris_skills::{Skill { name, description, body, path }, discover(project_root: &Path, extra_paths: &[PathBuf]) -> Vec<Skill>, discover_in(dirs: &[PathBuf]) -> Vec<Skill>}`

- [ ] **Step 1: 失敗するテストを書く**

`crates/polaris-skills/src/discovery.rs` の末尾に置く。

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn put(root: &std::path::Path, name: &str, desc: &str, body: &str) {
        let d = root.join(name);
        std::fs::create_dir_all(&d).expect("作れない");
        std::fs::write(
            d.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {desc}\n---\n{body}\n"),
        )
        .expect("書けない");
    }

    #[test]
    fn finds_skills_in_each_directory() {
        let a = tempfile::tempdir().expect("一時");
        let b = tempfile::tempdir().expect("一時");
        put(a.path(), "alpha", "あるふぁ", "A");
        put(b.path(), "beta", "べーた", "B");

        let found = discover_in(&[a.path().to_path_buf(), b.path().to_path_buf()]);
        let mut names: Vec<&str> = found.iter().map(|s| s.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    #[test]
    fn first_directory_wins_on_a_name_collision() {
        let first = tempfile::tempdir().expect("一時");
        let second = tempfile::tempdir().expect("一時");
        put(first.path(), "dup", "さき", "FIRST");
        put(second.path(), "dup", "あと", "SECOND");

        let found = discover_in(&[first.path().to_path_buf(), second.path().to_path_buf()]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].body.trim(), "FIRST");
    }

    #[test]
    fn an_invalid_skill_is_skipped_without_killing_the_others() {
        let root = tempfile::tempdir().expect("一時");
        put(root.path(), "good", "よい", "OK");
        let bad = root.path().join("Bad-Name");
        std::fs::create_dir_all(&bad).expect("作れない");
        std::fs::write(bad.join("SKILL.md"), "---\nname: Bad-Name\ndescription: x\n---\n").ok();

        let found = discover_in(&[root.path().to_path_buf()]);
        assert_eq!(found.len(), 1, "壊れた skill 1 件で全部が落ちてはいけない");
        assert_eq!(found[0].name, "good");
    }

    #[test]
    fn a_missing_directory_is_not_an_error() {
        let found = discover_in(&[std::path::PathBuf::from("/does/not/exist")]);
        assert!(found.is_empty());
    }

    #[test]
    fn a_directory_without_skill_md_is_ignored() {
        let root = tempfile::tempdir().expect("一時");
        std::fs::create_dir_all(root.path().join("notaskill")).expect("作れない");
        assert!(discover_in(&[root.path().to_path_buf()]).is_empty());
    }
}
```

3 つ目が要点である。`~/.polaris/skills/` に壊れた skill が 1 つ混ざっただけで全部の skill が使えなくなる実装は、実用に耐えない。

- [ ] **Step 2: テストが失敗することを確認する**

Run: `cargo test -p polaris-skills discovery`
Expected: コンパイルエラー。`discover_in` が未定義

- [ ] **Step 3: 実装を書く**

`crates/polaris-skills/src/discovery.rs` の先頭に置く。

```rust
//! skill の探索。1 件の破損が全体を巻き込まないよう、読めないものは飛ばす。

use std::path::{Path, PathBuf};

use crate::frontmatter;
use crate::Skill;

/// 与えられたディレクトリ群を順に走査する。名前が衝突したら先に見つけたものを採る。
pub fn discover_in(dirs: &[PathBuf]) -> Vec<Skill> {
    let mut out: Vec<Skill> = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(dir_name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let manifest = path.join("SKILL.md");
            let Ok(text) = std::fs::read_to_string(&manifest) else {
                continue;
            };
            let Ok((name, description, body)) = frontmatter::parse(&text, dir_name) else {
                continue;
            };
            if out.iter().any(|s| s.name == name) {
                continue;
            }
            out.push(Skill {
                name,
                description,
                body,
                path: manifest,
            });
        }
    }
    out
}

/// 既定の 2 箇所と設定で追加された場所を、この順に走査する。
pub fn discover(project_root: &Path, extra_paths: &[PathBuf]) -> Vec<Skill> {
    let mut dirs = vec![project_root.join(".polaris").join("skills")];
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(Path::new(&home).join(".polaris").join("skills"));
    }
    dirs.extend_from_slice(extra_paths);
    discover_in(&dirs)
}
```

`crates/polaris-skills/src/lib.rs` を次の内容にする。

```rust
//! Agent Skills 仕様に準拠した skill の読み込み。独自のフロントマターは足さない。

pub mod discovery;
pub mod frontmatter;

use std::path::PathBuf;

pub use discovery::{discover, discover_in};
pub use frontmatter::SkillError;

/// 読み込み済みの skill 1 件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
    pub path: PathBuf,
}
```

- [ ] **Step 4: テストが通ることを確認する**

Run: `cargo test -p polaris-skills`
Expected: 11 件とも PASS

- [ ] **Step 5: 衝突解決を変異で確かめる**

`out.iter().any(...)` の重複検査を外し、`first_directory_wins_on_a_name_collision` が落ちることを確認して戻す。観測した出力を報告に書く。

- [ ] **Step 6: コミットする**

```bash
git add crates/polaris-skills docs/filemap.md
git commit -F - <<'MSG'
feat(skills): discover skills across the search path

プロジェクト直下、ユーザーホーム、設定で追加された場所をこの順に走査し、
名前が衝突したら先に見つけたものを採る。

読めない skill と検証に落ちた skill は飛ばす。1 件の破損で全部が使えなく
なる実装は実用に耐えない。

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

### Task 4: skill ツール

**Files:**
- Modify: `crates/polaris-tools/src/lib.rs`
- Create: `crates/polaris-tools/src/skill.rs`
- Modify: `crates/polaris-tools/Cargo.toml`

**Interfaces:**
- Consumes: `polaris_skills::{Skill, discover_in}`
- Produces: `polaris_tools::skill::{lookup(skills: &[Skill], q: &str) -> String}`、`all_specs()` が `read` と `skill` の 2 本を返す

- [ ] **Step 1: 失敗するテストを書く**

`crates/polaris-tools/src/skill.rs` の末尾に置く。

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn fixtures() -> Vec<polaris_skills::Skill> {
        vec![
            polaris_skills::Skill {
                name: "git-commit".into(),
                description: "コミットを作る。commit や git の話題で使う。".into(),
                body: "本文A".into(),
                path: "/x/git-commit/SKILL.md".into(),
            },
            polaris_skills::Skill {
                name: "writing-style".into(),
                description: "日本語の散文を整える。".into(),
                body: "本文B".into(),
                path: "/x/writing-style/SKILL.md".into(),
            },
        ]
    }

    #[test]
    fn an_exact_name_returns_the_body() {
        let out = lookup(&fixtures(), "git-commit");
        assert!(out.contains("本文A"), "本文が返っていない: {out}");
        assert!(!out.contains("本文B"));
    }

    #[test]
    fn a_query_returns_names_and_descriptions_not_bodies() {
        let out = lookup(&fixtures(), "コミット");
        assert!(out.contains("git-commit"));
        assert!(!out.contains("本文A"), "検索で本文まで返してはいけない: {out}");
    }

    #[test]
    fn a_query_matching_nothing_says_so_and_lists_what_exists() {
        let out = lookup(&fixtures(), "まったく無関係な語");
        assert!(out.contains("git-commit") && out.contains("writing-style"));
    }

    #[test]
    fn an_empty_skill_set_says_so() {
        let out = lookup(&[], "何か");
        assert!(!out.is_empty(), "空を返してはいけない");
    }
}
```

2 つ目が要点である。検索で本文まで返すと、段階的開示の意味が消える。3 つ目は、何も当たらなかったときに沈黙せず、存在するものを見せることを固定する。空の応答は「skill が無い」と「見つからなかった」を区別できない。

- [ ] **Step 2: テストが失敗することを確認する**

Run: `cargo test -p polaris-tools skill`
Expected: コンパイルエラー。`lookup` が未定義

- [ ] **Step 3: 依存を足す**

`crates/polaris-tools/Cargo.toml` の `[dependencies]` へ追加する。

```toml
polaris-skills = { path = "../polaris-skills" }
```

- [ ] **Step 4: 実装を書く**

`crates/polaris-tools/src/skill.rs` の先頭に置く。

```rust
//! skill ツール。名前に完全一致すれば本文を、そうでなければ候補の一覧を返す。

use polaris_skills::Skill;

/// 与えられた語を skill 名として引き、外れたら名前と説明を検索する。
///
/// 検索が本文を返さないのは段階的開示のためである。候補を見てから読むかを
/// 決められるようにする。すべての本文を返すなら検索する意味が無い。
pub fn lookup(skills: &[Skill], q: &str) -> String {
    if skills.is_empty() {
        return "skill が 1 件も見つからない。探索先に SKILL.md が無い。".to_string();
    }

    if let Some(s) = skills.iter().find(|s| s.name == q) {
        return format!("# {}\n\n{}\n", s.name, s.body);
    }

    let needle = q.to_lowercase();
    let hits: Vec<&Skill> = skills
        .iter()
        .filter(|s| {
            s.name.to_lowercase().contains(&needle)
                || s.description.to_lowercase().contains(&needle)
        })
        .collect();

    if hits.is_empty() {
        let mut out = format!("{q} に当たる skill が無い。利用できるのは次のとおり。\n");
        for s in skills {
            out.push_str(&format!("- {}: {}\n", s.name, s.description));
        }
        return out;
    }

    let mut out = String::from("候補。本文が要るときは名前をそのまま渡す。\n");
    for s in hits {
        out.push_str(&format!("- {}: {}\n", s.name, s.description));
    }
    out
}
```

`crates/polaris-tools/src/lib.rs` へ `pub mod skill;` を追加し、`all_specs` を次のようにする。

```rust
pub fn all_specs() -> Vec<ToolSpec> {
    vec![read_spec(), skill_spec()]
}

fn skill_spec() -> ToolSpec {
    ToolSpec {
        name: "skill",
        description: "skill を引く。名前に完全一致すれば本文を返し、そうでなければ候補の名前と説明を返す。",
        parameters: serde_json::json!({
            "type": "object",
            "properties": { "q": { "type": "string" } },
            "required": ["q"]
        }),
    }
}
```

- [ ] **Step 5: テストが通ることを確認する**

Run: `cargo test -p polaris-tools`
Expected: 全て PASS

- [ ] **Step 6: 予算を再計測する**

Run: `cargo test -p polaris-core budget`
Expected: PASS。`always_on_context_stays_within_budget` は 990 以下を保つ

ツールが 1 本から 2 本になったため常時コンテキストが増える。増加後の下限と最悪ケースを実測して報告に書く。直前の値は下限 166、最悪ケース 342 である。`MAX_TOOLS` は 6 なので本数は問題ない。

- [ ] **Step 7: 検索が本文を返さないことを変異で確かめる**

検索経路の `format!` を本文込みに変え、`a_query_returns_names_and_descriptions_not_bodies` が落ちることを確認して戻す。観測した出力を報告に書く。

- [ ] **Step 8: コミットする**

```bash
git add crates/polaris-tools docs/filemap.md
git commit -F - <<'MSG'
feat(tools): add the skill tool

名前に完全一致すれば本文を、そうでなければ候補の名前と説明を返す。検索が
本文を返さないのは段階的開示のためで、候補を見てから読むかを決められる
ようにする。すべての本文を返すなら検索する意味が無い。

何も当たらなかった場合は沈黙せず、存在する skill を並べる。空の応答は
「skill が無い」と「見つからなかった」を区別できない。

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

### Task 5: エージェントループへの接続

**Files:**
- Modify: `crates/polaris-core/src/agent.rs`
- Modify: `crates/polaris-core/Cargo.toml`
- Modify: `crates/polaris-cli/src/main.rs`

**Interfaces:**
- Consumes: `polaris_tools::skill::lookup`、`polaris_skills::{Skill, discover}`、`polaris_core::config::load`
- Produces: `polaris_core::agent::run(provider, session, audit, stop, system, skills: &[Skill])`

- [ ] **Step 1: 失敗するテストを書く**

`crates/polaris-core/src/agent.rs` のテストモジュールへ追加する。

```rust
    #[tokio::test]
    async fn dispatches_the_skill_tool() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let skills = vec![polaris_skills::Skill {
            name: "demo".into(),
            description: "説明".into(),
            body: "デモ本文".into(),
            path: "/x/demo/SKILL.md".into(),
        }];

        let p = Scripted {
            replies: Mutex::new(vec![
                CompletionResponse {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "skill".into(),
                        arguments: serde_json::json!({ "q": "demo" }),
                    }],
                },
                CompletionResponse { text: "読んだ".into(), tool_calls: vec![] },
            ]),
        };

        let mut session = Session::new();
        session.push_user("demo の本文を読んで");
        let mut audit = AuditLog::open(&dir.path().join("audit.jsonl")).expect("開けない");
        let mut stop = StopTracker::new(10);
        let system = crate::prompt::build_system("", "");

        let out = run(&p, &mut session, &mut audit, &mut stop, &system, &skills)
            .await
            .expect("失敗");
        assert_eq!(out, "読んだ");

        let tool_msg = session
            .messages
            .iter()
            .find(|m| m.tool_call_id.is_some())
            .expect("ツール結果が積まれていない");
        assert!(tool_msg.content.contains("デモ本文"), "本文が渡っていない");
    }

    #[tokio::test]
    async fn the_skill_tool_reports_when_no_skills_are_loaded() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let p = Scripted {
            replies: Mutex::new(vec![
                CompletionResponse {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "skill".into(),
                        arguments: serde_json::json!({ "q": "何か" }),
                    }],
                },
                CompletionResponse { text: "了解".into(), tool_calls: vec![] },
            ]),
        };
        let mut session = Session::new();
        session.push_user("skill を探して");
        let mut audit = AuditLog::open(&dir.path().join("audit.jsonl")).expect("開けない");
        let mut stop = StopTracker::new(10);
        let system = crate::prompt::build_system("", "");

        run(&p, &mut session, &mut audit, &mut stop, &system, &[])
            .await
            .expect("失敗");

        let tool_msg = session
            .messages
            .iter()
            .find(|m| m.tool_call_id.is_some())
            .expect("ツール結果が積まれていない");
        assert!(!tool_msg.content.is_empty(), "空の結果を返してはいけない");
    }
```

- [ ] **Step 2: テストが失敗することを確認する**

Run: `cargo test -p polaris-core agent`
Expected: コンパイルエラー。`run` の引数が合わない

- [ ] **Step 3: 依存を足す**

`crates/polaris-core/Cargo.toml` の `[dependencies]` へ追加する。

```toml
polaris-skills = { path = "../polaris-skills" }
```

- [ ] **Step 4: 実装を書く**

`crates/polaris-core/src/agent.rs` の `run` に `skills: &[polaris_skills::Skill]` を最後の引数として足し、`dispatch` を次のようにする。

```rust
fn dispatch(
    call: &polaris_provider::ToolCall,
    skills: &[polaris_skills::Skill],
) -> Result<String, String> {
    match call.name.as_str() {
        "read" => {
            let path = call.arguments["path"]
                .as_str()
                .ok_or_else(|| "path が無い".to_string())?;
            let offset = call.arguments["offset"].as_u64().unwrap_or(0) as usize;
            let limit = call.arguments["limit"].as_u64().unwrap_or(2000) as usize;
            polaris_tools::read::read(std::path::Path::new(path), offset, limit)
                .map_err(|e| e.to_string())
        }
        "skill" => {
            let q = call.arguments["q"]
                .as_str()
                .ok_or_else(|| "q が無い".to_string())?;
            Ok(polaris_tools::skill::lookup(skills, q))
        }
        other => Err(format!("未知のツール: {other}")),
    }
}
```

`run` の中の `dispatch(call)` を `dispatch(call, skills)` に変える。

`crates/polaris-cli/src/main.rs` で、`agent::run` を呼ぶ前に skill を読み込み、最後の引数として渡す。

```rust
    let config = polaris_core::config::load(&cwd).unwrap_or_else(|e| {
        eprintln!("設定を読めない: {e}");
        polaris_core::config::Config::default()
    });
    let skills = polaris_skills::discover(&cwd, &config.skills_paths);
```

設定が壊れている場合に空へ潰さず標準エラーへ出すのは、Task 1 の判断と同じ理由による。読めなかったことを利用者へ届ける。

- [ ] **Step 5: テストが通ることを確認する**

Run: `cargo test --workspace`
Expected: 全て PASS

- [ ] **Step 6: 監査ログに skill 呼び出しが残ることを確かめる**

全ツール呼び出しが記録されるという不変条件に、新しいツールも含まれることを固定する。
`dispatches_the_skill_tool` の末尾へ次を追加する。

```rust
        let log = std::fs::read_to_string(dir.path().join("audit.jsonl")).expect("読めない");
        let skill_lines = log
            .lines()
            .filter(|l| l.contains("\"tool\":\"skill\""))
            .count();
        assert_eq!(skill_lines, 1, "skill の呼び出しが記録されていない: {log}");
```

Run: `cargo test -p polaris-core agent`
Expected: PASS

- [ ] **Step 7: コミットする**

```bash
git add crates/polaris-core crates/polaris-cli docs/filemap.md
git commit -F - <<'MSG'
feat(core): wire the skill tool into the loop

エージェントループが skill ツールを配送できるようにし、CLI が起動時に
設定と skill を読み込んで渡すようにした。

設定が壊れている場合は空へ潰さず標準エラーへ出す。読めなかったことと
書かれていなかったことは違う。

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

## 仕様との差分

仕様は「起動時と CI で `skills-ref validate` にかけ、仕様違反を検出する」と定めている。本計画は
検証を Rust 側で実装し、外部ツールを呼ばない。理由は 2 つある。起動経路に外部コマンドの実行を
持ち込むと、そのコマンドが無い環境で harness が動かなくなる。そして検証の結果を skill 単位で
握り潰す判断（1 件の破損で全体を止めない）は、外部ツールの終了コードでは表現できない。

`skills-ref validate` は開発時の検査として CI に置く価値が残るため、M3b の計画で扱う。

## M3a の完了条件

- `cargo test --workspace` が全て通る
- `cargo clippy --workspace --all-targets -- -D warnings` が通る
- 常時コンテキストがツール 2 本で 990 トークン以下に収まり、実測値が報告されている
- `~/.polaris/skills/` に置いた skill を `skill` ツールで名前引きできる
- 壊れた skill が 1 件あっても他の skill が読める
- 壊れた設定ファイルが黙って無視されない
- 監査ログに `skill` の呼び出しが記録される
