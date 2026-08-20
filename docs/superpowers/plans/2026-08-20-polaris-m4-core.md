# polaris M4 コア（`spawn` と単一波オーケストレーション）実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `spawn` ツールを追加し、subagent 型（`agents/` 配下、Agent Skills 形式）を1波だけ並列実行できるようにする。各 subagent は自分専用の有界ループ（ターン数・壁時計・スキーマ検証済み結果）を持ち、親の文脈には検証済みの結果だけが戻る。継続波（`spawn` が返した後続タスクをランタイムが同じ呼び出しの中で展開する仕組み）はこの計画に含めない——機構自体が無いところに機構だけを足しても独立に検証できないため、`spawn` が実際に動いてから別計画で追加する。

**Architecture:** `polaris-tools` は `spawn` ツールの JSON Schema と、検索・検証まわりの薄いユーティリティ（`Named` トレイト、JSON Schema バリデータ）だけを持つ。`polaris-core` は依存の向きの都合上（`polaris-core` は既に `polaris-tools` に依存しており、逆方向の依存は循環になる）、subagent の実行そのもの——`Provider`・`Session`・`AuditLog`・`StopTracker` を使う本体——を持つ。既存の1ターン1呼び出し・同期の `dispatch` はまず `async fn` へ変える。`spawn` の並列実行は `dispatch` の外側ループを並列化するのではなく、`spawn` 自身の実装内で `tokio::spawn` により行う。各 subagent タスクは `'static` を要求されるため、`ToolContext` を借用のまま共有せず、subagent ごとに所有権を持った `SandboxPolicy`（`Clone` 済み）・`Gate`・新設の `AutoApprove`（非対話環境向け、承認を求めず許可する）を都度組み立てる。`AuditLog` は `Arc<Mutex<AuditLog>>` に変え、`Provider` は `Arc<dyn Provider>` に変えて、subagent タスク間で共有する。

**Tech Stack:** Rust 1.96 / edition 2024。新規外部依存: `jsonschema`（結果スキーマ検証。正確なバージョンは実装時に `cargo add` で解決すること。ワークスペースの慣例に倣い `[workspace.dependencies]` へ足す）。`tokio` は既存のワークスペース依存だが `sync` feature が無いため追加し、`polaris-core` の `[dependencies]` へ昇格する（現在は `[dev-dependencies]` のみ）。

**Spec:** `docs/superpowers/specs/2026-08-16-polaris-harness-design.md` の「subagent 契約」「オーケストレーション」（波・深さ・並列度・階層とモデル・結果の各節。継続波は対象外）「サンドボックスと承認」「エラー処理」「監査」「テスト戦略」「受け入れ基準」

## Global Constraints

以下は仕様および本計画のスコープ決定から逐語で写した値・方針である。全タスクの要件に暗黙に含まれる。

- `BUDGET_LIMIT = 990`、`MAX_TOOLS = 6`（`crates/polaris-core/src/budget.rs`、既存・変更しない）。`spawn` はちょうど6本目であり、このスコープ内でこれ以上ツールを増やさない
- 深さは1固定。`spawn` は subagent の利用可能ツールへ**ランタイム側で無条件に**含めない（型定義の `allowed-tools` に書かれていても除外する）
- 並列度は既定で同時8、うち書込4。`tokio::sync::Semaphore` で実装し、初期値は設定可能にする（設定キーは Task 10 で定義）
- 継続波はこの計画に含めない。`spawn` の1呼び出しは1波のみを実行し、後続タスクの展開機構は実装しない
- 階層別プロバイダはこの計画のスコープでは全階層を既存プロバイダ（`openai`・`codex`）で賄う。`ollama` 等ローカルプロバイダの新規実装は行わない
- プロバイダのフォールバック連鎖はこの計画に含めない。subagent のプロバイダ障害はルートと同じ「1回叩いて失敗ならそのまま失敗」とし、そのタスクだけを失敗として記録し波は止めない
- 結果は型が宣言した JSON Schema でランタイムが検証する。不一致なら検証エラーを添えて1回だけ再試行し、なお不一致ならそのタスクのみ失敗として記録する。波全体は停止しない
- subagent の中間過程（内部のツール呼び出し・思考）は親の文脈へ一切入れない。親が受け取るのは検証済みの結果文字列のみ
- 監査ログは全ツール呼び出し（subagent 内部のものを含む）を対象とする。既存の `AuditLog::record` の秘密情報伏字化（`screen`）を経由しないログ書き込み経路を新設しない
- 検証は `cargo test --workspace`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo fmt --all -- --check` の3つ。`--all-targets` を落とすと、新規関数がまだ呼ばれていない段階で `dead_code` 警告が出る

## ファイル構成

| ファイル | 責務 |
| --- | --- |
| `crates/polaris-tools/src/skill.rs` | `Named` トレイトを新設し `Skill` に実装。既存の `lookup`/`list_candidates`/BM25/近傍呼び出しをジェネリックへ変更。振る舞いは変えない |
| `crates/polaris-tools/src/skill/bm25.rs`、`near_universal.rs` | `Named` に対してジェネリックへ変更するのみ。アルゴリズム自体は変えない |
| `crates/polaris-skills/src/frontmatter.rs` | `field`/`name_is_valid` を `pub(crate)` へ昇格。挙動は変えない |
| `crates/polaris-skills/src/agent_type.rs`（新規） | `AgentType` 構造体、その frontmatter 解析、`agents/` ディレクトリの discovery |
| `crates/polaris-skills/src/lib.rs` | `AgentType`・`AgentTypeError`・`discover_agent_types` の re-export |
| `crates/polaris-tools/src/lib.rs` | `spawn_spec()` の追加、`all_specs()` への算入、`impl Named for polaris_skills::AgentType` |
| `crates/polaris-tools/src/schema_validate.rs`（新規） | `jsonschema` クレートの薄いラッパー。値をスキーマへ照合するだけ |
| `crates/polaris-core/src/config.rs` | `Config.agents_paths`、`[agents] paths` の TOML キー |
| `crates/polaris-core/src/audit.rs` | `AuditLog::record` を `Arc<Mutex<AuditLog>>` 越しに呼べる形へ、`Record` へ `caller` フィールドを追加 |
| `crates/polaris-core/src/stop.rs` | `StopReason::WallSeconds`・`StopReason::SchemaMismatch` の追加、壁時計計測とスキーマ不一致カウントの追加 |
| `crates/polaris-core/src/agent.rs` | `dispatch` を `async fn` 化。`run`/`run_loop` の分離。`"spawn"` ディスパッチ腕の追加。`AutoApprove` の新設 |
| `crates/polaris-core/src/spawn.rs`（新規） | `SpawnTask` のパース、波の並列実行、書込先衝突検査、結果検証と1回だけの再試行 |
| `crates/polaris-cli/src/main.rs` | `agents/` discovery の配線、`Provider`/`AuditLog` を `Arc` へ、`run()` 呼び出しへの新規引数追加 |
| `crates/polaris-core/src/budget.rs` | `spawn` を含めた6ツールの実測が予算内に収まることを確認する新規テスト |
| `Cargo.toml`（workspace） | `tokio` に `sync` feature 追加、`jsonschema` を `[workspace.dependencies]` へ |
| `crates/polaris-core/Cargo.toml` | `tokio` を `[dependencies]` へ昇格 |
| `crates/polaris-tools/Cargo.toml` | `jsonschema` を `[dependencies]` へ |
| `agents/file-inspector/SKILL.md`、`agents/file-inspector/references/result.schema.json`（新規） | 実在する唯一の初期 subagent 型。単一ファイルを読み取り専用で棚卸しする |

タスクは依存順に並べてある。Task 1〜3 は discovery/検索の下地、Task 4〜7 は並行実行の土台、Task 8〜10 が `spawn` 本体、Task 11〜12 が予算計測と安全性検証である。

---

### Task 1: `skill` 検索を `Named` トレイトへ汎化する

**Files:**
- Modify: `crates/polaris-tools/src/skill.rs`
- Modify: `crates/polaris-tools/src/skill/bm25.rs`
- Modify: `crates/polaris-tools/src/skill/near_universal.rs`

**Interfaces:**
- Consumes: 既存の `polaris_skills::Skill`（`name`/`description`/`body`/`path`）
- Produces:
  - `pub trait Named { fn name(&self) -> &str; fn description(&self) -> &str; }`（`skill.rs` で定義）
  - `impl Named for polaris_skills::Skill`
  - `pub fn lookup<T: Named>(items: &[T], q: &str) -> String`（既存のシグネチャ `pub fn lookup(skills: &[Skill], q: &str) -> String` から変更。`Skill: Named` なので既存呼び出し `polaris_tools::skill::lookup(skills, q)` は無修正でコンパイルが通る）
  - `pub(crate) fn near_universal<T: Named>(items: &[T]) -> Vec<&T>`（既存 `pub fn near_universal(skills: &[Skill]) -> Vec<&Skill>` から変更）
  - `pub(crate) struct Bm25<'a, T: Named>`、`Bm25::new(items: &'a [T]) -> Self`、`Bm25::rank(&self, q: &str, k: usize) -> Vec<&'a T>`

Task 2 で新設する `AgentType` に対する `impl Named for AgentType` は、`polaris-tools` が `polaris-skills` に依存しているため `polaris-tools` 側に書く（Task 3 で行う）。ここでは `Skill` の実装だけを追加し、既存の挙動を一切変えない。

- [ ] **Step 1: 失敗するテストを書く（`Named` を実装した別の型で `lookup` が動くことを確認する）**

`crates/polaris-tools/src/skill.rs` の既存テストモジュール末尾に追加する。

```rust
#[cfg(test)]
mod named_generic_tests {
    use super::Named;

    /// `Skill` 以外の型でも `lookup` 等が動くことを保証するためだけの、
    /// テスト専用の最小実装。
    struct Fixture {
        name: String,
        description: String,
    }

    impl Named for Fixture {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            &self.description
        }
    }

    #[test]
    fn lookup_works_for_any_named_type_not_just_skill() {
        let items = vec![Fixture {
            name: "widget".to_string(),
            description: "handles widget-shaped requests".to_string(),
        }];
        let out = super::lookup(&items, "widget");
        assert!(out.contains("handles widget-shaped requests"), "{out}");
    }
}
```

Run: `cargo test -p polaris-tools named_generic_tests`

Expected: FAIL — `Named` は未定義、`lookup` は `&[Skill]` しか受け付けない（コンパイルエラー）。

- [ ] **Step 2: `Named` トレイトを定義し、`Skill` に実装する**

`crates/polaris-tools/src/skill.rs` の冒頭、`mod bm25;` の前に追加する。

```rust
/// BM25・近傍選定・`lookup` の整形ロジックが必要とする最小の形。
/// `Skill` と、M4 で追加する subagent の型定義の両方がこれを実装する
/// ことで、検索・整形のコードを2箇所に複製せずに済む。
pub trait Named {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
}

impl Named for polaris_skills::Skill {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
}
```

- [ ] **Step 3: `lookup`・`list_candidates`・`description_preview` の呼び出し箇所をジェネリックへ変更する**

`crates/polaris-tools/src/skill.rs` 内、`pub fn lookup(skills: &[Skill], q: &str) -> String` の宣言を次に置き換える。関数の中身（本文返却・空クエリ一覧・バイト上限ロジック）はそのまま。型パラメータの追加と、内部で呼んでいる `list_candidates`/`Bm25::new`/`near_universal` への型引数受け渡しだけを直す。

```rust
pub fn lookup<T: Named>(items: &[T], q: &str) -> String {
```

同様に `fn list_candidates(items: &[&Skill], header: &str, max_count: usize) -> String` を

```rust
fn list_candidates<T: Named>(items: &[&T], header: &str, max_count: usize) -> String {
```

へ変更する。本文中の `s.name`/`s.description` への直接アクセスは `s.name()`/`s.description()`（トレイトメソッド呼び出し）に変える。`description_preview(description: &str) -> String` は `&str` を直接受けているため変更不要。

- [ ] **Step 4: `bm25.rs` をジェネリックへ変更する**

`crates/polaris-tools/src/skill/bm25.rs` 冒頭の `use polaris_skills::Skill;` を `use super::Named;` に変える。

```rust
pub(crate) struct Bm25<'a, T: Named> {
    items: &'a [T],
    // 既存のインデックス用フィールドはそのまま
}

impl<'a, T: Named> Bm25<'a, T> {
    pub(crate) fn new(items: &'a [T]) -> Self {
        // 既存の本文はそのまま。`skills[i].name`/`skills[i].description` を
        // 使っていた箇所を `items[i].name()`/`items[i].description()` へ
        // 置き換える以外の変更はしない。
        ...
    }

    pub(crate) fn rank(&self, q: &str, k: usize) -> Vec<&'a T> {
        // 既存の本文のまま。戻り値の要素型が `&Skill` から `&T` になる
        // だけで、ランキングのロジック自体は変えない。
        ...
    }
}
```

`tokenize`・`expand_query`・定数（`K1`/`B`/`NAME_WEIGHT`/ストップワード/語幹化テーブル/同義語表）は `Skill` を参照していないため無修正。

- [ ] **Step 5: `near_universal.rs` をジェネリックへ変更する**

`crates/polaris-tools/src/skill/near_universal.rs` の `pub fn near_universal(skills: &[Skill]) -> Vec<&Skill>` を

```rust
pub fn near_universal<T: Named>(items: &[T]) -> Vec<&T> {
```

へ変更する。本文中の trigger 形式判定・量化語判定・固有名詞判定はすべて `description()`/`name()` 経由の文字列に対して動くため、判定ロジック自体（正規表現・比較）は変更不要。`skill.rs:9` の `pub use near_universal::{MAX_NEAR_UNIVERSAL, near_universal};` はそのまま。

- [ ] **Step 6: テストを通す**

Run: `cargo test -p polaris-tools`

Expected: 既存のテストすべてが無修正で通り、`named_generic_tests::lookup_works_for_any_named_type_not_just_skill` も通る。既存テストは `&[Skill]` を渡しているため、型推論だけでコンパイルが通るはずである——もし通らない場合、既存テストのどこかが `Skill` の具象型に依存した書き方（例えば `Vec<Skill>` を返す内部ヘルパの戻り値型注釈）をしている箇所であり、その箇所だけ型注釈を足す。

- [ ] **Step 7: 全体検証とコミット**

Run: `cargo clippy -p polaris-tools --all-targets -- -D warnings && cargo fmt -p polaris-tools -- --check`

```bash
git add crates/polaris-tools/src/skill.rs crates/polaris-tools/src/skill/bm25.rs crates/polaris-tools/src/skill/near_universal.rs
git commit -m "refactor(polaris-tools): generalize skill lookup over a Named trait

M4 で subagent 型の discovery が同じ BM25/near_universal/lookup を
再利用できるようにする準備。Skill 以外の型を通しても振る舞いが変わ
らないことを named_generic_tests で確認済み。

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 2: `AgentType` — subagent 型の frontmatter 解析と discovery

**Files:**
- Modify: `crates/polaris-skills/src/frontmatter.rs`（`field`/`name_is_valid` を `pub(crate)` へ）
- Create: `crates/polaris-skills/src/agent_type.rs`
- Modify: `crates/polaris-skills/src/lib.rs`（`mod agent_type;` と re-export）

**Interfaces:**
- Consumes: `frontmatter::field`（内部関数）、`frontmatter::name_is_valid`（内部関数）、両方とも既存の実装のまま可視性だけ変える
- Produces:
  - `pub struct AgentType { pub name: String, pub description: String, pub body: String, pub path: PathBuf, pub allowed_tools: Vec<String>, pub access: AgentAccess, pub tier: String, pub wall_seconds: u32, pub max_turns: u32, pub continuation: bool, pub output_schema: PathBuf }`
  - `pub enum AgentAccess { Read, ReadWrite }`
  - `pub enum AgentTypeError { ... }`（`SkillError` と同じ形の `thiserror::Error`）
  - `pub fn discover_agent_types(dirs: &[PathBuf]) -> DiscoveredAgentTypes`
  - `pub struct DiscoveredAgentTypes { pub agent_types: Vec<AgentType>, pub skipped: Vec<SkippedAgentType> }`

`AgentType` を既存の `Skill` に統合せず別の構造体にする理由: `metadata` の各キー（`polaris-access`・`polaris-tier`・`polaris-wall-seconds`・`polaris-max-turns`・`polaris-continuation`・`polaris-output`）は subagent 型にしか意味を持たない。これらの検証規則（`polaris-access` は `read`/`read-write` のどちらか、`polaris-wall-seconds`/`polaris-max-turns` は正の整数の文字列、等）を `Skill` の読み込み経路に混ぜると、無関係な素の skill の読み込みにまで新しい失敗モードを持ち込む。`name`/`description` の検証規則とディレクトリ探索の「同じ形式」は `frontmatter::field`/`name_is_valid` を再利用することで保つ。

- [ ] **Step 1: `frontmatter::field`/`name_is_valid` を `pub(crate)` へ昇格する**

`crates/polaris-skills/src/frontmatter.rs` 内、既存の `fn name_is_valid(name: &str) -> bool` を `pub(crate) fn name_is_valid(name: &str) -> bool` に、既存の `fn field(...)`（3引数、`front_lines`/フィールド名/`dir_name` を取る既存のヘルパ）を `pub(crate) fn field(...)` に変える。シグネチャ・本文は変更しない。

Run: `cargo build -p polaris-skills`

Expected: 可視性変更のみなので無警告で成功する。

- [ ] **Step 2: 失敗するテストを書く（有効な `AgentType` の frontmatter を解析する）**

`crates/polaris-skills/src/agent_type.rs` を新規作成し、まずテストから書く。

```rust
//! subagent 型の定義（`agents/<type>/SKILL.md`）の解析と discovery。
//! `frontmatter.rs` の `name`/`description` 検証規則を再利用しつつ、
//! subagent 固有の `allowed-tools`/`metadata` を独自に解析する。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_text() -> &'static str {
        "---\n\
name: file-inspector\n\
description: 単一ファイルを読み取り専用で棚卸しし、責務、入出力、対応するテストを返す。\n\
allowed-tools: read\n\
metadata:\n\
  polaris-access: read\n\
  polaris-tier: low\n\
  polaris-wall-seconds: \"360\"\n\
  polaris-max-turns: \"12\"\n\
  polaris-continuation: \"denied\"\n\
  polaris-output: references/result.schema.json\n\
---\n\
本文がそのまま subagent のシステムプロンプトとなる。\n"
    }

    #[test]
    fn a_valid_agent_type_parses_every_field() {
        let a = parse(valid_text(), "file-inspector", Path::new("agents/file-inspector")).unwrap();
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
        assert_eq!(a.body.trim(), "本文がそのまま subagent のシステムプロンプトとなる。");
    }
}
```

Run: `cargo test -p polaris-skills agent_type::tests`

Expected: FAIL — `parse` は未定義。

- [ ] **Step 3: `parse` を実装する**

同じファイルへ、`AgentTypeError` の下に追加する。

```rust
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
            if line.trim_end() == "metadata:" {
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
        let (key, raw_value) = trimmed.split_once(':').ok_or_else(|| {
            AgentTypeError::MalformedMetadataLine {
                agent: agent.to_string(),
                line: trimmed.to_string(),
            }
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
    let output_schema =
        dir_path.join(required_metadata(&metadata, dir_name, "polaris-output")?);

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
        continuation,
        output_schema,
    })
}
```

- [ ] **Step 3a: `frontmatter.rs` に `parse_fields`/`front_lines` を追加する（内部分解、外部への公開APIは変えない）**

既存の `pub fn parse(text: &str, dir_name: &str) -> Result<(String, String, String), SkillError>` は、内部で `---` の位置探索とフロントマター本体の切り出し、続いて `name`/`description` の抽出という2段構えになっている（既存実装より）。この2段目だけを別関数として切り出し、`agent_type.rs` から両方を再利用できるようにする。`crates/polaris-skills/src/frontmatter.rs` に次を追加する。

```rust
/// `parse` の前半——`---` 区切りを見つけてフロントマター本体を行配列に
/// 分解し、本文と共に返す。`name`/`description` の抽出はまだ行わない。
/// `agent_type.rs` が `allowed-tools`/`metadata` を読むために、この行
/// 配列がもう一度必要になる。
pub(crate) fn front_lines_and_body<'a>(
    text: &'a str,
    dir_name: &str,
) -> Result<(Vec<&'a str>, String), SkillError> {
    let rest = text
        .strip_prefix("---")
        .ok_or_else(|| SkillError::NoFrontmatter {
            skill: dir_name.to_string(),
        })?;
    let rest = rest.trim_start_matches(['\r', '\n']);

    let (front, after_close) = if let Some(after) = rest.strip_prefix("---") {
        ("", after)
    } else {
        let end = rest
            .find("\n---")
            .ok_or_else(|| SkillError::NoFrontmatter {
                skill: dir_name.to_string(),
            })?;
        (&rest[..end], &rest[end + 4..])
    };

    let after_close = after_close.trim_start_matches('-');
    let body = after_close.trim_start_matches(['\r', '\n']).to_string();
    let front_lines: Vec<&str> = front.lines().collect();
    Ok((front_lines, body))
}

/// `agent_type.rs` が、`name`/`description` は既存の検証込みで、
/// `allowed-tools`/`metadata` は生の行配列で、両方必要とするための橋渡し。
pub(crate) fn parse_fields(
    text: &str,
    dir_name: &str,
) -> Result<(String, String, String), SkillError> {
    parse(text, dir_name)
}

/// `agent_type.rs` が `allowed-tools`/`metadata` を読むための行配列だけを
/// 返す。`parse`/`parse_fields` が行う `name`/`description` の検証は
/// 行わない——それは呼び出し側が `parse_fields` で別途行う。
pub(crate) fn front_lines<'a>(text: &'a str, dir_name: &str) -> Result<Vec<&'a str>, SkillError> {
    front_lines_and_body(text, dir_name).map(|(lines, _)| lines)
}
```

既存の `pub fn parse(...)` 本体は変更しない（そのまま `name`/`description` の抽出と検証を行う）——ここでは `front_lines_and_body` を新設するだけで、`parse` 自身をこの新関数を呼ぶ形へ書き換えるのは任意（挙動が変わらない限り可）。書き換える場合は既存の `SkillError` を返す経路が一致することを Step 4 のテストで確認する。

Run: `cargo test -p polaris-skills`

Expected: 既存のテストすべてが無修正で通る（`parse` の公開シグネチャ・挙動を変えていないため）。

- [ ] **Step 4: `AgentType::parse` のテストを通す**

Run: `cargo test -p polaris-skills agent_type::tests`

Expected: PASS。

- [ ] **Step 5: 失敗するテストを書く（必須メタデータ欠落・不正値の拒否）**

`agent_type.rs` の `tests` モジュールに追加する。

```rust
    #[test]
    fn a_missing_metadata_key_is_rejected() {
        let text = "---\n\
name: bad\n\
description: 説明。\n\
metadata:\n\
  polaris-tier: low\n\
---\n\
本文。\n";
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
        let text = "---\n\
name: bad\n\
description: 説明。\n\
metadata:\n\
  polaris-access: sudo\n\
  polaris-tier: low\n\
  polaris-wall-seconds: \"1\"\n\
  polaris-max-turns: \"1\"\n\
  polaris-continuation: \"denied\"\n\
  polaris-output: r.json\n\
---\n\
本文。\n";
        let err = parse(text, "bad", Path::new("agents/bad")).unwrap_err();
        assert!(matches!(
            err,
            AgentTypeError::InvalidMetadataValue {
                key: "polaris-access",
                ..
            }
        ));
    }
```

Run: `cargo test -p polaris-skills agent_type::tests`

Expected: PASS（既に Step 3 の実装がこれらのエラー経路を実装済みのため）。ここで初めて実行して失敗する場合は、Step 3 の該当バリデーションを見直す。

- [ ] **Step 6: discovery を実装する**

同じファイルへ追加する。

```rust
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
```

- [ ] **Step 7: discovery のテストを書いて通す**

```rust
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
```

`agent_type.rs` の `[dev-dependencies]` に `tempfile` が要る——`crates/polaris-skills/Cargo.toml` には既に `tempfile = { workspace = true }` が `[dev-dependencies]` にあるため、追加は不要。

Run: `cargo test -p polaris-skills`

Expected: PASS。

- [ ] **Step 8: `lib.rs` へ re-export し、全体検証してコミット**

`crates/polaris-skills/src/lib.rs` へ追加する。

```rust
mod agent_type;
pub use agent_type::{
    AgentAccess, AgentType, AgentTypeError, DiscoveredAgentTypes, SkipCauseAgentType,
    SkippedAgentType, discover_agent_types, discover_agent_types_in,
};
```

Run: `cargo test -p polaris-skills && cargo clippy -p polaris-skills --all-targets -- -D warnings && cargo fmt -p polaris-skills -- --check`

```bash
git add crates/polaris-skills/
git commit -m "feat(polaris-skills): parse and discover agents/ subagent types

allowed-tools/metadata は既存の Skill 読み込みでは捨てられていたため
新設した。name/description の検証規則とディレクトリ探索の形は
frontmatter.rs の既存規則をそのまま再利用する。

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 3: `Named for AgentType`、config、CLI での discovery 配線

**Files:**
- Modify: `crates/polaris-tools/src/lib.rs`（`impl Named for AgentType`、`spawn_spec()` は Task 9 で追加するのでここでは触れない）
- Modify: `crates/polaris-core/src/config.rs`
- Modify: `crates/polaris-cli/src/main.rs`

**Interfaces:**
- Consumes: Task 1 の `polaris_tools::skill::Named`、Task 2 の `polaris_skills::AgentType`/`discover_agent_types`
- Produces: `Config.agents_paths: Vec<PathBuf>`、`main.rs` 内のローカル変数 `discovered_agents: polaris_skills::DiscoveredAgentTypes`（後続タスクが `run()` へ渡す形を決める）

- [ ] **Step 1: `impl Named for AgentType` を追加する**

`crates/polaris-tools/src/skill.rs`（Task 1 で `Named` を定義した箇所の直後）へ追加する。

```rust
impl Named for polaris_skills::AgentType {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
}
```

Run: `cargo build -p polaris-tools`

Expected: 成功。これで `polaris_tools::skill::lookup(&discovered_agents.agent_types, q)` が既存の `skill` 検索と同じ実装で使えるようになる（Task 9 で subagent 型の候補一覧を返すエラーメッセージに使う）。

- [ ] **Step 2: `Config` へ `agents_paths` を追加する**

`crates/polaris-core/src/config.rs` の既存の `pub struct Config { pub skills_paths: Vec<PathBuf> }` を次に変える。

```rust
pub struct Config {
    /// Additional places to look for skills. Not included in the 2 default locations.
    pub skills_paths: Vec<PathBuf>,
    /// Additional places to look for subagent types. Not included in the 2 default locations
    /// (`<project_root>/agents`, `<HOME>/.polaris/agents`).
    pub agents_paths: Vec<PathBuf>,
}
```

既存の TOML 読み込み（`[skills] paths = [...]` を `RawSkills.paths` へ写す既存コード）と同じ形で `[agents] paths = [...]` を読む。既存の `RawSkills`/`RawConfig` 相当の構造体に `agents: Option<RawAgents>`（`RawAgents { paths: Vec<PathBuf> }`）を追加し、`load`/`try_load_from` の中で `skills_paths` と同じ扱い（プロジェクト設定がキーを持てば丸ごと上書き、無ければグローバル設定）にする。既存の `skills_paths` の配線パターンをそのまま複製する。

Run: `cargo test -p polaris-core config::`

Expected: 既存のテストが通る。新しいテストを1本足す。

```rust
#[test]
fn agents_paths_defaults_to_empty_when_the_key_is_absent() {
    let cfg = Config::default();
    assert!(cfg.agents_paths.is_empty());
}
```

- [ ] **Step 3: `main.rs` で discovery を配線する**

`crates/polaris-cli/src/main.rs` の既存の

```rust
let discovered = polaris_skills::discover(&cwd, &config.skills_paths);
for line in format_skipped_skills(&discovered.skipped) {
    eprintln!("{line}");
}
```

の直後へ追加する。

```rust
let discovered_agents = polaris_skills::discover_agent_types(&cwd, &config.agents_paths);
for s in &discovered_agents.skipped {
    eprintln!("agent type skipped: {s}");
}
```

`discovered_agents` はこの時点ではまだどこにも渡さない（`run()`/`dispatch()` へ配線するのは Task 9）。ここでは discovery が動くことと、壊れた型定義がプロセス全体を落とさず警告に留まることだけを保証する。

- [ ] **Step 4: 統合テストを書く**

`crates/polaris-cli/tests/` 配下の既存の統合テストと同じ形（`assert_cmd` 等、既存のテストが使っているクレートに倣う）で、`agents/broken-type/SKILL.md` に不正な frontmatter を置いた一時ディレクトリで CLI を実行し、標準エラーに `"agent type skipped: broken-type"` を含み、かつプロセスが通常どおり完了する（`ExitCode::SUCCESS` またはタスク自体の成否のみで決まる）ことを確認する。既存の `format_skipped_skills` を使った同種のテストがあれば、その隣に追加しファイル名・アサーション文字列だけ揃える。

- [ ] **Step 5: 全体検証とコミット**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

```bash
git add crates/polaris-tools/src/skill.rs crates/polaris-core/src/config.rs crates/polaris-cli/
git commit -m "feat: discover agents/ subagent types at startup

skill と同じ形の discovery を agents/ にも配線した。壊れた型定義は
プロセス全体を落とさず警告に留める。この時点ではまだ dispatch/run
へは渡さない——Task 9 で spawn ツールが使うようになる。

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 4: `AuditLog` を並行 subagent から書けるようにする

**Files:**
- Modify: `crates/polaris-core/src/audit.rs`
- Modify: `crates/polaris-core/src/agent.rs`（`Record` を使う唯一の既存呼び出し箇所の型変更のみ）
- Modify: `crates/polaris-cli/src/main.rs`（`AuditLog::open` の戻り値を包む箇所）

**Interfaces:**
- Consumes: 既存の `AuditLog`/`Record`
- Produces:
  - `Record<'a>` へ `pub caller: &'a str` フィールドを追加（既存フィールドはそのまま。ルートは `"root"`、subagent は自分の型名を渡す）
  - `AuditLog::record` のシグネチャは `&mut self` のまま変えない（`Arc<Mutex<AuditLog>>` で包むのは呼び出し側の責務とし、`AuditLog` 自体は単純さを保つ）

`AuditLog` 自体を `Arc<Mutex<_>>` 対応の内部実装に変えるのではなく、既存の `&mut self` API はそのまま残し、共有が要る箇所（Task 9 の subagent 実行）で呼び出し側が `Arc<Mutex<AuditLog>>` を保持し、書き込み時だけロックを取る設計にする。こうすることで `AuditLog` 自体の型は変わらず、ルートの既存呼び出し（`audit.record(...)`、`&mut AuditLog` を直接持つ）は無修正で動く。

- [ ] **Step 1: 失敗するテストを書く（`caller` フィールド）**

`crates/polaris-core/src/audit.rs` の既存テストモジュールに追加する。

```rust
#[test]
fn caller_distinguishes_root_from_a_subagent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.jsonl");
    let mut log = AuditLog::open(&path).unwrap();
    log.record(&Record {
        tool: "read",
        detail: "{}",
        sandbox: None,
        target: None,
        result: "ok",
        caller: "root",
    })
    .unwrap();
    log.record(&Record {
        tool: "read",
        detail: "{}",
        sandbox: None,
        target: None,
        result: "ok",
        caller: "file-inspector",
    })
    .unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains("\"caller\":\"root\""));
    assert!(lines[1].contains("\"caller\":\"file-inspector\""));
}
```

Run: `cargo test -p polaris-core caller_distinguishes_root_from_a_subagent`

Expected: FAIL — `Record` に `caller` フィールドが無くコンパイルエラー。

- [ ] **Step 2: `Record`/`record` を拡張する**

`crates/polaris-core/src/audit.rs` の既存の `pub struct Record<'a> { pub tool: &'a str, pub detail: &'a str, pub sandbox: Option<&'a polaris_sandbox::SandboxPolicy>, pub target: Option<&'a Path>, pub result: &'a str }` へ末尾に1フィールド追加する。

```rust
pub struct Record<'a> {
    pub tool: &'a str,
    pub detail: &'a str,
    pub sandbox: Option<&'a polaris_sandbox::SandboxPolicy>,
    pub target: Option<&'a Path>,
    pub result: &'a str,
    /// 呼び出し主体。ルートは `"root"`、subagent はその型名。全ての新規
    /// フィールドは screen を経由するという既存の不変条件に従い、必ず
    /// screen(r.caller) を通す。
    pub caller: &'a str,
}
```

既存の `record` 本体、`serde_json::json!({...})` の組み立てへ1行足す。

```rust
let mut line = serde_json::json!({
    "tool": screen(r.tool),
    "detail": truncate_field(&screen(r.detail), MAX_DETAIL_BYTES),
    "result": truncate_field(&screen(r.result), MAX_RESULT_BYTES),
    "caller": screen(r.caller),
});
```

Run: `cargo test -p polaris-core caller_distinguishes_root_from_a_subagent`

Expected: PASS。

- [ ] **Step 3: 既存の唯一の呼び出し箇所（`agent.rs` のルートループ）を直す**

`crates/polaris-core/src/agent.rs` の既存の `audit.record(&Record { tool: &call.name, detail: &call.arguments.to_string(), sandbox: is_mutation.then_some(ctx.sandbox), target: target.as_deref(), result })?;` へ `caller: "root",` を追加する。

Run: `cargo test -p polaris-core`

Expected: 既存テストがすべて通る（`caller` を検証していない既存テストは無修正で動くが、コンパイルは新フィールド必須のため、この呼び出し箇所を直さない限り失敗する）。

- [ ] **Step 4: 全体検証とコミット**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

```bash
git add crates/polaris-core/src/audit.rs crates/polaris-core/src/agent.rs
git commit -m "feat(polaris-core): add caller field to audit records

subagent のツール呼び出しをルートと区別して監査ログに残すための下地。
AuditLog 自体の共有方法(Arc<Mutex<>>)は Task 9 で導入する呼び出し側
の責務とし、AuditLog 自体の型は変えない。

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 5: `dispatch` を `async fn` にする（挙動は変えない）

**Files:**
- Modify: `crates/polaris-core/src/agent.rs`
- Modify: `crates/polaris-core/Cargo.toml`（`tokio` を `[dependencies]` へ昇格、`sync` feature 追加）
- Modify: `Cargo.toml`（workspace の `tokio` feature リストへ `sync` を追加）

**Interfaces:**
- Consumes: 既存の `fn dispatch(call, skills, ctx) -> Result<String, String>`
- Produces: `async fn dispatch(call, skills, ctx) -> Result<String, String>`（シグネチャの型自体は変わらないが `async` になる）

この時点では `dispatch` 内のどの腕も実際には `.await` しない（`spawn` 腕はまだ無い）。目的は Task 9 で `spawn` 腕が非同期の並列実行を `.await` できるようにする土台だけを、挙動を変えずに先に入れておくこと。

- [ ] **Step 1: workspace の `tokio` feature に `sync` を追加する**

`Cargo.toml`（ワークスペースルート）の `[workspace.dependencies]` にある既存の

```toml
tokio = { version = "1", features = ["rt-multi-thread", "macros", "fs", "io-util", "net", "time"] }
```

を

```toml
tokio = { version = "1", features = ["rt-multi-thread", "macros", "fs", "io-util", "net", "time", "sync"] }
```

に変える（`Semaphore`/`Mutex` は Task 9・Task 4 の呼び出し側実装が使う）。

- [ ] **Step 2: `polaris-core` の `Cargo.toml` で `tokio` を `[dependencies]` へ昇格する**

`crates/polaris-core/Cargo.toml` の現在 `[dev-dependencies]` にある `tokio = { workspace = true }` を `[dependencies]` へ移す（`async-trait`・`tempfile` 等、他の既存 dev-dependency はそのまま `[dev-dependencies]` に残す）。

Run: `cargo build -p polaris-core`

Expected: 成功。`agent::run` は元々 `async fn` であり `provider.complete().await` を呼んでいたが、ランタイム自体は呼び出し元（`polaris-cli`）が提供していた。これで `polaris-core` 自身も `tokio` 型（`Semaphore`/`Mutex`）を扱えるようになる。

- [ ] **Step 3: `dispatch` を `async fn` にする**

`crates/polaris-core/src/agent.rs` の `fn dispatch(...)` 宣言を `async fn dispatch(...)` に変える。本文（5つの既存腕）は無修正——どの腕も `.await` を必要としない同期処理のままでよい。

- [ ] **Step 4: 呼び出し箇所を直す**

`run()` 内、既存の `let outcome = dispatch(call, skills, ctx);` を `let outcome = dispatch(call, skills, ctx).await;` に変える。

- [ ] **Step 5: コンパイルエラーを追って直す**

Run: `cargo build --workspace --tests 2>&1 | head -100`

`dispatch` を直接呼んでいるテスト関数（`agent.rs` の `#[cfg(test)] mod tests` 内、`declared_required_param` を使ったスキーマ結び付けテスト群など）はすべて型エラーとして検出される——`dispatch(...)` の戻り値が `Result<String, String>` から `impl Future<Output = Result<String, String>>` に変わるため、`.as_str()` 等その後の操作がすべてコンパイルエラーになる。エラーが出た箇所ごとに次の機械的な変更を行う。

1. `dispatch(...)` の呼び出し箇所に `.await` を足す
2. その呼び出しを含む `#[test] fn foo() { ... }` を `#[tokio::test] async fn foo() { ... }` に変える

これを `cargo build --workspace --tests` がエラーを出さなくなるまで繰り返す。コンパイラの型エラーが機械的に指し示すため、見落としは起きない。

- [ ] **Step 6: 全体検証とコミット**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

Expected: 全テストが無修正の挙動のまま通る（非同期化しただけで、どの腕もまだ実際には await 待ちしないため）。

```bash
git add Cargo.toml crates/polaris-core/Cargo.toml crates/polaris-core/src/agent.rs
git commit -m "refactor(polaris-core): make dispatch async

spawn ツール(Task 9)が並列 subagent 実行を await できるようにする
ための土台。この時点ではどの既存の腕も実際には await せず、挙動は
変わらない。

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 6: 結果スキーマ検証（`jsonschema` クレート）

**Files:**
- Modify: `Cargo.toml`（`[workspace.dependencies]` へ `jsonschema` 追加）
- Modify: `crates/polaris-tools/Cargo.toml`
- Create: `crates/polaris-tools/src/schema_validate.rs`
- Modify: `crates/polaris-tools/src/lib.rs`（`mod schema_validate; pub use schema_validate::...;`）

**Interfaces:**
- Produces: `pub fn validate(schema: &serde_json::Value, instance: &serde_json::Value) -> Result<(), String>`（`Err` はモデルへそのまま返せる、人が読める検証エラー文字列）

- [ ] **Step 1: 依存を追加する**

`Cargo.toml`（ワークスペースルート）の `[workspace.dependencies]` へ追加する。

```toml
jsonschema = "0.26"
```

正確なバージョンは `cargo add --package polaris-tools jsonschema --dry-run` で確認し、ここに書いた値と異なれば実際に解決された値へ合わせる。

`crates/polaris-tools/Cargo.toml` の `[dependencies]` へ追加する。

```toml
jsonschema = { workspace = true }
```

Run: `cargo build -p polaris-tools`

Expected: 依存解決が成功する。

- [ ] **Step 2: 失敗するテストを書く**

`crates/polaris-tools/src/schema_validate.rs` を新規作成する。

```rust
//! subagent の結果を、型が宣言した JSON Schema に照合するだけの薄い
//! ラッパー。バリデータ自体のエラー表現には立ち入らず、モデルへ
//! そのまま返せる1つの文字列へ畳む。

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_matching_instance_passes() {
        let schema = json!({
            "type": "object",
            "required": ["summary"],
            "properties": { "summary": { "type": "string" } }
        });
        let instance = json!({ "summary": "ok" });
        assert!(validate(&schema, &instance).is_ok());
    }

    #[test]
    fn a_missing_required_field_fails_with_a_readable_message() {
        let schema = json!({
            "type": "object",
            "required": ["summary"],
            "properties": { "summary": { "type": "string" } }
        });
        let instance = json!({});
        let err = validate(&schema, &instance).unwrap_err();
        assert!(err.contains("summary"), "{err}");
    }
}
```

Run: `cargo test -p polaris-tools schema_validate::tests`

Expected: FAIL — `validate` は未定義。

- [ ] **Step 3: `validate` を実装する**

同じファイルへ追加する。

```rust
/// `instance` を `schema` へ照合する。不一致なら、再試行時にモデルへ
/// そのまま見せられる1文へ畳んだ理由を返す。`jsonschema` クレート自体の
/// コンパイル済みバリデータの構築失敗（スキーマ自体が不正な JSON Schema
/// である場合）も同じ `Err(String)` として扱う——型定義の作者の誤りと
/// subagent の出力の誤りを、呼び出し側では区別する必要が無いため。
pub fn validate(schema: &serde_json::Value, instance: &serde_json::Value) -> Result<(), String> {
    let compiled = jsonschema::validator_for(schema)
        .map_err(|e| format!("the output schema itself is invalid: {e}"))?;
    let errors: Vec<String> = compiled
        .iter_errors(instance)
        .map(|e| format!("{} at {}", e, e.instance_path))
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}
```

`jsonschema` クレートの実際の API 名（`validator_for`/`iter_errors`/エラー型の `Display`/`instance_path` の有無）は、上記が想定と食い違えばコンパイルエラーとしてすぐ判明する。実装時に `cargo doc -p jsonschema --open` かクレートのドキュメントで正確なメソッド名を確認し、同じ意味（コンパイル済みバリデータを作る・不一致点を列挙する）を持つ実際の呼び出しへ置き換える。

- [ ] **Step 4: テストを通し、`lib.rs` へ配線する**

Run: `cargo test -p polaris-tools schema_validate::tests`

Expected: PASS。

`crates/polaris-tools/src/lib.rs` へ追加する。

```rust
mod schema_validate;
pub use schema_validate::validate;
```

- [ ] **Step 5: 全体検証とコミット**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

```bash
git add Cargo.toml crates/polaris-tools/
git commit -m "feat(polaris-tools): add JSON Schema validation for subagent results

M4のresult検証(1回だけ再試行、なお不一致ならタスク単体の失敗)が使う。
jsonschemaクレートを新規依存として追加した——ワークスペースに既存の
値照合バリデータが無かったため。

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 7: `StopTracker` へ壁時計とスキーマ不一致の停止条件を追加する

**Files:**
- Modify: `crates/polaris-core/src/stop.rs`

**Interfaces:**
- Consumes: 既存の `StopTracker`/`StopReason`
- Produces:
  - `StopReason::WallSeconds(u32)`、`StopReason::SchemaMismatch`（新規バリアント）
  - `StopTracker::with_wall_seconds(max_turns: u32, wall_seconds: u32) -> Self`（既存の `StopTracker::new(max_turns: u32) -> Self` は変えない。壁時計を持たないルートの既存呼び出しは無修正のまま）
  - `StopTracker::observe_wall_clock(&mut self) -> Option<StopReason>`
  - `StopTracker::observe_schema_mismatch(&mut self) -> Option<StopReason>`（2回目の不一致で `Some(StopReason::SchemaMismatch)`）

既存の `observe_turn`/`observe_error`/`observe_success` のシグネチャ・挙動は一切変えない。

- [ ] **Step 1: 失敗するテストを書く（壁時計超過）**

`crates/polaris-core/src/stop.rs` の既存テストモジュールに追加する。壁時計の経過を実時間の `sleep` に頼らず決定的にテストするため、`Instant` を外から差し込めるようにする。

```rust
#[test]
fn wall_clock_trips_after_the_declared_seconds() {
    let start = std::time::Instant::now();
    let mut t = StopTracker::with_wall_seconds(100, 0); // 0秒 = 即座に超過
    std::thread::sleep(std::time::Duration::from_millis(1));
    let r = t.observe_wall_clock();
    assert!(matches!(r, Some(StopReason::WallSeconds(0))));
    let _ = start; // 経過確認は表示上の意図のみ、アサーション自体は上のmatchesで完結
}

#[test]
fn wall_clock_does_not_trip_before_the_declared_seconds() {
    let mut t = StopTracker::with_wall_seconds(100, 3600);
    assert_eq!(t.observe_wall_clock(), None);
}

#[test]
fn a_tracker_without_wall_seconds_never_trips_on_wall_clock() {
    let mut t = StopTracker::new(100);
    assert_eq!(t.observe_wall_clock(), None);
}

#[test]
fn schema_mismatch_trips_on_the_second_occurrence() {
    let mut t = StopTracker::new(100);
    assert_eq!(t.observe_schema_mismatch(), None);
    assert_eq!(t.observe_schema_mismatch(), Some(StopReason::SchemaMismatch));
}
```

Run: `cargo test -p polaris-core stop::`

Expected: FAIL — `with_wall_seconds`/`observe_wall_clock`/`observe_schema_mismatch`/新バリアントが未定義。

- [ ] **Step 2: 実装する**

`crates/polaris-core/src/stop.rs` の既存の `pub enum StopReason { RepeatedError(String), MaxTurns }` へバリアントを足す。

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// The same error occurred 3 times in a row.
    RepeatedError(String),
    /// The turn limit was reached.
    MaxTurns,
    /// A subagent's declared wall-clock budget (seconds) was exceeded.
    WallSeconds(u32),
    /// A subagent's output failed schema validation twice in a row.
    SchemaMismatch,
}
```

既存の `pub struct StopTracker { last_error: Option<String>, streak: u32, turns: u32, max_turns: u32 }` へフィールドを足す。

```rust
pub struct StopTracker {
    last_error: Option<String>,
    streak: u32,
    turns: u32,
    max_turns: u32,
    wall_seconds: Option<u32>,
    started: Option<std::time::Instant>,
    schema_mismatches: u32,
}
```

既存の `impl StopTracker { pub fn new(max_turns: u32) -> Self { ... } }` はそのまま残し（新フィールドは `None`/`0` で初期化するよう本体を1行拡張するだけ）、新しいコンストラクタと新メソッドを追加する。

```rust
impl StopTracker {
    pub fn new(max_turns: u32) -> Self {
        Self {
            last_error: None,
            streak: 0,
            turns: 0,
            max_turns,
            wall_seconds: None,
            started: None,
            schema_mismatches: 0,
        }
    }

    /// subagent 用。壁時計の起点はこの呼び出し時点になる。
    pub fn with_wall_seconds(max_turns: u32, wall_seconds: u32) -> Self {
        Self {
            wall_seconds: Some(wall_seconds),
            started: Some(std::time::Instant::now()),
            ..Self::new(max_turns)
        }
    }

    /// 壁時計を持たないトラッカー(ルート)では常に `None`。
    pub fn observe_wall_clock(&mut self) -> Option<StopReason> {
        let (limit, started) = (self.wall_seconds?, self.started?);
        if started.elapsed().as_secs() >= u64::from(limit) {
            Some(StopReason::WallSeconds(limit))
        } else {
            None
        }
    }

    /// 2回連続の不一致で停止する。1回目は `None` を返し、呼び出し側が
    /// 検証エラーを添えて1回だけ再試行する運びになる。
    pub fn observe_schema_mismatch(&mut self) -> Option<StopReason> {
        self.schema_mismatches += 1;
        if self.schema_mismatches >= 2 {
            Some(StopReason::SchemaMismatch)
        } else {
            None
        }
    }
}
```

- [ ] **Step 3: テストを通す**

Run: `cargo test -p polaris-core stop::`

Expected: PASS。

- [ ] **Step 4: 全体検証とコミット**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

```bash
git add crates/polaris-core/src/stop.rs
git commit -m "feat(polaris-core): add wall-clock and schema-mismatch stop reasons

subagent 向け。既存の observe_turn/observe_error/observe_success の
シグネチャ・挙動は変えていない。ルート(StopTracker::new)は壁時計を
持たないため observe_wall_clock は常に None を返す。

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 8: `run` を土台として分離する — subagent が使う `run_loop`

**Files:**
- Modify: `crates/polaris-core/src/agent.rs`

**Interfaces:**
- Consumes: Task 5 で async 化済みの `dispatch`
- Produces:
  - `pub(crate) async fn run_loop(provider: &dyn Provider, session: &mut Session, audit: &mut AuditLog, stop: &mut StopTracker, system: &str, tools: &[polaris_tools::ToolSpec], skills: &[polaris_skills::Skill], caller: &str, ctx: &mut ToolContext<'_>) -> Result<String, AgentError>`
  - 既存の `pub async fn run(...)` はシグネチャ・挙動を変えず、内部で `run_loop` を呼ぶだけになる

`system`/`tools` を `AlwaysOn` からではなく直接引数として受け取るのは、`AlwaysOn` がルートの常時コンテキスト予算を守るためにわざと不変（ミューテータ無し）に設計されているため——`prompt.rs` の `AlwaysOn` の doc comment 参照。subagent のシステムプロンプト（型の SKILL.md 本文）とツール部分集合はルートの `AlwaysOn` に混ぜず、別経路で渡す。

- [ ] **Step 1: 失敗するテストを書く（`run_loop` が `run` と同じ結果を返す）**

`crates/polaris-core/src/agent.rs` の既存テストモジュールに追加する。既存のモックプロバイダ（`run` の既存テストが使っているもの、例えば `agent.rs` 内で定義されている `MockProvider` 相当）をそのまま使う。

```rust
#[tokio::test]
async fn run_loop_produces_the_same_result_as_run_for_an_equivalent_call() {
    let provider = mock_provider_returning_final_text("hello from run_loop");
    let mut session = Session::new();
    session.push_user("hi");
    let mut audit = AuditLog::open(&tempfile::NamedTempFile::new().unwrap().into_temp_path()).unwrap();
    let mut stop = StopTracker::new(10);
    let sandbox = polaris_sandbox::SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[]).unwrap();
    let helper = std::path::PathBuf::from("/bin/true");
    let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::Never);
    let mut approver = AlwaysAllow { asked: 0 };
    let mut ctx = ToolContext {
        sandbox: &sandbox,
        helper: &helper,
        gate: &mut gate,
        approver: &mut approver,
    };
    let result = run_loop(
        &provider,
        &mut session,
        &mut audit,
        &mut stop,
        "system prompt",
        &polaris_tools::all_specs(),
        &[],
        "root",
        &mut ctx,
    )
    .await
    .unwrap();
    assert_eq!(result, "hello from run_loop");
}
```

`mock_provider_returning_final_text` は既存テストが同種のモックを既に持っていれば同じものを再利用し、無ければ既存の `run` のテスト群がプロバイダをモックしている実際のパターン（`Provider` トレイトを実装するテスト用構造体）に倣って追加する。`AlwaysAllow` は既存の `agent.rs::tests::AlwaysAllow`（Task 実装時に見つかる、`asked: usize` を持つ構造体）をそのまま使う。

Run: `cargo test -p polaris-core run_loop_produces_the_same_result_as_run_for_an_equivalent_call`

Expected: FAIL — `run_loop` が未定義。

- [ ] **Step 2: `run_loop` を実装し、`run` をその薄い委譲にする**

既存の `pub async fn run(...) -> Result<String, AgentError> { loop { ... } }`（本文全体）を次の2つへ分割する。

```rust
pub async fn run(
    provider: &dyn Provider,
    session: &mut Session,
    audit: &mut AuditLog,
    stop: &mut StopTracker,
    always_on: &crate::prompt::AlwaysOn,
    skills: &[polaris_skills::Skill],
    ctx: &mut ToolContext<'_>,
) -> Result<String, AgentError> {
    run_loop(
        provider,
        session,
        audit,
        stop,
        always_on.system(),
        always_on.tools(),
        skills,
        "root",
        ctx,
    )
    .await
}

/// ルートの `run` と subagent 実行(Task 9)の両方が使う、ターン取りの
/// 中核。`system`/`tools` を `AlwaysOn` からではなく直接受け取るのは、
/// subagent の型ごとに異なるシステムプロンプトとツール部分集合を、
/// ルート用に不変設計された `AlwaysOn` に混ぜないため。
pub(crate) async fn run_loop(
    provider: &dyn Provider,
    session: &mut Session,
    audit: &mut AuditLog,
    stop: &mut StopTracker,
    system: &str,
    tools: &[polaris_tools::ToolSpec],
    skills: &[polaris_skills::Skill],
    caller: &str,
    ctx: &mut ToolContext<'_>,
) -> Result<String, AgentError> {
    loop {
        if let Some(r) = stop.observe_turn() {
            return Err(AgentError::Stopped(r));
        }
        if let Some(r) = stop.observe_wall_clock() {
            return Err(AgentError::Stopped(r));
        }

        let res = provider
            .complete(CompletionRequest {
                system: system.to_string(),
                messages: session.messages.clone(),
                tools: tools.to_vec(),
            })
            .await?;

        if res.tool_calls.is_empty() {
            session.push_assistant(&res.text);
            return Ok(res.text);
        }

        session.push_assistant_tool_calls(&res.text, res.tool_calls.clone());

        for call in &res.tool_calls {
            let outcome = dispatch(call, skills, ctx).await;
            let result: &str = match &outcome {
                Ok(body) => body.as_str(),
                Err(msg) => msg.as_str(),
            };
            let is_mutation = call.name == "write" || call.name == "edit";
            let target: Option<PathBuf> = if is_mutation {
                call.arguments["path"].as_str().map(PathBuf::from)
            } else {
                None
            };
            audit.record(&Record {
                tool: &call.name,
                detail: &call.arguments.to_string(),
                sandbox: is_mutation.then_some(ctx.sandbox),
                target: target.as_deref(),
                result,
                caller,
            })?;
            match outcome {
                Ok(body) => {
                    stop.observe_success();
                    session.push_tool_result(&call.id, &body);
                }
                Err(msg) => {
                    if let Some(r) = stop.observe_error(&msg) {
                        return Err(AgentError::Stopped(r));
                    }
                    session.push_tool_result(&call.id, &msg);
                }
            }
        }
    }
}
```

`stop.observe_wall_clock()` の呼び出しを追加した点だけが `run` の既存挙動からの差分——ルート用の `StopTracker::new(...)` は壁時計を持たないため常に `None` を返し、ルートの挙動は変わらない。

- [ ] **Step 3: 既存の `run` のテストがすべて通ることを確認する**

Run: `cargo test -p polaris-core`

Expected: 既存の `run` に対するテスト（`stops_when_tool_fails_three_times` 等）がすべて無修正で通る——`run` は `run_loop` へ委譲するだけで、渡す `system`/`tools`/`caller` は元の `always_on.system()`/`always_on.tools()`/`"root"` であり、挙動は完全に一致する。

- [ ] **Step 4: 全体検証とコミット**

Run: `cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

```bash
git add crates/polaris-core/src/agent.rs
git commit -m "refactor(polaris-core): extract run_loop as the shared turn-taking core

run はその薄い委譲になった。挙動は変えていない——既存テストが無修正
で通ることで確認済み。Task 9 の spawn がこの run_loop を subagent
ごとに独立したセッション/ツール部分集合で呼ぶ。

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 9: `spawn` ツール — スキーマとディスパッチ腕、単一 subagent の実行

**Files:**
- Modify: `crates/polaris-tools/src/lib.rs`（`spawn_spec()`、`all_specs()` への算入）
- Create: `crates/polaris-core/src/spawn.rs`
- Modify: `crates/polaris-core/src/agent.rs`（`dispatch`/`run`/`run_loop` のシグネチャへ `agent_types`/`provider_pool`/`shared_audit` を追加、`"spawn"` 腕、`AutoApprove` 新設）
- Modify: `crates/polaris-cli/src/main.rs`（`Provider`/`AuditLog` を `Arc` 化し、新引数を渡す）
- Create: `agents/file-inspector/SKILL.md`、`agents/file-inspector/references/result.schema.json`

この Task では並列実行そのもの（Task 10）にはまだ触れず、1波1タスクの `spawn` 呼び出しが最初から最後まで動くところまでを対象にする。

**Interfaces:**
- Produces:
  - `spawn_spec() -> ToolSpec`（`crates/polaris-tools/src/lib.rs`）
  - `pub struct SpawnTask { pub agent_type: String, pub task: String, pub write_root: Option<String> }`（`crates/polaris-core/src/spawn.rs`）
  - `pub async fn run_wave(tasks: Vec<SpawnTask>, agent_types: &[polaris_skills::AgentType], provider: Arc<dyn Provider>, audit: Arc<Mutex<AuditLog>>, base_sandbox: &SandboxPolicy, helper: &Path) -> String`（このタスクでは `tasks.len() == 1` を前提にした直列実装でよい。Task 10 で並列化する）

- [ ] **Step 1: `spawn_spec()` を追加する**

`crates/polaris-tools/src/lib.rs` の既存の `skill_spec()` の直後に追加する。仕様の呼び出し例 `spawn([{type, task}, ...])` は概念的な記法であり、OpenAI 互換の function calling は最上位が object のスキーマを要求する（既存5ツールすべてが `"type": "object"` である理由と同じ）ため、配列を `"tasks"` キーで包む。

```rust
fn spawn_spec() -> ToolSpec {
    ToolSpec {
        name: "spawn",
        description: "Run one or more subagents in parallel, one wave. Each subagent runs its own bounded loop and returns a schema-validated result; its intermediate steps never enter your context.",
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "tasks": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "type": {
                                "type": "string",
                                "description": "The subagent type's name, from agents/<type>/SKILL.md."
                            },
                            "task": {
                                "type": "string",
                                "description": "What this subagent should do."
                            },
                            "write_root": {
                                "type": "string",
                                "description": "Required only for a read-write type: the single directory this subagent may write under."
                            }
                        },
                        "required": ["type", "task"]
                    }
                }
            },
            "required": ["tasks"]
        }),
    }
}
```

`all_specs()` の末尾へ足す。

```rust
pub fn all_specs() -> Vec<ToolSpec> {
    vec![
        read_spec(),
        write_spec(),
        edit_spec(),
        bash_spec(),
        skill_spec(),
        spawn_spec(),
    ]
}
```

Run: `cargo test -p polaris-tools all_specs_has_unique_names`

Expected: PASS（既存テストが `spawn` という新しい名前の重複が無いことを含めて確認する）。

- [ ] **Step 2: `AutoApprove` を新設する**

`crates/polaris-core/src/agent.rs` へ追加する（`ToolContext` の定義の近く）。非対話環境（subagent はバックグラウンドで動き、承認を求める相手がいない）向け。書込先はこの手前で衝突検査済み・サンドボックスの書込可能ルートで強制済みであるため、ここでの承認は形式上のゲートを素通りさせるだけで安全性は失わない——実際の強制はサンドボックス側が担う。

```rust
/// subagent 用。人に尋ねる相手がいないため常に許可する。実際の強制は
/// サンドボックスの書込可能ルート(spawn が宣言と同一のオブジェクトとして
/// 構築する)が担うため、ここでの許可は安全性を弱めない。
pub(crate) struct AutoApprove;

impl crate::approval::Approver for AutoApprove {
    fn ask(&mut self, _reason: &str) -> crate::approval::Decision {
        crate::approval::Decision::Allow
    }
}
```

- [ ] **Step 3: `spawn.rs` — `SpawnTask` と単一タスクの実行**

`crates/polaris-core/src/spawn.rs` を新規作成する。

```rust
//! `spawn` ツールの実体。型の discovery は `polaris_tools::skill::lookup`
//! と同じ実装を再利用し(Task 1/3)、実行そのものは `agent::run_loop` を
//! 呼ぶ。並列化はこのファイルの中だけで完結させ、`agent::dispatch` の
//! 外側ループ自体は逐次のまま変えない(Task 5 の Architecture 参照)。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Mutex;

use polaris_provider::Provider;
use polaris_sandbox::{SandboxMode, SandboxPolicy};
use polaris_skills::{AgentAccess, AgentType};

use crate::agent::{AutoApprove, ToolContext, run_loop};
use crate::approval::{ApprovalPolicy, Gate};
use crate::audit::AuditLog;
use crate::session::Session;
use crate::stop::StopTracker;

#[derive(Debug, Clone)]
pub struct SpawnTask {
    pub agent_type: String,
    pub task: String,
    pub write_root: Option<String>,
}

#[derive(Debug)]
pub enum TaskOutcome {
    Ok(String),
    Failed(String),
}

/// 1件のタスクを、解決済みの型定義に沿って実行する。型が見つからない
/// 場合は `polaris_tools::skill::lookup` と同じ候補一覧の形でエラーを
/// 返す——discovery/検索の実装を再利用するだけでなく、失敗時の応答の
/// 形もルータ節のパターンをそのまま踏襲する。
pub async fn run_one(
    task: &SpawnTask,
    agent_types: &[AgentType],
    provider: Arc<dyn Provider>,
    audit: Arc<Mutex<AuditLog>>,
    base_sandbox: &SandboxPolicy,
    helper: &Path,
) -> TaskOutcome {
    let Some(agent) = agent_types.iter().find(|a| a.name == task.agent_type) else {
        let candidates = polaris_tools::skill::lookup(agent_types, &task.agent_type);
        return TaskOutcome::Failed(format!(
            "unknown subagent type {:?}. {candidates}",
            task.agent_type
        ));
    };

    // 深さ1: allowed_tools に何が書かれていても spawn は無条件で除外する。
    let mut tools: Vec<polaris_tools::ToolSpec> = polaris_tools::all_specs()
        .into_iter()
        .filter(|t| t.name != "spawn" && agent.allowed_tools.iter().any(|a| a == t.name))
        .collect();
    tools.sort_by_key(|t| t.name);

    let sandbox = match resolve_subagent_sandbox(agent, task, base_sandbox) {
        Ok(s) => s,
        Err(e) => return TaskOutcome::Failed(e),
    };

    let mut session = Session::new();
    session.push_user(&task.task);
    let mut stop = StopTracker::with_wall_seconds(agent.max_turns, agent.wall_seconds);
    let mut gate = Gate::new(ApprovalPolicy::Never);
    let mut approver = AutoApprove;
    let mut ctx = ToolContext {
        sandbox: &sandbox,
        helper,
        gate: &mut gate,
        approver: &mut approver,
    };

    let result = {
        let mut audit_guard = audit.lock().await;
        run_loop(
            provider.as_ref(),
            &mut session,
            &mut audit_guard,
            &mut stop,
            &agent.body,
            &tools,
            &[],
            &agent.name,
            &mut ctx,
        )
        .await
    };
    // ロックは1回のツール呼び出しの間だけ保持したいが、run_loop はループ
    // 全体でaudit を借用する。複数 subagent が同時に動くと、ある subagent
    // のターン全体の間、他の subagent の監査ログ書き込みが待たされる。
    // これは並列度(8)に対して許容できる直列化であり、ログの完全性の方を
    // 優先する——Task 10 のレビューで実測を見て、細かすぎると分かれば
    // ロック粒度をツール呼び出し単位まで下げる。

    match result {
        Ok(text) => match validate_output(agent, &text) {
            Ok(()) => TaskOutcome::Ok(text),
            Err(first_err) => {
                // 1回だけ、検証エラーを添えて再試行する。
                session.push_user(&format!(
                    "Your previous output did not match the required schema: {first_err}. \
                     Return output matching the schema exactly."
                ));
                let mut stop2 = StopTracker::with_wall_seconds(agent.max_turns, agent.wall_seconds);
                let mut gate2 = Gate::new(ApprovalPolicy::Never);
                let mut approver2 = AutoApprove;
                let mut ctx2 = ToolContext {
                    sandbox: &sandbox,
                    helper,
                    gate: &mut gate2,
                    approver: &mut approver2,
                };
                let retry = {
                    let mut audit_guard = audit.lock().await;
                    run_loop(
                        provider.as_ref(),
                        &mut session,
                        &mut audit_guard,
                        &mut stop2,
                        &agent.body,
                        &tools,
                        &[],
                        &agent.name,
                        &mut ctx2,
                    )
                    .await
                };
                match retry {
                    Ok(text2) => match validate_output(agent, &text2) {
                        Ok(()) => TaskOutcome::Ok(text2),
                        Err(second_err) => {
                            TaskOutcome::Failed(format!("schema mismatch after retry: {second_err}"))
                        }
                    },
                    Err(e) => TaskOutcome::Failed(e.to_string()),
                }
            }
        },
        Err(e) => TaskOutcome::Failed(e.to_string()),
    }
}

fn validate_output(agent: &AgentType, text: &str) -> Result<(), String> {
    let schema_text = std::fs::read_to_string(&agent.output_schema)
        .map_err(|e| format!("cannot read output schema {}: {e}", agent.output_schema.display()))?;
    let schema: serde_json::Value = serde_json::from_str(&schema_text)
        .map_err(|e| format!("output schema is not valid JSON: {e}"))?;
    let instance: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| format!("subagent output is not valid JSON: {e}"))?;
    polaris_tools::validate(&schema, &instance)
}

/// 型の `access` を実際のサンドボックス方針へ写す。read-write の場合、
/// タスクが宣言した `write_root` を親の書込可能ルートの範囲内に限る——
/// 親より強い権限を得る経路を作らない不変条件をここで検査する。
fn resolve_subagent_sandbox(
    agent: &AgentType,
    task: &SpawnTask,
    base_sandbox: &SandboxPolicy,
) -> Result<SandboxPolicy, String> {
    match agent.access {
        AgentAccess::Read => SandboxPolicy::new(SandboxMode::ReadOnly, &[])
            .map_err(|e| format!("cannot build read-only sandbox: {e}")),
        AgentAccess::ReadWrite => {
            let Some(root) = &task.write_root else {
                return Err(format!(
                    "{} is a read-write type and requires write_root",
                    agent.name
                ));
            };
            let root_path = PathBuf::from(root);
            let canonical = root_path
                .canonicalize()
                .map_err(|e| format!("write_root {root} does not exist: {e}"))?;
            if !base_sandbox.contains(&canonical) {
                return Err(format!(
                    "write_root {root} is outside the parent's own writable roots"
                ));
            }
            SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[canonical])
                .map_err(|e| format!("cannot build subagent sandbox: {e}"))
        }
    }
}
```

- [ ] **Step 4: `dispatch`/`run_loop` へ `agent_types`/`provider_pool`/`shared_audit`/`base_sandbox`/`helper` を通す**

`crates/polaris-core/src/agent.rs` の `dispatch` のシグネチャを拡張する。

```rust
async fn dispatch(
    call: &polaris_provider::ToolCall,
    skills: &[polaris_skills::Skill],
    agent_types: &[polaris_skills::AgentType],
    provider_pool: Arc<dyn polaris_provider::Provider>,
    shared_audit: Arc<tokio::sync::Mutex<AuditLog>>,
    ctx: &mut ToolContext<'_>,
) -> Result<String, String> {
    match call.name.as_str() {
        // 既存の "read"/"write"/"edit"/"bash"/"skill" の5腕は無修正。
        "spawn" => {
            let tasks_json = call.arguments["tasks"]
                .as_array()
                .ok_or_else(|| "tasks is missing".to_string())?;
            let mut tasks = Vec::with_capacity(tasks_json.len());
            for t in tasks_json {
                let agent_type = t["type"]
                    .as_str()
                    .ok_or_else(|| "tasks[].type is missing".to_string())?
                    .to_string();
                let task = t["task"]
                    .as_str()
                    .ok_or_else(|| "tasks[].task is missing".to_string())?
                    .to_string();
                let write_root = t["write_root"].as_str().map(str::to_string);
                tasks.push(crate::spawn::SpawnTask {
                    agent_type,
                    task,
                    write_root,
                });
            }
            Ok(crate::spawn::run_wave(
                tasks,
                agent_types,
                provider_pool,
                shared_audit,
                ctx.sandbox,
                ctx.helper,
            )
            .await)
        }
        other => Err(format!("unknown tool: {other}")),
    }
}
```

このシグネチャ変更は `run_loop`/`run` にも波及する——`run_loop`/`run` の両方へ `agent_types: &[polaris_skills::AgentType]`、`provider_pool: Arc<dyn Provider>`、`shared_audit: Arc<Mutex<AuditLog>>` を追加し、`dispatch` 呼び出しへそのまま横流しする。既存の `provider: &dyn Provider`・`audit: &mut AuditLog` 引数は変えない——ルート自身のループは今まで通りこの2つを直接使い、新しい `provider_pool`/`shared_audit` は `spawn` 腕専用に別途渡される。

`run_wave`（Task 9 では `tasks.len() == 1` を前提にした最小実装、Task 10 で並列化する）を `spawn.rs` へ追加する。

```rust
pub async fn run_wave(
    tasks: Vec<SpawnTask>,
    agent_types: &[AgentType],
    provider: Arc<dyn Provider>,
    audit: Arc<Mutex<AuditLog>>,
    base_sandbox: &SandboxPolicy,
    helper: &Path,
) -> String {
    // Task 10 で並列化・衝突検査を追加するまでの最小実装: 逐次に実行する。
    let mut lines = Vec::with_capacity(tasks.len());
    for task in &tasks {
        let outcome = run_one(task, agent_types, provider.clone(), audit.clone(), base_sandbox, helper).await;
        match outcome {
            TaskOutcome::Ok(text) => lines.push(format!("{}: {text}", task.agent_type)),
            TaskOutcome::Failed(msg) => lines.push(format!("{}: FAILED — {msg}", task.agent_type)),
        }
    }
    lines.join("\n")
}
```

- [ ] **Step 5: `main.rs` を `Arc` へ更新する**

`crates/polaris-cli/src/main.rs` の既存の `let provider: Box<dyn polaris_provider::Provider> = ...;` を `let provider: Arc<dyn polaris_provider::Provider> = ...;` に変える（構築箇所の `Box::new(...)` を `Arc::new(...)` へ置き換えるだけ、分岐条件自体は変えない）。既存の `AuditLog::open(&audit_path)?` を `Arc<tokio::sync::Mutex<AuditLog>>::new(tokio::sync::Mutex::new(AuditLog::open(&audit_path)?))` に変える。`agent::run(...)` の呼び出しへ、`discovered_agents.agent_types`・`provider.clone()`・`shared_audit.clone()` を追加で渡す。既存の `provider.as_ref()` はそのまま（`Arc<dyn T>` も `.as_ref() -> &dyn T` を持つため無修正で動く)。

Run: `cargo build --workspace 2>&1 | head -150`

Expected: 型不一致のコンパイルエラーが機械的に出るので、Step 4/5 の変更漏れを1つずつ潰す。特に `&mut audit`(ルート用)と `Arc<Mutex<AuditLog>>`(spawn用) が同じ `AuditLog` を指す実体として二重に存在しないよう、`main.rs` では `Arc<Mutex<AuditLog>>` を1つだけ作り、ルート用の `&mut AuditLog` はそのロックガードから得る（`let mut audit_guard = shared_audit.lock().await;` を `run()` 呼び出しの直前で行い、`&mut *audit_guard` を渡す)。

- [ ] **Step 6: `file-inspector` 実例を作る**

`agents/file-inspector/SKILL.md`

```markdown
---
name: file-inspector
description: 単一ファイルを読み取り専用で棚卸しし、責務、入出力、対応するテストを返す。
allowed-tools: read
metadata:
  polaris-access: read
  polaris-tier: low
  polaris-wall-seconds: "360"
  polaris-max-turns: "12"
  polaris-continuation: "denied"
  polaris-output: references/result.schema.json
---

あなたは単一ファイルを棚卸しする subagent である。与えられたパスを
`read` で読み、次の JSON だけを出力として返す。他のテキストを含めない。

- `path`: 調査したファイルのパス
- `responsibility`: このファイルの責務を1〜2文で
- `test_file`: 対応するテストファイルのパス（見つからなければ null）
```

`agents/file-inspector/references/result.schema.json`

```json
{
  "type": "object",
  "required": ["path", "responsibility"],
  "properties": {
    "path": { "type": "string" },
    "responsibility": { "type": "string" },
    "test_file": { "type": ["string", "null"] }
  }
}
```

- [ ] **Step 7: 単一タスクの結合テストを書く**

`crates/polaris-core/src/spawn.rs` のテストモジュール（新規）に、モックプロバイダで「`read` を1回呼び、`file-inspector` のスキーマに合う JSON を最終応答として返す」応答列を用意し、`run_one` が `TaskOutcome::Ok(...)` を返すことを確認するテストと、モックプロバイダがスキーマに合わない JSON を2回連続で返した場合に `TaskOutcome::Failed(...)` を返す(1回だけ再試行した)ことを確認するテストを書く。既存の `agent.rs` のモックプロバイダ構築パターン(Task 8 で確認済み)をそのまま使う。

Run: `cargo test -p polaris-core spawn::`

Expected: PASS。

- [ ] **Step 8: 全体検証とコミット**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

```bash
git add crates/polaris-tools/src/lib.rs crates/polaris-core/src/spawn.rs crates/polaris-core/src/agent.rs crates/polaris-cli/src/main.rs agents/
git commit -m "feat: spawn tool — single-subagent happy path

file-inspector を実例として、spawn が1件のsubagentタスクを実行し、
結果スキーマを検証し、不一致なら1回だけ再試行する経路を通す。
並列実行と書込先衝突検査はTask 10で追加する。

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 10: 並列実行・書込先衝突検査

**Files:**
- Modify: `crates/polaris-core/src/spawn.rs`
- Modify: `crates/polaris-core/src/config.rs`（並列度の設定キー）

**Interfaces:**
- Consumes: Task 9 の `run_one`
- Produces: `run_wave` を並列化し、同一波内の書込先重複を実行前に検出して波全体を拒否する

- [ ] **Step 1: 失敗するテストを書く（書込先の重複は1件も起動せず拒否する）**

`crates/polaris-core/src/spawn.rs` のテストモジュールに追加する。2件の read-write タスクが同じ `write_root` を宣言したケースで、`run_wave` が「1件も実行せずに」拒否メッセージだけを返すことを、`RecordingApprover` 相当のカウンタか、モックプロバイダの呼び出し回数(0回であること)で確認する。

```rust
#[tokio::test]
async fn overlapping_write_roots_reject_the_whole_wave_without_running_any_task() {
    let dir = tempfile::tempdir().unwrap();
    let call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let provider = counting_mock_provider(call_count.clone());
    let agent_types = vec![readwrite_fixture_agent_type()];
    let tasks = vec![
        SpawnTask {
            agent_type: "rw-fixture".to_string(),
            task: "a".to_string(),
            write_root: Some(dir.path().display().to_string()),
        },
        SpawnTask {
            agent_type: "rw-fixture".to_string(),
            task: "b".to_string(),
            write_root: Some(dir.path().display().to_string()),
        },
    ];
    let base_sandbox = polaris_sandbox::SandboxPolicy::new(
        polaris_sandbox::SandboxMode::WorkspaceWrite,
        &[dir.path().to_path_buf()],
    )
    .unwrap();
    let audit = shared_test_audit();
    let out = run_wave(
        tasks,
        &agent_types,
        std::sync::Arc::new(provider),
        audit,
        &base_sandbox,
        std::path::Path::new("/bin/true"),
    )
    .await;
    assert_eq!(call_count.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(out.contains("overlapping write_root"), "{out}");
}
```

`counting_mock_provider`/`readwrite_fixture_agent_type`/`shared_test_audit` は同じテストモジュール内のヘルパとして Step 2 の実装後に揃える（`AgentType` を直接構築するフィクスチャ、モック `Provider` の呼び出し回数を数える薄いラッパ、`Arc<Mutex<AuditLog>>` を一時ファイルで作るヘルパ）。

Run: `cargo test -p polaris-core overlapping_write_roots_reject_the_whole_wave_without_running_any_task`

Expected: FAIL — 現在の `run_wave` は衝突検査をしていない。

- [ ] **Step 2: 衝突検査と並列化を実装する**

`crates/polaris-core/src/spawn.rs` の `run_wave` を置き換える。

```rust
use tokio::sync::Semaphore;

const DEFAULT_CONCURRENCY: usize = 8;
const DEFAULT_WRITE_CONCURRENCY: usize = 4;

pub async fn run_wave(
    tasks: Vec<SpawnTask>,
    agent_types: &[AgentType],
    provider: Arc<dyn Provider>,
    audit: Arc<Mutex<AuditLog>>,
    base_sandbox: &SandboxPolicy,
    helper: &Path,
) -> String {
    if let Err(msg) = check_no_write_root_overlap(&tasks) {
        return msg;
    }

    let total_permits = Arc::new(Semaphore::new(DEFAULT_CONCURRENCY));
    let write_permits = Arc::new(Semaphore::new(DEFAULT_WRITE_CONCURRENCY));

    let mut handles = Vec::with_capacity(tasks.len());
    for task in tasks {
        let agent_types = agent_types.to_vec();
        let provider = provider.clone();
        let audit = audit.clone();
        let base_sandbox = base_sandbox.clone();
        let helper = helper.to_path_buf();
        let total_permits = total_permits.clone();
        let write_permits = write_permits.clone();
        let needs_write = task.write_root.is_some();

        handles.push(tokio::spawn(async move {
            let _total = total_permits.acquire().await.expect("semaphore closed");
            let _write = if needs_write {
                Some(write_permits.acquire().await.expect("semaphore closed"))
            } else {
                None
            };
            let outcome = run_one(&task, &agent_types, provider, audit, &base_sandbox, &helper).await;
            (task.agent_type, outcome)
        }));
    }

    let mut lines = Vec::with_capacity(handles.len());
    for h in handles {
        match h.await {
            Ok((agent_type, TaskOutcome::Ok(text))) => lines.push(format!("{agent_type}: {text}")),
            Ok((agent_type, TaskOutcome::Failed(msg))) => {
                lines.push(format!("{agent_type}: FAILED — {msg}"))
            }
            Err(e) => lines.push(format!("FAILED — subagent task panicked: {e}")),
        }
    }
    lines.join("\n")
}

/// 同一波内で `write_root` が重なるタスクがあれば、1件も起動せずに
/// 波全体を拒否する。部分的に実行された状態を残さないため。
fn check_no_write_root_overlap(tasks: &[SpawnTask]) -> Result<(), String> {
    let mut seen: Vec<&str> = Vec::new();
    for t in tasks {
        let Some(root) = t.write_root.as_deref() else {
            continue;
        };
        if seen.contains(&root) {
            return Err(format!(
                "spawn rejected: overlapping write_root {root:?} across tasks in the same wave"
            ));
        }
        seen.push(root);
    }
    Ok(())
}
```

`AgentType`(`Clone`) — `polaris_skills::AgentType` に `#[derive(Debug, Clone, PartialEq, Eq)]` が既に付いている(Task 2)ため、`agent_types.to_vec()` はそのまま動く。`tokio::spawn` が要求する `'static` は、この関数内で `task`/`agent_types`/`provider`/`audit`/`base_sandbox`/`helper` をすべて所有権ごと `async move` ブロックへ移すことで満たす——借用は一切持ち越さない。

- [ ] **Step 3: 衝突検査以外の並列実行のテストを書く**

2件の read-only タスク(`write_root` 無し、衝突しない)が両方成功し、`call_count` が2であることを確認するテストを追加する。

```rust
#[tokio::test]
async fn two_non_overlapping_tasks_both_run() {
    let call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let provider = counting_mock_provider_returning_valid_output(call_count.clone());
    let agent_types = vec![readonly_fixture_agent_type()];
    let tasks = vec![
        SpawnTask { agent_type: "ro-fixture".to_string(), task: "a".to_string(), write_root: None },
        SpawnTask { agent_type: "ro-fixture".to_string(), task: "b".to_string(), write_root: None },
    ];
    let base_sandbox = polaris_sandbox::SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[]).unwrap();
    let audit = shared_test_audit();
    let out = run_wave(tasks, &agent_types, std::sync::Arc::new(provider), audit, &base_sandbox, std::path::Path::new("/bin/true")).await;
    assert_eq!(call_count.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert!(out.contains("ro-fixture"), "{out}");
}
```

Run: `cargo test -p polaris-core spawn::`

Expected: PASS。

- [ ] **Step 4: 並列度を設定可能にする**

`crates/polaris-core/src/config.rs` の `Config` へ追加する。

```rust
pub struct Config {
    pub skills_paths: Vec<PathBuf>,
    pub agents_paths: Vec<PathBuf>,
    pub spawn_concurrency: usize,
    pub spawn_write_concurrency: usize,
}
```

既定値はそれぞれ `DEFAULT_CONCURRENCY`(8)・`DEFAULT_WRITE_CONCURRENCY`(4)。TOML キーは `[spawn] concurrency = 8` / `[spawn] write_concurrency = 4`。`run_wave` のシグネチャへ `concurrency: usize, write_concurrency: usize` を追加し、`Semaphore::new(DEFAULT_CONCURRENCY)` を `Semaphore::new(concurrency)` に置き換える。呼び出し元(`dispatch`)は `config` 由来の値を渡す——`config` 自体を `dispatch`/`ToolContext` へ渡す配線は、既存の `config.skills_paths`/`config.agents_paths` が discovery 時点だけで使われ `dispatch` まで届いていないのと同様、`main.rs` で `run()`/`run_loop()` 呼び出し時にこの2値だけを個別の引数として渡す(`Config` 全体は渡さない)。

- [ ] **Step 5: 全体検証とコミット**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

```bash
git add crates/polaris-core/src/spawn.rs crates/polaris-core/src/config.rs crates/polaris-core/src/agent.rs crates/polaris-cli/src/main.rs
git commit -m "feat: spawn — parallel execution and write-root collision detection

同一波内でwrite_rootが重なれば1件も起動せず波全体を拒否する。並列度は
tokio::sync::Semaphoreで既定8(書込4)、設定で変更可能にした。

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 11: 予算テスト — `spawn` を含めた6ツールの実測

**Files:**
- Modify: `crates/polaris-core/src/budget.rs`

**Interfaces:**
- Consumes: 既存の `always_on_tokens`/`assemble_always_on`/`BUDGET_LIMIT`/`MAX_TOOLS`
- Produces: 新規テスト1本のみ。既存のテスト・関数は変更しない

- [ ] **Step 1: 失敗するテストを書く**

`crates/polaris-core/src/budget.rs` の既存テストモジュールに追加する。既存の `always_on_context_stays_within_budget`(下限)・`absurdly_long_cwd_cannot_push_the_assembled_system_over_budget`(真の同時最大)と同じ実測手法で、`spawn` を含めた6ツール構成を測る。

```rust
#[test]
fn spawn_as_the_sixth_tool_still_stays_within_budget() {
    let tools = polaris_tools::all_specs();
    assert_eq!(tools.len(), MAX_TOOLS, "spawn should be exactly the 6th tool");
    let floor = always_on_tokens("system prompt placeholder", &tools);
    assert!(
        floor <= BUDGET_LIMIT,
        "6-tool floor {floor} exceeds BUDGET_LIMIT {BUDGET_LIMIT}"
    );
}
```

`always_on_tokens`/`BUDGET_LIMIT`/`MAX_TOOLS` の正確な呼び出し方(引数の型・並び)は既存の同ファイル内の他テストの呼び出し例に厳密に合わせる——このテストが唯一違うのは「測る対象の tools が5本ではなく `all_specs()` の全件(6本)である」点だけ。

Run: `cargo test -p polaris-core spawn_as_the_sixth_tool_still_stays_within_budget`

Expected: `all_specs()` が既に Task 9 で6本になっているため、実装なしでいきなり PASS するはずである——この Task の目的は「実測してテストに固定する」ことそのものであり、実装を新たに書くタスクではない。もし FAIL するなら実際の6ツール合計が 990 を超えている(仕様の見積 798 と食い違う)ということであり、既存ツールのスキーマ文言を見直す前に、まず `spawn_spec()` の description/parameters が仕様の再現実測時より冗長になっていないかを確認する。

- [ ] **Step 2: 常時コンテキストへの実質的な影響が無いことを確認する既存テストへの追記**

既存の `near_universal_skills_do_not_move_the_always_on_total` に相当するテストがあれば、その隣に「`agent_types` の件数を変えても `AlwaysOn` の合計トークン数は変わらない」ことを確認するテストを1本追加する(`assemble_always_on` は `skills: &[Skill]` だけを受け取り `agent_types` を受け取らないため、このテストは「`spawn` の常時コンテキストコストは agent_types の件数に依存しない」という設計上の主張そのものを固定する)。

```rust
#[test]
fn spawn_cost_does_not_depend_on_how_many_agent_types_are_discovered() {
    let with_zero = always_on_tokens("system prompt placeholder", &polaris_tools::all_specs());
    // spawn のスキーマは type/task/write_root という固定の形であり、
    // 実際に discover された agent_types の件数を一切引数に取らない
    // ため、比較対象を用意するまでもなく同じ呼び出しが同じ値を返す
    // ことそのものが不変条件である。
    let with_more = always_on_tokens("system prompt placeholder", &polaris_tools::all_specs());
    assert_eq!(with_zero, with_more);
}
```

- [ ] **Step 3: 全体検証とコミット**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

```bash
git add crates/polaris-core/src/budget.rs
git commit -m "test(polaris-core): pin spawn's real budget cost as the 6th tool

設計仕様書に記録した実測798トークンを、常設テストとして固定した。

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 12: 受け入れ基準の直接検証

**Files:**
- Modify: `crates/polaris-core/src/spawn.rs`
- Modify: `crates/polaris-core/src/agent.rs`

**Interfaces:** 新しい公開APIは無い。既存機能に対するテストのみ追加する。

このタスクは受け入れ基準のうち、実装だけで直接検証できる4項目を対象にする。基準5・6(固定タスク集での成功率・ターン数を Codex CLI と比較する)は実測が要る評価作業であり、この実装計画には含めない——`usage-measurement-clean-revert` の手法で `spawn` 実装後に別途行う。

- [ ] **Step 1: 基準2 — 呼び出し記述1件あたり100バイト以下**

`crates/polaris-core/src/spawn.rs` のテストモジュールに追加する。

```rust
#[test]
fn one_task_call_description_stays_under_100_bytes() {
    let task = serde_json::json!({
        "type": "file-inspector",
        "task": "crates/polaris-core/src/agent.rs"
    });
    let bytes = serde_json::to_string(&task).unwrap().len();
    assert!(bytes <= 100, "task call is {bytes} bytes: {task}");
}
```

Run: `cargo test -p polaris-core one_task_call_description_stays_under_100_bytes`

Expected: PASS(仕様の見積「1タスクあたり約60バイト」の実測固定)。長いパスを渡すタスクでは超過しうるため、このテストは「典型的な呼び出しが収まる」ことの固定であり、あらゆる入力での上限保証ではない——上限そのものを強制する実装は本計画のスコープに無い。

- [ ] **Step 2: 基準3 — subagent 向けサンドボックス方針も、実サンドボックスで拒否される**

`crates/polaris-core/src/spawn.rs` のテストモジュールに追加する。Task 9/10 の `resolve_subagent_sandbox` が組み立てた `SandboxPolicy` が、モックではなく実際の `run_confined` を通して拒否を発生させることを確認する——`polaris-sandbox` の既存テスト `a_write_outside_the_root_is_denied_by_the_real_sandbox` と同じ形。

```rust
#[test]
fn a_readwrite_subagent_sandbox_denies_writes_outside_its_declared_root_via_the_real_sandbox() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let agent = readwrite_fixture_agent_type();
    let task = SpawnTask {
        agent_type: agent.name.clone(),
        task: "x".to_string(),
        write_root: Some(root.path().display().to_string()),
    };
    let base_sandbox = polaris_sandbox::SandboxPolicy::new(
        polaris_sandbox::SandboxMode::WorkspaceWrite,
        &[root.path().to_path_buf()],
    )
    .unwrap();
    let sandbox = resolve_subagent_sandbox(&agent, &task, &base_sandbox).unwrap();

    let target = outside.path().canonicalize().unwrap().join("nope.txt");
    let out = polaris_sandbox::run_confined(
        &sandbox,
        std::path::Path::new("/bin/sh"),
        &["-c".into(), format!("echo pwned > {}", target.display())],
        None,
    )
    .unwrap();
    assert_ne!(out.status, 0, "a write outside the subagent's root succeeded: {out:?}");
    assert!(!target.exists());
}
```

Run: `cargo test -p polaris-core a_readwrite_subagent_sandbox_denies_writes_outside_its_declared_root_via_the_real_sandbox`

Expected: PASS。

- [ ] **Step 3: 基準4 — ルータの有無でキャッシュ接頭辞が一致する（`spawn` 追加でも崩れない）**

既存の budget.rs にキャッシュ接頭辞の不変条件テストがあれば(`skill` 追加時のM3bで同種のテストが既にあるはず)、その隣に「`spawn` を含めても `AlwaysOn` の先頭(システムプロンプト＋ツール定義の並び)が `agent_types` の中身によって変化しない」ことを確認するテストを追加する。`spawn_spec()` は静的な `ToolSpec`(定数)であり、`all_specs()` の呼び出しごとに同じ `Vec` を返すため、この不変条件は構造的に成り立つ——テストはそれを固定するだけで、新たな実装は要らない。

```rust
#[test]
fn spawn_spec_is_identical_across_calls_regardless_of_discovered_agent_types() {
    let a = polaris_tools::all_specs();
    let b = polaris_tools::all_specs();
    assert_eq!(
        serde_json::to_string(&a).unwrap(),
        serde_json::to_string(&b).unwrap()
    );
}
```

- [ ] **Step 4: 波は1件の失敗で止まらないことの確認**

`crates/polaris-core/src/spawn.rs` のテストモジュールに追加する。2件のタスクのうち1件が未知の型名(discovery に存在しない)で、もう1件が正しい型のケースで、`run_wave` が両方の結果を含み(片方は `FAILED`、もう片方は成功)、かつ成功した方の subagent が実際に実行された(呼び出しカウンタで確認)ことを検証する。

```rust
#[tokio::test]
async fn one_unknown_type_does_not_stop_the_other_task_in_the_wave() {
    let call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let provider = counting_mock_provider_returning_valid_output(call_count.clone());
    let agent_types = vec![readonly_fixture_agent_type()];
    let tasks = vec![
        SpawnTask { agent_type: "does-not-exist".to_string(), task: "a".to_string(), write_root: None },
        SpawnTask { agent_type: "ro-fixture".to_string(), task: "b".to_string(), write_root: None },
    ];
    let base_sandbox = polaris_sandbox::SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[]).unwrap();
    let audit = shared_test_audit();
    let out = run_wave(tasks, &agent_types, std::sync::Arc::new(provider), audit, &base_sandbox, std::path::Path::new("/bin/true")).await;
    assert_eq!(call_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(out.contains("FAILED"), "{out}");
    assert!(out.contains("ro-fixture"), "{out}");
}
```

Run: `cargo test -p polaris-core one_unknown_type_does_not_stop_the_other_task_in_the_wave`

Expected: PASS。

- [ ] **Step 5: 全体検証とコミット**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

```bash
git add crates/polaris-core/src/spawn.rs crates/polaris-core/src/agent.rs
git commit -m "test: pin M4 acceptance criteria 2/3/4 and wave failure isolation

受け入れ基準5・6(Codex CLIとの成功率・ターン数比較)は実測が要る評価
作業であり、この計画には含めない。usage-measurement-clean-revertの
手法でspawn実装後に別途行う。

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## 自己レビュー記録

- **spec 網羅性:** subagent 契約(型定義・呼び出し・フィールドの行き先) → Task 2/3/9。オーケストレーション(波・深さ・並列度・階層とモデル・結果) → Task 9/10。継続波は明示的にスコープ外。サンドボックスと承認(実効権限の積、書込先の宣言と強制の同一性、衝突時の全体拒否) → Task 9/10/12。エラー処理(プロバイダ障害はスコープ外と明記済み、停止条件、クラッシュはタスクパニックとして Task 10 で捕捉) → Task 7/9/10。監査(全ツール呼び出し対象、screen 経由) → Task 4。テスト戦略・受け入れ基準 → Task 11/12(基準5・6は評価作業として計画外に明記)。
- **プレースホルダ走査:** 「エラー処理を足す」式の曖昧な指示は無い——Task 5 の「コンパイラの型エラーを追って直す」だけは網羅列挙の代わりにコンパイラを oracle として使う設計であり、これは意図的な選択として本文中に理由を書いた。
- **型の一貫性:** `SpawnTask`(Task 9 で定義)のフィールド名 `agent_type`/`task`/`write_root` は Task 10・12 まで一貫。`AgentType`/`AgentAccess`(Task 2)は Task 3 の `Named` 実装、Task 9 の `resolve_subagent_sandbox`/discovery まで一貫。`Named` トレイト(Task 1)は `Skill`(Task 1)・`AgentType`(Task 3)の両方に実装され、`polaris_tools::skill::lookup`(Task 9 の未知型エラー)まで一貫して使われる。
- **見つかったが計画に含めなかったもの:** `polaris-provider` のフォールバック連鎖、`ollama` 等ローカルプロバイダ、継続波——いずれも仕様書側で既にスコープ外と明記済み(このセッションで修正済み)。
