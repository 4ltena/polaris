//! strict10のv3公開境界。モデルを呼ばず、v2 writerを開かない。
use super::*;
use crate::conversation_memory::structured::{StructuredIntent, ValidatedStructuredResponse};
use crate::conversation_state::{
    ConversationSnapshot, ConversationStateV2, RawEventV2, StructuredCoverage, content_hash,
};
use polaris_desktop_protocol::{
    ids::*,
    request::RunTarget,
    snapshot::{HistoryMode, MemoryPhase, MemoryStatus},
};
use polaris_memory::structured::StructuredDelta;
use polaris_memory::{
    MemoryStore,
    conversation::{PendingSummary, PublishedView, Scope},
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, path::Path};

mod fork;
mod frozen;
mod initialize;
mod related;
pub(super) use fork::{fork_seed, validate_transition};
pub(in crate::desktop_store) use frozen::with_inherited_prompt_attachments;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryResources {
    pub embedding_model: String,
    pub embedding_revision: String,
    pub embedding_dimension: u32,
    /// 実体照合済みruntime/helper/model manifestの識別子。実行許可ではない。
    pub fingerprint: String,
}
impl MemoryResources {
    pub fn validate(&self) -> StoreResult<()> {
        if self.embedding_model.is_empty()
            || self.embedding_revision.is_empty()
            || self.embedding_model.len() > 256
            || self.embedding_revision.len() > 256
            || !(1..=4096).contains(&self.embedding_dimension)
            || !hash(&self.fingerprint)
        {
            return Err(StoreError::Corrupt("Memory resource identifier"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceStamp {
    turn: DecimalU64,
    turn_hash: String,
    raw_offset: DecimalU64,
    raw_hash: String,
    content_revision: DecimalU64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SummaryAttempt {
    source: SourceStamp,
    run_id: RunId,
    attempt_id: AttemptId,
    operation_id: OperationId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    structured: Option<StructuredAttempt>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StructuredAttempt {
    intent: StructuredIntent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    response: Option<SavedStructuredResponse>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedStructuredResponse {
    pub summary: String,
    pub delta: StructuredDelta,
    pub coverage: StructuredCoverage,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishedSource {
    source: SourceStamp,
    generation: DecimalU64,
    summary_id: String,
    source_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    structured_publication_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    structured_coverage: Option<StructuredCoverage>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedMemory {
    schema_version: u32,
    epoch: DecimalU64,
    resources: MemoryResources,
    generation: DecimalU64,
    sources: Vec<PublishedSource>,
    attempts: Vec<SummaryAttempt>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    ancestors: Vec<fork::FrozenAncestor>,
}

/// 原文は公開済みprefixからの読取り投影だけ。ここからv2保存は行わない。
#[derive(Debug, Clone)]
pub struct MemorySnapshot {
    pub conversation: ConversationSnapshot,
    content_revision: DecimalU64,
    configuration: polaris_desktop_protocol::snapshot::Configuration,
    policy_revision: DecimalU64,
    workflow: Option<SavedWorkflow>,
}
impl MemorySnapshot {
    pub fn structured_view(
        &self,
    ) -> StoreResult<polaris_memory::structured::StructuredPublicationView> {
        let state = &self.conversation.state;
        if state.history_mode != HistoryMode::Structured10 {
            return Err(StoreError::RunConflict);
        }
        Ok(polaris_memory::structured::StructuredPublicationView {
            scope: polaris_memory::structured::PublicationScope {
                project_id: state.project_id.clone(),
                session_id: state.session_id.clone(),
                epoch: number(state.epoch)?,
                generation: number(state.generation)?,
            },
            visible_publication_ids: state.visible_structured_publication_ids.clone(),
            ancestors: state
                .ancestors
                .iter()
                .map(|ancestor| {
                    Ok(
                        polaris_memory::structured::StructuredAncestorPublicationRange {
                            session_id: ancestor.session_id.clone(),
                            epoch: number(ancestor.epoch)?,
                            generation: number(ancestor.generation)?,
                            visible_publication_ids: ancestor
                                .visible_structured_publication_ids
                                .clone(),
                        },
                    )
                })
                .collect::<StoreResult<_>>()?,
        })
    }
    pub fn view(&self) -> StoreResult<PublishedView> {
        let state = &self.conversation.state;
        Ok(PublishedView {
            scope: Scope {
                project_id: state.project_id.clone(),
                session_id: state.session_id.clone(),
                epoch: number(state.epoch)?,
                generation: number(state.generation)?,
            },
            visible_ids: state.visible_summary_ids.clone(),
            ancestors: state
                .ancestors
                .iter()
                .map(|ancestor| {
                    Ok(polaris_memory::conversation::AncestorRange {
                        session_id: ancestor.session_id.clone(),
                        epoch: number(ancestor.epoch)?,
                        generation: number(ancestor.generation)?,
                        visible_ids: ancestor.visible_summary_ids.clone(),
                    })
                })
                .collect::<StoreResult<_>>()?,
        })
    }
    pub fn expired_turn(&self) -> StoreResult<Option<u64>> {
        for turn in self.conversation.expired_turns() {
            self.conversation.validate_complete_turn(turn)?;
            if !self
                .conversation
                .state
                .visible_summary_ids
                .iter()
                .any(|id| id.starts_with(&format!("strict10-{turn}-")))
            {
                return Ok(Some(turn));
            }
        }
        Ok(None)
    }
}

#[derive(Debug, Clone)]
pub struct SummaryWork {
    pub expected: MemorySnapshot,
    pub source: ConversationSnapshot,
    pub turn: u64,
    pub summary: Option<String>,
    pub operation_id: OperationId,
    /// Only a newly published intent authorizes a new summary request.
    pub newly_started: bool,
    pub structured_intent: Option<StructuredIntent>,
    pub structured_response: Option<SavedStructuredResponse>,
}

fn hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}
fn number(value: u64) -> StoreResult<i64> {
    value.try_into().map_err(|_| StoreError::Overflow)
}
fn memory(p: &Published) -> StoreResult<&SavedMemory> {
    p.state
        .conversation_memory
        .as_ref()
        .ok_or(StoreError::Corrupt("Memory configuration not connected"))
}
fn run_index(p: &Published, target: &RunTarget) -> StoreResult<usize> {
    p.state
        .runs
        .iter()
        .position(|r| r.run.run_id == target.run_id && r.run.attempt_id == target.attempt_id)
        .ok_or(StoreError::NotFound)
}
fn active(p: &Published, target: &RunTarget) -> StoreResult<()> {
    let run = &p.state.runs[run_index(p, target)?];
    if !run.run.state.permits_start()
        || !matches!(
            run.configuration.history_mode,
            HistoryMode::Strict10 | HistoryMode::Structured10
        )
        || run.policy_revision != p.state.policy_revision
        || run.configuration != p.state.configuration
    {
        return Err(StoreError::RunConflict);
    }
    Ok(())
}
fn turn_hash(snapshot: &ConversationSnapshot, turn: u64) -> StoreResult<String> {
    snapshot.validate_complete_turn(turn)?;
    let events: Vec<_> = snapshot
        .events
        .iter()
        .filter(|e| e.epoch == snapshot.state.epoch && e.turn_id == turn)
        .collect();
    Ok(content_hash(&serde_json::to_vec(&events)?))
}
fn project(
    p: &Published,
    raw: Vec<RawEventV2>,
    offset: DecimalU64,
    raw_hash: String,
) -> StoreResult<ConversationSnapshot> {
    let m = memory(p)?;
    let turns: Vec<_> = raw
        .iter()
        .filter(|e| e.epoch == p.marker.epoch.get() && e.starts_turn)
        .map(|e| e.turn_id)
        .collect();
    let mut recent = turns;
    recent.drain(..recent.len().saturating_sub(10));
    Ok(ConversationSnapshot {
        state: ConversationStateV2 {
            workflow_config: None,
            session_settings: None,
            draft: Default::default(),
            goal: None,
            schema_version: 2,
            project_id: p.marker.project_id.as_str().into(),
            session_id: p.marker.session_id.as_str().into(),
            epoch: p.marker.epoch.get(),
            generation: m.generation.get(),
            raw_commit_offset: offset.get(),
            raw_commit_hash: raw_hash.clone(),
            history_mode: p.state.configuration.history_mode,
            recent_turn_ids: recent,
            visible_summary_ids: m.sources.iter().map(|s| s.summary_id.clone()).collect(),
            visible_structured_publication_ids: m
                .sources
                .iter()
                .filter_map(|s| s.structured_publication_id.clone())
                .collect(),
            structured_coverage: m.sources.last().and_then(|s| s.structured_coverage.clone()),
            workflow: p
                .state
                .workflow
                .as_ref()
                .map(|w| w.state.clone())
                .unwrap_or_default(),
            gates: p
                .state
                .workflow
                .as_ref()
                .map(|w| w.gates.clone())
                .unwrap_or_default(),
            workflow_artifacts: p.state.workflow_artifacts.clone(),
            verification_checkpoint: None,
            // Desktop admissions are bound in the desktop sidecar/marker;
            // this v2 memory projection never owns those receipts.
            admission_receipts: Default::default(),
            ancestors: m
                .ancestors
                .iter()
                .map(|ancestor| crate::conversation_state::AncestorRange {
                    session_id: ancestor.marker.session_id.as_str().into(),
                    epoch: ancestor.marker.epoch.get(),
                    generation: ancestor.generation.get(),
                    visible_summary_ids: ancestor.summary_ids.clone(),
                    visible_structured_publication_ids: ancestor.structured_ids.clone(),
                })
                .collect(),
        },
        raw_hash,
        raw_offset: offset.get(),
        events: raw,
    })
}

pub(super) fn validate(p: &Published) -> StoreResult<()> {
    fork::validate(p)?;
    for run in &p.state.runs {
        if let Some(status) = &run.memory {
            validate_status(status)?;
            if status.run_id != run.run.run_id
                || (status.retrieval_status.is_some()
                    && run.configuration.history_mode != HistoryMode::Structured10)
                || !matches!(
                    run.configuration.history_mode,
                    HistoryMode::Strict10 | HistoryMode::Structured10
                )
                || run.memory_resources.is_none()
            {
                return Err(StoreError::Corrupt("Run memory state"));
            }
        }
        if let Some(resources) = &run.memory_resources {
            resources.validate()?;
        }
    }
    let Some(m) = &p.state.conversation_memory else {
        return Ok(());
    };
    m.resources.validate()?;
    if m.schema_version != 1
        || m.epoch != p.marker.epoch
        || m.generation.get() != m.sources.len() as u64
        || m.attempts.len() > 65_536
        || m.sources.len() > 65_536
    {
        return Err(StoreError::Corrupt("Memory storage version or generation"));
    }
    let mut turns = BTreeSet::new();
    // Hash all required raw boundaries in one pass, including their newline.
    let required: BTreeSet<_> = m
        .attempts
        .iter()
        .filter_map(|a| a.structured.as_ref())
        .map(|a| a.intent.coverage.last_source_sequence)
        .collect();
    let mut boundaries = std::collections::BTreeMap::new();
    let mut raw_bytes = 0u64;
    use sha2::{Digest, Sha256};
    let mut raw_digest = Sha256::new();
    for event in &p.raw {
        let bytes = serde_json::to_vec(event)?;
        raw_bytes += bytes.len() as u64 + 1;
        raw_digest.update(&bytes);
        raw_digest.update(b"\n");
        if required.contains(&event.sequence) {
            boundaries.insert(
                event.sequence,
                (raw_bytes, format!("{:x}", raw_digest.clone().finalize())),
            );
        }
    }
    for (index, source) in m.sources.iter().enumerate() {
        if !turns.insert(source.source.turn)
            || source.generation.get() != index as u64 + 1
            || !source
                .summary_id
                .starts_with(&format!("strict10-{}-", source.source.turn.get()))
            || !source
                .source_id
                .starts_with(&format!("source-{}-", source.source.turn.get()))
        {
            return Err(StoreError::Corrupt("Memory published source"));
        }
        let attempt = m
            .attempts
            .iter()
            .find(|a| a.source.turn == source.source.turn)
            .ok_or(StoreError::Corrupt("Missing published memory attempt"))?;
        if attempt.source != source.source {
            return Err(StoreError::Corrupt("Published memory source association"));
        }
        match (
            &attempt.structured,
            &source.structured_publication_id,
            &source.structured_coverage,
        ) {
            (None, None, None) => {}
            (Some(structured), Some(id), Some(coverage)) => {
                let response = structured
                    .response
                    .as_ref()
                    .ok_or(StoreError::Corrupt("Missing published memory response"))?;
                if &response.delta.publication.publication_id != id
                    || &response.coverage != coverage
                    || u64::try_from(response.delta.publication.scope.generation).ok()
                        != Some(source.generation.get())
                {
                    return Err(StoreError::Corrupt(
                        "Published memory and structured response association",
                    ));
                }
            }
            _ => {
                return Err(StoreError::Corrupt(
                    "Published memory structured association",
                ));
            }
        }
    }
    let mut attempts = BTreeSet::new();
    let source_events: std::collections::BTreeMap<_, _> =
        p.raw.iter().map(|e| (e.sequence, e)).collect();
    for attempt in &m.attempts {
        if !attempts.insert(attempt.source.turn) {
            return Err(StoreError::Corrupt("Duplicate summary"));
        }
        let op = p
            .state
            .runs
            .iter()
            .find(|r| r.run.run_id == attempt.run_id && r.run.attempt_id == attempt.attempt_id)
            .and_then(|r| {
                r.operations
                    .iter()
                    .find(|o| o.operation_id == attempt.operation_id)
            })
            .ok_or(StoreError::Corrupt("Summary run reference"))?;
        if let Some(summary) = &attempt.summary {
            crate::conversation_memory::validate_summary(summary, attempt.source.turn.get())?;
            if op.result_id.is_none() {
                return Err(StoreError::Corrupt("Summary outcome unknown"));
            }
        }
        if let Some(structured) = &attempt.structured {
            crate::conversation_memory::structured::source_budget(&structured.intent.sources)?;
            let mut previous = None;
            for source in &structured.intent.sources {
                let event = source_events
                    .get(&source.sequence)
                    .ok_or(StoreError::Corrupt("Missing structured source"))?;
                if previous.is_some_and(|p| p >= source.sequence)
                    || event.epoch != m.epoch.get()
                    || source.role != event.message.role
                    || source.content != event.message.content
                    || source.source_id
                        != format!(
                            "source-v3:{}:{}:{}:{}",
                            p.marker.project_id.as_str(),
                            p.marker.session_id.as_str(),
                            event.epoch,
                            event.sequence
                        )
                {
                    return Err(StoreError::Corrupt("Structured source text binding"));
                }
                previous = Some(source.sequence);
            }
            let valid_source_end = if structured.intent.sources.is_empty() {
                // An empty extraction must inherit the same fully covered raw
                // prefix from an earlier *published structured* summary.
                structured.intent.coverage.raw_offset == attempt.source.raw_offset.get()
                    && structured.intent.coverage.prefix_hash == attempt.source.raw_hash
                    && m.sources.iter().any(|prior| {
                        prior.source.turn < attempt.source.turn
                            && prior.structured_publication_id.is_some()
                            && prior.structured_coverage.as_ref()
                                == Some(&structured.intent.coverage)
                            && prior.source.raw_offset == attempt.source.raw_offset
                            && prior.source.raw_hash == attempt.source.raw_hash
                            && m.attempts.iter().any(|saved| {
                                saved.source == prior.source
                                    && saved
                                        .structured
                                        .as_ref()
                                        .and_then(|s| s.response.as_ref())
                                        .is_some()
                            })
                    })
            } else {
                previous == Some(structured.intent.coverage.last_source_sequence)
            };
            if !valid_source_end
                || structured.intent.coverage.epoch != m.epoch.get()
                || structured.intent.coverage.raw_offset > attempt.source.raw_offset.get()
                || !hash(&structured.intent.coverage.prefix_hash)
                || boundaries.get(&structured.intent.coverage.last_source_sequence)
                    != Some(&(
                        structured.intent.coverage.raw_offset,
                        structured.intent.coverage.prefix_hash.clone(),
                    ))
            {
                return Err(StoreError::Corrupt("Saved structured coverage content"));
            }
            if let Some(response) = &structured.response {
                polaris_memory::structured::validate_delta(&response.delta)
                    .map_err(std::io::Error::other)?;
                let publication = &response.delta.publication;
                if response.coverage != structured.intent.coverage
                    || attempt.summary.as_deref() != Some(&response.summary)
                    || publication.raw_hash != attempt.source.raw_hash
                    || publication.scope.project_id != p.marker.project_id.as_str()
                    || publication.scope.session_id != p.marker.session_id.as_str()
                    || u64::try_from(publication.scope.epoch).ok() != Some(m.epoch.get())
                    || publication.prompt_version
                        != crate::conversation_memory::structured::STRUCTURED_PROMPT_VERSION
                {
                    return Err(StoreError::Corrupt("Saved structured response content"));
                }
            }
        }
    }
    for source in m
        .sources
        .iter()
        .map(|s| &s.source)
        .chain(m.attempts.iter().map(|a| &a.source))
    {
        if !hash(&source.raw_hash)
            || !hash(&source.turn_hash)
            || source.raw_offset > p.marker.raw_offset
            || source.content_revision > p.marker.content_revision
        {
            return Err(StoreError::Corrupt("Summary source reference"));
        }
    }
    Ok(())
}

fn validate_status(status: &MemoryStatus) -> StoreResult<()> {
    use polaris_desktop_protocol::snapshot::MemoryRetrievalStatus;
    let structured = status.retrieval_status.is_some();
    if (status.related_omission.is_some()
        && !matches!(
            status.retrieval_status,
            Some(MemoryRetrievalStatus::Found | MemoryRetrievalStatus::PendingReconfirmation)
        ))
        || status.detail.len() > 1024
        || status.recent_raw_turns.get() > 10
        || status.retrieval_sources.len() > if structured { 256 } else { 3 }
        || status
            .retrieval_sources
            .iter()
            .map(String::len)
            .sum::<usize>()
            > 64 * 1024
        || status
            .retrieval_sources
            .iter()
            .collect::<BTreeSet<_>>()
            .len()
            != status.retrieval_sources.len()
        || status.reference_tokens.get() > 768
        || status.retrieval_sources.iter().any(|uri| {
            if structured && uri.starts_with("conversation://structured/") {
                crate::conversation_memory::structured::sources::parse_source_uri(uri).is_err()
            } else {
                uri.len() > 1024 || crate::conversation_memory::parse_uri(uri).is_err()
            }
        })
        || (matches!(
            status.retrieval_status,
            Some(
                MemoryRetrievalStatus::NotFound
                    | MemoryRetrievalStatus::AmbiguousEntity
                    | MemoryRetrievalStatus::ExtractionIncomplete
                    | MemoryRetrievalStatus::EvidenceBudgetExceeded
            )
        ) && !status.retrieval_sources.is_empty())
    {
        return Err(StoreError::Corrupt("Memory state limit"));
    }
    Ok(())
}

impl Writer {
    /// Bind trusted resources before any summary/provider request. Existing memory is never reset.
    pub fn bind_memory(
        &mut self,
        target: &RunTarget,
        resources: MemoryResources,
    ) -> StoreResult<()> {
        resources.validate()?;
        let mut next = self.snapshot()?;
        active(&next, target)?;
        if let Some(saved) = &next.state.conversation_memory {
            if saved.resources != resources {
                return Err(StoreError::CasConflict("memory resources"));
            }
        } else {
            next.state.conversation_memory = Some(SavedMemory {
                schema_version: 1,
                epoch: next.marker.epoch,
                resources: resources.clone(),
                generation: DecimalU64::new(0),
                sources: vec![],
                attempts: vec![],
                ancestors: vec![],
            });
        }
        let index = run_index(&next, target)?;
        if next.state.runs[index].memory_resources.as_ref() == Some(&resources) {
            return Ok(());
        }
        next.state.runs[index].memory_resources = Some(resources);
        next.state.runs[index].memory = Some(MemoryStatus {
            run_id: target.run_id.clone(),
            phase: MemoryPhase::Preparing,
            detail: "Preparing conversation memory".into(),
            recent_raw_turns: DecimalU64::new(0),
            retrieval_sources: vec![],
            reference_tokens: DecimalU64::new(0),
            retrieval_status: None,
            related_omission: None,
            main_usage: None,
            summary_usage: None,
            embedding_usage: None,
            total_usage: None,
        });
        self.commit(next, false)
    }

    pub fn record_memory_status(
        &mut self,
        target: &RunTarget,
        status: MemoryStatus,
    ) -> StoreResult<()> {
        validate_status(&status)?;
        let mut next = self.snapshot()?;
        let index = run_index(&next, target)?;
        if status.run_id != target.run_id
            || next.state.runs[index].memory_resources.is_none()
            || (status.retrieval_status.is_some()
                && next.state.runs[index].configuration.history_mode != HistoryMode::Structured10)
        {
            return Err(StoreError::RunConflict);
        }
        if next.state.runs[index].memory.as_ref() == Some(&status) {
            return Ok(());
        }
        next.state.runs[index].memory = Some(status);
        self.commit(next, false)
    }

    pub fn memory_snapshot(&self, target: &RunTarget) -> StoreResult<MemorySnapshot> {
        let p = self.snapshot()?;
        active(&p, target)?;
        self.memory_raw_prefix(p.marker.raw_offset, &p.marker.raw_hash)?;
        Ok(MemorySnapshot {
            conversation: project(
                &p,
                p.raw.clone(),
                p.marker.raw_offset,
                p.marker.raw_hash.clone(),
            )?,
            content_revision: p.marker.content_revision,
            configuration: p.state.configuration.clone(),
            policy_revision: p.state.policy_revision,
            workflow: p.state.workflow.clone(),
        })
    }
    pub fn check_memory_snapshot(
        &self,
        target: &RunTarget,
        expected: &MemorySnapshot,
    ) -> StoreResult<()> {
        let now = self.memory_snapshot(target)?;
        if now.content_revision != expected.content_revision
            || now.configuration != expected.configuration
            || now.policy_revision != expected.policy_revision
            || now.workflow != expected.workflow
            || now.conversation.state != expected.conversation.state
            || now.conversation.raw_hash != expected.conversation.raw_hash
            || now.conversation.raw_offset != expected.conversation.raw_offset
        {
            return Err(StoreError::CasConflict("memory snapshot"));
        }
        Ok(())
    }

    pub fn begin_summary(
        &mut self,
        target: &RunTarget,
        expected: &MemorySnapshot,
        turn: u64,
    ) -> StoreResult<SummaryWork> {
        self.check_memory_snapshot(target, expected)?;
        if expected.expired_turn()? != Some(turn) {
            return Err(StoreError::Corrupt("Summary target turn"));
        }
        let digest = turn_hash(&expected.conversation, turn)?;
        let mut next = self.snapshot()?;
        let prior = memory(&next)?
            .attempts
            .iter()
            .find(|a| a.source.turn.get() == turn)
            .cloned();
        if let Some(prior) = prior {
            if prior.source.turn_hash != digest {
                return Err(StoreError::Corrupt("Summary target source changed"));
            }
            let bytes = self.memory_raw_prefix(prior.source.raw_offset, &prior.source.raw_hash)?;
            let raw = bytes
                .split(|b| *b == b'\n')
                .filter(|line| !line.is_empty())
                .map(serde_json::from_slice)
                .collect::<Result<Vec<RawEventV2>, _>>()?;
            let source = project(&next, raw, prior.source.raw_offset, prior.source.raw_hash)?;
            if let Some(structured) = &prior.structured {
                if crate::conversation_memory::structured::structured_summary_intent(&source)?
                    .as_ref()
                    != Some(&structured.intent)
                {
                    return Err(StoreError::Corrupt("Saved structured source map"));
                }
                if let Some(response) = &structured.response {
                    crate::conversation_memory::structured::validate_saved_response(
                        &source,
                        turn,
                        &structured.intent,
                        &response.summary,
                        &response.delta,
                        &response.coverage,
                    )?;
                }
            }
            return Ok(SummaryWork {
                expected: expected.clone(),
                source,
                turn,
                summary: prior.summary,
                operation_id: prior.operation_id,
                newly_started: false,
                structured_intent: prior.structured.as_ref().map(|value| value.intent.clone()),
                structured_response: prior.structured.and_then(|value| value.response),
            });
        }
        fork::reserve(&next, turn, &next.marker.raw_hash)?;
        let operation_id = OperationId::new(format!("memory-{}-{turn}", digest))?;
        let index = run_index(&next, target)?;
        if next
            .state
            .runs
            .iter()
            .any(|r| r.operations.iter().any(|o| o.operation_id == operation_id))
        {
            return Err(StoreError::RunConflict);
        }
        next.state.runs[index].operations.push(Operation {
            operation_id: operation_id.clone(),
            result_id: None,
            mcp: None,
            prompt: None,
            recipe: None,
        });
        let source = SourceStamp {
            turn: DecimalU64::new(turn),
            turn_hash: digest,
            raw_offset: next.marker.raw_offset,
            raw_hash: next.marker.raw_hash.clone(),
            content_revision: next.marker.content_revision,
        };
        let structured_intent =
            if expected.conversation.state.history_mode == HistoryMode::Structured10 {
                Some(
                    crate::conversation_memory::structured::structured_summary_intent(
                        &expected.conversation,
                    )
                    .map_err(std::io::Error::other)?
                    .ok_or(StoreError::Corrupt("No structured extraction target"))?,
                )
            } else {
                None
            };
        next.state
            .conversation_memory
            .as_mut()
            .unwrap()
            .attempts
            .push(SummaryAttempt {
                source,
                run_id: target.run_id.clone(),
                attempt_id: target.attempt_id.clone(),
                operation_id: operation_id.clone(),
                summary: None,
                structured: structured_intent.clone().map(|intent| StructuredAttempt {
                    intent,
                    response: None,
                }),
            });
        self.commit(next, false)?;
        Ok(SummaryWork {
            expected: expected.clone(),
            source: expected.conversation.clone(),
            turn,
            summary: None,
            operation_id,
            newly_started: true,
            structured_intent,
            structured_response: None,
        })
    }

    pub fn save_summary(
        &mut self,
        target: &RunTarget,
        work: &SummaryWork,
        summary: &str,
    ) -> StoreResult<()> {
        self.check_memory_snapshot(target, &work.expected)?;
        crate::conversation_memory::summary_evidence(&work.source, work.turn, summary)?;
        let mut next = self.snapshot()?;
        let attempt = next
            .state
            .conversation_memory
            .as_mut()
            .ok_or(StoreError::NotFound)?
            .attempts
            .iter_mut()
            .find(|a| {
                a.operation_id == work.operation_id
                    && a.run_id == target.run_id
                    && a.attempt_id == target.attempt_id
            })
            .ok_or(StoreError::NotFound)?;
        if attempt.summary.as_deref().is_some_and(|old| old != summary) {
            return Err(StoreError::RunConflict);
        }
        attempt.summary = Some(summary.into());
        let index = run_index(&next, target)?;
        let operation = next.state.runs[index]
            .operations
            .iter_mut()
            .find(|o| o.operation_id == work.operation_id)
            .ok_or(StoreError::NotFound)?;
        operation.result_id = Some(ResultId::new(format!(
            "summary-{}",
            content_hash(summary.as_bytes())
        ))?);
        self.commit(next, false)
    }

    pub fn save_structured_response(
        &mut self,
        target: &RunTarget,
        work: &SummaryWork,
        response: &ValidatedStructuredResponse,
    ) -> StoreResult<()> {
        self.check_memory_snapshot(target, &work.expected)?;
        crate::conversation_memory::summary_evidence(&work.source, work.turn, &response.summary)?;
        let intent = work
            .structured_intent
            .as_ref()
            .ok_or(StoreError::RunConflict)?;
        crate::conversation_memory::structured::validate_saved_response(
            &work.source,
            work.turn,
            intent,
            &response.summary,
            &response.delta,
            &response.coverage,
        )?;
        if response.coverage != intent.coverage
            || response.delta.publication.raw_hash != work.source.raw_hash
            || response.delta.publication.scope.generation
                != number(
                    work.expected
                        .conversation
                        .state
                        .generation
                        .checked_add(1)
                        .ok_or(StoreError::Overflow)?,
                )?
        {
            return Err(StoreError::Corrupt(
                "Structured extraction result source or generation",
            ));
        }
        let saved = SavedStructuredResponse {
            summary: response.summary.clone(),
            delta: response.delta.clone(),
            coverage: response.coverage.clone(),
        };
        let mut next = self.snapshot()?;
        let attempt = next
            .state
            .conversation_memory
            .as_mut()
            .ok_or(StoreError::NotFound)?
            .attempts
            .iter_mut()
            .find(|a| {
                a.operation_id == work.operation_id
                    && a.run_id == target.run_id
                    && a.attempt_id == target.attempt_id
            })
            .ok_or(StoreError::NotFound)?;
        let structured = attempt.structured.as_mut().ok_or(StoreError::RunConflict)?;
        if structured.intent != *intent
            || structured
                .response
                .as_ref()
                .is_some_and(|old| old != &saved)
        {
            return Err(StoreError::RunConflict);
        }
        attempt.summary = Some(saved.summary.clone());
        structured.response = Some(saved.clone());
        let index = run_index(&next, target)?;
        let operation = next.state.runs[index]
            .operations
            .iter_mut()
            .find(|o| o.operation_id == work.operation_id)
            .ok_or(StoreError::NotFound)?;
        operation.result_id = Some(ResultId::new(format!(
            "summary-{}",
            content_hash(saved.summary.as_bytes())
        ))?);
        self.commit(next, false)
    }

    /// The pending index and raw-prefix provenance are checked before one v3 publication.
    pub fn publish_memory(
        &mut self,
        target: &RunTarget,
        work: &SummaryWork,
        pending: &PendingSummary,
        database: &Path,
    ) -> StoreResult<()> {
        self.check_memory_snapshot(target, &work.expected)?;
        if work.structured_intent.is_some() {
            return Err(StoreError::RunConflict);
        }
        let mut next = self.snapshot()?;
        let m = memory(&next)?;
        let attempt = m
            .attempts
            .iter()
            .find(|a| a.operation_id == work.operation_id)
            .ok_or(StoreError::NotFound)?
            .clone();
        let generation = m.generation.checked_add(1)?;
        let suffix = &attempt.source.raw_hash[..16];
        let mut view = work.expected.view()?;
        view.scope.generation = number(generation.get())?;
        if pending.scope != view.scope
            || pending.id != format!("strict10-{}-{suffix}", work.turn)
            || pending.source.id != format!("source-{}-{suffix}", work.turn)
            || pending.source.start_turn != number(work.turn)?
            || pending.source.end_turn != number(work.turn)?
            || pending.source.raw_hash != attempt.source.raw_hash
            || attempt.summary.as_deref() != Some(&pending.summary)
            || pending.embedding.model != m.resources.embedding_model
            || pending.embedding.revision != m.resources.embedding_revision
            || pending.embedding.dimension != i64::from(m.resources.embedding_dimension)
            || pending.model != "gpt-6-astra"
            || pending.effort != "medium"
            || pending.prompt_version != "strict10-v1"
        {
            return Err(StoreError::Corrupt("Published memory content"));
        }
        self.memory_raw_prefix(attempt.source.raw_offset, &attempt.source.raw_hash)?;
        view.visible_ids.push(pending.id.clone());
        let m = next.state.conversation_memory.as_mut().unwrap();
        m.generation = generation;
        m.sources.push(PublishedSource {
            source: attempt.source,
            generation,
            summary_id: pending.id.clone(),
            source_id: pending.source.id.clone(),
            structured_publication_id: None,
            structured_coverage: None,
        });
        let mut db = MemoryStore::open(database).map_err(std::io::Error::other)?;
        db.insert_pending_summary(pending)
            .map_err(std::io::Error::other)?;
        frozen::publish(self, &mut db, &view, &pending.id, None, next)
    }

    pub fn publish_structured_memory(
        &mut self,
        target: &RunTarget,
        work: &SummaryWork,
        pending: &PendingSummary,
        response: &SavedStructuredResponse,
        database: &Path,
    ) -> StoreResult<()> {
        self.check_memory_snapshot(target, &work.expected)?;
        crate::conversation_memory::structured::validate_saved_response(
            &work.source,
            work.turn,
            work.structured_intent
                .as_ref()
                .ok_or(StoreError::RunConflict)?,
            &response.summary,
            &response.delta,
            &response.coverage,
        )?;
        let mut next = self.snapshot()?;
        let attempt = memory(&next)?
            .attempts
            .iter()
            .find(|a| a.operation_id == work.operation_id)
            .ok_or(StoreError::NotFound)?
            .clone();
        if attempt.summary.as_deref() != Some(&pending.summary)
            || attempt
                .structured
                .as_ref()
                .and_then(|value| value.response.as_ref())
                != Some(response)
            || response.delta.publication.raw_hash != attempt.source.raw_hash
        {
            return Err(StoreError::Corrupt("Published structured memory content"));
        }
        let generation = memory(&next)?.generation.checked_add(1)?;
        let suffix = &attempt.source.raw_hash[..16];
        let mut view = work.expected.view()?;
        view.scope.generation = number(generation.get())?;
        if pending.scope != view.scope
            || pending.id != format!("strict10-{}-{suffix}", work.turn)
            || pending.source.id != format!("source-{}-{suffix}", work.turn)
            || pending.source.raw_hash != attempt.source.raw_hash
            || response.delta.publication.scope
                != (polaris_memory::structured::PublicationScope {
                    project_id: view.scope.project_id.clone(),
                    session_id: view.scope.session_id.clone(),
                    epoch: view.scope.epoch,
                    generation: view.scope.generation,
                })
        {
            return Err(StoreError::Corrupt("Structured memory scope"));
        }
        self.memory_raw_prefix(attempt.source.raw_offset, &attempt.source.raw_hash)?;
        view.visible_ids.push(pending.id.clone());
        let memory = next.state.conversation_memory.as_mut().unwrap();
        memory.generation = generation;
        memory.sources.push(PublishedSource {
            source: attempt.source,
            generation,
            summary_id: pending.id.clone(),
            source_id: pending.source.id.clone(),
            structured_publication_id: Some(response.delta.publication.publication_id.clone()),
            structured_coverage: Some(response.coverage.clone()),
        });
        let mut db = MemoryStore::open(database).map_err(std::io::Error::other)?;
        db.enable_structured_memory()
            .map_err(std::io::Error::other)?;
        db.insert_pending_summary(pending)
            .map_err(std::io::Error::other)?;
        frozen::publish(
            self,
            &mut db,
            &view,
            &pending.id,
            Some(&response.delta),
            next,
        )
    }

    pub fn read_memory_source(
        &self,
        target: &RunTarget,
        database: &Path,
        uri: &str,
    ) -> StoreResult<String> {
        let expected = self.memory_snapshot(target)?;
        let db = MemoryStore::open(database).map_err(std::io::Error::other)?;
        frozen::with_receipts(self, target, &expected, &db, |owners| {
            if uri.starts_with("conversation://structured/") {
                let (publication, source, offset) =
                    crate::conversation_memory::structured::sources::parse_source_uri(uri)?;
                return frozen::resolve_source(
                    owners,
                    &db,
                    &expected.structured_view()?,
                    &publication,
                    &source,
                )?
                .page(offset)
                .map_err(Into::into);
            }
            let (id, requested, offset) = crate::conversation_memory::parse_uri(uri)?;
            db.with_conversation_source(&expected.view()?, &id, |reference| {
                let read = || -> StoreResult<String> {
                    let scope = polaris_memory::structured::PublicationScope {
                        project_id: reference.scope.project_id.clone(),
                        session_id: reference.scope.session_id.clone(),
                        epoch: reference.scope.epoch,
                        generation: reference.scope.generation,
                    };
                    let owner = owners
                        .iter()
                        .find(|owner| owner.matches(&scope))
                        .ok_or(StoreError::NotFound)?;
                    let proof = owner
                        .summaries
                        .get(&reference.summary_id)
                        .ok_or(StoreError::NotFound)?;
                    let receipt = &proof.source;
                    if reference.scope.generation != number(receipt.generation.get())?
                        || reference.source.id != receipt.source_id
                        || reference.source.raw_hash != receipt.source.raw_hash
                        || reference.source.start_turn != number(receipt.source.turn.get())?
                        || reference.source.end_turn != reference.source.start_turn
                    {
                        return Err(StoreError::Corrupt(
                            "Source reference does not match saved source",
                        ));
                    }
                    let (start, end) = requested
                        .unwrap_or((reference.source.start_turn, reference.source.end_turn));
                    if start != reference.source.start_turn || end != reference.source.end_turn {
                        return Err(StoreError::Corrupt("Source reference range mismatch"));
                    }
                    let events = owner.summary_raw(proof)?;
                    let payload = serde_json::to_string(&events)?;
                    crate::conversation_memory::source_page(&id, start, end, offset, &payload)
                        .map_err(Into::into)
                };
                Ok(read())
            })
            .map_err(std::io::Error::other)?
        })
    }

    /// Mandatory-only comparison for owner regression tests. Production uses
    /// the shared mandatory/related transaction in `related`.
    #[cfg(test)]
    pub(crate) fn prepare_structured_memory(
        &self,
        target: &RunTarget,
        expected: &MemorySnapshot,
        database: &Path,
    ) -> StoreResult<crate::conversation_memory::structured::packing::StructuredContext> {
        // Reject invalid owners and projections before creating or migrating
        // the automatic path's database. The shared read path checks again.
        self.check_memory_snapshot(target, expected)?;
        expected.structured_view()?;
        expected.related_query()?;
        let db = MemoryStore::open(database).map_err(std::io::Error::other)?;
        self.prepare_structured_memory_from_store(target, expected, &db)
    }

    #[cfg(test)]
    fn prepare_structured_memory_from_store(
        &self,
        target: &RunTarget,
        expected: &MemorySnapshot,
        db: &MemoryStore,
    ) -> StoreResult<crate::conversation_memory::structured::packing::StructuredContext> {
        frozen::with_receipts(self, target, expected, db, |owners| {
            self.prepare_structured_memory_from_receipts(expected, db, owners)
        })
    }

    fn prepare_structured_memory_from_receipts(
        &self,
        expected: &MemorySnapshot,
        db: &MemoryStore,
        owners: &[frozen::FrozenMemory<'_>],
    ) -> StoreResult<crate::conversation_memory::structured::packing::StructuredContext> {
        let view = expected.structured_view()?;
        crate::conversation_memory::structured::packing::prepare_context(
            db,
            &expected.conversation,
            &view,
            expected.related_query()?,
            |publication, source| {
                frozen::resolve_source(owners, db, &view, &publication.publication_id, source)
                    .map_err(std::io::Error::other)
            },
        )
        .map_err(Into::into)
    }
}

#[cfg(all(test, unix))]
mod tests;
