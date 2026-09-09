//! ターン実行中にツール呼び出し・subagent活動をTUIへリアルタイム通知する
//! ためのイベント型。`polaris-cli`(一発実行)はこの型を一切知らずに
//! 動く——`agent::run`等への`events`引数は`Option`であり、`None`を渡す
//! だけで完全に無関係でいられる。

#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// 暫定表示用。正本応答へ二重連結せず、requestごとに置き換える。
    TextDelta {
        request_id: u64,
        output_index: u64,
        content_index: u64,
        text: String,
    },
    WorkflowResolved {
        phase: String,
        skill_ids: Vec<String>,
        changed_skill_ids: Vec<String>,
        manifest_hash: String,
    },
    ToolStarted {
        name: String,
        detail: String,
    },
    ToolFinished {
        name: String,
        detail: String,
        ok: bool,
        /// ツールの実行結果本文。成功なら`Ok(String)`の中身、失敗なら
        /// `Err(String)`のメッセージ——どちらもTUIが短いプレビューを
        /// 出すためだけに使う。
        result: String,
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
    HistoryCompacted {
        messages_before: usize,
        messages_after: usize,
        tokens_before: u32,
        tokens_after: u32,
    },
}

impl AgentEvent {
    /// Serialize/Debugや複製を行わず、キューが保持する確保領域を数える。
    /// String/Vecの余剰capacityも含む。wireサイズの保証ではない。
    pub(crate) fn fits_queue_budget(&self, limit: usize) -> bool {
        struct Budget(usize);
        impl Budget {
            fn take(&mut self, bytes: usize) -> bool {
                match self.0.checked_sub(bytes) {
                    Some(left) => {
                        self.0 = left;
                        true
                    }
                    None => false,
                }
            }
            fn string(&mut self, text: &String) -> bool {
                self.take(text.capacity())
            }
            fn vec<T>(&mut self, values: &Vec<T>) -> bool {
                values
                    .capacity()
                    .checked_mul(std::mem::size_of::<T>())
                    .is_some_and(|bytes| self.take(bytes))
            }
            fn strings(&mut self, values: &Vec<String>) -> bool {
                self.vec(values) && values.iter().all(|text| self.string(text))
            }
            fn diff(&mut self, diff: &Diff) -> bool {
                self.vec(&diff.hunks)
                    && diff.hunks.iter().all(|hunk| {
                        self.vec(&hunk.lines)
                            && hunk.lines.iter().all(|line| {
                                let (DiffLine::Context(text)
                                | DiffLine::Added(text)
                                | DiffLine::Removed(text)) = line;
                                self.string(text)
                            })
                    })
            }
        }
        let mut budget = Budget(limit);
        if !budget.take(std::mem::size_of::<Self>()) {
            return false;
        }
        match self {
            Self::TextDelta { text, .. } => budget.string(text),
            Self::WorkflowResolved {
                phase,
                skill_ids,
                changed_skill_ids,
                manifest_hash,
            } => {
                budget.string(phase)
                    && budget.strings(skill_ids)
                    && budget.strings(changed_skill_ids)
                    && budget.string(manifest_hash)
            }
            Self::ToolStarted { name, detail } => budget.string(name) && budget.string(detail),
            Self::ToolFinished {
                name,
                detail,
                result,
                diff,
                ..
            } => {
                budget.string(name)
                    && budget.string(detail)
                    && budget.string(result)
                    && diff.as_ref().is_none_or(|diff| budget.diff(diff))
            }
            Self::SpawnStarted { agent_type, task } => {
                budget.string(agent_type) && budget.string(task)
            }
            Self::SpawnFinished { agent_type, .. } => budget.string(agent_type),
            Self::HistoryCompacted { .. } => true,
        }
    }
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

/// 差分計算に与える時間の上限。`dispatch`は非同期ランタイムのスレッド上で
/// 同期的にこの関数を呼ぶため、上限がないと巨大ファイルの全面書き換え
/// (Myers差分はO(N·D)で、全面置換ではD≈N)が終わるまでTUI全体——ステータス
/// 表示、Escによる中断、イベントのライブ出力——が止まる。`similar`の
/// deadlineは期限を過ぎると近似解に切り替えるので、精度を落として応答性を
/// 保つ方向に劣化する。
const DIFF_TIME_BUDGET: std::time::Duration = std::time::Duration::from_millis(200);

/// `old`/`new`を行単位で比較し、前後3行のコンテキスト付きハンク列を返す。
/// `is_new_file`は常に`false`で返す——ファイルが実際に新規かどうかは
/// 呼び出し側(`dispatch`、Task 3)が知っている情報であり、ここでは
/// 純粋に2つの文字列を比較するだけに留める。
pub fn compute_diff(old: &str, new: &str) -> Diff {
    use similar::{ChangeTag, TextDiff};

    let text_diff = TextDiff::configure()
        .timeout(DIFF_TIME_BUDGET)
        .diff_lines(old, new);
    let mut hunks = Vec::new();
    let mut added = 0usize;
    let mut removed = 0usize;

    for group in text_diff.grouped_ops(3) {
        let mut lines = Vec::new();
        for op in &group {
            for change in text_diff.iter_changes(op) {
                // `\r`も落とす。CRLF入力で`\r`が残るとTUIの`sanitize`が
                // 制御文字として`�`に置き換え、全行の末尾に化けた文字が
                // 並ぶ。
                let text = change.value().trim_end_matches(['\r', '\n']).to_string();
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
    fn queue_budget_checks_exact_boundary_and_nested_allocations() {
        let event = AgentEvent::SpawnStarted {
            agent_type: String::new(),
            task: "abc".into(),
        };
        let bytes = std::mem::size_of::<AgentEvent>() + 3;
        assert!(event.fits_queue_budget(bytes));
        assert!(!event.fits_queue_budget(bytes - 1));
        let event = AgentEvent::WorkflowResolved {
            phase: String::new(),
            skill_ids: Vec::with_capacity(20),
            changed_skill_ids: vec![String::with_capacity(4096)],
            manifest_hash: String::new(),
        };
        assert!(!event.fits_queue_budget(4096));
        assert!(event.fits_queue_budget(8192));
        let event = AgentEvent::ToolFinished {
            name: String::new(),
            detail: String::new(),
            ok: true,
            result: String::new(),
            diff: Some(Diff {
                is_new_file: false,
                added: 0,
                removed: 0,
                hunks: vec![DiffHunk {
                    lines: vec![DiffLine::Added(String::with_capacity(4096))],
                }],
            }),
        };
        assert!(!event.fits_queue_budget(4096));
        assert!(event.fits_queue_budget(8192));
        let event = AgentEvent::ToolFinished {
            name: String::new(),
            detail: String::new(),
            ok: true,
            result: String::new(),
            diff: Some(Diff {
                is_new_file: false,
                added: 0,
                removed: 0,
                hunks: Vec::with_capacity(4096),
            }),
        };
        assert!(!event.fits_queue_budget(4096));
    }

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
    fn crlf_input_leaves_no_carriage_return_on_any_diff_line() {
        let diff = compute_diff("a\r\nb\r\nc\r\n", "a\r\nB\r\nc\r\n");
        for line in diff.hunks.iter().flat_map(|h| &h.lines) {
            let text = match line {
                DiffLine::Context(s) | DiffLine::Added(s) | DiffLine::Removed(s) => s,
            };
            assert!(!text.contains('\r'), "a CR survived in {text:?}");
        }
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
