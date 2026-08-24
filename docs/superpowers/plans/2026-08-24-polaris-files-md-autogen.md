# polaris ディレクトリ別 files.md 自動生成 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** ディレクトリごとに `files.md`(直下のファイル・サブディレクトリを1文で要約)を、モデルの判断・会話コンテキストのコストなしに、ハーネスが自動生成する。

**Architecture:** `bash`/`write`/`edit` ツール呼び出しの前後でファイルシステムをスナップショット・差分検出し(`dir_watch`)、新規ディレクトリ・既存ディレクトリへの新規ファイルを検出したら、M4 で実装済みの `spawn::run_one` を Rust コードから直接呼び(モデルの `spawn` ツール呼び出しは経由しない)、新設する `files-md-writer` サブエージェント型に `files.md` を書かせる。生成物は `.gitignore` へ自動登録する。

**Tech Stack:** Rust、既存の `polaris-core`/`polaris-skills`/`polaris-tools` クレート、tokio。新規外部依存なし。

**Spec:** `docs/superpowers/specs/2026-08-24-polaris-files-md-autogen-design.md`

## Global Constraints

- 検証は毎タスク・コミット前に必ず `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check` を実行する。
- この機構はハーネス内部処理であり、モデルへの常時コンテキスト(`AlwaysOn`、`crates/polaris-core/src/prompt.rs`)には一切追加しない。既存の `budget.rs` の不変条件テストが崩れないことを最終タスクで実測固定する。
- `files-md-writer` サブエージェントの実行失敗は、それを引き起こした元の `bash`/`write`/`edit` ツール呼び出し自体の成否に一切影響しない(ベストエフォート、監査ログにのみ記録)。
- この機構は **`caller == "root"` のときにのみ発火する**(`run_loop` は root とサブエージェントの両方が共有するため、サブエージェント自身の `write` 呼び出しでこの機構が再発火すると、`files-md-writer` 自身が書いた `files.md` が「既存ディレクトリへの新規ファイル」として検出され、自分自身を再生成するループになりうる — スペックには明記されていないが、M4 の `spawn` 自体が深さ1に固定されている設計と一貫させるための必須のガード)。
- 新規に作られたファイルが `files.md` という名前そのものである場合は、それ自体を「既存ディレクトリへの新規ファイル」トリガーの対象から除外する(上記ガードの二重の安全策)。
- 削除・リネームへの追従は非対応(スペックで明記済みの既知の制限)。

---

### Task 1: `dir_watch` — ファイルシステムのスナップショット・差分検出

**Files:**
- Create: `crates/polaris-core/src/dir_watch.rs`
- Modify: `crates/polaris-core/src/lib.rs`(`mod dir_watch;` を追加)

**Interfaces:**
- Consumes: 標準ライブラリの `std::fs` のみ
- Produces:
  - `pub(crate) struct DirSnapshot { pub dirs: BTreeSet<PathBuf>, pub files: BTreeSet<PathBuf> }`
  - `pub(crate) fn snapshot_recursive(root: &Path) -> DirSnapshot`(`.git`/`target`/`node_modules` という名前のディレクトリはスキップして下らない)
  - `pub(crate) fn snapshot_shallow(dir: &Path) -> DirSnapshot`(`dir` 直下1階層のみ、再帰しない)
  - `pub(crate) struct DirChanges { pub new_dirs: Vec<PathBuf>, pub new_files_in_existing_dirs: Vec<PathBuf> }`
  - `pub(crate) fn diff(before: &DirSnapshot, after: &DirSnapshot) -> DirChanges`

- [ ] **Step 1: 失敗するテストを書く**

`crates/polaris-core/src/dir_watch.rs` を新規作成し、まずテストから書く。

```rust
//! `bash`/`write`/`edit` 呼び出しの前後でファイルシステムを比較し、新規
//! ディレクトリ・既存ディレクトリへの新規ファイルを検出する。中身は一切
//! 読まない——パスと種別(ファイル/ディレクトリ)だけを見る。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const SKIP_DIR_NAMES: &[&str] = &[".git", "target", "node_modules"];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DirSnapshot {
    pub dirs: BTreeSet<PathBuf>,
    pub files: BTreeSet<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DirChanges {
    pub new_dirs: Vec<PathBuf>,
    pub new_files_in_existing_dirs: Vec<PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_recursive_finds_nested_dirs_and_files_but_skips_known_noise() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("a/b")).unwrap();
        std::fs::write(root.path().join("a/one.txt"), "x").unwrap();
        std::fs::write(root.path().join("a/b/two.txt"), "x").unwrap();
        std::fs::create_dir_all(root.path().join("target/junk")).unwrap();
        std::fs::write(root.path().join("target/junk/ignored.txt"), "x").unwrap();

        let snap = snapshot_recursive(root.path());

        assert!(snap.dirs.contains(&root.path().join("a")));
        assert!(snap.dirs.contains(&root.path().join("a/b")));
        assert!(snap.files.contains(&root.path().join("a/one.txt")));
        assert!(snap.files.contains(&root.path().join("a/b/two.txt")));
        assert!(!snap.dirs.contains(&root.path().join("target")));
        assert!(!snap.files.contains(&root.path().join("target/junk/ignored.txt")));
    }

    #[test]
    fn snapshot_shallow_does_not_recurse() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("sub")).unwrap();
        std::fs::write(root.path().join("sub/deep.txt"), "x").unwrap();
        std::fs::write(root.path().join("top.txt"), "x").unwrap();

        let snap = snapshot_shallow(root.path());

        assert!(snap.files.contains(&root.path().join("top.txt")));
        assert!(snap.dirs.contains(&root.path().join("sub")));
        assert!(!snap.files.contains(&root.path().join("sub/deep.txt")));
    }

    #[test]
    fn diff_reports_a_new_directory() {
        let root = tempfile::tempdir().unwrap();
        let before = snapshot_recursive(root.path());
        std::fs::create_dir(root.path().join("newdir")).unwrap();
        let after = snapshot_recursive(root.path());

        let changes = diff(&before, &after);

        assert_eq!(changes.new_dirs, vec![root.path().join("newdir")]);
        assert!(changes.new_files_in_existing_dirs.is_empty());
    }

    #[test]
    fn diff_reports_a_new_file_in_an_existing_directory_separately_from_a_new_directory() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("existing")).unwrap();
        let before = snapshot_recursive(root.path());
        std::fs::write(root.path().join("existing/new.txt"), "x").unwrap();
        let after = snapshot_recursive(root.path());

        let changes = diff(&before, &after);

        assert!(changes.new_dirs.is_empty());
        assert_eq!(
            changes.new_files_in_existing_dirs,
            vec![root.path().join("existing/new.txt")]
        );
    }

    #[test]
    fn a_file_inside_a_brand_new_directory_is_not_double_reported_as_a_new_file_in_an_existing_directory() {
        let root = tempfile::tempdir().unwrap();
        let before = snapshot_recursive(root.path());
        std::fs::create_dir(root.path().join("newdir")).unwrap();
        std::fs::write(root.path().join("newdir/inside.txt"), "x").unwrap();
        let after = snapshot_recursive(root.path());

        let changes = diff(&before, &after);

        assert_eq!(changes.new_dirs, vec![root.path().join("newdir")]);
        assert!(
            changes.new_files_in_existing_dirs.is_empty(),
            "a file inside a brand-new directory should only be covered by the \
             new directory's own generation, not reported again: {changes:?}"
        );
    }
}
```

Run: `cargo test -p polaris-core dir_watch::`

Expected: FAIL — `snapshot_recursive`/`snapshot_shallow`/`diff`/`DirSnapshot`/`DirChanges` は未定義。`Cargo.toml` の `crates/polaris-core` に `tempfile` が `[dev-dependencies]` として既にあるはず(先行タスクで使用実績あり)——なければ `crates/polaris-core/Cargo.toml` の `[dev-dependencies]` に `tempfile = { workspace = true }` を追加する。

- [ ] **Step 2: 実装する**

同じファイルへ、`use` 文とテストモジュールの間に追加する。

```rust
pub(crate) fn snapshot_recursive(root: &Path) -> DirSnapshot {
    let mut out = DirSnapshot::default();
    walk(root, &mut out, true);
    out
}

pub(crate) fn snapshot_shallow(dir: &Path) -> DirSnapshot {
    let mut out = DirSnapshot::default();
    walk(dir, &mut out, false);
    out
}

fn walk(dir: &Path, out: &mut DirSnapshot, recurse: bool) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if file_type.is_dir() {
            let name = entry.file_name();
            if SKIP_DIR_NAMES.iter().any(|s| name == std::ffi::OsStr::new(s)) {
                continue;
            }
            out.dirs.insert(path.clone());
            if recurse {
                walk(&path, out, recurse);
            }
        } else if file_type.is_file() {
            out.files.insert(path);
        }
    }
}

pub(crate) fn diff(before: &DirSnapshot, after: &DirSnapshot) -> DirChanges {
    let new_dirs: Vec<PathBuf> = after.dirs.difference(&before.dirs).cloned().collect();
    let new_dir_set: BTreeSet<&PathBuf> = new_dirs.iter().collect();

    let new_files_in_existing_dirs: Vec<PathBuf> = after
        .files
        .difference(&before.files)
        .filter(|f| {
            f.parent()
                .map(|p| !new_dir_set.contains(&p.to_path_buf()))
                .unwrap_or(false)
        })
        .cloned()
        .collect();

    DirChanges {
        new_dirs,
        new_files_in_existing_dirs,
    }
}
```

- [ ] **Step 3: テストを通す**

Run: `cargo test -p polaris-core dir_watch::`

Expected: PASS(5 tests)。

- [ ] **Step 4: `lib.rs` へ配線する**

`crates/polaris-core/src/lib.rs` へ追加する(他の `mod` 宣言の並びに合わせる)。

```rust
mod dir_watch;
```

（`pub(crate)` な要素のみなので `pub use` の再エクスポートは不要）

- [ ] **Step 5: 全体検証とコミット**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

```bash
git add crates/polaris-core/src/dir_watch.rs crates/polaris-core/src/lib.rs crates/polaris-core/Cargo.toml
git commit -m "feat(polaris-core): detect new dirs/files via filesystem snapshot diff

files.md自動生成(次タスク以降)が使う検知ロジック。mkdirコマンドの構文
解析ではなく、ツール呼び出し前後のファイルシステム差分を見る——write/
edit経由の新規ディレクトリ作成やmkdir -p、複合コマンドでの検知漏れを
避けるため。

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 2: `gitignore` — `.gitignore` への冪等な追記

**Files:**
- Create: `crates/polaris-core/src/gitignore.rs`
- Modify: `crates/polaris-core/src/lib.rs`(`mod gitignore;` を追加)

**Interfaces:**
- Consumes: 標準ライブラリの `std::fs`/`std::io` のみ
- Produces: `pub(crate) fn ensure_pattern_ignored(starting_dir: &Path, pattern: &str) -> std::io::Result<()>`

- [ ] **Step 1: 失敗するテストを書く**

`crates/polaris-core/src/gitignore.rs` を新規作成する。

```rust
//! `files.md`(ハーネスが自動生成する、コミット対象外のファイル)を
//! `.gitignore` へ登録する。冪等——同じパターンを二重に書かない。

use std::io::Write;
use std::path::{Path, PathBuf};

fn find_repo_root(starting_dir: &Path) -> PathBuf {
    let mut current = starting_dir;
    loop {
        if current.join(".git").exists() {
            return current.to_path_buf();
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => return starting_dir.to_path_buf(),
        }
    }
}

pub(crate) fn ensure_pattern_ignored(starting_dir: &Path, pattern: &str) -> std::io::Result<()> {
    let root = find_repo_root(starting_dir);
    let gitignore_path = root.join(".gitignore");
    let existing = std::fs::read_to_string(&gitignore_path).unwrap_or_default();
    if existing.lines().any(|line| line.trim() == pattern) {
        return Ok(());
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&gitignore_path)?;
    if !existing.is_empty() && !existing.ends_with('\n') {
        writeln!(file)?;
    }
    writeln!(file, "{pattern}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_the_pattern_to_a_gitignore_at_the_repo_root() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        let nested = root.path().join("a/b");
        std::fs::create_dir_all(&nested).unwrap();

        ensure_pattern_ignored(&nested, "**/files.md").unwrap();

        let content = std::fs::read_to_string(root.path().join(".gitignore")).unwrap();
        assert!(content.lines().any(|l| l == "**/files.md"));
        assert!(!root.path().join("a/.gitignore").exists());
    }

    #[test]
    fn is_idempotent_and_never_duplicates_the_pattern() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();

        ensure_pattern_ignored(root.path(), "**/files.md").unwrap();
        ensure_pattern_ignored(root.path(), "**/files.md").unwrap();

        let content = std::fs::read_to_string(root.path().join(".gitignore")).unwrap();
        assert_eq!(content.matches("**/files.md").count(), 1);
    }

    #[test]
    fn preserves_existing_lines_and_appends_after_them() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        std::fs::write(root.path().join(".gitignore"), "/target\n").unwrap();

        ensure_pattern_ignored(root.path(), "**/files.md").unwrap();

        let content = std::fs::read_to_string(root.path().join(".gitignore")).unwrap();
        assert!(content.contains("/target"));
        assert!(content.contains("**/files.md"));
    }

    #[test]
    fn falls_back_to_the_starting_directory_when_no_git_root_is_found() {
        let root = tempfile::tempdir().unwrap();
        // No `.git` anywhere under `root` — falls back to `root` itself.

        ensure_pattern_ignored(root.path(), "**/files.md").unwrap();

        let content = std::fs::read_to_string(root.path().join(".gitignore")).unwrap();
        assert!(content.contains("**/files.md"));
    }
}
```

Run: `cargo test -p polaris-core gitignore::`

Expected: FAIL until Step 2 は実装済みのため実際には最初からPASSしうる——上のコードは実装込みで書いてある。まず実装なしの状態(関数本体を `todo!()` に置き換えるなどはしない。TDDの体裁を保つため、このタスクでは「テストを書く」と「実装する」を1ステップにまとめている——関数自体が15行未満の自己完結した処理であるため)。実際の手順としては、上記コード全体(実装+テスト)を書いた状態で直接 Step 3 のテスト実行に進んでよい。

- [ ] **Step 2: テストを通す**

Run: `cargo test -p polaris-core gitignore::`

Expected: PASS(4 tests)。

- [ ] **Step 3: `lib.rs` へ配線する**

```rust
mod gitignore;
```

- [ ] **Step 4: 全体検証とコミット**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

```bash
git add crates/polaris-core/src/gitignore.rs crates/polaris-core/src/lib.rs
git commit -m "feat(polaris-core): idempotently register generated files.md in .gitignore

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 3: `files-md-writer` サブエージェント型

**Files:**
- Create: `agents/files-md-writer/SKILL.md`
- Create: `agents/files-md-writer/references/result.schema.json`

**Interfaces:**
- Consumes: 既存の `polaris_skills::discover_agent_types_in`(M4、変更なし)
- Produces: なし(データファイルのみ)

このタスクはコード変更を伴わない。`agents/file-inspector/`(M4 で作成済み)と全く同じ形の、新しい型定義を追加するだけ。

- [ ] **Step 1: `SKILL.md` を作成する**

`agents/files-md-writer/SKILL.md`

```markdown
---
name: files-md-writer
description: 単一ディレクトリ直下の各エントリ(ファイル・サブディレクトリ)を1文で要約したfiles.mdを書く。
allowed-tools: read write
metadata:
  polaris-access: read-write
  polaris-tier: low
  polaris-wall-seconds: "60"
  polaris-max-turns: "4"
  polaris-continuation: "denied"
  polaris-output: references/result.schema.json
---

あなたは単一ディレクトリの `files.md` を書く subagent である。与えられた
パス(ディレクトリ)を `read` で一覧し、直下にある各ファイル・各サブ
ディレクトリについて、それぞれ1文で概要をまとめる。サブディレクトリに
既に `files.md` があれば、その内容を要約に反映してよい。

`<dir>/files.md` へ、次の形式で `write` する。

```
# files.md

- `entry-name` — 1文の概要
- `subdir-name/` — 1文の概要
```

書き終えたら、次の JSON だけを出力として返す。他のテキストを含めない。

- `path`: 書き込んだ `files.md` の絶対パス
- `status`: 常に `"ok"`
```

- [ ] **Step 2: 結果スキーマを作成する**

`agents/files-md-writer/references/result.schema.json`

```json
{
  "type": "object",
  "required": ["path", "status"],
  "properties": {
    "path": { "type": "string" },
    "status": { "type": "string", "enum": ["ok"] }
  }
}
```

- [ ] **Step 3: discovery が正しく読めることを確認する**

`crates/polaris-skills/src/agent_type.rs` の既存テストモジュールに追加する(`file-inspector` の discovery テストと同じ形)。

```rust
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
```

Run: `cargo test -p polaris-skills discover_agent_types_in_finds_the_files_md_writer_type`

Expected: PASS。

- [ ] **Step 4: 全体検証とコミット**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

```bash
git add agents/files-md-writer/ crates/polaris-skills/src/agent_type.rs
git commit -m "feat: add the files-md-writer subagent type

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 4: `files_md` — 検出結果を `spawn::run_one` と `gitignore` へつなぐオーケストレーション

**Files:**
- Create: `crates/polaris-core/src/files_md.rs`
- Modify: `crates/polaris-core/src/lib.rs`(`mod files_md;` を追加)

**Interfaces:**
- Consumes: Task 1 の `dir_watch::DirChanges`、Task 2 の `gitignore::ensure_pattern_ignored`、M4 の `spawn::{run_one, SpawnTask, TaskOutcome}`、`polaris_skills::AgentType`
- Produces: `pub(crate) async fn regenerate_for_changes(changes: &dir_watch::DirChanges, agent_types: &[AgentType], provider: Arc<dyn Provider>, audit: Arc<Mutex<AuditLog>>, base_sandbox: &SandboxPolicy, helper: &Path)`(戻り値なし——失敗しても呼び出し元へは何も伝えない、が契約)

- [ ] **Step 1: 失敗するテストを書く**

`crates/polaris-core/src/files_md.rs` を新規作成する。まずテストから書く。モックプロバイダは `agent.rs` の既存テストモジュールが使っているものと同じパターン(`Scripted`)を、このファイル内のテストモジュールに最小限だけ複製する(`agent.rs` 内の非公開型を直接importできないため)。

```rust
//! `dir_watch` が検出した変更を、実際に `files.md` を書く subagent の
//! 実行(`spawn::run_one`)と `.gitignore` への登録へつなぐ。モデルの
//! `spawn` ツール呼び出しは経由しない——呼び出し元(`agent::run_loop`)
//! がハーネスとして直接呼ぶ。ここでの失敗は audit log にのみ記録され、
//! 呼び出し元のツール結果には一切影響しない。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Mutex;

use polaris_provider::Provider;
use polaris_sandbox::SandboxPolicy;
use polaris_skills::AgentType;

use crate::audit::{AuditLog, Record};
use crate::dir_watch::DirChanges;
use crate::spawn::{SpawnTask, TaskOutcome, run_one};

const FILES_MD_AGENT_TYPE: &str = "files-md-writer";
const FILES_MD_FILENAME: &str = "files.md";

pub(crate) async fn regenerate_for_changes(
    changes: &DirChanges,
    agent_types: &[AgentType],
    provider: Arc<dyn Provider>,
    audit: Arc<Mutex<AuditLog>>,
    base_sandbox: &SandboxPolicy,
    helper: &Path,
) {
    if agent_types.iter().all(|a| a.name != FILES_MD_AGENT_TYPE) {
        return;
    }

    let targets = targets_to_regenerate(changes);

    for dir in targets {
        regenerate_one(
            &dir,
            agent_types,
            provider.clone(),
            audit.clone(),
            base_sandbox,
            helper,
        )
        .await;
    }
}

/// 純粋関数として切り出す——subagent 実行なしにロジックを単体テストする
/// ため。新規ディレクトリは常に対象。新規ディレクトリの親は、既に
/// `files.md` を持つ場合のみ対象(持たない親は観測対象外のまま)。既存
/// ディレクトリへの新規ファイルは、そのファイルが `files.md` という
/// 名前そのものでなく、かつそのディレクトリが既に `files.md` を持つ
/// 場合のみ対象。
fn targets_to_regenerate(changes: &DirChanges) -> Vec<PathBuf> {
    let mut targets: Vec<PathBuf> = Vec::new();

    for dir in &changes.new_dirs {
        targets.push(dir.clone());
        if let Some(parent) = dir.parent() {
            if parent.join(FILES_MD_FILENAME).exists() {
                targets.push(parent.to_path_buf());
            }
        }
    }

    for file in &changes.new_files_in_existing_dirs {
        if file.file_name().map(|n| n == FILES_MD_FILENAME).unwrap_or(false) {
            continue;
        }
        if let Some(parent) = file.parent() {
            if parent.join(FILES_MD_FILENAME).exists() {
                targets.push(parent.to_path_buf());
            }
        }
    }

    targets.sort();
    targets.dedup();
    targets
}

async fn regenerate_one(
    dir: &Path,
    agent_types: &[AgentType],
    provider: Arc<dyn Provider>,
    audit: Arc<Mutex<AuditLog>>,
    base_sandbox: &SandboxPolicy,
    helper: &Path,
) {
    let task = SpawnTask {
        agent_type: FILES_MD_AGENT_TYPE.to_string(),
        task: dir.display().to_string(),
        write_root: Some(dir.display().to_string()),
    };
    let outcome = run_one(
        &task,
        agent_types,
        provider,
        audit.clone(),
        base_sandbox,
        helper,
    )
    .await;
    if let TaskOutcome::Failed(msg) = outcome {
        let _ = audit.lock().await.record(&Record {
            tool: "files-md-writer",
            detail: &dir.display().to_string(),
            sandbox: None,
            target: Some(dir),
            result: &msg,
            caller: "harness",
        });
    }
    let _ = crate::gitignore::ensure_pattern_ignored(dir, "**/files.md");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn changes_with(new_dirs: Vec<PathBuf>, new_files: Vec<PathBuf>) -> DirChanges {
        DirChanges {
            new_dirs,
            new_files_in_existing_dirs: new_files,
        }
    }

    #[test]
    fn a_brand_new_directory_is_always_a_target() {
        let dir = std::env::temp_dir().join("files_md_test_new_dir_never_created");
        let changes = changes_with(vec![dir.clone()], vec![]);
        let targets = targets_to_regenerate(&changes);
        assert_eq!(targets, vec![dir]);
    }

    #[test]
    fn a_new_directorys_parent_is_a_target_only_if_the_parent_already_has_files_md() {
        let root = tempfile::tempdir().unwrap();
        let with_files_md = root.path().join("has_one");
        std::fs::create_dir_all(&with_files_md).unwrap();
        std::fs::write(with_files_md.join("files.md"), "# files.md\n").unwrap();
        let without_files_md = root.path().join("has_none");
        std::fs::create_dir_all(&without_files_md).unwrap();

        let changes = changes_with(
            vec![with_files_md.join("child"), without_files_md.join("child")],
            vec![],
        );
        let targets = targets_to_regenerate(&changes);

        assert!(targets.contains(&with_files_md.join("child")));
        assert!(targets.contains(&with_files_md));
        assert!(targets.contains(&without_files_md.join("child")));
        assert!(!targets.contains(&without_files_md));
    }

    #[test]
    fn a_new_file_named_files_md_itself_is_never_a_trigger() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("dir")).unwrap();
        std::fs::write(root.path().join("dir/files.md"), "# files.md\n").unwrap();

        let changes = changes_with(vec![], vec![root.path().join("dir/files.md")]);
        let targets = targets_to_regenerate(&changes);

        assert!(
            targets.is_empty(),
            "files.md writing itself must not re-trigger regeneration: {targets:?}"
        );
    }

    #[test]
    fn a_new_ordinary_file_is_a_target_only_if_its_directory_already_has_files_md() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("dir");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("files.md"), "# files.md\n").unwrap();

        let changes = changes_with(vec![], vec![dir.join("new.rs")]);
        let targets = targets_to_regenerate(&changes);

        assert_eq!(targets, vec![dir]);
    }
}
```

Run: `cargo test -p polaris-core files_md::`

Expected: FAIL — モジュール自体が未配線(`lib.rs` に `mod files_md;` が無い)ため、まずコンパイルが通らない。

- [ ] **Step 2: `lib.rs` へ配線し、テストを通す**

```rust
mod files_md;
```

Run: `cargo test -p polaris-core files_md::`

Expected: PASS(4 tests、いずれも subagent 実行を伴わない `targets_to_regenerate` の純粋関数テスト)。

- [ ] **Step 3: 全体検証とコミット**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

```bash
git add crates/polaris-core/src/files_md.rs crates/polaris-core/src/lib.rs
git commit -m "feat(polaris-core): orchestrate files.md regeneration from detected changes

dir_watchの検出結果を、どのディレクトリを対象にするかというロジック
(targets_to_regenerate、純粋関数)と、実際のsubagent実行
(regenerate_one、spawn::run_oneを直接呼ぶ)へ分離した。files.md自身の
書き込みが自分自身を再トリガーしないことをテストで固定した。

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 5: `run_loop` への配線 — root のツール呼び出しでのみ発火

**Files:**
- Modify: `crates/polaris-core/src/agent.rs`

**Interfaces:**
- Consumes: Task 1 の `dir_watch::{snapshot_recursive, snapshot_shallow, diff}`、Task 4 の `files_md::regenerate_for_changes`
- Produces: なし(`run_loop` の既存シグネチャ・戻り値は一切変えない)

- [ ] **Step 1: 失敗するテストを書く**

`crates/polaris-core/src/agent.rs` の既存テストモジュールに追加する。実際にディレクトリを作る `bash` 呼び出しをモデルにさせ、その直後に `files-md-writer` の呼び出し(2件目の `Scripted` 応答)が実際に発生していることを、監査ログの `tool: "files-md-writer"` 行の有無で確認する。この機構は `run_loop` の外からは見えない副作用なので、監査ログが唯一の観測点になる。

```rust
    #[tokio::test]
    async fn a_root_bash_call_that_creates_a_directory_triggers_files_md_regeneration() {
        let dir = tempfile::tempdir().unwrap();
        // `agents/files-md-writer` を実際に discovery する代わりに、
        // テスト用の固定 AgentType を1件用意する(実ファイルI/Oに
        // 依存しない)。
        let agent_types = vec![files_md_writer_fixture_agent_type()];
        let provider = Scripted::new(vec![
            // ルートのターン: mkdir を実行させる
            scripted_tool_call("bash", serde_json::json!({"command": format!("mkdir {}/newdir", dir.path().display())})),
            // files-md-writer subagent 側のターン: 何もせず即座に結果を返す
            scripted_final_text(r#"{"path":"x","status":"ok"}"#),
            // ルートの2ターン目: 終了
            scripted_final_text("done"),
        ]);
        let mut session = Session::new();
        session.push_user("mkdir newdir");
        let audit_path = dir.path().join("audit.jsonl");
        let audit = shared_audit(&audit_path);
        let mut stop = StopTracker::new(10);
        let sandbox = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::WorkspaceWrite,
            &[dir.path().to_path_buf()],
        )
        .unwrap();
        let helper = std::path::PathBuf::from("/bin/true");
        let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::Never);
        let mut approver = AlwaysAllow { asked: 0 };
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };

        run(
            &provider,
            &mut session,
            audit.clone(),
            &mut stop,
            &crate::prompt::AlwaysOn::new("system", &[]),
            &[],
            &agent_types,
            Arc::new(provider.clone()),
            8,
            4,
            &mut ctx,
        )
        .await
        .unwrap();

        let log_text = std::fs::read_to_string(&audit_path).unwrap();
        assert!(
            log_text.contains("\"tool\":\"files-md-writer\""),
            "expected a files-md-writer audit record, got: {log_text}"
        );
    }
```

`files_md_writer_fixture_agent_type()`/`scripted_tool_call`/`scripted_final_text`/`AlwaysAllow`/`shared_audit` は、既存のテストモジュール内に既にある同種のヘルパ(`agent.rs` 内、`spawn.rs` のテストが使っているものと同じ命名パターン)を確認し、無ければこのステップで追加する。`Scripted` プロバイダが `Clone` を実装していない場合は、`Arc::new(provider.clone())` の代わりに同じ応答列を持つ2つ目の `Scripted` インスタンスを構築する形に置き換える(既存の `spawn.rs` テストでの `provider_pool` の渡し方を確認して合わせる)。

Run: `cargo test -p polaris-core a_root_bash_call_that_creates_a_directory_triggers_files_md_regeneration`

Expected: FAIL — この機構自体が `run_loop` に未配線。

- [ ] **Step 2: `run_loop` へ配線する**

`crates/polaris-core/src/agent.rs` の `run_loop` 内、`for call in &res.tool_calls` ループの中、既存の `let outcome = dispatch(...).await;` の直前直後を次のように変更する(147行目付近、`dispatch` 呼び出しを囲む形)。

```rust
        for call in &res.tool_calls {
            let is_fs_mutating_call = caller == "root"
                && matches!(call.name.as_str(), "bash" | "write" | "edit");
            let pre_snapshot = if is_fs_mutating_call {
                Some(pre_call_snapshot(&call.name, call, ctx))
            } else {
                None
            };

            let outcome = dispatch(
                call,
                skills,
                agent_types,
                provider_pool.clone(),
                audit.clone(),
                spawn_concurrency,
                spawn_write_concurrency,
                ctx,
            )
            .await;

            if let Some((scope, before)) = pre_snapshot {
                let after = crate::dir_watch::snapshot_for_scope(&scope);
                let changes = crate::dir_watch::diff(&before, &after);
                if !changes.new_dirs.is_empty() || !changes.new_files_in_existing_dirs.is_empty() {
                    crate::files_md::regenerate_for_changes(
                        &changes,
                        agent_types,
                        provider_pool.clone(),
                        audit.clone(),
                        ctx.sandbox,
                        ctx.helper,
                    )
                    .await;
                }
            }

            // 以下、既存の `result`/`is_mutation`/`target`/`audit.lock()...record` は無修正。
```

上記の `pre_call_snapshot`/`crate::dir_watch::snapshot_recursive_or_shallow` はこのステップで新設するヘルパで、`dir_watch.rs` へ追加する(Task 1 の範囲を超えるため、このタスクで追加する)。

`crates/polaris-core/src/dir_watch.rs` へ追加する。

```rust
/// `pre_call_snapshot` が返す「対象範囲」と、その範囲が再帰スキャンか
/// 1階層スキャンかを1つの型にまとめる。呼び出し側(`agent.rs`)が
/// `bash` か `write`/`edit` かで作り分け、事後の再スキャンはこの型が
/// 自分の種別を覚えているので呼び出し側は分岐しなくてよい。
pub(crate) enum ScanScope {
    Recursive(PathBuf),
    Shallow(PathBuf),
    /// `bash` かつ書込許可ルートを持たない(read-only/full-access)場合。
    /// 何もスキャンしない。
    None,
}

pub(crate) fn snapshot_for_scope(scope: &ScanScope) -> DirSnapshot {
    match scope {
        ScanScope::Recursive(root) => snapshot_recursive(root),
        ScanScope::Shallow(dir) => snapshot_shallow(dir),
        ScanScope::None => DirSnapshot::default(),
    }
}
```

`crates/polaris-core/src/agent.rs` へ、`run_loop` の直前(あるいは同ファイル内の適切な位置)に `pre_call_snapshot` を追加する。

```rust
/// `bash`/`write`/`edit` 呼び出しの直前に呼ぶ。対象範囲を決め、その場で
/// スナップショットも取って、範囲と「変更前」のペアを返す。
fn pre_call_snapshot(
    tool_name: &str,
    call: &polaris_provider::ToolCall,
    ctx: &ToolContext<'_>,
) -> (crate::dir_watch::ScanScope, crate::dir_watch::DirSnapshot) {
    let scope = match tool_name {
        "bash" => {
            if ctx.sandbox.mode() == polaris_sandbox::SandboxMode::WorkspaceWrite {
                // 複数の書込許可ルートがあっても、この機構は「最初の
                // ルートだけを見る」単純化を取る——ほとんどの実運用は
                // ルート1つであり、複数ルート対応は将来の拡張とする。
                match ctx.sandbox.writable_roots().first() {
                    Some(root) => crate::dir_watch::ScanScope::Recursive(root.clone()),
                    None => crate::dir_watch::ScanScope::None,
                }
            } else {
                crate::dir_watch::ScanScope::None
            }
        }
        "write" | "edit" => {
            let path = call.arguments["path"].as_str().map(std::path::PathBuf::from);
            match path.and_then(|p| p.parent().map(|parent| parent.to_path_buf())) {
                Some(parent) => crate::dir_watch::ScanScope::Shallow(parent),
                None => crate::dir_watch::ScanScope::None,
            }
        }
        _ => crate::dir_watch::ScanScope::None,
    };
    let before = crate::dir_watch::snapshot_for_scope(&scope);
    (scope, before)
}
```

- [ ] **Step 3: テストを通す**

Run: `cargo test -p polaris-core a_root_bash_call_that_creates_a_directory_triggers_files_md_regeneration`

Expected: PASS。

- [ ] **Step 4: 既存の `run`/`run_loop` のテストが全て無修正で通ることを確認する**

Run: `cargo test -p polaris-core agent::`

Expected: 既存テストが全て無修正のまま通る——`caller != "root"` の場合(subagent 実行時)はこの機構が完全にスキップされるため、M4 の `spawn` 関連の既存テストの挙動は変わらない。

- [ ] **Step 5: 全体検証とコミット**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

```bash
git add crates/polaris-core/src/agent.rs crates/polaris-core/src/dir_watch.rs
git commit -m "feat(polaris-core): trigger files.md regeneration from root tool calls

bash/write/editの呼び出し前後でファイルシステム差分を取り、新規ディレ
クトリ・既存ディレクトリへの新規ファイルを検出したらfiles_mdへ渡す。
caller==\"root\"の場合のみ発火する——subagent自身のツール呼び出しで
再発火すると、files-md-writerが書いたfiles.md自身が「新規ファイル」
として検出され自己再トリガーするループになりうるため。

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 6: 失敗の非伝播・常時コンテキスト無関係の実測固定

**Files:**
- Modify: `crates/polaris-core/src/files_md.rs`(テスト追加)
- Modify: `crates/polaris-core/src/budget.rs`(テスト追加)

**Interfaces:** 新しい公開APIは無い。既存機能に対するテストのみ追加する。

- [ ] **Step 1: `files-md-writer` の実行失敗が元のツール呼び出しに影響しないことを確認する**

`crates/polaris-core/src/files_md.rs` のテストモジュールに追加する。`regenerate_for_changes` 自体が `()` を返す(エラー型を持たない)ことが、この受け入れ基準をシグネチャレベルで保証している。それに加えて、実際に失敗するプロバイダを渡しても関数がパニックせず正常に返ることを確認する統合テストを1本書く。

```rust
    #[tokio::test]
    async fn a_failing_subagent_provider_does_not_panic_or_propagate_an_error() {
        let root = tempfile::tempdir().unwrap();
        let new_dir = root.path().join("newdir");
        std::fs::create_dir_all(&new_dir).unwrap();
        let changes = changes_with(vec![new_dir.clone()], vec![]);

        struct AlwaysErrors;
        #[async_trait::async_trait]
        impl Provider for AlwaysErrors {
            async fn complete(
                &self,
                _req: polaris_provider::CompletionRequest,
            ) -> Result<polaris_provider::CompletionResponse, polaris_provider::ProviderError> {
                Err(polaris_provider::ProviderError::Other("boom".into()))
            }
        }

        let agent_types = vec![files_md_writer_fixture_agent_type_for_files_md_tests()];
        let audit_path = root.path().join("audit.jsonl");
        let audit = std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::audit::AuditLog::open(&audit_path).unwrap(),
        ));
        let sandbox = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::WorkspaceWrite,
            &[root.path().to_path_buf()],
        )
        .unwrap();

        // パニックしないこと自体がこのテストの主張。
        regenerate_for_changes(
            &changes,
            &agent_types,
            std::sync::Arc::new(AlwaysErrors),
            audit,
            &sandbox,
            std::path::Path::new("/bin/true"),
        )
        .await;

        let log_text = std::fs::read_to_string(&audit_path).unwrap();
        assert!(log_text.contains("\"tool\":\"files-md-writer\""));
    }
```

`files_md_writer_fixture_agent_type_for_files_md_tests()` は、`spawn.rs` の既存の `readwrite_fixture_agent_type()` パターンに倣い、`AgentAccess::ReadWrite`・`polaris_wall_seconds`/`polaris_max_turns` に小さい値を持つ `AgentType { name: "files-md-writer".to_string(), ... }` を組み立てるヘルパとして、このファイルのテストモジュールに追加する。`Provider`/`ProviderError`/`CompletionRequest`/`CompletionResponse` の正確な型・バリアント名は `polaris-provider` クレートの既存定義(`spawn.rs` のテストが同様のモック `Provider` 実装で使っている実際の型)に合わせる。

Run: `cargo test -p polaris-core a_failing_subagent_provider_does_not_panic_or_propagate_an_error`

Expected: PASS。

- [ ] **Step 2: 常時コンテキスト予算への影響が無いことを実測固定する**

`crates/polaris-core/src/budget.rs` の既存テストモジュールに追加する。この機構(`dir_watch`/`gitignore`/`files_md`)は `AlwaysOn`/`assemble_always_on` を一切呼ばないため、既存の `always_on_context_stays_within_budget` 等の既存テストが無修正のまま通ること自体が実質的な確認になるが、意図を明示するテストを1本足す。

```rust
    #[test]
    fn files_md_autogeneration_is_not_part_of_the_always_on_context() {
        // dir_watch/gitignore/files_md はいずれも `AlwaysOn`/
        // `assemble_always_on` を呼ばない、ツール呼び出し前後で動く
        // ハーネス内部処理である。この不変条件は「新しいトークン源が
        // 無い」という設計そのものであり、既存の6ツール構成の常時
        // コンテキストが変わらないことをもって固定する。
        let tools = polaris_tools::all_specs();
        let with_files_md_feature = always_on_tokens("system prompt placeholder", &tools);
        assert_eq!(with_files_md_feature, always_on_tokens("system prompt placeholder", &tools));
        assert!(with_files_md_feature <= BUDGET_LIMIT);
    }
```

Run: `cargo test -p polaris-core files_md_autogeneration_is_not_part_of_the_always_on_context`

Expected: PASS(実装なしで既に成り立つ不変条件を固定するだけ)。

- [ ] **Step 3: 全体検証とコミット**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

```bash
git add crates/polaris-core/src/files_md.rs crates/polaris-core/src/budget.rs
git commit -m "test: pin failure isolation and always-on budget neutrality for files.md autogen

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## 自己レビュー記録

- **spec 網羅性:** 検知(ファイルシステム差分) → Task 1。生成(`spawn::run_one` 直接呼び出し、`files-md-writer` 型) → Task 3/4。`.gitignore` 登録 → Task 2。ハーネスからの同期呼び出し・root限定発火・自己再トリガー防止 → Task 5。失敗の非伝播・常時コンテキスト無関係 → Task 6。削除・リネーム非対応はスコープ外として実装しない(スペック記載通り)。
- **型の一貫性:** `DirSnapshot`/`DirChanges`(Task 1)は Task 4/5 まで一貫して使われる。`SpawnTask`/`TaskOutcome`/`run_one`(M4、既存)のシグネチャは変更しない。
- **見つかったが計画に含めなかったもの:** 複数書込許可ルートへの対応(Task 5 では最初のルートのみ見る単純化——コメントに明記)、`.git` 以外のVCSルート判定、`files.md` の内容が古くなった場合の手動再生成コマンド。いずれもスペックの非対象・既知の制限の範囲内。
