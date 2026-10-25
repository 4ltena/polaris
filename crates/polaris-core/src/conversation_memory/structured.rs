//! Host validation for the structured10 outer summary envelope.
//!
//! The provider returns names, spans and semantic classifications only.  This
//! module binds every accepted item to the immutable raw snapshot and builds
//! host-owned v3 IDs and publication metadata.

use crate::conversation_state::{
    ConversationSnapshot, RawEventV2, StructuredCoverage, content_hash,
};
use polaris_memory::structured::{
    EntityKind, FactRelation, FactStatus, OriginScope, PublicationScope, StructuredAlias,
    StructuredDelta, StructuredEntity, StructuredEntityRelation, StructuredEntityRevision,
    StructuredFactRevision, StructuredPublication, stable_entity_id, stable_fact_id,
    validate_delta,
};
use polaris_provider::Role;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
};

mod authority;
pub(crate) use authority::source_requirements;
#[cfg(test)]
mod authority_tests;
mod facts;
pub mod identity;
pub mod packing;
pub mod retrieval;
pub mod sources;
#[cfg(test)]
mod transitions_tests;
use facts::{FactCandidate, FactUpdates};

/// Host-only queries bound to an immutable publication view. Model output can
/// describe a fact but cannot select an arbitrary stored ID or permission.
pub trait StructuredResolver {
    fn entity(&self, kind: EntityKind, name: &str, project: &str) -> io::Result<Option<String>>;
    fn facts(
        &self,
        _entity: &str,
        _kind: &str,
        _condition: &str,
    ) -> io::Result<Vec<StructuredFactRevision>> {
        Ok(vec![])
    }
    fn fact(&self, _id: &str) -> io::Result<Option<StructuredFactRevision>> {
        Ok(None)
    }
    fn dependents(&self, _id: &str) -> io::Result<Vec<String>> {
        Ok(vec![])
    }
}

pub struct PublishedResolver<'a> {
    pub memory: &'a polaris_memory::MemoryStore,
    pub view: &'a polaris_memory::structured::StructuredPublicationView,
}
impl StructuredResolver for PublishedResolver<'_> {
    fn entity(&self, kind: EntityKind, name: &str, project: &str) -> io::Result<Option<String>> {
        self.memory
            .resolve_structured_entity(self.view, kind, name, project)
            .map_err(io::Error::other)
    }
    fn facts(
        &self,
        entity: &str,
        kind: &str,
        condition: &str,
    ) -> io::Result<Vec<StructuredFactRevision>> {
        self.memory
            .structured_fact_candidates(self.view, entity, kind, condition)
            .map_err(io::Error::other)
    }
    fn fact(&self, id: &str) -> io::Result<Option<StructuredFactRevision>> {
        self.memory
            .structured_fact_revision(self.view, id)
            .map_err(io::Error::other)
    }
    fn dependents(&self, id: &str) -> io::Result<Vec<String>> {
        self.memory
            .structured_dependents(self.view, id)
            .map_err(io::Error::other)
    }
}

pub const STRUCTURED_PROMPT_VERSION: &str = "structured10-v1";
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_METADATA_BYTES: usize = 1024;
const MAX_SOURCE_TOKENS: usize = 16_384;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredPromptSource {
    pub source_id: String,
    pub sequence: u64,
    pub role: Role,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StructuredIntent {
    pub sources: Vec<StructuredPromptSource>,
    pub coverage: StructuredCoverage,
}

/// One copy of every source, with explicit targets for the expired-turn summary.
#[derive(Debug, Clone, Serialize)]
pub struct StructuredSummaryInput {
    pub sources: Vec<StructuredPromptSource>,
    pub summary_target_source_ids: Vec<String>,
    pub extraction_source_ids: Vec<String>,
    pub related_entities: Vec<polaris_memory::structured::StructuredEntityHint>,
}

pub fn summary_input(
    snapshot: &ConversationSnapshot,
    turn: u64,
    intent: &StructuredIntent,
) -> io::Result<StructuredSummaryInput> {
    if structured_summary_intent(snapshot)?.as_ref() != Some(intent) {
        return Err(io::Error::other(
            "Structured extraction source map does not match the original text",
        ));
    }
    super::strict_history::turn_messages(snapshot, turn)?;
    let mut sources: BTreeMap<_, _> = intent
        .sources
        .iter()
        .cloned()
        .map(|s| (s.sequence, s))
        .collect();
    let mut summary_target_source_ids = Vec::new();
    for event in snapshot
        .events
        .iter()
        .filter(|e| e.epoch == snapshot.state.epoch && e.turn_id == turn)
    {
        let id = source_id(snapshot, event);
        summary_target_source_ids.push(id.clone());
        sources
            .entry(event.sequence)
            .or_insert_with(|| StructuredPromptSource {
                source_id: id,
                sequence: event.sequence,
                role: event.message.role,
                content: event.message.content.clone(),
            });
    }
    if summary_target_source_ids.is_empty() {
        return Err(io::Error::other(
            "No target source for structured summarization",
        ));
    }
    let input = StructuredSummaryInput {
        sources: sources.into_values().collect(),
        summary_target_source_ids,
        extraction_source_ids: intent.sources.iter().map(|s| s.source_id.clone()).collect(),
        related_entities: vec![],
    };
    source_budget(&input.sources)?;
    Ok(input)
}

pub fn source_budget(sources: &[StructuredPromptSource]) -> io::Result<()> {
    let text = serde_json::to_string(sources)?;
    if text.len() > MAX_RESPONSE_BYTES || crate::budget::count_tokens(&text) > MAX_SOURCE_TOKENS {
        return Err(io::Error::other(
            "Structured source exceeds 64 KiB or 16,384 reference tokens",
        ));
    }
    Ok(())
}

pub fn hint_budget(hints: &[polaris_memory::structured::StructuredEntityHint]) -> io::Result<()> {
    if hints.len() > 64 || crate::budget::count_tokens(&serde_json::to_string(hints)?) > 2_048 {
        return Err(io::Error::other(
            "Related entity candidates exceed 64 entries or 2,048 reference tokens; ambiguity cannot be retained",
        ));
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    summary: serde_json::Value,
    processed_source_ids: Vec<String>,
    entity_candidates: Vec<EntityCandidate>,
    fact_deltas: Vec<FactCandidate>,
    #[serde(default)]
    entity_relations: Vec<EntityRelationCandidate>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct EntityReference {
    kind: EntityKind,
    name: String,
    subject_project: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EntityRelationCandidate {
    source_id: String,
    evidence_span: Span,
    evidence_text: String,
    speech_act: String,
    from: EntityReference,
    to: EntityReference,
    relation: String,
}

impl EntityRelationCandidate {
    fn validate(
        &self,
        sources: &BTreeMap<&str, &StructuredPromptSource>,
        user_inputs: &BTreeSet<u64>,
        resolver: &dyn StructuredResolver,
        pending: &BTreeMap<String, PendingEntity>,
    ) -> io::Result<StructuredEntityRelation> {
        let source = checked_source(
            sources,
            &self.source_id,
            &self.evidence_span,
            &self.evidence_text,
        )?;
        validate_candidate_fields(
            &self.speech_act,
            &[
                &self.from.name,
                &self.from.subject_project,
                &self.to.name,
                &self.to.subject_project,
                &self.evidence_text,
            ],
        )?;
        if source.role != Role::User
            || !user_inputs.contains(&source.sequence)
            || !matches!(self.speech_act.as_str(), "assert" | "adopt" | "correct")
            || !matches!(
                self.relation.as_str(),
                "owns" | "belongs_to" | "distinct_from"
            )
            || !self.evidence_text.contains(&self.from.name)
            || !self.evidence_text.contains(&self.to.name)
        {
            return Err(io::Error::other(
                "Entity relation has no valid evidence in the original text",
            ));
        }
        let resolve = |reference: &EntityReference| {
            resolve_entity(
                resolver,
                pending,
                reference.kind,
                &reference.name,
                &reference.subject_project,
            )?
            .ok_or_else(|| io::Error::other("Entity relation reference is missing or ambiguous"))
        };
        let from = resolve(&self.from)?;
        let to = resolve(&self.to)?;
        if from == to {
            return Err(io::Error::other("Entity relation endpoints are identical"));
        }
        Ok(StructuredEntityRelation {
            source_entity_id: from,
            target_entity_id: to,
            relation: self.relation.clone(),
            source_id: self.source_id.clone(),
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Span {
    start: usize,
    length: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct EntityCandidate {
    source_id: String,
    evidence_span: Span,
    speech_act: String,
    ordinal: u32,
    kind: EntityKind,
    name: String,
    subject_project: String,
    evidence_text: String,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    same_as: Option<String>,
    #[serde(default)]
    renamed_from: Option<String>,
    #[serde(default)]
    distinct_from: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ValidatedStructuredResponse {
    pub summary: String,
    pub delta: StructuredDelta,
    pub coverage: StructuredCoverage,
}

pub fn validate_saved_response(
    snapshot: &ConversationSnapshot,
    turn: u64,
    intent: &StructuredIntent,
    summary: &str,
    delta: &StructuredDelta,
    coverage: &StructuredCoverage,
) -> io::Result<()> {
    validate_delta(delta).map_err(io::Error::other)?;
    super::summary_evidence(snapshot, turn, summary)?;
    let p = &delta.publication;
    if structured_summary_intent(snapshot)?.as_ref() != Some(intent)
        || coverage != &intent.coverage
        || p.raw_hash != snapshot.raw_hash
        || p.scope.project_id != snapshot.state.project_id
        || p.scope.session_id != snapshot.state.session_id
        || u64::try_from(p.scope.epoch).ok() != Some(snapshot.state.epoch)
        || u64::try_from(p.scope.generation).ok() != snapshot.state.generation.checked_add(1)
        || p.publication_id != format!("structured10-{}-{}", turn, &snapshot.raw_hash[..16])
        || p.prompt_version != STRUCTURED_PROMPT_VERSION
    {
        return Err(io::Error::other(
            "The saved structured response input, scope, or coverage does not match",
        ));
    }
    authority::validate_sources(snapshot, intent, delta)
}

pub fn source_id(snapshot: &ConversationSnapshot, event: &RawEventV2) -> String {
    format!(
        "source-v3:{}:{}:{}:{}",
        snapshot.state.project_id, snapshot.state.session_id, snapshot.state.epoch, event.sequence
    )
}

/// A summary may remain due after extraction has reached the durable end.
/// In that case keep the expired turn's summary but permit no new state rows.
/// Uncovered, uncommitted, oversized, or incomplete tool suffixes never qualify.
pub fn structured_summary_intent(
    snapshot: &ConversationSnapshot,
) -> io::Result<Option<StructuredIntent>> {
    if let Some(intent) = structured_intent(snapshot)? {
        return Ok(Some(intent));
    }
    let Some(coverage) = &snapshot.state.structured_coverage else {
        return Ok(None);
    };
    if snapshot.state.visible_structured_publication_ids.is_empty()
        || coverage.raw_offset != snapshot.raw_offset
        || snapshot.raw_offset != snapshot.state.raw_commit_offset
    {
        return Ok(None);
    }
    if snapshot.state.raw_commit_hash != snapshot.raw_hash {
        return Err(io::Error::other(
            "Structured summary finalized source hash mismatch",
        ));
    }
    let raw = snapshot_raw(snapshot)?;
    crate::conversation_state::validate_coverage_boundary(
        &raw,
        snapshot,
        snapshot.state.raw_commit_offset,
        coverage,
    )?;
    Ok(Some(StructuredIntent {
        sources: vec![],
        coverage: coverage.clone(),
    }))
}

/// Selects the oldest complete-message prefix from an uncovered epoch suffix.
/// A single oversized event is never split and therefore leaves coverage behind.
pub fn structured_intent(snapshot: &ConversationSnapshot) -> io::Result<Option<StructuredIntent>> {
    let start = snapshot
        .state
        .structured_coverage
        .as_ref()
        .map(|v| v.last_source_sequence)
        .unwrap_or(0);
    let mut sources = Vec::new();
    let events: Vec<_> = snapshot
        .events
        .iter()
        .filter(|event| event.epoch == snapshot.state.epoch && event.sequence > start)
        .collect();
    let mut index = 0;
    while index < events.len() {
        let mut end = index + 1;
        if !events[index].message.tool_calls.is_empty() {
            let mut pending: BTreeSet<_> = events[index]
                .message
                .tool_calls
                .iter()
                .map(|call| call.id.as_str())
                .collect();
            while end < events.len() && !pending.is_empty() {
                if events[end].message.role == Role::Tool
                    && let Some(id) = events[end].message.tool_call_id.as_deref()
                {
                    pending.remove(id);
                }
                end += 1;
            }
            if !pending.is_empty() {
                break;
            }
        }
        let group: Vec<_> = events[index..end]
            .iter()
            .map(|event| StructuredPromptSource {
                source_id: source_id(snapshot, event),
                sequence: event.sequence,
                role: event.message.role,
                content: event.message.content.clone(),
            })
            .collect();
        let mut next_sources = sources.clone();
        next_sources.extend(group);
        if source_budget(&next_sources).is_err() {
            break;
        }
        sources = next_sources;
        index = end;
    }
    let Some(last_sequence) = sources.last().map(|source| source.sequence) else {
        return Ok(None);
    };
    let raw = snapshot_raw(snapshot)?;
    if raw.len() as u64 != snapshot.raw_offset || content_hash(&raw) != snapshot.raw_hash {
        return Err(io::Error::other(
            "Cannot reconstruct structured extraction raw byte boundaries",
        ));
    }
    let offset = raw_offset_for_sequence(&raw, last_sequence)?;
    Ok(Some(StructuredIntent {
        sources,
        coverage: StructuredCoverage {
            epoch: snapshot.state.epoch,
            last_source_sequence: last_sequence,
            raw_offset: offset as u64,
            prefix_hash: content_hash(&raw[..offset]),
        },
    }))
}

pub fn validate_response(
    snapshot: &ConversationSnapshot,
    turn: u64,
    intent: &StructuredIntent,
    response: &str,
) -> io::Result<ValidatedStructuredResponse> {
    validate_response_with_entities(snapshot, turn, intent, response, &|_, _, _| Ok(None))
}
type EntityResolver<'a> = dyn Fn(EntityKind, &str, &str) -> io::Result<Option<String>> + 'a;

pub fn validate_response_with_entities(
    snapshot: &ConversationSnapshot,
    turn: u64,
    intent: &StructuredIntent,
    response: &str,
    resolve: &EntityResolver<'_>,
) -> io::Result<ValidatedStructuredResponse> {
    struct EntityOnly<'a>(&'a EntityResolver<'a>);
    impl StructuredResolver for EntityOnly<'_> {
        fn entity(
            &self,
            kind: EntityKind,
            name: &str,
            project: &str,
        ) -> io::Result<Option<String>> {
            (self.0)(kind, name, project)
        }
    }
    validate_response_with_resolver(snapshot, turn, intent, response, &EntityOnly(resolve))
}

pub fn validate_response_with_resolver(
    snapshot: &ConversationSnapshot,
    turn: u64,
    intent: &StructuredIntent,
    response: &str,
    resolver: &dyn StructuredResolver,
) -> io::Result<ValidatedStructuredResponse> {
    if structured_summary_intent(snapshot)?.as_ref() != Some(intent) {
        return Err(io::Error::other(
            "Structured extraction source map does not match the original text",
        ));
    }
    if response.len() > MAX_RESPONSE_BYTES {
        return Err(io::Error::other(
            "Structured extraction response exceeds 64 KiB",
        ));
    }
    let envelope: Envelope = serde_json::from_str(response)
        .map_err(|_| io::Error::other("Invalid structured extraction response format"))?;
    let summary = serde_json::to_string(&envelope.summary)?;
    super::validate_summary(&summary, turn)?;
    let expected: Vec<_> = intent
        .sources
        .iter()
        .map(|s| s.source_id.as_str())
        .collect();
    if envelope
        .processed_source_ids
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        != expected
        || envelope
            .processed_source_ids
            .iter()
            .collect::<BTreeSet<_>>()
            .len()
            != expected.len()
        || envelope.entity_candidates.len() > 32
        || envelope.fact_deltas.len() > 64
        || envelope.entity_relations.len() > 32
    {
        return Err(io::Error::other(
            "Invalid structured extraction source set or count",
        ));
    }
    let sources: BTreeMap<_, _> = intent
        .sources
        .iter()
        .map(|s| (s.source_id.as_str(), s))
        .collect();
    let user_inputs = authority::user_inputs(snapshot);
    let origin = OriginScope {
        project_id: snapshot.state.project_id.clone(),
        session_id: snapshot.state.session_id.clone(),
        epoch: i64::try_from(snapshot.state.epoch)
            .map_err(|_| io::Error::other("epoch overflow"))?,
    };
    let generation = snapshot
        .state
        .generation
        .checked_add(1)
        .ok_or_else(|| io::Error::other("generation overflow"))?;
    let scope = PublicationScope {
        project_id: origin.project_id.clone(),
        session_id: origin.session_id.clone(),
        epoch: origin.epoch,
        generation: i64::try_from(generation)
            .map_err(|_| io::Error::other("generation overflow"))?,
    };
    let mut entities = Vec::new();
    let mut revisions = Vec::new();
    let mut aliases = Vec::new();
    let mut relations = Vec::new();
    let mut entity_ordinals = BTreeSet::new();
    if envelope
        .entity_candidates
        .iter()
        .any(|c| !sources.contains_key(c.source_id.as_str()))
        || envelope
            .fact_deltas
            .iter()
            .any(|c| !sources.contains_key(c.source_id.as_str()))
        || envelope
            .entity_relations
            .iter()
            .any(|c| !sources.contains_key(c.source_id.as_str()))
    {
        return Err(io::Error::other(
            "The structured candidate source is outside the extraction range",
        ));
    }
    let mut pending_entities = BTreeMap::new();
    let mut fact_updates = FactUpdates::default();
    for current_source in &intent.sources {
        // Process sources in durable order, not the order supplied by the model.
        let mut entity_candidates: Vec<_> = envelope
            .entity_candidates
            .iter()
            .filter(|c| c.source_id == current_source.source_id)
            .cloned()
            .collect();
        entity_candidates.sort_by_key(|c| c.ordinal);
        for candidate in entity_candidates {
            let source = checked_source(
                &sources,
                &candidate.source_id,
                &candidate.evidence_span,
                &candidate.evidence_text,
            )?;
            validate_candidate_fields(
                &candidate.speech_act,
                &[
                    &candidate.name,
                    &candidate.subject_project,
                    &candidate.evidence_text,
                ],
            )?;
            if candidate.aliases.len() > 32 || candidate.aliases.iter().any(|a| invalid_metadata(a))
            {
                return Err(io::Error::other("Invalid structured entity alias"));
            }
            if !entity_ordinals.insert((candidate.source_id.clone(), candidate.ordinal))
                || !candidate.evidence_text.contains(&candidate.name)
            {
                return Err(io::Error::other(
                    "Invalid structured entity name or ordinal",
                ));
            }
            let identity_targets: Vec<_> = [
                &candidate.same_as,
                &candidate.renamed_from,
                &candidate.distinct_from,
            ]
            .into_iter()
            .flatten()
            .collect();
            if identity_targets.len() > 1
                || ((!identity_targets.is_empty() || !candidate.aliases.is_empty())
                    && (source.role != Role::User
                        || !user_inputs.contains(&source.sequence)
                        || !matches!(
                            candidate.speech_act.as_str(),
                            "assert" | "adopt" | "correct"
                        )
                        || identity_targets
                            .iter()
                            .copied()
                            .chain(candidate.aliases.iter())
                            .any(|name| !candidate.evidence_text.contains(name.as_str()))))
            {
                return Err(io::Error::other(
                    "Structured entity identity change has no valid source evidence",
                ));
            }
            for relation in [
                &candidate.same_as,
                &candidate.renamed_from,
                &candidate.distinct_from,
            ] {
                if relation
                    .as_ref()
                    .is_some_and(|value| invalid_metadata(value))
                {
                    return Err(io::Error::other("Invalid structured entity relation"));
                }
            }
            let mut id = stable_entity_id(
                &origin,
                &candidate.source_id,
                candidate.ordinal,
                candidate.kind,
            )
            .map_err(io::Error::other)?;
            let existing = if let Some(target) = candidate
                .same_as
                .as_ref()
                .or(candidate.renamed_from.as_ref())
            {
                id = resolve_entity(
                    resolver,
                    &pending_entities,
                    candidate.kind,
                    target,
                    &candidate.subject_project,
                )?
                .ok_or_else(|| {
                    io::Error::other("Structured entity relation is missing or ambiguous")
                })?;
                true
            } else {
                false
            };
            if let Some(target) = &candidate.distinct_from {
                let target_id = resolve_entity(
                    resolver,
                    &pending_entities,
                    candidate.kind,
                    target,
                    &candidate.subject_project,
                )?
                .ok_or_else(|| {
                    io::Error::other("Structured entity distinct_from is missing or ambiguous")
                })?;
                if target_id == id {
                    return Err(io::Error::other(
                        "Structured entity distinct_from references the same entity",
                    ));
                }
                relations.push(StructuredEntityRelation {
                    source_entity_id: id.clone(),
                    target_entity_id: target_id,
                    relation: "distinct_from".into(),
                    source_id: source.source_id.clone(),
                });
            }
            if !existing {
                entities.push(StructuredEntity {
                    entity_id: id.clone(),
                    origin: origin.clone(),
                    origin_source_id: source.source_id.clone(),
                    origin_ordinal: candidate.ordinal,
                    kind: candidate.kind,
                });
            }
            if revisions.iter().any(|r: &StructuredEntityRevision| {
                r.entity_id == id && r.source_id == source.source_id
            }) {
                return Err(io::Error::other(
                    "Entity updates within the same source are ambiguous",
                ));
            }
            revisions.retain(|r| r.entity_id != id);
            let state = pending_entities
                .entry(id.clone())
                .or_insert_with(|| PendingEntity {
                    kind: candidate.kind,
                    name: candidate.name.clone(),
                    project: candidate.subject_project.clone(),
                    aliases: BTreeSet::new(),
                });
            state.name = candidate.name.clone();
            state.project = candidate.subject_project.clone();
            revisions.push(StructuredEntityRevision {
                entity_id: id.clone(),
                source_id: source.source_id.clone(),
                canonical_name: candidate.name,
                description: candidate.evidence_text,
                subject_project: candidate.subject_project,
            });
            let explicit_old_name = candidate.renamed_from.or(candidate.same_as);
            let candidate_aliases: BTreeSet<_> = candidate
                .aliases
                .into_iter()
                .chain(explicit_old_name)
                .filter(|alias| alias != &state.name)
                .collect();
            state.aliases.extend(candidate_aliases.iter().cloned());
            aliases.extend(candidate_aliases.into_iter().map(|alias| StructuredAlias {
                entity_id: id.clone(),
                match_alias: match candidate.kind {
                    EntityKind::Person | EntityKind::Product | EntityKind::Other => {
                        alias.trim().to_lowercase()
                    }
                    _ => alias.clone(),
                },
                display_alias: alias,
                relation: "alias".into(),
                source_id: source.source_id.clone(),
            }));
        }
        for relation in envelope
            .entity_relations
            .iter()
            .filter(|r| r.source_id == current_source.source_id)
        {
            let row = relation.validate(&sources, &user_inputs, resolver, &pending_entities)?;
            if relations.contains(&row) {
                return Err(io::Error::other("Duplicate entity relation candidates"));
            }
            relations.push(row);
        }
        let mut fact_candidates: Vec<_> = envelope
            .fact_deltas
            .iter()
            .filter(|c| c.source_id == current_source.source_id)
            .cloned()
            .collect();
        fact_candidates.sort_by_key(|c| c.ordinal);
        for candidate in fact_candidates {
            fact_updates.apply(
                candidate,
                &sources,
                &user_inputs,
                resolver,
                &|kind, name, project| {
                    resolve_entity(resolver, &pending_entities, kind, name, project)
                },
                &snapshot.state.project_id,
            )?;
        }
    }
    let mut delta = StructuredDelta {
        publication: StructuredPublication {
            scope,
            publication_id: format!("structured10-{}-{}", turn, &snapshot.raw_hash[..16]),
            raw_hash: snapshot.raw_hash.clone(),
            delta_hash: String::new(),
            prompt_version: STRUCTURED_PROMPT_VERSION.into(),
        },
        entities,
        entity_revisions: revisions,
        aliases,
        facts: fact_updates.rows.into_values().collect(),
        dependencies: fact_updates.dependencies,
        relations,
    };
    delta.publication.delta_hash =
        polaris_memory::structured::delta_hash(&delta).map_err(io::Error::other)?;
    validate_delta(&delta).map_err(io::Error::other)?;
    authority::validate_sources(snapshot, intent, &delta)?;
    Ok(ValidatedStructuredResponse {
        summary,
        delta,
        coverage: intent.coverage.clone(),
    })
}

fn checked_source<'a>(
    sources: &BTreeMap<&str, &'a StructuredPromptSource>,
    id: &str,
    span: &Span,
    evidence: &str,
) -> io::Result<&'a StructuredPromptSource> {
    let source = sources
        .get(id)
        .copied()
        .ok_or_else(|| io::Error::other("Invalid structured candidate source ID"))?;
    let end = span
        .start
        .checked_add(span.length)
        .ok_or_else(|| io::Error::other("Invalid structured candidate span"))?;
    if end > source.content.len()
        || !source.content.is_char_boundary(span.start)
        || !source.content.is_char_boundary(end)
        || &source.content[span.start..end] != evidence
    {
        return Err(io::Error::other(
            "Structured candidate evidence span does not match the original text",
        ));
    }
    Ok(source)
}

struct PendingEntity {
    kind: EntityKind,
    name: String,
    project: String,
    aliases: BTreeSet<String>,
}

fn resolve_entity(
    resolver: &dyn StructuredResolver,
    pending: &BTreeMap<String, PendingEntity>,
    kind: EntityKind,
    name: &str,
    project: &str,
) -> io::Result<Option<String>> {
    let matches = |candidate: &str| match kind {
        EntityKind::Person | EntityKind::Product | EntityKind::Other => {
            candidate.trim().to_lowercase() == name.trim().to_lowercase()
        }
        _ => candidate == name,
    };
    let mut ids: BTreeSet<_> = pending
        .iter()
        .filter(|(_, e)| {
            e.kind == kind
                && e.project == project
                && (matches(&e.name) || e.aliases.iter().any(|a| matches(a)))
        })
        .map(|(id, _)| id.clone())
        .collect();
    if let Some(id) = resolver.entity(kind, name, project)?
        && (!pending.contains_key(&id) || ids.contains(&id))
    {
        ids.insert(id);
    }
    if ids.len() > 1 {
        return Err(io::Error::other("Ambiguous structured entity reference"));
    }
    Ok(ids.into_iter().next())
}
fn invalid_metadata(value: &str) -> bool {
    value.is_empty() || value.len() > MAX_METADATA_BYTES
}
fn validate_candidate_fields(speech_act: &str, fields: &[&str]) -> io::Result<()> {
    if !matches!(
        speech_act,
        "assert"
            | "adopt"
            | "correct"
            | "withdraw"
            | "propose"
            | "quote"
            | "hypothetical"
            | "reject"
            | "observation"
            | "uncertain"
    ) || fields.iter().any(|v| invalid_metadata(v))
    {
        return Err(io::Error::other(
            "Invalid structured candidate metadata or utterance classification",
        ));
    }
    Ok(())
}
fn snapshot_raw(snapshot: &ConversationSnapshot) -> io::Result<Vec<u8>> {
    let mut raw = Vec::new();
    for event in &snapshot.events {
        serde_json::to_writer(&mut raw, event)?;
        raw.push(b'\n');
    }
    Ok(raw)
}
fn raw_offset_for_sequence(raw: &[u8], sequence: u64) -> io::Result<usize> {
    let mut offset = 0;
    for line in raw.split_inclusive(|v| *v == b'\n') {
        offset += line.len();
        let event: RawEventV2 = serde_json::from_slice(line)?;
        if event.sequence == sequence {
            return Ok(offset);
        }
    }
    Err(io::Error::other("Missing structured coverage sequence"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation_state::{ConversationStateV2, content_hash};
    use polaris_desktop_protocol::snapshot::HistoryMode;
    use polaris_provider::Message;

    fn snapshot() -> ConversationSnapshot {
        let mut state = ConversationStateV2::new(
            "project".into(),
            "00000000-0000-4000-8000-000000000001".into(),
        )
        .unwrap();
        state.history_mode = HistoryMode::Structured10;
        let events = vec![RawEventV2 {
            schema_version: 2,
            sequence: 1,
            epoch: 0,
            turn_id: 1,
            starts_turn: true,
            message: Message::user("Aster is production"),
        }];
        let raw = snapshot_raw(&ConversationSnapshot {
            state: state.clone(),
            raw_hash: String::new(),
            raw_offset: 0,
            events: events.clone(),
        })
        .unwrap();
        state.raw_commit_offset = raw.len() as u64;
        state.raw_commit_hash = content_hash(&raw);
        ConversationSnapshot {
            state,
            raw_hash: content_hash(&raw),
            raw_offset: raw.len() as u64,
            events,
        }
    }

    #[test]
    fn envelope_requires_exact_source_and_evidence_before_constructing_host_ids() {
        let snapshot = snapshot();
        let intent = structured_intent(&snapshot).unwrap().unwrap();
        let source_id = &intent.sources[0].source_id;
        let response = serde_json::json!({
            "summary":{"facts":["Aster is production"],"decisions":[],"constraints":[],"corrections":[],"open_items":[],"source_turn_ids":[1]},
            "processed_source_ids":[source_id],
            "entity_candidates":[{"source_id":source_id,"evidence_span":{"start":0,"length":5},"speech_act":"assert","ordinal":0,"kind":"product","name":"Aster","subject_project":"project","evidence_text":"Aster"}],
            "fact_deltas":[{"source_id":source_id,"evidence_span":{"start":0,"length":5},"speech_act":"assert","ordinal":0,"entity_kind":"product","entity_name":"Aster","fact_kind":"environment","value":"production","unit":"none","condition":"","relation":"assert","evidence_text":"Aster"}]
        }).to_string();
        let valid = validate_response(&snapshot, 1, &intent, &response).unwrap();
        assert_eq!(valid.coverage.last_source_sequence, 1);
        assert_eq!(valid.delta.entities.len(), 1);
        assert_eq!(valid.delta.facts[0].status, FactStatus::Active);
        let forged = response.replace(source_id, "source-v3-forged");
        assert!(validate_response(&snapshot, 1, &intent, &forged).is_err());
    }

    #[test]
    fn assistant_or_quote_candidate_cannot_change_current_fact() {
        let mut snapshot = snapshot();
        snapshot.events[0].message.role = Role::Assistant;
        let raw = snapshot_raw(&snapshot).unwrap();
        snapshot.raw_hash = content_hash(&raw);
        snapshot.raw_offset = raw.len() as u64;
        let intent = structured_intent(&snapshot).unwrap().unwrap();
        let source_id = &intent.sources[0].source_id;
        let response = serde_json::json!({
            "summary":{"facts":["Aster is production"],"decisions":[],"constraints":[],"corrections":[],"open_items":[],"source_turn_ids":[1]},
            "processed_source_ids":[source_id],
            "entity_candidates":[{"source_id":source_id,"evidence_span":{"start":0,"length":5},"speech_act":"observation","ordinal":0,"kind":"product","name":"Aster","subject_project":"project","evidence_text":"Aster"}],
            "fact_deltas":[{"source_id":source_id,"evidence_span":{"start":0,"length":5},"speech_act":"observation","ordinal":0,"entity_kind":"product","entity_name":"Aster","fact_kind":"environment","value":"production","unit":"none","condition":"","relation":"assert","evidence_text":"Aster"}]
        }).to_string();
        let valid = validate_response(&snapshot, 1, &intent, &response).unwrap();
        assert_eq!(valid.delta.facts[0].status, FactStatus::Proposed);
    }

    #[test]
    fn summary_and_extraction_share_one_source_map_with_explicit_targets() {
        let snapshot = snapshot();
        let intent = structured_intent(&snapshot).unwrap().unwrap();
        let input = summary_input(&snapshot, 1, &intent).unwrap();
        assert_eq!(input.sources.len(), 1);
        assert_eq!(input.summary_target_source_ids, input.extraction_source_ids);
        let wire = serde_json::to_string(&input).unwrap();
        assert_eq!(wire.matches("Aster is production").count(), 1);
        let mut forged = intent.clone();
        forged.sources[0].role = Role::Assistant;
        assert!(summary_input(&snapshot, 1, &forged).is_err());
    }

    #[test]
    fn source_token_limit_applies_below_the_byte_limit_without_splitting() {
        let mut snapshot = snapshot();
        snapshot.events[0].message.content = "\u{10000}".repeat(6000);
        assert!(snapshot.events[0].message.content.len() < MAX_RESPONSE_BYTES);
        assert!(
            crate::budget::count_tokens(&snapshot.events[0].message.content) > MAX_SOURCE_TOKENS
        );
        let raw = snapshot_raw(&snapshot).unwrap();
        snapshot.raw_hash = content_hash(&raw);
        snapshot.raw_offset = raw.len() as u64;
        assert!(structured_intent(&snapshot).unwrap().is_none());
        snapshot.events[0].message.content = "bounded source".into();
        let raw = snapshot_raw(&snapshot).unwrap();
        snapshot.raw_hash = content_hash(&raw);
        snapshot.raw_offset = raw.len() as u64;
        assert_eq!(
            structured_intent(&snapshot).unwrap().unwrap().sources[0].content,
            "bounded source"
        );
    }

    #[test]
    fn renamed_entity_reuses_resolved_stable_id_without_new_entity_row() {
        let mut snapshot = snapshot();
        snapshot.events[0].message.content = "Rename Rigel to Aster".into();
        let raw = snapshot_raw(&snapshot).unwrap();
        snapshot.raw_hash = content_hash(&raw);
        snapshot.raw_offset = raw.len() as u64;
        let intent = structured_intent(&snapshot).unwrap().unwrap();
        let source = &intent.sources[0].source_id;
        let old = "a".repeat(64);
        let response = serde_json::json!({"summary":{"facts":["Aster"],"decisions":[],"constraints":[],"corrections":[],"open_items":[],"source_turn_ids":[1]},"processed_source_ids":[source],"entity_candidates":[{"source_id":source,"evidence_span":{"start":0,"length":21},"speech_act":"assert","ordinal":0,"kind":"product","name":"Aster","subject_project":"project","evidence_text":"Rename Rigel to Aster","renamed_from":"Rigel","aliases":["Rigel"]}],"fact_deltas":[]});
        let resolver =
            |_: EntityKind, name: &str, _: &str| Ok((name == "Rigel").then(|| old.clone()));
        let result = validate_response_with_entities(
            &snapshot,
            1,
            &intent,
            &response.to_string(),
            &resolver,
        )
        .unwrap();
        assert!(result.delta.entities.is_empty());
        assert_eq!(result.delta.entity_revisions[0].entity_id, old);
        assert_eq!(
            result.delta.aliases[0].entity_id,
            result.delta.entity_revisions[0].entity_id
        );
        let mut without_repeated_alias = response.clone();
        without_repeated_alias["entity_candidates"][0]["aliases"] = serde_json::json!([]);
        let renamed = validate_response_with_entities(
            &snapshot,
            1,
            &intent,
            &without_repeated_alias.to_string(),
            &resolver,
        )
        .unwrap();
        assert_eq!(renamed.delta.aliases[0].display_alias, "Rigel");
        for speech in [
            "quote",
            "hypothetical",
            "reject",
            "observation",
            "uncertain",
            "propose",
            "withdraw",
        ] {
            let mut bad = response.clone();
            bad["entity_candidates"][0]["speech_act"] = speech.into();
            assert!(
                validate_response_with_entities(&snapshot, 1, &intent, &bad.to_string(), &resolver)
                    .is_err(),
                "{speech}"
            );
        }
        for role in [Role::Assistant, Role::Tool] {
            let mut bad_intent = intent.clone();
            bad_intent.sources[0].role = role;
            assert!(
                validate_response_with_entities(
                    &snapshot,
                    1,
                    &bad_intent,
                    &response.to_string(),
                    &resolver
                )
                .is_err()
            );
        }
        for (field, value) in [
            ("aliases", serde_json::json!(["Invented"])),
            ("renamed_from", serde_json::json!("Invented")),
            ("same_as", serde_json::json!("Rigel")),
        ] {
            let mut bad = response.clone();
            bad["entity_candidates"][0][field] = value;
            assert!(
                validate_response_with_entities(&snapshot, 1, &intent, &bad.to_string(), &resolver)
                    .is_err(),
                "{field}"
            );
        }
        let mut incomplete = response.clone();
        incomplete["entity_candidates"][0]["evidence_span"] =
            serde_json::json!({"start":16,"length":5});
        incomplete["entity_candidates"][0]["evidence_text"] = "Aster".into();
        assert!(
            validate_response_with_entities(
                &snapshot,
                1,
                &intent,
                &incomplete.to_string(),
                &resolver
            )
            .is_err()
        );
    }
}
