//! Strict ten-turn preparation over the shared v2 session store.
//!
//! This module deliberately owns no session state. The session layer appends raw
//! events first; `prepare` then uses short locks only to snapshot and publish.

use super::{publish_summary_snapshot, published_view};
use crate::conversation_state::{
    ConversationSnapshot, ConversationStore, RawEventV2, content_hash,
};
use polaris_memory::{
    MemoryStore,
    conversation::{
        ConversationHit, ConversationQuery, EmbeddingMetadata, PendingSummary, QueryEmbedding,
        Scope, SourceMetadata,
    },
};
use polaris_provider::Message;
use std::{
    collections::BTreeMap,
    future::Future,
    io,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::Duration,
};

const SUMMARY_MODEL: &str = "gpt-6-astra";
const SUMMARY_EFFORT: &str = "medium";
const SUMMARY_PROMPT_VERSION: &str = "strict10-v1";
const MAX_SOURCE_BYTES: usize = 4 * 1024;
const MAX_SOURCE_TOKENS: usize = 1024;
const MAX_HELPER_OUTPUT_BYTES: u64 = 256 * 1024;
const MAX_HELPER_ERROR_BYTES: u64 = 16 * 1024;

pub type StrictFuture<'a, T> = Pin<Box<dyn Future<Output = io::Result<T>> + Send + 'a>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingModel {
    pub model: String,
    pub revision: String,
    pub dimension: i64,
}

#[derive(Debug, Clone)]
pub struct SummaryRequest {
    pub source_turn_id: u64,
    pub messages: Vec<Message>,
    pub model: &'static str,
    pub effort: &'static str,
    pub prompt_version: &'static str,
}

/// A separately configured provider. The caller supplies this from its dedicated
/// gpt-6-astra/medium client; no global model selection is consulted here.
pub trait StrictSummaryProvider: Send + Sync {
    fn summarize<'a>(&'a self, request: SummaryRequest) -> StrictFuture<'a, String>;
}

pub trait StrictEmbedder: Send + Sync {
    fn metadata(&self) -> &EmbeddingModel;
    fn embed_passage<'a>(&'a self, text: &'a str) -> StrictFuture<'a, Vec<f32>>;
    /// A query can become several tokenizer-bounded chunks. Every returned
    /// vector is searched and the caller merges the resulting ranked hits.
    fn embed_query<'a>(&'a self, text: &'a str) -> StrictFuture<'a, Vec<Vec<f32>>>;
}

#[derive(Debug, Clone)]
pub struct PreparedHistory {
    pub snapshot: ConversationSnapshot,
    pub messages: Vec<Message>,
    pub retrieval: Vec<ConversationHit>,
    pub published_generation: u64,
    pub published_raw_hash: String,
}

#[derive(Debug, Clone)]
pub struct StrictRecovery {
    pub snapshot: ConversationSnapshot,
    pub published_generation: u64,
    pub published_raw_hash: String,
}

pub struct StrictHistory {
    summary: Arc<dyn StrictSummaryProvider>,
    embedder: Arc<dyn StrictEmbedder>,
}

impl StrictHistory {
    pub fn new(summary: Arc<dyn StrictSummaryProvider>, embedder: Arc<dyn StrictEmbedder>) -> Self {
        Self { summary, embedder }
    }

    /// Assumes the current user message was durably appended by Session first.
    /// Any persistence, provider, embedding, or CAS failure aborts preparation.
    pub async fn prepare(
        &self,
        store: &ConversationStore,
        database: &Path,
        query: &str,
    ) -> io::Result<PreparedHistory> {
        self.prepare_observed(store, database, query, &|_| Ok(()))
            .await
    }

    pub async fn prepare_observed(
        &self,
        store: &ConversationStore,
        database: &Path,
        query: &str,
        published: &(dyn Fn(&ConversationSnapshot) -> io::Result<()> + Send + Sync),
    ) -> io::Result<PreparedHistory> {
        if query.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "strict10 query is empty",
            ));
        }
        self.recover_pending_observed(store, database, published)
            .await?;
        let snapshot = store.lock()?.snapshot()?;
        let view = published_view(&snapshot.state)?;
        let vectors = self.embedder.embed_query(query).await?;
        if vectors.is_empty() {
            return Err(io::Error::other(
                "embedding helper returned no query vector",
            ));
        }
        let metadata = self.embedder.metadata().clone();
        let mut hits = BTreeMap::<(String, String, i64, i64, i64), ConversationHit>::new();
        let memory = MemoryStore::open(database).map_err(io::Error::other)?;
        for values in vectors {
            validate_vector(&values, &metadata)?;
            let found = memory
                .search_conversation(ConversationQuery {
                    published: &view,
                    keywords: query,
                    embedding: QueryEmbedding {
                        model: metadata.model.clone(),
                        revision: metadata.revision.clone(),
                        dimension: metadata.dimension,
                        values,
                    },
                })
                .map_err(io::Error::other)?;
            for hit in found {
                let key = (
                    hit.scope.session_id.clone(),
                    hit.source.id.clone(),
                    hit.scope.epoch,
                    hit.source.start_turn,
                    hit.source.end_turn,
                );
                match hits.get(&key) {
                    Some(old) if old.score >= hit.score => {}
                    _ => {
                        hits.insert(key, hit);
                    }
                }
            }
        }
        let mut retrieval: Vec<_> = hits.into_values().collect();
        retrieval.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
        let tokenizer = tiktoken_rs::o200k_base_singleton();
        let mut tokens: usize = 0;
        let mut bounded = Vec::new();
        for hit in retrieval {
            let count = tokenizer.encode_with_special_tokens(&hit.render()).len();
            if bounded.len() == 3 || tokens.saturating_add(count) > 768 {
                continue;
            }
            tokens += count;
            bounded.push(hit);
        }
        let retrieval = bounded;
        let final_snapshot = store.lock()?.snapshot()?;
        if !same_snapshot(&snapshot, &final_snapshot) {
            return Err(io::Error::other(
                "conversation changed while strict10 query embedding ran",
            ));
        }
        let mut final_memory = MemoryStore::open(database).map_err(io::Error::other)?;
        final_memory
            .with_conversation_marker(&view, || Ok(()))
            .map_err(io::Error::other)?;
        Ok(PreparedHistory {
            messages: final_snapshot.recent_messages(),
            snapshot: final_snapshot,
            retrieval,
            published_generation: snapshot.state.generation,
            published_raw_hash: snapshot.raw_hash,
        })
    }

    /// Resume-time recovery. Call before appending a new user input so a pending
    /// result names the same complete raw snapshot and can be published without a
    /// repeat summary request. It does not issue retrieval queries.
    pub async fn recover_pending(
        &self,
        store: &ConversationStore,
        database: &Path,
    ) -> io::Result<StrictRecovery> {
        self.recover_pending_observed(store, database, &|_| Ok(()))
            .await
    }

    pub async fn recover_pending_observed(
        &self,
        store: &ConversationStore,
        database: &Path,
        published: &(dyn Fn(&ConversationSnapshot) -> io::Result<()> + Send + Sync),
    ) -> io::Result<StrictRecovery> {
        loop {
            let snapshot = store.lock()?.snapshot()?;
            let Some(turn) = unsummarized_expired_turn(&snapshot)? else {
                return Ok(StrictRecovery {
                    snapshot: snapshot.clone(),
                    published_generation: snapshot.state.generation,
                    published_raw_hash: snapshot.raw_hash,
                });
            };
            let committed = self
                .summarize_and_publish(store, database, snapshot, turn)
                .await?;
            // No await between publication and accepting its exact generation.
            published(&committed)?;
        }
    }

    async fn summarize_and_publish(
        &self,
        store: &ConversationStore,
        database: &Path,
        snapshot: ConversationSnapshot,
        turn: u64,
    ) -> io::Result<ConversationSnapshot> {
        snapshot.validate_complete_turn(turn)?;
        let generation = snapshot
            .state
            .generation
            .checked_add(1)
            .ok_or_else(|| io::Error::other("conversation generation overflow"))?;
        let scope = Scope {
            project_id: snapshot.state.project_id.clone(),
            session_id: snapshot.state.session_id.clone(),
            epoch: snapshot
                .state
                .epoch
                .try_into()
                .map_err(|_| io::Error::other("epoch overflow"))?,
            generation: generation
                .try_into()
                .map_err(|_| io::Error::other("generation overflow"))?,
        };
        let suffix = &snapshot.raw_hash[..16];
        let id = format!("strict10-{turn}-{suffix}");
        let reusable = MemoryStore::open(database)
            .map_err(io::Error::other)?
            .pending_summary(&scope, &id)
            .map_err(io::Error::other)?;
        let pending = match reusable {
            Some(item) => item,
            None => {
                let messages = turn_messages(&snapshot, turn)?;
                let summary = self
                    .summary
                    .summarize(SummaryRequest {
                        source_turn_id: turn,
                        messages,
                        model: SUMMARY_MODEL,
                        effort: SUMMARY_EFFORT,
                        prompt_version: SUMMARY_PROMPT_VERSION,
                    })
                    .await?;
                if summary.trim().is_empty() {
                    return Err(io::Error::other(
                        "summary provider returned an empty summary",
                    ));
                }
                validate_summary(&summary, turn)?;
                let values = self.embedder.embed_passage(&summary).await?;
                let metadata = self.embedder.metadata().clone();
                validate_vector(&values, &metadata)?;
                let summary_input_hash = content_hash(summary.as_bytes());
                PendingSummary {
                    scope,
                    id: id.clone(),
                    source: SourceMetadata {
                        id: format!("source-{turn}-{suffix}"),
                        start_turn: turn
                            .try_into()
                            .map_err(|_| io::Error::other("turn overflow"))?,
                        end_turn: turn
                            .try_into()
                            .map_err(|_| io::Error::other("turn overflow"))?,
                        raw_hash: snapshot.raw_hash.clone(),
                    },
                    summary_hash: content_hash(summary.as_bytes()),
                    summary,
                    model: SUMMARY_MODEL.into(),
                    effort: SUMMARY_EFFORT.into(),
                    prompt_version: SUMMARY_PROMPT_VERSION.into(),
                    embedding: EmbeddingMetadata {
                        model: metadata.model,
                        revision: metadata.revision,
                        dimension: metadata.dimension,
                        input_hash: summary_input_hash,
                        values,
                    },
                }
            }
        };
        publish_summary_snapshot(store, database, &snapshot, &pending)
    }
}

fn unsummarized_expired_turn(snapshot: &ConversationSnapshot) -> io::Result<Option<u64>> {
    let view = published_view(&snapshot.state)?;
    for turn in snapshot.expired_turns() {
        let id_prefix = format!("strict10-{turn}-");
        // IDs are deterministic from the raw hash. A prior publication is visible
        // only through the marker, so this is safe across restart and retries.
        if !view.visible_ids.iter().any(|id| id.starts_with(&id_prefix)) {
            return Ok(Some(turn));
        }
        snapshot.validate_complete_turn(turn)?;
    }
    Ok(None)
}

fn turn_messages(snapshot: &ConversationSnapshot, turn: u64) -> io::Result<Vec<Message>> {
    snapshot.validate_complete_turn(turn)?;
    Ok(snapshot
        .events
        .iter()
        .filter(|event| event.epoch == snapshot.state.epoch && event.turn_id == turn)
        .map(|event| event.message.clone())
        .collect())
}

fn validate_vector(values: &[f32], metadata: &EmbeddingModel) -> io::Result<()> {
    if values.len() != usize::try_from(metadata.dimension).unwrap_or(0)
        || values.iter().any(|value| !value.is_finite())
        || values.iter().map(|value| value * value).sum::<f32>() <= 0.0
    {
        return Err(io::Error::other(
            "embedding helper returned an invalid vector",
        ));
    }
    Ok(())
}

fn same_snapshot(left: &ConversationSnapshot, right: &ConversationSnapshot) -> bool {
    left.state == right.state
        && left.raw_hash == right.raw_hash
        && left.raw_offset == right.raw_offset
        && serde_json::to_vec(&left.events).ok() == serde_json::to_vec(&right.events).ok()
}

fn validate_summary(summary: &str, turn: u64) -> io::Result<()> {
    let tokens = tiktoken_rs::o200k_base_singleton()
        .encode_with_special_tokens(summary)
        .len();
    if tokens > 256 {
        return Err(io::Error::other("summary exceeds 256 tokens"));
    }
    let value: serde_json::Value =
        serde_json::from_str(summary).map_err(|_| io::Error::other("summary is not valid JSON"))?;
    let object = value
        .as_object()
        .ok_or_else(|| io::Error::other("summary JSON is not an object"))?;
    for field in [
        "facts",
        "decisions",
        "constraints",
        "corrections",
        "open_items",
    ] {
        let entries = object
            .get(field)
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| io::Error::other("summary JSON shape is invalid"))?;
        if entries
            .iter()
            .any(|entry| entry.as_str().is_none_or(|text| text.trim().is_empty()))
        {
            return Err(io::Error::other("summary contains invalid evidence"));
        }
    }
    let ids = object
        .get("source_turn_ids")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| io::Error::other("summary source IDs are invalid"))?;
    if ids.is_empty() || ids.iter().any(|id| id.as_u64() != Some(turn)) {
        return Err(io::Error::other(
            "summary source IDs do not match the expired turn",
        ));
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct ResolvedConversationSource {
    pub text: String,
    pub source_id: String,
    pub start_turn: u64,
    pub end_turn: u64,
}

/// Resolves only `conversation://SOURCE_ID?start=N&end=M`; no filesystem path is
/// accepted. The memory index fixes scope/hash before the immutable snapshot opens.
pub fn read_source(store: &ConversationStore, database: &Path, uri: &str) -> io::Result<String> {
    Ok(resolve_conversation_uri(store, database, uri)?.text)
}

pub fn resolve_conversation_uri(
    store: &ConversationStore,
    database: &Path,
    uri: &str,
) -> io::Result<ResolvedConversationSource> {
    let (source_id, requested, offset) = parse_uri(uri)?;
    let current = store.lock()?.snapshot()?;
    let view = published_view(&current.state)?;
    let memory = MemoryStore::open(database).map_err(io::Error::other)?;
    memory
        .with_conversation_source(&view, &source_id, |reference| {
            let source_store = ConversationStore::open(
                store.data_root().map_err(polaris_memory::Error::Io)?,
                &reference.scope.project_id,
                &reference.scope.session_id,
            )
            .map_err(polaris_memory::Error::Io)?;
            let generation: u64 =
                reference.scope.generation.try_into().map_err(|_| {
                    polaris_memory::Error::InvalidInput("source generation is invalid")
                })?;
            let snapshot_generation =
                generation
                    .checked_sub(1)
                    .ok_or(polaris_memory::Error::InvalidInput(
                        "source generation is invalid",
                    ))?;
            let snapshot = source_store
                .load_snapshot(snapshot_generation, &reference.source.raw_hash)
                .map_err(polaris_memory::Error::Io)?;
            let (start, end) =
                requested.unwrap_or((reference.source.start_turn, reference.source.end_turn));
            if start < reference.source.start_turn || end > reference.source.end_turn || start > end
            {
                return Err(polaris_memory::Error::InvalidInput(
                    "requested source range is outside the indexed source",
                ));
            }
            let events: Vec<&RawEventV2> = snapshot
                .events
                .iter()
                .filter(|event| {
                    event.epoch == reference.scope.epoch as u64
                        && event.turn_id >= start as u64
                        && event.turn_id <= end as u64
                })
                .collect();
            if events.is_empty() {
                return Err(polaris_memory::Error::InvalidInput(
                    "indexed source has no matching raw events",
                ));
            }
            let payload = serde_json::to_string(&events)
                .map_err(|_| polaris_memory::Error::InvalidInput("source serialization failed"))?;
            let text = source_page(&reference.source.id, start, end, offset, &payload)
                .map_err(polaris_memory::Error::Io)?;
            Ok(ResolvedConversationSource {
                text,
                source_id: reference.source.id,
                start_turn: start as u64,
                end_turn: end as u64,
            })
        })
        .map_err(io::Error::other)
}

type ParsedSourceUri = (String, Option<(i64, i64)>, usize);
fn parse_uri(uri: &str) -> io::Result<ParsedSourceUri> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "conversation URI is invalid");
    let rest = uri.strip_prefix("conversation://").ok_or_else(invalid)?;
    let (source, query) = rest.split_once('?').unwrap_or((rest, ""));
    if source.is_empty()
        || !source
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(invalid());
    }
    let (mut start, mut end, mut offset) = (None, None, None);
    if !query.is_empty() {
        for pair in query.split('&') {
            let (key, value) = pair.split_once('=').ok_or_else(invalid)?;
            match key {
                "start" if start.is_none() => {
                    start = Some(value.parse::<i64>().map_err(|_| invalid())?)
                }
                "end" if end.is_none() => end = Some(value.parse::<i64>().map_err(|_| invalid())?),
                "offset" if offset.is_none() => {
                    offset = Some(value.parse::<usize>().map_err(|_| invalid())?)
                }
                _ => return Err(invalid()),
            }
        }
    }
    let range = match (start, end) {
        (None, None) => None,
        (Some(a), Some(b)) if a >= 0 && b >= a => Some((a, b)),
        _ => return Err(invalid()),
    };
    Ok((source.into(), range, offset.unwrap_or(0)))
}

fn source_page(
    source: &str,
    start: i64,
    end: i64,
    offset: usize,
    payload: &str,
) -> io::Result<String> {
    if offset >= payload.len() || !payload.is_char_boundary(offset) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source offset is invalid",
        ));
    }
    let mut stop = payload.len().min(offset.saturating_add(MAX_SOURCE_BYTES));
    while !payload.is_char_boundary(stop) {
        stop -= 1;
    }
    loop {
        let next = if stop < payload.len() {
            format!("conversation://{source}?start={start}&end={end}&offset={stop}")
        } else {
            "none".into()
        };
        let page = format!(
            "historical evidence source_id={source} turns={start}-{end} bytes={offset}-{stop}/{} next={next}\n{}",
            payload.len(),
            &payload[offset..stop]
        );
        if page.len() <= MAX_SOURCE_BYTES
            && tiktoken_rs::o200k_base_singleton()
                .encode_with_special_tokens(&page)
                .len()
                <= MAX_SOURCE_TOKENS
        {
            return Ok(page);
        }
        if stop == offset {
            return Err(io::Error::other("source metadata exceeds recovery budget"));
        }
        // Remove a bounded chunk, always at a UTF-8 boundary. Continuation uses
        // immutable payload byte offsets, so pages never skip or duplicate data.
        stop = offset + (stop - offset).saturating_sub(64);
        while !payload.is_char_boundary(stop) {
            stop -= 1;
        }
    }
}

/// Fixed-argv, offline stdio configuration. The runtime is intentionally not
/// auto-discovered or downloaded. Parent creates the helper in its private run dir.
#[derive(Debug, Clone)]
pub struct LocalStdioEmbedder {
    metadata: EmbeddingModel,
    pub python: PathBuf,
    pub helper: PathBuf,
    pub model_path: PathBuf,
    pub run_temp: PathBuf,
    pub cpu_threads: usize,
    pub timeout: Duration,
    pub attempt_ledger: Option<polaris_provider::attempts::AttemptLedger>,
}

impl LocalStdioEmbedder {
    pub fn new(
        python: PathBuf,
        helper: PathBuf,
        model_path: PathBuf,
        run_temp: PathBuf,
        revision: String,
        cpu_threads: usize,
        timeout: Duration,
    ) -> io::Result<Self> {
        if !python.is_absolute()
            || !helper.is_absolute()
            || !model_path.is_absolute()
            || !run_temp.is_absolute()
            || revision.is_empty()
            || cpu_threads == 0
            || timeout.is_zero()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "local embedding configuration is invalid",
            ));
        }
        Ok(Self {
            metadata: EmbeddingModel {
                model: "intfloat/multilingual-e5-small".into(),
                revision,
                dimension: 384,
            },
            python,
            helper,
            model_path,
            run_temp,
            cpu_threads,
            timeout,
            attempt_ledger: None,
        })
    }
}

async fn read_bounded<R>(reader: R, limit: u64) -> io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut bytes = Vec::new();
    reader.take(limit + 1).read_to_end(&mut bytes).await?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::other(
            "embedding helper stream exceeds the fixed limit",
        ));
    }
    Ok(bytes)
}

impl StrictEmbedder for LocalStdioEmbedder {
    fn metadata(&self) -> &EmbeddingModel {
        &self.metadata
    }
    fn embed_passage<'a>(&'a self, text: &'a str) -> StrictFuture<'a, Vec<f32>> {
        Box::pin(async move {
            let vectors = self.call("passage", text).await?;
            if vectors.len() != 1 {
                return Err(io::Error::other("passage embedding was split or malformed"));
            }
            Ok(vectors.into_iter().next().expect("checked vector count"))
        })
    }
    fn embed_query<'a>(&'a self, text: &'a str) -> StrictFuture<'a, Vec<Vec<f32>>> {
        Box::pin(async move { self.call("query", text).await })
    }
}

impl LocalStdioEmbedder {
    async fn call(&self, kind: &str, text: &str) -> io::Result<Vec<Vec<f32>>> {
        let mut attempt = self
            .attempt_ledger
            .as_ref()
            .map(|l| l.begin_embedding(&self.metadata.model))
            .transpose()
            .map_err(io::Error::other)?;
        let result = self.call_inner(kind, text).await;
        if let Some(attempt) = &mut attempt {
            attempt.finish(
                result.as_ref().ok().map(|(_, tokens)| *tokens),
                result.is_ok(),
            );
        }
        result.map(|(vectors, _)| vectors)
    }

    async fn call_inner(&self, kind: &str, text: &str) -> io::Result<(Vec<Vec<f32>>, u32)> {
        if text.is_empty() || text.len() > 64 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "embedding input is outside the fixed limit",
            ));
        }
        let request = serde_json::json!({"kind": kind, "text": text}).to_string();
        use std::process::Stdio;
        use tokio::{io::AsyncWriteExt, process::Command};
        // Each helper gets a fresh private directory. `TempDir` removes only this
        // call's files on success, timeout, error, or future cancellation.
        let call_temp = tempfile::Builder::new()
            .prefix("embedding-call-")
            .tempdir_in(&self.run_temp)?;
        let deadline = tokio::time::Instant::now() + self.timeout;
        let mut child = Command::new(&self.python)
            .arg("-I")
            .arg("-B")
            .arg(&self.helper)
            .arg("--model-path")
            .arg(&self.model_path)
            .arg("--revision")
            .arg(&self.metadata.revision)
            .arg("--dimension")
            .arg("384")
            .arg("--max-tokens")
            .arg("512")
            .kill_on_drop(true)
            .env_clear()
            .env("LANG", "C.UTF-8")
            .env("LC_ALL", "C.UTF-8")
            .env("PATH", "/usr/bin:/bin")
            .env("TMPDIR", call_temp.path())
            .env("TRANSFORMERS_OFFLINE", "1")
            .env("HF_HUB_OFFLINE", "1")
            .env("PYTHONNOUSERSITE", "1")
            .env("TOKENIZERS_PARALLELISM", "false")
            .env("OMP_NUM_THREADS", self.cpu_threads.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("embedding stdin is unavailable"))?;
        let write = async {
            stdin.write_all(format!("{request}\n").as_bytes()).await?;
            stdin.shutdown().await
        };
        match tokio::time::timeout_at(deadline, write).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                child.kill().await?;
                return Err(error);
            }
            Err(_) => {
                child.kill().await?;
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "embedding helper stdin timed out and was reaped",
                ));
            }
        }
        drop(stdin);
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("embedding stdout is unavailable"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("embedding stderr is unavailable"))?;
        let out_task = tokio::spawn(read_bounded(stdout, MAX_HELPER_OUTPUT_BYTES));
        let err_task = tokio::spawn(read_bounded(stderr, MAX_HELPER_ERROR_BYTES));
        let status = tokio::select! {
            result = child.wait() => result?,
            _ = tokio::time::sleep_until(deadline) => {
                child.kill().await?;
                let _ = out_task.await;
                let _ = err_task.await;
                return Err(io::Error::new(io::ErrorKind::TimedOut, "embedding helper timed out and was reaped"));
            }
        };
        let output = out_task
            .await
            .map_err(|_| io::Error::other("embedding stdout task failed"))??;
        let stderr = err_task
            .await
            .map_err(|_| io::Error::other("embedding stderr task failed"))??;
        if !status.success() {
            return Err(io::Error::other(format!(
                "embedding helper exited unsuccessfully: {}",
                String::from_utf8_lossy(&stderr)
            )));
        }
        let output = String::from_utf8(output)
            .map_err(|_| io::Error::other("embedding helper stdout is not UTF-8"))?;
        let response: serde_json::Value = serde_json::from_str(output.trim())
            .map_err(|_| io::Error::other("embedding helper returned invalid JSON"))?;
        let vectors = response
            .get("vectors")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| io::Error::other("embedding helper response has no vectors"))?
            .iter()
            .map(|row| {
                row.as_array()
                    .ok_or_else(|| io::Error::other("embedding vector is invalid"))
                    .and_then(|row| {
                        row.iter()
                            .map(|value| {
                                value
                                    .as_f64()
                                    .map(|value| value as f32)
                                    .ok_or_else(|| io::Error::other("embedding value is invalid"))
                            })
                            .collect()
                    })
            })
            .collect::<io::Result<Vec<Vec<f32>>>>()?;
        if vectors.len() > 32 {
            return Err(io::Error::other(
                "embedding helper returned too many vectors",
            ));
        }
        for values in &vectors {
            validate_vector(values, &self.metadata)?;
        }
        let tokens = response
            .get("input_tokens")
            .and_then(serde_json::Value::as_u64)
            .filter(|n| *n > 0 && *n <= 32 * 512)
            .ok_or_else(|| io::Error::other("embedding token usage is missing or invalid"))?
            as u32;
        Ok((vectors, tokens))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation_state::ConversationStateV2;

    const ID: &str = "00000000-0000-4000-8000-000000000001";

    struct Summary;
    impl StrictSummaryProvider for Summary {
        fn summarize<'a>(&'a self, request: SummaryRequest) -> StrictFuture<'a, String> {
            Box::pin(async move {
                Ok(format!(
                    r#"{{"facts":["remember {}"],"decisions":[],"constraints":[],"corrections":[],"open_items":[],"source_turn_ids":[{}]}}"#,
                    request.source_turn_id, request.source_turn_id
                ))
            })
        }
    }
    struct Embedder {
        model: EmbeddingModel,
    }
    impl Embedder {
        fn new() -> Self {
            Self {
                model: EmbeddingModel {
                    model: "fixture".into(),
                    revision: "v1".into(),
                    dimension: 384,
                },
            }
        }
    }
    impl StrictEmbedder for Embedder {
        fn metadata(&self) -> &EmbeddingModel {
            &self.model
        }
        fn embed_passage<'a>(&'a self, _text: &'a str) -> StrictFuture<'a, Vec<f32>> {
            Box::pin(async { Ok(vec![1.0; 384]) })
        }
        fn embed_query<'a>(&'a self, _text: &'a str) -> StrictFuture<'a, Vec<Vec<f32>>> {
            Box::pin(async { Ok(vec![vec![1.0; 384]]) })
        }
    }
    struct FailingSummary;
    impl StrictSummaryProvider for FailingSummary {
        fn summarize<'a>(&'a self, _request: SummaryRequest) -> StrictFuture<'a, String> {
            Box::pin(async { Err(io::Error::other("fixture failure")) })
        }
    }
    fn setup() -> (tempfile::TempDir, ConversationStore, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let store = ConversationStore::create(
            root.path(),
            ConversationStateV2::new("p".into(), ID.into()).unwrap(),
        )
        .unwrap();
        let mut lock = store.lock().unwrap();
        for turn in 1..=11 {
            lock.append(Message::user(format!("turn {turn}")), true)
                .unwrap();
        }
        drop(lock);
        let database = root.path().join("memory.sqlite3");
        (root, store, database)
    }
    #[tokio::test]
    async fn source_snapshot_content_tampering_is_rejected() {
        let (_root, store, database) = setup();
        let history = StrictHistory::new(Arc::new(Summary), Arc::new(Embedder::new()));
        history
            .prepare(&store, &database, "remember")
            .await
            .unwrap();
        let path = std::fs::read_dir(store.directory().join("snapshots"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let mut snapshot: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        snapshot["events"][0]["message"]["content"] = "changed original".into();
        let generation = snapshot["state"]["generation"].as_u64().unwrap();
        let hash = snapshot["raw_hash"].as_str().unwrap().to_string();
        std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
        assert!(store.load_snapshot(generation, &hash).is_err());
    }

    #[tokio::test]
    async fn publication_cursor_survives_query_failure() {
        struct FailingQuery(Embedder);
        impl StrictEmbedder for FailingQuery {
            fn metadata(&self) -> &EmbeddingModel {
                self.0.metadata()
            }
            fn embed_passage<'a>(&'a self, text: &'a str) -> StrictFuture<'a, Vec<f32>> {
                self.0.embed_passage(text)
            }
            fn embed_query<'a>(&'a self, _: &'a str) -> StrictFuture<'a, Vec<Vec<f32>>> {
                Box::pin(async { Err(io::Error::other("query failure")) })
            }
        }
        let (root, store, database) = setup();
        let saved =
            crate::session_store::PersistedSession::open(root.path(), &database, "p", ID).unwrap();
        let history =
            StrictHistory::new(Arc::new(Summary), Arc::new(FailingQuery(Embedder::new())));
        assert!(
            history
                .prepare_observed(&store, &database, "remember", &|s| saved
                    .accept_publication(s))
                .await
                .is_err()
        );
        let snapshot = saved.snapshot().unwrap();
        assert_eq!(snapshot.state.visible_summary_ids.len(), 1);
        saved.append(Message::user("continue"), true, None).unwrap();
        assert!(saved.snapshot().is_ok());
    }

    #[test]
    fn source_pages_recover_large_unicode_payload_without_gaps() {
        let payload = "過去の原文🌟".repeat(1500);
        let mut offset = 0;
        let mut recovered = String::new();
        loop {
            let page = source_page("source-1", 1, 1, offset, &payload).unwrap();
            assert!(page.len() <= MAX_SOURCE_BYTES);
            assert!(
                tiktoken_rs::o200k_base_singleton()
                    .encode_with_special_tokens(&page)
                    .len()
                    <= MAX_SOURCE_TOKENS
            );
            let (header, body) = page.split_once('\n').unwrap();
            recovered.push_str(body);
            let next = header.split("next=").nth(1).unwrap();
            if next == "none" {
                break;
            }
            let (_, _, next_offset) = parse_uri(next).unwrap();
            assert!(next_offset > offset);
            offset = next_offset;
        }
        assert_eq!(payload, recovered);
        assert!(parse_uri("conversation://source-1?offset=1&offset=2").is_err());
        assert!(source_page("source-1", 1, 1, 1, &payload).is_err());
    }

    #[tokio::test]
    async fn prepares_latest_ten_publishes_expired_turn_and_resolves_source() {
        let (_root, store, database) = setup();
        let history = StrictHistory::new(Arc::new(Summary), Arc::new(Embedder::new()));
        let prepared = history
            .prepare(&store, &database, "remember")
            .await
            .unwrap();
        assert_eq!(prepared.messages.len(), 10);
        assert_eq!(prepared.messages[0].content, "turn 2");
        assert_eq!(prepared.published_generation, 1);
        assert_eq!(prepared.retrieval.len(), 1);
        let source = &prepared.retrieval[0].source.id;
        let text = read_source(
            &store,
            &database,
            &format!("conversation://{source}?start=1&end=1"),
        )
        .unwrap();
        assert!(text.contains("turn 1"));
        assert!(read_source(&store, &database, "conversation:///tmp/no").is_err());
    }
    #[tokio::test]
    async fn summary_failure_is_fail_closed_and_preserves_raw() {
        let (_root, store, database) = setup();
        let history = StrictHistory::new(Arc::new(FailingSummary), Arc::new(Embedder::new()));
        assert!(
            history
                .prepare(&store, &database, "remember")
                .await
                .is_err()
        );
        let snapshot = store.lock().unwrap().snapshot().unwrap();
        assert_eq!(snapshot.events.len(), 11);
        assert_eq!(snapshot.state.generation, 0);
        assert!(snapshot.expired_turns().contains(&1));
    }
}
