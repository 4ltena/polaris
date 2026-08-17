//! 追記専用の監査ログ。署名は付けない。インプロセスでは署名する主体と
//! 行為する主体が同一であり、署名はログ以上のことを証明しないため。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;

use crate::secret_screen::{FilterResult, screen_text};

pub struct AuditLog {
    file: File,
}

impl AuditLog {
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self { file })
    }

    /// 1 呼び出しを 1 行として追記する。書き込む文字列は `tool` / `detail`
    /// のどちらも、書く直前に必ず [`screen`] を通す。`tool` はモデルの
    /// ツール呼び出しからそのまま渡ってくる値であり、閉じた集合ではない
    /// （プロンプトインジェクションを受けたモデルが任意の文字列を出せる）
    /// ため、例外なく同じ経路を通す。
    pub fn record(&mut self, tool: &str, detail: &str) -> std::io::Result<()> {
        let line = serde_json::json!({ "tool": screen(tool), "detail": screen(detail) });
        writeln!(self.file, "{line}")?;
        self.file.flush()
    }
}

/// 監査ログへ書くあらゆる文字列が通る唯一の関門。`FilterResult::Drop`
/// （行全体が丸ごとシークレットだった場合）は元の文字列を一切書かず
/// `[DROPPED]` に置き換える。
fn screen(s: &str) -> String {
    match screen_text(s) {
        FilterResult::Keep(s) | FilterResult::Redacted(s) => s,
        FilterResult::Drop => "[DROPPED]".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_secrets_before_writing() {
        let dir = tempfile::tempdir().expect("一時ディレクトリを作れない");
        let path = dir.path().join("audit.jsonl");
        let mut log = AuditLog::open(&path).expect("開けない");
        log.record(
            "bash",
            "export API_TOKEN=sk-abcdefghijklmnopqrstuvwxyz012345",
        )
        .expect("書けない");

        let body = std::fs::read_to_string(&path).expect("読めない");
        assert!(
            !body.contains("sk-abcdefghijklmnopqrstuvwxyz012345"),
            "生の値が残っている"
        );
        assert!(body.contains("[REDACTED]"), "伏字化されていない");
        assert!(body.contains("\"tool\":\"bash\""), "ツール名が無い");
    }

    #[test]
    fn appends_one_line_per_record() {
        let dir = tempfile::tempdir().expect("一時ディレクトリを作れない");
        let path = dir.path().join("audit.jsonl");
        let mut log = AuditLog::open(&path).expect("開けない");
        log.record("read", "src/main.rs").expect("書けない");
        log.record("read", "src/lib.rs").expect("書けない");

        let body = std::fs::read_to_string(&path).expect("読めない");
        assert_eq!(body.lines().count(), 2);
    }

    #[test]
    fn redacts_secrets_in_tool_field_too() {
        // tool はモデルのツール呼び出しからそのまま渡ってくる値であり、
        // プロンプトインジェクションを受けたモデルが任意の文字列を出しうる。
        // detail と同じ経路で伏字化されることを確認する。
        let dir = tempfile::tempdir().expect("一時ディレクトリを作れない");
        let path = dir.path().join("audit.jsonl");
        let mut log = AuditLog::open(&path).expect("開けない");
        log.record(
            "export API_TOKEN=sk-abcdefghijklmnopqrstuvwxyz012345",
            "harmless detail",
        )
        .expect("書けない");

        let body = std::fs::read_to_string(&path).expect("読めない");
        assert!(
            !body.contains("sk-abcdefghijklmnopqrstuvwxyz012345"),
            "tool 内の生の値が残っている"
        );
        assert!(body.contains("[REDACTED]"), "tool が伏字化されていない");
    }

    #[test]
    fn writes_dropped_marker_when_entire_detail_is_secret() {
        // 行全体が裸のシークレットのみで、周囲の文脈が無い場合
        // screen_text は FilterResult::Drop を返す（secret_screen::tests::
        // drops_bare_high_entropy_secret_with_no_surrounding_context と同じ
        // 入力）。record はこの分岐で元の文字列を一切書かず [DROPPED] を書く。
        let dir = tempfile::tempdir().expect("一時ディレクトリを作れない");
        let path = dir.path().join("audit.jsonl");
        let mut log = AuditLog::open(&path).expect("開けない");
        let bare_secret = "sk-abc123DEF456ghi789XYZ000aaa111";
        assert!(matches!(screen_text(bare_secret), FilterResult::Drop));

        log.record("bash", bare_secret).expect("書けない");

        let body = std::fs::read_to_string(&path).expect("読めない");
        assert!(!body.contains(bare_secret), "生の値が残っている");
        assert!(
            body.contains("[DROPPED]"),
            "Drop 分岐でマーカーが書かれていない"
        );
    }
}
