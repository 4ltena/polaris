# apsis M1 ヘッドレス最小ループ Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `read` ツールだけを持つヘッドレスエージェントを動かし、常時コンテキスト 990 トークン以下という不変条件をテストで固定する。

**Architecture:** Cargo workspace に `apsis-tools` / `apsis-provider` / `apsis-core` / `apsis-cli` の 4 クレートを置く。ツール定義は JSON Schema として直列化し、その実バイト列をトークナイザで数えて予算テストに掛ける。プロバイダは非同期のトレイトとし、SSE の解析は commons の `sse-decoder` を取り込む。全ツール呼び出しは追記専用 JSONL へ記録し、記録前に commons の `secret-screen` を通す。

**Tech Stack:** Rust 1.96.0 / edition 2024、tokio、reqwest、serde、tiktoken-rs 0.12、clap、thiserror、async-trait、wiremock(dev)、tempfile(dev)

**Spec:** `docs/superpowers/specs/2026-08-16-apsis-harness-design.md`

## Global Constraints

- Rust edition は `2024`、`rust-toolchain.toml` の channel は `1.96.0`、components に `rustfmt` と `clippy` を含める
- 常時コンテキストは 990 トークン以下。計測の基準トークナイザは `tiktoken_rs::o200k_base()` とする
- 常時提供するツールは 6 本を超えない
- クレート名の接頭辞は `apsis-`
- 監査ログへ書く文字列は、書く直前に必ず `secret_screen::screen_text` を通す
- commons から取り込んだファイルは取り込み後に改変してよい。改変は元へ戻さない
- コミットのタイトルは英語、本文は日本語可。末尾に空行 1 行を挟んで `Co-Authored-By: Claude <noreply@anthropic.com>` を付ける

## 仕様からの逸脱

仕様では監査ログを `apsis-agents` の責務としているが、監査ログは subagent に限らず全ツール呼び出しを対象とする。M1 には `apsis-agents` が存在しないため、`apsis-core` の `audit` モジュールへ置く。仕様側もこの配置へ改める。

## マイルストーン地図

この計画は M1 のみを対象とする。M2 以降はそれぞれ別の計画として書く。

| | 内容 | 単体で動くもの |
| --- | --- | --- |
| M1 | 本計画。read ツール、1 プロバイダ、予算テスト、監査ログ、停止条件 | ファイルを読んで答えるヘッドレスエージェント |
| M2 | `write` / `edit` / `bash` と `apsis-sandbox`。宣言外書き込みの実サンドボックス拒否テスト | 編集とコマンド実行ができる |
| M3 | `apsis-skills` と `apsis-router`。`atomic-file-replace` の取り込み | skill が自動で添付される |
| M4 | `apsis-agents`。`spawn`、波、継続波、path claim | subagent の並列実行 |
| M5 | `apsis-tui` と圧縮、マルチプロバイダとフォールバック、セッションの追記永続化と再開 | 対話型の日常ドライバ |

## ファイル構成

```
apsis/
├── Cargo.toml                              workspace 定義と共通依存
├── rust-toolchain.toml                     1.96.0 固定
├── .gitignore
└── crates/
    ├── apsis-tools/
    │   ├── Cargo.toml
    │   └── src/
    │       ├── lib.rs                      ToolSpec、ToolError、all_specs
    │       ├── path_policy.rs              読み取り拒否パスの判定
    │       └── read.rs                     read ツールの実装
    ├── apsis-provider/
    │   ├── Cargo.toml
    │   └── src/
    │       ├── lib.rs                      Provider トレイトと要求・応答の型
    │       ├── sse.rs                      commons/sse-decoder を取り込む
    │       └── openai.rs                   OpenAI 互換の実装
    ├── apsis-core/
    │   ├── Cargo.toml
    │   └── src/
    │       ├── lib.rs                      モジュール宣言
    │       ├── prompt.rs                   システムプロンプト
    │       ├── budget.rs                   トークン計測
    │       ├── audit.rs                    追記専用 JSONL
    │       ├── secret_screen/mod.rs        commons/secret-screen を取り込む
    │       ├── stop.rs                     停止条件
    │       ├── session.rs                  メッセージ履歴
    │       └── agent.rs                    エージェントループ
    └── apsis-cli/
        ├── Cargo.toml
        └── src/main.rs                     一発実行の入口
```

---

### Task 1: workspace 骨格とツール定義

**Files:**
- Create: `Cargo.toml`
- Create: `rust-toolchain.toml`
- Create: `.gitignore`
- Create: `crates/apsis-tools/Cargo.toml`
- Create: `crates/apsis-tools/src/lib.rs`
- Test: `crates/apsis-tools/src/lib.rs` の `#[cfg(test)]` モジュール

**Interfaces:**
- Consumes: なし
- Produces: `apsis_tools::ToolSpec { name: &'static str, description: &'static str, parameters: serde_json::Value }`、`apsis_tools::all_specs() -> Vec<ToolSpec>`、`apsis_tools::ToolError`

- [ ] **Step 1: 失敗するテストを書く**

`crates/apsis-tools/src/lib.rs` の末尾に置く。

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

- [ ] **Step 2: テストが失敗することを確認する**

Run: `cargo test -p apsis-tools`
Expected: コンパイルエラー。`all_specs` と `ToolSpec` が未定義

- [ ] **Step 3: workspace とクレートを作る**

`Cargo.toml`

```toml
[workspace]
resolver = "3"
members = ["crates/apsis-tools", "crates/apsis-provider", "crates/apsis-core", "crates/apsis-cli"]

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

`crates/apsis-tools/Cargo.toml`

```toml
[package]
name = "apsis-tools"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true

[dependencies]
serde = { workspace = true }
serde_json = { workspace = true }
thiserror = { workspace = true }
```

- [ ] **Step 4: 最小の実装を書く**

`crates/apsis-tools/src/lib.rs` の先頭に置く。

```rust
//! apsis の組込みツール。常時提供するツールは 6 本を超えない。

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

Run: `cargo test -p apsis-tools`
Expected: 2 件とも PASS

- [ ] **Step 6: コミットする**

```bash
git add Cargo.toml rust-toolchain.toml .gitignore crates/apsis-tools
git commit -F - <<'MSG'
feat(tools): add ToolSpec and read tool schema

workspace の骨格と apsis-tools を置く。ツール定義は JSON Schema として
直列化し、この実バイト列を後続タスクの予算計測が数える。

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

### Task 2: 常時コンテキスト予算テスト

**Files:**
- Create: `crates/apsis-core/Cargo.toml`
- Create: `crates/apsis-core/src/lib.rs`
- Create: `crates/apsis-core/src/prompt.rs`
- Create: `crates/apsis-core/src/budget.rs`
- Test: `crates/apsis-core/src/budget.rs` の `#[cfg(test)]` モジュール

**Interfaces:**
- Consumes: `apsis_tools::{ToolSpec, all_specs}`
- Produces: `apsis_core::budget::count_tokens(&str) -> usize`、`apsis_core::budget::always_on_tokens(&str, &[ToolSpec]) -> usize`、`apsis_core::prompt::SYSTEM_PROMPT: &str`、`apsis_core::budget::BUDGET_LIMIT: usize`、`apsis_core::budget::MAX_TOOLS: usize`

- [ ] **Step 1: 失敗するテストを書く**

`crates/apsis-core/src/budget.rs` の末尾に置く。

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::SYSTEM_PROMPT;

    #[test]
    fn always_on_context_stays_within_budget() {
        let specs = apsis_tools::all_specs();
        let n = always_on_tokens(SYSTEM_PROMPT, &specs);
        assert!(
            n <= BUDGET_LIMIT,
            "常時コンテキストが {n} トークン。上限 {BUDGET_LIMIT} を超えている"
        );
    }

    #[test]
    fn tool_count_stays_within_limit() {
        let n = apsis_tools::all_specs().len();
        assert!(n <= MAX_TOOLS, "ツールが {n} 本。上限 {MAX_TOOLS} 本を超えている");
    }

    #[test]
    fn count_tokens_is_nonzero_for_nonempty_text() {
        assert!(count_tokens("hello world") > 0);
        assert_eq!(count_tokens(""), 0);
    }
}
```

- [ ] **Step 2: テストが失敗することを確認する**

Run: `cargo test -p apsis-core`
Expected: コンパイルエラー。`always_on_tokens` と `SYSTEM_PROMPT` が未定義

- [ ] **Step 3: クレートを作る**

`crates/apsis-core/Cargo.toml`

```toml
[package]
name = "apsis-core"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true

[dependencies]
apsis-tools = { path = "../apsis-tools" }
serde = { workspace = true }
serde_json = { workspace = true }
thiserror = { workspace = true }
tiktoken-rs = { workspace = true }
```

`crates/apsis-core/src/lib.rs`

```rust
pub mod budget;
pub mod prompt;
```

- [ ] **Step 4: 最小の実装を書く**

`crates/apsis-core/src/prompt.rs`

```rust
/// 常時載るシステムプロンプト。振る舞いの指示を削ると往復が増えて総コストが
/// 上がるため、短さのためにここを削らない。削る対象は構造の重複に限る。
pub const SYSTEM_PROMPT: &str = "\
You are apsis, a coding agent. Read files and answer with what the code actually does.

Rules:
- State file paths as path:line so they can be opened directly.
- Never guess file contents. Read them.
- If the same error occurs three times in a row, stop and report it.
- Do not claim work is done without showing the command output that proves it.
";
```

`crates/apsis-core/src/budget.rs` の先頭に置く。

```rust
//! 常時コンテキストの計測。数値は測定で担保し、見積で運用しない。

use apsis_tools::ToolSpec;

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

Run: `cargo test -p apsis-core`
Expected: 3 件とも PASS。予算超過なら実測値がメッセージに出るので、`SYSTEM_PROMPT` を削って収める

- [ ] **Step 6: コミットする**

```bash
git add crates/apsis-core
git commit -F - <<'MSG'
test(core): enforce always-on context budget

システムプロンプトと実際に直列化したツール定義を基準トークナイザで数え、
990 トークンと 6 本の上限をテストで固定する。見積のまま運用すると必ず
膨らむため、数値を不変条件として置く。

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

### Task 3: 読み取りパス方針

**Files:**
- Create: `crates/apsis-tools/src/path_policy.rs`
- Modify: `crates/apsis-tools/src/lib.rs`（`pub mod path_policy;` を追加）
- Test: `crates/apsis-tools/src/path_policy.rs` の `#[cfg(test)]` モジュール

**Interfaces:**
- Consumes: なし
- Produces: `apsis_tools::path_policy::is_denied(&std::path::Path) -> bool`

- [ ] **Step 1: 失敗するテストを書く**

`crates/apsis-tools/src/path_policy.rs` の末尾に置く。

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

Run: `cargo test -p apsis-tools path_policy`
Expected: コンパイルエラー。`is_denied` が未定義

- [ ] **Step 3: 最小の実装を書く**

`crates/apsis-tools/src/path_policy.rs` の先頭に置く。

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

`crates/apsis-tools/src/lib.rs` の先頭付近へ追加する。

```rust
pub mod path_policy;
```

- [ ] **Step 4: テストが通ることを確認する**

Run: `cargo test -p apsis-tools path_policy`
Expected: 2 件とも PASS

- [ ] **Step 5: コミットする**

```bash
git add crates/apsis-tools
git commit -F - <<'MSG'
feat(tools): deny reading secret-bearing paths

.env、秘密鍵、SSH と GnuPG、クラウド認証、キーチェーンを読み取りから外す。
環境変数の設定を説明する通常の文書まで巻き込まないよう、名前の完全一致と
.env. 接頭辞に限定してテストで固定する。

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

### Task 4: read ツール

**Files:**
- Create: `crates/apsis-tools/src/read.rs`
- Modify: `crates/apsis-tools/src/lib.rs`（`pub mod read;` を追加）
- Modify: `crates/apsis-tools/Cargo.toml`（`tempfile` を dev-dependencies へ追加）
- Test: `crates/apsis-tools/src/read.rs` の `#[cfg(test)]` モジュール

**Interfaces:**
- Consumes: `apsis_tools::path_policy::is_denied`、`apsis_tools::ToolError`
- Produces: `apsis_tools::read::read(path: &std::path::Path, offset: usize, limit: usize) -> Result<String, ToolError>`

- [ ] **Step 1: 失敗するテストを書く**

`crates/apsis-tools/src/read.rs` の末尾に置く。

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

Run: `cargo test -p apsis-tools read`
Expected: コンパイルエラー。`read` が未定義

- [ ] **Step 3: 依存を足す**

`crates/apsis-tools/Cargo.toml` へ追加する。

```toml
[dev-dependencies]
tempfile = { workspace = true }
```

- [ ] **Step 4: 最小の実装を書く**

`crates/apsis-tools/src/read.rs` の先頭に置く。

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

`crates/apsis-tools/src/lib.rs` へ追加する。

```rust
pub mod read;
```

- [ ] **Step 5: テストが通ることを確認する**

Run: `cargo test -p apsis-tools read`
Expected: 3 件とも PASS

- [ ] **Step 6: コミットする**

```bash
git add crates/apsis-tools
git commit -F - <<'MSG'
feat(tools): implement read with line numbers and path policy

行番号を 1 起点で付けるのは、モデルが path:line の形で位置を示せるように
するため。拒否パスは読み取りの前に弾き、エラーを ToolError::PathDenied として
呼び出し側へ返す。

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

### Task 5: 監査ログ

**Files:**
- Create: `crates/apsis-core/src/secret_screen/mod.rs`（commons から取り込む）
- Create: `crates/apsis-core/src/audit.rs`
- Modify: `crates/apsis-core/src/lib.rs`
- Modify: `crates/apsis-core/Cargo.toml`
- Test: `crates/apsis-core/src/audit.rs` の `#[cfg(test)]` モジュール

**Interfaces:**
- Consumes: なし
- Produces: `apsis_core::audit::AuditLog::open(&Path) -> std::io::Result<AuditLog>`、`AuditLog::record(&mut self, tool: &str, detail: &str) -> std::io::Result<()>`、`apsis_core::secret_screen::{screen_text, FilterResult}`

- [ ] **Step 1: commons からシークレット伏字化を取り込む**

```bash
python3 ~/.claude/skills/commons-catalog/scripts/commons.py use secret-screen \
  --into crates/apsis-core/src/secret_screen \
  --project codex/apsis --lang rust
mv crates/apsis-core/src/secret_screen/lib.rs crates/apsis-core/src/secret_screen/mod.rs
```

`use` は `src/` の接頭辞を落として複写するため、`crates/apsis-core/src/secret_screen/lib.rs` へ置かれる。Rust のモジュール解決に合わせて `mod.rs` へ改名する。取り込んだコードの改変は commons 側へ伝播しない。

- [ ] **Step 2: 失敗するテストを書く**

`crates/apsis-core/src/audit.rs` の末尾に置く。

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

Run: `cargo test -p apsis-core audit`
Expected: コンパイルエラー。`AuditLog` が未定義

- [ ] **Step 4: 依存を足す**

`crates/apsis-core/Cargo.toml` へ追加する。

```toml
regex = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }
```

`regex` は取り込んだ `secret_screen` が使う。

- [ ] **Step 5: 最小の実装を書く**

`crates/apsis-core/src/audit.rs` の先頭に置く。

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

`crates/apsis-core/src/lib.rs` へ追加する。

```rust
pub mod audit;
pub mod secret_screen;
```

- [ ] **Step 6: テストが通ることを確認する**

Run: `cargo test -p apsis-core`
Expected: 取り込んだ `secret_screen` の 20 件と audit の 2 件を含めて全て PASS

- [ ] **Step 7: コミットする**

```bash
git add crates/apsis-core
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

### Task 6: Provider トレイトと SSE

**Files:**
- Create: `crates/apsis-provider/Cargo.toml`
- Create: `crates/apsis-provider/src/lib.rs`
- Create: `crates/apsis-provider/src/sse.rs`（commons から取り込む）
- Test: `crates/apsis-provider/src/lib.rs` の `#[cfg(test)]` モジュール

**Interfaces:**
- Consumes: なし
- Produces: `apsis_provider::{Provider, CompletionRequest, CompletionResponse, Message, Role, ToolCall, ProviderError}`、`apsis_provider::sse::{SseDecoder, SseEvent}`

- [ ] **Step 1: commons から SSE デコーダを取り込む**

```bash
python3 ~/.claude/skills/commons-catalog/scripts/commons.py use sse-decoder \
  --into crates/apsis-provider/src \
  --project codex/apsis --lang rust
```

`src/sse.rs` の接頭辞が落ちて `crates/apsis-provider/src/sse.rs` へ置かれる。改名は不要。

- [ ] **Step 2: 失敗するテストを書く**

`crates/apsis-provider/src/lib.rs` の末尾に置く。

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

Run: `cargo test -p apsis-provider`
Expected: コンパイルエラー。`Provider` などが未定義

- [ ] **Step 4: クレートを作る**

`crates/apsis-provider/Cargo.toml`

```toml
[package]
name = "apsis-provider"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true

[dependencies]
apsis-tools = { path = "../apsis-tools" }
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

`crates/apsis-provider/src/lib.rs` の先頭に置く。

```rust
//! プロバイダ抽象。トランスポートに依存する部分は各実装が持ち、
//! ここには要求と応答の形だけを置く。

pub mod sse;

use apsis_tools::ToolSpec;
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

Run: `cargo test -p apsis-provider`
Expected: 取り込んだ `sse` の 7 件と lib の 2 件を含めて全て PASS

- [ ] **Step 7: コミットする**

```bash
git add crates/apsis-provider
git commit -F - <<'MSG'
feat(provider): add Provider trait and vendored SSE decoder

commons の sse-decoder を取り込む。transport から独立しているため
非同期の実装でもそのまま使える。トレイトはオブジェクト安全にして、
プロバイダを実行時に差し替えられるようにする。

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

### Task 7: OpenAI 互換プロバイダ

**Files:**
- Create: `crates/apsis-provider/src/openai.rs`
- Modify: `crates/apsis-provider/src/lib.rs`（`pub mod openai;` を追加）
- Test: `crates/apsis-provider/src/openai.rs` の `#[cfg(test)]` モジュール

**Interfaces:**
- Consumes: `apsis_provider::{Provider, CompletionRequest, CompletionResponse, ToolCall, ProviderError, Message, Role}`
- Produces: `apsis_provider::openai::OpenAiProvider::new(base_url: String, api_key: String, model: String) -> OpenAiProvider`

- [ ] **Step 1: 失敗するテストを書く**

`crates/apsis-provider/src/openai.rs` の末尾に置く。

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

Run: `cargo test -p apsis-provider openai`
Expected: コンパイルエラー。`OpenAiProvider` が未定義

- [ ] **Step 3: 最小の実装を書く**

`crates/apsis-provider/src/openai.rs` の先頭に置く。

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

`crates/apsis-provider/src/lib.rs` へ追加する。

```rust
pub mod openai;
```

- [ ] **Step 4: テストが通ることを確認する**

Run: `cargo test -p apsis-provider`
Expected: 全て PASS

- [ ] **Step 5: コミットする**

```bash
git add crates/apsis-provider
git commit -F - <<'MSG'
feat(provider): implement OpenAI-compatible completion

base_url を差し替えれば互換エンドポイントも叩ける形にする。ツール呼び出しの
arguments は文字列で返るため、ここで JSON へ戻して上位へ渡す。
モックサーバに対して成功経路と HTTP エラー経路を固定する。

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

### Task 8: 停止条件

**Files:**
- Create: `crates/apsis-core/src/stop.rs`
- Modify: `crates/apsis-core/src/lib.rs`
- Test: `crates/apsis-core/src/stop.rs` の `#[cfg(test)]` モジュール

**Interfaces:**
- Consumes: なし
- Produces: `apsis_core::stop::{StopTracker, StopReason}`、`StopTracker::new(max_turns: u32)`、`StopTracker::observe_error(&mut self, msg: &str) -> Option<StopReason>`、`StopTracker::observe_turn(&mut self) -> Option<StopReason>`

- [ ] **Step 1: 失敗するテストを書く**

`crates/apsis-core/src/stop.rs` の末尾に置く。

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

Run: `cargo test -p apsis-core stop`
Expected: コンパイルエラー。`StopTracker` が未定義

- [ ] **Step 3: 最小の実装を書く**

`crates/apsis-core/src/stop.rs` の先頭に置く。

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

`crates/apsis-core/src/lib.rs` へ追加する。

```rust
pub mod stop;
```

- [ ] **Step 4: テストが通ることを確認する**

Run: `cargo test -p apsis-core stop`
Expected: 3 件とも PASS

- [ ] **Step 5: コミットする**

```bash
git add crates/apsis-core
git commit -F - <<'MSG'
feat(core): add quantitative stop conditions

同一エラー 3 連続とターン上限で止める。エラーが変われば連続数を数え直す。
自動修復は入れない。失敗を失敗として返し、判断を呼び出し側へ戻す。

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

### Task 9: エージェントループ

**Files:**
- Create: `crates/apsis-core/src/session.rs`
- Create: `crates/apsis-core/src/agent.rs`
- Modify: `crates/apsis-core/src/lib.rs`
- Modify: `crates/apsis-core/Cargo.toml`
- Test: `crates/apsis-core/src/agent.rs` の `#[cfg(test)]` モジュール

**Interfaces:**
- Consumes: `apsis_provider::{Provider, CompletionRequest, CompletionResponse, Message, Role, ToolCall}`、`apsis_tools::{all_specs, read::read}`、`apsis_core::stop::{StopTracker, StopReason}`、`apsis_core::audit::AuditLog`、`apsis_core::prompt::SYSTEM_PROMPT`
- Produces: `apsis_core::session::Session::new() -> Session`、`Session::push_user(&mut self, &str)`、`apsis_core::agent::run(provider: &dyn Provider, session: &mut Session, audit: &mut AuditLog, stop: &mut StopTracker) -> Result<String, AgentError>`

- [ ] **Step 1: 失敗するテストを書く**

`crates/apsis-core/src/agent.rs` の末尾に置く。

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use apsis_provider::{CompletionResponse, ToolCall};
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
        ) -> Result<CompletionResponse, apsis_provider::ProviderError> {
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

        let out = run(&p, &mut session, &mut audit, &mut stop).await.expect("失敗");
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

        let err = run(&p, &mut session, &mut audit, &mut stop).await.expect_err("止まるべき");
        assert!(matches!(err, AgentError::Stopped(StopReason::RepeatedError(_))));
    }
}
```

- [ ] **Step 2: テストが失敗することを確認する**

Run: `cargo test -p apsis-core agent`
Expected: コンパイルエラー。`Session` と `run` が未定義

- [ ] **Step 3: 依存を足す**

`crates/apsis-core/Cargo.toml` へ追加する。

```toml
apsis-provider = { path = "../apsis-provider" }
async-trait = { workspace = true }
tokio = { workspace = true }
```

- [ ] **Step 4: セッションを実装する**

`crates/apsis-core/src/session.rs`

```rust
//! メッセージ履歴。M1 では追加のみで、圧縮もディスクへの永続化も持たない。
//! 永続化と再開は M5 で入れる。

use apsis_provider::{Message, Role};

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

`crates/apsis-core/src/agent.rs` の先頭に置く。

```rust
//! エージェントループ。ツール呼び出しが無くなった時点の本文を返す。

use std::path::Path;

use apsis_provider::{CompletionRequest, Provider};

use crate::audit::AuditLog;
use crate::prompt::SYSTEM_PROMPT;
use crate::session::Session;
use crate::stop::{StopReason, StopTracker};

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("停止した: {0:?}")]
    Stopped(StopReason),
    #[error("プロバイダ: {0}")]
    Provider(#[from] apsis_provider::ProviderError),
    #[error("入出力: {0}")]
    Io(#[from] std::io::Error),
}

pub async fn run(
    provider: &dyn Provider,
    session: &mut Session,
    audit: &mut AuditLog,
    stop: &mut StopTracker,
) -> Result<String, AgentError> {
    loop {
        if let Some(r) = stop.observe_turn() {
            return Err(AgentError::Stopped(r));
        }

        let res = provider
            .complete(CompletionRequest {
                system: SYSTEM_PROMPT.to_string(),
                messages: session.messages.clone(),
                tools: apsis_tools::all_specs(),
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
fn dispatch(call: &apsis_provider::ToolCall) -> Result<String, String> {
    match call.name.as_str() {
        "read" => {
            let path = call.arguments["path"]
                .as_str()
                .ok_or_else(|| "path が無い".to_string())?;
            let offset = call.arguments["offset"].as_u64().unwrap_or(0) as usize;
            let limit = call.arguments["limit"].as_u64().unwrap_or(2000) as usize;
            apsis_tools::read::read(Path::new(path), offset, limit).map_err(|e| e.to_string())
        }
        other => Err(format!("未知のツール: {other}")),
    }
}
```

`crates/apsis-core/src/lib.rs` へ追加する。

```rust
pub mod agent;
pub mod session;
```

- [ ] **Step 6: テストが通ることを確認する**

Run: `cargo test -p apsis-core`
Expected: 全て PASS

- [ ] **Step 7: コミットする**

```bash
git add crates/apsis-core
git commit -F - <<'MSG'
feat(core): add the agent loop

ツール呼び出しが尽きた時点の本文を返す。ツールの失敗は例外にせず
文字列としてモデルへ返し、同一の失敗が 3 回続いたところで停止する。
呼び出しは実行のたびに監査ログへ記録する。

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

### Task 10: CLI 一発実行

**Files:**
- Create: `crates/apsis-cli/Cargo.toml`
- Create: `crates/apsis-cli/src/main.rs`
- Create: `crates/apsis-cli/tests/cli.rs`
- Test: `crates/apsis-cli/tests/cli.rs`

**Interfaces:**
- Consumes: `apsis_core::{agent::run, session::Session, audit::AuditLog, stop::StopTracker}`、`apsis_provider::openai::OpenAiProvider`
- Produces: バイナリ `apsis`

- [ ] **Step 1: 失敗するテストを書く**

`crates/apsis-cli/tests/cli.rs`

```rust
use std::process::Command;

/// API キーが無い状態で起動したら、鍵が無いことを明示して終了する。
/// 実際のネットワークへは出ない。
#[test]
fn reports_missing_api_key() {
    let exe = env!("CARGO_BIN_EXE_apsis");
    let out = Command::new(exe)
        .args(["-p", "hello"])
        .env_remove("APSIS_API_KEY")
        .output()
        .expect("起動できない");

    assert!(!out.status.success(), "鍵が無いのに成功している");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("APSIS_API_KEY"), "鍵が無いことを伝えていない: {err}");
}
```

- [ ] **Step 2: テストが失敗することを確認する**

Run: `cargo test -p apsis-cli`
Expected: `CARGO_BIN_EXE_apsis` が解決できずコンパイルエラー

- [ ] **Step 3: クレートを作る**

`crates/apsis-cli/Cargo.toml`

```toml
[package]
name = "apsis-cli"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true

[[bin]]
name = "apsis"
path = "src/main.rs"

[dependencies]
apsis-core = { path = "../apsis-core" }
apsis-provider = { path = "../apsis-provider" }
clap = { workspace = true }
tokio = { workspace = true }
```

- [ ] **Step 4: 最小の実装を書く**

`crates/apsis-cli/src/main.rs`

```rust
use std::path::PathBuf;
use std::process::ExitCode;

use apsis_core::{agent, audit::AuditLog, session::Session, stop::StopTracker};
use apsis_provider::openai::OpenAiProvider;
use clap::Parser;

#[derive(Parser)]
#[command(name = "apsis", about = "最小コンテキストのコーディングエージェント")]
struct Args {
    /// 実行する指示。
    #[arg(short, long)]
    prompt: String,

    /// 監査ログの出力先。
    #[arg(long, default_value = "apsis-audit.jsonl")]
    audit: PathBuf,

    /// 1 回の実行で許すターン数の上限。
    #[arg(long, default_value_t = 20)]
    max_turns: u32,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();

    let Ok(api_key) = std::env::var("APSIS_API_KEY") else {
        eprintln!("APSIS_API_KEY が設定されていない");
        return ExitCode::FAILURE;
    };
    let base_url =
        std::env::var("APSIS_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".into());
    let model = std::env::var("APSIS_MODEL").unwrap_or_else(|_| "gpt-5.4".into());

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

    match agent::run(&provider, &mut session, &mut audit, &mut stop).await {
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

Run: `cargo test -p apsis-cli`
Expected: PASS

- [ ] **Step 6: 全体を通す**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: 全て成功。予算テストが実測値とともに通ることを確認する

- [ ] **Step 7: コミットする**

```bash
git add crates/apsis-cli
git commit -F - <<'MSG'
feat(cli): add headless one-shot entry point

APSIS_API_KEY と APSIS_BASE_URL と APSIS_MODEL で接続先を決める。
鍵が無い場合はネットワークへ出る前に終了し、その旨を標準エラーへ出す。

Co-Authored-By: Claude <noreply@anthropic.com>
MSG
```

---

## M1 の完了条件

- `cargo test --workspace` が全て通る
- `cargo clippy --workspace --all-targets -- -D warnings` が通る
- 予算テストが 990 トークン以下を実測で示す
- `.ssh/id_rsa` への読み取りが `PathDenied` で拒否される
- 監査ログに生の資格情報が現れない
- `APSIS_API_KEY` を設定した状態で `apsis -p "Cargo.toml は何行か"` が答えを返す
