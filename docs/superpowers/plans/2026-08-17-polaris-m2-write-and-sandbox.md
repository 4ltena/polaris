# polaris M2 実装計画 — write / edit / bash とサンドボックス

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** ファイルシステムを変更する 3 ツール（`write`、`edit`、`bash`）を、実 OS サンドボックスの内側でのみ動くように実装する。

**Architecture:** 強制はプロセス境界でしか効かないため、変更操作はすべて子プロセスを経由する。`bash` はもともと子を起こす。`write` と `edit` は、書込可能ルートの外へ退避した polaris 自身のバイナリを拘束モードで起動し、標準入力から受けた 1 件の操作を実行させる。`polaris-sandbox` は方針と正規化済みルートだけを持ち、macOS は生成した Seatbelt プロファイルを `/usr/bin/sandbox-exec` へ渡し、Linux は `landlock` の ruleset を子の `pre_exec` で適用する。承認境界は別層にあり、対象パスが一意に定まる `write` と `edit` だけを事前に判定する。

**Tech Stack:** Rust 2024 edition、rust-version 1.96、tokio、serde / serde_json、`landlock` クレート（Linux のみ）、`tempfile`（テスト）。macOS 側の追加依存は無い（`/usr/bin/sandbox-exec` を起動するだけ）。

**Spec:** `docs/superpowers/specs/2026-08-16-polaris-harness-design.md`

## Global Constraints

仕様から逐語で引く。全タスクの要件はこの節を暗黙に含む。

- クレートは 9 個で固定する。増やすことは設計上の後退とみなす。本計画で追加してよいのは `polaris-sandbox` の 1 個だけである
- 常時提供するツールは 6 本で固定する。本計画の完了時点で `read`、`write`、`edit`、`bash`、`skill` の 5 本になる
- 常時コンテキストは 990 トークン以下。この上限はディスクから読む入力の大きさに左右されない
- `sandbox_mode` は `read-only`、`workspace-write`、`full-access` の 3 値をとる
- 強制は macOS では実行時に生成した Seatbelt プロファイルを `/usr/bin/sandbox-exec` へ渡し、Linux では `landlock` クレートの ruleset を子の `pre_exec` の中で `restrict_self()` する。両者は共通の抽象を持たず、プラットフォームごとに明示的な実装を置く
- ファイルシステムへの変更はすべてサンドボックス境界を越える。`full-access` であっても越える
- 再実行するバイナリは書込可能ルートの外に置く
- 書込可能ルートと対象パスは、方針を組み立てる前に正規化する
- 強制が適用に失敗した状態は、拒否とは別の事象として扱い、必ず硬い失敗にする
- `write` と `edit` は述語で予測し、越えるなら実行前に停止して承認を求める。`bash` は試行し、拒否されたらモデルへ理由を返す
- 拒否メッセージには、拒否されたパス、現在の方針、書込可能ルートを含める
- 監査は追記専用の JSONL に、ツール呼び出し 1 回を 1 行として記録する。型、解決後のサンドボックス方針、書込先、結果を含める
- 監査へ書くあらゆる文字列は、書く直前にシークレット伏字化を通す
- `polaris-router` は `polaris-sandbox` に依存しない
- 受け入れ基準 3: 宣言した書込可能ルートの外を指すパスへの書き込みが、実サンドボックスで拒否される。モックを用いず、`write` ツールを通して書き込みを試み、拒否を観測する。共有の起動ヘルパを直接叩く経路では確認したことにならない
- 既存のハードリンク経由の書き込みは拒否されない。これは保証しない範囲であり、述語側の `st_nlink > 1` 検出は緩和であって保証ではない
- 実 API 呼び出しをテストで行わない。API キーを読まない、探さない

## 現在の実体

実装者は自分のタスクしか見ない。既存コードの署名をここに集約する。**これらは 2026-08-17 時点で実際に読んで確認した内容だが、木の側が正である。食い違ったら木に従い、報告すること。**

`polaris-tools`（`crates/polaris-tools/src/lib.rs`）

```rust
pub struct ToolSpec { pub name: &'static str, pub description: &'static str, pub parameters: serde_json::Value }

pub enum ToolError {
    PathDenied(String),                                   // "パス {0} は読み取りを許可されていない"
    Io(#[from] std::io::Error),                           // "入出力エラー: {0}"
    NotAFile(String),                                     // "{0} は通常ファイルではない"
    TooLarge { path: String, limit: u64, actual: u64 },
}

pub fn all_specs() -> Vec<ToolSpec>                       // 現在は read_spec(), skill_spec() の 2 本
pub mod path_policy;                                      // pub fn is_denied(path: &Path) -> bool
pub mod read;                                             // pub const MAX_READ_BYTES: u64 = 5*1024*1024;
                                                          // pub const DEFAULT_LIMIT: usize = 2000;
                                                          // pub fn read(path: &Path, offset: usize, limit: usize) -> Result<String, ToolError>
pub mod skill;                                            // pub fn lookup(skills: &[Skill], q: &str) -> String
```

`polaris-core`（`crates/polaris-core/src/`）

```rust
// prompt.rs
pub struct AlwaysOn { /* 私有: system, tools, skills_seen */ }
impl AlwaysOn {
    pub fn system(&self) -> &str;
    pub fn tools(&self) -> &[ToolSpec];
    pub fn tokens(&self) -> usize;
    pub fn skills_seen(&self) -> usize;
}
pub fn assemble_always_on(constitution: &str, environment: &str, skills: &[polaris_skills::Skill]) -> AlwaysOn;
pub(crate) fn build_system(constitution: &str, environment: &str) -> String;

// agent.rs
pub enum AgentError { Stopped(StopReason), Provider(..), Io(..) }
pub async fn run(
    provider: &dyn Provider,
    session: &mut Session,
    audit: &mut AuditLog,
    stop: &mut StopTracker,
    always_on: &crate::prompt::AlwaysOn,
    skills: &[polaris_skills::Skill],
) -> Result<String, AgentError>;
fn dispatch(call: &polaris_provider::ToolCall, skills: &[polaris_skills::Skill]) -> Result<String, String>;

// audit.rs
pub struct AuditLog;
impl AuditLog {
    pub fn open(path: &Path) -> std::io::Result<Self>;
    pub fn record(&mut self, tool: &str, detail: &str) -> std::io::Result<()>;  // 本計画で署名が変わる
}

// session.rs
pub struct Session { pub messages: Vec<Message> }
impl Session {
    pub fn new() -> Self;
    pub fn push_user(&mut self, content: &str);
    pub fn push_assistant(&mut self, content: &str);
    pub fn push_assistant_tool_calls(&mut self, content: &str, tool_calls: Vec<ToolCall>);
    pub fn push_tool_result(&mut self, tool_call_id: &str, content: &str);
}

// stop.rs
pub struct StopTracker;
impl StopTracker {
    pub fn new(max_turns: u32) -> Self;
    pub fn observe_error(&mut self, msg: &str) -> Option<StopReason>;
    pub fn observe_success(&mut self);
    pub fn observe_turn(&mut self) -> Option<StopReason>;
}

// secret_screen.rs
pub fn screen_text(s: &str) -> FilterResult;              // Keep(String) | Redacted(String) | Drop
pub fn is_excluded_path(path: &Path) -> bool;             // 現在どこからも呼ばれていない
```

`polaris-cli`（`crates/polaris-cli/src/main.rs`）は `clap::Parser` の `Args { prompt, audit, max_turns }` を持ち、`assemble_always_on` を 1 回呼んで `agent::run` へ渡す。

## File Structure

**新規クレート `polaris-sandbox`**（`crates/polaris-sandbox/`）

| ファイル | 責務 |
| --- | --- |
| `src/lib.rs` | 公開 API の集約と `SandboxError` |
| `src/policy.rs` | `SandboxMode`、`SandboxPolicy`、ルートの正規化 |
| `src/confine.rs` | `run_confined` のプラットフォーム振り分けと `Outcome` |
| `src/macos.rs` | Seatbelt プロファイル生成と `sandbox-exec` の argv |
| `src/linux.rs` | landlock ruleset と `pre_exec` フック |
| `src/helper.rs` | 拘束ヘルパへ渡す操作の型（`Mutation`）と JSON 表現 |
| `src/stage.rs` | ヘルパ用バイナリを書込可能ルートの外へ退避する |

**`polaris-tools` への追加**

| ファイル | 責務 |
| --- | --- |
| `src/predicate.rs` | 拒否の事前予測。`Verdict` を返す助言層。`polaris-sandbox` ではなくここに置くのは、述語が `path_policy::is_denied` を使い、依存の向きが `polaris-tools` → `polaris-sandbox` の一方向だからである |
| `src/write.rs` | `write` ツール。ヘルパ経由 |
| `src/edit.rs` | `edit` ツール。ヘルパ経由 |
| `src/bash.rs` | `bash` ツール。`run_confined` 直接 |

**`polaris-core` の変更**

| ファイル | 変更 |
| --- | --- |
| `src/audit.rs` | `record` が方針・書込先・結果を運ぶ |
| `src/agent.rs` | `dispatch` が方針と承認境界を受ける |
| `src/project.rs`（新規） | プロジェクトルートの解決 |

**`polaris-cli` の変更**

| ファイル | 変更 |
| --- | --- |
| `src/main.rs` | 拘束ヘルパのサブコマンド、方針の組み立て、承認の入出力 |

## 型の一覧

**全タスクがこの定義を共有する。** 後のタスクで名前や型を変えないこと。

```rust
// polaris-sandbox/src/policy.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SandboxMode { ReadOnly, WorkspaceWrite, FullAccess }

#[derive(Debug, Clone)]
pub struct SandboxPolicy {
    mode: SandboxMode,
    writable_roots: Vec<PathBuf>,   // 正規化済み。私有
}
impl SandboxPolicy {
    pub fn new(mode: SandboxMode, roots: &[PathBuf]) -> Result<Self, SandboxError>;
    pub fn mode(&self) -> SandboxMode;
    pub fn writable_roots(&self) -> &[PathBuf];
    pub fn describe(&self) -> String;   // 拒否メッセージ用。方針とルートを 1 行で
}

// polaris-tools/src/predicate.rs
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allowed,
    NeedsApproval { reason: String },
}
pub fn predict(policy: &SandboxPolicy, target: &Path) -> Verdict;

// polaris-sandbox/src/confine.rs
#[derive(Debug)]
pub struct Outcome {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}
pub fn run_confined(
    policy: &SandboxPolicy,
    program: &Path,
    args: &[String],
    stdin: Option<&str>,
) -> Result<Outcome, SandboxError>;

// polaris-sandbox/src/helper.rs
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Mutation {
    Write { path: PathBuf, content: String },
    Edit { path: PathBuf, old: String, new: String },
}
pub fn apply(m: &Mutation) -> Result<String, String>;   // ヘルパ側で実行する

// polaris-sandbox/src/stage.rs
pub fn staged_helper(policy: &SandboxPolicy, state_dir: &Path) -> Result<PathBuf, SandboxError>;

// polaris-sandbox/src/lib.rs
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("サンドボックスを適用できなかった: {0}")]
    NotEnforced(String),
    #[error("拒否された: {path}（方針 {policy}）")]
    Denied { path: String, policy: String },
    #[error("入出力: {0}")]
    Io(#[from] std::io::Error),
    #[error("このプラットフォームには強制の委譲先が無い")]
    UnsupportedPlatform,
}

// polaris-core/src/project.rs
pub fn resolve_root(start: &Path) -> PathBuf;
```

---

### Task 1: `polaris-sandbox` クレートと方針の型

**Files:**
- Create: `crates/polaris-sandbox/Cargo.toml`
- Create: `crates/polaris-sandbox/src/lib.rs`
- Create: `crates/polaris-sandbox/src/policy.rs`

**Interfaces:**
- Consumes: なし（最初のタスク）
- Produces: `polaris_sandbox::{SandboxError, SandboxMode, SandboxPolicy}`。`SandboxPolicy::new(mode: SandboxMode, roots: &[PathBuf]) -> Result<SandboxPolicy, SandboxError>`、`.mode() -> SandboxMode`、`.writable_roots() -> &[PathBuf]`、`.describe() -> String`

この計画で追加してよいクレートはこの 1 個だけである。仕様はクレート数を 9 に固定しており、`polaris-sandbox` はその 9 個に含まれる。

- [ ] **Step 1: 失敗するテストを書く**

`crates/polaris-sandbox/src/policy.rs` の末尾へ置く。

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_root_reached_through_a_symlink_is_stored_canonicalised() {
        // macOS の /tmp は /private/tmp へのシンボリックリンクであり、
        // tempfile::tempdir() もその下に作られる。正規化しないと、強制側は
        // 与えられたルートと実際のパスが一致しないと判断して「全部拒否」に
        // 倒れる。しかもその拒否は方針違反の拒否と区別がつかないため、
        // 受け入れ基準のテストが誤った理由で通ってしまう。
        let real = tempfile::tempdir().expect("一時ディレクトリ");
        let link_parent = tempfile::tempdir().expect("一時ディレクトリ");
        let link = link_parent.path().join("link-to-root");
        std::os::unix::fs::symlink(real.path(), &link).expect("symlink");

        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[link.clone()])
            .expect("方針を作れない");

        let stored = &policy.writable_roots()[0];
        assert_eq!(
            stored,
            &real.path().canonicalize().expect("canonicalize"),
            "ルートが正規化されていない。格納値 {} はリンクのまま",
            stored.display()
        );
        assert_ne!(stored, &link, "リンクのパスがそのまま入っている");
    }

    #[test]
    fn workspace_write_requires_at_least_one_root() {
        // ルート 0 件の workspace-write は「どこへも書けない」を意味するが、
        // 呼び出し側の組み立て漏れと区別がつかない。区別できない状態を
        // 黙って受け取らない。
        let err = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[])
            .expect_err("ルート 0 件が通ってしまった");
        assert!(
            matches!(err, SandboxError::NotEnforced(_)),
            "想定と違うエラー: {err}"
        );
    }

    #[test]
    fn read_only_rejects_writable_roots() {
        // read-only にルートを渡せてしまうと、方針の名前と実際の権限が
        // 食い違う。宣言と強制を同一のオブジェクトにするという設計目標に反する。
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let err = SandboxPolicy::new(SandboxMode::ReadOnly, &[dir.path().to_path_buf()])
            .expect_err("read-only にルートが通ってしまった");
        assert!(matches!(err, SandboxError::NotEnforced(_)), "{err}");
    }

    #[test]
    fn a_nonexistent_root_is_rejected_rather_than_silently_dropped() {
        // 存在しないルートは canonicalize できない。黙って捨てると、
        // 書けるつもりの場所が減っていることに誰も気づかない。
        let missing = std::path::PathBuf::from("/definitely/not/here/polaris-test");
        let err = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[missing])
            .expect_err("存在しないルートが通ってしまった");
        assert!(matches!(err, SandboxError::Io(_)), "{err}");
    }

    #[test]
    fn full_access_needs_no_roots_and_keeps_none() {
        let policy =
            SandboxPolicy::new(SandboxMode::FullAccess, &[]).expect("full-access を作れない");
        assert_eq!(policy.mode(), SandboxMode::FullAccess);
        assert!(policy.writable_roots().is_empty());
    }

    #[test]
    fn describe_names_the_mode_and_every_root() {
        // 拒否メッセージはこの文字列を含む。仕様が「拒否されたパス、現在の
        // 方針、書込可能ルート」を要求しているので、ルートを 1 件でも
        // 落とす整形はモデルに誤った地図を渡すことになる。
        let a = tempfile::tempdir().expect("一時ディレクトリ");
        let b = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = SandboxPolicy::new(
            SandboxMode::WorkspaceWrite,
            &[a.path().to_path_buf(), b.path().to_path_buf()],
        )
        .expect("方針を作れない");

        let s = policy.describe();
        assert!(s.contains("workspace-write"), "方針名が無い: {s}");
        for root in policy.writable_roots() {
            assert!(
                s.contains(&root.display().to_string()),
                "ルート {} が説明に無い: {s}",
                root.display()
            );
        }
    }
}
```

- [ ] **Step 2: クレートを作る**

`crates/polaris-sandbox/Cargo.toml`:

```toml
[package]
name = "polaris-sandbox"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true

[dependencies]
serde = { workspace = true }
serde_json = { workspace = true }
thiserror = { workspace = true }

[target.'cfg(target_os = "linux")'.dependencies]
landlock = "0.4"

[dev-dependencies]
tempfile = { workspace = true }
```

ワークスペースの `members` は `["crates/*"]` のグロブなので、ルートの `Cargo.toml` は編集しなくてよい。

- [ ] **Step 3: テストを走らせて失敗を確認する**

Run: `cargo test -p polaris-sandbox`
Expected: コンパイルエラー。`policy` モジュールも `SandboxPolicy` も無い。

- [ ] **Step 4: 実装する**

`crates/polaris-sandbox/src/lib.rs`:

```rust
//! サンドボックス方針の定義と、OS 機構への委譲。
//!
//! このクレートは強制の仕組みそのものを実装しない。方針と正規化済みの
//! 書込可能ルートを保持し、プラットフォーム固有の機構へ渡すだけである。

pub mod policy;

pub use policy::{SandboxMode, SandboxPolicy};

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("サンドボックスを適用できなかった: {0}")]
    NotEnforced(String),
    #[error("拒否された: {path}（方針 {policy}）")]
    Denied { path: String, policy: String },
    #[error("入出力: {0}")]
    Io(#[from] std::io::Error),
    #[error("このプラットフォームには強制の委譲先が無い")]
    UnsupportedPlatform,
}
```

`crates/polaris-sandbox/src/policy.rs`:

```rust
//! 方針と書込可能ルート。ルートは構築時に正規化する。

use std::path::{Path, PathBuf};

use crate::SandboxError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    ReadOnly,
    WorkspaceWrite,
    FullAccess,
}

impl SandboxMode {
    /// 方針名。監査ログと拒否メッセージが同じ綴りを使う。
    pub fn as_str(self) -> &'static str {
        match self {
            SandboxMode::ReadOnly => "read-only",
            SandboxMode::WorkspaceWrite => "workspace-write",
            SandboxMode::FullAccess => "full-access",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SandboxPolicy {
    mode: SandboxMode,
    writable_roots: Vec<PathBuf>,
}

impl SandboxPolicy {
    /// ルートは必ず正規化して保持する。経路の途中にシンボリックリンクが
    /// あると、強制側は与えられたパスと実際のパスを別物と判断して全てを
    /// 拒否する。macOS の `/tmp` と `/var` が該当するため端の事例ではない。
    pub fn new(mode: SandboxMode, roots: &[PathBuf]) -> Result<Self, SandboxError> {
        match mode {
            SandboxMode::ReadOnly if !roots.is_empty() => {
                return Err(SandboxError::NotEnforced(
                    "read-only に書込可能ルートは指定できない".into(),
                ));
            }
            SandboxMode::WorkspaceWrite if roots.is_empty() => {
                return Err(SandboxError::NotEnforced(
                    "workspace-write には書込可能ルートが 1 件以上要る".into(),
                ));
            }
            _ => {}
        }

        // full-access ではルートを保持しない。保持すると、実際には効いて
        // いない値が方針の説明に現れ、読む側を誤らせる。
        let writable_roots = if mode == SandboxMode::FullAccess {
            Vec::new()
        } else {
            let mut canonical = Vec::with_capacity(roots.len());
            for r in roots {
                canonical.push(r.canonicalize()?);
            }
            canonical
        };

        Ok(Self {
            mode,
            writable_roots,
        })
    }

    pub fn mode(&self) -> SandboxMode {
        self.mode
    }

    pub fn writable_roots(&self) -> &[PathBuf] {
        &self.writable_roots
    }

    /// 拒否メッセージ用。仕様は拒否されたパスと現在の方針と書込可能ルートを
    /// 含めることを要求している。ここは後半 2 つを担う。
    pub fn describe(&self) -> String {
        if self.writable_roots.is_empty() {
            return self.mode.as_str().to_string();
        }
        let roots: Vec<String> = self
            .writable_roots
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        format!("{}（書込可能: {}）", self.mode.as_str(), roots.join(", "))
    }

    /// 正規化済みの対象パスが、いずれかのルートの内側にあるか。
    /// `full-access` は常に真、`read-only` は常に偽。
    pub fn contains(&self, canonical_target: &Path) -> bool {
        match self.mode {
            SandboxMode::FullAccess => true,
            SandboxMode::ReadOnly => false,
            SandboxMode::WorkspaceWrite => self
                .writable_roots
                .iter()
                .any(|r| canonical_target.starts_with(r)),
        }
    }
}
```

- [ ] **Step 5: テストを走らせて通ることを確認する**

Run: `cargo test -p polaris-sandbox`
Expected: 6 件すべて PASS。

- [ ] **Step 6: 変異で検証する**

各テストが守っている対象を実際に壊し、そのテストだけが落ちることを確認してから戻す。**変異を当てたあと、当たったことをファイルから読み直して確かめること。** 当たっていない変異と捕捉されない変異は外形が同じで、区別がつかない。

1. `r.canonicalize()?` を `r.clone()` に置換 → `a_root_reached_through_a_symlink_is_stored_canonicalised` だけが落ちる
2. `WorkspaceWrite if roots.is_empty()` の分岐を削除 → `workspace_write_requires_at_least_one_root` だけが落ちる
3. `describe` の `roots.join(", ")` を `roots[0].clone()` に置換 → `describe_names_the_mode_and_every_root` だけが落ちる

- [ ] **Step 7: コミット**

```bash
cargo clippy -p polaris-sandbox --all-targets -- -D warnings
cargo fmt --all -- --check
git add crates/polaris-sandbox Cargo.lock
git commit -m "feat(sandbox): define the policy and canonicalise writable roots"
```

`docs/filemap.md` のスナップショットテストが落ちたら `UPDATE_FILEMAP=1 cargo test -p polaris-core --test filemap` で再生成し、同じコミットへ含めること。

---

### Task 2: 拒否を事前に予測する述語

**Files:**
- Create: `crates/polaris-tools/src/predicate.rs`
- Modify: `crates/polaris-tools/src/lib.rs`（`pub mod predicate;` を足す）
- Modify: `crates/polaris-tools/Cargo.toml`（`polaris-sandbox` への依存を足す）

**Interfaces:**
- Consumes: `polaris_sandbox::{SandboxMode, SandboxPolicy}`、`SandboxPolicy::contains(&self, canonical_target: &Path) -> bool`、`.describe() -> String`、`polaris_tools::path_policy::is_denied(path: &Path) -> bool`
- Produces: `polaris_tools::predicate::{Verdict, predict}`。`pub enum Verdict { Allowed, NeedsApproval { reason: String } }`、`pub fn predict(policy: &SandboxPolicy, target: &Path) -> Verdict`

述語は助言であって強制ではない。強制側は理由を説明できないためここが要る。親ディレクトリが存在しない拒否は `ENOENT` を返し、`EACCES` は Linux では拒否だが macOS では通常の失敗なので、errno による分類は成立しない。述語と強制は必ず食い違うが、この設計では食い違いが破れではなく「無駄な 1 ターン」か「余計な確認」に落ちる。

- [ ] **Step 1: 失敗するテストを書く**

`crates/polaris-tools/src/predicate.rs` の末尾へ置く。

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use polaris_sandbox::{SandboxMode, SandboxPolicy};

    fn workspace(root: &std::path::Path) -> SandboxPolicy {
        SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.to_path_buf()])
            .expect("方針を作れない")
    }

    #[test]
    fn a_target_inside_the_root_is_allowed() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = workspace(dir.path());
        assert_eq!(predict(&policy, &dir.path().join("a.txt")), Verdict::Allowed);
    }

    #[test]
    fn a_target_outside_the_root_needs_approval_and_says_where_it_may_write() {
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let outside = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = workspace(root.path());

        let target = outside.path().join("b.txt");
        let Verdict::NeedsApproval { reason } = predict(&policy, &target) else {
            panic!("ルート外が許可された");
        };
        // 仕様は拒否されたパス、現在の方針、書込可能ルートの 3 点を要求する。
        // どれか 1 つでも欠けると、モデルは同じ失敗を繰り返す。
        assert!(reason.contains(&target.display().to_string()), "パスが無い: {reason}");
        assert!(reason.contains("workspace-write"), "方針が無い: {reason}");
        assert!(
            reason.contains(&root.path().canonicalize().unwrap().display().to_string()),
            "書込可能ルートが無い: {reason}"
        );
    }

    #[test]
    fn a_target_whose_parent_does_not_exist_yet_is_still_judged_by_its_ancestors() {
        // write は新規作成なので、対象も途中のディレクトリも存在しないことが
        // 普通にある。存在しないパスは canonicalize できないため、素朴に
        // canonicalize すると全ての新規作成が「判定不能」に落ちる。
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = workspace(dir.path());
        let deep = dir.path().join("no/such/dir/c.txt");
        assert_eq!(predict(&policy, &deep), Verdict::Allowed);
    }

    #[test]
    fn a_symlink_pointing_outside_the_root_needs_approval() {
        // ルートの内側にあるシンボリックリンクが外を指していれば、
        // 見かけのパスは内側でも書き先は外側である。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let outside = tempfile::tempdir().expect("一時ディレクトリ");
        let victim = outside.path().join("victim.txt");
        std::fs::write(&victim, "original").expect("書けない");

        let link = root.path().join("looks-inside.txt");
        std::os::unix::fs::symlink(&victim, &link).expect("symlink");

        let policy = workspace(root.path());
        assert!(
            matches!(predict(&policy, &link), Verdict::NeedsApproval { .. }),
            "外を指すリンクが許可された"
        );
    }

    #[test]
    fn an_existing_hardlink_is_surfaced_for_approval() {
        // ハードリンク経由の書き込みは実サンドボックスでも拒否されない。
        // 両機構ともパスで判定し inode で判定しないためである（仕様の
        // 「保証しない範囲」参照）。述語側で気づける唯一の手掛かりが
        // リンク数なので、複数リンクを持つ既存ファイルは承認へ回す。
        // これは緩和であって保証ではない。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let outside = tempfile::tempdir().expect("一時ディレクトリ");
        let real = outside.path().join("real.txt");
        std::fs::write(&real, "original").expect("書けない");

        let linked = root.path().join("hardlink.txt");
        std::fs::hard_link(&real, &linked).expect("hard_link");

        let policy = workspace(root.path());
        let Verdict::NeedsApproval { reason } = predict(&policy, &linked) else {
            panic!("ハードリンクが素通りした");
        };
        assert!(reason.contains("ハードリンク"), "理由が伝わらない: {reason}");
    }

    #[test]
    fn a_sensitive_path_inside_the_root_still_needs_approval() {
        // OS の強制はワークスペース内側の機密ファイルを区別できない。
        // 「書込可能ルートの内側だから安全」ではないので、読み取り側と
        // 同じ path_policy をここでも通す。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = workspace(root.path());
        let secret = root.path().join(".env");
        assert!(
            matches!(predict(&policy, &secret), Verdict::NeedsApproval { .. }),
            ".env への書き込みが素通りした"
        );
    }

    #[test]
    fn read_only_needs_approval_for_any_write() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = SandboxPolicy::new(SandboxMode::ReadOnly, &[]).expect("方針");
        assert!(matches!(
            predict(&policy, &dir.path().join("a.txt")),
            Verdict::NeedsApproval { .. }
        ));
    }

    #[test]
    fn full_access_allows_any_path() {
        let policy = SandboxPolicy::new(SandboxMode::FullAccess, &[]).expect("方針");
        assert_eq!(
            predict(&policy, std::path::Path::new("/etc/hosts")),
            Verdict::Allowed
        );
    }
}
```

- [ ] **Step 2: 依存を足す**

`crates/polaris-tools/Cargo.toml` の `[dependencies]` へ加える。

```toml
polaris-sandbox = { path = "../polaris-sandbox" }
```

`crates/polaris-tools/src/lib.rs` のモジュール宣言へ加える。既存の並びは `path_policy`、`read`、`skill` なので、アルファベット順で `path_policy` の次に入る。

```rust
pub mod path_policy;
pub mod predicate;
pub mod read;
pub mod skill;
```

- [ ] **Step 3: テストを走らせて失敗を確認する**

Run: `cargo test -p polaris-tools predicate`
Expected: コンパイルエラー。`predict` も `Verdict` も無い。

- [ ] **Step 4: 実装する**

`crates/polaris-tools/src/predicate.rs`:

```rust
//! 書き込みが拒否されるかを事前に予測する。
//!
//! ここは助言であって強制ではない。強制は `polaris-sandbox` が OS へ委譲する。
//! 述語が要るのは、強制側が理由を説明できないからである。親ディレクトリが
//! 存在しない場合の拒否は `ENOENT` を返し、`EACCES` は Linux では拒否だが
//! macOS では通常の失敗なので、errno から「方針違反」を復元できない。
//!
//! 述語と強制は必ず食い違う。食い違いが破れにならないのは、述語が承認を
//! 求める側にしか倒れないためである。述語が甘ければ強制が止め、述語が
//! 厳しければ余計な確認が 1 回増える。

use std::path::{Path, PathBuf};

use polaris_sandbox::{SandboxMode, SandboxPolicy};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allowed,
    NeedsApproval { reason: String },
}

pub fn predict(policy: &SandboxPolicy, target: &Path) -> Verdict {
    if policy.mode() == SandboxMode::FullAccess {
        return Verdict::Allowed;
    }

    let resolved = resolve_for_judgement(target);

    if !policy.contains(&resolved) {
        return Verdict::NeedsApproval {
            reason: format!(
                "{} は書込可能な範囲の外にある。方針 {}",
                target.display(),
                policy.describe()
            ),
        };
    }

    // ワークスペースの内側であっても、機密として扱うパスは承認へ回す。
    // OS の強制はこの区別を表現できない。
    if crate::path_policy::is_denied(&resolved) {
        return Verdict::NeedsApproval {
            reason: format!(
                "{} は機密として扱うパスに該当する。方針 {}",
                target.display(),
                policy.describe()
            ),
        };
    }

    if has_extra_hard_links(&resolved) {
        return Verdict::NeedsApproval {
            reason: format!(
                "{} はハードリンクを持つ（書き込みが範囲外の実体へ届きうる）。方針 {}",
                target.display(),
                policy.describe()
            ),
        };
    }

    Verdict::Allowed
}

/// 判定用にパスを解決する。存在しないパスは canonicalize できないので、
/// 存在する最も近い祖先まで遡って正規化し、残りを繋ぎ直す。新規作成では
/// 対象も途中のディレクトリも存在しないのが普通であり、ここを素朴に
/// canonicalize すると全ての新規作成が判定不能になる。
fn resolve_for_judgement(target: &Path) -> PathBuf {
    if let Ok(c) = target.canonicalize() {
        return c;
    }

    let mut tail = Vec::new();
    let mut cursor = target;
    loop {
        match cursor.parent() {
            Some(parent) => {
                if let Some(name) = cursor.file_name() {
                    tail.push(name.to_owned());
                }
                if let Ok(c) = parent.canonicalize() {
                    let mut out = c;
                    for name in tail.iter().rev() {
                        out.push(name);
                    }
                    return out;
                }
                cursor = parent;
            }
            // ルートまで遡っても正規化できない。判定できないものを
            // 「内側」と答えないため、元のパスをそのまま返す。方針の
            // ルートと一致しないので、呼び出し側では承認へ倒れる。
            None => return target.to_path_buf(),
        }
    }
}

/// 既存ファイルが複数のリンクを持つか。存在しないパスとメタデータを
/// 読めないパスは偽を返す。ここで真を返せないケースがあることは、
/// 仕様の「保証しない範囲」に明記した限界そのものである。
fn has_extra_hard_links(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::symlink_metadata(path)
            .map(|m| m.is_file() && m.nlink() > 1)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}
```

- [ ] **Step 5: テストを走らせて通ることを確認する**

Run: `cargo test -p polaris-tools predicate`
Expected: 8 件すべて PASS。

- [ ] **Step 6: 変異で検証する**

1. `resolve_for_judgement` の遡り処理を削除して `target.to_path_buf()` だけを返す → `a_target_whose_parent_does_not_exist_yet_is_still_judged_by_its_ancestors` だけが落ちる
2. `has_extra_hard_links` を常に `false` を返すようにする → `an_existing_hardlink_is_surfaced_for_approval` だけが落ちる
3. `path_policy::is_denied` の分岐を削除 → `a_sensitive_path_inside_the_root_still_needs_approval` だけが落ちる
4. `reason` の書式から `policy.describe()` を落とす → `a_target_outside_the_root_needs_approval_and_says_where_it_may_write` だけが落ちる

いずれも変異が実際にファイルへ当たったことを読み直して確認してから走らせること。

- [ ] **Step 7: コミット**

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
git add crates/polaris-tools Cargo.lock
git commit -m "feat(tools): predict sandbox denials before attempting the write"
```

---

### Task 3: macOS の Seatbelt プロファイル生成

**Files:**
- Create: `crates/polaris-sandbox/src/macos.rs`
- Modify: `crates/polaris-sandbox/src/lib.rs`（`#[cfg(target_os = "macos")] pub mod macos;` を足す）

**Interfaces:**
- Consumes: `crate::policy::{SandboxMode, SandboxPolicy}`、`SandboxPolicy::{mode, writable_roots, describe}`
- Produces: `crate::macos::{SANDBOX_EXEC, build_profile, build_args}`。`pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";`、`pub fn build_profile(policy: &SandboxPolicy) -> String`、`pub fn build_args(policy: &SandboxPolicy, program: &Path, args: &[String]) -> Vec<String>`

このタスクは文字列と argv を組み立てるだけで、プロセスを起動しない。起動と実拒否の確認は Task 5 が行う。純粋関数に切り分けるのは、プロファイルの中身をプロセス起動なしに固定できるようにするためである。

パスをプロファイル本文へ直接埋め込まず、Seatbelt の `-D key=value` と `(param "KEY")` を使う。空白や特殊文字を含むパスで SBPL の引用規則を踏むのを避けるためであり、これは Codex CLI が実際に採っている方法でもある。

`sandbox-exec` は `/usr/bin` の実体だけを使い、`PATH` を引かない。`PATH` 上の同名バイナリで差し替えられる経路を塞ぐ。`/usr/bin/sandbox-exec` 自体が改竄されている状況では、攻撃者はすでに root を持っている。

- [ ] **Step 1: 失敗するテストを書く**

`crates/polaris-sandbox/src/macos.rs` の末尾へ置く。

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{SandboxMode, SandboxPolicy};

    fn workspace(roots: &[std::path::PathBuf]) -> SandboxPolicy {
        SandboxPolicy::new(SandboxMode::WorkspaceWrite, roots).expect("方針を作れない")
    }

    #[test]
    fn the_profile_starts_closed() {
        // deny default が無ければ、以降の allow は「既定で全許可の上に
        // 少し足す」ことになり、方針の意味が反転する。
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let p = build_profile(&workspace(&[dir.path().to_path_buf()]));
        assert!(p.contains("(deny default)"), "既定拒否が無い:\n{p}");
    }

    #[test]
    fn every_writable_root_gets_its_own_parameterised_subpath() {
        // ルートを 1 件でも落とすと、書けるはずの場所が黙って減る。
        let a = tempfile::tempdir().expect("一時ディレクトリ");
        let b = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = workspace(&[a.path().to_path_buf(), b.path().to_path_buf()]);
        let p = build_profile(&policy);

        assert!(p.contains("(subpath (param \"WRITABLE_ROOT_0\"))"), "{p}");
        assert!(p.contains("(subpath (param \"WRITABLE_ROOT_1\"))"), "{p}");
        assert_eq!(
            p.matches("WRITABLE_ROOT_").count(),
            2,
            "ルート数とパラメータ数が一致しない:\n{p}"
        );
    }

    #[test]
    fn paths_never_appear_verbatim_in_the_profile_body() {
        // パスを本文へ直接書くと、空白や括弧を含むパスで SBPL が壊れる。
        // 値は必ず -D 側へ渡す。
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = workspace(&[dir.path().to_path_buf()]);
        let p = build_profile(&policy);
        let root = policy.writable_roots()[0].display().to_string();
        assert!(!p.contains(&root), "パスが本文に埋め込まれている:\n{p}");
    }

    #[test]
    fn read_only_grants_no_write_at_all() {
        // read-only でルートは持てない（Task 1 で拒否される）。ここで見るのは
        // 書き込み許可の節そのものが出ないこと。空のルート一覧に対して
        // (allow file-write*) だけが裸で残ると、全書き込みが許可される。
        let policy = SandboxPolicy::new(SandboxMode::ReadOnly, &[]).expect("方針");
        let p = build_profile(&policy);
        assert!(!p.contains("file-write*"), "書き込み許可がある:\n{p}");
        assert!(p.contains("(allow file-read*)"), "読み取りが許可されていない:\n{p}");
    }

    #[test]
    fn full_access_still_produces_a_profile_so_the_path_is_the_same_one_we_test() {
        // full-access でも境界を越える。プロファイルを作らない分岐を設けると、
        // 試験する経路と本番の経路が別物になる。
        let policy = SandboxPolicy::new(SandboxMode::FullAccess, &[]).expect("方針");
        let p = build_profile(&policy);
        assert!(p.starts_with("(version 1)"), "{p}");
        assert!(p.contains("(allow default)"), "{p}");
    }

    #[test]
    fn args_pass_each_root_as_a_d_parameter_and_separate_the_command_with_dashdash() {
        let a = tempfile::tempdir().expect("一時ディレクトリ");
        let b = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = workspace(&[a.path().to_path_buf(), b.path().to_path_buf()]);

        let args = build_args(
            &policy,
            std::path::Path::new("/bin/echo"),
            &["hello".to_string()],
        );

        assert_eq!(args[0], "-p", "プロファイルの指定が先頭でない: {args:?}");
        let root0 = policy.writable_roots()[0].display();
        assert!(
            args.iter().any(|a| a == &format!("-DWRITABLE_ROOT_0={root0}")),
            "ルート 0 が -D で渡っていない: {args:?}"
        );
        let sep = args
            .iter()
            .position(|a| a == "--")
            .expect("-- が無い: コマンドと引数の境界が曖昧になる");
        assert_eq!(args[sep + 1], "/bin/echo");
        assert_eq!(args[sep + 2], "hello");
    }
}
```

- [ ] **Step 2: テストを走らせて失敗を確認する**

Run: `cargo test -p polaris-sandbox macos`
Expected: コンパイルエラー。`macos` モジュールが無い。

- [ ] **Step 3: 実装する**

`crates/polaris-sandbox/src/lib.rs` へ追加する。

```rust
#[cfg(target_os = "macos")]
pub mod macos;
```

`crates/polaris-sandbox/src/macos.rs`:

```rust
//! macOS の強制。実行時に Seatbelt プロファイルを組み立て、
//! `/usr/bin/sandbox-exec` へ渡して子を起動する。
//!
//! パスは本文へ埋め込まず `-D key=value` と `(param "KEY")` で渡す。
//! 空白や括弧を含むパスで SBPL の引用規則を踏まないためである。

use std::path::Path;

use crate::policy::{SandboxMode, SandboxPolicy};

/// `PATH` を引かない。`PATH` 上の同名バイナリで差し替えられる経路を塞ぐ。
/// この実体そのものが改竄されている状況では、攻撃者はすでに root を持つ。
pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// 方針から SBPL のプロファイル本文を組み立てる。
pub fn build_profile(policy: &SandboxPolicy) -> String {
    let mut p = String::from("(version 1)\n");

    if policy.mode() == SandboxMode::FullAccess {
        // 境界は越えるが制限しない。プロファイルを作らない分岐にしないのは、
        // 試験する経路と本番の経路を同一に保つためである。
        p.push_str("(allow default)\n");
        return p;
    }

    p.push_str("(deny default)\n");
    // シェルがシェルとして振る舞うために要る最低限。
    p.push_str("(allow process-fork)\n");
    p.push_str("(allow process-exec)\n");
    p.push_str("(allow signal (target same-sandbox))\n");
    p.push_str("(allow file-read* file-write* file-ioctl (literal \"/dev/ptmx\"))\n");
    p.push_str("(allow file-ioctl (regex #\"^/dev/ttys[0-9]+\"))\n");
    p.push_str("(allow file-read*)\n");

    let roots = policy.writable_roots();
    if !roots.is_empty() {
        p.push_str("(allow file-write*\n");
        for i in 0..roots.len() {
            p.push_str(&format!("  (subpath (param \"WRITABLE_ROOT_{i}\"))\n"));
        }
        p.push_str(")\n");
    }

    p
}

/// `sandbox-exec` へ渡す argv を組み立てる。プログラム本体と引数は
/// `--` の後ろへ置き、境界を曖昧にしない。
pub fn build_args(policy: &SandboxPolicy, program: &Path, args: &[String]) -> Vec<String> {
    let mut out = vec!["-p".to_string(), build_profile(policy)];
    for (i, root) in policy.writable_roots().iter().enumerate() {
        out.push(format!("-DWRITABLE_ROOT_{i}={}", root.display()));
    }
    out.push("--".to_string());
    out.push(program.display().to_string());
    out.extend(args.iter().cloned());
    out
}
```

- [ ] **Step 4: テストを走らせて通ることを確認する**

Run: `cargo test -p polaris-sandbox macos`
Expected: 6 件すべて PASS。

- [ ] **Step 5: 生成したプロファイルが実際に効くことを手で 1 回確かめる**

このタスクは起動しないが、組み立てた本文が SBPL として妥当かどうかは文字列テストでは分からない。次を 1 回だけ手で走らせ、結果を報告へ書くこと。

```bash
mkdir -p /tmp/polaris-sbtest && cd /tmp/polaris-sbtest
ROOT=$(cd /tmp/polaris-sbtest && pwd -P)
cat > p.sb <<'SB'
(version 1)
(deny default)
(allow process-fork)
(allow process-exec)
(allow signal (target same-sandbox))
(allow file-read* file-write* file-ioctl (literal "/dev/ptmx"))
(allow file-ioctl (regex #"^/dev/ttys[0-9]+"))
(allow file-read*)
(allow file-write*
  (subpath (param "WRITABLE_ROOT_0"))
)
SB
/usr/bin/sandbox-exec -p "$(cat p.sb)" -DWRITABLE_ROOT_0=$ROOT -- /bin/sh -c "echo ok > $ROOT/inside.txt" ; echo "内側 exit=$?"
/usr/bin/sandbox-exec -p "$(cat p.sb)" -DWRITABLE_ROOT_0=$ROOT -- /bin/sh -c 'echo pwned > /tmp/polaris-should-not-exist.txt' ; echo "外側 exit=$?"
ls /tmp/polaris-should-not-exist.txt 2>&1 | head -1
```

期待する結果は、内側が exit 0、外側が非 0、そして `/tmp/polaris-should-not-exist.txt` が存在しないことである。`$ROOT` に `pwd -P` を使うのは、`/tmp` が `/private/tmp` へのシンボリックリンクであり、正規化しないとルートが一致せず全てが拒否されるためである。

**この確認が期待どおりにならなかった場合は、実装を先へ進めず報告すること。** プロファイルの雛形が誤っていれば、Task 5 の受け入れテストは「拒否された」ように見えて実際は別の理由で失敗する。

- [ ] **Step 6: 変異で検証する**

1. `p.push_str("(deny default)\n")` を削除 → `the_profile_starts_closed` だけが落ちる
2. ルートのループを `for i in 0..1` に固定 → `every_writable_root_gets_its_own_parameterised_subpath` だけが落ちる
3. `-DWRITABLE_ROOT_{i}={}` の代わりにプロファイル本文へ `root.display()` を直接埋める → `paths_never_appear_verbatim_in_the_profile_body` だけが落ちる
4. `out.push("--".to_string())` を削除 → `args_pass_each_root_as_a_d_parameter_and_separate_the_command_with_dashdash` だけが落ちる

- [ ] **Step 7: コミット**

```bash
cargo clippy -p polaris-sandbox --all-targets -- -D warnings
cargo fmt --all -- --check
git add crates/polaris-sandbox
git commit -m "feat(sandbox): build the macOS Seatbelt profile and argv"
```

---

### Task 4: Linux の landlock 適用

**Files:**
- Create: `crates/polaris-sandbox/src/linux.rs`
- Modify: `crates/polaris-sandbox/src/lib.rs`（`#[cfg(target_os = "linux")] pub mod linux;` を足す）

**Interfaces:**
- Consumes: `crate::policy::{SandboxMode, SandboxPolicy}`、`crate::SandboxError`
- Produces: `crate::linux::apply_to_current_process(policy: &SandboxPolicy) -> Result<(), SandboxError>`。子の `pre_exec` から呼ぶ

**このタスクは開発機（macOS）ではコンパイルすらされない。** `#[cfg(target_os = "linux")]` の内側にあるため、`cargo test` は型検査もしない。検証はコンテナで行う。この機には `docker` と `podman` の両方がある。

**下に書くコードは `landlock` クレート 0.4 の API に対する私の下書きであり、実際の API と一致することを確認していない。** クレートの docs.rs か `~/.cargo/registry` の実体を読み、食い違ったら**木ではなくクレートに従い、どこがどう違ったかを報告すること。** 私の指示はこのセッションで 18 回誤っており、実装者が指摘するのが最も速い訂正経路になっている。

`restrict_self()` はスレッド単位で一方向である。呼んだスレッドと、それ以降に作られたスレッドや子プロセスにのみ効く。したがって走行中のハーネス本体では絶対に呼ばず、`Command::pre_exec` の内側、すなわち fork 後 exec 前の子でのみ呼ぶ。

- [ ] **Step 1: 実装する**

`crates/polaris-sandbox/src/lib.rs` へ追加する。

```rust
#[cfg(target_os = "linux")]
pub mod linux;
```

`crates/polaris-sandbox/src/linux.rs`:

```rust
//! Linux の強制。landlock の ruleset を子の中で自分自身へ適用する。
//!
//! `restrict_self()` はスレッド単位で一方向であり、呼んだ後に作られた
//! スレッドと子へ継承される。走行中のハーネス本体で呼ぶと、そのスレッドが
//! 恒久的に制限され、以降の全ての作業が巻き添えになる。呼ぶ場所は
//! `Command::pre_exec` の内側だけである。

use crate::policy::{SandboxMode, SandboxPolicy};
use crate::SandboxError;

/// 実用上の下限。ABI 1（カーネル 5.13）ではディレクトリを跨ぐ rename と
/// link を表現できず、エディタや多くのツールが使う「一時ファイルへ書いて
/// rename で置き換える」保存が扱えない。ABI 2（5.19）を下限とする。
const REQUIRED_ABI: landlock::ABI = landlock::ABI::V2;

/// 現在のプロセス（＝ fork 済みの子）へ方針を適用する。
///
/// `full-access` では何も適用しない。制限しないことが方針だからである。
/// ただし呼び出し側は `full-access` でも子を起こす。試験する経路と本番の
/// 経路を同一に保つためである。
pub fn apply_to_current_process(policy: &SandboxPolicy) -> Result<(), SandboxError> {
    use landlock::{
        Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr,
        RulesetStatus,
    };

    if policy.mode() == SandboxMode::FullAccess {
        return Ok(());
    }

    let mut ruleset = Ruleset::default()
        .handle_access(AccessFs::from_all(REQUIRED_ABI))
        .map_err(|e| SandboxError::NotEnforced(format!("ruleset を作れない: {e}")))?
        .create()
        .map_err(|e| SandboxError::NotEnforced(format!("ruleset を作れない: {e}")))?;

    // 読み取りは全体に許す。read-only と workspace-write の違いは
    // 書き込み側にしかない。
    ruleset = ruleset
        .add_rule(PathBeneath::new(
            PathFd::new("/").map_err(|e| SandboxError::NotEnforced(format!("/ を開けない: {e}")))?,
            AccessFs::from_read(REQUIRED_ABI),
        ))
        .map_err(|e| SandboxError::NotEnforced(format!("読み取り規則を足せない: {e}")))?;

    for root in policy.writable_roots() {
        ruleset = ruleset
            .add_rule(PathBeneath::new(
                PathFd::new(root).map_err(|e| {
                    SandboxError::NotEnforced(format!("{} を開けない: {e}", root.display()))
                })?,
                AccessFs::from_all(REQUIRED_ABI),
            ))
            .map_err(|e| SandboxError::NotEnforced(format!("書き込み規則を足せない: {e}")))?;
    }

    let status = ruleset
        .restrict_self()
        .map_err(|e| SandboxError::NotEnforced(format!("restrict_self に失敗: {e}")))?;

    // 適用されなかった状態は拒否ではない。ここを通してしまうと、
    // 守っていない状態が守っている状態と同じ見た目になる。
    if status.ruleset == RulesetStatus::NotEnforced {
        return Err(SandboxError::NotEnforced(
            "カーネルが landlock を強制しなかった（カーネルが古いか、seccomp に塞がれている）"
                .into(),
        ));
    }

    Ok(())
}
```

- [ ] **Step 2: コンテナで型検査と実拒否を確認する**

開発機では検証できないため、Linux コンテナで行う。`docker` と `podman` のどちらでもよい。

```bash
cd /Users/kn/File/projects/codex/polaris
docker run --rm -v "$PWD":/w -w /w rust:1.96 bash -c '
  set -e
  uname -r
  cargo check -p polaris-sandbox --all-targets
'
```

まずカーネル版と型検査を通す。カーネルが 5.19 未満なら landlock ABI 2 は使えないので、その事実を報告すること。

次に実拒否を確認する。`polaris-sandbox` に一時的なテストを足し、コンテナの中だけで走らせる。

```rust
#[cfg(all(test, target_os = "linux"))]
mod linux_enforcement {
    use super::*;
    use crate::policy::{SandboxMode, SandboxPolicy};

    #[test]
    fn a_write_outside_the_root_is_denied_by_the_real_kernel() {
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let outside = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("方針");

        let target = outside.path().join("should-not-exist.txt");
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(format!("echo pwned > {}", target.display()));

        let policy_for_child = policy.clone();
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(move || {
                crate::linux::apply_to_current_process(&policy_for_child)
                    .map_err(|e| std::io::Error::other(e.to_string()))
            });
        }

        let status = cmd.status().expect("起動できない");
        assert!(!status.success(), "ルート外への書き込みが成功した");
        assert!(!target.exists(), "ファイルが作られている: {}", target.display());
    }
}
```

```bash
docker run --rm -v "$PWD":/w -w /w rust:1.96 bash -c 'cargo test -p polaris-sandbox linux_enforcement -- --nocapture'
```

**この確認の結果は 3 通りあり、どれであっても報告すること。**

1. 拒否が観測できた。期待どおり
2. `restrict_self` が `NotEnforced` を返した。コンテナの seccomp が landlock のシステムコールを塞いでいる可能性が高い。`--security-opt seccomp=unconfined` を付けて再試行し、それで通るなら「既定のコンテナ設定では landlock が効かない」という事実として報告する。これは調査が「実験でしか決まらない」と名指しした点であり、答えが出ること自体に価値がある
3. コンパイルが通らない。私の API 下書きが誤っている。クレートの実体に合わせて直し、どこが違ったかを報告する

**2 が起きた場合、回避策を入れて緑にしないこと。** 適用されなかったことを硬い失敗として扱うのがこの設計の要点であり、テストを通すために硬さを緩めると、守っていない状態が守っている状態と同じ見た目になる。

- [ ] **Step 3: 変異で検証する（コンテナ内）**

`RulesetStatus::NotEnforced` の分岐を削除し、`a_write_outside_the_root_is_denied_by_the_real_kernel` が落ちることを確認する。落ちない場合、その環境では landlock が最初から効いていないことになるので、Step 2 の 2 番として報告すること。

- [ ] **Step 4: コミット**

コンテナ用の一時テストは残す。開発機では `cfg(target_os = "linux")` によりコンパイルされないため、macOS 側の 173 件には影響しない。

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
git add crates/polaris-sandbox
git commit -m "feat(sandbox): apply a landlock ruleset in the child on Linux"
```

---

### Task 5: `run_confined` と「適用の失敗」を拒否と区別する

**Files:**
- Create: `crates/polaris-sandbox/src/confine.rs`
- Modify: `crates/polaris-sandbox/src/lib.rs`（`pub mod confine;` と再エクスポート）

**Interfaces:**
- Consumes: `crate::macos::{SANDBOX_EXEC, build_args}`（macOS）、`crate::linux::apply_to_current_process`（Linux）、`crate::policy::SandboxPolicy`
- Produces: `crate::confine::{Outcome, run_confined}`。`pub struct Outcome { pub status: i32, pub stdout: String, pub stderr: String }`、`pub fn run_confined(policy: &SandboxPolicy, program: &Path, args: &[String], stdin: Option<&str>) -> Result<Outcome, SandboxError>`

このタスクが M2 の中核である。**「サンドボックスが適用に失敗した」と「サンドボックスが拒否した」は別の事象であり、前者を後者として扱ってはならない。** 入れ子や仮想化された環境では適用自体が失敗しうる。適用の失敗を拒否として扱うと、守っていない状態が守っている状態と同じ見た目になる。これはこのブランチが繰り返し踏んできた欠陥クラスそのものである。

`Outcome` は「子が走ったが失敗した」を表す。`SandboxError::NotEnforced` は「子を拘束できなかった」を表す。呼び出し側は前者をモデルへ返し、後者では実行そのものを諦める。

- [ ] **Step 1: 失敗するテストを書く**

`crates/polaris-sandbox/src/confine.rs` の末尾へ置く。

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{SandboxMode, SandboxPolicy};

    #[test]
    fn a_command_inside_the_root_succeeds_and_its_output_comes_back() {
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("方針");

        let target = policy.writable_roots()[0].join("inside.txt");
        let out = run_confined(
            &policy,
            std::path::Path::new("/bin/sh"),
            &["-c".into(), format!("echo ok > {}", target.display())],
            None,
        )
        .expect("拘束実行そのものが失敗した");

        assert_eq!(out.status, 0, "内側への書き込みが失敗した: {out:?}");
        assert_eq!(
            std::fs::read_to_string(&target).expect("読めない").trim(),
            "ok"
        );
    }

    #[test]
    fn a_write_outside_the_root_is_denied_by_the_real_sandbox() {
        // 受け入れ基準 3 の土台。モックを使わず、実際に書き込みを試みて
        // 拒否を観測する。ツール経由の確認は Task 8 が別に行う。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let outside = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("方針");

        let target = outside.path().canonicalize().expect("canonicalize").join("nope.txt");
        let out = run_confined(
            &policy,
            std::path::Path::new("/bin/sh"),
            &["-c".into(), format!("echo pwned > {}", target.display())],
            None,
        )
        .expect("拘束実行そのものが失敗した");

        assert_ne!(out.status, 0, "ルート外への書き込みが成功した: {out:?}");
        assert!(
            !target.exists(),
            "ファイルが作られている: {}",
            target.display()
        );
    }

    #[test]
    fn stdin_reaches_the_child() {
        // write / edit のヘルパは操作を標準入力から受け取る。ここが通らないと
        // 変更操作が一切成立しない。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("方針");

        let out = run_confined(
            &policy,
            std::path::Path::new("/bin/cat"),
            &[],
            Some("あいうえお"),
        )
        .expect("拘束実行そのものが失敗した");

        assert_eq!(out.status, 0, "{out:?}");
        assert_eq!(out.stdout.trim(), "あいうえお");
    }

    #[test]
    fn full_access_still_crosses_the_boundary() {
        // 制限しない方針でも子を起こす。ここで直接実行へ分岐すると、
        // 試験している経路と本番の経路が別物になる。
        let policy = SandboxPolicy::new(SandboxMode::FullAccess, &[]).expect("方針");
        let out = run_confined(
            &policy,
            std::path::Path::new("/bin/sh"),
            &["-c".into(), "echo crossed".into()],
            None,
        )
        .expect("拘束実行そのものが失敗した");
        assert_eq!(out.status, 0);
        assert_eq!(out.stdout.trim(), "crossed");
    }

    #[test]
    fn a_sandbox_that_failed_to_apply_is_an_error_not_a_denial() {
        // 適用の失敗を Outcome として返すと、呼び出し側はそれを方針違反と
        // 区別できない。区別できなければ、拘束されていない子が走ったことに
        // 誰も気づけない。ここでは classify_apply_failure を直接試す。
        assert!(
            classify_apply_failure("sandbox-exec: sandbox_apply: Operation not permitted").is_some(),
            "適用失敗を検出できていない"
        );
        assert!(
            classify_apply_failure("sh: /nope: Operation not permitted").is_none(),
            "通常の拒否を適用失敗と誤判定している"
        );
        assert!(classify_apply_failure("").is_none());
    }
}
```

- [ ] **Step 2: テストを走らせて失敗を確認する**

Run: `cargo test -p polaris-sandbox confine`
Expected: コンパイルエラー。`confine` モジュールが無い。

- [ ] **Step 3: 実装する**

`crates/polaris-sandbox/src/lib.rs` へ追加する。

```rust
pub mod confine;
pub use confine::{run_confined, Outcome};
```

`crates/polaris-sandbox/src/confine.rs`:

```rust
//! 拘束下でのプロセス起動。プラットフォームごとの実装をここで振り分ける。
//!
//! 返す `Outcome` は「子が走ったうえでの結果」である。子を拘束できなかった
//! 場合は `Outcome` ではなく `SandboxError::NotEnforced` を返す。両者を
//! 混ぜると、拘束されていない子が走ったことを呼び出し側が検出できない。

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::policy::SandboxPolicy;
use crate::SandboxError;

#[derive(Debug)]
pub struct Outcome {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

/// 拘束下で `program` を起動する。`stdin` を渡すと子の標準入力へ流し込む。
pub fn run_confined(
    policy: &SandboxPolicy,
    program: &Path,
    args: &[String],
    stdin: Option<&str>,
) -> Result<Outcome, SandboxError> {
    let mut cmd = build_command(policy, program, args)?;

    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());

    let mut child = cmd.spawn()?;

    if let Some(s) = stdin {
        child
            .stdin
            .as_mut()
            .ok_or_else(|| SandboxError::NotEnforced("標準入力を開けない".into()))?
            .write_all(s.as_bytes())?;
        // drop して EOF を送る。閉じないと `cat` のような子が待ち続ける。
        drop(child.stdin.take());
    }

    let out = child.wait_with_output()?;
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    // 拘束できなかった場合はここで打ち切る。子が走ったかどうかに関わらず、
    // 走った子が拘束されていた保証が無いためである。
    if let Some(detail) = classify_apply_failure(&stderr) {
        return Err(SandboxError::NotEnforced(detail));
    }

    Ok(Outcome {
        status: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr,
    })
}

#[cfg(target_os = "macos")]
fn build_command(
    policy: &SandboxPolicy,
    program: &Path,
    args: &[String],
) -> Result<Command, SandboxError> {
    let mut cmd = Command::new(crate::macos::SANDBOX_EXEC);
    cmd.args(crate::macos::build_args(policy, program, args));
    Ok(cmd)
}

#[cfg(target_os = "linux")]
fn build_command(
    policy: &SandboxPolicy,
    program: &Path,
    args: &[String],
) -> Result<Command, SandboxError> {
    use std::os::unix::process::CommandExt;

    let mut cmd = Command::new(program);
    cmd.args(args);

    let policy_for_child = policy.clone();
    // SAFETY: pre_exec は fork 後 exec 前の子でのみ走る。ここで呼ぶ
    // restrict_self はスレッド単位で一方向なので、親のスレッドには影響しない。
    // 呼び出す関数はメモリ確保を伴うが、この子は直後に exec するため
    // async-signal-safety の制約下にある区間は短く、landlock クレート自身が
    // この使い方を想定している。
    unsafe {
        cmd.pre_exec(move || {
            crate::linux::apply_to_current_process(&policy_for_child)
                .map_err(|e| std::io::Error::other(e.to_string()))
        });
    }
    Ok(cmd)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn build_command(
    _policy: &SandboxPolicy,
    _program: &Path,
    _args: &[String],
) -> Result<Command, SandboxError> {
    // 強制の委譲先が無い環境では、拘束されていない子を起こさない。
    // 「サンドボックスが無いので素通しで実行する」は、この設計では
    // 選択肢に入らない。
    Err(SandboxError::UnsupportedPlatform)
}

/// 標準エラーから「サンドボックスの適用そのものが失敗した」を検出する。
///
/// 方針違反による拒否とは別の事象である。macOS では `sandbox-exec` が
/// `sandbox_apply:` を含む行を出す。通常の拒否は子自身のエラーメッセージ
/// （`Operation not permitted` など）として現れるため、その文字列だけで
/// 判定すると両者を取り違える。
fn classify_apply_failure(stderr: &str) -> Option<String> {
    if stderr.contains("sandbox_apply") {
        return Some(format!("sandbox-exec が方針を適用できなかった: {stderr}"));
    }
    if stderr.contains("landlock") && stderr.contains("強制しなかった") {
        return Some(format!("landlock が強制されなかった: {stderr}"));
    }
    None
}
```

- [ ] **Step 4: テストを走らせて通ることを確認する**

Run: `cargo test -p polaris-sandbox confine -- --nocapture`
Expected: 5 件すべて PASS。**`a_write_outside_the_root_is_denied_by_the_real_sandbox` が通ることが M2 の要である。** これが「拒否された」ではなく「適用に失敗した」で落ちていないことを、`--nocapture` の出力で確認すること。

- [ ] **Step 5: 変異で検証する**

1. `build_args` の `(subpath (param "WRITABLE_ROOT_0"))` の行を出さないようにする → `a_command_inside_the_root_succeeds_and_its_output_comes_back` が落ちる（内側にも書けなくなる）
2. `build_profile` の `(deny default)` を `(allow default)` に置換 → `a_write_outside_the_root_is_denied_by_the_real_sandbox` だけが落ちる。**この変異が落ちないなら、そのテストは実際には何も守っていない**
3. `classify_apply_failure` を常に `None` を返すようにする → `a_sandbox_that_failed_to_apply_is_an_error_not_a_denial` だけが落ちる
4. `full-access` のときだけ `run_confined` を素通しの `Command::new(program)` に差し替える → `full_access_still_crosses_the_boundary` は通ってしまう。**これは意図的に検出できない変異である。** 経路が同一であることは出力からは見えないため、レビュー時に読んで確認する項目として報告に書くこと

- [ ] **Step 6: コミット**

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
git add crates/polaris-sandbox
git commit -m "feat(sandbox): run confined children and fail hard when the sandbox did not apply"
```

---

### Task 6: プロジェクトルートの解決

**Files:**
- Create: `crates/polaris-core/src/project.rs`
- Modify: `crates/polaris-core/src/lib.rs`（`pub mod project;`）

**Interfaces:**
- Consumes: なし
- Produces: `polaris_core::project::resolve_root(start: &Path) -> PathBuf`

M1 と M3a では「プロジェクトルート」がプロセスの作業ディレクトリそのものだった。`crates/polaris-core/` から起動すると AGENTS.md も設定も skills も黙って消える、という指摘を M3a から繰り越している。M2 ではこれが安全性の問題になる。書込可能ルートがプロジェクトルートから導かれるため、ルートを取り違えると書ける範囲が意図と食い違う。

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_subdirectory_resolves_to_the_repository_root() {
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        std::fs::create_dir(root.path().join(".git")).expect("mkdir");
        let deep = root.path().join("crates/polaris-core/src");
        std::fs::create_dir_all(&deep).expect("mkdir");

        assert_eq!(
            resolve_root(&deep),
            root.path().canonicalize().expect("canonicalize")
        );
    }

    #[test]
    fn a_polaris_directory_also_marks_the_root() {
        // git を使わない利用者もいる。`.polaris/` があればそこをルートとする。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        std::fs::create_dir(root.path().join(".polaris")).expect("mkdir");
        let deep = root.path().join("a/b");
        std::fs::create_dir_all(&deep).expect("mkdir");

        assert_eq!(
            resolve_root(&deep),
            root.path().canonicalize().expect("canonicalize")
        );
    }

    #[test]
    fn the_nearest_marker_wins() {
        // 入れ子のリポジトリでは内側が勝つ。外側を選ぶと、書込可能ルートが
        // 意図より広がる。
        let outer = tempfile::tempdir().expect("一時ディレクトリ");
        std::fs::create_dir(outer.path().join(".git")).expect("mkdir");
        let inner = outer.path().join("vendor/thing");
        std::fs::create_dir_all(inner.join(".git")).expect("mkdir");
        let deep = inner.join("src");
        std::fs::create_dir_all(&deep).expect("mkdir");

        assert_eq!(
            resolve_root(&deep),
            inner.canonicalize().expect("canonicalize")
        );
    }

    #[test]
    fn without_any_marker_the_starting_directory_is_the_root() {
        // 目印が無いときに `/` まで遡ると、書込可能ルートがファイルシステム
        // 全体になる。遡りは必ず止める。
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let deep = dir.path().join("x/y");
        std::fs::create_dir_all(&deep).expect("mkdir");

        assert_eq!(resolve_root(&deep), deep.canonicalize().expect("canonicalize"));
    }
}
```

- [ ] **Step 2: テストを走らせて失敗を確認する**

Run: `cargo test -p polaris-core project`
Expected: コンパイルエラー。`project` モジュールが無い。

- [ ] **Step 3: 実装する**

```rust
//! プロジェクトルートの解決。
//!
//! M2 以降、書込可能ルートはここが返す値から導かれる。作業ディレクトリを
//! そのままルートにすると、リポジトリの深い場所から起動しただけで書ける
//! 範囲が変わる。目印が無いときに `/` まで遡らないのは、そこで遡ると
//! 書込可能ルートがファイルシステム全体になるためである。

use std::path::{Path, PathBuf};

/// 目印となるディレクトリ。最も近いものを採る。
const MARKERS: &[&str] = &[".git", ".polaris"];

pub fn resolve_root(start: &Path) -> PathBuf {
    let canonical = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());

    let mut cursor: &Path = &canonical;
    loop {
        if MARKERS.iter().any(|m| cursor.join(m).exists()) {
            return cursor.to_path_buf();
        }
        match cursor.parent() {
            Some(p) => cursor = p,
            None => return canonical,
        }
    }
}
```

- [ ] **Step 4: テストを走らせて通ることを確認する**

Run: `cargo test -p polaris-core project`
Expected: 4 件すべて PASS。

- [ ] **Step 5: 変異で検証する**

1. `MARKERS` から `".polaris"` を外す → `a_polaris_directory_also_marks_the_root` だけが落ちる
2. 遡りの向きを変え、最初に見つけた祖先ではなく最も遠い祖先を返すようにする → `the_nearest_marker_wins` だけが落ちる
3. `None => return canonical` を `None => return PathBuf::from("/")` に置換 → `without_any_marker_the_starting_directory_is_the_root` だけが落ちる

- [ ] **Step 6: コミット**

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
git add crates/polaris-core
git commit -m "feat(core): resolve the project root instead of trusting the cwd"
```

---

### Task 7: 拘束ヘルパと、バイナリの退避

**Files:**
- Create: `crates/polaris-sandbox/src/helper.rs`
- Create: `crates/polaris-sandbox/src/stage.rs`
- Modify: `crates/polaris-sandbox/src/lib.rs`
- Modify: `crates/polaris-cli/src/main.rs`（拘束モードの入口）
- Modify: `crates/polaris-cli/Cargo.toml`（`polaris-sandbox` への依存）

**Interfaces:**
- Consumes: `crate::policy::SandboxPolicy`、`SandboxPolicy::{writable_roots}`、`crate::SandboxError`
- Produces: `crate::helper::{Mutation, apply}`、`crate::stage::staged_helper`。`pub enum Mutation { Write { path: PathBuf, content: String }, Edit { path: PathBuf, old: String, new: String } }`、`pub fn apply(m: &Mutation) -> Result<String, String>`、`pub fn staged_helper(policy: &SandboxPolicy, state_dir: &Path) -> Result<PathBuf, SandboxError>`

`write` と `edit` は、拘束された子の中で実際の変更を行う。子として起こすのは polaris 自身のバイナリであり、操作は JSON 1 件を標準入力から受け取る。シェルを挟まないのは、パスや内容に含まれる引用符と改行で壊れる経路を作らないためである。

**再実行するバイナリは書込可能ルートの外へ置く。** `current_exe()` は通常 `target/debug/polaris` にあり、ワークスペースの内側である。ワークスペースへ書ける者がそれを差し替えれば、次の変更操作が差し替えられたコードを拘束下で実行する。M4 で `read write` 型の subagent がこれを行えば、型が与えていない権限を得る経路になり、「subagent が親より強い権限を得る経路は存在しない」が破れる。

- [ ] **Step 1: 失敗するテストを書く**

`crates/polaris-sandbox/src/helper.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_creates_the_file_and_its_parents() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let target = dir.path().join("a/b/c.txt");
        let m = Mutation::Write {
            path: target.clone(),
            content: "本文".into(),
        };
        apply(&m).expect("失敗した");
        assert_eq!(std::fs::read_to_string(&target).expect("読めない"), "本文");
    }

    #[test]
    fn edit_replaces_exactly_one_occurrence() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let target = dir.path().join("f.txt");
        std::fs::write(&target, "前 xxx 後").expect("書けない");

        apply(&Mutation::Edit {
            path: target.clone(),
            old: "xxx".into(),
            new: "yyy".into(),
        })
        .expect("失敗した");

        assert_eq!(std::fs::read_to_string(&target).expect("読めない"), "前 yyy 後");
    }

    #[test]
    fn edit_refuses_when_the_marker_appears_more_than_once() {
        // どちらを置き換えたのかモデルに分からない置換は、成功として
        // 返してはならない。1 件目だけ黙って置き換えるのが最悪であり、
        // 通ったのに意図と違う結果が残る。
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let target = dir.path().join("f.txt");
        std::fs::write(&target, "xxx と xxx").expect("書けない");

        let err = apply(&Mutation::Edit {
            path: target.clone(),
            old: "xxx".into(),
            new: "yyy".into(),
        })
        .expect_err("複数一致が通ってしまった");
        assert!(err.contains("2"), "件数が伝わらない: {err}");
        assert_eq!(
            std::fs::read_to_string(&target).expect("読めない"),
            "xxx と xxx",
            "拒否したのに書き換わっている"
        );
    }

    #[test]
    fn edit_refuses_when_the_marker_is_absent() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let target = dir.path().join("f.txt");
        std::fs::write(&target, "何も無い").expect("書けない");

        let err = apply(&Mutation::Edit {
            path: target,
            old: "xxx".into(),
            new: "yyy".into(),
        })
        .expect_err("不一致が通ってしまった");
        assert!(err.contains("見つからない"), "{err}");
    }

    #[test]
    fn a_mutation_round_trips_through_json() {
        // ヘルパへは JSON 1 行で渡る。往復で壊れると変更操作が成立しない。
        let m = Mutation::Write {
            path: "/a/b".into(),
            content: "改行\nと \"引用符\"".into(),
        };
        let s = serde_json::to_string(&m).expect("直列化");
        let back: Mutation = serde_json::from_str(&s).expect("復元");
        match back {
            Mutation::Write { path, content } => {
                assert_eq!(path, std::path::PathBuf::from("/a/b"));
                assert_eq!(content, "改行\nと \"引用符\"");
            }
            other => panic!("別の変種になった: {other:?}"),
        }
    }
}
```

`crates/polaris-sandbox/src/stage.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{SandboxMode, SandboxPolicy};

    #[test]
    fn a_helper_inside_a_writable_root_is_copied_out_of_it() {
        // ここが M4 の権限昇格を塞ぐ。ワークスペースへ書ける者がヘルパを
        // 差し替えられる状態を残さない。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let state = tempfile::tempdir().expect("一時ディレクトリ");

        let fake_exe = root.path().join("polaris");
        std::fs::write(&fake_exe, b"#!/bin/sh\nexit 0\n").expect("書けない");

        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("方針");

        let staged = staged_helper_from(&policy, state.path(), &fake_exe).expect("退避できない");

        assert!(
            !staged.starts_with(policy.writable_roots()[0].as_path()),
            "退避先が書込可能ルートの内側にある: {}",
            staged.display()
        );
        assert!(staged.exists(), "退避先にファイルが無い");
    }

    #[test]
    fn a_helper_already_outside_every_root_is_used_as_is() {
        // 不要な複製をしない。インストール済みのバイナリを毎回コピーする
        // 必要は無い。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let elsewhere = tempfile::tempdir().expect("一時ディレクトリ");
        let state = tempfile::tempdir().expect("一時ディレクトリ");

        let exe = elsewhere.path().join("polaris");
        std::fs::write(&exe, b"x").expect("書けない");

        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("方針");

        let staged = staged_helper_from(&policy, state.path(), &exe).expect("解決できない");
        assert_eq!(staged, exe.canonicalize().expect("canonicalize"));
    }

    #[test]
    fn a_stale_staged_copy_is_refreshed_when_the_source_changes() {
        // 内容が変わったのに古い複製を使い続けると、直したはずのヘルパが
        // 動かない。しかも症状は「直っていない」であり、原因が見えにくい。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let state = tempfile::tempdir().expect("一時ディレクトリ");
        let exe = root.path().join("polaris");

        std::fs::write(&exe, b"version-1").expect("書けない");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("方針");
        let first = staged_helper_from(&policy, state.path(), &exe).expect("退避");
        assert_eq!(std::fs::read(&first).expect("読めない"), b"version-1");

        std::fs::write(&exe, b"version-2").expect("書けない");
        let second = staged_helper_from(&policy, state.path(), &exe).expect("退避");
        assert_eq!(
            std::fs::read(&second).expect("読めない"),
            b"version-2",
            "古い複製が使われている"
        );
    }
}
```

- [ ] **Step 2: テストを走らせて失敗を確認する**

Run: `cargo test -p polaris-sandbox helper stage`
Expected: コンパイルエラー。両モジュールとも無い。

- [ ] **Step 3: 実装する**

`crates/polaris-sandbox/src/lib.rs` へ追加する。

```rust
pub mod helper;
pub mod stage;
pub use helper::Mutation;
```

`crates/polaris-sandbox/src/helper.rs`:

```rust
//! 拘束された子の中で実行する変更操作。
//!
//! 親から子へは JSON 1 件で渡る。シェルを挟まないのは、パスや内容に含まれる
//! 引用符と改行で壊れる経路を作らないためである。

use std::path::PathBuf;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Mutation {
    Write { path: PathBuf, content: String },
    Edit { path: PathBuf, old: String, new: String },
}

/// 変更を実行する。成功なら人間とモデルの双方が読める 1 行を返す。
pub fn apply(m: &Mutation) -> Result<String, String> {
    match m {
        Mutation::Write { path, content } => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            std::fs::write(path, content).map_err(|e| e.to_string())?;
            Ok(format!("{} へ {} バイト書いた", path.display(), content.len()))
        }
        Mutation::Edit { path, old, new } => {
            let body = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
            let hits = body.matches(old.as_str()).count();
            match hits {
                0 => Err(format!("{} に置換対象が見つからない", path.display())),
                1 => {
                    let out = body.replace(old.as_str(), new.as_str());
                    std::fs::write(path, out).map_err(|e| e.to_string())?;
                    Ok(format!("{} を 1 箇所置換した", path.display()))
                }
                // どちらを置き換えたのか呼び出し側に分からない置換は成功と
                // して返さない。1 件目だけ黙って置き換えるのが最悪であり、
                // 通ったのに意図と違う結果が残る。
                n => Err(format!(
                    "{} に置換対象が {n} 箇所ある。一意に定まる文字列を渡すこと",
                    path.display()
                )),
            }
        }
    }
}
```

`crates/polaris-sandbox/src/stage.rs`:

```rust
//! ヘルパ用バイナリを書込可能ルートの外へ退避する。
//!
//! `current_exe()` は通常ビルド生成物の中、すなわちワークスペースの内側に
//! ある。ワークスペースへ書ける者がそれを差し替えれば、次の変更操作が
//! 差し替えられたコードを拘束下で実行する。M4 の `read write` 型 subagent に
//! とって、これは型が与えていない権限を得る経路そのものになる。

use std::path::{Path, PathBuf};

use crate::policy::SandboxPolicy;
use crate::SandboxError;

/// 実行中のバイナリを退避したうえでその場所を返す。
pub fn staged_helper(policy: &SandboxPolicy, state_dir: &Path) -> Result<PathBuf, SandboxError> {
    let exe = std::env::current_exe()?;
    staged_helper_from(policy, state_dir, &exe)
}

/// テストから実体を差し替えられるようにした本体。
pub fn staged_helper_from(
    policy: &SandboxPolicy,
    state_dir: &Path,
    exe: &Path,
) -> Result<PathBuf, SandboxError> {
    let canonical = exe.canonicalize()?;

    let inside_writable = policy
        .writable_roots()
        .iter()
        .any(|r| canonical.starts_with(r));
    if !inside_writable {
        return Ok(canonical);
    }

    std::fs::create_dir_all(state_dir)?;
    let dest = state_dir.join("polaris-helper");

    // 内容が変わっていれば必ず複製し直す。古い複製を使い続けると、
    // 直したはずのヘルパが動かないうえ、症状が「直っていない」なので
    // 原因が見えにくい。サイズと更新時刻ではなく中身で比べる。
    let need_copy = match std::fs::read(&dest) {
        Ok(existing) => existing != std::fs::read(&canonical)?,
        Err(_) => true,
    };
    if need_copy {
        std::fs::copy(&canonical, &dest)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o700))?;
        }
    }

    Ok(dest.canonicalize()?)
}
```

- [ ] **Step 4: CLI に拘束モードの入口を作る**

`crates/polaris-cli/Cargo.toml` の `[dependencies]` へ `polaris-sandbox = { path = "../polaris-sandbox" }` を足す。

`crates/polaris-cli/src/main.rs` の `Args` へ足す。

```rust
    /// 拘束された子として 1 件の変更操作を標準入力から読んで実行する。
    /// 内部用であり、利用者が直接使うものではない。
    #[arg(long, hide = true)]
    confined_apply: bool,
```

`main` の先頭、`POLARIS_API_KEY` を読むより前に置く。ヘルパ経路は設定も skills もプロバイダも読まない。読ませると、拘束下の子が余計なファイルへ触れ、攻撃面が広がる。

```rust
    if args.confined_apply {
        return run_confined_apply();
    }
```

```rust
/// 拘束された子としての入口。標準入力の JSON 1 件を実行して終わる。
fn run_confined_apply() -> ExitCode {
    use std::io::Read;

    let mut buf = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
        eprintln!("標準入力を読めない: {e}");
        return ExitCode::FAILURE;
    }
    let mutation: polaris_sandbox::Mutation = match serde_json::from_str(&buf) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("操作を解釈できない: {e}");
            return ExitCode::FAILURE;
        }
    };
    match polaris_sandbox::helper::apply(&mutation) {
        Ok(msg) => {
            println!("{msg}");
            ExitCode::SUCCESS
        }
        Err(msg) => {
            eprintln!("{msg}");
            ExitCode::FAILURE
        }
    }
}
```

`serde_json` を `crates/polaris-cli/Cargo.toml` の依存へ足すこと。

- [ ] **Step 5: テストを走らせて通ることを確認する**

Run: `cargo test -p polaris-sandbox helper stage`
Expected: 8 件すべて PASS。

- [ ] **Step 6: 変異で検証する**

1. `inside_writable` の判定を常に `false` にする → `a_helper_inside_a_writable_root_is_copied_out_of_it` だけが落ちる
2. `need_copy` を常に `false` にする → `a_stale_staged_copy_is_refreshed_when_the_source_changes` だけが落ちる
3. `apply` の `Edit` から複数一致の分岐を消し、`replace` を素通しにする → `edit_refuses_when_the_marker_appears_more_than_once` だけが落ちる

- [ ] **Step 7: コミット**

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
git add crates/polaris-sandbox crates/polaris-cli Cargo.lock
git commit -m "feat(sandbox): apply mutations in a confined child from a staged binary"
```

---

### Task 8: `write` と `edit` ツール

**Files:**
- Create: `crates/polaris-tools/src/write.rs`
- Create: `crates/polaris-tools/src/edit.rs`
- Modify: `crates/polaris-tools/src/lib.rs`（モジュール宣言、`ToolError` の追加、`all_specs` へ 2 本）

**Interfaces:**
- Consumes: `polaris_sandbox::{SandboxPolicy, run_confined, Mutation, SandboxError}`、`polaris_sandbox::Outcome { status, stdout, stderr }`
- Produces: `polaris_tools::write::write(policy: &SandboxPolicy, helper: &Path, path: &Path, content: &str) -> Result<String, ToolError>`、`polaris_tools::edit::edit(policy: &SandboxPolicy, helper: &Path, path: &Path, old: &str, new: &str) -> Result<String, ToolError>`、`ToolError::Sandbox`

2 本を 1 タスクにまとめるのは、経路が完全に同一で `Mutation` の変種だけが違うためである。レビュアが片方を認めてもう片方を退ける状況が無い。

**受け入れ基準 3 の確認はここで行う。** 仕様は「`write` ツールを通して書き込みを試み、拒否を観測する。共有の起動ヘルパを直接叩く経路では確認したことにならない」と定めている。Task 5 の `run_confined` 直叩きは土台の確認であって、基準の充足ではない。

- [ ] **Step 1: 失敗するテストを書く**

`crates/polaris-tools/src/write.rs` の末尾へ置く。ヘルパのパスは、テストでは polaris のバイナリではなく小さなシェルスクリプトを使う。ツールがヘルパを起こして JSON を渡し、結果を解釈するという経路そのものを見るためであり、CLI 全体をビルドしなくても確かめられる。

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use polaris_sandbox::{SandboxMode, SandboxPolicy};

    /// 標準入力の JSON を読んで `path` へ `content` を書くだけの、最小の
    /// ヘルパ代役。polaris 本体を経由せずにツール側の経路を確かめる。
    fn fake_helper(dir: &std::path::Path) -> std::path::PathBuf {
        let p = dir.join("fake-helper");
        std::fs::write(
            &p,
            r#"#!/bin/sh
python3 -c '
import json,sys,os
m=json.load(sys.stdin)
os.makedirs(os.path.dirname(m["path"]), exist_ok=True)
open(m["path"],"w").write(m["content"])
print("wrote")
'
"#,
        )
        .expect("書けない");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        p
    }

    #[test]
    fn a_write_inside_the_root_succeeds_through_the_confined_helper() {
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let helper_dir = tempfile::tempdir().expect("一時ディレクトリ");
        let helper = fake_helper(helper_dir.path());
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("方針");

        let target = policy.writable_roots()[0].join("out.txt");
        let msg = write(&policy, &helper, &target, "本文").expect("失敗した");

        assert_eq!(std::fs::read_to_string(&target).expect("読めない"), "本文");
        assert!(!msg.trim().is_empty(), "結果の説明が空");
    }

    #[test]
    fn a_write_outside_the_root_is_denied_by_the_real_sandbox_through_the_tool() {
        // 受け入れ基準 3。モックを使わず、ツールを通して実際に書き込みを
        // 試み、拒否を観測する。ヘルパを直接叩く経路では基準を満たさない。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let outside = tempfile::tempdir().expect("一時ディレクトリ");
        let helper_dir = tempfile::tempdir().expect("一時ディレクトリ");
        let helper = fake_helper(helper_dir.path());
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("方針");

        let target = outside
            .path()
            .canonicalize()
            .expect("canonicalize")
            .join("pwned.txt");
        let err = write(&policy, &helper, &target, "本文").expect_err("ルート外への書き込みが成功した");

        assert!(
            !target.exists(),
            "ファイルが作られている: {}",
            target.display()
        );
        // 拒否メッセージは、拒否されたパスと方針と書込可能ルートを含む。
        let msg = err.to_string();
        assert!(msg.contains(&target.display().to_string()), "パスが無い: {msg}");
        assert!(msg.contains("workspace-write"), "方針が無い: {msg}");
    }

    #[test]
    fn a_helper_that_cannot_be_confined_is_an_error_not_a_silent_success() {
        // 拘束できなかったのに書けてしまう状態を作らない。
        // 存在しないヘルパを渡すと run_confined が Io で失敗する。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("方針");
        let missing = root.path().join("no-such-helper");
        let target = policy.writable_roots()[0].join("x.txt");

        assert!(write(&policy, &missing, &target, "本文").is_err());
        assert!(!target.exists());
    }
}
```

`crates/polaris-tools/src/edit.rs` の末尾へ置く。

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use polaris_sandbox::{SandboxMode, SandboxPolicy};

    fn fake_helper(dir: &std::path::Path) -> std::path::PathBuf {
        let p = dir.join("fake-edit-helper");
        std::fs::write(
            &p,
            r#"#!/bin/sh
python3 -c '
import json,sys
m=json.load(sys.stdin)
body=open(m["path"]).read()
n=body.count(m["old"])
if n != 1:
    sys.stderr.write("置換対象が %d 箇所" % n); sys.exit(1)
open(m["path"],"w").write(body.replace(m["old"], m["new"]))
print("edited")
'
"#,
        )
        .expect("書けない");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        p
    }

    #[test]
    fn an_edit_inside_the_root_succeeds() {
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let helper_dir = tempfile::tempdir().expect("一時ディレクトリ");
        let helper = fake_helper(helper_dir.path());
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("方針");

        let target = policy.writable_roots()[0].join("f.txt");
        std::fs::write(&target, "前 xxx 後").expect("書けない");

        edit(&policy, &helper, &target, "xxx", "yyy").expect("失敗した");
        assert_eq!(std::fs::read_to_string(&target).expect("読めない"), "前 yyy 後");
    }

    #[test]
    fn a_failing_edit_returns_the_child_s_reason_rather_than_a_bare_exit_code() {
        // 「exit 1」だけを返すと、モデルは何を直せばよいか分からず同じ
        // 失敗を繰り返す。往復とトークンの浪費になる。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let helper_dir = tempfile::tempdir().expect("一時ディレクトリ");
        let helper = fake_helper(helper_dir.path());
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("方針");

        let target = policy.writable_roots()[0].join("f.txt");
        std::fs::write(&target, "xxx と xxx").expect("書けない");

        let err = edit(&policy, &helper, &target, "xxx", "yyy").expect_err("複数一致が通った");
        assert!(err.to_string().contains("2 箇所"), "理由が伝わらない: {err}");
    }
}
```

- [ ] **Step 2: テストを走らせて失敗を確認する**

Run: `cargo test -p polaris-tools write edit`
Expected: コンパイルエラー。両モジュールとも無い。

- [ ] **Step 3: `ToolError` を広げる**

`crates/polaris-tools/src/lib.rs` の `ToolError` へ追加する。

```rust
    #[error("サンドボックス: {0}")]
    Sandbox(#[from] polaris_sandbox::SandboxError),
    #[error("{path} への書き込みは拒否された。方針 {policy}。子の出力: {detail}")]
    WriteDenied {
        path: String,
        policy: String,
        detail: String,
    },
```

- [ ] **Step 4: 実装する**

`crates/polaris-tools/src/write.rs`:

```rust
//! `write` ツール。実際の書き込みは拘束された子の中で起きる。
//!
//! プロセス内で `std::fs::write` を呼ばないのは、OS の強制がプロセス境界で
//! しか効かないためである。プロセス内で書けば、守っているのはこのクレートの
//! パス判定だけになり、判定の誤りがそのまま範囲外への書き込みになる。

use std::path::Path;

use polaris_sandbox::{run_confined, Mutation, SandboxPolicy};

use crate::ToolError;

pub fn write(
    policy: &SandboxPolicy,
    helper: &Path,
    path: &Path,
    content: &str,
) -> Result<String, ToolError> {
    let mutation = Mutation::Write {
        path: path.to_path_buf(),
        content: content.to_string(),
    };
    run_mutation(policy, helper, &mutation, path)
}

/// `write` と `edit` が共有する起動と結果の解釈。
pub(crate) fn run_mutation(
    policy: &SandboxPolicy,
    helper: &Path,
    mutation: &Mutation,
    path: &Path,
) -> Result<String, ToolError> {
    let payload = serde_json::to_string(mutation).map_err(|e| {
        ToolError::Io(std::io::Error::other(format!("操作を直列化できない: {e}")))
    })?;

    let outcome = run_confined(
        policy,
        helper,
        &["--confined-apply".to_string()],
        Some(&payload),
    )?;

    if outcome.status == 0 {
        return Ok(outcome.stdout.trim().to_string());
    }

    Err(ToolError::WriteDenied {
        path: path.display().to_string(),
        policy: policy.describe(),
        detail: outcome.stderr.trim().to_string(),
    })
}
```

`crates/polaris-tools/src/edit.rs`:

```rust
//! `edit` ツール。`write` と同じ拘束経路を通る。

use std::path::Path;

use polaris_sandbox::{Mutation, SandboxPolicy};

use crate::ToolError;

pub fn edit(
    policy: &SandboxPolicy,
    helper: &Path,
    path: &Path,
    old: &str,
    new: &str,
) -> Result<String, ToolError> {
    let mutation = Mutation::Edit {
        path: path.to_path_buf(),
        old: old.to_string(),
        new: new.to_string(),
    };
    crate::write::run_mutation(policy, helper, &mutation, path)
}
```

- [ ] **Step 5: ツール定義を足す**

`crates/polaris-tools/src/lib.rs`:

```rust
pub fn all_specs() -> Vec<ToolSpec> {
    vec![read_spec(), write_spec(), edit_spec(), skill_spec()]
}

fn write_spec() -> ToolSpec {
    ToolSpec {
        name: "write",
        description: "ファイルを新規作成する。既存ファイルは上書きする。途中のディレクトリは作る。",
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "書き込む先のパス。" },
                "content": { "type": "string", "description": "ファイル全体の内容。" }
            },
            "required": ["path", "content"]
        }),
    }
}

fn edit_spec() -> ToolSpec {
    ToolSpec {
        name: "edit",
        description: "既存ファイルの一部を置き換える。old はファイル内で一意に定まる文字列にすること。複数一致すると失敗する。",
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "編集するファイルのパス。" },
                "old": { "type": "string", "description": "置換前の文字列。ファイル内で一意であること。" },
                "new": { "type": "string", "description": "置換後の文字列。" }
            },
            "required": ["path", "old", "new"]
        }),
    }
}
```

- [ ] **Step 6: テストを走らせて通ることを確認する**

Run: `cargo test --workspace`
Expected: 全件 PASS。ツール本数が 4 本になったので、予算のテストが落ちる場合は Task 12 で扱う。**落ちた場合はここで数値を無理に合わせず、落ちたことを報告すること。**

- [ ] **Step 7: 変異で検証する**

1. `build_profile` の `(deny default)` を `(allow default)` に置換 → `a_write_outside_the_root_is_denied_by_the_real_sandbox_through_the_tool` だけが落ちる。**これが M2 の受け入れ基準を守っている唯一の変異である**
2. `run_mutation` の `outcome.status == 0` 判定を無条件 `Ok` に置換 → 同じテストが落ちる
3. `ToolError::WriteDenied` の書式から `policy.describe()` を落とす → 同じテストが落ちる
4. `edit` の `detail` を空文字列に固定 → `a_failing_edit_returns_the_child_s_reason_rather_than_a_bare_exit_code` だけが落ちる

- [ ] **Step 8: コミット**

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
git add crates/polaris-tools Cargo.lock
git commit -m "feat(tools): add write and edit through the confined helper"
```

---

### Task 9: `bash` ツール

**Files:**
- Create: `crates/polaris-tools/src/bash.rs`
- Modify: `crates/polaris-tools/src/lib.rs`（モジュール宣言と `all_specs` へ 1 本）

**Interfaces:**
- Consumes: `polaris_sandbox::{SandboxPolicy, run_confined}`、`polaris_sandbox::Outcome`
- Produces: `polaris_tools::bash::run(policy: &SandboxPolicy, command: &str) -> Result<String, ToolError>`、`pub const MAX_OUTPUT_BYTES: usize`

`bash` は述語を通さない。任意のコードを実行するので、何に触れるかを事前に決定できないためである。試行し、拒否されたらモデルへ理由を返す。仕様のエラー処理節が定める拒否メッセージはこの経路のためにある。

出力には上限を置き、切り詰めたときは切り詰めたことを本文で述べる。印の無い部分的な結果は、完全な答えとして提示された誤った答えである。`grep` と `find` は独立したツールにせずここを通るので、大きな出力は例外ではなく通常の事象である。

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use polaris_sandbox::{SandboxMode, SandboxPolicy};

    fn workspace(root: &std::path::Path) -> SandboxPolicy {
        SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.to_path_buf()]).expect("方針")
    }

    #[test]
    fn stdout_comes_back() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let out = run(&workspace(dir.path()), "echo こんにちは").expect("失敗");
        assert!(out.contains("こんにちは"), "{out}");
    }

    #[test]
    fn a_failing_command_returns_its_stderr_and_its_exit_code() {
        // 終了コードだけを返すと、モデルは何が悪かったのか分からず同じ
        // コマンドを繰り返す。
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let err = run(&workspace(dir.path()), "echo 失敗の理由 >&2; exit 3")
            .expect_err("失敗が成功として返った");
        let msg = err.to_string();
        assert!(msg.contains("失敗の理由"), "標準エラーが無い: {msg}");
        assert!(msg.contains('3'), "終了コードが無い: {msg}");
    }

    #[test]
    fn a_write_outside_the_root_is_denied_and_the_message_names_the_policy() {
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let outside = tempfile::tempdir().expect("一時ディレクトリ");
        let target = outside
            .path()
            .canonicalize()
            .expect("canonicalize")
            .join("pwned.txt");

        let err = run(
            &workspace(root.path()),
            &format!("echo pwned > {}", target.display()),
        )
        .expect_err("ルート外への書き込みが成功した");

        assert!(!target.exists(), "ファイルが作られている");
        assert!(
            err.to_string().contains("workspace-write"),
            "方針が伝わらない: {err}"
        );
    }

    #[test]
    fn a_write_inside_the_root_succeeds() {
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = workspace(root.path());
        let target = policy.writable_roots()[0].join("ok.txt");
        run(&policy, &format!("echo ok > {}", target.display())).expect("内側への書き込みが失敗");
        assert!(target.exists());
    }

    #[test]
    fn oversized_output_is_truncated_and_says_so() {
        // 上限そのものを試す。短い入力で「切り詰めなかった」ことだけを見る
        // テストは、上限を何桁変えても通ってしまう。
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let out = run(
            &workspace(dir.path()),
            &format!("head -c {} /dev/zero | tr '\\0' 'a'", MAX_OUTPUT_BYTES * 2),
        )
        .expect("失敗");

        assert!(out.len() < MAX_OUTPUT_BYTES * 2, "切り詰められていない");
        assert!(out.contains("切り詰め"), "切り詰めたことが本文に無い");
    }

    #[test]
    fn truncation_lands_on_a_character_boundary() {
        // バイト単位で切ると多バイト文字の途中で割れる。
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let n = MAX_OUTPUT_BYTES / 3 + 10;
        let out = run(&workspace(dir.path()), &format!("for i in $(seq {n}); do printf 'あ'; done"))
            .expect("失敗");
        assert!(out.is_char_boundary(out.len()), "文字境界で切れていない");
    }
}
```

- [ ] **Step 2: テストを走らせて失敗を確認する**

Run: `cargo test -p polaris-tools bash`
Expected: コンパイルエラー。`bash` モジュールが無い。

- [ ] **Step 3: 実装する**

```rust
//! `bash` ツール。拘束された `/bin/sh` の中でコマンドを走らせる。
//!
//! 述語を通さない。任意のコードを実行するため、何に触れるかを事前に
//! 決定できないからである。試行し、拒否されたら理由をモデルへ返す。

use std::path::Path;

use polaris_sandbox::{run_confined, SandboxPolicy};

use crate::ToolError;

/// 返す出力の上限。`grep` と `find` がここを通るため、大きな出力は
/// 例外ではなく通常の事象である。
pub const MAX_OUTPUT_BYTES: usize = 32 * 1024;

pub fn run(policy: &SandboxPolicy, command: &str) -> Result<String, ToolError> {
    let outcome = run_confined(
        policy,
        Path::new("/bin/sh"),
        &["-c".to_string(), command.to_string()],
        None,
    )?;

    let combined = if outcome.stderr.trim().is_empty() {
        outcome.stdout
    } else {
        format!("{}{}", outcome.stdout, outcome.stderr)
    };
    let body = truncate(&combined);

    if outcome.status == 0 {
        return Ok(body);
    }

    Err(ToolError::CommandFailed {
        status: outcome.status,
        policy: policy.describe(),
        detail: body,
    })
}

/// 上限を超えたら文字境界で切り、切り詰めたことを本文で述べる。
/// 印の無い部分的な結果は、完全な答えとして提示された誤った答えである。
fn truncate(s: &str) -> String {
    if s.len() <= MAX_OUTPUT_BYTES {
        return s.to_string();
    }
    let mut end = MAX_OUTPUT_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[出力が {} バイトを超えたのでここで切り詰めた。必要なら範囲を絞って再実行すること]",
        &s[..end],
        MAX_OUTPUT_BYTES
    )
}
```

`ToolError` へ追加する。

```rust
    #[error("コマンドが終了コード {status} で失敗した。方針 {policy}。出力: {detail}")]
    CommandFailed {
        status: i32,
        policy: String,
        detail: String,
    },
```

`all_specs` へ追加する。

```rust
fn bash_spec() -> ToolSpec {
    ToolSpec {
        name: "bash",
        description: "シェルコマンドを実行する。grep と find もここから使う。サンドボックスの外への書き込みは拒否される。",
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "/bin/sh -c へ渡すコマンド行。" }
            },
            "required": ["command"]
        }),
    }
}
```

`all_specs()` は `vec![read_spec(), write_spec(), edit_spec(), bash_spec(), skill_spec()]` の 5 本になる。**上限は 6 本であり、`spawn` を M4 で足すと 6 本に達する。ここで 6 本を超えていないことを既存のテストが見ている。**

- [ ] **Step 4: テストを走らせて通ることを確認する**

Run: `cargo test -p polaris-tools bash`
Expected: 6 件すべて PASS。

- [ ] **Step 5: 変異で検証する**

1. `truncate` の上限判定を `s.len() <= usize::MAX` に置換 → `oversized_output_is_truncated_and_says_so` だけが落ちる
2. `truncate` の切り詰め文言を消して `&s[..end]` だけを返す → 同じテストが落ちる
3. `while end > 0 && !s.is_char_boundary(end)` を削除して `&s[..MAX_OUTPUT_BYTES]` にする → `truncation_lands_on_a_character_boundary` が panic で落ちる
4. `outcome.status == 0` を無条件 `Ok` に置換 → `a_failing_command_returns_its_stderr_and_its_exit_code` と `a_write_outside_the_root_is_denied_and_the_message_names_the_policy` が落ちる

- [ ] **Step 6: コミット**

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
git add crates/polaris-tools
git commit -m "feat(tools): add bash inside the sandbox with bounded, marked output"
```

---

### Task 10: 承認境界

**Files:**
- Create: `crates/polaris-core/src/approval.rs`
- Modify: `crates/polaris-core/src/lib.rs`

**Interfaces:**
- Consumes: `polaris_tools::predicate::{Verdict, predict}`、`polaris_sandbox::SandboxPolicy`
- Produces: `polaris_core::approval::{ApprovalPolicy, Decision, Approver, Gate}`。`pub enum ApprovalPolicy { Never, OnRequest, Always }`、`pub enum Decision { Allow, Deny }`、`pub trait Approver { fn ask(&mut self, reason: &str) -> Decision; }`、`pub struct Gate`、`Gate::new(policy: ApprovalPolicy) -> Gate`、`Gate::check(&mut self, sandbox: &SandboxPolicy, target: &Path, approver: &mut dyn Approver) -> Result<(), String>`

`sandbox_mode` が技術的境界を、`approval_policy` が停止して確認する条件を定める。二つは直交する。承認を求める相手を trait にするのは、テストが実際の端末入力を要らないようにするためである。

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use polaris_sandbox::{SandboxMode, SandboxPolicy};

    struct Scripted {
        answers: Vec<Decision>,
        asked: Vec<String>,
    }
    impl Approver for Scripted {
        fn ask(&mut self, reason: &str) -> Decision {
            self.asked.push(reason.to_string());
            self.answers.remove(0)
        }
    }

    fn workspace(root: &std::path::Path) -> SandboxPolicy {
        SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.to_path_buf()]).expect("方針")
    }

    #[test]
    fn a_target_inside_the_root_is_never_asked_about() {
        // 範囲内の書き込みで毎回止まると、承認が意味を失う。
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let sandbox = workspace(dir.path());
        let mut approver = Scripted { answers: vec![], asked: vec![] };
        let mut gate = Gate::new(ApprovalPolicy::OnRequest);

        gate.check(&sandbox, &sandbox.writable_roots()[0].join("a.txt"), &mut approver)
            .expect("範囲内が拒否された");
        assert!(approver.asked.is_empty(), "余計に尋ねた: {:?}", approver.asked);
    }

    #[test]
    fn a_target_outside_the_root_is_asked_about_with_the_reason() {
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let outside = tempfile::tempdir().expect("一時ディレクトリ");
        let sandbox = workspace(root.path());
        let mut approver = Scripted { answers: vec![Decision::Allow], asked: vec![] };
        let mut gate = Gate::new(ApprovalPolicy::OnRequest);

        let target = outside.path().join("b.txt");
        gate.check(&sandbox, &target, &mut approver).expect("承認したのに拒否された");

        assert_eq!(approver.asked.len(), 1);
        assert!(
            approver.asked[0].contains(&target.display().to_string()),
            "理由にパスが無い: {}",
            approver.asked[0]
        );
    }

    #[test]
    fn a_denied_approval_stops_the_operation() {
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let outside = tempfile::tempdir().expect("一時ディレクトリ");
        let sandbox = workspace(root.path());
        let mut approver = Scripted { answers: vec![Decision::Deny], asked: vec![] };
        let mut gate = Gate::new(ApprovalPolicy::OnRequest);

        gate.check(&sandbox, &outside.path().join("b.txt"), &mut approver)
            .expect_err("拒否したのに通った");
    }

    #[test]
    fn never_refuses_without_asking() {
        // 無人実行では尋ねる相手がいない。尋ねずに通すのではなく、尋ねずに
        // 断る。通してしまうと、無人であることが権限の拡大になる。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let outside = tempfile::tempdir().expect("一時ディレクトリ");
        let sandbox = workspace(root.path());
        let mut approver = Scripted { answers: vec![Decision::Allow], asked: vec![] };
        let mut gate = Gate::new(ApprovalPolicy::Never);

        gate.check(&sandbox, &outside.path().join("b.txt"), &mut approver)
            .expect_err("Never なのに通った");
        assert!(approver.asked.is_empty(), "Never なのに尋ねた");
    }

    #[test]
    fn always_asks_even_inside_the_root() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let sandbox = workspace(dir.path());
        let mut approver = Scripted { answers: vec![Decision::Allow], asked: vec![] };
        let mut gate = Gate::new(ApprovalPolicy::Always);

        gate.check(&sandbox, &sandbox.writable_roots()[0].join("a.txt"), &mut approver)
            .expect("承認したのに拒否された");
        assert_eq!(approver.asked.len(), 1, "Always なのに尋ねなかった");
    }
}
```

- [ ] **Step 2: テストを走らせて失敗を確認する**

Run: `cargo test -p polaris-core approval`
Expected: コンパイルエラー。

- [ ] **Step 3: 実装する**

```rust
//! 承認境界。`sandbox_mode` が技術的境界を、`approval_policy` が停止して
//! 確認する条件を定める。二つは直交する。
//!
//! 尋ねる相手を trait にするのは、テストが実際の端末入力を要らないように
//! するためである。

use std::path::Path;

use polaris_sandbox::SandboxPolicy;
use polaris_tools::predicate::{predict, Verdict};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalPolicy {
    /// 尋ねない。範囲外はそのまま断る。
    Never,
    /// 範囲外のときだけ尋ねる。
    OnRequest,
    /// 変更操作のたびに尋ねる。
    Always,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
}

pub trait Approver {
    fn ask(&mut self, reason: &str) -> Decision;
}

pub struct Gate {
    policy: ApprovalPolicy,
}

impl Gate {
    pub fn new(policy: ApprovalPolicy) -> Self {
        Self { policy }
    }

    /// 変更操作の前に呼ぶ。通ってよければ `Ok(())`、止めるなら理由を返す。
    pub fn check(
        &mut self,
        sandbox: &SandboxPolicy,
        target: &Path,
        approver: &mut dyn Approver,
    ) -> Result<(), String> {
        let verdict = predict(sandbox, target);

        let reason = match (&verdict, self.policy) {
            (Verdict::Allowed, ApprovalPolicy::Always) => {
                format!("{} へ書き込む。方針 {}", target.display(), sandbox.describe())
            }
            (Verdict::Allowed, _) => return Ok(()),
            (Verdict::NeedsApproval { reason }, _) => reason.clone(),
        };

        // 尋ねる相手がいない設定では、尋ねずに断る。通してしまうと、
        // 無人であることがそのまま権限の拡大になる。
        if self.policy == ApprovalPolicy::Never {
            return Err(reason);
        }

        match approver.ask(&reason) {
            Decision::Allow => Ok(()),
            Decision::Deny => Err(format!("利用者が承認しなかった: {reason}")),
        }
    }
}
```

- [ ] **Step 4: テストを走らせて通ることを確認する**

Run: `cargo test -p polaris-core approval`
Expected: 5 件すべて PASS。

- [ ] **Step 5: 変異で検証する**

1. `ApprovalPolicy::Never` の分岐を削除 → `never_refuses_without_asking` だけが落ちる
2. `(Verdict::Allowed, ApprovalPolicy::Always)` の腕を削除 → `always_asks_even_inside_the_root` だけが落ちる
3. `Decision::Deny` の腕を `Ok(())` に置換 → `a_denied_approval_stops_the_operation` だけが落ちる

- [ ] **Step 6: コミット**

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
git add crates/polaris-core
git commit -m "feat(core): gate mutations behind an approval boundary"
```

---

### Task 11: 監査ログが方針と書込先と結果を運ぶ

**Files:**
- Modify: `crates/polaris-core/src/audit.rs`
- Modify: `crates/polaris-core/Cargo.toml`（`polaris-sandbox` への依存）
- Modify: `crates/polaris-core/src/agent.rs`（呼び出し側の追随）

**Interfaces:**
- Consumes: `polaris_sandbox::SandboxPolicy`、`SandboxPolicy::describe`、`crate::secret_screen::screen_text`
- Produces: `polaris_core::audit::Record`、`AuditLog::record(&mut self, r: &Record<'_>) -> std::io::Result<()>`

仕様は監査へ「型、解決後のサンドボックス方針、書込先、結果」を含めることを定めている。M1 の実装は型と引数しか運べず、この不足を指摘として繰り越していた。M2 で書込操作が入るため、ここで閉じる。

**新しい欄も必ず伏字化を通す。** M1 では `tool` 欄が伏字化を通っていない時期があり、プロンプトインジェクションでモデルに任意のツール名を吐かせる経路から生の文字列がログへ落ちうる状態だった。欄を増やすたびに同じ穴が開く。

- [ ] **Step 1: 失敗するテストを書く**

```rust
    #[test]
    fn a_record_carries_the_policy_the_target_and_the_result() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let path = dir.path().join("audit.jsonl");
        let mut log = AuditLog::open(&path).expect("開けない");

        let policy = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::WorkspaceWrite,
            &[root.path().to_path_buf()],
        )
        .expect("方針");

        log.record(&Record {
            tool: "write",
            detail: "{\"path\":\"a.txt\"}",
            sandbox: Some(&policy),
            target: Some(std::path::Path::new("/w/a.txt")),
            result: "ok",
        })
        .expect("書けない");

        let line = std::fs::read_to_string(&path).expect("読めない");
        let v: serde_json::Value = serde_json::from_str(line.trim()).expect("JSON でない");
        assert_eq!(v["tool"], "write");
        assert_eq!(v["result"], "ok");
        assert_eq!(v["target"], "/w/a.txt");
        assert!(
            v["sandbox"].as_str().expect("sandbox が無い").contains("workspace-write"),
            "方針が記録されていない: {v}"
        );
    }

    #[test]
    fn every_new_field_passes_through_the_secret_screen() {
        // 欄を増やすたびに伏字化を通し忘れる穴が開く。M1 では tool 欄が
        // 通っていない時期があった。ここでは target と result の双方に
        // 秘密らしき文字列を入れ、そのまま落ちないことを見る。
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let path = dir.path().join("audit.jsonl");
        let mut log = AuditLog::open(&path).expect("開けない");

        let secret = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        log.record(&Record {
            tool: "bash",
            detail: "echo x",
            sandbox: None,
            target: Some(std::path::Path::new(secret)),
            result: secret,
        })
        .expect("書けない");

        let line = std::fs::read_to_string(&path).expect("読めない");
        assert!(
            !line.contains(secret),
            "秘密が生のまま監査ログに落ちている: {line}"
        );
    }

    #[test]
    fn an_absent_policy_and_target_are_omitted_rather_than_written_as_empty() {
        // 空文字列を書くと、「方針が無い」と「方針が空文字列だった」が
        // 区別できなくなる。
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let path = dir.path().join("audit.jsonl");
        let mut log = AuditLog::open(&path).expect("開けない");

        log.record(&Record {
            tool: "read",
            detail: "{}",
            sandbox: None,
            target: None,
            result: "ok",
        })
        .expect("書けない");

        let line = std::fs::read_to_string(&path).expect("読めない");
        let v: serde_json::Value = serde_json::from_str(line.trim()).expect("JSON でない");
        assert!(v.get("sandbox").is_none() || v["sandbox"].is_null(), "{v}");
        assert!(v.get("target").is_none() || v["target"].is_null(), "{v}");
    }
```

- [ ] **Step 2: テストを走らせて失敗を確認する**

Run: `cargo test -p polaris-core audit`
Expected: コンパイルエラー。`Record` が無い。

- [ ] **Step 3: 実装する**

`crates/polaris-core/Cargo.toml` の `[dependencies]` へ `polaris-sandbox = { path = "../polaris-sandbox" }` を足す。

`crates/polaris-core/src/audit.rs`:

```rust
/// 監査ログ 1 行の内容。仕様が求める「型、解決後のサンドボックス方針、
/// 書込先、結果」をこの型が運ぶ。
pub struct Record<'a> {
    pub tool: &'a str,
    pub detail: &'a str,
    pub sandbox: Option<&'a polaris_sandbox::SandboxPolicy>,
    pub target: Option<&'a std::path::Path>,
    pub result: &'a str,
}

impl AuditLog {
    /// 1 行を追記する。**ここへ渡るあらゆる文字列は書く直前に伏字化を通る。**
    /// 欄を増やすときは必ず `screen` を通すこと。通し忘れは、そのまま
    /// 生の資格情報がログへ落ちる経路になる。
    pub fn record(&mut self, r: &Record<'_>) -> std::io::Result<()> {
        let mut line = serde_json::json!({
            "tool": screen(r.tool),
            "detail": screen(r.detail),
            "result": screen(r.result),
        });
        if let Some(p) = r.sandbox {
            line["sandbox"] = serde_json::Value::String(screen(&p.describe()));
        }
        if let Some(t) = r.target {
            line["target"] = serde_json::Value::String(screen(&t.display().to_string()));
        }
        writeln!(self.file, "{line}")?;
        self.file.flush()
    }
}
```

- [ ] **Step 4: 呼び出し側を追随させる**

`crates/polaris-core/src/agent.rs` の `audit.record(&call.name, &call.arguments.to_string())?` を `Record` 版へ置き換える。この時点では `sandbox` と `target` は `None` でよい。実際に埋めるのは Task 12 である。

- [ ] **Step 5: テストを走らせて通ることを確認する**

Run: `cargo test --workspace`
Expected: 全件 PASS。

- [ ] **Step 6: 変異で検証する**

1. `screen(r.result)` を `r.result.to_string()` に置換 → `every_new_field_passes_through_the_secret_screen` だけが落ちる
2. `target` の `screen` を外す → 同じテストが落ちる
3. `if let Some(p) = r.sandbox` を無条件の空文字列書き込みに置換 → `an_absent_policy_and_target_are_omitted_rather_than_written_as_empty` だけが落ちる

- [ ] **Step 7: コミット**

```bash
cargo clippy --workspace --all-targets -- -D warnings
git add crates/polaris-core Cargo.lock
git commit -m "feat(core): record the sandbox policy, target and result in the audit log"
```

---

### Task 12: ループと CLI への接続、予算の再測定

**Files:**
- Modify: `crates/polaris-core/src/agent.rs`
- Modify: `crates/polaris-cli/src/main.rs`
- Modify: `crates/polaris-core/src/budget.rs`（上限に触れる場合のみ）

**Interfaces:**
- Consumes: これまでの全タスクの成果
- Produces: `agent::run` が承認境界とサンドボックス方針を受け取る形

- [ ] **Step 1: 失敗するテストを書く**

`crates/polaris-core/src/agent.rs` のテストモジュールへ追加する。既存の
`dispatches_the_skill_tool` と同じ `Scripted` プロバイダを使う。

```rust
    /// テスト用の承認者。常に許可し、尋ねられた回数を数える。
    struct AlwaysAllow {
        asked: usize,
    }
    impl crate::approval::Approver for AlwaysAllow {
        fn ask(&mut self, _reason: &str) -> crate::approval::Decision {
            self.asked += 1;
            crate::approval::Decision::Allow
        }
    }

    /// `write` を 1 回呼んでから本文を返すプロバイダと、その周辺一式。
    /// ヘルパは polaris 本体ではなく最小のシェルスクリプトを使う。ここで
    /// 見たいのはループの配線であって、ヘルパ自身の正しさではない。
    fn write_helper(dir: &std::path::Path) -> std::path::PathBuf {
        let p = dir.join("helper.sh");
        std::fs::write(
            &p,
            r#"#!/bin/sh
python3 -c '
import json,sys,os
m=json.load(sys.stdin)
os.makedirs(os.path.dirname(m["path"]), exist_ok=True)
open(m["path"],"w").write(m["content"])
print("wrote")
'
"#,
        )
        .expect("書けない");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        p
    }

    #[tokio::test]
    async fn the_write_tool_reads_the_argument_names_its_schema_declares() {
        // 公開スキーマが宣言する引数名と dispatch が読む名前を束ねる。
        // スキーマ側だけを改名しても全テストが通る状態を残さない
        // （M3a の B1 と同じ欠陥クラス）。引数名はスキーマから取り出し、
        // このテストの中に literal で書かない。
        let specs = polaris_tools::all_specs();
        let spec = specs
            .iter()
            .find(|s| s.name == "write")
            .expect("write が無い");
        let json = serde_json::to_value(spec).expect("直列化");
        let required: Vec<String> = json["parameters"]["required"]
            .as_array()
            .expect("required が無い")
            .iter()
            .map(|v| v.as_str().expect("required の要素が文字列でない").to_string())
            .collect();
        assert_eq!(required.len(), 2, "write の必須引数が 2 個でない: {required:?}");

        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let helper_dir = tempfile::tempdir().expect("一時ディレクトリ");
        let helper = write_helper(helper_dir.path());
        let sandbox = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::WorkspaceWrite,
            &[root.path().to_path_buf()],
        )
        .expect("方針");
        let target = sandbox.writable_roots()[0].join("out.txt");

        // スキーマが宣言する名前だけを使って引数を組み立てる。片方が
        // "path" 以外へ改名されていれば、それが content 用の値を受け取り、
        // 書き込み先が食い違って下の assert が落ちる。
        let mut args = serde_json::Map::new();
        for name in &required {
            let value = if name.contains("path") {
                target.display().to_string()
            } else {
                "本文".to_string()
            };
            args.insert(name.clone(), serde_json::Value::String(value));
        }

        let p = Scripted {
            replies: Mutex::new(vec![
                CompletionResponse {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "write".into(),
                        arguments: serde_json::Value::Object(args),
                    }],
                },
                CompletionResponse {
                    text: "書いた".into(),
                    tool_calls: vec![],
                },
            ]),
        };

        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let mut session = Session::new();
        session.push_user("a.txt を作って");
        let mut audit = AuditLog::open(&dir.path().join("audit.jsonl")).expect("開けない");
        let mut stop = StopTracker::new(10);
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::OnRequest);
        let mut approver = AlwaysAllow { asked: 0 };
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };

        let out = run(&p, &mut session, &mut audit, &mut stop, &always_on, &[], &mut ctx)
            .await
            .expect("ループが失敗した");

        assert_eq!(out, "書いた");
        assert_eq!(
            std::fs::read_to_string(&target).expect("書かれていない"),
            "本文",
            "スキーマが宣言する引数名を dispatch が読んでいない"
        );
    }

    #[tokio::test]
    async fn a_denied_write_comes_back_as_a_tool_result_not_a_loop_failure() {
        // サンドボックス拒否は例外ではない。モデルが理解できる形で返し、
        // ループは続く。ここで Err にすると、モデルは別の場所へ書き直す
        // 機会を失い、拒否がそのまま実行の失敗になる。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let outside = tempfile::tempdir().expect("一時ディレクトリ");
        let helper_dir = tempfile::tempdir().expect("一時ディレクトリ");
        let helper = write_helper(helper_dir.path());
        let sandbox = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::WorkspaceWrite,
            &[root.path().to_path_buf()],
        )
        .expect("方針");

        let target = outside
            .path()
            .canonicalize()
            .expect("canonicalize")
            .join("pwned.txt");

        let p = Scripted {
            replies: Mutex::new(vec![
                CompletionResponse {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "write".into(),
                        arguments: serde_json::json!({
                            "path": target.display().to_string(),
                            "content": "本文"
                        }),
                    }],
                },
                CompletionResponse {
                    text: "別の場所へ書き直す".into(),
                    tool_calls: vec![],
                },
            ]),
        };

        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let mut session = Session::new();
        session.push_user("外へ書いて");
        let mut audit = AuditLog::open(&dir.path().join("audit.jsonl")).expect("開けない");
        let mut stop = StopTracker::new(10);
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        // Never にして、承認で通り抜ける経路を塞ぐ。ここで見たいのは
        // 「拒否がツール結果として返る」ことである。
        let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::Never);
        let mut approver = AlwaysAllow { asked: 0 };
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };

        let out = run(&p, &mut session, &mut audit, &mut stop, &always_on, &[], &mut ctx)
            .await
            .expect("拒否でループごと失敗した");

        assert_eq!(out, "別の場所へ書き直す", "ループが 2 ターン目へ進んでいない");
        assert!(!target.exists(), "ファイルが作られている");

        let tool_msg = session
            .messages
            .iter()
            .find(|m| m.tool_call_id.is_some())
            .expect("ツール結果が積まれていない");
        assert!(
            tool_msg.content.contains("workspace-write"),
            "拒否の理由に方針が無い: {}",
            tool_msg.content
        );
        assert!(
            tool_msg.content.contains(&target.display().to_string()),
            "拒否の理由にパスが無い: {}",
            tool_msg.content
        );
    }
```

- [ ] **Step 2: `dispatch` を広げる**

`dispatch` は方針、ヘルパのパス、承認ゲートを受け取る必要がある。署名を変える。

```rust
fn dispatch(
    call: &polaris_provider::ToolCall,
    skills: &[polaris_skills::Skill],
    ctx: &mut ToolContext<'_>,
) -> Result<String, String>;

/// 変更操作に要る文脈。`agent::run` が受け取って `dispatch` へ渡す。
pub struct ToolContext<'a> {
    pub sandbox: &'a polaris_sandbox::SandboxPolicy,
    pub helper: &'a std::path::Path,
    pub gate: &'a mut crate::approval::Gate,
    pub approver: &'a mut dyn crate::approval::Approver,
}
```

`write` と `edit` の腕では、実行前に `ctx.gate.check(ctx.sandbox, path, ctx.approver)` を通す。断られたらその理由をそのままモデルへ返す。`bash` の腕では通さない。

- [ ] **Step 3: `run` の署名を変える**

```rust
pub async fn run(
    provider: &dyn Provider,
    session: &mut Session,
    audit: &mut AuditLog,
    stop: &mut StopTracker,
    always_on: &crate::prompt::AlwaysOn,
    skills: &[polaris_skills::Skill],
    ctx: &mut ToolContext<'_>,
) -> Result<String, AgentError>;
```

既存の `agent.rs` のテストは `run(...)` を 6 引数で呼んでいる。7 引数目を足す必要がある。**これは署名変更に伴う不可避の追随であり、既存テストの意図を変えないこと。**

監査の記録では、`write` と `edit` のときに `sandbox` と `target` を埋める。Task 11 で `None` にしていた箇所である。

- [ ] **Step 4: CLI を接続する**

`main.rs` で次を組み立てる。

- `let root = polaris_core::project::resolve_root(&cwd);`
- `let sandbox = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.clone()])?;`
- `let helper = polaris_sandbox::stage::staged_helper(&sandbox, &state_dir)?;`
- `let mut gate = Gate::new(ApprovalPolicy::OnRequest);`
- 端末から尋ねる `Approver` の実装（標準入力から `y` / `n` を読む）

`state_dir` は監査ログと同じ `~/.polaris/state/<project-id>/` を使う。`default_audit_path` がすでにそのディレクトリを作っているので、パスの組み立てを関数へ切り出して共有すること。

`--sandbox` と `--approval` の CLI 引数を足す。既定はそれぞれ `workspace-write` と `on-request` とする。

- [ ] **Step 5: 予算を測り直す**

ツールが 2 本から 5 本へ増える。`cargo test -p polaris-core budget -- --nocapture` で下限と真の同時最大を測り直し、**両方が 990 未満であることを確認する。** 超えた場合は勝手に上限を上げないこと。上限 990 は仕様の受け入れ基準であり、超えたなら報告する事象である。

測った値と、それを産んだテスト名を報告へ書くこと。

- [ ] **Step 6: 全体を通す**

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

`docs/filemap.md` が落ちたら `UPDATE_FILEMAP=1 cargo test -p polaris-core --test filemap` で再生成する。

- [ ] **Step 7: コミット**

```bash
git add -A crates docs/filemap.md Cargo.lock
git commit -m "feat: wire the sandbox, approval gate and mutation tools into the loop"
```

---

## 繰り越した M1 の指摘のうち、本計画で閉じるもの

| 指摘 | 閉じる場所 |
| --- | --- |
| ハードリンク経由でパス方針を迂回できる | Task 2（述語で `st_nlink > 1` を承認へ回す）。仕様の「保証しない範囲」にも明記済み。**閉じるのは緩和であって保証ではない** |
| 監査ログの記録がサンドボックス方針と書込先を運べない | Task 11 |
| `secret_screen::is_excluded_path` が未使用のまま公開されている | Task 2 で `path_policy::is_denied` を述語から使う。`is_excluded_path` 自体が依然として未使用なら、Task 12 で削除するか使う先を決めること |
| プロジェクトルートがプロセスの cwd である | Task 6 |

本計画で閉じないもの。M3b 以降へ持ち越す。

- 5 MiB の読み取り上限がファイルを縛るが返すバイト数を縛らない
- サイズ上限のテストが 12 バイトしか書いていない
- クライアント生成がタイムアウト定数を適用することを固定するテストが無い
- 憲法の見出しフォールバックが `## ` でしか止まらない

## Self-Review

**1. 仕様の網羅**

| 仕様の条項 | 対応するタスク |
| --- | --- |
| ツール 6 本のうち `write` / `edit` / `bash` | Task 8、Task 9。完了時点で 5 本、`spawn` は M4 |
| `sandbox_mode` の 3 値 | Task 1 |
| macOS は Seatbelt、Linux は landlock | Task 3、Task 4 |
| `polaris-sandbox` は方針と書込可能ルートのみ保持 | Task 1 |
| 変更はすべて境界を越える | Task 5、Task 7、Task 8 |
| 再実行するバイナリを書込可能ルートの外へ | Task 7 |
| ルートと対象パスの正規化 | Task 1、Task 2 |
| 適用の失敗は硬い失敗 | Task 5 |
| `write` / `edit` は予測して停止、`bash` は試行して返す | Task 10、Task 9 |
| 拒否メッセージにパス・方針・ルート | Task 2、Task 8、Task 9 |
| 監査に型・方針・書込先・結果 | Task 11 |
| 監査へ書く文字列の伏字化 | Task 11 |
| 受け入れ基準 3 | Task 8（ツール経由）、Task 5（土台） |
| 常時コンテキスト 990 以下 | Task 12 |

subagent の実効権限と `spawn` の書込先衝突は M4 の範囲であり、本計画には含まない。

**2. 未記入の箇所**

無い。Task 12 の Step 1 は当初骨子のまま残していたが、書き下ろした。骨子を残すと、実装者が意図を推測して埋めることになり、このセッションで 18 回起きた「確認せずに書いた指示が誤っていた」と同じ経路をもう一度開くことになる。

**3. 型の一貫性**

`SandboxPolicy`、`SandboxMode`、`Verdict`、`Outcome`、`Mutation`、`Record`、`ToolContext` は「型の一覧」節で 1 回だけ定義し、各タスクの Interfaces 欄はそこから引いている。`predict` は `polaris-tools` に置く（依存の向きが `polaris-tools` → `polaris-sandbox` の一方向であるため）。`run_mutation` は `write.rs` に置き `edit.rs` から `pub(crate)` で呼ぶ。
