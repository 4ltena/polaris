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

/// 候補一覧の出力全体に許す上限バイト数。
///
/// `MAX_RESULTS` が縛るのは件数だけである。`description` は仕様上 1,024
/// 文字まで許され、日本語なら 1 件で 3 KB に達するため、20 件そろうと
/// 約 61 KB になる —— 同じファイルが 1 件の本文に課している
/// `MAX_BODY_BYTES`（32 KiB）の倍を、より緩い根拠で通してしまう。候補
/// 一覧は「どれを読むかを選ぶための目次」であって読み物ではないので、
/// 目次が本文の上限を超えることはない。本文上限の 1/4 にあたる 8 KiB を
/// 上限とし、超える分は件数上限と同じ文言で打ち切ったことを明示する。
const MAX_LIST_BYTES: usize = 8 * 1024;

/// 一致しなかったときに文言へ差し戻すクエリの上限バイト数。
///
/// クエリはモデルが書いた任意長の文字列で、この文言はツール結果として
/// 会話履歴に残り、以後のターンで毎回再送される。このファイルで唯一
/// 上限の無い入力だった。何を探したのかが分かれば足りるので短くてよい。
const MAX_ECHOED_QUERY_BYTES: usize = 120;

/// `text` を高々 `limit` バイトへ切り詰める。UTF-8 の文字境界を跨がないよう
/// 境界を後退させる。切り詰めが実際に起きたかを bool で返す。
fn cap_bytes(text: &str, limit: usize) -> (&str, bool) {
    if text.len() <= limit {
        return (text, false);
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

/// 候補一覧を `MAX_RESULTS` 件かつ `MAX_LIST_BYTES` バイトまで整形する。
/// どちらかで打ち切ったら「打ち切った」と明示する —— 沈黙して一部だけ
/// 返すと、モデルはそれが全件だと誤解する。
fn list_candidates(items: &[&Skill], header: &str) -> String {
    let mut out = String::from(header);
    let mut shown = 0usize;
    for s in items.iter().take(MAX_RESULTS) {
        let line = format!("- {}: {}\n", s.name, s.description);
        // 1 件目だけは上限を超えても出す。1 件も出さずに「打ち切った」と
        // だけ返すと、モデルには次に打つ手が何も残らない。したがって出力は
        // 高々 `MAX_LIST_BYTES` + 見出し + 候補 1 件分に収まる。
        if shown > 0 && out.len() + line.len() > MAX_LIST_BYTES {
            break;
        }
        out.push_str(&line);
        shown += 1;
    }
    if shown < items.len() {
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

    // 前後の空白を一度だけ落とし、以降の空判定・完全一致判定・部分一致
    // 判定すべてで同じ値を使う。ここで trim した値と別の場所で untrimmed
    // な q を使うと、前後に空白が付いた完全一致クエリが一致判定をすり抜け
    // てしまう。
    let q = q.trim();

    if let Some(s) = skills.iter().find(|s| s.name == q) {
        let (body, truncated) = cap_bytes(&s.body, MAX_BODY_BYTES);
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
    if q.is_empty() {
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
        // q はモデルが書いた任意長の文字列である。そのまま差し戻すと、
        // 上限の無い入力が上限の無い出力になり、しかも履歴に残って毎ターン
        // 再送される。何を探したのかが伝わる長さで切り、切ったと断る。
        let (echoed, truncated) = cap_bytes(q, MAX_ECHOED_QUERY_BYTES);
        let header = if truncated {
            format!(
                "{echoed}…（クエリが長いので先頭 {MAX_ECHOED_QUERY_BYTES} バイトのみ表示）に当たる skill が無い。利用できるのは次のとおり。\n"
            )
        } else {
            format!("{echoed} に当たる skill が無い。利用できるのは次のとおり。\n")
        };
        return list_candidates(&all, &header);
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
    fn a_padded_query_finds_the_same_skill_as_the_unpadded_query() {
        // 空判定は q.trim() で行うのに、直前の完全一致判定は untrimmed の
        // q をそのまま使っていた。前後に空白が付いた完全一致クエリは
        // 一致判定に落ち、空判定にも当たらず、素通りして検索へ流れ込み
        // 「一致なし」の候補一覧に化けてしまう。trim を一度だけ行い、
        // 空判定にも一致判定にも同じ値を使うことを固定する。
        let unpadded = lookup(&fixtures(), "git-commit");
        let padded = lookup(&fixtures(), " git-commit ");
        assert_eq!(
            padded, unpadded,
            "前後の空白を trim せずに一致判定している: {padded}"
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
    fn search_results_are_capped_by_bytes_not_only_by_count() {
        // 仕様が `description` に許す上限は 1,024 文字。日本語なら 1 件で
        // 約 3 KB になり、`MAX_RESULTS` の 20 件がそろうと約 61 KB
        // —— 同じファイルが 1 件の本文に課している 32 KiB の倍が、
        // 件数しか見ない上限の隙間から素通りする。
        let description = "あ".repeat(1024);
        let skills: Vec<Skill> = (0..MAX_RESULTS)
            .map(|i| Skill {
                name: format!("fat-{i:02}"),
                description: description.clone(),
                body: "本文".into(),
                path: format!("/x/fat-{i:02}/SKILL.md").into(),
            })
            .collect();
        let one_entry = format!("- fat-00: {description}\n").len();

        let out = lookup(&skills, "");
        let shown = out.lines().filter(|l| l.starts_with("- fat-")).count();

        assert!(
            shown < skills.len(),
            "バイト数の上限が効いていない: {shown} 件すべてを表示した"
        );
        assert!(
            out.len() <= MAX_LIST_BYTES + one_entry + 256,
            "候補一覧が上限を超えている: {} バイト",
            out.len()
        );
        assert!(
            out.len() < MAX_BODY_BYTES,
            "候補一覧が 1 件の本文に許した上限より大きい: {} バイト",
            out.len()
        );
        assert!(
            out.contains("件のみ表示"),
            "一部しか表示していないと分かる文言が無い: {out}"
        );
    }

    #[test]
    fn a_single_candidate_over_the_byte_cap_is_still_returned() {
        // 上限を理由に 1 件も出さずに「打ち切った」とだけ返すと、モデルには
        // 次に打つ手が何も残らない。1 件目は必ず出す。
        let skills = vec![Skill {
            name: "huge".into(),
            description: "説".repeat(MAX_LIST_BYTES),
            body: "本文".into(),
            path: "/x/huge/SKILL.md".into(),
        }];

        let out = lookup(&skills, "");
        assert!(out.contains("- huge:"), "候補が 1 件も出ていない");
        assert!(
            !out.contains("件のみ表示"),
            "全件表示したのに打ち切ったと言っている: {}",
            &out[..out.len().min(200)]
        );
    }

    #[test]
    fn a_no_match_message_does_not_echo_the_query_back_unbounded() {
        // q はモデルが書いた任意長の文字列で、この文言は履歴に残って以後
        // 毎ターン再送される。このファイルで唯一、上限の無い入力だった。
        let q = "見つからない語".repeat(2000);
        let out = lookup(&fixtures(), &q);

        assert!(
            out.len() < 1024,
            "クエリをそのまま差し戻している: 出力 {} バイト、クエリ {} バイト",
            out.len(),
            q.len()
        );
        assert!(
            out.contains("クエリが長いので"),
            "クエリを切り詰めたと分かる文言が無い: {out}"
        );
        assert!(
            out.contains("git-commit") && out.contains("writing-style"),
            "候補一覧が出ていない: {out}"
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
