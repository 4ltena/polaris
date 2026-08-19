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
        "PR", "PRS", "CI", "CD", "API", "APIS", "SDK", "CLI", "URL", "URI", "JSON", "YAML", "HTTP",
        "HTTPS", "SQL", "UI", "UX", "ID", "IDS", "TDD", "QA", "CSS", "HTML", "JS", "TS", "OS",
        "IO", "DB",
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

#[cfg(test)]
mod tests {
    use super::*;

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
        // Descending input order so a truncate-before-sort (or no-sort)
        // implementation produces a visibly different, wrong result instead
        // of coincidentally matching sort-then-truncate's output.
        let skills: Vec<Skill> = (0..(MAX_NEAR_UNIVERSAL + 5))
            .rev()
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
        let expected: Vec<String> = (0..MAX_NEAR_UNIVERSAL)
            .map(|i| format!("universal-{i:02}"))
            .collect();
        assert_eq!(
            names, expected,
            "result was not sorted to name-ascending before being capped to MAX_NEAR_UNIVERSAL"
        );
    }

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
