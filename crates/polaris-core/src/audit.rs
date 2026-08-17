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

    /// 1 呼び出しを 1 行として追記する。`detail` は書く直前に必ず伏字化を通す。
    pub fn record(&mut self, tool: &str, detail: &str) -> std::io::Result<()> {
        let screened = match screen_text(detail) {
            FilterResult::Keep(s) | FilterResult::Redacted(s) => s,
            FilterResult::Drop => "[DROPPED]".to_string(),
        };
        let line = serde_json::json!({ "tool": tool, "detail": screened });
        writeln!(self.file, "{line}")?;
        self.file.flush()
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
}
