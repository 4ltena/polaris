//! 常時コンテキストの計測。数値は測定で担保し、見積で運用しない。

use polaris_provider::openai::tool_wire_shape;
use polaris_tools::ToolSpec;

/// 常時コンテキストの上限。
pub const BUDGET_LIMIT: usize = 990;

/// 常時提供するツールの上限本数。
pub const MAX_TOOLS: usize = 6;

/// 基準トークナイザで数える。プロバイダごとに実数は前後するため、
/// 予算の判定は常にこの基準で行う。
///
/// `o200k_base()` は呼ぶたびに約20万行のランクテーブルを埋め込みデータから
/// 再構築するため、呼び出し1回ごとに構築すると `cap()` の切り詰めループ
/// （行や文字ごとに再計測する）で顕著に遅い。プロセス内で1度だけ構築した
/// シングルトンを使い回す。
pub fn count_tokens(text: &str) -> usize {
    let bpe = tiktoken_rs::o200k_base_singleton();
    bpe.encode_with_special_tokens(text).len()
}

/// 毎ターン載るものの合計。システムプロンプトと、実際に送られる
/// ツール定義の直列化結果を数える。
///
/// `ToolSpec` をそのまま直列化した形ではなく、`polaris_provider::openai::
/// tool_wire_shape` が作るワイヤ形式（`{"type":"function","function":{…}}`)
/// を数える。プロバイダが実際に送るバイト列と、ここで測るバイト列が
/// 別々の場所で独立に組み立てられていたことがあり、その食い違いの分だけ
/// 予算が実態より小さく出ていた。
pub fn always_on_tokens(system_prompt: &str, tools: &[ToolSpec]) -> usize {
    let wire = tool_wire_shape(tools);
    let tools_json = serde_json::to_string(&wire).expect("ツール定義を直列化できない");
    count_tokens(system_prompt) + count_tokens(&tools_json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::SYSTEM_PROMPT;

    #[test]
    fn always_on_context_stays_within_budget() {
        let specs = polaris_tools::all_specs();
        let n = always_on_tokens(SYSTEM_PROMPT, &specs);
        assert!(
            n <= BUDGET_LIMIT,
            "常時コンテキストが {n} トークン。上限 {BUDGET_LIMIT} を超えている"
        );
    }

    #[test]
    fn tool_count_stays_within_limit() {
        let n = polaris_tools::all_specs().len();
        assert!(
            n <= MAX_TOOLS,
            "ツールが {n} 本。上限 {MAX_TOOLS} 本を超えている"
        );
    }

    #[test]
    fn always_on_tokens_counts_the_wire_shape_not_the_bare_tool_spec() {
        // `serde_json::to_string(&specs)` を直接数えると `{"type":"function",
        // "function":{...}}` の包み分だけ少なく出る — 実際に送るバイト列と
        // 計測するバイト列が別の場所で独立に組み立てられていたことによる
        // 食い違いで、この milestone が測定を組織原理とする根拠そのものを
        // 崩していた。ここで両者が一致しない（＝ここが素朴な直列化では
        // なくワイヤ形式を数えている）ことを固定する。
        let specs = polaris_tools::all_specs();
        let naive_tools_tokens = count_tokens(&serde_json::to_string(&specs).unwrap());
        let measured_tools_tokens =
            always_on_tokens(SYSTEM_PROMPT, &specs) - count_tokens(SYSTEM_PROMPT);
        assert!(
            measured_tools_tokens > naive_tools_tokens,
            "ワイヤ形式の包み分の差が無い: naive={naive_tools_tokens} measured={measured_tools_tokens}"
        );
    }

    /// 一時ディレクトリへ `n` 件の skill を実際に作って読み込み、`main.rs` と
    /// 同じ手順で常時コンテキストを組み立てて計測する。
    ///
    /// cwd とブランチ名は固定値を使う。一時ディレクトリのパスをそのまま
    /// 環境ブロックへ入れると、ランダムなディレクトリ名の長さの違いだけで
    /// トークン数が動き、「skill の数で動いたのか」を判別できなくなる。
    fn always_on_with_skills(n: usize) -> (usize, String, Vec<polaris_skills::Skill>) {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        for i in 0..n {
            let name = format!("catalog-probe-{i:03}");
            let d = dir.path().join(&name);
            std::fs::create_dir_all(&d).expect("作れない");
            std::fs::write(
                d.join("SKILL.md"),
                format!(
                    "---\nname: {name}\ndescription: 常時コンテキストへ漏れていないかを見る目印 {i:03}。\n---\n本文 {i}\n"
                ),
            )
            .expect("書けない");
        }

        let discovered = polaris_skills::discover_in(&[dir.path().to_path_buf()]);
        // 読めていなければ以下の比較は「0 件と 0 件を比べる」空振りになる。
        assert_eq!(
            discovered.skills.len(),
            n,
            "fixture の skill が {n} 件読めていない"
        );

        let env = crate::constitution::environment_block(
            std::path::Path::new("/w/polaris"),
            Some("feat/m1-headless-loop"),
        );
        let system = crate::prompt::build_system("プロジェクトの規則。", &env);
        let tokens = always_on_tokens(&system, &polaris_tools::all_specs());
        (tokens, system, discovered.skills)
    }

    #[test]
    fn the_always_on_total_does_not_move_as_the_number_of_skills_grows() {
        // 仕様のテスト戦略が名指ししている検査（「skill を15件から100件へ
        // 増やしても合計が変化しないことを確認する」）であり、受け入れ基準 1
        // が挙げる3つの入力のうち、この milestone が持ち込んだ唯一のもの。
        // 憲法の大きさと環境情報の長さには既に番人がいるが、skill の数には
        // いなかった。
        let (zero, _, _) = always_on_with_skills(0);
        let (fifteen, _, _) = always_on_with_skills(15);
        let (hundred, system, skills) = always_on_with_skills(100);

        assert_eq!(
            fifteen, hundred,
            "skill を15件から100件へ増やすと常時コンテキストが {fifteen} から {hundred} トークンへ動いた"
        );
        assert_eq!(
            zero, hundred,
            "skill が0件のときと100件のときで常時コンテキストが違う: {zero} と {hundred}"
        );
        assert!(hundred <= BUDGET_LIMIT, "上限 {BUDGET_LIMIT} を超えている");

        // 合計の一致だけでは、将来 skill 由来の文字列が入り込んでも「たまたま
        // トークン数が同じ」場合を見逃す。常時載る2つの経路（システム
        // プロンプトと、実際に送られるツール定義）に skill の名前も説明も
        // 一切現れないことを直接固定する —— 常時コンテキストへカタログ行を
        // 足す、スキーマへ skill 名の enum を入れる、という将来の変更は
        // どちらもここで落ちる。
        let wire = serde_json::to_string(&polaris_provider::openai::tool_wire_shape(
            &polaris_tools::all_specs(),
        ))
        .expect("直列化できない");
        for s in &skills {
            assert!(
                !system.contains(&s.name) && !wire.contains(&s.name),
                "skill の名前 {} が常時コンテキストへ漏れている",
                s.name
            );
            assert!(
                !system.contains(&s.description) && !wire.contains(&s.description),
                "skill の説明が常時コンテキストへ漏れている: {}",
                s.name
            );
        }
    }

    #[test]
    fn count_tokens_is_nonzero_for_nonempty_text() {
        assert!(count_tokens("hello world") > 0);
        assert_eq!(count_tokens(""), 0);
    }
}
