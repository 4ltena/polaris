//! Shared strict ten-turn preparation with v2 publication and v3 owner integration.
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

mod related_history;
mod structured_history;
#[cfg(test)]
mod structured_tests;

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
    /// Present only for structured10.  The provider returns the fixed outer
    /// envelope in the same completion as the ordinary summary.
    pub structured_sources: Option<super::structured::StructuredSummaryInput>,
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
    pub structured: Option<super::structured::packing::StructuredContext>,
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
    /// Optional, like every other use of the judge. It reorders the related
    /// candidates and nothing else; without it retrieval behaves exactly as
    /// it did before.
    judge: Option<Arc<dyn polaris_provider::decision::Decider>>,
}

impl StrictHistory {
    pub fn new(summary: Arc<dyn StrictSummaryProvider>, embedder: Arc<dyn StrictEmbedder>) -> Self {
        Self {
            summary,
            embedder,
            judge: None,
        }
    }

    pub fn judged_by(
        mut self,
        judge: Option<Arc<dyn polaris_provider::decision::Decider>>,
    ) -> Self {
        self.judge = judge;
        self
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
        let prepared = self
            .prepare_for_session_observed(store, database, query, published)
            .await?;
        if let Some(context) = &prepared.structured {
            super::observation::prepare_structured(context)?.require_dispatch()?;
        }
        Ok(prepared)
    }

    /// Session accepts this exact snapshot before exposing even a classified
    /// refusal. Public preparation keeps its existing io::Result contract.
    pub(crate) async fn prepare_for_session_observed(
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
        let structured = if snapshot.state.history_mode
            == crate::conversation_state::HistoryMode::Structured10
        {
            Some(self.prepare_structured_context(store, database, &snapshot, query)?)
        } else {
            None
        };
        let retrieval = if structured.is_some() {
            Vec::new()
        } else {
            self.retrieve(&view, database, query).await?
        };
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
            structured,
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
        if snapshot.state.history_mode == crate::conversation_state::HistoryMode::Structured10 {
            let work = super::begin_structured_summary(store, database, &snapshot, turn)?;
            return self.publish_structured_work(store, database, work).await;
        }
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
                let summary = self.summarize_turn(&snapshot, turn).await?;
                self.embed_summary(&snapshot, turn, summary).await?
            }
        };
        publish_summary_snapshot(store, database, &snapshot, &pending)
    }

    /// Shared processing only: callers own durable intent, publication and cancellation.
    pub(crate) fn embedding_metadata(&self) -> &EmbeddingModel {
        self.embedder.metadata()
    }

    pub(crate) async fn embed_related_query(
        &self,
        query: &str,
    ) -> io::Result<super::related::EmbeddedRelatedQuery> {
        if query.trim().is_empty()
            || query.len() > 64 * 1024
            || query
                .split_whitespace()
                .map(str::to_lowercase)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                > 128
        {
            return Err(io::Error::other("関連検索の入力が上限外です"));
        }
        let metadata = self.embedder.metadata().clone();
        if !(1..=4096).contains(&metadata.dimension) {
            return Err(io::Error::other("関連検索の埋め込み次元が上限外です"));
        }
        let vectors = self.embedder.embed_query(query).await?;
        if vectors.is_empty() || vectors.len() > 32 || &metadata != self.embedder.metadata() {
            return Err(io::Error::other("関連検索の埋め込み応答が不正です"));
        }
        let mut parts = Vec::with_capacity(vectors.len());
        for values in vectors {
            validate_vector(&values, &metadata)?;
            parts.push(QueryEmbedding {
                model: metadata.model.clone(),
                revision: metadata.revision.clone(),
                dimension: metadata.dimension,
                values,
            });
        }
        Ok(super::related::EmbeddedRelatedQuery::new(query, parts))
    }

    pub(crate) async fn retrieve(
        &self,
        view: &polaris_memory::conversation::PublishedView,
        database: &Path,
        query: &str,
    ) -> io::Result<Vec<ConversationHit>> {
        if query.trim().is_empty() {
            return Err(io::Error::other("strict10 query is empty"));
        }
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
                    published: view,
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
        Ok(bounded)
    }

    pub(crate) async fn summarize_turn(
        &self,
        snapshot: &ConversationSnapshot,
        turn: u64,
    ) -> io::Result<String> {
        let summary = self
            .summary
            .summarize(SummaryRequest {
                source_turn_id: turn,
                messages: turn_messages(snapshot, turn)?,
                model: SUMMARY_MODEL,
                effort: SUMMARY_EFFORT,
                prompt_version: SUMMARY_PROMPT_VERSION,
                structured_sources: None,
            })
            .await?;
        summary_evidence(snapshot, turn, &summary)?;
        Ok(summary)
    }

    pub(crate) async fn summarize_structured_turn(
        &self,
        snapshot: &ConversationSnapshot,
        turn: u64,
        intent: &super::structured::StructuredIntent,
        database: &Path,
        view: &polaris_memory::structured::StructuredPublicationView,
    ) -> io::Result<super::structured::ValidatedStructuredResponse> {
        let mut input = super::structured::summary_input(snapshot, turn, intent)?;
        input.related_entities = {
            let memory = MemoryStore::open(database).map_err(io::Error::other)?;
            memory
                .structured_entity_hints(
                    view,
                    &intent
                        .sources
                        .iter()
                        .map(|s| s.content.as_str())
                        .collect::<Vec<_>>(),
                )
                .map_err(io::Error::other)?
        };
        super::structured::hint_budget(&input.related_entities)?;
        let response = self
            .summary
            .summarize(SummaryRequest {
                source_turn_id: turn,
                messages: vec![],
                model: SUMMARY_MODEL,
                effort: SUMMARY_EFFORT,
                prompt_version: super::structured::STRUCTURED_PROMPT_VERSION,
                structured_sources: Some(input),
            })
            .await?;
        let memory = MemoryStore::open(database).map_err(io::Error::other)?;
        super::structured::validate_response_with_resolver(
            snapshot,
            turn,
            intent,
            &response,
            &super::structured::PublishedResolver {
                memory: &memory,
                view,
            },
        )
    }

    pub(crate) async fn embed_summary(
        &self,
        snapshot: &ConversationSnapshot,
        turn: u64,
        summary: String,
    ) -> io::Result<PendingSummary> {
        let evidence = summary_evidence(snapshot, turn, &summary)?;
        let values = self.embedder.embed_passage(&summary).await?;
        let metadata = self.embedder.metadata().clone();
        validate_vector(&values, &metadata)?;
        let summary_hash = content_hash(summary.as_bytes());
        Ok(PendingSummary {
            scope: evidence.scope,
            id: evidence.id,
            source: evidence.source,
            summary_hash: summary_hash.clone(),
            summary,
            model: SUMMARY_MODEL.into(),
            effort: SUMMARY_EFFORT.into(),
            prompt_version: SUMMARY_PROMPT_VERSION.into(),
            embedding: EmbeddingMetadata {
                model: metadata.model,
                revision: metadata.revision,
                dimension: metadata.dimension,
                input_hash: summary_hash,
                values,
            },
        })
    }
}

/// Construct the same complete evidence used by retrieval before spending on
/// embedding or saving a summary that could never fit the injection budget.
pub(crate) fn summary_evidence(
    snapshot: &ConversationSnapshot,
    turn: u64,
    summary: &str,
) -> io::Result<ConversationHit> {
    validate_summary(summary, turn)?;
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
    let suffix = snapshot
        .raw_hash
        .get(..16)
        .ok_or_else(|| io::Error::other("invalid raw hash"))?;
    let evidence = ConversationHit {
        scope,
        id: format!("strict10-{turn}-{suffix}"),
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
        summary: summary.into(),
        score: 0.0,
    };
    evidence.injection_tokens().map_err(io::Error::other)?;
    Ok(evidence)
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

pub(super) fn turn_messages(
    snapshot: &ConversationSnapshot,
    turn: u64,
) -> io::Result<Vec<Message>> {
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

pub(crate) fn validate_summary(summary: &str, turn: u64) -> io::Result<()> {
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
    if uri.starts_with("conversation://structured/") {
        return super::structured::sources::resolve_uri(store, database, uri);
    }
    let (source_id, requested, offset) = parse_uri(uri)?;
    let lock = store.lock()?;
    let current = lock.snapshot()?;
    let memory = MemoryStore::open(database).map_err(io::Error::other)?;
    let read = || {
        resolve_legacy_source_from_snapshot(store, &current, &memory, &source_id, requested, offset)
    };
    if current.state.history_mode == crate::conversation_state::HistoryMode::Structured10 {
        super::structured_owner::with_verified_receipts(store, &current, &memory, |_| read())
    } else {
        read()
    }
}

fn resolve_legacy_source_from_snapshot(
    store: &ConversationStore,
    current: &ConversationSnapshot,
    memory: &MemoryStore,
    source_id: &str,
    requested: Option<(i64, i64)>,
    offset: usize,
) -> io::Result<ResolvedConversationSource> {
    let view = published_view(&current.state)?;
    memory
        .with_conversation_source(&view, source_id, |reference| {
            let parent;
            let source_store = if reference.scope.session_id == current.state.session_id {
                store
            } else {
                parent = ConversationStore::open(
                    store.data_root().map_err(polaris_memory::Error::Io)?,
                    &reference.scope.project_id,
                    &reference.scope.session_id,
                )
                .map_err(polaris_memory::Error::Io)?;
                &parent
            };
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
            if current.state.history_mode == crate::conversation_state::HistoryMode::Structured10 {
                verify_structured_summary_source(current, source_store, &snapshot, &reference)
                    .map_err(polaris_memory::Error::Io)?;
            }
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

/// Legacy summary URIs are still valid historical evidence in Structured10,
/// but their v2 index metadata must match the paired saved extraction result.
fn verify_structured_summary_source(
    current: &ConversationSnapshot,
    source_store: &ConversationStore,
    source: &ConversationSnapshot,
    reference: &polaris_memory::conversation::SourceReference,
) -> io::Result<String> {
    let invalid = || io::Error::other("過去の原文参照が構造化保存記録と一致しません");
    let turn = u64::try_from(reference.source.start_turn).map_err(|_| invalid())?;
    let response = source_store
        .bound_structured_attempt(source, turn)?
        .and_then(|receipt| receipt.response)
        .ok_or_else(invalid)?;
    let evidence = summary_evidence(source, turn, &response.summary)?;
    let publication = response.delta.publication;
    let granted = if publication.scope.session_id == current.state.session_id {
        source.state.epoch == current.state.epoch
            && current.state.generation > source.state.generation
            && current
                .state
                .visible_structured_publication_ids
                .contains(&publication.publication_id)
    } else {
        current.state.ancestors.iter().any(|ancestor| {
            ancestor.session_id == publication.scope.session_id
                && ancestor.epoch == source.state.epoch
                && ancestor.generation > source.state.generation
                && ancestor
                    .visible_structured_publication_ids
                    .contains(&publication.publication_id)
        })
    };
    if !granted
        || reference.summary_id != evidence.id
        || reference.scope != evidence.scope
        || reference.source != evidence.source
    {
        return Err(invalid());
    }
    Ok(response.summary)
}

type ParsedSourceUri = (String, Option<(i64, i64)>, usize);
pub(crate) fn parse_uri(uri: &str) -> io::Result<ParsedSourceUri> {
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

pub(crate) fn source_page(
    source: &str,
    start: i64,
    end: i64,
    offset: usize,
    payload: &str,
) -> io::Result<String> {
    evidence_page(payload, offset, |stop| {
        let next = if stop < payload.len() {
            format!("conversation://{source}?start={start}&end={end}&offset={stop}")
        } else {
            "none".into()
        };
        format!(
            "historical evidence source_id={source} turns={start}-{end} bytes={offset}-{stop}/{} next={next}",
            payload.len()
        )
    })
}

/// Shared bounded UTF-8 paging. Callers supply only their immutable source
/// metadata and continuation URI; the size/token policy is identical.
pub(crate) fn evidence_page(
    payload: &str,
    offset: usize,
    header: impl Fn(usize) -> String,
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
    while stop > offset {
        let page = format!("{}\n{}", header(stop), &payload[offset..stop]);
        if page.len() <= MAX_SOURCE_BYTES && crate::budget::count_tokens(&page) <= MAX_SOURCE_TOKENS
        {
            return Ok(page);
        }
        if stop - offset > 64 {
            stop -= 64;
            while !payload.is_char_boundary(stop) {
                stop -= 1;
            }
        } else {
            // A whole final chunk may not fit while one scalar still does.
            stop = offset
                + payload[offset..stop]
                    .char_indices()
                    .next_back()
                    .expect("nonempty UTF-8 prefix")
                    .0;
        }
    }
    Err(io::Error::other("source metadata exceeds recovery budget"))
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

/// The models the bundled helper will run, and the hidden size each has.
///
/// The same list lives in `scripts/local_embedding.py` as `MODELS`, and
/// `the_two_sides_of_the_model_allowlist_agree` holds them equal. A model
/// named on one side only is a helper invocation that dies at the argument
/// check, which is loud, but the pair is cheap to keep honest.
///
/// strict10 pins the small one; the preselection ranking stage (M10 L2)
/// pins the base one. Nothing chooses at runtime — the caller names which
/// model it pinned, and a snapshot whose real hidden size disagrees is
/// refused inside the helper.
pub const EMBEDDING_MODELS: [(&str, i64); 2] = [
    ("intfloat/multilingual-e5-small", 384),
    ("intfloat/multilingual-e5-base", 768),
];

/// The model strict10 has always used. Named so the strict path cannot
/// drift onto the other one by editing a list.
pub const STRICT_EMBEDDING_MODEL: &str = "intfloat/multilingual-e5-small";

/// The model the preselection ranking stage uses (M10 L2).
pub const PRESELECT_EMBEDDING_MODEL: &str = "intfloat/multilingual-e5-base";

impl LocalStdioEmbedder {
    /// strict10's embedder. Pinned to [`STRICT_EMBEDDING_MODEL`] so that
    /// path cannot acquire a second model by configuration.
    pub fn new(
        python: PathBuf,
        helper: PathBuf,
        model_path: PathBuf,
        run_temp: PathBuf,
        revision: String,
        cpu_threads: usize,
        timeout: Duration,
    ) -> io::Result<Self> {
        Self::with_model(
            python,
            helper,
            model_path,
            run_temp,
            STRICT_EMBEDDING_MODEL,
            revision,
            cpu_threads,
            timeout,
        )
    }

    /// The same embedder against a named model from [`EMBEDDING_MODELS`].
    #[allow(clippy::too_many_arguments)]
    pub fn with_model(
        python: PathBuf,
        helper: PathBuf,
        model_path: PathBuf,
        run_temp: PathBuf,
        model: &str,
        revision: String,
        cpu_threads: usize,
        timeout: Duration,
    ) -> io::Result<Self> {
        let Some((model, dimension)) = EMBEDDING_MODELS
            .iter()
            .find(|(name, _)| *name == model)
            .map(|(name, dimension)| ((*name).to_string(), *dimension))
        else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "この埋め込みモデルは同梱helperの許可一覧にありません",
            ));
        };
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
                model,
                revision,
                dimension,
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
        let response = self.helper_call(Some(request)).await?;
        self.parse_vectors(response)
    }

    /// Bounded offline model/package readiness; never sends a provider request.
    pub async fn preflight(&self) -> io::Result<serde_json::Value> {
        let receipt = self.helper_call(None).await?;
        let packages = receipt
            .get("packages")
            .and_then(serde_json::Value::as_object);
        if receipt.as_object().is_none_or(|r| r.len() != 4)
            || receipt["model"] != self.metadata.model
            || receipt["revision"] != self.metadata.revision
            || receipt["dimension"].as_i64() != Some(self.metadata.dimension)
            || packages.is_none_or(|p| {
                p.len() != 4
                    || ["torch", "transformers", "tokenizers", "safetensors"]
                        .iter()
                        .any(|name| {
                            p.get(*name)
                                .and_then(serde_json::Value::as_str)
                                .is_none_or(|v| v.is_empty() || v.len() > 128)
                        })
            })
        {
            return Err(io::Error::other(
                "埋め込み資源の準備結果が契約と一致しません",
            ));
        }
        Ok(receipt)
    }

    async fn helper_call(&self, request: Option<String>) -> io::Result<serde_json::Value> {
        use std::process::Stdio;
        use tokio::{io::AsyncWriteExt, process::Command};
        // Each helper gets a fresh private directory. `TempDir` removes only this
        // call's files on success, timeout, error, or future cancellation.
        let call_temp = tempfile::Builder::new()
            .prefix("embedding-call-")
            .tempdir_in(&self.run_temp)?;
        let deadline = tokio::time::Instant::now() + self.timeout;
        let mut command = Command::new(&self.python);
        command
            .arg("-I")
            .arg("-B")
            .arg(&self.helper)
            .arg("--model-path")
            .arg(&self.model_path)
            .arg("--model-name")
            .arg(&self.metadata.model)
            .arg("--revision")
            .arg(&self.metadata.revision)
            .arg("--dimension")
            .arg(self.metadata.dimension.to_string())
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
            .stderr(Stdio::piped());
        if request.is_none() {
            command.arg("--preflight");
        }
        let mut child = command.spawn()?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("embedding stdin is unavailable"))?;
        let write = async {
            if let Some(request) = request {
                stdin.write_all(format!("{request}\n").as_bytes()).await?;
            }
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
        Ok(response)
    }

    fn parse_vectors(&self, response: serde_json::Value) -> io::Result<(Vec<Vec<f32>>, u32)> {
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

    /// The allowlist exists twice — here and in the helper — and both
    /// copies decide whether a run happens. A model added to one side only
    /// dies at the helper's argument check, which is at least loud, but a
    /// *dimension* that disagrees is worse: the helper would refuse on the
    /// real hidden size and nobody would know which side was wrong.
    #[test]
    fn the_two_sides_of_the_model_allowlist_agree() {
        let helper = include_str!("../../../../scripts/local_embedding.py");
        let block = helper
            .split_once("MODELS = {")
            .and_then(|(_, rest)| rest.split_once('}'))
            .map(|(block, _)| block)
            .expect("the helper no longer declares MODELS");
        let declared: Vec<(String, i64)> = block
            .lines()
            .filter_map(|line| line.trim().strip_suffix(','))
            .filter_map(|line| line.split_once(':'))
            .map(|(name, size)| {
                (
                    name.trim().trim_matches('"').to_string(),
                    size.trim().parse().expect("a model size is not a number"),
                )
            })
            .collect();
        let held: Vec<(String, i64)> = EMBEDDING_MODELS
            .iter()
            .map(|(name, size)| ((*name).to_string(), *size))
            .collect();
        assert_eq!(declared, held, "helper {declared:?} against Rust {held:?}");
        assert!(held.iter().any(|(name, _)| name == STRICT_EMBEDDING_MODEL));
        assert!(
            held.iter()
                .any(|(name, _)| name == PRESELECT_EMBEDDING_MODEL)
        );
        assert_ne!(STRICT_EMBEDDING_MODEL, PRESELECT_EMBEDDING_MODEL);
    }

    /// `new` is strict10's door and must stay on one model however the
    /// allowlist grows. `with_model` refuses anything not on it.
    #[test]
    fn the_strict_constructor_is_pinned_and_an_unlisted_model_is_refused() {
        let dir = tempfile::tempdir().expect("temp dir");
        let python = dir.path().join("python3");
        let helper = dir.path().join("helper.py");
        std::fs::write(&python, "").expect("write");
        std::fs::write(&helper, "").expect("write");
        let make = |model: Option<&str>| match model {
            None => LocalStdioEmbedder::new(
                python.clone(),
                helper.clone(),
                dir.path().to_path_buf(),
                dir.path().to_path_buf(),
                "rev".into(),
                2,
                Duration::from_secs(1),
            ),
            Some(model) => LocalStdioEmbedder::with_model(
                python.clone(),
                helper.clone(),
                dir.path().to_path_buf(),
                dir.path().to_path_buf(),
                model,
                "rev".into(),
                2,
                Duration::from_secs(1),
            ),
        };
        let strict = make(None).expect("strict embedder");
        assert_eq!(strict.metadata().model, STRICT_EMBEDDING_MODEL);
        assert_eq!(strict.metadata().dimension, 384);

        let preselect = make(Some(PRESELECT_EMBEDDING_MODEL)).expect("preselect embedder");
        assert_eq!(preselect.metadata().dimension, 768);

        for refused in ["intfloat/multilingual-e5-large", "", "e5-base"] {
            assert!(make(Some(refused)).is_err(), "{refused}");
        }
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn cancelled_local_embedding_reaps_owned_helper_and_records_unknown_usage() {
        use polaris_provider::attempts::{AttemptLedger, AttemptStatus, UsageObservation};
        let root = tempfile::tempdir().unwrap();
        let model = root.path().join("model");
        let temporary = root.path().join("calls");
        std::fs::create_dir(&model).unwrap();
        std::fs::create_dir(&temporary).unwrap();
        let helper = root.path().join("fixture.py");
        // Deterministic process fixture: no model, packages, network or subprocesses.
        // Publish readiness atomically, after the PID contents are complete.
        std::fs::write(&helper, "import os, sys, time\nfrom pathlib import Path\nroot = Path(sys.argv[sys.argv.index('--model-path') + 1])\n(root / 'pid.tmp').write_text(str(os.getpid()))\nos.replace(root / 'pid.tmp', root / 'pid')\nwhile True: time.sleep(0.01)\n").unwrap();
        let ledger = AttemptLedger::default();
        let mut embedder = LocalStdioEmbedder::new(
            "/usr/bin/python3".into(),
            helper,
            model.clone(),
            temporary.clone(),
            "fixture".into(),
            1,
            Duration::from_secs(10),
        )
        .unwrap();
        embedder.attempt_ledger = Some(ledger.clone());
        let task = tokio::spawn(async move { embedder.embed_query("pending fixture").await });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let pid: i32 = loop {
            if let Ok(pid) = std::fs::read_to_string(model.join("pid")) {
                break pid.parse().unwrap();
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "owned Python fixture did not start"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        loop {
            // Signal zero only observes the child created above; it sends no signal.
            if unsafe { libc::kill(pid, 0) } == -1
                && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "cancelled embedding child was not reaped"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(std::fs::read_dir(temporary).unwrap().count(), 0);
        let records = ledger.snapshot();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].status, AttemptStatus::Cancelled);
        assert_eq!(records[0].usage, UsageObservation::Missing);
    }

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

    #[test]
    fn source_paging_refuses_metadata_that_leaves_no_forward_progress() {
        assert!(evidence_page("a", 0, |_| "x".repeat(MAX_SOURCE_BYTES - 1)).is_err());
        let one = evidence_page("aa", 0, |_| " ".repeat(MAX_SOURCE_BYTES - 2)).unwrap();
        assert_eq!(one.split_once('\n').unwrap().1, "a");
        let scalar = evidence_page("あa", 0, |_| " ".repeat(MAX_SOURCE_BYTES - 4)).unwrap();
        assert_eq!(scalar.split_once('\n').unwrap().1, "あ");
        assert_eq!(
            evidence_page("a", 0, |_| "evidence".into()).unwrap(),
            "evidence\na"
        );
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
