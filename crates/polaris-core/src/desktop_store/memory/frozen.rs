//! Frozen memory evidence only; this never recovers or executes a parent ledger.
use super::*;
use crate::desktop_store::{disk, writer::MemoryDirectory};
use polaris_memory::structured::{
    PublicationScope, StructuredPublication, StructuredPublicationView,
};
use polaris_provider::Role;
use serde::de::DeserializeSeed;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufReader, Read, Seek, SeekFrom},
};

mod bounded;
mod initialize;
mod publication;
pub(super) use initialize::initialize;
mod prompt;
mod raw;
#[cfg(test)]
pub(super) use publication::before_marker;
pub(super) use publication::publish;
mod history;
mod sidecar;
pub(in crate::desktop_store) use history::with_inherited_prompt_attachments;

type OperationKey = (RunId, AttemptId, OperationId);

fn charge(used: &mut usize, value: &impl Serialize) -> StoreResult<()> {
    let count = bounded::encoded_size(value, bounded::RECORD.saturating_sub(*used))?;
    *used = used
        .checked_add(count)
        .and_then(|v| v.checked_add(2))
        .filter(|v| *v <= bounded::RECORD)
        .ok_or(StoreError::Overflow)?;
    Ok(())
}

#[derive(Serialize)]
struct AttemptProof {
    source: SourceStamp,
    operation: OperationKey,
    summary_hash: Option<String>,
    coverage: Option<StructuredCoverage>,
    source_count: usize,
    last_source: Option<u64>,
    publication: Option<StructuredPublication>,
}
#[derive(Serialize, PartialEq, Eq)]
struct RawBinding {
    role: Role,
    source_id: String,
    content_hash: String,
    requires_user_input: bool,
}

struct Collector<'a> {
    marker: &'a Marker,
    grants: BTreeSet<&'a str>,
    used: usize,
    revision: Option<DecimalU64>,
    mode: Option<HistoryMode>,
    memory_version: Option<u32>,
    epoch: Option<DecimalU64>,
    generation: Option<DecimalU64>,
    resources: Option<MemoryResources>,
    sources: BTreeMap<u64, PublishedSource>,
    attempts: BTreeMap<u64, AttemptProof>,
    operations: BTreeMap<OperationKey, bool>,
    bindings: BTreeMap<u64, RawBinding>,
    run_count: u64,
    all_runs_terminal: bool,
    inherited_turn_count: u64,
    ancestors: Vec<fork::FrozenAncestor>,
    prompts: BTreeMap<u64, prompt::Selected>,
    prompt_operations: BTreeSet<OperationId>,
    approvals: BTreeSet<OperationId>,
}
impl<'a> Collector<'a> {
    fn new(marker: &'a Marker, ids: &'a [String]) -> Self {
        Self {
            marker,
            grants: ids.iter().map(String::as_str).collect(),
            used: 3 * bounded::SOURCE,
            revision: None,
            mode: None,
            memory_version: None,
            epoch: None,
            generation: None,
            resources: None,
            sources: BTreeMap::new(),
            attempts: BTreeMap::new(),
            operations: BTreeMap::new(),
            bindings: BTreeMap::new(),
            run_count: 0,
            all_runs_terminal: true,
            inherited_turn_count: 0,
            ancestors: Vec::new(),
            prompts: BTreeMap::new(),
            prompt_operations: BTreeSet::new(),
            approvals: BTreeSet::new(),
        }
    }
    fn charge(&mut self, value: &impl Serialize) -> StoreResult<()> {
        charge(&mut self.used, value)
    }
    fn source(&mut self, source: PublishedSource) -> StoreResult<()> {
        if source.generation.get() != self.sources.len() as u64 + 1
            || source.source.turn.get() == 0
            || !source
                .summary_id
                .starts_with(&format!("strict10-{}-", source.source.turn.get()))
            || !source
                .source_id
                .starts_with(&format!("source-{}-", source.source.turn.get()))
        {
            return Err(StoreError::Corrupt("固定記憶の公開出典"));
        }
        self.stamp(&source.source)?;
        self.charge(&source)?;
        if self
            .sources
            .insert(source.source.turn.get(), source)
            .is_some()
        {
            return Err(StoreError::Corrupt("固定記憶の出典重複"));
        }
        Ok(())
    }
    fn stamp(&self, source: &SourceStamp) -> StoreResult<()> {
        if source.turn.get() == 0
            || !hash(&source.turn_hash)
            || !hash(&source.raw_hash)
            || source.raw_offset.get() == 0
            || source.raw_offset > self.marker.raw_offset
            || source.content_revision > self.marker.content_revision
        {
            return Err(StoreError::Corrupt("固定記憶の原文参照"));
        }
        Ok(())
    }
    fn run(&mut self, run: RunRecord) -> StoreResult<()> {
        self.run_count = self.run_count.checked_add(1).ok_or(StoreError::Overflow)?;
        self.all_runs_terminal &= run.run.state.is_terminal();
        if let Some(selected) = prompt::Selected::from_record(&run)? {
            self.charge(&(self.run_count, &selected))?;
            self.prompts.insert(self.run_count, selected);
        }
        if let Some(roles) = &run.role_bindings {
            roles.validate()?;
        }
        if let Some(account) = &run.account {
            account.validate()?;
        }
        if let Some(workflow) = &run.workflow {
            workflow.validate()?;
        }
        for op in run.operations {
            if op.prompt.is_some() {
                self.charge(&op.operation_id)?;
                self.prompt_operations.insert(op.operation_id.clone());
            }
            let key = (
                run.run.run_id.clone(),
                run.run.attempt_id.clone(),
                op.operation_id,
            );
            let complete = op.result_id.is_some();
            self.charge(&(&key, complete))?;
            if self.operations.insert(key, complete).is_some() {
                return Err(StoreError::Corrupt("固定記憶の実行参照重複"));
            }
        }
        Ok(())
    }
    fn attempt(
        &mut self,
        attempt: SummaryAttempt,
        submit: &mut dyn FnMut(&StructuredDelta) -> polaris_memory::Result<()>,
    ) -> StoreResult<()> {
        self.stamp(&attempt.source)?;
        if let Some(summary) = &attempt.summary {
            crate::conversation_memory::validate_summary(summary, attempt.source.turn.get())?;
        }
        let mut proof = AttemptProof {
            source: attempt.source,
            operation: (attempt.run_id, attempt.attempt_id, attempt.operation_id),
            summary_hash: attempt.summary.as_ref().map(|s| content_hash(s.as_bytes())),
            coverage: None,
            source_count: 0,
            last_source: None,
            publication: None,
        };
        if let Some(structured) = attempt.structured {
            crate::conversation_memory::structured::source_budget(&structured.intent.sources)?;
            for source in &structured.intent.sources {
                if proof
                    .last_source
                    .is_some_and(|previous| previous >= source.sequence)
                    || source.sequence == 0
                    || source.source_id
                        != format!(
                            "source-v3:{}:{}:{}:{}",
                            self.marker.project_id.as_str(),
                            self.marker.session_id.as_str(),
                            self.marker.epoch.get(),
                            source.sequence
                        )
                {
                    return Err(StoreError::Corrupt("固定記憶の構造化source順序"));
                }
                proof.last_source = Some(source.sequence);
                let binding = RawBinding {
                    role: source.role,
                    source_id: source.source_id.clone(),
                    content_hash: content_hash(source.content.as_bytes()),
                    requires_user_input: false,
                };
                if let Some(prior) = self.bindings.get(&source.sequence) {
                    if prior.role != binding.role
                        || prior.source_id != binding.source_id
                        || prior.content_hash != binding.content_hash
                    {
                        return Err(StoreError::Corrupt("固定記憶のsource対応重複"));
                    }
                } else {
                    self.charge(&(source.sequence, &binding))?;
                    self.bindings.insert(source.sequence, binding);
                }
            }
            proof.source_count = structured.intent.sources.len();
            let coverage = &structured.intent.coverage;
            if coverage.epoch != self.marker.epoch.get()
                || !hash(&coverage.prefix_hash)
                || coverage.raw_offset > proof.source.raw_offset.get()
                || (proof.source_count > 0
                    && proof.last_source != Some(coverage.last_source_sequence))
            {
                return Err(StoreError::Corrupt("固定記憶のcoverage"));
            }
            proof.coverage = Some(coverage.clone());
            if let Some(response) = structured.response {
                polaris_memory::structured::validate_delta(&response.delta)
                    .map_err(std::io::Error::other)?;
                for (id, requires_user) in
                    crate::conversation_memory::structured::source_requirements(&response.delta)?
                {
                    let source = structured
                        .intent
                        .sources
                        .iter()
                        .find(|source| source.source_id == id)
                        .ok_or(StoreError::Corrupt("固定記憶の構造化出典が抽出範囲外"))?;
                    let binding = self
                        .bindings
                        .get_mut(&source.sequence)
                        .ok_or(StoreError::Corrupt("固定記憶の構造化出典不在"))?;
                    // false occupies more encoded bytes than true, so the
                    // original binding charge remains an upper bound.
                    binding.requires_user_input |= requires_user;
                }
                let p = &response.delta.publication;
                if response.coverage != *coverage
                    || attempt.summary.as_ref() != Some(&response.summary)
                    || p.raw_hash != proof.source.raw_hash
                    || p.scope.project_id != self.marker.project_id.as_str()
                    || p.scope.session_id != self.marker.session_id.as_str()
                    || u64::try_from(p.scope.epoch).ok() != Some(self.marker.epoch.get())
                    || p.prompt_version
                        != crate::conversation_memory::structured::STRUCTURED_PROMPT_VERSION
                {
                    return Err(StoreError::Corrupt("固定記憶の構造化保存応答"));
                }
                if self.grants.contains(p.publication_id.as_str()) {
                    submit(&response.delta).map_err(std::io::Error::other)?;
                }
                proof.publication = Some(p.clone());
            }
        }
        self.charge(&proof)?;
        if self
            .attempts
            .insert(proof.source.turn.get(), proof)
            .is_some()
        {
            return Err(StoreError::Corrupt("固定記憶のattempt重複"));
        }
        Ok(())
    }
    fn finish(
        &self,
        generation: u64,
        summary_ids: &[String],
        structured_ids: &[String],
    ) -> StoreResult<()> {
        if !self.prompt_operations.is_disjoint(&self.approvals) {
            return Err(StoreError::Corrupt("MCP prompt操作の対応"));
        }
        if self.revision != Some(self.marker.session_revision)
            || self.memory_version != Some(1)
            || self.epoch != Some(self.marker.epoch)
            || self.generation.map(DecimalU64::get) != Some(generation)
            || generation != self.sources.len() as u64
            || !matches!(
                self.mode,
                Some(HistoryMode::Strict10 | HistoryMode::Structured10)
            )
        {
            return Err(StoreError::Corrupt("固定記憶の版・世代"));
        }
        self.resources
            .as_ref()
            .ok_or(StoreError::Corrupt("固定記憶の資源不在"))?
            .validate()?;
        let summaries: BTreeSet<_> = self.sources.values().map(|s| &s.summary_id).collect();
        let structured: BTreeSet<_> = self
            .sources
            .values()
            .filter_map(|s| s.structured_publication_id.as_ref())
            .collect();
        if summaries.len() != self.sources.len()
            || summaries != summary_ids.iter().collect()
            || summaries.len() != summary_ids.len()
            || structured.len() != structured_ids.len()
            || structured != structured_ids.iter().collect()
        {
            return Err(StoreError::Corrupt("固定記憶のexact公開ID"));
        }
        for source in self.sources.values() {
            let attempt = self
                .attempts
                .get(&source.source.turn.get())
                .ok_or(StoreError::Corrupt("固定記憶のattempt不在"))?;
            if attempt.source != source.source || attempt.summary_hash.is_none() {
                return Err(StoreError::Corrupt("固定記憶の出典対応"));
            }
            match (
                &attempt.publication,
                &source.structured_publication_id,
                &source.structured_coverage,
            ) {
                (Some(p), Some(id), Some(coverage))
                    if &p.publication_id == id
                        && attempt.coverage.as_ref() == Some(coverage)
                        && u64::try_from(p.scope.generation).ok()
                            == Some(source.generation.get()) => {}
                (None, None, None) if attempt.coverage.is_none() => {}
                _ => return Err(StoreError::Corrupt("固定記憶の公開応答対応")),
            }
        }
        for attempt in self.attempts.values() {
            let complete = self
                .operations
                .get(&attempt.operation)
                .ok_or(StoreError::Corrupt("固定記憶の実行参照不在"))?;
            if attempt.summary_hash.is_some() && !complete {
                return Err(StoreError::Corrupt("固定記憶の結果未確定"));
            }
            if attempt.source_count == 0
                && let Some(coverage) = &attempt.coverage
                && (coverage.raw_offset != attempt.source.raw_offset.get()
                    || coverage.prefix_hash != attempt.source.raw_hash
                    || !self.sources.values().any(|s| {
                        s.source.turn < attempt.source.turn
                            && s.structured_publication_id.is_some()
                            && s.structured_coverage.as_ref() == Some(coverage)
                            && s.source.raw_offset == attempt.source.raw_offset
                            && s.source.raw_hash == attempt.source.raw_hash
                    }))
            {
                return Err(StoreError::Corrupt("固定記憶の空抽出coverage"));
            }
        }
        Ok(())
    }
}

struct Files<'a> {
    owner: MemoryDirectory<'a>,
    marker: Marker,
    raw: File,
    work: bounded::ReadWork,
}
impl Files<'_> {
    fn raw(&self) -> StoreResult<File> {
        self.owner.verify()?;
        let file = self.owner.directory.open(disk::RAW, false, false)?;
        disk::same(&self.raw, &file)?;
        Ok(file)
    }
}

#[derive(Serialize)]
pub(super) struct SummaryProof {
    pub(super) source: PublishedSource,
    summary_hash: String,
    publication: Option<StructuredPublication>,
}
pub(super) struct FrozenMemory<'a> {
    files: Files<'a>,
    scope: PublicationScope,
    pub(super) resources: MemoryResources,
    pub(super) summaries: BTreeMap<String, SummaryProof>,
}
impl<'a> FrozenMemory<'a> {
    pub(super) fn load(
        writer: &'a Writer,
        work: bounded::ReadWork,
        grant: &fork::FrozenAncestor,
        submit: &mut dyn FnMut(&StructuredDelta) -> polaris_memory::Result<()>,
    ) -> StoreResult<Self> {
        Self::load_with_history(writer, work, grant, submit).map(|(owner, _)| owner)
    }

    fn load_with_history(
        writer: &'a Writer,
        work: bounded::ReadWork,
        grant: &fork::FrozenAncestor,
        submit: &mut dyn FnMut(&StructuredDelta) -> polaris_memory::Result<()>,
    ) -> StoreResult<(Self, history::Proof)> {
        let marker = &grant.marker;
        let generation = grant.generation.get();
        let summary_ids = &grant.summary_ids;
        let structured_ids = &grant.structured_ids;
        if &marker.project_id != writer.coordinates().0 {
            return Err(StoreError::TargetMismatch);
        }
        let scope = PublicationScope {
            project_id: marker.project_id.as_str().into(),
            session_id: marker.session_id.as_str().into(),
            epoch: number(marker.epoch.get())?,
            generation: number(generation)?,
        };
        let owner = writer.memory_directory(&marker.session_id)?;
        let mut bytes = Vec::new();
        owner
            .directory
            .open(disk::MARKER, false, false)?
            .take(8193)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 8192 {
            return Err(StoreError::Overflow);
        }
        let current: Marker = serde_json::from_slice(&bytes)?;
        if current.project_id != marker.project_id
            || current.session_id != marker.session_id
            || current.epoch != marker.epoch
        {
            return Err(StoreError::TargetMismatch);
        }
        if current.deleted {
            return Err(StoreError::Deleted);
        }
        if current.schema_version != 3
            || marker.schema_version != 3
            || marker.deleted
            || current.session_revision < marker.session_revision
            || current.content_revision < marker.content_revision
            || current.raw_offset < marker.raw_offset
            || current.sidecar_offset < marker.sidecar_offset
            || current.content_revision > current.session_revision
            || marker.content_revision > marker.session_revision
            || !hash(&current.raw_hash)
            || !hash(&current.sidecar_hash)
            || (writer.coordinates().1 == &marker.session_id && current != *marker)
        {
            return Err(StoreError::Corrupt("固定記憶の現在marker"));
        }
        let (mut side, start, lines) = checked_prefix(
            &owner,
            &work,
            disk::SIDECAR,
            marker.sidecar_offset.get(),
            &marker.sidecar_hash,
        )?;
        if lines
            != marker
                .session_revision
                .get()
                .checked_add(1)
                .ok_or(StoreError::Overflow)?
        {
            return Err(StoreError::Corrupt("固定記憶の付随世代"));
        }
        // The decoder is restricted to this exact final-line length. Reserve
        // it before serde can repeatedly rearm per-record budgets.
        work.charge(marker.sidecar_offset.get() - start)?;
        side.seek(SeekFrom::Start(start))?;
        let mut collector = Collector::new(marker, structured_ids);
        let budget = bounded::Budget::default();
        budget.arm(256)?;
        let reader = bounded::Limited {
            inner: BufReader::with_capacity(
                bounded::BUFFER,
                side.take(marker.sidecar_offset.get() - start),
            ),
            budget: &budget,
        };
        let mut parser = serde_json::Deserializer::from_reader(reader);
        sidecar::SidecarSeed {
            budget: &budget,
            collector: &mut collector,
            submit,
        }
        .deserialize(&mut parser)?;
        budget.arm(256)?;
        parser.end()?;
        collector.finish(generation, summary_ids, structured_ids)?;
        let files = Files {
            raw: owner.directory.open(disk::RAW, false, false)?,
            owner,
            marker: marker.clone(),
            work,
        };
        let attachments = raw::validate(&files, &collector)?;
        let history = history::Proof {
            run_count: collector.run_count,
            inherited: collector.inherited_turn_count,
            all_terminal: collector.all_runs_terminal,
            ancestors: collector.ancestors,
            attachments,
        };
        let mut summaries = BTreeMap::new();
        for source in collector.sources.into_values() {
            let attempt = collector
                .attempts
                .remove(&source.source.turn.get())
                .ok_or(StoreError::Corrupt("固定記憶のattempt不在"))?;
            summaries.insert(
                source.summary_id.clone(),
                SummaryProof {
                    source,
                    summary_hash: attempt
                        .summary_hash
                        .ok_or(StoreError::Corrupt("固定記憶の要約不在"))?,
                    publication: attempt.publication,
                },
            );
        }
        Ok((
            Self {
                files,
                scope,
                resources: collector
                    .resources
                    .ok_or(StoreError::Corrupt("固定記憶の資源不在"))?,
                summaries,
            },
            history,
        ))
    }
    pub(super) fn matches(&self, scope: &PublicationScope) -> bool {
        self.scope.project_id == scope.project_id
            && self.scope.session_id == scope.session_id
            && self.scope.epoch == scope.epoch
            && scope.generation <= self.scope.generation
    }
    pub(super) fn verify_summary(&self, pending: &PendingSummary) -> StoreResult<&SummaryProof> {
        let source = self
            .summaries
            .get(&pending.id)
            .ok_or(StoreError::NotFound)?;
        let receipt = &source.source;
        let scope = PublicationScope {
            project_id: pending.scope.project_id.clone(),
            session_id: pending.scope.session_id.clone(),
            epoch: pending.scope.epoch,
            generation: pending.scope.generation,
        };
        if !self.matches(&scope)
            || number(receipt.generation.get())? != pending.scope.generation
            || pending.source.id != receipt.source_id
            || pending.source.start_turn != number(receipt.source.turn.get())?
            || pending.source.end_turn != pending.source.start_turn
            || pending.source.raw_hash != receipt.source.raw_hash
            || pending.summary_hash != source.summary_hash
            || content_hash(pending.summary.as_bytes()) != source.summary_hash
            || pending.embedding.input_hash != source.summary_hash
            || pending.embedding.model != self.resources.embedding_model
            || pending.embedding.revision != self.resources.embedding_revision
            || pending.embedding.dimension != i64::from(self.resources.embedding_dimension)
            || pending.model != "gpt-6-astra"
            || pending.effort != "medium"
            || pending.prompt_version != "strict10-v1"
            || match (&source.publication, &receipt.structured_publication_id) {
                (None, None) => false,
                (Some(p), Some(id)) => {
                    p.scope != scope
                        || p.raw_hash != receipt.source.raw_hash
                        || &p.publication_id != id
                }
                _ => true,
            }
        {
            return Err(StoreError::Corrupt("固定記憶の保存要約・出典不一致"));
        }
        Ok(source)
    }
    pub(super) fn summary_raw(&self, proof: &SummaryProof) -> StoreResult<Vec<RawEventV2>> {
        raw::turn(&self.files, &proof.source.source)
    }
}

/// All receipts, raw proofs and nested index queries share one SQLite snapshot.
/// No parent directory is opened until the corresponding tombstone check passes.
pub(super) fn with_receipts<'a, T>(
    writer: &'a Writer,
    target: &RunTarget,
    expected: &MemorySnapshot,
    db: &MemoryStore,
    read: impl FnOnce(&[FrozenMemory<'a>]) -> StoreResult<T>,
) -> StoreResult<T> {
    writer.check_memory_snapshot(target, expected)?;
    let result = with_published_receipts(writer, db, read)?;
    writer.check_memory_snapshot(target, expected)?;
    Ok(result)
}

fn grants(p: &Published) -> StoreResult<(Vec<fork::FrozenAncestor>, StructuredPublicationView)> {
    let saved = memory(p)?;
    let mut targets = vec![fork::FrozenAncestor {
        marker: p.marker.clone(),
        generation: saved.generation,
        summary_ids: saved.sources.iter().map(|s| s.summary_id.clone()).collect(),
        structured_ids: saved
            .sources
            .iter()
            .filter_map(|s| s.structured_publication_id.clone())
            .collect(),
    }];
    targets.extend(saved.ancestors.clone());
    let view = StructuredPublicationView {
        scope: PublicationScope {
            project_id: p.marker.project_id.as_str().into(),
            session_id: p.marker.session_id.as_str().into(),
            epoch: number(p.marker.epoch.get())?,
            generation: number(saved.generation.get())?,
        },
        visible_publication_ids: targets[0].structured_ids.clone(),
        ancestors: saved
            .ancestors
            .iter()
            .map(|a| {
                Ok(
                    polaris_memory::structured::StructuredAncestorPublicationRange {
                        session_id: a.marker.session_id.as_str().into(),
                        epoch: number(a.marker.epoch.get())?,
                        generation: number(a.generation.get())?,
                        visible_publication_ids: a.structured_ids.clone(),
                    },
                )
            })
            .collect::<StoreResult<_>>()?,
    };
    Ok((targets, view))
}

pub(super) fn with_published_receipts<'a, T>(
    writer: &'a Writer,
    db: &MemoryStore,
    read: impl FnOnce(&[FrozenMemory<'a>]) -> StoreResult<T>,
) -> StoreResult<T> {
    let p = writer.snapshot()?;
    let saved = memory(&p)?;
    let work = bounded::ReadWork::default();
    let (targets, view) = grants(&p)?;
    let result = db
        .with_verified_structured_deltas(&[], || {
            let run = || -> StoreResult<T> {
                let mut owners = Vec::new();
                let mut owner_error = None;
                let mut retained = 0usize;
                let mut retain = |owner: FrozenMemory<'a>| -> StoreResult<()> {
                    if owner.resources != saved.resources {
                        return Err(StoreError::Corrupt("固定祖先の記憶資源不一致"));
                    }
                    charge(
                        &mut retained,
                        &(&owner.scope, &owner.resources, &owner.summaries),
                    )?;
                    owners.push(owner);
                    Ok(())
                };
                let checked = db.with_verified_structured_receipt_stream(
                    &view,
                    &mut |scope, _, submit| {
                        let source = targets
                            .iter()
                            .find(|t| {
                                t.marker.session_id.as_str() == scope.session_id
                                    && number(t.marker.epoch.get()).ok() == Some(scope.epoch)
                            })
                            .ok_or(polaris_memory::Error::InvalidInput(
                                "frozen owner outside grants",
                            ))?;
                        match FrozenMemory::load(writer, work.clone(), source, submit)
                            .and_then(&mut retain)
                        {
                            Ok(()) => Ok(()),
                            Err(error) => {
                                owner_error = Some(error);
                                Err(polaris_memory::Error::InvalidInput(
                                    "frozen owner verification failed",
                                ))
                            }
                        }
                    },
                    || Ok(()),
                );
                if let Some(error) = owner_error {
                    return Err(error);
                }
                checked.map_err(std::io::Error::other)?;
                // Empty v3 grants can still carry strict10 summaries. Check their
                // tombstone with an empty local view inside the same outer snapshot.
                for source in targets.iter().filter(|t| t.structured_ids.is_empty()) {
                    let empty = StructuredPublicationView {
                        scope: PublicationScope {
                            project_id: source.marker.project_id.as_str().into(),
                            session_id: source.marker.session_id.as_str().into(),
                            epoch: number(source.marker.epoch.get())?,
                            generation: number(source.generation.get())?,
                        },
                        visible_publication_ids: vec![],
                        ancestors: vec![],
                    };
                    db.with_verified_structured_receipts(
                        &empty,
                        &mut |_, _| {
                            Err(polaris_memory::Error::InvalidInput(
                                "unexpected empty receipt",
                            ))
                        },
                        || Ok(()),
                    )
                    .map_err(std::io::Error::other)?;
                    retain(FrozenMemory::load(
                        writer,
                        work.clone(),
                        source,
                        &mut |_| {
                            Err(polaris_memory::Error::InvalidInput(
                                "unexpected empty receipt",
                            ))
                        },
                    )?)?;
                }
                read(&owners)
            };
            Ok(run())
        })
        .map_err(std::io::Error::other)??;
    Ok(result)
}

pub(super) fn resolve_source(
    owners: &[FrozenMemory<'_>],
    db: &MemoryStore,
    view: &StructuredPublicationView,
    publication_id: &str,
    source_id: &str,
) -> StoreResult<crate::conversation_memory::structured::sources::ResolvedStructuredSource> {
    db.with_structured_source(view, publication_id, source_id, |publication| {
        let run = || -> StoreResult<_> {
            let owner = owners.iter().find(|o| o.matches(&publication.scope)).ok_or(StoreError::NotFound)?;
            let proof = owner.summaries.values().find(|s| s.publication.as_ref() == Some(&publication)).ok_or(StoreError::NotFound)?;
            let event = raw::event(&owner.files, &proof.source.source, source_id)?;
            crate::conversation_memory::structured::sources::ResolvedStructuredSource::from_verified_event(publication, source_id, event).map_err(Into::into)
        };
        Ok(run())
    }).map_err(std::io::Error::other)?
}

fn checked_prefix(
    owner: &MemoryDirectory<'_>,
    work: &bounded::ReadWork,
    name: &str,
    offset: u64,
    expected_hash: &str,
) -> StoreResult<(File, u64, u64)> {
    work.charge(offset)?;
    owner.verify()?;
    let mut file = owner.directory.open(name, false, false)?;
    if file.metadata()?.len() < offset || !hash(expected_hash) {
        return Err(StoreError::Corrupt("固定記憶のprefix範囲"));
    }
    let mut digest = Sha256::new();
    let mut buffer = [0u8; bounded::BUFFER];
    let (mut read, mut start, mut last_start, mut lines, mut newline) =
        (0u64, 0u64, 0u64, 0u64, false);
    while read < offset {
        let limit = usize::try_from((offset - read).min(buffer.len() as u64))
            .map_err(|_| StoreError::Overflow)?;
        let count = file.read(&mut buffer[..limit])?;
        if count == 0 {
            return Err(StoreError::Corrupt("固定記憶の短いprefix"));
        }
        digest.update(&buffer[..count]);
        for (i, byte) in buffer[..count].iter().enumerate() {
            newline = *byte == b'\n';
            if newline {
                last_start = start;
                start = read + i as u64 + 1;
                lines = lines.checked_add(1).ok_or(StoreError::Overflow)?;
            }
        }
        read += count as u64;
    }
    if (offset > 0 && !newline) || format!("{:x}", digest.finalize()) != expected_hash {
        return Err(StoreError::Corrupt("固定記憶のprefix hash・改行"));
    }
    owner.verify()?;
    disk::same(&file, &owner.directory.open(name, false, false)?)?;
    Ok((file, last_start, lines))
}
