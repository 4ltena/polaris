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
        assert!(
            !out.contains("本文A"),
            "検索で本文まで返してはいけない: {out}"
        );
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
