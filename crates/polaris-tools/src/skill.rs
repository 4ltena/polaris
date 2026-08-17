//! skill ツール。名前に完全一致すれば本文を、そうでなければ候補の一覧を返す。

use polaris_skills::Skill;

/// SKILL.md 本文を返す際の上限バイト数。Agent Skills 仕様は本文を概ね
/// 5,000 トークン未満に保ち、詳細は参照ファイルへ逃がすことを推奨している。
/// ここではトークナイザを持たないためバイト数で近似する。日本語混じりの
/// 本文では 1 トークンあたり概ね 2〜4 バイトになりやすいため、5,000 トークン
/// を厳密な下限ではなく「およそこの規模」の目安として扱い、余裕を持たせて
/// 32 KiB を上限にする —— 仕様の推奨よりも明確に緩いが、暴走した本文が
/// 会話コストを際限なく押し上げることは防ぐ。`read` が `MAX_READ_BYTES` で
/// 同じ役割を果たしているのと対をなす。
pub const MAX_BODY_BYTES: usize = 32 * 1024;

/// 検索結果として一度に返す候補の上限件数。会話履歴はターンごとにまるごと
/// 再送されるため、件数を無制限にすると skill が増えるほど毎ターンの
/// コストが際限なく増える —— 990 トークン予算が別の場所で防いでいるのと
/// 同じ種類のコストが、この経路から素通りしてしまう。20 件は、一度に
/// 見渡せる規模を残しつつ、打ち切りが起きたら明示して絞り込みを促すための
/// 目安として選んだ。
const MAX_RESULTS: usize = 20;

/// `body` を高々 `limit` バイトへ切り詰める。UTF-8 の文字境界を跨がないよう
/// 境界を後退させる。切り詰めが実際に起きたかを bool で返す。
fn cap_body(body: &str, limit: usize) -> (&str, bool) {
    if body.len() <= limit {
        return (body, false);
    }
    let mut end = limit;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    (&body[..end], true)
}

/// 候補一覧を `MAX_RESULTS` 件まで整形する。それを超えたら「打ち切った」と
/// 明示する —— 沈黙して一部だけ返すと、モデルはそれが全件だと誤解する。
fn list_candidates(items: &[&Skill], header: &str) -> String {
    let mut out = String::from(header);
    let mut shown = 0usize;
    for s in items.iter().take(MAX_RESULTS) {
        out.push_str(&format!("- {}: {}\n", s.name, s.description));
        shown += 1;
    }
    if items.len() > MAX_RESULTS {
        out.push_str(&format!(
            "(全 {} 件中 {shown} 件のみ表示。絞り込むか名前を直接渡すこと。)\n",
            items.len()
        ));
    }
    out
}

/// 与えられた語を skill 名として引き、外れたら名前と説明を検索する。
///
/// 検索が本文を返さないのは段階的開示のためである。候補を見てから読むかを
/// 決められるようにする。すべての本文を返すなら検索する意味が無い。
pub fn lookup(skills: &[Skill], q: &str) -> String {
    if skills.is_empty() {
        return "skill が 1 件も見つからない。探索先に SKILL.md が無い。".to_string();
    }

    if let Some(s) = skills.iter().find(|s| s.name == q) {
        let (body, truncated) = cap_body(&s.body, MAX_BODY_BYTES);
        let mut out = format!("# {}\n\n{}\n", s.name, body);
        if truncated {
            out.push_str(&format!(
                "\n(本文を {MAX_BODY_BYTES} バイトで打ち切った。全文が要るときは {} を直接読むこと。)\n",
                s.path.display()
            ));
        }
        return out;
    }

    // 空文字列・空白のみの q は「意図的に何も絞り込まない」問い合わせとして
    // 扱う。Rust の `str::contains` は空の針に対して常に真を返すため、下の
    // 検索へ素通しすると事実上「全 skill を返せ」になってしまう。それ自体は
    // 「何があるか見せてほしい」という妥当な要求の読み方でもあるので、黙って
    // 全件流すのではなく、そう解釈したことを明示したうえで同じ件数上限を
    // かけて返す。
    if q.trim().is_empty() {
        let all: Vec<&Skill> = skills.iter().collect();
        return list_candidates(
            &all,
            "q が空なので、存在する skill を列挙する。絞り込むには名前や語を渡すこと。\n",
        );
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
        let all: Vec<&Skill> = skills.iter().collect();
        return list_candidates(
            &all,
            &format!("{q} に当たる skill が無い。利用できるのは次のとおり。\n"),
        );
    }

    list_candidates(&hits, "候補。本文が要るときは名前をそのまま渡す。\n")
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
    fn an_empty_skill_set_is_distinguishable_from_a_query_matching_nothing() {
        // どちらも非空の文字列を返すという点だけを見れば見分けが付かない。
        // 「skill が1件も無い」ことを示す固有の文言が、単に検索が外れた場合
        // の出力には現れないことを固定する。`skills.is_empty()` の早期
        // return を削除すると、空集合への問い合わせは hits.is_empty() 経路
        // に落ち、この文言を含まない出力になるため、この違いを検出できる。
        let empty_set = lookup(&[], "何か");
        assert!(
            empty_set.contains("1 件も見つからない"),
            "skill が1件も無いことを示す文言が無い: {empty_set}"
        );

        let no_match = lookup(&fixtures(), "まったく無関係な語");
        assert!(
            !no_match.contains("1 件も見つからない"),
            "1件もヒットしなかっただけなのに「1件も無い」と言っている: {no_match}"
        );
    }

    #[test]
    fn an_empty_query_lists_what_exists_instead_of_matching_everything_silently() {
        // Rust の `"anything".contains("")` は常に真なので、空文字列を検索
        // へ素通しすると全件が「ヒット」してしまう。それを候補一覧として
        // 返すこと自体は妥当だが、本文までは含めない・そう解釈したと分かる
        // ことを固定する。
        //
        // 「名前が含まれて本文は含まれない」というだけでは、意図的な分岐を
        // 削って検索へ素通しさせても（needle が空文字なので全件が hits に
        // 入り、同じ list_candidates で整形されるため）見分けが付かない。
        // 意図的な分岐だけが出す固有の文言まで固定して、その分岐が実際に
        // 通っていることを検出できるようにする。
        let out = lookup(&fixtures(), "");
        assert!(out.contains("git-commit") && out.contains("writing-style"));
        assert!(!out.contains("本文A") && !out.contains("本文B"));
        assert!(
            out.contains("空なので"),
            "空文字列を意図的な問い合わせとして扱ったと分かる文言が無い: {out}"
        );
    }

    #[test]
    fn a_whitespace_only_query_is_treated_the_same_as_empty() {
        // 空白のみは素通しだと「候補が無い」経路（hits が空）に落ちても
        // 一覧は出るため、上と同じ理由で固有の文言まで確かめる。
        let out = lookup(&fixtures(), "   ");
        assert!(out.contains("git-commit") && out.contains("writing-style"));
        assert!(
            out.contains("空なので"),
            "空白のみを意図的な問い合わせとして扱ったと分かる文言が無い: {out}"
        );
    }

    #[test]
    fn search_results_are_capped_and_say_so_when_truncated() {
        let skills: Vec<Skill> = (0..(MAX_RESULTS + 10))
            .map(|i| Skill {
                name: format!("skill-{i:02}"),
                description: "テスト用の説明。".into(),
                body: "本文".into(),
                path: format!("/x/skill-{i:02}/SKILL.md").into(),
            })
            .collect();
        let total = skills.len();

        let out = lookup(&skills, "");
        let shown = out.lines().filter(|l| l.starts_with("- skill-")).count();
        assert_eq!(
            shown, MAX_RESULTS,
            "上限 {MAX_RESULTS} 件だけを表示すべき: {shown} 件表示された"
        );
        assert!(
            out.contains(&total.to_string()),
            "全 {total} 件のうち一部しか表示していないと分かる文言が無い: {out}"
        );
    }

    #[test]
    fn an_oversized_body_is_truncated_and_says_so() {
        let big_body = "あ".repeat(MAX_BODY_BYTES);
        let skills = vec![Skill {
            name: "big".into(),
            description: "巨大な skill".into(),
            body: big_body.clone(),
            path: "/x/big/SKILL.md".into(),
        }];

        let out = lookup(&skills, "big");
        assert!(
            out.len() < big_body.len(),
            "本文が打ち切られていない: 出力 {} バイト、本文 {} バイト",
            out.len(),
            big_body.len()
        );
        assert!(
            out.contains("打ち切"),
            "打ち切ったことを示す文言が無い: {}",
            &out[out.len().saturating_sub(120)..]
        );
    }

    #[test]
    fn a_body_within_the_cap_is_returned_whole_and_unmarked() {
        let body = "本文".repeat(10);
        let skills = vec![Skill {
            name: "small".into(),
            description: "小さい skill".into(),
            body: body.clone(),
            path: "/x/small/SKILL.md".into(),
        }];

        let out = lookup(&skills, "small");
        assert!(out.contains(&body));
        assert!(!out.contains("打ち切"));
    }
}
