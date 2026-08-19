# polaris M3b BM25 skill ルータ 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `polaris-tools::skill::lookup` の検索経路を部分文字列一致から BM25 ランキングへ置き換え、trigger 形式かつ量化語を含み固有技術名を持たない「ほぼ常に関連する」skill をクエリの一致に関係なく結果へ常時含める。複数語の自然文クエリで、現行実装では 0 件だった検索が実際に skill を見つけられるようにする。

**Architecture:** `polaris-tools::skill` の下へ 2 つのサブモジュールを新設する。`bm25.rs` はトークナイザ・語幹化・同義語展開・BM25 ランキングだけを持つ自己完結した計算モジュールで、`Skill` 以外の外部状態に依存しない。`near_universal.rs` は description の書き方（trigger 形式・量化語・固有名詞の有無）だけで判定する選定モジュールで、これも `Skill` 以外に依存しない。両者は性質の異なる壊れ方をする（前者はスコア計算のバグ、後者は正規表現・文字列判定のバグ）ため、レビューの単位として分ける。`skill.rs` 本体はこの 2 つを呼び出して結果を合成するだけの、薄い統合層のまま残す。`polaris-core` 側は一切変更しない。既存の常時コンテキスト不変条件（`budget.rs`）に、この変更が実際に触れていないことを確認する新しいテストだけを 1 本足す。

**Tech Stack:** Rust 1.96 / edition 2024。新しい外部クレート依存は無し。`regex`（ワークスペース既存の依存）を trigger 形式・量化語の判定に使う。既存の `polaris_skills::Skill` 型を変更しない。

**Spec:** `docs/superpowers/specs/2026-08-20-polaris-skill-bm25-router-design.md`

## Global Constraints

以下は仕様から逐語で写した値である。全タスクの要件に暗黙に含まれる。

- `lookup` の公開シグネチャ `pub fn lookup(skills: &[Skill], q: &str) -> String` は変更しない
- `MAX_BODY_BYTES`（32 KiB）、`MAX_RESULTS`（20）、`MAX_LIST_BYTES`（8 KiB）、`MAX_ECHOED_QUERY_BYTES`（120）は変更しない
- BM25 パラメータ: `k1 = 1.5`、`b = 0.75`、name フィールドの重み `3`
- ストップワード（17 語）: `a, an, the, and, or, of, to, for, in, on, with, this, that, it, is, are, be`
- 語幹化の接尾辞規則（この順で最初に一致したものを適用。トークン長が 4 以下ならそのまま。除去後の長さが 3 未満になる場合は適用しない）: `ies→y`、`ing→''`、`ed→''`、`es→''`、`s→''`
- 同義語表（9 組、キー以外の追加は行わない）:
  ```
  credential   -> secret, key, token, password
  leak         -> expose, exfiltrate, commit, leaked
  403          -> forbidden, permission, denied, unauthorized
  install      -> dependency, package, npm, pip
  failing      -> broken, error, crash
  audit        -> review, scan, inspect
  permissions  -> access, acl, authorization
  ftp          -> deploy, upload, publish
  ci           -> pipeline, workflow, automation
  ```
- trigger 形式の判定: `(?i)^(use|apply)\s+(when|for|before)`
- 量化語の判定: `(?i)\b(any|every|all)\b`
- 固有名詞判定から除外する一般略語一覧（28 語、大小文字を無視して比較）:
  `PR, PRs, CI, CD, API, APIs, SDK, CLI, URL, URI, JSON, YAML, HTTP, HTTPS, SQL, UI, UX, ID, IDs, TDD, QA, CSS, HTML, JS, TS, OS, IO, DB`
- `near_universal` の上限 `MAX_NEAR_UNIVERSAL = 20`。超過時は name の昇順で先頭 20 件のみを返す
- `assemble_always_on` の引数・戻り値・既存テストは一切変更しない。触れて良いのは `budget.rs` へ新しいテストを 1 本追加することだけ
- 検証は `cargo test --workspace`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo fmt --all -- --check` の 3 つ。`--all-targets` を落とすと、各タスク単体の新規関数がまだ本体コードから呼ばれていない段階で `dead_code` 警告が出る（テストコードからの呼び出しは `--all-targets` の下でのみ使用済みと判定される）

## ファイル構成

| ファイル | 責務 |
| --- | --- |
| `crates/polaris-tools/src/skill/bm25.rs` | トークナイザ、語幹化、同義語展開、BM25 ランキング。`Skill` の中身をどう並べるかだけを知り、`lookup` の呼び出し規約や `near_universal` の判定基準は知らない |
| `crates/polaris-tools/src/skill/near_universal.rs` | trigger 形式・量化語・固有名詞の判定と選定。BM25 のスコアリングを一切知らない |
| `crates/polaris-tools/src/skill.rs` | 既存の body 返却・空クエリ一覧・バイト上限ロジックはそのまま残し、検索経路だけを `bm25` と `near_universal` の呼び出しに差し替える薄い統合層 |
| `crates/polaris-core/src/budget.rs` | 変更なし。`near_universal` の該当件数が `AlwaysOn` のトークン数に影響しないことを確認する新しいテストのみ追加 |

`bm25.rs` と `near_universal.rs` を分けるのは、壊れ方が別だからである。前者はスコア計算式や語幹化の境界条件のバグ、後者は正規表現や文字列判定のバグであり、1 ファイルに混ぜるとどちらの層の欠陥かをテストの失敗が指さなくなる。

---

### Task 1: `bm25` サブモジュール

**Files:**
- Modify: `crates/polaris-tools/Cargo.toml`（`regex` 依存を追加）
- Create: `crates/polaris-tools/src/skill/bm25.rs`
- Modify: `crates/polaris-tools/src/skill.rs`（冒頭に `mod bm25;` を追加するだけ。`lookup` 本体はまだ変更しない）

**Interfaces:**
- Consumes: `polaris_skills::Skill`（既存、`name` / `description` / `body` / `path` フィールドを持つ）
- Produces:
  - `pub(crate) fn tokenize(text: &str, do_stem: bool) -> Vec<String>`
  - `pub(crate) fn expand_query(q: &str) -> String`
  - `pub(crate) struct Bm25<'a>`、`impl<'a> Bm25<'a> { pub(crate) fn new(skills: &'a [Skill]) -> Self; pub(crate) fn rank(&self, q: &str, k: usize) -> Vec<&'a Skill>; }`

- [ ] **Step 0: `polaris-tools` へ `regex` 依存を足す**

`regex` はワークスペースの `[workspace.dependencies]` に既にある（`polaris-core::secret_screen` が使用）が、`polaris-tools` 自身の `Cargo.toml` にはまだ無い。`crates/polaris-tools/Cargo.toml` の `[dependencies]` へ 1 行追加する。

```toml
regex = { workspace = true }
```

追加後の `[dependencies]` 全体は次の並びになる。

```toml
[dependencies]
polaris-sandbox = { path = "../polaris-sandbox" }
polaris-skills = { path = "../polaris-skills" }
regex = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
thiserror = { workspace = true }
```

Run: `cargo check -p polaris-tools`

Expected: 依存追加だけなので変更なく成功する（まだ `regex` を使うコードは無い）。

- [ ] **Step 1: 失敗するテストを書く（語幹化とトークナイズ）**

`crates/polaris-tools/src/skill/bm25.rs` を新規作成し、まず以下を書く。

```rust
//! BM25 ランキング。トークナイズ・語幹化・同義語展開・スコアリングだけを
//! 持ち、`near_universal` の選定基準や `lookup` の呼び出し規約を知らない。

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;

use polaris_skills::Skill;

const K1: f64 = 1.5;
const B: f64 = 0.75;
const NAME_WEIGHT: usize = 3;

const SUFFIXES: &[(&str, &str)] = &[
    ("ies", "y"),
    ("ing", ""),
    ("ed", ""),
    ("es", ""),
    ("s", ""),
];

/// 接尾辞除去による軽量な語幹化。長さ 4 以下のトークンはそのまま返す。
/// トークンは `tokenize` が生成した `[a-z0-9]+` 由来のものだけを想定し、
/// バイト長と文字数が一致する ASCII 前提で長さを扱う。
fn stem(tok: &str) -> String {
    if tok.len() <= 4 {
        return tok.to_string();
    }
    for (suf, repl) in SUFFIXES {
        if let Some(stripped) = tok.strip_suffix(suf) {
            if stripped.len() >= 3 {
                return format!("{stripped}{repl}");
            }
        }
    }
    tok.to_string()
}

static TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[a-z0-9]+").unwrap());

static STOPWORDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "a", "an", "the", "and", "or", "of", "to", "for", "in", "on", "with", "this", "that",
        "it", "is", "are", "be",
    ]
    .into_iter()
    .collect()
});

pub(crate) fn tokenize(text: &str, do_stem: bool) -> Vec<String> {
    let lower = text.to_lowercase();
    TOKEN_RE
        .find_iter(&lower)
        .map(|m| m.as_str().to_string())
        .filter(|t| !STOPWORDS.contains(t.as_str()))
        .map(|t| if do_stem { stem(&t) } else { t })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stemming_strips_a_recognized_suffix_when_long_enough() {
        assert_eq!(stem("deploying"), "deploy");
        assert_eq!(stem("creates"), "create");
    }

    #[test]
    fn stemming_leaves_short_tokens_unchanged() {
        assert_eq!(stem("used"), "used");
        assert_eq!(stem("git"), "git");
    }

    #[test]
    fn stemming_does_not_strip_below_the_minimum_stem_length() {
        // "es" stripped from "ies"-less "yes" would leave "y", under the
        // length-3 floor, so it must stay unchanged instead.
        assert_eq!(stem("yes"), "yes");
    }

    #[test]
    fn tokenize_lowercases_strips_stopwords_and_stems() {
        let toks = tokenize("Creates a commit. Used for commit or git topics.", true);
        assert_eq!(toks, vec!["create", "commit", "used", "commit", "git", "topic"]);
    }

    #[test]
    fn tokenize_without_stemming_keeps_the_surface_form() {
        let toks = tokenize("deploying", false);
        assert_eq!(toks, vec!["deploying"]);
    }
}
```

- [ ] **Step 2: テストを実行して通ることを確認する**

Run: `cargo test -p polaris-tools --lib skill::bm25:: -- --nocapture`

Expected: 5 件のテストが PASS（`stem`/`tokenize` はまだ最小実装ではなく本実装をそのまま書いたので、このステップは「書いた実装が自分の想定通りに動くか」の確認になる。落ちた場合は語幹化の期待値を実装ではなく仕様（Global Constraints の接尾辞規則）に合わせて直す）。

- [ ] **Step 3: 同義語展開を追加する**

`bm25.rs` の `tokenize` の下、`#[cfg(test)]` の上に追記する。

```rust
const SYNONYMS: &[(&str, &[&str])] = &[
    ("credential", &["secret", "key", "token", "password"]),
    ("leak", &["expose", "exfiltrate", "commit", "leaked"]),
    ("403", &["forbidden", "permission", "denied", "unauthorized"]),
    ("install", &["dependency", "package", "npm", "pip"]),
    ("failing", &["broken", "error", "crash"]),
    ("audit", &["review", "scan", "inspect"]),
    ("permissions", &["access", "acl", "authorization"]),
    ("ftp", &["deploy", "upload", "publish"]),
    ("ci", &["pipeline", "workflow", "automation"]),
];

/// クエリのトークンを同義語表のキーと照合し、一致すれば値をクエリ文字列
/// の末尾へ追記する。表になければクエリはそのまま返す。
pub(crate) fn expand_query(q: &str) -> String {
    let toks = tokenize(q, false);
    let mut extra: Vec<&str> = Vec::new();
    for t in &toks {
        let stemmed_t = stem(t);
        for (key, syns) in SYNONYMS {
            if t == key || stemmed_t == stem(key) {
                extra.extend_from_slice(syns);
            }
        }
    }
    if extra.is_empty() {
        q.to_string()
    } else {
        format!("{q} {}", extra.join(" "))
    }
}
```

テストモジュールへ追記する。

```rust
    #[test]
    fn expand_query_appends_synonyms_for_a_matched_key() {
        let expanded = expand_query("credential rotation");
        assert!(expanded.contains("secret"));
        assert!(expanded.contains("token"));
    }

    #[test]
    fn expand_query_matches_through_stemming() {
        // "installing" stems to "install", which is a synonym key.
        let expanded = expand_query("installing a package");
        assert!(expanded.contains("dependency"));
    }

    #[test]
    fn expand_query_is_unchanged_when_nothing_matches() {
        let q = "how to configure the sandbox";
        assert_eq!(expand_query(q), q);
    }
```

- [ ] **Step 4: テストを実行して通ることを確認する**

Run: `cargo test -p polaris-tools --lib skill::bm25:: -- --nocapture`

Expected: 8 件すべて PASS。

- [ ] **Step 5: BM25 本体を追加する**

`bm25.rs` の `SYNONYMS`/`expand_query` の下、テストモジュールの上に追記する。

```rust
fn doc_tokens(skill: &Skill) -> Vec<String> {
    let name_text = skill.name.replace(['-', '_', ':'], " ");
    let name_toks = tokenize(&name_text, true);
    let mut doc = Vec::with_capacity(name_toks.len() * NAME_WEIGHT);
    for _ in 0..NAME_WEIGHT {
        doc.extend(name_toks.iter().cloned());
    }
    doc.extend(tokenize(&skill.description, true));
    doc
}

/// 呼び出しごとに組み立てる BM25 インデックス。skill 数は数百〜千のオー
/// ダーであり、`lookup` の呼び出しをまたいだキャッシュは持たない。
pub(crate) struct Bm25<'a> {
    skills: &'a [Skill],
    doc_len: Vec<usize>,
    avgdl: f64,
    idf: HashMap<String, f64>,
    tf: Vec<HashMap<String, u32>>,
}

impl<'a> Bm25<'a> {
    pub(crate) fn new(skills: &'a [Skill]) -> Self {
        let docs: Vec<Vec<String>> = skills.iter().map(doc_tokens).collect();
        let doc_len: Vec<usize> = docs.iter().map(Vec::len).collect();
        let avgdl = if docs.is_empty() {
            0.0
        } else {
            doc_len.iter().sum::<usize>() as f64 / docs.len() as f64
        };

        let mut df: HashMap<String, usize> = HashMap::new();
        for doc in &docs {
            let unique: HashSet<&String> = doc.iter().collect();
            for term in unique {
                *df.entry(term.clone()).or_insert(0) += 1;
            }
        }
        let n = docs.len() as f64;
        let idf: HashMap<String, f64> = df
            .into_iter()
            .map(|(term, d)| {
                let d = d as f64;
                (term, ((n - d + 0.5) / (d + 0.5) + 1.0).ln())
            })
            .collect();

        let tf: Vec<HashMap<String, u32>> = docs
            .into_iter()
            .map(|doc| {
                let mut counts = HashMap::new();
                for term in doc {
                    *counts.entry(term).or_insert(0u32) += 1;
                }
                counts
            })
            .collect();

        Bm25 {
            skills,
            doc_len,
            avgdl,
            idf,
            tf,
        }
    }

    /// クエリを同義語展開してからランキングする。スコア 0（クエリと共通
    /// トークンが一つも無い）の skill は候補から除外する。同点は name の
    /// 昇順で安定させる。
    pub(crate) fn rank(&self, q: &str, k: usize) -> Vec<&'a Skill> {
        let expanded = expand_query(q);
        let qtoks = tokenize(&expanded, true);
        let mut scored: Vec<(f64, usize)> = Vec::new();
        for i in 0..self.skills.len() {
            let mut s = 0.0;
            let dl = self.doc_len[i] as f64;
            for term in &qtoks {
                let Some(&f) = self.tf[i].get(term) else {
                    continue;
                };
                let f = f as f64;
                let idf = *self.idf.get(term).unwrap_or(&0.0);
                s += idf * (f * (K1 + 1.0)) / (f + K1 * (1.0 - B + B * dl / self.avgdl));
            }
            if s > 0.0 {
                scored.push((s, i));
            }
        }
        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| self.skills[a.1].name.cmp(&self.skills[b.1].name))
        });
        scored
            .into_iter()
            .take(k)
            .map(|(_, i)| &self.skills[i])
            .collect()
    }
}
```

テストモジュールへ追記する（fixture ヘルパも含む）。

```rust
    fn fixtures() -> Vec<Skill> {
        vec![
            Skill {
                name: "git-commit".into(),
                description: "Creates a commit. Used for commit or git topics.".into(),
                body: "Body A".into(),
                path: "/x/git-commit/SKILL.md".into(),
            },
            Skill {
                name: "writing-style".into(),
                description: "Polishes Japanese prose.".into(),
                body: "Body B".into(),
                path: "/x/writing-style/SKILL.md".into(),
            },
            Skill {
                name: "deploy-tool".into(),
                description: "Handles deployment to production.".into(),
                body: "Body C".into(),
                path: "/x/deploy-tool/SKILL.md".into(),
            },
        ]
    }

    #[test]
    fn rank_finds_a_skill_by_a_synonym_the_description_never_uses() {
        // "credential" expands to "secret", which git-commit's description
        // never contains, but nothing else does either -- add a skill that
        // does, so the synonym channel is actually exercised.
        let mut skills = fixtures();
        skills.push(Skill {
            name: "secret-scanner".into(),
            description: "Scans for leaked secrets before commit.".into(),
            body: "Body D".into(),
            path: "/x/secret-scanner/SKILL.md".into(),
        });
        let index = Bm25::new(&skills);
        let hits = index.rank("credential", 20);
        assert!(
            hits.iter().any(|s| s.name == "secret-scanner"),
            "synonym expansion did not surface secret-scanner: {:?}",
            hits.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn rank_finds_a_skill_through_stemming() {
        let index = Bm25::new(&fixtures());
        let hits = index.rank("deploying to prod", 20);
        assert!(hits.iter().any(|s| s.name == "deploy-tool"));
    }

    #[test]
    fn rank_prefers_a_name_field_match_over_a_description_only_match() {
        let skills = vec![
            Skill {
                name: "widget".into(),
                description: "Mentions gadget only in passing detail.".into(),
                body: "b".into(),
                path: "/x/widget/SKILL.md".into(),
            },
            Skill {
                name: "gadget-helper".into(),
                description: "Mentions gadget only in passing detail.".into(),
                body: "b".into(),
                path: "/x/gadget-helper/SKILL.md".into(),
            },
        ];
        let index = Bm25::new(&skills);
        let hits = index.rank("gadget", 20);
        assert_eq!(
            hits.first().map(|s| s.name.as_str()),
            Some("gadget-helper"),
            "the name-field match did not rank first: {:?}",
            hits.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn rank_excludes_skills_with_zero_score() {
        let index = Bm25::new(&fixtures());
        let hits = index.rank("completely unrelated term", 20);
        assert!(hits.is_empty());
    }

    #[test]
    fn rank_respects_a_multi_word_query_the_old_substring_match_could_never_find() {
        // The old substring match required the whole query to appear as
        // one contiguous run; a phrase like this never appears verbatim in
        // any description, yet its individual tokens do.
        let index = Bm25::new(&fixtures());
        let hits = index.rank("how do I write a commit for this", 20);
        assert!(hits.iter().any(|s| s.name == "git-commit"));
    }
```

- [ ] **Step 6: テストを実行して通ることを確認する**

Run: `cargo test -p polaris-tools --lib skill::bm25:: -- --nocapture`

Expected: 13 件すべて PASS。

- [ ] **Step 7: `skill.rs` へモジュール宣言を足す**

`crates/polaris-tools/src/skill.rs` の冒頭、`use polaris_skills::Skill;` の直後に追記する。

```rust
mod bm25;
```

- [ ] **Step 8: 全体を検証する**

Run: `cargo test -p polaris-tools && cargo clippy -p polaris-tools --all-targets -- -D warnings && cargo fmt -p polaris-tools -- --check`

Expected: すべて成功。`mod bm25;` を足しただけでは `lookup` はまだ何も呼んでいないため、`bm25` 内の関数はここではまだ `dead_code` 警告の対象になる可能性があるが、`--all-targets` がテストコードからの使用を検出するため警告は出ない。もし出た場合は `--all-targets` が抜けていないか確認する。

- [ ] **Step 9: コミット**

```bash
git add crates/polaris-tools/src/skill/bm25.rs crates/polaris-tools/src/skill.rs
git commit -m "feat(polaris-tools): add BM25 ranking as an unwired skill submodule"
```

---

### Task 2: `near_universal` サブモジュール

**Files:**
- Create: `crates/polaris-tools/src/skill/near_universal.rs`
- Modify: `crates/polaris-tools/src/skill.rs`（`mod near_universal;` と再エクスポートを追加するだけ）

**Interfaces:**
- Consumes: `polaris_skills::Skill`
- Produces:
  - `pub fn near_universal(skills: &[Skill]) -> Vec<&Skill>`
  - `pub const MAX_NEAR_UNIVERSAL: usize`
  - どちらも `polaris_tools::skill::near_universal` / `polaris_tools::skill::MAX_NEAR_UNIVERSAL` として外部（`polaris-core`）から到達可能にする

- [ ] **Step 1: 失敗するテストを書く（固有名詞判定）**

`crates/polaris-tools/src/skill/near_universal.rs` を新規作成する。

```rust
//! 「ほぼ常に関連する」skill の選定。BM25 のスコアリングを一切知らず、
//! description の書き方の慣習（trigger 形式・量化語・固有名詞の有無）だ
//! けを見る。基準の根拠は
//! `docs/superpowers/specs/2026-08-20-polaris-skill-bm25-router-design.md`
//! を参照。

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;

use polaris_skills::Skill;

/// この選定が返す件数の安全弁。測定したコーパス（831 件）では 4 件しか
/// 該当しなかったが、将来 trigger 形式の skill が異常に多いコーパスが来
/// ても、この設計が避けようとした「常時コンテキストが skill 数に比例し
/// て太る」問題を別の経路で再現しないための上限である。
pub const MAX_NEAR_UNIVERSAL: usize = 20;

static GENERIC_ACRONYMS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "PR", "PRS", "CI", "CD", "API", "APIS", "SDK", "CLI", "URL", "URI", "JSON", "YAML",
        "HTTP", "HTTPS", "SQL", "UI", "UX", "ID", "IDS", "TDD", "QA", "CSS", "HTML", "JS", "TS",
        "OS", "IO", "DB",
    ]
    .into_iter()
    .collect()
});

/// `desc` の中に、固有の製品・技術名と見なせる語があるかを判定する。文
/// 頭の "Use"/"Apply" は除外し、大文字で始まる単語のうち一般的な略語一
/// 覧に含まれないものが一つでもあれば true を返す。
fn names_specific_tech(desc: &str) -> bool {
    for (i, word) in desc.split_whitespace().enumerate() {
        if i == 0 {
            continue;
        }
        let clean: String = word.chars().filter(|c| c.is_ascii_alphabetic()).collect();
        if clean.len() < 2 {
            continue;
        }
        let Some(first) = clean.chars().next() else {
            continue;
        };
        if !first.is_uppercase() {
            continue;
        }
        if GENERIC_ACRONYMS.contains(clean.to_uppercase().as_str()) {
            continue;
        }
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_description_naming_a_specific_product_is_flagged() {
        assert!(names_specific_tech(
            "Use when doing any task involving Supabase."
        ));
    }

    #[test]
    fn a_description_with_only_generic_acronyms_is_not_flagged() {
        assert!(!names_specific_tech(
            "Use when handling any API call, before shipping."
        ));
    }

    #[test]
    fn a_fully_generic_description_is_not_flagged() {
        assert!(!names_specific_tech(
            "Use when implementing any feature or bugfix, before writing implementation code"
        ));
    }
}
```

- [ ] **Step 2: テストを実行して通ることを確認する**

Run: `cargo test -p polaris-tools --lib skill::near_universal:: -- --nocapture`

Expected: 3 件すべて PASS。

- [ ] **Step 3: trigger 形式・量化語の判定と選定本体を追加する**

`names_specific_tech` の下、テストモジュールの上に追記する。

```rust
static TRIGGER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(use|apply)\s+(when|for|before)").unwrap());
static QUANT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(any|every|all)\b").unwrap());

/// 3 条件（trigger 形式、量化語、固有名詞なし）をすべて満たす skill を
/// 選ぶ。`MAX_NEAR_UNIVERSAL` を超えた場合は name の昇順で先頭のみ返す。
pub fn near_universal(skills: &[Skill]) -> Vec<&Skill> {
    let mut selected: Vec<&Skill> = skills
        .iter()
        .filter(|s| {
            let desc = s.description.trim();
            TRIGGER_RE.is_match(desc) && QUANT_RE.is_match(desc) && !names_specific_tech(desc)
        })
        .collect();
    selected.sort_by(|a, b| a.name.cmp(&b.name));
    selected.truncate(MAX_NEAR_UNIVERSAL);
    selected
}
```

テストモジュールへ追記する。

```rust
    fn skill(name: &str, description: &str) -> Skill {
        Skill {
            name: name.into(),
            description: description.into(),
            body: "body".into(),
            path: format!("/x/{name}/SKILL.md").into(),
        }
    }

    #[test]
    fn a_skill_meeting_all_three_conditions_is_selected() {
        let skills = vec![skill(
            "test-driven-development",
            "Use when implementing any feature or bugfix, before writing implementation code",
        )];
        let result = near_universal(&skills);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "test-driven-development");
    }

    #[test]
    fn a_trigger_style_skill_without_a_quantifier_is_not_selected() {
        let skills = vec![skill(
            "narrow-trigger",
            "Use when the deploy script fails, before retrying manually",
        )];
        assert!(near_universal(&skills).is_empty());
    }

    #[test]
    fn a_skill_naming_a_specific_product_is_not_selected_even_with_a_quantifier() {
        let skills = vec![skill(
            "supabase",
            "Use when doing any task involving Supabase.",
        )];
        assert!(near_universal(&skills).is_empty());
    }

    #[test]
    fn a_non_trigger_style_skill_is_not_selected() {
        let skills = vec![skill(
            "spec-first-development",
            "Define and approve specifications before any substantial feature.",
        )];
        assert!(near_universal(&skills).is_empty());
    }

    #[test]
    fn an_empty_skill_set_returns_an_empty_vec() {
        assert!(near_universal(&[]).is_empty());
    }

    #[test]
    fn selection_is_capped_and_sorted_by_name_when_it_exceeds_the_limit() {
        let skills: Vec<Skill> = (0..(MAX_NEAR_UNIVERSAL + 5))
            .map(|i| {
                skill(
                    &format!("universal-{i:02}"),
                    "Use when implementing any feature or bugfix, before writing implementation code",
                )
            })
            .collect();
        let result = near_universal(&skills);
        assert_eq!(result.len(), MAX_NEAR_UNIVERSAL);
        let names: Vec<&str> = result.iter().map(|s| s.name.as_str()).collect();
        let mut sorted_names = names.clone();
        sorted_names.sort();
        assert_eq!(names, sorted_names, "result was not sorted by name");
        assert_eq!(names[0], "universal-00");
    }
```

- [ ] **Step 4: テストを実行して通ることを確認する**

Run: `cargo test -p polaris-tools --lib skill::near_universal:: -- --nocapture`

Expected: 9 件すべて PASS。

- [ ] **Step 5: `skill.rs` へモジュール宣言と再エクスポートを足す**

`crates/polaris-tools/src/skill.rs` の `mod bm25;` の下に追記する。

```rust
mod near_universal;

pub use near_universal::{near_universal, MAX_NEAR_UNIVERSAL};
```

- [ ] **Step 6: 全体を検証する**

Run: `cargo test -p polaris-tools && cargo clippy -p polaris-tools --all-targets -- -D warnings && cargo fmt -p polaris-tools -- --check`

Expected: すべて成功。

- [ ] **Step 7: コミット**

```bash
git add crates/polaris-tools/src/skill/near_universal.rs crates/polaris-tools/src/skill.rs
git commit -m "feat(polaris-tools): add near-universal skill selection as an unwired submodule"
```

---

### Task 3: `lookup` への統合

**Files:**
- Modify: `crates/polaris-tools/src/skill.rs`

**Interfaces:**
- Consumes: Task 1 の `bm25::Bm25::{new, rank}`、Task 2 の `near_universal::{near_universal, MAX_NEAR_UNIVERSAL}`
- Produces: `pub fn lookup(skills: &[Skill], q: &str) -> String`（シグネチャは変更なし、内部実装のみ差し替え）。`list_candidates` はこのファイル内のプライベート関数のままだが、シグネチャに `max_count: usize` を追加する

- [ ] **Step 1: 既存の全テストが今の実装のままで通ることを確認する（変更前の基準線）**

Run: `cargo test -p polaris-tools --lib skill:: -- --nocapture`

Expected: 既存の 13 件（`an_exact_name_returns_the_body` から `a_body_within_the_cap_is_returned_whole_and_unmarked` まで）がすべて PASS。この時点ではまだ `lookup` を変更していない。

- [ ] **Step 2: `list_candidates` のシグネチャを変える**

`crates/polaris-tools/src/skill.rs` の `list_candidates` を次のように置き換える。

```rust
fn list_candidates(items: &[&Skill], header: &str, max_count: usize) -> String {
    let mut out = String::from(header);
    let mut shown = 0usize;
    for s in items.iter().take(max_count) {
        let line = format!("- {}: {}\n", s.name, s.description);
        if shown > 0 && out.len() + line.len() > MAX_LIST_BYTES {
            break;
        }
        out.push_str(&line);
        shown += 1;
    }
    if shown < items.len() {
        out.push_str(&format!(
            "(showing {shown} of {} total; narrow the query or pass a name directly.)\n",
            items.len()
        ));
    }
    out
}
```

変わったのは `MAX_RESULTS` を内部で直接使っていた `.take(MAX_RESULTS)` を、引数 `max_count` に差し替えただけである。呼び出し側をまだ直していないので、この時点ではコンパイルが通らない（既存の呼び出し 3 箇所が 2 引数のまま）。次のステップで直す。

- [ ] **Step 3: `lookup` の検索経路を書き換える**

`lookup` 関数全体を次のように置き換える（exact-name 一致と body 返却部分は変更なし。空クエリの分岐と検索の分岐が変わる）。

```rust
pub fn lookup(skills: &[Skill], q: &str) -> String {
    if skills.is_empty() {
        return "no skill was found at all. there is no SKILL.md at the search location."
            .to_string();
    }

    let q = q.trim();

    if let Some(s) = skills.iter().find(|s| s.name == q) {
        let (body, truncated) = cap_bytes(&s.body, MAX_BODY_BYTES);
        let mut out = format!("# {}\n\n{}\n", s.name, body);
        if truncated {
            out.push_str(&format!(
                "\n(the body was truncated at {MAX_BODY_BYTES} bytes. read {} directly for the full text.)\n",
                s.path.display()
            ));
        }
        return out;
    }

    if q.is_empty() {
        let all: Vec<&Skill> = skills.iter().collect();
        return list_candidates(
            &all,
            "q is empty, so listing the skills that exist. pass a name or term to narrow it down.\n",
            MAX_RESULTS,
        );
    }

    let index = bm25::Bm25::new(skills);
    let ranked = index.rank(q, MAX_RESULTS);
    let universal = near_universal::near_universal(skills);
    let mut combined: Vec<&Skill> = ranked;
    for u in universal {
        if !combined.iter().any(|s| s.name == u.name) {
            combined.push(u);
        }
    }

    if combined.is_empty() {
        let all: Vec<&Skill> = skills.iter().collect();
        let (echoed, truncated) = cap_bytes(q, MAX_ECHOED_QUERY_BYTES);
        let header = if truncated {
            format!(
                "{echoed}… (query is long, showing only the first {MAX_ECHOED_QUERY_BYTES} bytes) matched no skill. what's available is listed below.\n"
            )
        } else {
            format!("{echoed} matched no skill. what's available is listed below.\n")
        };
        return list_candidates(&all, &header, MAX_RESULTS);
    }

    list_candidates(
        &combined,
        "candidates. pass the name as-is if you need the body.\n",
        MAX_RESULTS + near_universal::MAX_NEAR_UNIVERSAL,
    )
}
```

`near_universal` を候補の末尾へ追加してから `MAX_RESULTS` 単独ではなく
`MAX_RESULTS + MAX_NEAR_UNIVERSAL` を `list_candidates` へ渡しているのは、
BM25 が既に `MAX_RESULTS` 件を埋めた状態で `near_universal` の要素を追加
した場合に、`list_candidates` 内部の件数上限で末尾へ足したはずの要素が
再び切り落とされるのを防ぐためである。`combined` 自体の実際の長さは
BM25 の `MAX_RESULTS` 件と `near_universal` の `MAX_NEAR_UNIVERSAL` 件を
足した値を超えないため、この上限は実質的に発火しない安全弁である。

- [ ] **Step 4: 既存テストを再実行し、すべて変更なしで通ることを確認する**

Run: `cargo test -p polaris-tools --lib skill:: -- --nocapture`

Expected: Step 1 と同じ 13 件がすべて PASS（1 件も書き換えていない）。もし
落ちるテストがあれば、`list_candidates` の呼び出し 3 箇所（空クエリ・
一致なし・検索結果）に渡す `max_count` の値が Step 2/3 の記述と一致して
いるか確認する。

- [ ] **Step 5: 統合の失敗するテストを書く**

`skill.rs` の既存 `mod tests` の末尾（最後の `}` の直前）へ追記する。

```rust
    #[test]
    fn a_multi_word_query_finds_a_skill_the_old_substring_match_never_could() {
        let skills = vec![
            Skill {
                name: "git-commit".into(),
                description: "Creates a commit. Used for commit or git topics.".into(),
                body: "Body A".into(),
                path: "/x/git-commit/SKILL.md".into(),
            },
            Skill {
                name: "deploy-tool".into(),
                description: "Handles deployment to production servers.".into(),
                body: "Body B".into(),
                path: "/x/deploy-tool/SKILL.md".into(),
            },
        ];
        let out = lookup(&skills, "how do I deploy this to production");
        assert!(
            out.contains("deploy-tool"),
            "multi-word query did not find deploy-tool: {out}"
        );
    }

    #[test]
    fn a_synonym_query_finds_a_skill_that_never_uses_the_query_word() {
        let skills = vec![Skill {
            name: "secret-scanner".into(),
            description: "Scans for leaked secrets before commit.".into(),
            body: "body".into(),
            path: "/x/secret-scanner/SKILL.md".into(),
        }];
        let out = lookup(&skills, "credential rotation policy");
        assert!(
            out.contains("secret-scanner"),
            "synonym expansion did not surface secret-scanner: {out}"
        );
    }

    #[test]
    fn a_stemmed_query_finds_a_skill_using_a_different_word_form() {
        let skills = vec![Skill {
            name: "deploy-tool".into(),
            description: "Handles deployment to production servers.".into(),
            body: "body".into(),
            path: "/x/deploy-tool/SKILL.md".into(),
        }];
        let out = lookup(&skills, "deploying to prod");
        assert!(out.contains("deploy-tool"));
    }

    #[test]
    fn a_near_universal_skill_is_always_included_regardless_of_query_relevance() {
        let skills = vec![
            Skill {
                name: "test-driven-development".into(),
                description: "Use when implementing any feature or bugfix, before writing implementation code".into(),
                body: "body".into(),
                path: "/x/test-driven-development/SKILL.md".into(),
            },
            Skill {
                name: "deploy-tool".into(),
                description: "Handles deployment to production servers.".into(),
                body: "body".into(),
                path: "/x/deploy-tool/SKILL.md".into(),
            },
        ];
        // "production servers" matches deploy-tool by BM25, and has no
        // lexical relationship to test-driven-development at all -- the
        // near-universal skill must still show up.
        let out = lookup(&skills, "production servers");
        assert!(out.contains("deploy-tool"));
        assert!(
            out.contains("test-driven-development"),
            "near-universal skill was not included despite zero query relevance: {out}"
        );
    }

    #[test]
    fn a_near_universal_skill_does_not_appear_on_an_exact_name_match() {
        let skills = vec![
            Skill {
                name: "test-driven-development".into(),
                description: "Use when implementing any feature or bugfix, before writing implementation code".into(),
                body: "TDD body".into(),
                path: "/x/test-driven-development/SKILL.md".into(),
            },
            Skill {
                name: "deploy-tool".into(),
                description: "Handles deployment to production servers.".into(),
                body: "deploy body".into(),
                path: "/x/deploy-tool/SKILL.md".into(),
            },
        ];
        let out = lookup(&skills, "deploy-tool");
        assert!(out.contains("deploy body"));
        assert!(
            !out.contains("test-driven-development"),
            "near-universal skill leaked into an exact-name-match result: {out}"
        );
    }

    #[test]
    fn a_near_universal_skill_does_not_appear_on_an_empty_query_beyond_its_natural_listing() {
        // An empty query already lists everything, so a near-universal
        // skill appears there too -- but it must not be duplicated or
        // specially annotated, just present once like any other skill.
        let skills = vec![Skill {
            name: "test-driven-development".into(),
            description: "Use when implementing any feature or bugfix, before writing implementation code".into(),
            body: "body".into(),
            path: "/x/test-driven-development/SKILL.md".into(),
        }];
        let out = lookup(&skills, "");
        let occurrences = out.matches("test-driven-development").count();
        assert_eq!(occurrences, 1, "listed more than once: {out}");
    }

    #[test]
    fn a_corpus_with_no_near_universal_skill_behaves_exactly_as_before() {
        let skills = vec![Skill {
            name: "deploy-tool".into(),
            description: "Handles deployment to production servers.".into(),
            body: "body".into(),
            path: "/x/deploy-tool/SKILL.md".into(),
        }];
        let out = lookup(&skills, "deploying to prod");
        assert!(out.contains("deploy-tool"));
        assert!(out.contains("candidates. pass the name as-is if you need the body."));
        assert_eq!(out.matches("deploy-tool").count(), 1);
    }
```

- [ ] **Step 6: 統合テストを実行する**

Run: `cargo test -p polaris-tools --lib skill:: -- --nocapture`

Expected: 既存 13 件 + 新規 7 件の合計 20 件がすべて PASS。

- [ ] **Step 7: 全体を検証する**

Run: `cargo test -p polaris-tools && cargo clippy -p polaris-tools --all-targets -- -D warnings && cargo fmt -p polaris-tools -- --check`

Expected: すべて成功。

- [ ] **Step 8: コミット**

```bash
git add crates/polaris-tools/src/skill.rs
git commit -m "feat(polaris-tools): wire BM25 ranking and near-universal selection into skill lookup"
```

---

### Task 4: `budget.rs` の不変条件テスト

**Files:**
- Modify: `crates/polaris-core/src/budget.rs`

**Interfaces:**
- Consumes: Task 2/3 の `polaris_tools::skill::{near_universal, MAX_NEAR_UNIVERSAL}`、既存の `crate::prompt::assemble_always_on`
- Produces: 新しいテスト関数のみ。公開 API の変更はない

- [ ] **Step 1: 失敗する（かもしれない）テストを書く**

`crates/polaris-core/src/budget.rs` の
`the_always_on_total_does_not_move_as_the_number_of_skills_grows` テスト
関数の直後に追記する。

```rust
    /// M3b で `lookup` の戻り値へ near-universal な skill を常時含めるよ
    /// うにしたが、それは `AlwaysOn`（システムプロンプトとツール定義）
    /// とは別の経路（ツール結果、メッセージ末尾）である。この設計が
    /// `assemble_always_on` に一切触れていないことを、near-universal 該
    /// 当が 0 件・少数（4 件）・上限（`MAX_NEAR_UNIVERSAL` 件）のどの場
    /// 合でもトークン数が変わらないことで確認する。
    #[test]
    fn near_universal_skills_do_not_move_the_always_on_total() {
        fn universal_skill(i: usize) -> polaris_skills::Skill {
            polaris_skills::Skill {
                name: format!("universal-{i:02}"),
                description: "Use when implementing any feature or bugfix, before writing implementation code".into(),
                body: "body".into(),
                path: format!("/x/universal-{i:02}/SKILL.md").into(),
            }
        }
        fn ordinary_skill(i: usize) -> polaris_skills::Skill {
            polaris_skills::Skill {
                name: format!("ordinary-{i:02}"),
                description: "Handles Stripe webhook signature verification.".into(),
                body: "body".into(),
                path: format!("/x/ordinary-{i:02}/SKILL.md").into(),
            }
        }

        let none: Vec<polaris_skills::Skill> = (0..10).map(ordinary_skill).collect();
        let some: Vec<polaris_skills::Skill> = (0..4)
            .map(universal_skill)
            .chain((0..10).map(ordinary_skill))
            .collect();
        let many: Vec<polaris_skills::Skill> = (0..polaris_tools::skill::MAX_NEAR_UNIVERSAL)
            .map(universal_skill)
            .chain((0..10).map(ordinary_skill))
            .collect();

        // Confirm the fixtures actually exercise what they claim to before
        // trusting the token-count comparison below.
        assert_eq!(polaris_tools::skill::near_universal(&none).len(), 0);
        assert_eq!(polaris_tools::skill::near_universal(&some).len(), 4);
        assert_eq!(
            polaris_tools::skill::near_universal(&many).len(),
            polaris_tools::skill::MAX_NEAR_UNIVERSAL
        );

        let tokens_none = crate::prompt::assemble_always_on("", "", &none).tokens();
        let tokens_some = crate::prompt::assemble_always_on("", "", &some).tokens();
        let tokens_many = crate::prompt::assemble_always_on("", "", &many).tokens();

        assert_eq!(
            tokens_none, tokens_some,
            "AlwaysOn tokens moved when near-universal skills were added"
        );
        assert_eq!(
            tokens_none, tokens_many,
            "AlwaysOn tokens moved when the near-universal set reached its cap"
        );
    }
```

- [ ] **Step 2: テストを実行して通ることを確認する**

Run: `cargo test -p polaris-core --lib budget:: -- --nocapture`

Expected: 既存のテストに加えてこの新規テストが PASS。落ちる場合、
`polaris_tools::skill::near_universal(&none).len()` 等の fixture 確認そ
のものが落ちているなら Task 2 の実装との齟齬であり、トークン数比較が落
ちているなら `assemble_always_on` が意図せず skill の中身を見てしまって
いることを意味するので、`prompt.rs` を変更していないか確認する（この計
画では変更しない）。

- [ ] **Step 3: 全体を検証する**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`

Expected: すべて成功。ワークスペース全体を通すのはこのタスクが最後で、
Task 1〜3 で touch した `polaris-tools` と、このタスクで touch した
`polaris-core` の両方が噛み合っていることを確認するためである。

- [ ] **Step 4: コミット**

```bash
git add crates/polaris-core/src/budget.rs
git commit -m "test(polaris-core): pin that near-universal skills never move the always-on total"
```
