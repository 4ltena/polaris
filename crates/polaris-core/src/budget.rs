//! 常時コンテキストの計測。数値は測定で担保し、見積で運用しない。

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
pub fn always_on_tokens(system_prompt: &str, tools: &[ToolSpec]) -> usize {
    let tools_json = serde_json::to_string(tools).expect("ツール定義を直列化できない");
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
    fn count_tokens_is_nonzero_for_nonempty_text() {
        assert!(count_tokens("hello world") > 0);
        assert_eq!(count_tokens(""), 0);
    }
}
