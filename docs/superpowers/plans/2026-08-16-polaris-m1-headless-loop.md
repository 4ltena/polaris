# polaris M1 ヘッドレス最小ループ Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `read` ツールだけを持つヘッドレスエージェントを動かし、常時コンテキスト 990 トークン以下という不変条件をテストで固定する。

**Architecture:** Cargo workspace に `polaris-tools` / `polaris-provider` / `polaris-core` / `polaris-cli` の 4 クレートを置く。ツール定義は JSON Schema として直列化し、その実バイト列をトークナイザで数えて予算テストに掛ける。プロバイダは非同期のトレイトとし、SSE の解析は commons の `sse-decoder` を取り込む。全ツール呼び出しは追記専用 JSONL へ記録し、記録前に commons の `secret-screen` を通す。

**Tech Stack:** Rust 1.96.0 / edition 2024、tokio、reqwest、serde、tiktoken-rs 0.12、clap、thiserror、async-trait、wiremock(dev)、tempfile(dev)

**Spec:** `docs/superpowers/specs/2026-08-16-polaris-harness-design.md`

## Global Constraints

- Rust edition は `2024`、`rust-toolchain.toml` の channel は `1.96.0`、components に `rustfmt` と `clippy` を含める
- 常時コンテキストは 990 トークン以下。計測の基準トークナイザは `tiktoken_rs::o200k_base()` とする
- 常時提供するツールは 6 本を超えない
- 憲法ブロックは 150 トークンを超えない。AGENTS.md の全文は常時コンテキストへ載せない
- 常時コンテキストの合計は AGENTS.md の大きさに左右されない。超過分は切り詰める
- クレート名の接頭辞は `polaris-`
- 監査ログへ書く文字列は、書く直前に必ず `secret_screen::screen_text` を通す
- commons から取り込んだファイルは取り込み後に改変してよい。改変は元へ戻さない
- コミットのタイトルは英語、本文は日本語可。末尾に空行 1 行を挟んで `Co-Authored-By: Claude <noreply@anthropic.com>` を付ける

## 仕様からの逸脱

仕様では監査ログを `polaris-agents` の責務としているが、監査ログは subagent に限らず全ツール呼び出しを対象とする。M1 には `polaris-agents` が存在しないため、`polaris-core` の `audit` モジュールへ置く。仕様側もこの配置へ改める。

## マイルストーン地図

この計画は M1 のみを対象とする。M2 以降はそれぞれ別の計画として書く。

| | 内容 | 単体で動くもの |
| --- | --- | --- |
| M1 | 本計画。read ツール、1 プロバイダ、予算テスト、監査ログ、停止条件 | ファイルを読んで答えるヘッドレスエージェント |
| M2 | `write` / `edit` / `bash` と `polaris-sandbox`。宣言外書き込みの実サンドボックス拒否テスト。M1 と本 M2 の完了をもって `v1.0.0`（hamal）とする | 編集とコマンド実行ができる |
| M3 | `polaris-skills` と `polaris-router`。`atomic-file-replace` の取り込み | skill が自動で添付される |
| M4 | `polaris-agents`。`spawn`、波、継続波、path claim | subagent の並列実行 |
| M5 | `polaris-tui` と圧縮、マルチプロバイダとフォールバック、セッションの追記永続化と再開 | 対話型の日常ドライバ |

## ファイル構成

```
polaris/
├── Cargo.toml                              workspace 定義と共通依存
├── rust-toolchain.toml                     1.96.0 固定
├── .gitignore
└── crates/
    ├── polaris-tools/
    │   ├── Cargo.toml
    │   └── src/
    │       ├── lib.rs                      ToolSpec、ToolError、all_specs
    │       ├── path_policy.rs              読み取り拒否パスの判定
    │       └── read.rs                     read ツールの実装
    ├── polaris-provider/
    │   ├── Cargo.toml
    │   └── src/
    │       ├── lib.rs                      Provider トレイトと要求・応答の型
    │       ├── sse.rs                      commons/sse-decoder を取り込む
    │       └── openai.rs                   OpenAI 互換の実装
    ├── polaris-core/
    │   ├── Cargo.toml
    │   └── src/
    │       ├── lib.rs                      モジュール宣言
    │       ├── prompt.rs                   システムプロンプトと組み立て
    │       ├── constitution.rs             AGENTS.md 憲法ブロックと環境情報
    │       ├── budget.rs                   トークン計測
    │       ├── audit.rs                    追記専用 JSONL
    │       ├── secret_screen/mod.rs        commons/secret-screen を取り込む
    │       ├── stop.rs                     停止条件
    │       ├── session.rs                  メッセージ履歴
    │       └── agent.rs                    エージェントループ
    └── polaris-cli/
        ├── Cargo.toml
        └── src/main.rs                     一発実行の入口
```

---

### Task 1: workspace 骨格とツール定義

**Files:**
- Create: `Cargo.toml`
- Create: `rust-toolchain.toml`
- Create: `.gitignore`
- Create: `crates/polaris-tools/Cargo.toml`
- Create: `crates/polaris-tools/src/lib.rs`
- Test: `crates/polaris-tools/src/lib.rs` の `#[cfg(test)]` モジュール

**Interfaces:**
- Consumes: なし
- Produces: `polaris_tools::ToolSpec { name: &'static str, description: &'static str, parameters: serde_json::Value }`、`polaris_tools::all_specs() -> Vec<ToolSpec>`、`polaris_tools::ToolError`

- [ ] **Step 1: workspace とクレートの骨格を作る**

テストを走らせるには workspace が読める状態である必要がある。先に骨格を置く。
この時点で `crates/polaris-tools/src/lib.rs` は空のファイルとして作る。

`Cargo.toml`

```toml
[workspace]
resolver = "3"
members = ["crates/*"]

[workspace.package]
edition = "2024"
rust-version = "1.96"

[workspace.dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
thiserror = "2"
tokio = { version = "1", features = ["rt-multi-thread", "macros", "fs", "io-util"] }
reqwest = { version = "0.12", default-features = false, features = ["json", "stream", "rustls-tls"] }
async-trait = "0.1"
tiktoken-rs = "0.12"
regex = "1"
clap = { version = "4", features = ["derive"] }
wiremock = "0.6"
tempfile = "3"
```

`rust-toolchain.toml`

```toml
[toolchain]
channel = "1.96.0"
components = ["rustfmt", "clippy"]
profile = "default"
```

`.gitignore`

```
/target
CLAUDE.md
AGENTS.md
.claude/
.codex/
```

`crates/polaris-tools/Cargo.toml`

```toml
[package]
name = "polaris-tools"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true

[dependencies]
serde = { workspace = true }
serde_json = { workspace = true }
thiserror = { workspace = true }
```
- [ ] **Step 2: 失敗するテストを書く**

`crates/polaris-tools/src/lib.rs` の末尾に置く。

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_spec_serializes_with_required_path() {
        let specs = all_specs();
        let read = specs.iter().find(|s| s.name == "read").expect("read が無い");
        let json = serde_json::to_value(read).expect("直列化できない");
        assert_eq!(json["name"], "read");
        assert_eq!(json["parameters"]["required"][0], "path");
        assert_eq!(json["parameters"]["properties"]["path"]["type"], "string");
    }

    #[test]
    fn all_specs_has_unique_names() {
        let specs = all_specs();
        let mut names: Vec<&str> = specs.iter().map(|s| s.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "ツール名が重複している");
    }
}
```

- [ ] **Step 3: テストが失敗することを確認する**

Run: `cargo test -p polaris-tools`
Expected: コンパイルエラー。`all_specs` と `ToolSpec` が未定義

- [ ] **Step 4: 最小の実装を書く**

`crates/polaris-tools/src/lib.rs` の先頭に置く。

```rust
//! polaris の組込みツール。常時提供するツールは 6 本を超えない。

use serde::Serialize;

/// モデルへ渡すツール定義。`parameters` は JSON Schema。
#[derive(Debug, Clone, Serialize)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: serde_json::Value,
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("パス {0} は読み取りを許可されていない")]
    PathDenied(String),
    #[error("入出力エラー: {0}")]
    Io(#[from] std::io::Error),
}

/// 常時提供するツールの一覧。
pub fn all_specs() -> Vec<ToolSpec> {
    vec![read_spec()]
}

fn read_spec() -> ToolSpec {
    ToolSpec {
        name: "read",
        description: "ファイルを読む。行番号付きで返す。offset と limit で範囲を指定できる。",
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "offset": { "type": "integer" },
                "limit": { "type": "integer" }
            },
            "required": ["path"]
        }),
    }
}
```

- [ ] **Step 5: テストが通ることを確認する**

Run: `cargo test -p polaris-tools`
Expected: 2 件とも PASS

- [ ] **Step 6: コミットする**

```bash
git add Cargo.toml rust-toolchain.toml .gitignore crates/polaris-tools
git commit -F - <<'MSG'
feat(tools): add ToolSpec and read tool schema

workspace の骨格と polaris-tools を置く。ツール定義は JSON Schema として
直列化し、この実バイト列を後続タスクの予算計測が数える。

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

### Task 2: 常時コンテキスト予算テスト

**Files:**
- Create: `crates/polaris-core/Cargo.toml`
- Create: `crates/polaris-core/src/lib.rs`
- Create: `crates/polaris-core/src/prompt.rs`
- Create: `crates/polaris-core/src/budget.rs`
- Test: `crates/polaris-core/src/budget.rs` の `#[cfg(test)]` モジュール

**Interfaces:**
- Consumes: `polaris_tools::{ToolSpec, all_specs}`
- Produces: `polaris_core::budget::count_tokens(&str) -> usize`、`polaris_core::budget::always_on_tokens(&str, &[ToolSpec]) -> usize`、`polaris_core::prompt::SYSTEM_PROMPT: &str`、`polaris_core::budget::BUDGET_LIMIT: usize`、`polaris_core::budget::MAX_TOOLS: usize`

- [ ] **Step 1: クレートの骨格を作る**

`Cargo.toml` と `lib.rs` をここで作成する。`budget.rs` と `prompt.rs` は
この時点では空のファイルとして作る。

`crates/polaris-core/Cargo.toml`

```toml
[package]
name = "polaris-core"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true

[dependencies]
polaris-tools = { path = "../polaris-tools" }
serde = { workspace = true }
serde_json = { workspace = true }
thiserror = { workspace = true }
tiktoken-rs = { workspace = true }
```

`crates/polaris-core/src/lib.rs`

```rust
pub mod budget;
pub mod prompt;
```
- [ ] **Step 2: 失敗するテストを書く**

`crates/polaris-core/src/budget.rs` の末尾に置く。

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::SYSTEM_PROMPT;

    #[test]
    fn always_on_context_stays_within_budget() {
        let specs = polaris_tools::all_specs();
        let n = always_on_tokens(SYSTEM_PROMPT, &specs);
        assert!(
            n <= BUDGET_LIMIT,
            "常時コンテキストが {n} トークン。上限 {BUDGET_LIMIT} を超えている"
        );
    }

    #[test]
    fn tool_count_stays_within_limit() {
        let n = polaris_tools::all_specs().len();
        assert!(n <= MAX_TOOLS, "ツールが {n} 本。上限 {MAX_TOOLS} 本を超えている");
    }

    #[test]
    fn count_tokens_is_nonzero_for_nonempty_text() {
        assert!(count_tokens("hello world") > 0);
        assert_eq!(count_tokens(""), 0);
    }
}
```

- [ ] **Step 3: テストが失敗することを確認する**

Run: `cargo test -p polaris-core`
Expected: コンパイルエラー。`always_on_tokens` と `SYSTEM_PROMPT` が未定義

- [ ] **Step 4: 最小の実装を書く**

`crates/polaris-core/src/prompt.rs`

```rust
/// 常時載るシステムプロンプト。振る舞いの指示を削ると往復が増えて総コストが
/// 上がるため、短さのためにここを削らない。削る対象は構造の重複に限る。
pub const SYSTEM_PROMPT: &str = "\
You are polaris, a coding agent. Read files and answer with what the code actually does.

Rules:
- State file paths as path:line so they can be opened directly.
- Never guess file contents. Read them.
- If the same error occurs three times in a row, stop and report it.
- Do not claim work is done without showing the command output that proves it.
";
```

`crates/polaris-core/src/budget.rs` の先頭に置く。

```rust
//! 常時コンテキストの計測。数値は測定で担保し、見積で運用しない。

use polaris_tools::ToolSpec;

/// 常時コンテキストの上限。
pub const BUDGET_LIMIT: usize = 990;

/// 常時提供するツールの上限本数。
pub const MAX_TOOLS: usize = 6;

/// 基準トークナイザで数える。プロバイダごとに実数は前後するため、
/// 予算の判定は常にこの基準で行う。
pub fn count_tokens(text: &str) -> usize {
    let bpe = tiktoken_rs::o200k_base().expect("o200k_base を読めない");
    bpe.encode_with_special_tokens(text).len()
}

/// 毎ターン載るものの合計。システムプロンプトと、実際に送られる
/// ツール定義の直列化結果を数える。
pub fn always_on_tokens(system_prompt: &str, tools: &[ToolSpec]) -> usize {
    let tools_json = serde_json::to_string(tools).expect("ツール定義を直列化できない");
    count_tokens(system_prompt) + count_tokens(&tools_json)
}
```

- [ ] **Step 5: テストが通ることを確認する**

Run: `cargo test -p polaris-core`
Expected: 3 件とも PASS。予算超過なら実測値がメッセージに出るので、`SYSTEM_PROMPT` を削って収める

- [ ] **Step 6: コミットする**

```bash
git add crates/polaris-core
git commit -F - <<'MSG'
test(core): enforce always-on context budget

システムプロンプトと実際に直列化したツール定義を基準トークナイザで数え、
990 トークンと 6 本の上限をテストで固定する。見積のまま運用すると必ず
膨らむため、数値を不変条件として置く。

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

### Task 3: 憲法ブロックと環境情報

仕様は常時コンテキストに「AGENTS.md 憲法ブロック 150 トークン」を計上している。
ここを実装しないまま予算テストを通すと、実態より軽い値で合格し、後から憲法を
足した瞬間に上限を破る。あわせて、モデルが 1 ターン使って調べる環境の事実も
先に渡す。

読む対象はグローバル規則 `~/.polaris/AGENTS.md` とプロジェクト規則
`<project-root>/AGENTS.md` の 2 つ。この順に連結し、合算してから上限で切り詰める。

**Files:**
- Create: `crates/polaris-core/src/constitution.rs`
- Modify: `crates/polaris-core/src/prompt.rs`
- Modify: `crates/polaris-core/src/lib.rs`
- Modify: `crates/polaris-core/Cargo.toml`
- Test: `crates/polaris-core/src/constitution.rs` の `#[cfg(test)]` モジュール

**Interfaces:**
- Consumes: `polaris_core::budget::{count_tokens, always_on_tokens, BUDGET_LIMIT}`、`polaris_core::prompt::SYSTEM_PROMPT`
- Produces: `polaris_core::constitution::CONSTITUTION_LIMIT: usize`、`constitution::extract_always_on(&str) -> String`、`constitution::load(project_root: &std::path::Path) -> String`、`constitution::load_from(global_agents: Option<&std::path::Path>, project_root: &std::path::Path) -> String`、`constitution::environment_block(cwd: &std::path::Path, branch: Option<&str>) -> String`、`polaris_core::prompt::build_system(constitution: &str, environment: &str) -> String`

- [ ] **Step 1: 失敗するテストを書く**

`crates/polaris-core/src/constitution.rs` の末尾に置く。

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const MARKED: &str = "\
# AGENTS

前置き。ここは載せない。

<!-- polaris:always-on -->
main へ直接 push しない。
<!-- /polaris:always-on -->

## 詳細
長い手続き。ここも載せない。
";

    const HEADING: &str = "\
# AGENTS

## Always on

main へ直接 push しない。

## Skill routing

長い手続き。ここは載せない。
";

    #[test]
    fn extracts_marked_block_only() {
        let got = extract_always_on(MARKED);
        assert_eq!(got, "main へ直接 push しない。");
    }

    #[test]
    fn falls_back_to_always_on_heading_and_stops_at_next_section() {
        let got = extract_always_on(HEADING);
        assert_eq!(got, "main へ直接 push しない。");
        assert!(!got.contains("Skill routing"));
    }

    #[test]
    fn returns_empty_without_marker_or_heading() {
        assert_eq!(extract_always_on("# AGENTS\n\n本文だけ。\n"), "");
    }

    #[test]
    fn merges_global_then_project_rules() {
        let g = tempfile::tempdir().expect("一時ディレクトリ");
        let pj = tempfile::tempdir().expect("一時ディレクトリ");
        std::fs::write(
            g.path().join("AGENTS.md"),
            "## Always on\n\n日本語で応答する。\n",
        )
        .expect("書けない");
        std::fs::write(
            pj.path().join("AGENTS.md"),
            "## Always on\n\nmain へ直接 push しない。\n",
        )
        .expect("書けない");

        let got = load_from(Some(&g.path().join("AGENTS.md")), pj.path());
        assert!(got.contains("日本語で応答する。"), "グローバル規則が落ちている");
        assert!(got.contains("main へ直接 push しない。"), "プロジェクト規則が落ちている");
        let gi = got.find("日本語").expect("グローバルが無い");
        let pi = got.find("main へ").expect("プロジェクトが無い");
        assert!(gi < pi, "グローバルが先に来ていない");
    }

    #[test]
    fn caps_oversized_constitution() {
        let mut body = String::from("<!-- polaris:always-on -->\n");
        for i in 0..500 {
            body.push_str(&format!("規則 {i}: 長い行をここに書き連ねる。\n"));
        }
        body.push_str("<!-- /polaris:always-on -->\n");

        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        std::fs::write(dir.path().join("AGENTS.md"), &body).expect("書けない");

        let got = load(dir.path());
        assert!(
            count_tokens(&got) <= CONSTITUTION_LIMIT,
            "切り詰められていない: {} トークン",
            count_tokens(&got)
        );
        assert!(!got.is_empty(), "全部捨ててはいけない");
    }

    #[test]
    fn returns_empty_when_neither_file_exists() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        assert_eq!(load_from(None, dir.path()), "");
    }

    #[test]
    fn environment_block_carries_cwd_and_branch() {
        let got = environment_block(Path::new("/w/polaris"), Some("feat/x"));
        assert!(got.contains("/w/polaris"));
        assert!(got.contains("feat/x"));
    }

    #[test]
    fn full_always_on_context_stays_within_budget() {
        let constitution = "a".repeat(2000);
        let capped = cap(&constitution, CONSTITUTION_LIMIT);
        let env = environment_block(Path::new("/w/polaris"), Some("feat/m1-headless-loop"));
        let system = crate::prompt::build_system(&capped, &env);

        let n = crate::budget::always_on_tokens(&system, &polaris_tools::all_specs());
        assert!(
            n <= crate::budget::BUDGET_LIMIT,
            "憲法と環境を含めた常時コンテキストが {n} トークン。上限を超えている"
        );
    }
}
```

- [ ] **Step 2: テストが失敗することを確認する**

Run: `cargo test -p polaris-core constitution`
Expected: コンパイルエラー。`extract_always_on` と `load` が未定義

- [ ] **Step 3: dev 依存を足す**

`crates/polaris-core/Cargo.toml` へ追加する。

```toml
[dev-dependencies]
tempfile = { workspace = true }
```

- [ ] **Step 4: 最小の実装を書く**

`crates/polaris-core/src/constitution.rs` の先頭に置く。

```rust
//! 常時載る文脈のうち、ハーネスが所有しない部分。AGENTS.md の全文は載せない。
//! 上限で切り詰めるため、AGENTS.md がどれだけ大きくても予算は破れない。

use std::path::Path;

use crate::budget::count_tokens;

/// 憲法ブロックに許すトークン数の上限。
pub const CONSTITUTION_LIMIT: usize = 150;

const BEGIN: &str = "<!-- polaris:always-on -->";
const END: &str = "<!-- /polaris:always-on -->";

/// AGENTS.md から常時載せる部分だけを取り出す。
///
/// マーカーで囲まれていればその内側を返す。マーカーが無ければ `## Always on`
/// 見出しの節を次の `## ` の手前まで返す。どちらも無ければ空文字列を返す。
/// 全文を返す経路は存在しない。
pub fn extract_always_on(markdown: &str) -> String {
    if let Some(start) = markdown.find(BEGIN) {
        let after = start + BEGIN.len();
        if let Some(rel) = markdown[after..].find(END) {
            return markdown[after..after + rel].trim().to_string();
        }
    }

    let mut out: Vec<&str> = Vec::new();
    let mut inside = false;
    for line in markdown.lines() {
        if inside {
            if line.starts_with("## ") {
                break;
            }
            out.push(line);
        } else if line.starts_with("## ") && line[3..].trim() == "Always on" {
            inside = true;
        }
    }
    out.join("\n").trim().to_string()
}

/// 上限を超えていたら行単位で切り詰める。全部落とすことはしない。
fn cap(text: &str, limit: usize) -> String {
    if count_tokens(text) <= limit {
        return text.to_string();
    }
    let mut kept: Vec<&str> = Vec::new();
    for line in text.lines() {
        let mut trial = kept.clone();
        trial.push(line);
        if count_tokens(&trial.join("\n")) > limit {
            break;
        }
        kept.push(line);
    }
    if kept.is_empty() {
        // 1 行目だけで上限を超える場合は、文字単位で落として先頭を残す。
        let mut s: String = text.lines().next().unwrap_or_default().to_string();
        while count_tokens(&s) > limit && !s.is_empty() {
            s.truncate(s.len() - s.chars().last().map_or(0, |c| c.len_utf8()));
        }
        return s;
    }
    kept.join("\n")
}

fn read_block(path: &Path) -> String {
    std::fs::read_to_string(path)
        .map(|b| extract_always_on(&b))
        .unwrap_or_default()
}

/// グローバル規則とプロジェクト規則をこの順に読み、合算して上限で切り詰める。
/// どちらが欠けていても失敗ではない。テストから経路を固定できるよう、
/// グローバル側のパスは引数で受ける。
pub fn load_from(global_agents: Option<&Path>, project_root: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(g) = global_agents {
        let b = read_block(g);
        if !b.is_empty() {
            parts.push(b);
        }
    }
    let b = read_block(&project_root.join("AGENTS.md"));
    if !b.is_empty() {
        parts.push(b);
    }
    cap(&parts.join("\n"), CONSTITUTION_LIMIT)
}

/// `~/.polaris/AGENTS.md` をグローバル規則として解決してから読む。
pub fn load(project_root: &Path) -> String {
    let global = std::env::var_os("HOME").map(|h| Path::new(&h).join(".polaris").join("AGENTS.md"));
    load_from(global.as_deref(), project_root)
}

/// 環境情報。モデルが 1 ターン使って調べる事実を先に渡す。
pub fn environment_block(cwd: &Path, branch: Option<&str>) -> String {
    let mut s = format!("cwd: {}", cwd.display());
    if let Some(b) = branch {
        s.push_str(&format!("\ngit branch: {b}"));
    }
    s
}
```

`crates/polaris-core/src/prompt.rs` の末尾へ追加する。

```rust
/// 常時載る文脈を組み立てる。空の節は見出しごと落とす。
///
/// 組み立て結果はセッションを通して同一でなければならない。ここが毎ターン
/// 変わるとプロンプトキャッシュの接頭辞が動き、履歴全体が未キャッシュ扱いになる。
pub fn build_system(constitution: &str, environment: &str) -> String {
    let mut s = String::from(SYSTEM_PROMPT);
    if !constitution.is_empty() {
        s.push_str("\n## Project rules\n");
        s.push_str(constitution);
        s.push('\n');
    }
    if !environment.is_empty() {
        s.push_str("\n## Environment\n");
        s.push_str(environment);
        s.push('\n');
    }
    s
}
```

`crates/polaris-core/src/lib.rs` へ追加する。

```rust
pub mod constitution;
```

`crates/polaris-core/Cargo.toml` の `[dependencies]` に `polaris-tools` が無ければ追加する。

- [ ] **Step 5: テストが通ることを確認する**

Run: `cargo test -p polaris-core`
Expected: 全て PASS。最後のテストが、憲法と環境を含めた状態でも上限内であることを示す

- [ ] **Step 6: コミットする**

```bash
git add crates/polaris-core
git commit -F - <<'MSG'
feat(core): load capped constitution block and environment

AGENTS.md の全文ではなく、マーカーで囲まれた部分か `## Always on` の節だけを
常時コンテキストへ載せる。150 トークンの上限で切り詰めるため、AGENTS.md が
どれだけ大きくなっても 990 の予算は破れない。

環境情報を先に渡すのは、モデルが cwd やブランチを調べるだけで 1 往復
使うのを避けるため。往復 1 回のコストは履歴全体であり、常時コンテキストの
数十トークンより桁で大きい。

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

### Task 4: 読み取りパス方針

**Files:**
- Create: `crates/polaris-tools/src/path_policy.rs`
- Modify: `crates/polaris-tools/src/lib.rs`（`pub mod path_policy;` を追加）
- Test: `crates/polaris-tools/src/path_policy.rs` の `#[cfg(test)]` モジュール

**Interfaces:**
- Consumes: なし
- Produces: `polaris_tools::path_policy::is_denied(&std::path::Path) -> bool`

- [ ] **Step 1: 失敗するテストを書く**

`crates/polaris-tools/src/path_policy.rs` の末尾に置く。

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn denies_secret_bearing_paths() {
        for p in [
            "/home/u/proj/.env",
            "/home/u/proj/.env.local",
            "/home/u/.ssh/id_ed25519",
            "/home/u/.ssh/known_hosts",
            "/home/u/.gnupg/secring.gpg",
            "/home/u/.aws/credentials",
            "/home/u/key.pem",
            "/home/u/cert.pub",
            "/home/u/Library/Keychains/login.keychain-db",
        ] {
            assert!(is_denied(Path::new(p)), "{p} は拒否されるべき");
        }
    }

    #[test]
    fn allows_ordinary_source_paths() {
        for p in [
            "/home/u/proj/src/main.rs",
            "/home/u/proj/Cargo.toml",
            "/home/u/proj/docs/env-setup.md",
            "/home/u/proj/environment.rs",
        ] {
            assert!(!is_denied(Path::new(p)), "{p} は許可されるべき");
        }
    }
}
```

`docs/env-setup.md` と `environment.rs` を許可側に置いているのは、`env` を含むだけの通常のファイルを巻き込まないことを固定するためである。

- [ ] **Step 2: テストが失敗することを確認する**

Run: `cargo test -p polaris-tools path_policy`
Expected: コンパイルエラー。`is_denied` が未定義

- [ ] **Step 3: 最小の実装を書く**

`crates/polaris-tools/src/path_policy.rs` の先頭に置く。

```rust
//! 読み取りを拒否するパスの判定。過検出より見逃しを避ける方向に倒す。

use std::path::Path;

/// パスの一部にこれらのディレクトリ名が現れたら拒否する。
const DENIED_DIRS: &[&str] = &[".ssh", ".gnupg", ".aws", "gcloud", "Keychains"];

/// ファイル名がこれらと完全一致したら拒否する。
const DENIED_NAMES: &[&str] = &[".env", "credentials", "id_rsa", "id_ed25519", "id_ecdsa"];

/// 拡張子がこれらなら拒否する。
const DENIED_EXTS: &[&str] = &["pem", "key", "p12", "pfx", "pub"];

pub fn is_denied(path: &Path) -> bool {
    for c in path.components() {
        let s = c.as_os_str().to_string_lossy();
        if DENIED_DIRS.iter().any(|d| s == *d) {
            return true;
        }
    }

    let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return false;
    };

    if DENIED_NAMES.iter().any(|n| name == *n) {
        return true;
    }
    // `.env.local` のような接尾辞付きも拒否する。`environment.rs` は巻き込まない。
    if name.starts_with(".env.") {
        return true;
    }
    if name.contains("keychain") {
        return true;
    }
    if let Some(ext) = path.extension().map(|e| e.to_string_lossy().into_owned()) {
        if DENIED_EXTS.iter().any(|e| ext == *e) {
            return true;
        }
    }
    false
}
```

`crates/polaris-tools/src/lib.rs` の先頭付近へ追加する。

```rust
pub mod path_policy;
```

- [ ] **Step 4: テストが通ることを確認する**

Run: `cargo test -p polaris-tools path_policy`
Expected: 2 件とも PASS

- [ ] **Step 5: コミットする**

```bash
git add crates/polaris-tools
git commit -F - <<'MSG'
feat(tools): deny reading secret-bearing paths

.env、秘密鍵、SSH と GnuPG、クラウド認証、キーチェーンを読み取りから外す。
環境変数の設定を説明する通常の文書まで巻き込まないよう、名前の完全一致と
.env. 接頭辞に限定してテストで固定する。

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

### Task 5: read ツール

**Files:**
- Create: `crates/polaris-tools/src/read.rs`
- Modify: `crates/polaris-tools/src/lib.rs`（`pub mod read;` を追加）
- Modify: `crates/polaris-tools/Cargo.toml`（`tempfile` を dev-dependencies へ追加）
- Test: `crates/polaris-tools/src/read.rs` の `#[cfg(test)]` モジュール

**Interfaces:**
- Consumes: `polaris_tools::path_policy::is_denied`、`polaris_tools::ToolError`
- Produces: `polaris_tools::read::read(path: &std::path::Path, offset: usize, limit: usize) -> Result<String, ToolError>`

- [ ] **Step 1: 失敗するテストを書く**

`crates/polaris-tools/src/read.rs` の末尾に置く。

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn fixture(lines: &[&str]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().expect("一時ファイルを作れない");
        for l in lines {
            writeln!(f, "{l}").expect("書き込めない");
        }
        f.flush().expect("flush できない");
        f
    }

    #[test]
    fn numbers_lines_from_one() {
        let f = fixture(&["alpha", "beta"]);
        let out = read(f.path(), 0, 100).expect("読めない");
        assert_eq!(out, "1\talpha\n2\tbeta\n");
    }

    #[test]
    fn honors_offset_and_limit() {
        let f = fixture(&["a", "b", "c", "d"]);
        let out = read(f.path(), 1, 2).expect("読めない");
        assert_eq!(out, "2\tb\n3\tc\n");
    }

    #[test]
    fn refuses_denied_paths() {
        let err = read(std::path::Path::new("/home/u/.ssh/id_rsa"), 0, 100)
            .expect_err("拒否されるべき");
        assert!(matches!(err, ToolError::PathDenied(_)));
    }
}
```

- [ ] **Step 2: テストが失敗することを確認する**

Run: `cargo test -p polaris-tools read`
Expected: コンパイルエラー。`read` が未定義

- [ ] **Step 3: 依存を足す**

`crates/polaris-tools/Cargo.toml` へ追加する。

```toml
[dev-dependencies]
tempfile = { workspace = true }
```

- [ ] **Step 4: 最小の実装を書く**

`crates/polaris-tools/src/read.rs` の先頭に置く。

```rust
//! read ツール。行番号を付けて返すのは、モデルが path:line で位置を示せるようにするため。

use std::path::Path;

use crate::{ToolError, path_policy};

/// `offset` は 0 起点の行番号、`limit` は返す行数。出力の行番号は 1 起点。
pub fn read(path: &Path, offset: usize, limit: usize) -> Result<String, ToolError> {
    if path_policy::is_denied(path) {
        return Err(ToolError::PathDenied(path.display().to_string()));
    }
    let body = std::fs::read_to_string(path)?;
    let mut out = String::new();
    for (i, line) in body.lines().enumerate().skip(offset).take(limit) {
        out.push_str(&format!("{}\t{}\n", i + 1, line));
    }
    Ok(out)
}
```

`crates/polaris-tools/src/lib.rs` へ追加する。

```rust
pub mod read;
```

- [ ] **Step 5: テストが通ることを確認する**

Run: `cargo test -p polaris-tools read`
Expected: 3 件とも PASS

- [ ] **Step 6: コミットする**

```bash
git add crates/polaris-tools
git commit -F - <<'MSG'
feat(tools): implement read with line numbers and path policy

行番号を 1 起点で付けるのは、モデルが path:line の形で位置を示せるように
するため。拒否パスは読み取りの前に弾き、エラーを ToolError::PathDenied として
呼び出し側へ返す。

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

### Task 6: 監査ログ

**Files:**
- Create: `crates/polaris-core/src/secret_screen/mod.rs`（commons から取り込む）
- Create: `crates/polaris-core/src/audit.rs`
- Modify: `crates/polaris-core/src/lib.rs`
- Modify: `crates/polaris-core/Cargo.toml`
- Test: `crates/polaris-core/src/audit.rs` の `#[cfg(test)]` モジュール

**Interfaces:**
- Consumes: なし
- Produces: `polaris_core::audit::AuditLog::open(&Path) -> std::io::Result<AuditLog>`、`AuditLog::record(&mut self, tool: &str, detail: &str) -> std::io::Result<()>`、`polaris_core::secret_screen::{screen_text, FilterResult}`

- [ ] **Step 1: commons からシークレット伏字化を取り込む**

```bash
python3 ~/.claude/skills/commons-catalog/scripts/commons.py use secret-screen \
  --into crates/polaris-core/src/secret_screen \
  --project codex/polaris --lang rust
mv crates/polaris-core/src/secret_screen/lib.rs crates/polaris-core/src/secret_screen/mod.rs
```

`use` は `src/` の接頭辞を落として複写するため、`crates/polaris-core/src/secret_screen/lib.rs` へ置かれる。Rust のモジュール解決に合わせて `mod.rs` へ改名する。取り込んだコードの改変は commons 側へ伝播しない。

- [ ] **Step 2: 失敗するテストを書く**

`crates/polaris-core/src/audit.rs` の末尾に置く。

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_secrets_before_writing() {
        let dir = tempfile::tempdir().expect("一時ディレクトリを作れない");
        let path = dir.path().join("audit.jsonl");
        let mut log = AuditLog::open(&path).expect("開けない");
        log.record("bash", "export API_TOKEN=sk-abcdefghijklmnopqrstuvwxyz012345")
            .expect("書けない");

        let body = std::fs::read_to_string(&path).expect("読めない");
        assert!(!body.contains("sk-abcdefghijklmnopqrstuvwxyz012345"), "生の値が残っている");
        assert!(body.contains("[REDACTED]"), "伏字化されていない");
        assert!(body.contains("\"tool\":\"bash\""), "ツール名が無い");
    }

    #[test]
    fn appends_one_line_per_record() {
        let dir = tempfile::tempdir().expect("一時ディレクトリを作れない");
        let path = dir.path().join("audit.jsonl");
        let mut log = AuditLog::open(&path).expect("開けない");
        log.record("read", "src/main.rs").expect("書けない");
        log.record("read", "src/lib.rs").expect("書けない");

        let body = std::fs::read_to_string(&path).expect("読めない");
        assert_eq!(body.lines().count(), 2);
    }
}
```

- [ ] **Step 3: テストが失敗することを確認する**

Run: `cargo test -p polaris-core audit`
Expected: コンパイルエラー。`AuditLog` が未定義

- [ ] **Step 4: 依存を足す**

`crates/polaris-core/Cargo.toml` へ追加する。

```toml
regex = { workspace = true }
```

`regex` は取り込んだ `secret_screen` が使う。`[dev-dependencies]` の `tempfile` は
憲法ブロックのタスクで既に追加されている。節を二重に作らず、無い場合だけ足す。

- [ ] **Step 5: 最小の実装を書く**

`crates/polaris-core/src/audit.rs` の先頭に置く。

```rust
//! 追記専用の監査ログ。署名は付けない。インプロセスでは署名する主体と
//! 行為する主体が同一であり、署名はログ以上のことを証明しないため。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;

use crate::secret_screen::{FilterResult, screen_text};

pub struct AuditLog {
    file: File,
}

impl AuditLog {
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self { file })
    }

    /// 1 呼び出しを 1 行として追記する。`detail` は書く直前に必ず伏字化を通す。
    pub fn record(&mut self, tool: &str, detail: &str) -> std::io::Result<()> {
        let screened = match screen_text(detail) {
            FilterResult::Keep(s) | FilterResult::Redacted(s) => s,
            FilterResult::Drop => "[DROPPED]".to_string(),
        };
        let line = serde_json::json!({ "tool": tool, "detail": screened });
        writeln!(self.file, "{line}")?;
        self.file.flush()
    }
}
```

`crates/polaris-core/src/lib.rs` へ追加する。

```rust
pub mod audit;
pub mod secret_screen;
```

- [ ] **Step 6: テストが通ることを確認する**

Run: `cargo test -p polaris-core`
Expected: 取り込んだ `secret_screen` の 20 件と audit の 2 件を含めて全て PASS

- [ ] **Step 7: コミットする**

```bash
git add crates/polaris-core
git commit -F - <<'MSG'
feat(core): add append-only audit log with secret redaction

commons の secret-screen を取り込み、記録の直前に必ず通す。bash の
コマンド文字列を残す以上、生の資格情報がログへ落ちる経路を塞ぐ。
署名は付けない。インプロセスでは署名する主体と行為する主体が同一で、
ログ以上のことを証明しないため。

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

### Task 7: Provider トレイトと SSE

**Files:**
- Create: `crates/polaris-provider/Cargo.toml`
- Create: `crates/polaris-provider/src/lib.rs`
- Create: `crates/polaris-provider/src/sse.rs`（commons から取り込む）
- Test: `crates/polaris-provider/src/lib.rs` の `#[cfg(test)]` モジュール

**Interfaces:**
- Consumes: なし
- Produces: `polaris_provider::{Provider, CompletionRequest, CompletionResponse, Message, Role, ToolCall, ProviderError}`、`polaris_provider::sse::{SseDecoder, SseEvent}`

- [ ] **Step 1: commons から SSE デコーダを取り込む**

```bash
python3 ~/.claude/skills/commons-catalog/scripts/commons.py use sse-decoder \
  --into crates/polaris-provider/src \
  --project codex/polaris --lang rust
```

`src/sse.rs` の接頭辞が落ちて `crates/polaris-provider/src/sse.rs` へ置かれる。改名は不要。

- [ ] **Step 2: 失敗するテストを書く**

`crates/polaris-provider/src/lib.rs` の末尾に置く。

```rust
#[cfg(test)]
mod tests {
    use super::*;

    struct Canned {
        reply: CompletionResponse,
    }

    #[async_trait::async_trait]
    impl Provider for Canned {
        async fn complete(
            &self,
            _req: CompletionRequest,
        ) -> Result<CompletionResponse, ProviderError> {
            Ok(self.reply.clone())
        }
    }

    #[tokio::test]
    async fn provider_trait_is_object_safe_and_returns_tool_calls() {
        let p: Box<dyn Provider> = Box::new(Canned {
            reply: CompletionResponse {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({ "path": "src/main.rs" }),
                }],
            },
        });
        let res = p
            .complete(CompletionRequest {
                system: "s".into(),
                messages: vec![Message { role: Role::User, content: "go".into() }],
                tools: vec![],
            })
            .await
            .expect("失敗した");
        assert_eq!(res.tool_calls.len(), 1);
        assert_eq!(res.tool_calls[0].name, "read");
    }

    #[test]
    fn sse_decoder_is_reachable() {
        let mut d = sse::SseDecoder::new();
        let evs = d.push(b"data: hello\n\n");
        assert_eq!(evs[0].data, "hello");
    }
}
```

- [ ] **Step 3: テストが失敗することを確認する**

Run: `cargo test -p polaris-provider`
Expected: コンパイルエラー。`Provider` などが未定義

- [ ] **Step 4: クレートを作る**

`crates/polaris-provider/Cargo.toml`

```toml
[package]
name = "polaris-provider"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true

[dependencies]
polaris-tools = { path = "../polaris-tools" }
serde = { workspace = true }
serde_json = { workspace = true }
thiserror = { workspace = true }
async-trait = { workspace = true }
reqwest = { workspace = true }
tokio = { workspace = true }

[dev-dependencies]
tokio = { workspace = true }
wiremock = { workspace = true }
```

- [ ] **Step 5: 最小の実装を書く**

`crates/polaris-provider/src/lib.rs` の先頭に置く。

```rust
//! プロバイダ抽象。トランスポートに依存する部分は各実装が持ち、
//! ここには要求と応答の形だけを置く。

pub mod sse;

use polaris_tools::ToolSpec;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

pub struct CompletionRequest {
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
}

#[derive(Debug, Clone, Default)]
pub struct CompletionResponse {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("HTTP エラー: {0}")]
    Http(String),
    #[error("応答を解釈できない: {0}")]
    Decode(String),
}

#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    async fn complete(&self, req: CompletionRequest)
    -> Result<CompletionResponse, ProviderError>;
}
```

- [ ] **Step 6: テストが通ることを確認する**

Run: `cargo test -p polaris-provider`
Expected: 取り込んだ `sse` の 7 件と lib の 2 件を含めて全て PASS

- [ ] **Step 7: コミットする**

```bash
git add crates/polaris-provider
git commit -F - <<'MSG'
feat(provider): add Provider trait and vendored SSE decoder

commons の sse-decoder を取り込む。transport から独立しているため
非同期の実装でもそのまま使える。トレイトはオブジェクト安全にして、
プロバイダを実行時に差し替えられるようにする。

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

### Task 8: OpenAI 互換プロバイダ

**Files:**
- Create: `crates/polaris-provider/src/openai.rs`
- Modify: `crates/polaris-provider/src/lib.rs`（`pub mod openai;` を追加）
- Test: `crates/polaris-provider/src/openai.rs` の `#[cfg(test)]` モジュール

**Interfaces:**
- Consumes: `polaris_provider::{Provider, CompletionRequest, CompletionResponse, ToolCall, ProviderError, Message, Role}`
- Produces: `polaris_provider::openai::OpenAiProvider::new(base_url: String, api_key: String, model: String) -> OpenAiProvider`

- [ ] **Step 1: 失敗するテストを書く**

`crates/polaris-provider/src/openai.rs` の末尾に置く。

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn parses_tool_call_from_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{
                    "message": {
                        "content": null,
                        "tool_calls": [{
                            "id": "call_1",
                            "function": { "name": "read", "arguments": "{\"path\":\"a.rs\"}" }
                        }]
                    }
                }]
            })))
            .mount(&server)
            .await;

        let p = OpenAiProvider::new(server.uri(), "k".into(), "m".into());
        let res = p
            .complete(CompletionRequest {
                system: "s".into(),
                messages: vec![Message { role: Role::User, content: "go".into() }],
                tools: vec![],
            })
            .await
            .expect("失敗した");

        assert_eq!(res.tool_calls.len(), 1);
        assert_eq!(res.tool_calls[0].name, "read");
        assert_eq!(res.tool_calls[0].arguments["path"], "a.rs");
    }

    #[tokio::test]
    async fn surfaces_http_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let p = OpenAiProvider::new(server.uri(), "k".into(), "m".into());
        let err = p
            .complete(CompletionRequest {
                system: "s".into(),
                messages: vec![],
                tools: vec![],
            })
            .await
            .expect_err("エラーになるべき");
        assert!(matches!(err, ProviderError::Http(_)));
    }
}
```

- [ ] **Step 2: テストが失敗することを確認する**

Run: `cargo test -p polaris-provider openai`
Expected: コンパイルエラー。`OpenAiProvider` が未定義

- [ ] **Step 3: 最小の実装を書く**

`crates/polaris-provider/src/openai.rs` の先頭に置く。

```rust
//! OpenAI 互換のチャット補完。base_url を差し替えれば互換エンドポイントも叩ける。

use serde_json::Value;

use crate::{
    CompletionRequest, CompletionResponse, Provider, ProviderError, Role, ToolCall,
};

pub struct OpenAiProvider {
    base_url: String,
    api_key: String,
    model: String,
    client: reqwest::Client,
}

impl OpenAiProvider {
    pub fn new(base_url: String, api_key: String, model: String) -> Self {
        Self { base_url, api_key, model, client: reqwest::Client::new() }
    }
}

fn role_str(r: Role) -> &'static str {
    match r {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

#[async_trait::async_trait]
impl Provider for OpenAiProvider {
    async fn complete(
        &self,
        req: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        let mut messages = vec![serde_json::json!({
            "role": "system",
            "content": req.system,
        })];
        for m in &req.messages {
            messages.push(serde_json::json!({
                "role": role_str(m.role),
                "content": m.content,
            }));
        }

        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                })
            })
            .collect();

        let mut body = serde_json::json!({
            "model": self.model,
            "messages": messages,
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }

        let resp = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Http(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(ProviderError::Http(format!("status {}", resp.status())));
        }

        let v: Value = resp
            .json()
            .await
            .map_err(|e| ProviderError::Decode(e.to_string()))?;

        let msg = &v["choices"][0]["message"];
        let text = msg["content"].as_str().unwrap_or_default().to_string();

        let mut tool_calls = Vec::new();
        if let Some(calls) = msg["tool_calls"].as_array() {
            for c in calls {
                let name = c["function"]["name"].as_str().unwrap_or_default().to_string();
                let raw = c["function"]["arguments"].as_str().unwrap_or("{}");
                let arguments: Value = serde_json::from_str(raw)
                    .map_err(|e| ProviderError::Decode(e.to_string()))?;
                tool_calls.push(ToolCall {
                    id: c["id"].as_str().unwrap_or_default().to_string(),
                    name,
                    arguments,
                });
            }
        }

        Ok(CompletionResponse { text, tool_calls })
    }
}
```

`crates/polaris-provider/src/lib.rs` へ追加する。

```rust
pub mod openai;
```

- [ ] **Step 4: テストが通ることを確認する**

Run: `cargo test -p polaris-provider`
Expected: 全て PASS

- [ ] **Step 5: コミットする**

```bash
git add crates/polaris-provider
git commit -F - <<'MSG'
feat(provider): implement OpenAI-compatible completion

base_url を差し替えれば互換エンドポイントも叩ける形にする。ツール呼び出しの
arguments は文字列で返るため、ここで JSON へ戻して上位へ渡す。
モックサーバに対して成功経路と HTTP エラー経路を固定する。

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

### Task 9: 停止条件

**Files:**
- Create: `crates/polaris-core/src/stop.rs`
- Modify: `crates/polaris-core/src/lib.rs`
- Test: `crates/polaris-core/src/stop.rs` の `#[cfg(test)]` モジュール

**Interfaces:**
- Consumes: なし
- Produces: `polaris_core::stop::{StopTracker, StopReason}`、`StopTracker::new(max_turns: u32)`、`StopTracker::observe_error(&mut self, msg: &str) -> Option<StopReason>`、`StopTracker::observe_turn(&mut self) -> Option<StopReason>`

- [ ] **Step 1: 失敗するテストを書く**

`crates/polaris-core/src/stop.rs` の末尾に置く。

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stops_after_three_identical_errors() {
        let mut t = StopTracker::new(100);
        assert!(t.observe_error("boom").is_none());
        assert!(t.observe_error("boom").is_none());
        assert!(matches!(t.observe_error("boom"), Some(StopReason::RepeatedError(_))));
    }

    #[test]
    fn different_errors_reset_the_streak() {
        let mut t = StopTracker::new(100);
        t.observe_error("boom");
        t.observe_error("boom");
        assert!(t.observe_error("other").is_none());
        assert!(t.observe_error("other").is_none());
        assert!(matches!(t.observe_error("other"), Some(StopReason::RepeatedError(_))));
    }

    #[test]
    fn stops_at_max_turns() {
        let mut t = StopTracker::new(2);
        assert!(t.observe_turn().is_none());
        assert!(matches!(t.observe_turn(), Some(StopReason::MaxTurns)));
    }
}
```

- [ ] **Step 2: テストが失敗することを確認する**

Run: `cargo test -p polaris-core stop`
Expected: コンパイルエラー。`StopTracker` が未定義

- [ ] **Step 3: 最小の実装を書く**

`crates/polaris-core/src/stop.rs` の先頭に置く。

```rust
//! 停止条件。自動修復は行わない。壊れたまま回り続けるのが最も高くつくため、
//! 判断は呼び出し側へ返す。

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// 同一のエラーが 3 回続いた。
    RepeatedError(String),
    /// ターン数の上限に達した。
    MaxTurns,
}

pub struct StopTracker {
    last_error: Option<String>,
    streak: u32,
    turns: u32,
    max_turns: u32,
}

impl StopTracker {
    pub fn new(max_turns: u32) -> Self {
        Self { last_error: None, streak: 0, turns: 0, max_turns }
    }

    pub fn observe_error(&mut self, msg: &str) -> Option<StopReason> {
        if self.last_error.as_deref() == Some(msg) {
            self.streak += 1;
        } else {
            self.last_error = Some(msg.to_string());
            self.streak = 1;
        }
        if self.streak >= 3 {
            return Some(StopReason::RepeatedError(msg.to_string()));
        }
        None
    }

    pub fn observe_turn(&mut self) -> Option<StopReason> {
        self.turns += 1;
        if self.turns >= self.max_turns {
            return Some(StopReason::MaxTurns);
        }
        None
    }
}
```

`crates/polaris-core/src/lib.rs` へ追加する。

```rust
pub mod stop;
```

- [ ] **Step 4: テストが通ることを確認する**

Run: `cargo test -p polaris-core stop`
Expected: 3 件とも PASS

- [ ] **Step 5: コミットする**

```bash
git add crates/polaris-core
git commit -F - <<'MSG'
feat(core): add quantitative stop conditions

同一エラー 3 連続とターン上限で止める。エラーが変われば連続数を数え直す。
自動修復は入れない。失敗を失敗として返し、判断を呼び出し側へ戻す。

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

### Task 10: エージェントループ

**Files:**
- Create: `crates/polaris-core/src/session.rs`
- Create: `crates/polaris-core/src/agent.rs`
- Modify: `crates/polaris-core/src/lib.rs`
- Modify: `crates/polaris-core/Cargo.toml`
- Test: `crates/polaris-core/src/agent.rs` の `#[cfg(test)]` モジュール

**Interfaces:**
- Consumes: `polaris_provider::{Provider, CompletionRequest, CompletionResponse, Message, Role, ToolCall}`、`polaris_tools::{all_specs, read::read}`、`polaris_core::stop::{StopTracker, StopReason}`、`polaris_core::audit::AuditLog`、`polaris_core::prompt::build_system`
- Produces: `polaris_core::session::Session::new() -> Session`、`Session::push_user(&mut self, &str)`、`polaris_core::agent::run(provider: &dyn Provider, session: &mut Session, audit: &mut AuditLog, stop: &mut StopTracker, system: &str) -> Result<String, AgentError>`

- [ ] **Step 1: 失敗するテストを書く**

`crates/polaris-core/src/agent.rs` の末尾に置く。

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use polaris_provider::{CompletionResponse, ToolCall};
    use std::sync::Mutex;

    /// 1 回目はツール呼び出し、2 回目は本文を返すプロバイダ。
    struct Scripted {
        replies: Mutex<Vec<CompletionResponse>>,
    }

    #[async_trait::async_trait]
    impl Provider for Scripted {
        async fn complete(
            &self,
            _req: CompletionRequest,
        ) -> Result<CompletionResponse, polaris_provider::ProviderError> {
            let mut r = self.replies.lock().expect("lock");
            Ok(if r.is_empty() { CompletionResponse::default() } else { r.remove(0) })
        }
    }

    #[tokio::test]
    async fn runs_tool_then_returns_final_text() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let target = dir.path().join("a.txt");
        std::fs::write(&target, "hello\n").expect("書けない");

        let p = Scripted {
            replies: Mutex::new(vec![
                CompletionResponse {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({ "path": target.to_str().unwrap() }),
                    }],
                },
                CompletionResponse { text: "1 行だった".into(), tool_calls: vec![] },
            ]),
        };

        let mut session = Session::new();
        session.push_user("a.txt は何行か");
        let mut audit = AuditLog::open(&dir.path().join("audit.jsonl")).expect("開けない");
        let mut stop = StopTracker::new(10);

        let system = crate::prompt::build_system("", "");
        let out = run(&p, &mut session, &mut audit, &mut stop, &system)
            .await
            .expect("失敗");
        assert_eq!(out, "1 行だった");

        let log = std::fs::read_to_string(dir.path().join("audit.jsonl")).expect("読めない");
        assert!(log.contains("\"tool\":\"read\""), "read が記録されていない");
    }

    #[tokio::test]
    async fn stops_when_tool_fails_three_times() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let call = || CompletionResponse {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "c".into(),
                name: "read".into(),
                arguments: serde_json::json!({ "path": "/home/u/.ssh/id_rsa" }),
            }],
        };
        let p = Scripted { replies: Mutex::new(vec![call(), call(), call()]) };

        let mut session = Session::new();
        session.push_user("読んで");
        let mut audit = AuditLog::open(&dir.path().join("audit.jsonl")).expect("開けない");
        let mut stop = StopTracker::new(50);

        let system = crate::prompt::build_system("", "");
        let err = run(&p, &mut session, &mut audit, &mut stop, &system)
            .await
            .expect_err("止まるべき");
        assert!(matches!(err, AgentError::Stopped(StopReason::RepeatedError(_))));
    }
}
```

- [ ] **Step 2: テストが失敗することを確認する**

Run: `cargo test -p polaris-core agent`
Expected: コンパイルエラー。`Session` と `run` が未定義

- [ ] **Step 3: 依存を足す**

`crates/polaris-core/Cargo.toml` へ追加する。

```toml
polaris-provider = { path = "../polaris-provider" }
async-trait = { workspace = true }
tokio = { workspace = true }
```

- [ ] **Step 4: セッションを実装する**

`crates/polaris-core/src/session.rs`

```rust
//! メッセージ履歴。M1 では追加のみで、圧縮もディスクへの永続化も持たない。
//! 永続化と再開は M5 で入れる。

use polaris_provider::{Message, Role};

#[derive(Default)]
pub struct Session {
    pub messages: Vec<Message>,
}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_user(&mut self, content: &str) {
        self.messages.push(Message { role: Role::User, content: content.to_string() });
    }

    pub fn push_assistant(&mut self, content: &str) {
        self.messages.push(Message { role: Role::Assistant, content: content.to_string() });
    }

    pub fn push_tool_result(&mut self, content: &str) {
        self.messages.push(Message { role: Role::Tool, content: content.to_string() });
    }
}
```

- [ ] **Step 5: ループを実装する**

`crates/polaris-core/src/agent.rs` の先頭に置く。

```rust
//! エージェントループ。ツール呼び出しが無くなった時点の本文を返す。

use std::path::Path;

use polaris_provider::{CompletionRequest, Provider};

use crate::audit::AuditLog;
use crate::session::Session;
use crate::stop::{StopReason, StopTracker};

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("停止した: {0:?}")]
    Stopped(StopReason),
    #[error("プロバイダ: {0}")]
    Provider(#[from] polaris_provider::ProviderError),
    #[error("入出力: {0}")]
    Io(#[from] std::io::Error),
}

/// `system` は憲法ブロックと環境情報を含めて組み立て済みのものを渡す。
/// ループ内で組み立てないのは、毎ターン同じ文字列を送ってキャッシュ接頭辞を
/// 動かさないことを呼び出し側で保証させるため。
pub async fn run(
    provider: &dyn Provider,
    session: &mut Session,
    audit: &mut AuditLog,
    stop: &mut StopTracker,
    system: &str,
) -> Result<String, AgentError> {
    loop {
        if let Some(r) = stop.observe_turn() {
            return Err(AgentError::Stopped(r));
        }

        let res = provider
            .complete(CompletionRequest {
                system: system.to_string(),
                messages: session.messages.clone(),
                tools: polaris_tools::all_specs(),
            })
            .await?;

        if res.tool_calls.is_empty() {
            session.push_assistant(&res.text);
            return Ok(res.text);
        }

        for call in &res.tool_calls {
            let outcome = dispatch(call);
            audit.record(&call.name, &call.arguments.to_string())?;
            match outcome {
                Ok(body) => session.push_tool_result(&body),
                Err(msg) => {
                    if let Some(r) = stop.observe_error(&msg) {
                        return Err(AgentError::Stopped(r));
                    }
                    session.push_tool_result(&msg);
                }
            }
        }
    }
}

/// ツール呼び出しを実際の実装へ振り分ける。失敗はモデルへ返す文字列にする。
fn dispatch(call: &polaris_provider::ToolCall) -> Result<String, String> {
    match call.name.as_str() {
        "read" => {
            let path = call.arguments["path"]
                .as_str()
                .ok_or_else(|| "path が無い".to_string())?;
            let offset = call.arguments["offset"].as_u64().unwrap_or(0) as usize;
            let limit = call.arguments["limit"].as_u64().unwrap_or(2000) as usize;
            polaris_tools::read::read(Path::new(path), offset, limit).map_err(|e| e.to_string())
        }
        other => Err(format!("未知のツール: {other}")),
    }
}
```

`crates/polaris-core/src/lib.rs` へ追加する。

```rust
pub mod agent;
pub mod session;
```

- [ ] **Step 6: テストが通ることを確認する**

Run: `cargo test -p polaris-core`
Expected: 全て PASS

- [ ] **Step 7: コミットする**

```bash
git add crates/polaris-core
git commit -F - <<'MSG'
feat(core): add the agent loop

ツール呼び出しが尽きた時点の本文を返す。ツールの失敗は例外にせず
文字列としてモデルへ返し、同一の失敗が 3 回続いたところで停止する。
呼び出しは実行のたびに監査ログへ記録する。

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

### Task 11: CLI 一発実行

**Files:**
- Create: `crates/polaris-cli/Cargo.toml`
- Create: `crates/polaris-cli/src/main.rs`
- Create: `crates/polaris-cli/tests/cli.rs`
- Test: `crates/polaris-cli/tests/cli.rs`

**Interfaces:**
- Consumes: `polaris_core::{agent::run, session::Session, audit::AuditLog, stop::StopTracker, constitution, prompt::build_system}`、`polaris_provider::openai::OpenAiProvider`
- Produces: バイナリ `polaris`

- [ ] **Step 1: 失敗するテストを書く**

`crates/polaris-cli/tests/cli.rs`

```rust
use std::process::Command;

/// API キーが無い状態で起動したら、鍵が無いことを明示して終了する。
/// 実際のネットワークへは出ない。
#[test]
fn reports_missing_api_key() {
    let exe = env!("CARGO_BIN_EXE_polaris");
    let out = Command::new(exe)
        .args(["-p", "hello"])
        .env_remove("POLARIS_API_KEY")
        .output()
        .expect("起動できない");

    assert!(!out.status.success(), "鍵が無いのに成功している");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("POLARIS_API_KEY"), "鍵が無いことを伝えていない: {err}");
}
```

- [ ] **Step 2: テストが失敗することを確認する**

Run: `cargo test -p polaris-cli`
Expected: `CARGO_BIN_EXE_polaris` が解決できずコンパイルエラー

- [ ] **Step 3: クレートを作る**

`crates/polaris-cli/Cargo.toml`

```toml
[package]
name = "polaris-cli"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true

[[bin]]
name = "polaris"
path = "src/main.rs"

[dependencies]
polaris-core = { path = "../polaris-core" }
polaris-provider = { path = "../polaris-provider" }
clap = { workspace = true }
tokio = { workspace = true }
```

- [ ] **Step 4: 最小の実装を書く**

`crates/polaris-cli/src/main.rs`

```rust
use std::path::PathBuf;
use std::process::ExitCode;

use polaris_core::{agent, audit::AuditLog, constitution, prompt, session::Session, stop::StopTracker};
use polaris_provider::openai::OpenAiProvider;
use clap::Parser;

#[derive(Parser)]
#[command(name = "polaris", about = "最小コンテキストのコーディングエージェント")]
struct Args {
    /// 実行する指示。
    #[arg(short, long)]
    prompt: String,

    /// 監査ログの出力先。
    #[arg(long, default_value = "polaris-audit.jsonl")]
    audit: PathBuf,

    /// 1 回の実行で許すターン数の上限。
    #[arg(long, default_value_t = 20)]
    max_turns: u32,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();

    let Ok(api_key) = std::env::var("POLARIS_API_KEY") else {
        eprintln!("POLARIS_API_KEY が設定されていない");
        return ExitCode::FAILURE;
    };
    let base_url =
        std::env::var("POLARIS_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".into());
    let model = std::env::var("POLARIS_MODEL").unwrap_or_else(|_| "gpt-5.4".into());

    let provider = OpenAiProvider::new(base_url, api_key, model);
    let mut session = Session::new();
    session.push_user(&args.prompt);

    let mut audit = match AuditLog::open(&args.audit) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("監査ログを開けない: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut stop = StopTracker::new(args.max_turns);

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let constitution = constitution::load(&cwd);
    let environment = constitution::environment_block(&cwd, None);
    let system = prompt::build_system(&constitution, &environment);

    match agent::run(&provider, &mut session, &mut audit, &mut stop, &system).await {
        Ok(text) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
```

- [ ] **Step 5: テストが通ることを確認する**

Run: `cargo test -p polaris-cli`
Expected: PASS

- [ ] **Step 6: 全体を通す**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: 全て成功。予算テストが実測値とともに通ることを確認する

- [ ] **Step 7: コミットする**

```bash
git add crates/polaris-cli
git commit -F - <<'MSG'
feat(cli): add headless one-shot entry point

POLARIS_API_KEY と POLARIS_BASE_URL と POLARIS_MODEL で接続先を決める。
鍵が無い場合はネットワークへ出る前に終了し、その旨を標準エラーへ出す。

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

## M1 の完了条件

- `cargo test --workspace` が全て通る
- `cargo clippy --workspace --all-targets -- -D warnings` が通る
- 予算テストが 990 トークン以下を実測で示す
- 巨大な AGENTS.md を置いても常時コンテキストが 990 を超えない
- `.ssh/id_rsa` への読み取りが `PathDenied` で拒否される
- 監査ログに生の資格情報が現れない
- `POLARIS_API_KEY` を設定した状態で `polaris -p "Cargo.toml は何行か"` が答えを返す
