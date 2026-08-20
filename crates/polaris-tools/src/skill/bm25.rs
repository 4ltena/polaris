//! BM25 ランキング。トークナイズ・語幹化・同義語展開・スコアリングだけを
//! 持ち、`near_universal` の選定基準や `lookup` の呼び出し規約を知らない。

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;

use super::Named;

const K1: f64 = 1.5;
const B: f64 = 0.75;
const NAME_WEIGHT: usize = 3;

const SUFFIXES: &[(&str, &str)] = &[("ies", "y"), ("ing", ""), ("ed", ""), ("es", ""), ("s", "")];

/// 接尾辞除去による軽量な語幹化。長さ 4 以下のトークンはそのまま返す。
/// トークンは `tokenize` が生成した `[a-z0-9]+` 由来のものだけを想定し、
/// バイト長と文字数が一致する ASCII 前提で長さを扱う。
fn stem(tok: &str) -> String {
    if tok.len() <= 4 {
        return tok.to_string();
    }
    for (suf, repl) in SUFFIXES {
        if let Some(stripped) = tok.strip_suffix(suf)
            && stripped.len() >= 3
        {
            return format!("{stripped}{repl}");
        }
    }
    tok.to_string()
}

static TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[a-z0-9]+").unwrap());

static STOPWORDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "a", "an", "the", "and", "or", "of", "to", "for", "in", "on", "with", "this", "that", "it",
        "is", "are", "be",
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

const SYNONYMS: &[(&str, &[&str])] = &[
    ("credential", &["secret", "key", "token", "password"]),
    ("leak", &["expose", "exfiltrate", "commit", "leaked"]),
    (
        "403",
        &["forbidden", "permission", "denied", "unauthorized"],
    ),
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

fn doc_tokens<T: Named>(skill: &T) -> Vec<String> {
    let name_text = skill.name().replace(['-', '_', ':'], " ");
    let name_toks = tokenize(&name_text, true);
    let mut doc = Vec::with_capacity(name_toks.len() * NAME_WEIGHT);
    for _ in 0..NAME_WEIGHT {
        doc.extend(name_toks.iter().cloned());
    }
    doc.extend(tokenize(skill.description(), true));
    doc
}

/// 呼び出しごとに組み立てる BM25 インデックス。skill 数は数百〜千のオー
/// ダーであり、`lookup` の呼び出しをまたいだキャッシュは持たない。
pub(crate) struct Bm25<'a, T: Named> {
    items: &'a [T],
    doc_len: Vec<usize>,
    avgdl: f64,
    idf: HashMap<String, f64>,
    tf: Vec<HashMap<String, u32>>,
}

impl<'a, T: Named> Bm25<'a, T> {
    pub(crate) fn new(items: &'a [T]) -> Self {
        let docs: Vec<Vec<String>> = items.iter().map(doc_tokens).collect();
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
            items,
            doc_len,
            avgdl,
            idf,
            tf,
        }
    }

    /// クエリを同義語展開してからランキングする。スコア 0（クエリと共通
    /// トークンが一つも無い）の skill は候補から除外する。同点は name の
    /// 昇順で安定させる。
    pub(crate) fn rank(&self, q: &str, k: usize) -> Vec<&'a T> {
        let expanded = expand_query(q);
        let qtoks = tokenize(&expanded, true);
        let mut scored: Vec<(f64, usize)> = Vec::new();
        for i in 0..self.items.len() {
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
                .then_with(|| self.items[a.1].name().cmp(self.items[b.1].name()))
        });
        scored
            .into_iter()
            .take(k)
            .map(|(_, i)| &self.items[i])
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_skills::Skill;

    #[test]
    fn stemming_strips_a_recognized_suffix_when_long_enough() {
        assert_eq!(stem("deploying"), "deploy");
        // The suffix table matches "es" before "s" (first-match-wins, per
        // the Global Constraints order), so "creates" strips as "es" ->
        // "" rather than falling through to "s" -> "".
        assert_eq!(stem("creates"), "creat");
    }

    #[test]
    fn stemming_leaves_short_tokens_unchanged() {
        assert_eq!(stem("used"), "used");
        assert_eq!(stem("git"), "git");
    }

    #[test]
    fn stemming_does_not_strip_below_the_minimum_stem_length() {
        // "yes" returns via the earlier len() <= 4 short-circuit -- it
        // never reaches the suffix loop below at all.
        assert_eq!(stem("yes"), "yes");
        // "bring" (len 5, > 4) reaches the suffix loop and matches "ing",
        // but the stripped remainder "br" has length 2, under the
        // length-3 floor, so the rule does not apply. This is the
        // floor's actual trigger case.
        assert_eq!(stem("bring"), "bring");
    }

    #[test]
    fn tokenize_lowercases_strips_stopwords_and_stems() {
        let toks = tokenize("Creates a commit. Used for commit or git topics.", true);
        assert_eq!(
            toks,
            vec!["creat", "commit", "used", "commit", "git", "topic"]
        );
    }

    #[test]
    fn tokenize_without_stemming_keeps_the_surface_form() {
        let toks = tokenize("deploying", false);
        assert_eq!(toks, vec!["deploying"]);
    }

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
        let skills = fixtures();
        let index = Bm25::new(&skills);
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
        let skills = fixtures();
        let index = Bm25::new(&skills);
        let hits = index.rank("completely unrelated term", 20);
        assert!(hits.is_empty());
    }

    #[test]
    fn rank_respects_a_multi_word_query_the_old_substring_match_could_never_find() {
        // The old substring match required the whole query to appear as
        // one contiguous run; a phrase like this never appears verbatim in
        // any description, yet its individual tokens do.
        let skills = fixtures();
        let index = Bm25::new(&skills);
        let hits = index.rank("how do I write a commit for this", 20);
        assert!(hits.iter().any(|s| s.name == "git-commit"));
    }

    #[test]
    fn rank_truncates_to_k_even_when_more_candidates_score_positively() {
        let skills: Vec<Skill> = (0..10)
            .map(|i| Skill {
                name: format!("deploy-tool-{i}"),
                description: "Handles deployment to production servers.".into(),
                body: "b".into(),
                path: format!("/x/deploy-tool-{i}/SKILL.md").into(),
            })
            .collect();
        let index = Bm25::new(&skills);
        let hits = index.rank("deploying to production", 3);
        assert_eq!(hits.len(), 3);
    }

    #[test]
    fn rank_breaks_ties_by_name_ascending() {
        let skills = vec![
            Skill {
                name: "zeta-deploy".into(),
                description: "Handles deployment to production.".into(),
                body: "b".into(),
                path: "/x/zeta-deploy/SKILL.md".into(),
            },
            Skill {
                name: "alpha-deploy".into(),
                description: "Handles deployment to production.".into(),
                body: "b".into(),
                path: "/x/alpha-deploy/SKILL.md".into(),
            },
        ];
        let index = Bm25::new(&skills);
        let hits = index.rank("deployment production", 20);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].name, "alpha-deploy");
        assert_eq!(hits[1].name, "zeta-deploy");
    }
}
