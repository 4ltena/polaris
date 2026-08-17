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

    #[test]
    fn count_tokens_is_nonzero_for_nonempty_text() {
        assert!(count_tokens("hello world") > 0);
        assert_eq!(count_tokens(""), 0);
    }
}
