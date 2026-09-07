//! Opt-in, content-free diagnostics for each actual Codex HTTP attempt.
//! `POLARIS_METRICS_PATH` is read for every HTTP attempt; absent means no file
//! or fingerprints. Existing files must already be private (0600); macOS and
//! Linux are supported. Open failures abort before send; write failures warn.
//! `POLARIS_CACHE_NAMESPACE` is captured when constructing the provider. It
//! accepts 1..=64 ASCII alphanumeric/`.`/`_`/`-` bytes. Its hash is appended to
//! the existing cache key, so equal prefixes retain equal keys within a run.
//! This is a routing hint, not a guarantee of backend cache isolation.
//! `POLARIS_CACHE_MODE` is also captured at construction: absent leaves the
//! body unchanged; `implicit`/`explicit` opt into experimental GPT-6 settings.
//! Explicit mode marks the latest four eligible user/developer/tool boundaries.
//! Older tool outputs stay normalized to arrays as the marker window advances.
//! Requests are independent: no mutable boundary state is shared by children.
//! Codex endpoint support and cache performance are unverified; no fallback.
//! `request_key` hashes the complete JSON body; `prefix` hashes instructions
//! and tools; `visible_history` excludes encrypted reasoning items. Lengths
//! are UTF-8 bytes of compact JSON, not token counts. `visible_history_items`
//! contains one hash/byte-length pair per visible wire item for longest-prefix
//! comparison without storing prior request state. Missing usage is null.
//! Fingerprints use the existing stable FNV-1a algorithm, not a secrecy primitive.
use std::ffi::OsStr;
use std::fs::File;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{Value, json};

use super::cache_prefix::CachePrefixDiagnostics;
use crate::cache_pacing::Mode as PacingMode;
use crate::turn_affinity::TurnAffinityMode;
use crate::{CompletionResponse, ProviderError};

fn fingerprint(value: &Value) -> Value {
    let bytes = value.to_string();
    json!({"hash": hash(bytes.as_bytes()), "bytes": bytes.len()})
}

fn hash(bytes: &[u8]) -> String {
    format!(
        "fnv1a64-{:016x}",
        crate::fnv1a(0xcbf2_9ce4_8422_2325, bytes)
    )
}

pub(super) fn apply_namespace(
    body: &mut Value,
    namespace: Option<&OsStr>,
) -> Result<(), ProviderError> {
    let Some(namespace) = namespace else {
        return Ok(());
    };
    let namespace = namespace.to_str().filter(|s| {
        !s.is_empty() && s.len() <= 64
            && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    }).ok_or_else(|| ProviderError::Decode(
        "POLARIS_CACHE_NAMESPACE は1〜64文字の英数字・ピリオド・下線・ハイフンで指定してください".into()
    ))?;
    let original = body["prompt_cache_key"].as_str().unwrap_or_default();
    body["prompt_cache_key"] = json!(format!("{original}-{}", hash(namespace.as_bytes())));
    Ok(())
}

/// Experimental wire shape from the official prompt-caching guide:
/// https://developers.openai.com/api/docs/guides/prompt-caching
pub(super) fn apply_cache_mode(
    body: &mut Value,
    mode: Option<&OsStr>,
) -> Result<(), ProviderError> {
    let Some(mode) = mode else { return Ok(()) };
    let mode = mode
        .to_str()
        .filter(|mode| matches!(*mode, "implicit" | "explicit"))
        .ok_or_else(|| {
            ProviderError::Decode(
                "POLARIS_CACHE_MODE は implicit または explicit を指定してください".into(),
            )
        })?;
    let model = body["model"].as_str().unwrap_or_default();
    if model != "gpt-6" && !model.starts_with("gpt-6-") {
        return Err(ProviderError::Decode(
            "POLARIS_CACHE_MODE は実験用のGPT-6モデル限定設定です".into(),
        ));
    }
    body["prompt_cache_options"] = json!({"mode": mode});
    if mode == "implicit" {
        return Ok(());
    }
    let Some(items) = body["input"].as_array_mut() else {
        return Ok(());
    };
    // Reconstruct boundaries from this request only, so appends, forks and
    // compaction cannot accidentally reuse another child's boundary indices.
    // Keep at most four markers. Eviction beyond this window is intentional.
    let mut boundaries = std::collections::VecDeque::with_capacity(4);
    for (index, item) in items.iter_mut().enumerate() {
        let field = if item["type"] == "function_call_output" {
            if let Some(text) = item["output"].as_str() {
                item["output"] = json!([{"type": "input_text", "text": text}]);
            }
            "output"
        } else if item["role"] == "user" || item["role"] == "developer" {
            "content"
        } else {
            continue;
        };
        if let Some(parts) = item[field].as_array_mut() {
            // Input to this helper normally comes freshly from build_body.
            // Removing markers makes repeat application bounded and idempotent.
            for part in parts.iter_mut() {
                if let Some(object) = part.as_object_mut() {
                    object.remove("prompt_cache_breakpoint");
                }
            }
            if let Some(part_index) = parts
                .iter()
                .rposition(|p| p["type"] == "input_text" && p["text"].is_string())
            {
                if boundaries.len() == 4 {
                    boundaries.pop_front();
                }
                boundaries.push_back((index, field, part_index));
            }
        }
    }
    for (index, field, part_index) in boundaries {
        items[index][field][part_index]["prompt_cache_breakpoint"] = json!({"mode": "explicit"});
    }
    Ok(())
}

// reqwest does not expose TLS/DNS-specific predicates. Inspect its source
// chain only to choose a fixed allowlisted label; never persist source text.
// Text-based TLS/DNS recognition is best effort, not a definitive diagnosis.
pub(super) fn transport_cause(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        return "timeout";
    }
    let cause = source_cause(error);
    if cause != "unknown" {
        return cause;
    }
    if error.is_connect() {
        "connect"
    } else if error.is_body() {
        "body_transport"
    } else if error.is_request() {
        "request_transport"
    } else {
        "unknown"
    }
}

fn source_cause(error: &(dyn std::error::Error + 'static)) -> &'static str {
    let mut current = Some(error);
    let mut found = "unknown";
    // Bound traversal even for an unusual cyclic source implementation.
    for _ in 0..16 {
        let Some(error) = current else { break };
        let text = error.to_string().to_ascii_lowercase();
        if text.contains("certificate") || text.contains("tls") || text.contains("ssl") {
            return "tls";
        }
        if text.contains("proxy") || text.contains("tunnel") {
            found = "proxy";
        }
        if text.contains("dns")
            || text.contains("name resolution")
            || text.contains("lookup address")
        {
            found = "dns";
        }
        if let Some(io) = error.downcast_ref::<io::Error>() {
            let label = match io.kind() {
                io::ErrorKind::ConnectionRefused => "connection_refused",
                io::ErrorKind::ConnectionReset => "connection_reset",
                io::ErrorKind::ConnectionAborted => "connection_aborted",
                io::ErrorKind::NotConnected => "not_connected",
                io::ErrorKind::NetworkUnreachable => "network_unreachable",
                io::ErrorKind::HostUnreachable => "host_unreachable",
                io::ErrorKind::PermissionDenied => "permission_denied",
                io::ErrorKind::TimedOut => "timeout",
                _ => "unknown",
            };
            if label != "unknown" && found == "unknown" {
                found = label;
            }
        }
        current = error.source();
    }
    found
}

#[derive(Clone, Copy, Default, Serialize)]
pub(super) struct TokenUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
}

impl TokenUsage {
    pub(super) fn parse(v: &Value) -> Self {
        Self {
            input_tokens: v.get("input_tokens").and_then(Value::as_u64),
            output_tokens: v.get("output_tokens").and_then(Value::as_u64),
            cache_read_tokens: v
                .pointer("/input_tokens_details/cached_tokens")
                .and_then(Value::as_u64),
            cache_write_tokens: v
                .get("cache_write_tokens")
                .and_then(Value::as_u64)
                .or_else(|| {
                    v.pointer("/input_tokens_details/cache_write_tokens")
                        .and_then(Value::as_u64)
                }),
        }
    }
}

// No paths or underlying error strings are surfaced: either may contain secrets.
fn metrics_error() -> ProviderError {
    ProviderError::Decode(
        "POLARIS_METRICS_PATH の診断ファイルを安全に開けません（通常ファイル・権限0600が必要）"
            .into(),
    )
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn private_file(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    // Refuse special files before opening. O_NOFOLLOW closes the final-symlink
    // race; O_NONBLOCK avoids blocking if a regular file is replaced by a FIFO.
    if let Ok(meta) = std::fs::symlink_metadata(path)
        && !meta.is_file()
    {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    #[cfg(target_os = "linux")]
    const FLAGS: i32 = 0x20000 | 0x800;
    #[cfg(target_os = "macos")]
    const FLAGS: i32 = 0x100 | 0x4;
    let file = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(FLAGS)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.permissions().mode() & 0o777 != 0o600 || meta.nlink() != 1 {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(file)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn private_file(_path: &Path) -> io::Result<File> {
    // Do not silently produce a world-readable file on platforms without mode 0600.
    Err(io::ErrorKind::Unsupported.into())
}

pub(super) struct RequestMetric {
    file: File,
    start: Instant,
    record: Value,
    pub(super) usage: TokenUsage,
    pub(super) http_status: Option<u16>,
    pub(super) server_cancelled: bool,
    pub(super) transport_cause: Option<&'static str>,
    outcome: &'static str,
    dispatched: bool,
}

impl RequestMetric {
    pub(super) fn from_env(
        body: &Value,
        cache_prefix: CachePrefixDiagnostics,
    ) -> Result<Option<Self>, ProviderError> {
        std::env::var_os("POLARIS_METRICS_PATH")
            .map(|path| Self::new_with_cache_prefix(Path::new(&path), body, cache_prefix))
            .transpose()
    }

    #[cfg(test)]
    fn new(path: &Path, body: &Value) -> Result<Self, ProviderError> {
        // Existing metrics-only tests do not model a provider profile. Keep
        // that local fixture path explicit; live attempts always use
        // `new_with_cache_prefix` above.
        Self::new_with_cache_prefix(
            path,
            body,
            CachePrefixDiagnostics {
                profile: "compact",
                version: "compact-v1",
                guide_hash: crate::fnv1a(0xcbf2_9ce4_8422_2325, b""),
                target_applied: false,
            },
        )
    }

    fn new_with_cache_prefix(
        path: &Path,
        body: &Value,
        cache_prefix: CachePrefixDiagnostics,
    ) -> Result<Self, ProviderError> {
        let file = private_file(path).map_err(|_| metrics_error())?;
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let mut visible = body["input"].clone();
        if let Some(items) = visible.as_array_mut() {
            items.retain(|item| item["type"] != "reasoning");
        }
        let visible_items: Vec<Value> = visible
            .as_array()
            .into_iter()
            .flatten()
            .map(fingerprint)
            .collect();
        let prefix = json!({"instructions": body["instructions"], "tools": body["tools"]});
        Ok(Self {
            file,
            start: Instant::now(),
            usage: TokenUsage::default(),
            http_status: None,
            server_cancelled: false,
            transport_cause: None,
            outcome: "cancelled",
            dispatched: true,
            record: json!({
                "schema_version": 1, "provider": "codex", "started_unix_ms": timestamp,
                "attempt_id": format!("{}-{timestamp}-{}", std::process::id(), SEQUENCE.fetch_add(1, Ordering::Relaxed)),
                "model": body["model"], "effort": body.pointer("/reasoning/effort"),
                "cache_mode": body.pointer("/prompt_cache_options/mode"),
                "cache_prefix": {
                    "profile": cache_prefix.profile,
                    "version": cache_prefix.version,
                    "hash": format!("fnv1a64-{:016x}", cache_prefix.guide_hash),
                    "target_applied": cache_prefix.target_applied,
                },
                "request_key": fingerprint(body)["hash"],
                "prompt_cache_key": body["prompt_cache_key"],
                "request": fingerprint(body), "visible_history": fingerprint(&visible),
                "prefix": fingerprint(&prefix), "history_items": visible.as_array().map(Vec::len),
                "visible_history_items": visible_items,
            }),
        })
    }

    pub(super) fn defer_dispatch(&mut self) {
        self.dispatched = false;
    }

    pub(super) fn start_dispatch(&mut self, dispatch: &crate::cache_pacing::Dispatch) {
        self.start = dispatch.started;
        self.record["started_unix_ms"] = json!(dispatch.unix_ms);
        self.dispatched = true;
    }

    pub(super) fn finish(&mut self, result: &Result<CompletionResponse, ProviderError>) {
        self.outcome = match result {
            _ if self.server_cancelled => "cancelled",
            Ok(_) => "completed",
            Err(ProviderError::Auth(_)) => "auth_error",
            Err(ProviderError::Http(_)) => "http_error",
            Err(ProviderError::Decode(_)) => "decode_error",
        };
    }

    pub(super) fn turn_affinity(
        &mut self,
        mode: TurnAffinityMode,
        context_present: bool,
        sent: bool,
        received: bool,
    ) {
        self.record["turn_affinity"] = json!({
            "mode": mode.as_str(),
            "context_present": context_present,
            "sent": sent,
            "received": received,
        });
    }

    pub(super) fn cache_pacing(
        &mut self,
        mode: PacingMode,
        interval: std::time::Duration,
        wait: std::time::Duration,
        offset: std::time::Duration,
    ) {
        self.record["cache_pacing"] = json!({"mode": mode.as_str(), "interval_ms": interval.as_millis() as u64, "wait_ms": wait.as_millis() as u64, "dispatch_offset_ms": offset.as_millis() as u64});
    }

    fn write(&mut self) -> io::Result<()> {
        self.record["elapsed_ms"] = json!(self.start.elapsed().as_millis());
        self.record["outcome"] = json!(self.outcome);
        self.record["transport_cause"] = json!(self.transport_cause);
        self.record["http_status"] = json!(self.http_status);
        self.record["usage"] = json!(self.usage);
        self.record["usage_missing"] =
            json!(self.usage.input_tokens.is_none() || self.usage.output_tokens.is_none());
        let mut line = serde_json::to_vec(&self.record)?;
        line.push(b'\n');
        // Shared children/threads and separate benchmark processes append whole
        // lines under the same advisory lock. No mutex is held across await.
        self.file.lock()?;
        let result = self.file.write_all(&line).and_then(|()| self.file.flush());
        let unlocked = self.file.unlock();
        result.and(unlocked)
    }
}

impl Drop for RequestMetric {
    fn drop(&mut self) {
        if self.dispatched && self.write().is_err() {
            eprintln!("Polaris: 診断ログの保存に失敗しました。今回の計測には欠測があります。");
        }
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use crate::{CompletionRequest, Message};
    use std::os::unix::fs::PermissionsExt;

    struct LogPath(std::path::PathBuf);
    impl LogPath {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "polaris-metrics-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            Self(path)
        }
        fn records(&self) -> Vec<Value> {
            std::fs::read_to_string(&self.0)
                .unwrap()
                .lines()
                .map(|s| serde_json::from_str(s).unwrap())
                .collect()
        }
    }
    impl Drop for LogPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    fn body() -> Value {
        super::super::build_body(
            "gpt-6-astra",
            &CompletionRequest {
                system: "SECRET_SYSTEM".into(),
                messages: vec![
                    Message::user("SECRET_PROMPT"),
                    Message::tool_result("call", "SECRET_TOOL"),
                ],
                tools: vec![],
            },
            Some("medium"),
        )
    }

    #[test]
    fn cache_pacing_cancelled_before_dispatch_does_not_log_a_model_request() {
        let path = LogPath::new();
        let mut metric = RequestMetric::new(&path.0, &body()).unwrap();
        metric.defer_dispatch();
        drop(metric);
        assert!(path.records().is_empty());
    }

    #[test]
    fn cache_pacing_dispatch_resets_timing_and_preserves_error_outcome() {
        let path = LogPath::new();
        let mut metric = RequestMetric::new(&path.0, &body()).unwrap();
        metric.defer_dispatch();
        let dispatch = crate::cache_pacing::Dispatch {
            wait: std::time::Duration::from_secs(5),
            offset: std::time::Duration::from_secs(10),
            started: Instant::now(),
            unix_ms: 123456,
        };
        metric.start_dispatch(&dispatch);
        assert_eq!(metric.start, dispatch.started);
        metric.cache_pacing(
            PacingMode::On,
            crate::cache_pacing::INTERVAL,
            dispatch.wait,
            dispatch.offset,
        );
        metric.finish(&Err(ProviderError::Auth("expired".into())));
        drop(metric);
        let rows = path.records();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["started_unix_ms"], 123456);
        assert_eq!(rows[0]["outcome"], "auth_error");
        assert_eq!(
            rows[0]["cache_pacing"],
            json!({
                "mode": "on", "interval_ms": 5000, "wait_ms": 5000, "dispatch_offset_ms": 10000,
            })
        );
    }

    #[test]
    fn cache_mode_absent_preserves_serialized_body_and_implicit_only_adds_mode() {
        let original = body();
        let bytes = serde_json::to_vec(&original).unwrap();
        let mut request = original.clone();
        apply_cache_mode(&mut request, None).unwrap();
        assert_eq!(serde_json::to_vec(&request).unwrap(), bytes);
        apply_cache_mode(&mut request, Some(OsStr::new("implicit"))).unwrap();
        assert_eq!(request["prompt_cache_options"], json!({"mode":"implicit"}));
        request
            .as_object_mut()
            .unwrap()
            .remove("prompt_cache_options");
        assert_eq!(request, original);
    }

    #[test]
    fn cache_mode_explicit_preserves_past_boundaries_across_appends() {
        let mut request = body();
        apply_cache_mode(&mut request, Some(OsStr::new("explicit"))).unwrap();
        let past = request["input"].as_array().unwrap().clone();
        assert_eq!(
            past[0]["content"][0]["prompt_cache_breakpoint"],
            json!({"mode":"explicit"})
        );
        assert_eq!(
            past[1]["output"][0],
            json!({"type":"input_text", "text":"SECRET_TOOL", "prompt_cache_breakpoint":{"mode":"explicit"}})
        );
        let mut next = body();
        next["input"]
            .as_array_mut()
            .unwrap()
            .extend(super::super::input_items(&[
                Message::assistant("answer"),
                Message::user("next"),
                Message::tool_result("call2", "result2"),
            ]));
        apply_cache_mode(&mut next, Some(OsStr::new("explicit"))).unwrap();
        assert_eq!(
            &next["input"].as_array().unwrap()[..past.len()],
            past.as_slice()
        );
        assert_eq!(
            next["input"][4]["output"][0]["prompt_cache_breakpoint"],
            json!({"mode":"explicit"})
        );
        assert_eq!(next["reasoning"], request["reasoning"]);
        assert_eq!(next["prompt_cache_key"], request["prompt_cache_key"]);
        let unchanged = next.clone();
        apply_cache_mode(&mut next, Some(OsStr::new("explicit"))).unwrap();
        assert_eq!(next, unchanged);
    }

    #[test]
    fn cache_mode_explicit_keeps_only_last_four_boundaries() {
        let mut request = body();
        request["input"] = json!(super::super::input_items(
            &(0..7)
                .map(|i| Message::tool_result(format!("c{i}"), format!("r{i}")))
                .collect::<Vec<_>>()
        ));
        apply_cache_mode(&mut request, Some(OsStr::new("explicit"))).unwrap();
        for (i, item) in request["input"].as_array().unwrap().iter().enumerate() {
            assert_eq!(item["output"][0]["text"], format!("r{i}"));
            assert_eq!(
                item["output"][0].get("prompt_cache_breakpoint").is_some(),
                i >= 3
            );
        }
    }

    #[test]
    fn cache_mode_invalid_config_and_non_gpt6_fail_without_body_change() {
        for invalid in ["", "EXPLICIT", "auto", "SECRET_INVALID"] {
            let mut request = body();
            let before = request.clone();
            let error = apply_cache_mode(&mut request, Some(OsStr::new(invalid))).unwrap_err();
            assert!(error.to_string().contains("POLARIS_CACHE_MODE"));
            assert!(!error.to_string().contains("SECRET"));
            assert_eq!(request, before);
        }
        let mut request = body();
        request["model"] = json!("gpt-5.6-sol");
        let before = request.clone();
        assert!(apply_cache_mode(&mut request, Some(OsStr::new("explicit"))).is_err());
        assert_eq!(request, before);
    }

    #[test]
    fn cache_mode_is_recorded_in_metrics_including_absent() {
        let path = LogPath::new();
        for mode in [None, Some("implicit"), Some("explicit")] {
            let mut request = body();
            apply_cache_mode(&mut request, mode.map(OsStr::new)).unwrap();
            drop(RequestMetric::new(&path.0, &request).unwrap());
        }
        let rows = path.records();
        assert_eq!(rows[0]["cache_mode"], Value::Null);
        assert_eq!(rows[1]["cache_mode"], "implicit");
        assert_eq!(rows[2]["cache_mode"], "explicit");
    }

    #[test]
    fn cache_prefix_diagnostics_record_identifiers_not_the_guide() {
        let path = LogPath::new();
        let guide = super::super::cache_prefix::stable_guide();
        drop(
            RequestMetric::new_with_cache_prefix(
                &path.0,
                &body(),
                CachePrefixDiagnostics {
                    profile: "stable",
                    version: "stable-v1",
                    guide_hash: crate::fnv1a(0xcbf2_9ce4_8422_2325, guide.as_bytes()),
                    target_applied: true,
                },
            )
            .unwrap(),
        );
        let row = &path.records()[0];
        assert_eq!(row["cache_prefix"]["profile"], "stable");
        assert_eq!(row["cache_prefix"]["version"], "stable-v1");
        assert!(
            row["cache_prefix"]["hash"]
                .as_str()
                .unwrap()
                .starts_with("fnv1a64-")
        );
        assert_eq!(row["cache_prefix"]["target_applied"], true);
        assert!(!std::fs::read_to_string(&path.0).unwrap().contains(guide));
    }

    #[test]
    fn metrics_concurrent_appends_remain_valid_jsonl() {
        let path = LogPath::new();
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    for _ in 0..8 {
                        drop(RequestMetric::new(&path.0, &body()).unwrap());
                    }
                });
            }
        });
        assert_eq!(path.records().len(), 32);
    }

    #[test]
    fn metrics_write_failure_is_detected() {
        let path = LogPath::new();
        let mut guard = RequestMetric::new(&path.0, &body()).unwrap();
        guard.file = File::open(&path.0).unwrap();
        assert!(guard.write().is_err());
    }

    #[test]
    fn metrics_transport_classification_never_returns_source_text() {
        for (message, expected) in [
            ("invalid peer certificate: SECRET", "tls"),
            ("dns error: SECRET", "dns"),
            ("unsuccessful tunnel: SECRET", "proxy"),
            ("SECRET response body", "unknown"),
        ] {
            assert_eq!(source_cause(&io::Error::other(message)), expected);
        }
        assert_eq!(
            source_cause(&io::Error::from(io::ErrorKind::PermissionDenied)),
            "permission_denied"
        );
        assert_eq!(
            source_cause(&io::Error::from(io::ErrorKind::ConnectionRefused)),
            "connection_refused"
        );
    }

    #[test]
    fn metrics_usage_preserves_unknown_zero_and_large_values() {
        let usage = TokenUsage::parse(
            &json!({"input_tokens": 5_000_000_000u64, "output_tokens": 0,
            "input_tokens_details": {"cached_tokens": 0}}),
        );
        assert_eq!(usage.input_tokens, Some(5_000_000_000));
        assert_eq!(usage.output_tokens, Some(0));
        assert_eq!(usage.cache_read_tokens, Some(0));
        assert_eq!(usage.cache_write_tokens, None);
        assert_eq!(
            TokenUsage::parse(&json!({"cache_write_tokens": 42})).cache_write_tokens,
            Some(42)
        );
        assert_eq!(
            TokenUsage::parse(&json!({"input_tokens_details": {"cache_write_tokens": 7}}))
                .cache_write_tokens,
            Some(7)
        );
        assert_eq!(
            TokenUsage::parse(&json!({"input_tokens": -1})).input_tokens,
            None
        );
    }

    #[test]
    fn metrics_jsonl_is_private_content_free_and_one_line_per_attempt() {
        let path = LogPath::new();
        let request = body();
        for status in [401, 200] {
            let mut guard = RequestMetric::new(&path.0, &request).unwrap();
            guard.http_status = Some(status);
            if status == 401 {
                guard.finish(&Err(ProviderError::Auth("SECRET_AUTH_RESPONSE".into())));
            } else {
                guard.usage = TokenUsage::parse(&json!({"input_tokens": 12, "output_tokens": 3}));
                guard.finish(&Ok(CompletionResponse {
                    text: "SECRET_RESPONSE".into(),
                    tool_calls: vec![],
                    reasoning: vec![],
                    usage: None,
                }));
            }
        }
        let rows = path.records();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["outcome"], "auth_error");
        assert_eq!(rows[0]["usage_missing"], true);
        assert_eq!(rows[1]["outcome"], "completed");
        assert_eq!(rows[1]["usage_missing"], false);
        assert_eq!(rows[1]["usage"]["cache_read_tokens"], Value::Null);
        assert_eq!(rows[1]["model"], "gpt-6-astra");
        assert_eq!(rows[1]["effort"], "medium");
        assert_eq!(rows[0]["request_key"], rows[1]["request_key"]);
        assert_ne!(rows[0]["attempt_id"], rows[1]["attempt_id"]);
        assert!(rows[1]["elapsed_ms"].as_u64().is_some());
        assert!(!std::fs::read_to_string(&path.0).unwrap().contains("SECRET"));
        assert_eq!(
            std::fs::metadata(&path.0).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn metrics_rejects_insecure_files_and_symlinks_without_writes() {
        let path = LogPath::new();
        std::fs::write(&path.0, "unchanged").unwrap();
        std::fs::set_permissions(&path.0, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(RequestMetric::new(&path.0, &body()).is_err());
        let link = LogPath::new();
        std::os::unix::fs::symlink(&path.0, &link.0).unwrap();
        assert!(RequestMetric::new(&link.0, &body()).is_err());
        assert_eq!(std::fs::read_to_string(&path.0).unwrap(), "unchanged");
        assert!(
            RequestMetric::new(&path.0.join("SECRET_PATH"), &body())
                .err()
                .unwrap()
                .to_string()
                .find("SECRET")
                .is_none()
        );
    }

    #[test]
    fn metrics_fingerprints_track_history_and_keep_prefix_stable() {
        let path = LogPath::new();
        let mut request = body();
        drop(RequestMetric::new(&path.0, &request).unwrap());
        request["input"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type": "message", "content": "more"}));
        drop(RequestMetric::new(&path.0, &request).unwrap());
        request["input"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type": "reasoning", "encrypted_content": "opaque"}));
        drop(RequestMetric::new(&path.0, &request).unwrap());
        let rows = path.records();
        assert_eq!(rows[0]["prefix"], rows[1]["prefix"]);
        assert_ne!(rows[0]["visible_history"], rows[1]["visible_history"]);
        assert_eq!(rows[1]["visible_history"], rows[2]["visible_history"]);
        let original_items = rows[0]["visible_history_items"].as_array().unwrap();
        let appended_items = rows[1]["visible_history_items"].as_array().unwrap();
        assert_eq!(original_items.len(), 2);
        assert_eq!(appended_items.len(), 3);
        assert_eq!(&appended_items[..2], original_items.as_slice());
        assert_eq!(
            rows[1]["visible_history_items"],
            rows[2]["visible_history_items"]
        );
        for item in appended_items {
            assert_eq!(item.as_object().unwrap().len(), 2);
            assert!(item["hash"].as_str().unwrap().starts_with("fnv1a64-"));
            assert!(item["bytes"].as_u64().unwrap() > 0);
        }
        assert_ne!(rows[1]["request_key"], rows[2]["request_key"]);
        request["input"][1]["output"] = json!("changed tool result");
        drop(RequestMetric::new(&path.0, &request).unwrap());
        let updated = path.records();
        let changed_items = updated[3]["visible_history_items"].as_array().unwrap();
        let common = appended_items
            .iter()
            .zip(changed_items)
            .take_while(|(a, b)| a == b)
            .count();
        assert_eq!(common, 1);
    }

    #[test]
    fn metrics_namespace_is_bounded_stable_and_opt_in() {
        let original = body();
        let mut request = original.clone();
        apply_namespace(&mut request, None).unwrap();
        assert_eq!(request, original);
        apply_namespace(&mut request, Some(OsStr::new("run-1"))).unwrap();
        let mut same = original.clone();
        apply_namespace(&mut same, Some(OsStr::new("run-1"))).unwrap();
        assert_eq!(same, request);
        let mut different = original.clone();
        apply_namespace(&mut different, Some(OsStr::new("run-2"))).unwrap();
        assert_ne!(different["prompt_cache_key"], request["prompt_cache_key"]);
        for invalid in ["", "bad/name", &"a".repeat(65)] {
            assert!(apply_namespace(&mut original.clone(), Some(OsStr::new(invalid))).is_err());
        }
        assert_eq!(request["input"], original["input"]);
        assert!(
            !request["prompt_cache_key"]
                .as_str()
                .unwrap()
                .contains("run-1")
        );
    }

    #[tokio::test]
    async fn metrics_dropped_future_records_cancellation_once() {
        let path = LogPath::new();
        let guard = RequestMetric::new(&path.0, &body()).unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _guard = guard;
            tx.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        rx.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let rows = path.records();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["outcome"], "cancelled");
        assert_eq!(rows[0]["usage"]["input_tokens"], Value::Null);
    }

    #[test]
    fn metrics_folder_observes_partial_usage_on_server_failure() {
        for kind in [
            "response.failed",
            "response.cancelled",
            "response.completed",
        ] {
            let mut folder = super::super::Folder::new();
            let event = json!({"type": kind, "response": {"usage": {"output_tokens": 2}, "error": {"message": "SECRET_BODY"}}});
            let result = folder.push(format!("data: {event}\n\n").as_bytes());
            assert_eq!(result.is_ok(), kind == "response.completed");
            assert_eq!(folder.metrics_usage.output_tokens, Some(2));
            assert_eq!(folder.metrics_usage.input_tokens, None);
            assert_eq!(folder.cancelled, kind == "response.cancelled");
        }
    }
}
