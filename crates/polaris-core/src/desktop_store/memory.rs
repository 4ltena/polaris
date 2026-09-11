//! strict10のv3公開境界。モデルを呼ばず、v2 writerを開かない。
use super::*;
use crate::conversation_state::{
    ConversationSnapshot, ConversationStateV2, RawEventV2, content_hash,
};
use polaris_desktop_protocol::{
    ids::*,
    request::RunTarget,
    snapshot::{HistoryMode, MemoryPhase, MemoryStatus},
};
use polaris_memory::{
    MemoryStore,
    conversation::{PendingSummary, PublishedView, Scope},
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, path::Path};

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
            return Err(StoreError::Corrupt("記憶資源の識別子"));
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
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishedSource {
    source: SourceStamp,
    generation: DecimalU64,
    summary_id: String,
    source_id: String,
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
            ancestors: vec![],
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
        .ok_or(StoreError::Corrupt("記憶設定が未接続"))
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
        || run.configuration.history_mode != HistoryMode::Strict10
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
            schema_version: 2,
            project_id: p.marker.project_id.as_str().into(),
            session_id: p.marker.session_id.as_str().into(),
            epoch: p.marker.epoch.get(),
            generation: m.generation.get(),
            raw_commit_offset: offset.get(),
            raw_commit_hash: raw_hash.clone(),
            history_mode: HistoryMode::Strict10,
            recent_turn_ids: recent,
            visible_summary_ids: m.sources.iter().map(|s| s.summary_id.clone()).collect(),
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
            ancestors: vec![],
        },
        raw_hash,
        raw_offset: offset.get(),
        events: raw,
    })
}

pub(super) fn validate(p: &Published) -> StoreResult<()> {
    for run in &p.state.runs {
        if let Some(status) = &run.memory {
            validate_status(status)?;
            if status.run_id != run.run.run_id
                || run.configuration.history_mode != HistoryMode::Strict10
                || run.memory_resources.is_none()
            {
                return Err(StoreError::Corrupt("実行の記憶状態"));
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
        return Err(StoreError::Corrupt("記憶の保存版・世代"));
    }
    let mut turns = BTreeSet::new();
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
            return Err(StoreError::Corrupt("記憶の公開出典"));
        }
    }
    let mut attempts = BTreeSet::new();
    for attempt in &m.attempts {
        if !attempts.insert(attempt.source.turn) {
            return Err(StoreError::Corrupt("要約の重複"));
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
            .ok_or(StoreError::Corrupt("要約の実行参照"))?;
        if let Some(summary) = &attempt.summary {
            crate::conversation_memory::validate_summary(summary, attempt.source.turn.get())?;
            if op.result_id.is_none() {
                return Err(StoreError::Corrupt("要約結果が未確定"));
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
            return Err(StoreError::Corrupt("要約の原文参照"));
        }
    }
    Ok(())
}

fn validate_status(status: &MemoryStatus) -> StoreResult<()> {
    if status.detail.len() > 1024
        || status.recent_raw_turns.get() > 10
        || status.retrieval_sources.len() > 3
        || status.reference_tokens.get() > 768
        || status
            .retrieval_sources
            .iter()
            .any(|uri| uri.len() > 1024 || crate::conversation_memory::parse_uri(uri).is_err())
    {
        return Err(StoreError::Corrupt("記憶状態の上限"));
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
            detail: "会話記憶を準備しています".into(),
            recent_raw_turns: DecimalU64::new(0),
            retrieval_sources: vec![],
            reference_tokens: DecimalU64::new(0),
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
        if status.run_id != target.run_id || next.state.runs[index].memory_resources.is_none() {
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
            return Err(StoreError::Corrupt("要約対象のターン"));
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
                return Err(StoreError::Corrupt("要約対象の原文変更"));
            }
            let bytes = self.memory_raw_prefix(prior.source.raw_offset, &prior.source.raw_hash)?;
            let raw = bytes
                .split(|b| *b == b'\n')
                .filter(|line| !line.is_empty())
                .map(serde_json::from_slice)
                .collect::<Result<Vec<RawEventV2>, _>>()?;
            let source = project(&next, raw, prior.source.raw_offset, prior.source.raw_hash)?;
            return Ok(SummaryWork {
                expected: expected.clone(),
                source,
                turn,
                summary: prior.summary,
                operation_id: prior.operation_id,
                newly_started: false,
            });
        }
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
        });
        let source = SourceStamp {
            turn: DecimalU64::new(turn),
            turn_hash: digest,
            raw_offset: next.marker.raw_offset,
            raw_hash: next.marker.raw_hash.clone(),
            content_revision: next.marker.content_revision,
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
            });
        self.commit(next, false)?;
        Ok(SummaryWork {
            expected: expected.clone(),
            source: expected.conversation.clone(),
            turn,
            summary: None,
            operation_id,
            newly_started: true,
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

    /// The pending index and raw-prefix provenance are checked before one v3 publication.
    pub fn publish_memory(
        &mut self,
        target: &RunTarget,
        work: &SummaryWork,
        pending: &PendingSummary,
        database: &Path,
    ) -> StoreResult<()> {
        self.check_memory_snapshot(target, &work.expected)?;
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
            return Err(StoreError::Corrupt("記憶の公開内容"));
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
        });
        let mut db = MemoryStore::open(database).map_err(std::io::Error::other)?;
        db.insert_pending_summary(pending)
            .map_err(std::io::Error::other)?;
        db.publish_pending(&view, std::slice::from_ref(&pending.id), || {
            self.commit(next, false)
                .map_err(|e| polaris_memory::Error::Io(std::io::Error::other(e)))
        })
        .map_err(std::io::Error::other)?;
        Ok(())
    }

    pub fn read_memory_source(
        &self,
        target: &RunTarget,
        database: &Path,
        uri: &str,
    ) -> StoreResult<String> {
        let expected = self.memory_snapshot(target)?;
        let (id, requested, offset) = crate::conversation_memory::parse_uri(uri)?;
        let p = self.snapshot()?;
        let receipt = memory(&p)?
            .sources
            .iter()
            .find(|s| s.source_id == id)
            .ok_or(StoreError::NotFound)?;
        let view = expected.view()?;
        let db = MemoryStore::open(database).map_err(std::io::Error::other)?;
        db.with_conversation_source(&view, &id, |reference| {
            let fail = || polaris_memory::Error::InvalidInput("v3 source scope/hash mismatch");
            if reference.scope.project_id != view.scope.project_id
                || reference.scope.session_id != view.scope.session_id
                || reference.scope.epoch != view.scope.epoch
                || reference.scope.generation != receipt.generation.get() as i64
                || reference.source.raw_hash != receipt.source.raw_hash
                || reference.source.start_turn != receipt.source.turn.get() as i64
                || reference.source.end_turn != reference.source.start_turn
            {
                return Err(fail());
            }
            let (start, end) =
                requested.unwrap_or((reference.source.start_turn, reference.source.end_turn));
            if start != reference.source.start_turn || end != reference.source.end_turn {
                return Err(fail());
            }
            let bytes = self
                .memory_raw_prefix(receipt.source.raw_offset, &receipt.source.raw_hash)
                .map_err(|e| polaris_memory::Error::Io(std::io::Error::other(e)))?;
            let events = bytes
                .split(|b| *b == b'\n')
                .filter(|line| !line.is_empty())
                .map(serde_json::from_slice)
                .collect::<Result<Vec<RawEventV2>, _>>()
                .map_err(|_| fail())?;
            let events: Vec<_> = events
                .iter()
                .filter(|e| {
                    e.epoch == p.marker.epoch.get() && e.turn_id == receipt.source.turn.get()
                })
                .collect();
            if content_hash(&serde_json::to_vec(&events).map_err(|_| fail())?)
                != receipt.source.turn_hash
            {
                return Err(fail());
            }
            let payload = serde_json::to_string(&events).map_err(|_| fail())?;
            crate::conversation_memory::source_page(&id, start, end, offset, &payload)
                .map_err(polaris_memory::Error::Io)
        })
        .map_err(std::io::Error::other)
        .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests;
