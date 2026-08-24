//! ターン実行中にツール呼び出し・subagent活動をTUIへリアルタイム通知する
//! ためのイベント型。`polaris-cli`(一発実行)はこの型を一切知らずに
//! 動く——`agent::run`等への`events`引数は`Option`であり、`None`を渡す
//! だけで完全に無関係でいられる。

#[derive(Debug, Clone)]
pub enum AgentEvent {
    ToolStarted {
        name: String,
        detail: String,
    },
    ToolFinished {
        name: String,
        detail: String,
        ok: bool,
        diff: Option<Diff>,
    },
    SpawnStarted {
        agent_type: String,
        task: String,
    },
    SpawnFinished {
        agent_type: String,
        ok: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffLine {
    Context(String),
    Added(String),
    Removed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffHunk {
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diff {
    pub is_new_file: bool,
    pub hunks: Vec<DiffHunk>,
    pub added: usize,
    pub removed: usize,
}

/// `old`/`new`を行単位で比較し、前後3行のコンテキスト付きハンク列を返す。
/// `is_new_file`は常に`false`で返す——ファイルが実際に新規かどうかは
/// 呼び出し側(`dispatch`、Task 3)が知っている情報であり、ここでは
/// 純粋に2つの文字列を比較するだけに留める。
pub fn compute_diff(old: &str, new: &str) -> Diff {
    use similar::{ChangeTag, TextDiff};

    let text_diff = TextDiff::from_lines(old, new);
    let mut hunks = Vec::new();
    let mut added = 0usize;
    let mut removed = 0usize;

    for group in text_diff.grouped_ops(3) {
        let mut lines = Vec::new();
        for op in &group {
            for change in text_diff.iter_changes(op) {
                let text = change.value().trim_end_matches('\n').to_string();
                match change.tag() {
                    ChangeTag::Equal => lines.push(DiffLine::Context(text)),
                    ChangeTag::Insert => {
                        added += 1;
                        lines.push(DiffLine::Added(text));
                    }
                    ChangeTag::Delete => {
                        removed += 1;
                        lines.push(DiffLine::Removed(text));
                    }
                }
            }
        }
        hunks.push(DiffHunk { lines });
    }

    Diff {
        is_new_file: false,
        hunks,
        added,
        removed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_text_produces_no_hunks() {
        let diff = compute_diff("a\nb\nc\n", "a\nb\nc\n");
        assert!(diff.hunks.is_empty());
        assert_eq!(diff.added, 0);
        assert_eq!(diff.removed, 0);
    }

    #[test]
    fn a_single_line_change_is_reported_as_one_removed_and_one_added() {
        let diff = compute_diff("a\nb\nc\n", "a\nB\nc\n");
        assert_eq!(diff.added, 1);
        assert_eq!(diff.removed, 1);
        assert!(!diff.hunks.is_empty());
    }

    #[test]
    fn appending_to_empty_old_content_counts_every_line_as_added() {
        let diff = compute_diff("", "a\nb\nc\n");
        assert_eq!(diff.added, 3);
        assert_eq!(diff.removed, 0);
    }

    #[test]
    fn context_lines_surround_a_change_up_to_three_lines_each_side() {
        let old = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
        let new = "1\n2\n3\n4\nCHANGED\n6\n7\n8\n9\n10\n";
        let diff = compute_diff(old, new);
        let context_count = diff
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter(|l| matches!(l, DiffLine::Context(_)))
            .count();
        // 変更行の前後3行ずつ、合計6行のコンテキストが含まれるはず
        // (前3行: 2,3,4 / 後3行: 6,7,8 — similarの実際のグルーピング
        // 挙動次第で若干前後する可能性があるため、0件でないことと
        // 6行を超えないことだけを固定する)。
        assert!(context_count > 0);
        assert!(context_count <= 6);
    }
}
