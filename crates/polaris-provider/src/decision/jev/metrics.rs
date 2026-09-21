//! Opt-in physical Jev attempts, with a durable start before network dispatch.
//! No state, questions, answers, endpoint, or credential enters this journal.

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

pub(super) fn logical_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}-{}",
        std::process::id(),
        now_ms(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

pub(super) struct Attempt {
    file: File,
    record: Value,
    finished: bool,
}

impl Attempt {
    pub(super) fn start(
        path: &Path,
        model: &str,
        logical: &str,
        number: usize,
    ) -> io::Result<Self> {
        let file = crate::codex::metrics::private_file(path)?;
        let mut held = Self {
            file,
            finished: false,
            record: json!({
                "schema_version": 1, "provider": "jev", "event": "started",
                "model": model, "logical_id": logical,
                "attempt_id": format!("{logical}-{number}"),
                "attempt_number": number, "started_unix_ms": now_ms(),
                "finished_unix_ms": null, "http_status": null,
                "outcome": "running", "usage": {"input_tokens": null, "output_tokens": null},
            }),
        };
        held.write()?;
        Ok(held)
    }

    pub(super) fn status(&mut self, code: u16) {
        self.record["http_status"] = json!(code);
    }

    pub(super) fn usage(&mut self, input: Option<u64>, output: Option<u64>) {
        self.record["usage"] = json!({"input_tokens": input, "output_tokens": output});
    }

    pub(super) fn finish(&mut self, outcome: &'static str) -> io::Result<()> {
        self.finished = true;
        self.record["event"] = json!("finished");
        self.record["finished_unix_ms"] = json!(now_ms());
        self.record["outcome"] = json!(outcome);
        self.write()
    }

    fn write(&mut self) -> io::Result<()> {
        let mut line = serde_json::to_vec(&self.record)?;
        line.push(b'\n');
        self.file.lock()?;
        let result = self
            .file
            .write_all(&line)
            .and_then(|()| self.file.sync_all());
        result.and(self.file.unlock())
    }
}

impl Drop for Attempt {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.finish("cancelled");
        }
    }
}
