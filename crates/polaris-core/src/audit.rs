//! Append-only audit log. Not signed: in-process, the entity signing and the
//! entity acting are the same, so a signature would prove nothing beyond
//! what the log itself already shows.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;

use crate::secret_screen::{FilterResult, screen_text};

pub struct AuditLog {
    file: File,
}

#[cfg(all(test, unix))]
mod private_file_tests {
    use super::*;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    #[test]
    fn retains_open_file_when_its_name_is_replaced() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("audit");
        let file = OpenOptions::new().create_new(true).append(true).mode(0o600).open(&path).unwrap();
        let mut audit = AuditLog::from_private_file(file).unwrap();
        let retained = root.path().join("retained");
        std::fs::rename(&path, &retained).unwrap();
        std::fs::write(&path, b"replacement").unwrap();
        audit.record(&Record { tool: "test", detail: "entry", sandbox: None, target: None, result: "ok", caller: "root" }).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
        assert!(std::fs::read_to_string(retained).unwrap().contains("entry"));
    }

    #[test]
    fn refuses_nonappend_or_shared_file_without_writing() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("audit");
        let file = OpenOptions::new().create_new(true).write(true).mode(0o600).open(&path).unwrap();
        assert!(AuditLog::from_private_file(file).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(AuditLog::from_private_file(OpenOptions::new().append(true).open(&path).unwrap()).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::hard_link(&path, root.path().join("alias")).unwrap();
        assert!(AuditLog::from_private_file(OpenOptions::new().append(true).open(&path).unwrap()).is_err());
        assert_eq!(std::fs::metadata(path).unwrap().len(), 0);
    }
}

impl AuditLog {
    /// Consume an already securely opened private append-only file. The trusted
    /// caller owns path/parent validation; this entry never reopens that path.
    #[cfg(unix)]
    pub fn from_private_file(file: File) -> std::io::Result<Self> {
        use std::os::{fd::AsRawFd, unix::fs::MetadataExt};
        let metadata = file.metadata()?;
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
        if flags < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.nlink() != 1
            || metadata.mode() & 0o7777 != 0o600
            || flags & libc::O_APPEND == 0
            || !matches!(flags & libc::O_ACCMODE, libc::O_WRONLY | libc::O_RDWR)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "private append-only audit file required",
            ));
        }
        Ok(Self { file })
    }

    pub fn open(path: &Path) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self { file })
    }

    /// Appends one line. **Every string that reaches here passes through
    /// redaction immediately before being written.** Whenever a field is
    /// added, it must be passed through `screen`. Forgetting to do so is
    /// a direct path for raw credentials to fall into the log. `tool` is a
    /// value that comes straight from the model's tool call and is not a
    /// closed set (a model under prompt injection can emit an arbitrary
    /// string), so it goes through the same path as every other field,
    /// without exception.
    pub fn record(&mut self, r: &Record<'_>) -> std::io::Result<()> {
        let mut line = serde_json::json!({
            "tool": screen(r.tool),
            "detail": truncate_field(&screen(r.detail), MAX_DETAIL_BYTES),
            "result": truncate_field(&screen(r.result), MAX_RESULT_BYTES),
            "caller": screen(r.caller),
        });
        if let Some(p) = r.sandbox {
            line["sandbox"] = serde_json::Value::String(screen(&p.describe()));
        }
        if let Some(t) = r.target {
            line["target"] = serde_json::Value::String(screen(&t.display().to_string()));
        }
        writeln!(self.file, "{line}")?;
        self.file.flush()
    }
}

/// The content of one audit log line. This type carries the "kind, resolved
/// sandbox policy, write target, and result" that the spec requires.
/// `sandbox` and `target` are received as `Option` so that "absent" can be
/// distinguished from "was an empty string"; when `None`, the field is
/// omitted entirely (the job of [`AuditLog::record`]).
pub struct Record<'a> {
    pub tool: &'a str,
    pub detail: &'a str,
    pub sandbox: Option<&'a polaris_sandbox::SandboxPolicy>,
    pub target: Option<&'a Path>,
    pub result: &'a str,
    /// 呼び出し主体。ルートは `"root"`、subagent はその型名。全ての新規
    /// フィールドは screen を経由するという既存の不変条件に従い、必ず
    /// screen(r.caller) を通す。
    pub caller: &'a str,
}

/// The single gate through which every string written to the audit log
/// passes. `FilterResult::Drop` (when the entire line was nothing but a
/// secret) never writes the original string at all — it's replaced with
/// `[DROPPED]`.
fn screen(s: &str) -> String {
    match screen_text(s) {
        FilterResult::Keep(s) | FilterResult::Redacted(s) => s,
        FilterResult::Drop => "[DROPPED]".to_string(),
    }
}

/// Ceiling (in bytes) for the `result` field alone. The audit log's job is
/// to let "what was done and how it ended" be reconstructed — it is not a
/// place to duplicate the full body of a successful call in its entirety.
/// In particular, a successful `read` returns the entire file as its body,
/// so without a ceiling the audit log would become a duplicate of the
/// workspace.
///
/// `polaris_tools::bash::MAX_OUTPUT_BYTES` (32 KiB) is the ceiling on the
/// primary channel returned to the model, and that one needs to be the
/// genuine article of the response. This is a secondary record, and it only
/// needs to leave enough of a trail for a reader to reconstruct the story
/// and go look at the genuine article themselves, so we pick a smaller
/// 4 KiB here.
const MAX_RESULT_BYTES: usize = 4 * 1024;

/// Ceiling (in bytes) for the `detail` field alone. `detail` was originally
/// left unbounded on the reasoning that "it's bounded because it's an
/// argument the model sent" — that reasoning was wrong. `write`'s `content`
/// can carry an entire file, and `edit`'s `old`/`new` can also carry an
/// entire chunk of a file (see `polaris-tools::write_spec` / `edit_spec`).
/// This is structurally the same hazard as `result` on a successful `read`
/// (the entire file body), and the audit log's job here — that it suffices
/// to be able to reconstruct "what was requested" — is identical to
/// `result`'s. There's no reason to pick a different value, so we adopt the
/// same 4 KiB as `MAX_RESULT_BYTES` (the values matching is a deliberate
/// choice; the constants are kept separate so that if either one ever needs
/// to change on its own, it can be changed independently).
const MAX_DETAIL_BYTES: usize = MAX_RESULT_BYTES;

/// Truncates a field down to the ceiling. **Simply cutting at a raw byte
/// offset can split a multi-byte character in half and panic**, so we walk
/// back to a character boundary before cutting (the same approach as
/// `polaris_tools::bash::truncate`). We spell out in the body that
/// truncation happened, because an unmarked partial result presented as a
/// complete answer is a wrong answer. Both `result` and `detail` pass
/// through this (see [`AuditLog::record`]).
fn truncate_field(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[truncated here in the audit log for exceeding {} bytes; check the original tool call or target file for the full text]",
        &s[..end],
        max_bytes
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_secrets_before_writing() {
        let dir = tempfile::tempdir().expect("cannot create temp directory");
        let path = dir.path().join("audit.jsonl");
        let mut log = AuditLog::open(&path).expect("cannot open");
        log.record(&Record {
            tool: "bash",
            detail: "export API_TOKEN=sk-abcdefghijklmnopqrstuvwxyz012345",
            sandbox: None,
            target: None,
            result: "ok",
            caller: "root",
        })
        .expect("cannot write");

        let body = std::fs::read_to_string(&path).expect("cannot read");
        assert!(
            !body.contains("sk-abcdefghijklmnopqrstuvwxyz012345"),
            "the raw value survived"
        );
        assert!(body.contains("[REDACTED]"), "not redacted");
        assert!(body.contains("\"tool\":\"bash\""), "tool name is missing");
    }

    #[test]
    fn appends_one_line_per_record() {
        let dir = tempfile::tempdir().expect("cannot create temp directory");
        let path = dir.path().join("audit.jsonl");
        let mut log = AuditLog::open(&path).expect("cannot open");
        log.record(&Record {
            tool: "read",
            detail: "src/main.rs",
            sandbox: None,
            target: None,
            result: "ok",
            caller: "root",
        })
        .expect("cannot write");
        log.record(&Record {
            tool: "read",
            detail: "src/lib.rs",
            sandbox: None,
            target: None,
            result: "ok",
            caller: "root",
        })
        .expect("cannot write");

        let body = std::fs::read_to_string(&path).expect("cannot read");
        assert_eq!(body.lines().count(), 2);
    }

    #[test]
    fn redacts_secrets_in_tool_field_too() {
        // tool is a value that comes straight from the model's tool call,
        // and a model under prompt injection can emit an arbitrary string.
        // Confirm it's redacted through the same path as detail.
        let dir = tempfile::tempdir().expect("cannot create temp directory");
        let path = dir.path().join("audit.jsonl");
        let mut log = AuditLog::open(&path).expect("cannot open");
        log.record(&Record {
            tool: "export API_TOKEN=sk-abcdefghijklmnopqrstuvwxyz012345",
            detail: "harmless detail",
            sandbox: None,
            target: None,
            result: "ok",
            caller: "root",
        })
        .expect("cannot write");

        let body = std::fs::read_to_string(&path).expect("cannot read");
        assert!(
            !body.contains("sk-abcdefghijklmnopqrstuvwxyz012345"),
            "the raw value inside tool survived"
        );
        assert!(body.contains("[REDACTED]"), "tool was not redacted");
    }

    #[test]
    fn a_record_carries_the_policy_the_target_and_the_result() {
        let dir = tempfile::tempdir().expect("temp directory");
        let root = tempfile::tempdir().expect("temp directory");
        let path = dir.path().join("audit.jsonl");
        let mut log = AuditLog::open(&path).expect("cannot open");

        let policy = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::WorkspaceWrite,
            &[root.path().to_path_buf()],
        )
        .expect("policy");

        log.record(&Record {
            tool: "write",
            detail: "{\"path\":\"a.txt\"}",
            sandbox: Some(&policy),
            target: Some(std::path::Path::new("/w/a.txt")),
            result: "ok",
            caller: "root",
        })
        .expect("cannot write");

        let line = std::fs::read_to_string(&path).expect("cannot read");
        let v: serde_json::Value = serde_json::from_str(line.trim()).expect("not JSON");
        assert_eq!(v["tool"], "write");
        assert_eq!(v["result"], "ok");
        assert_eq!(v["target"], "/w/a.txt");
        assert!(
            v["sandbox"]
                .as_str()
                .expect("sandbox is missing")
                .contains("workspace-write"),
            "policy was not recorded: {v}"
        );
    }

    #[test]
    fn every_new_field_passes_through_the_secret_screen() {
        // Every time a field is added, a hole opens up where redaction can
        // be forgotten. In M1 there was a period where the tool field wasn't
        // passing through it. Here we put secret-looking strings into both
        // target and result, and check that they don't fall through as-is.
        let dir = tempfile::tempdir().expect("temp directory");
        let path = dir.path().join("audit.jsonl");
        let mut log = AuditLog::open(&path).expect("cannot open");

        let secret = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        log.record(&Record {
            tool: "bash",
            detail: "echo x",
            sandbox: None,
            target: Some(std::path::Path::new(secret)),
            result: secret,
            caller: "root",
        })
        .expect("cannot write");

        let line = std::fs::read_to_string(&path).expect("cannot read");
        assert!(
            !line.contains(secret),
            "the secret fell into the audit log raw: {line}"
        );
    }

    #[test]
    fn an_absent_policy_and_target_are_omitted_rather_than_written_as_empty() {
        // Writing an empty string would make "there is no policy"
        // indistinguishable from "the policy was an empty string".
        let dir = tempfile::tempdir().expect("temp directory");
        let path = dir.path().join("audit.jsonl");
        let mut log = AuditLog::open(&path).expect("cannot open");

        log.record(&Record {
            tool: "read",
            detail: "{}",
            sandbox: None,
            target: None,
            result: "ok",
            caller: "root",
        })
        .expect("cannot write");

        let line = std::fs::read_to_string(&path).expect("cannot read");
        let v: serde_json::Value = serde_json::from_str(line.trim()).expect("not JSON");
        assert!(v.get("sandbox").is_none() || v["sandbox"].is_null(), "{v}");
        assert!(v.get("target").is_none() || v["target"].is_null(), "{v}");
    }

    #[test]
    fn writes_dropped_marker_when_entire_detail_is_secret() {
        // When the entire line is nothing but a bare secret with no
        // surrounding context, screen_text returns FilterResult::Drop (the
        // same input as secret_screen::tests::
        // drops_bare_high_entropy_secret_with_no_surrounding_context). On
        // this branch, record never writes the original string at all — it
        // writes [DROPPED].
        let dir = tempfile::tempdir().expect("cannot create temp directory");
        let path = dir.path().join("audit.jsonl");
        let mut log = AuditLog::open(&path).expect("cannot open");
        let bare_secret = "sk-abc123DEF456ghi789XYZ000aaa111";
        assert!(matches!(screen_text(bare_secret), FilterResult::Drop));

        log.record(&Record {
            tool: "bash",
            detail: bare_secret,
            sandbox: None,
            target: None,
            result: "ok",
            caller: "root",
        })
        .expect("cannot write");

        let body = std::fs::read_to_string(&path).expect("cannot read");
        assert!(!body.contains(bare_secret), "the raw value survived");
        assert!(
            body.contains("[DROPPED]"),
            "the marker wasn't written on the Drop branch"
        );
    }

    #[test]
    fn a_result_over_the_ceiling_is_truncated_and_says_so() {
        // A successful read hands back the entire file as result. Without a
        // ceiling, the audit log would become a duplicate of the workspace.
        //
        // A run of 20+ characters with no whitespace trips screen()'s
        // high-entropy detection and gets wholesale [DROPPED] (which is
        // correct behavior in itself), so we use content that looks like a
        // file body, with words separated by whitespace.
        let dir = tempfile::tempdir().expect("temp directory");
        let path = dir.path().join("audit.jsonl");
        let mut log = AuditLog::open(&path).expect("cannot open");

        let long = "lorem ipsum dolor sit amet ".repeat(MAX_RESULT_BYTES / 10);
        log.record(&Record {
            tool: "read",
            detail: "{}",
            sandbox: None,
            target: None,
            result: &long,
            caller: "root",
        })
        .expect("cannot write");

        let line = std::fs::read_to_string(&path).expect("cannot read");
        let v: serde_json::Value = serde_json::from_str(line.trim()).expect("not JSON");
        let recorded = v["result"].as_str().expect("result is missing");
        assert!(
            recorded.len() < long.len(),
            "was not truncated: {} bytes",
            recorded.len()
        );
        assert!(
            recorded.contains("truncated"),
            "the body doesn't say it was truncated: {recorded}"
        );
    }

    #[test]
    fn truncation_lands_on_a_character_boundary() {
        // Simply cutting at a raw byte offset can split a multi-byte
        // character in half and panic. "★" is 3 bytes, so simply cutting at
        // MAX_RESULT_BYTES (4096, not a multiple of 3) is guaranteed to land
        // on a position that isn't a boundary.
        let dir = tempfile::tempdir().expect("temp directory");
        let path = dir.path().join("audit.jsonl");
        let mut log = AuditLog::open(&path).expect("cannot open");

        let long = "★".repeat(MAX_RESULT_BYTES / 3 + 10);
        log.record(&Record {
            tool: "read",
            detail: "{}",
            sandbox: None,
            target: None,
            result: &long,
            caller: "root",
        })
        .expect("cannot write (may have panicked from cutting mid-boundary)");

        let line = std::fs::read_to_string(&path).expect("cannot read");
        let v: serde_json::Value = serde_json::from_str(line.trim()).expect("not JSON");
        let recorded = v["result"].as_str().expect("result is missing");
        assert!(
            recorded.len() <= MAX_RESULT_BYTES + 200,
            "not within range of the ceiling: {} bytes",
            recorded.len()
        );
    }

    #[test]
    fn caller_distinguishes_root_from_a_subagent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let mut log = AuditLog::open(&path).unwrap();
        log.record(&Record {
            tool: "read",
            detail: "{}",
            sandbox: None,
            target: None,
            result: "ok",
            caller: "root",
        })
        .unwrap();
        log.record(&Record {
            tool: "read",
            detail: "{}",
            sandbox: None,
            target: None,
            result: "ok",
            caller: "file-inspector",
        })
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"caller\":\"root\""));
        assert!(lines[1].contains("\"caller\":\"file-inspector\""));
    }

    #[test]
    fn a_detail_over_the_ceiling_is_truncated_and_says_so() {
        // detail was originally left unbounded on the reasoning that "it's
        // bounded because it's an argument the model sent" — that reasoning
        // was wrong. write's content and edit's old/new can carry an entire
        // file. Confirm it passes through the same path as result
        // (truncate_field).
        //
        // A run of 20+ characters with no whitespace trips screen()'s
        // high-entropy detection and gets wholesale [DROPPED] (which is
        // correct behavior in itself), so we use content that looks like a
        // file body, with words separated by whitespace.
        let dir = tempfile::tempdir().expect("temp directory");
        let path = dir.path().join("audit.jsonl");
        let mut log = AuditLog::open(&path).expect("cannot open");

        let long = "lorem ipsum dolor sit amet ".repeat(MAX_DETAIL_BYTES / 10);
        log.record(&Record {
            tool: "write",
            detail: &long,
            sandbox: None,
            target: None,
            result: "ok",
            caller: "root",
        })
        .expect("cannot write");

        let line = std::fs::read_to_string(&path).expect("cannot read");
        let v: serde_json::Value = serde_json::from_str(line.trim()).expect("not JSON");
        let recorded = v["detail"].as_str().expect("detail is missing");
        assert!(
            recorded.len() < long.len(),
            "was not truncated: {} bytes",
            recorded.len()
        );
        assert!(
            recorded.contains("truncated"),
            "the body doesn't say it was truncated: {recorded}"
        );
    }
}
