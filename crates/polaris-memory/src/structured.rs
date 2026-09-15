//! Version 3 structured-memory schema and host-owned domain values.
//!
//! Opt-in schema, exact publication checks and frozen-view identity queries.
//! Semantic extraction and model context packing remain host responsibilities.

use crate::{Error, MemoryStore, Result};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[cfg(test)]
mod receipt_stream_tests;
pub mod retrieval;
mod sources;

pub const STRUCTURED_MEMORY_COMPONENT: &str = "structured_memory";
pub const STRUCTURED_MEMORY_SCHEMA_VERSION: i64 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OriginScope {
    pub project_id: String,
    pub session_id: String,
    pub epoch: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicationScope {
    pub project_id: String,
    pub session_id: String,
    pub epoch: i64,
    pub generation: i64,
}

/// Exact marker grants used only while validating a new publication.  This is
/// intentionally separate from M3-3's general retrieval view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredPublicationView {
    pub scope: PublicationScope,
    pub visible_publication_ids: Vec<String>,
    pub ancestors: Vec<StructuredAncestorPublicationRange>,
}

impl StructuredPublicationView {
    /// Bound all inherited grants before receipt reads; reserve a new publication
    /// before starting extraction so publication cannot exceed the readable set.
    pub fn check_receipt_capacity(&self, next_publication: Option<&str>) -> Result<()> {
        if self.ancestors.len() > 1_024 {
            return Err(Error::InvalidInput("structured receipt ancestor limit"));
        }
        let mut count = usize::from(next_publication.is_some());
        let mut bytes = next_publication.map_or(0, str::len);
        for ids in std::iter::once(&self.visible_publication_ids).chain(
            self.ancestors
                .iter()
                .map(|ancestor| &ancestor.visible_publication_ids),
        ) {
            count = count.saturating_add(ids.len());
            if count > 65_536 {
                return Err(Error::InvalidInput("structured receipt grant limit"));
            }
            for id in ids {
                bytes = bytes.saturating_add(id.len());
                if bytes > 4 * 1024 * 1024 {
                    return Err(Error::InvalidInput("structured receipt grant byte limit"));
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredAncestorPublicationRange {
    pub session_id: String,
    pub epoch: i64,
    pub generation: i64,
    pub visible_publication_ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    Person,
    Product,
    Path,
    Function,
    ConfigurationKey,
    ErrorCode,
    Version,
    Other,
}

impl EntityKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Person => "person",
            Self::Product => "product",
            Self::Path => "path",
            Self::Function => "function",
            Self::ConfigurationKey => "configuration_key",
            Self::ErrorCode => "error_code",
            Self::Version => "version",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactStatus {
    Active,
    Proposed,
    Superseded,
    NeedsReconfirmation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactRelation {
    Assert,
    Propose,
    Replace,
    Withdraw,
    Unresolved,
    DependsOn,
}

/// Host-generated publication metadata. Model output must not supply these IDs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredPublication {
    pub scope: PublicationScope,
    pub publication_id: String,
    pub raw_hash: String,
    pub delta_hash: String,
    pub prompt_version: String,
}

/// Fully host-resolved rows for one append-only publication.  Model output is
/// deliberately represented elsewhere: callers must validate source evidence,
/// scope and state transitions before constructing this value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredDelta {
    pub publication: StructuredPublication,
    pub entities: Vec<StructuredEntity>,
    pub entity_revisions: Vec<StructuredEntityRevision>,
    pub aliases: Vec<StructuredAlias>,
    pub facts: Vec<StructuredFactRevision>,
    #[serde(default)]
    pub dependencies: Vec<StructuredFactDependency>,
    #[serde(default)]
    pub relations: Vec<StructuredEntityRelation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredEntity {
    pub entity_id: String,
    pub origin: OriginScope,
    pub origin_source_id: String,
    pub origin_ordinal: u32,
    pub kind: EntityKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredEntityRevision {
    pub entity_id: String,
    pub source_id: String,
    pub canonical_name: String,
    pub description: String,
    pub subject_project: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredAlias {
    pub entity_id: String,
    pub display_alias: String,
    pub match_alias: String,
    pub relation: String,
    pub source_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredFactRevision {
    pub fact_id: String,
    pub entity_id: String,
    pub fact_kind: String,
    pub value_json: String,
    pub unit: String,
    pub condition: String,
    pub status: FactStatus,
    pub reason: String,
    pub successor_fact_id: Option<String>,
    pub source_id: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredFactDependency {
    pub dependent_fact_id: String,
    pub dependency_fact_id: String,
    pub source_id: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredEntityRelation {
    pub source_entity_id: String,
    pub target_entity_id: String,
    pub relation: String,
    pub source_id: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedStructuredFact {
    pub fact_id: String,
    pub status: FactStatus,
    pub scope: PublicationScope,
}

/// A current fact and its exact publishing snapshot. The raw hash is a
/// provenance binding; callers must still verify the original raw bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredFactEvidence {
    pub fact: StructuredFactRevision,
    pub publication: StructuredPublication,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredEntityHint {
    pub entity_id: String,
    pub kind: EntityKind,
    pub name: String,
    pub subject_project: String,
    pub matching_aliases: Vec<String>,
}

pub fn validate_publication(publication: &StructuredPublication) -> Result<()> {
    validate_publication_scope(&publication.scope)?;
    validate_id(&publication.publication_id)?;
    validate_hash(&publication.raw_hash)?;
    validate_hash(&publication.delta_hash)?;
    validate_metadata(&publication.prompt_version)?;
    Ok(())
}

pub fn validate_delta(delta: &StructuredDelta) -> Result<()> {
    validate_publication(&delta.publication)?;
    if delta_hash(delta)? != delta.publication.delta_hash {
        return Err(Error::InvalidInput("structured delta hash mismatch"));
    }
    if delta.entities.len() > 32
        || delta.entity_revisions.len() > 32
        || delta.aliases.len() > 1024
        || delta.facts.len() > 64
    {
        return Err(Error::InvalidInput(
            "structured delta exceeds candidate limit",
        ));
    }
    let mut entities = std::collections::BTreeSet::new();
    for entity in &delta.entities {
        validate_id(&entity.entity_id)?;
        validate_origin_scope(&entity.origin)?;
        validate_id(&entity.origin_source_id)?;
        if !entities.insert(&entity.entity_id)
            || stable_entity_id(
                &entity.origin,
                &entity.origin_source_id,
                entity.origin_ordinal,
                entity.kind,
            )? != entity.entity_id
        {
            return Err(Error::InvalidInput("invalid structured entity"));
        }
    }
    let mut revisions = std::collections::BTreeSet::new();
    for revision in &delta.entity_revisions {
        if !revisions.insert((&revision.entity_id, &revision.source_id)) {
            return Err(Error::InvalidInput("invalid structured entity revision"));
        }
        for value in [
            &revision.entity_id,
            &revision.source_id,
            &revision.canonical_name,
            &revision.description,
            &revision.subject_project,
        ] {
            validate_metadata(value)?;
        }
    }
    let mut aliases = std::collections::BTreeSet::new();
    for alias in &delta.aliases {
        if !aliases.insert((&alias.entity_id, &alias.display_alias, &alias.source_id)) {
            return Err(Error::InvalidInput("invalid structured alias"));
        }
        for value in [
            &alias.entity_id,
            &alias.display_alias,
            &alias.match_alias,
            &alias.relation,
            &alias.source_id,
        ] {
            validate_metadata(value)?;
        }
    }
    let mut facts = std::collections::BTreeSet::new();
    for fact in &delta.facts {
        if !facts.insert((&fact.fact_id, &fact.source_id)) {
            return Err(Error::InvalidInput("invalid structured fact"));
        }
        for value in [
            &fact.fact_id,
            &fact.entity_id,
            &fact.fact_kind,
            &fact.unit,
            &fact.reason,
            &fact.source_id,
        ] {
            validate_metadata(value)?;
        }
        validate_condition(&fact.condition)?;
        if fact.value_json.len() > 16 * 1024
            || serde_json::from_str::<serde_json::Value>(&fact.value_json).is_err()
        {
            return Err(Error::InvalidInput("invalid structured fact value"));
        }
        if fact
            .successor_fact_id
            .as_ref()
            .is_some_and(|id| validate_id(id).is_err())
        {
            return Err(Error::InvalidInput("invalid structured successor"));
        }
    }
    if delta.dependencies.len() > 32 || delta.relations.len() > 32 {
        return Err(Error::InvalidInput("structured relation limit"));
    }
    for dependency in &delta.dependencies {
        for value in [
            &dependency.dependent_fact_id,
            &dependency.dependency_fact_id,
            &dependency.source_id,
        ] {
            validate_id(value)?;
        }
        if dependency.dependent_fact_id == dependency.dependency_fact_id {
            return Err(Error::InvalidInput("self dependency"));
        }
    }
    for relation in &delta.relations {
        if !matches!(
            relation.relation.as_str(),
            "owns" | "belongs_to" | "distinct_from"
        ) {
            return Err(Error::InvalidInput("unsupported entity relation"));
        }
        for value in [
            &relation.source_entity_id,
            &relation.target_entity_id,
            &relation.relation,
            &relation.source_id,
        ] {
            validate_metadata(value)?;
        }
        if relation.source_entity_id == relation.target_entity_id {
            return Err(Error::InvalidInput("self entity relation"));
        }
    }
    Ok(())
}

/// One canonical integrity digest used by extraction, persistence and replay.
/// JSON object keys are ordered by serde_json's map; row order is preserved.
pub fn delta_hash(delta: &StructuredDelta) -> Result<String> {
    let p = &delta.publication;
    let body = serde_json::to_vec(&(
        &p.scope,
        &p.publication_id,
        &p.raw_hash,
        &p.prompt_version,
        &delta.entities,
        &delta.entity_revisions,
        &delta.aliases,
        &delta.facts,
        &delta.dependencies,
        &delta.relations,
    ))
    .map_err(|_| Error::InvalidInput("structured delta encoding"))?;
    Ok(format!("{:x}", Sha256::digest(body)))
}

/// Stable across rename, later generations, and child overlays.
pub fn stable_entity_id(
    origin: &OriginScope,
    source_id: &str,
    ordinal: u32,
    kind: EntityKind,
) -> Result<String> {
    validate_origin_scope(origin)?;
    validate_id(source_id)?;
    Ok(stable_id(&[
        "entity-v3",
        &origin.project_id,
        &origin.session_id,
        &origin.epoch.to_string(),
        source_id,
        &ordinal.to_string(),
        kind.as_str(),
    ]))
}

/// Fact identity deliberately includes its first source rather than a mutable value.
pub fn stable_fact_id(
    entity_id: &str,
    fact_kind: &str,
    condition: &str,
    source_id: &str,
) -> Result<String> {
    validate_id(entity_id)?;
    validate_metadata(fact_kind)?;
    validate_condition(condition)?;
    validate_id(source_id)?;
    Ok(stable_id(&[
        "fact-v3", entity_id, fact_kind, condition, source_id,
    ]))
}

/// Only forward transitions are accepted. A replacement or withdrawal must be
/// represented by a new revision, never by changing the old row in place.
pub fn validate_fact_transition(
    previous: Option<FactStatus>,
    next: FactStatus,
    relation: FactRelation,
) -> Result<()> {
    let valid = matches!(
        (previous, next, relation),
        (
            None,
            FactStatus::Active,
            FactRelation::Assert
                | FactRelation::Replace
                | FactRelation::Unresolved
                | FactRelation::DependsOn,
        ) | (None, FactStatus::Proposed, FactRelation::Propose)
            | (
                Some(FactStatus::Active),
                FactStatus::Superseded,
                FactRelation::Replace | FactRelation::Withdraw,
            )
            | (
                Some(FactStatus::Active),
                FactStatus::NeedsReconfirmation,
                FactRelation::DependsOn
            )
            | (
                Some(FactStatus::Proposed),
                FactStatus::Superseded,
                FactRelation::Withdraw | FactRelation::Replace,
            )
            | (
                Some(FactStatus::NeedsReconfirmation),
                FactStatus::Superseded,
                FactRelation::Replace | FactRelation::Withdraw,
            )
    );
    valid
        .then_some(())
        .ok_or(Error::InvalidInput("invalid structured fact transition"))
}

/// Explicit opt-in migration. Legacy and strict10 callers only use `open`, so
/// they never create v3 tables or schema metadata.
impl MemoryStore {
    /// Exact names and aliases only. Return all matching identities together;
    /// a cap must never silently disambiguate two same-name entities.
    pub fn structured_entity_hints(
        &self,
        view: &StructuredPublicationView,
        sources: &[&str],
    ) -> Result<Vec<StructuredEntityHint>> {
        validate_structured_view(view)?;
        if sources.len() > 4096
            || sources
                .iter()
                .try_fold(0usize, |sum, s| sum.checked_add(s.len()))
                .is_none_or(|n| n > 64 * 1024)
        {
            return Err(Error::InvalidInput("structured hint source limit"));
        }
        let mut budget = HintBudget {
            retained: 2,
            ..HintBudget::default()
        };
        let mut effective = std::collections::BTreeMap::new();
        for (scope, ids) in publication_scopes(view) {
            if structured_scope_forgotten(&self.connection, &scope)? {
                continue;
            }
            for (id, revisions) in scoped_entity_revisions_with_budget(
                &self.connection,
                &scope,
                ids,
                Some(&mut budget),
            )? {
                effective.entry(id).or_insert(revisions);
            }
        }
        let mut entities = std::collections::BTreeMap::new();
        let mut hints = std::collections::BTreeMap::new();
        for (id, revisions) in effective {
            let [(kind, name, project)] = revisions.as_slice() else {
                return Err(Error::InvalidInput(
                    "structured entity revision is ambiguous",
                ));
            };
            let entity = StructuredEntityHint {
                entity_id: id.clone(),
                kind: parse_entity_kind(kind)?,
                name: name.clone(),
                subject_project: project.clone(),
                matching_aliases: vec![],
            };
            if budget.matches(entity.kind, sources, name)? {
                budget.insert(&mut hints, &entity, None)?;
            }
            entities.insert(id, entity);
        }
        // Stream aliases under exact grants. Unrelated history is charged too;
        // exceeding work limits returns no partial hint or arbitrary winner.
        for (scope, ids) in publication_scopes(view) {
            if structured_scope_forgotten(&self.connection, &scope)? {
                continue;
            }
            for publication in ids {
                budget.scan(publication.len())?;
                let mut q = self.connection.prepare("SELECT entity_id,display_alias FROM conversation_aliases_v3 WHERE project_id=?1 AND session_id=?2 AND epoch=?3 AND generation<=?4 AND publication_id=?5")?;
                for row in q.query_map(
                    params![
                        scope.project_id,
                        scope.session_id,
                        scope.epoch,
                        scope.generation,
                        publication
                    ],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                )? {
                    let (id, alias) = row?;
                    budget.scan(id.len().saturating_add(alias.len()))?;
                    validate_metadata(&alias)?;
                    if let Some(entity) = entities.get(&id)
                        && budget.matches(entity.kind, sources, &alias)?
                    {
                        budget.insert(&mut hints, entity, Some(alias))?;
                    }
                }
            }
        }
        Ok(hints.into_values().collect())
    }
    pub fn enable_structured_memory(&mut self) -> Result<()> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        enable_structured_schema(&tx)?;
        tx.commit()?;
        Ok(())
    }

    pub fn structured_memory_schema_version(&self) -> Result<Option<i64>> {
        let meta_exists: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='memory_schema_meta')",
            [], |row| row.get(0),
        )?;
        if !meta_exists {
            return Ok(None);
        }
        let value = self
            .connection
            .query_row(
                "SELECT schema_version FROM memory_schema_meta WHERE component=?1",
                [STRUCTURED_MEMORY_COMPONENT],
                |row| row.get(0),
            )
            .optional()?;
        Ok(value)
    }

    /// Inserts a validated v3 publication without making it visible.  The core
    /// marker owner supplies visibility in the same transaction through the
    /// conversation publication primitive.
    pub fn insert_structured_delta(
        &mut self,
        delta: &StructuredDelta,
        view: &StructuredPublicationView,
    ) -> Result<()> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_structured_schema(&tx)?;
        insert_delta(&tx, delta, view)?;
        tx.commit()?;
        Ok(())
    }

    pub fn structured_publication(
        &self,
        scope: &PublicationScope,
        publication_id: &str,
    ) -> Result<Option<StructuredPublication>> {
        validate_publication_scope(scope)?;
        validate_id(publication_id)?;
        self.connection.query_row(
            "SELECT raw_hash,delta_hash,prompt_version FROM conversation_publications_v3 WHERE project_id=?1 AND session_id=?2 AND epoch=?3 AND generation=?4 AND publication_id=?5",
            params![scope.project_id,scope.session_id,scope.epoch,scope.generation,publication_id],
            |row| Ok(StructuredPublication { scope: scope.clone(), publication_id: publication_id.into(), raw_hash: row.get(0)?, delta_hash: row.get(1)?, prompt_version: row.get(2)? }),
        ).optional().map_err(Into::into)
    }

    pub fn verify_structured_delta(&self, delta: &StructuredDelta) -> Result<()> {
        verify_stored_delta(&self.connection, delta)
    }

    /// Bind a complete owner-selected set of saved deltas and all nested
    /// structured reads to one SQLite snapshot. This grants no extra scope.
    pub fn with_verified_structured_deltas<T>(
        &self,
        deltas: &[&StructuredDelta],
        read: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        if deltas.len() > 16_384 {
            return Err(Error::InvalidInput("structured saved delta limit"));
        }
        let owned = self.structured_read_transaction()?;
        for delta in deltas {
            verify_stored_delta(&self.connection, delta)?;
        }
        let result = read()?;
        if let Some(tx) = owned {
            tx.commit()?;
        }
        Ok(result)
    }

    /// Verify owner receipts incrementally under one read snapshot. The reader
    /// is invoked only for exact grants whose source session is not forgotten.
    pub fn with_verified_structured_receipts<T, R>(
        &self,
        view: &StructuredPublicationView,
        reader: &mut R,
        read: impl FnOnce() -> Result<T>,
    ) -> Result<T>
    where
        R: FnMut(&PublicationScope, &str) -> Result<StructuredDelta>,
    {
        let owned = self.structured_read_transaction()?;
        crate::conversation::verify_prior_structured(&self.connection, view, reader)?;
        let result = read()?;
        if let Some(tx) = owned {
            tx.commit()?;
        }
        Ok(result)
    }

    /// Scan each granted scope once, submitting saved receipts in any order.
    /// Every grant must be submitted exactly once and verified against the same
    /// SQLite snapshot before `read`. A failed submission remains fatal even if
    /// the scanner ignores its error. Receipt contents are not retained here.
    pub fn with_verified_structured_receipt_stream<T, S>(
        &self,
        view: &StructuredPublicationView,
        scanner: &mut S,
        read: impl FnOnce() -> Result<T>,
    ) -> Result<T>
    where
        S: FnMut(
            &PublicationScope,
            &[String],
            &mut dyn FnMut(&StructuredDelta) -> Result<()>,
        ) -> Result<()>,
    {
        let owned = self.structured_read_transaction()?;
        crate::conversation::verify_prior_structured_stream(&self.connection, view, scanner)?;
        let result = read()?;
        if let Some(tx) = owned {
            tx.commit()?;
        }
        Ok(result)
    }

    pub(crate) fn structured_read_transaction(&self) -> Result<Option<rusqlite::Transaction<'_>>> {
        if self.connection.is_autocommit() {
            Ok(Some(self.connection.unchecked_transaction()?))
        } else {
            // An owner already holds the snapshot across validation and reads.
            Ok(None)
        }
    }

    pub fn structured_publication_visible(
        &self,
        scope: &PublicationScope,
        publication_id: &str,
    ) -> Result<bool> {
        validate_publication_scope(scope)?;
        validate_id(publication_id)?;
        if structured_scope_forgotten(&self.connection, scope)? {
            return Ok(false);
        }
        self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM conversation_publications_v3 WHERE project_id=?1 AND session_id=?2 AND epoch=?3 AND generation<=?4 AND publication_id=?5)",
            params![scope.project_id,scope.session_id,scope.epoch,scope.generation,publication_id],
            |row| row.get(0),
        ).map_err(Into::into)
    }

    /// Resolves one exact entity name under marker grants.  Current-session
    /// grants are searched before frozen ancestors; no unlisted later parent
    /// publication can enter a child validation decision.
    pub fn resolve_structured_entity(
        &self,
        view: &StructuredPublicationView,
        kind: EntityKind,
        canonical_name: &str,
        subject_project: &str,
    ) -> Result<Option<String>> {
        validate_structured_view(view)?;
        validate_metadata(canonical_name)?;
        validate_metadata(subject_project)?;
        let mut scopes = vec![(view.scope.clone(), &view.visible_publication_ids)];
        for ancestor in &view.ancestors {
            scopes.push((
                PublicationScope {
                    project_id: view.scope.project_id.clone(),
                    session_id: ancestor.session_id.clone(),
                    epoch: ancestor.epoch,
                    generation: ancestor.generation,
                },
                &ancestor.visible_publication_ids,
            ));
        }
        let mut effective = std::collections::BTreeMap::new();
        let mut aliases = std::collections::BTreeMap::<String, Vec<String>>::new();
        for (scope, ids) in scopes {
            if structured_scope_forgotten(&self.connection, &scope)? {
                continue;
            }
            for (entity_id, rows) in scoped_entity_revisions(&self.connection, &scope, ids)? {
                effective.entry(entity_id).or_insert(rows);
            }
            for (entity_id, names) in scoped_entity_aliases(&self.connection, &scope, ids)? {
                aliases.entry(entity_id).or_default().extend(names);
            }
        }
        let mut found = std::collections::BTreeSet::new();
        for (entity_id, rows) in effective {
            let [(row_kind, name, project)] = rows.as_slice() else {
                return Err(Error::InvalidInput(
                    "structured entity revision is ambiguous",
                ));
            };
            if row_kind == kind.as_str()
                && project == subject_project
                && (entity_name_matches(kind, name, canonical_name)
                    || aliases.get(&entity_id).is_some_and(|names| {
                        names
                            .iter()
                            .any(|alias| entity_name_matches(kind, alias, canonical_name))
                    }))
            {
                found.insert(entity_id);
            }
        }
        if found.len() > 1 {
            return Err(Error::InvalidInput("structured entity name is ambiguous"));
        }
        Ok(found.into_iter().next())
    }
    pub fn resolve_structured_fact(
        &self,
        view: &StructuredPublicationView,
        entity_id: &str,
        fact_kind: &str,
        condition: &str,
    ) -> Result<Option<ResolvedStructuredFact>> {
        validate_structured_view(view)?;
        validate_id(entity_id)?;
        validate_metadata(fact_kind)?;
        validate_condition(condition)?;
        let mut scopes = vec![(view.scope.clone(), &view.visible_publication_ids)];
        for a in &view.ancestors {
            scopes.push((
                PublicationScope {
                    project_id: view.scope.project_id.clone(),
                    session_id: a.session_id.clone(),
                    epoch: a.epoch,
                    generation: a.generation,
                },
                &a.visible_publication_ids,
            ));
        }
        let mut effective = std::collections::BTreeMap::new();
        for (scope, ids) in scopes {
            if structured_scope_forgotten(&self.connection, &scope)? {
                continue;
            }
            let found = scoped_fact_matches(
                &self.connection,
                &scope,
                ids,
                entity_id,
                fact_kind,
                condition,
            )?;
            for (fact_id, (status, generation)) in found {
                effective
                    .entry(fact_id)
                    .or_insert((status, generation, scope.clone()));
            }
        }
        if effective.len() > 1 {
            return Err(Error::InvalidInput("structured fact is ambiguous"));
        }
        Ok(effective
            .into_iter()
            .next()
            .map(
                |(fact_id, (status, generation, scope))| ResolvedStructuredFact {
                    fact_id,
                    status,
                    scope: PublicationScope {
                        generation,
                        ..scope
                    },
                },
            ))
    }
    pub fn structured_dependents(
        &self,
        view: &StructuredPublicationView,
        fact_id: &str,
    ) -> Result<Vec<String>> {
        validate_structured_view(view)?;
        validate_id(fact_id)?;
        let mut out = std::collections::BTreeSet::new();
        let scopes = std::iter::once((view.scope.clone(), &view.visible_publication_ids)).chain(
            view.ancestors.iter().map(|a| {
                (
                    PublicationScope {
                        project_id: view.scope.project_id.clone(),
                        session_id: a.session_id.clone(),
                        epoch: a.epoch,
                        generation: a.generation,
                    },
                    &a.visible_publication_ids,
                )
            }),
        );
        for (scope, ids) in scopes {
            if structured_scope_forgotten(&self.connection, &scope)? {
                continue;
            }
            for id in ids {
                let mut q=self.connection.prepare("SELECT dependent_fact_id FROM conversation_fact_dependencies_v3 WHERE project_id=?1 AND session_id=?2 AND epoch=?3 AND generation<=?4 AND publication_id=?5 AND dependency_fact_id=?6")?;
                for row in q.query_map(
                    params![
                        scope.project_id,
                        scope.session_id,
                        scope.epoch,
                        scope.generation,
                        id,
                        fact_id
                    ],
                    |r| r.get(0),
                )? {
                    out.insert(row?);
                }
            }
        }
        Ok(out.into_iter().collect())
    }

    /// Full, current rows under the same frozen grants used by publication.
    /// Historical, proposed and unresolved rows remain distinct for the host
    /// transition resolver; none is silently promoted to a current decision.
    pub fn structured_fact_candidates(
        &self,
        view: &StructuredPublicationView,
        entity_id: &str,
        fact_kind: &str,
        condition: &str,
    ) -> Result<Vec<StructuredFactRevision>> {
        validate_structured_view(view)?;
        validate_id(entity_id)?;
        validate_metadata(fact_kind)?;
        validate_condition(condition)?;
        let mut ids = std::collections::BTreeSet::new();
        for (scope, publications) in publication_scopes(view) {
            if structured_scope_forgotten(&self.connection, &scope)? {
                continue;
            }
            ids.extend(
                scoped_fact_matches(
                    &self.connection,
                    &scope,
                    publications,
                    entity_id,
                    fact_kind,
                    condition,
                )?
                .into_keys(),
            );
        }
        ids.into_iter()
            .map(|id| {
                self.structured_fact_revision(view, &id)?
                    .ok_or(Error::InvalidInput("structured fact is absent"))
            })
            .collect()
    }

    pub fn structured_fact_revision(
        &self,
        view: &StructuredPublicationView,
        fact_id: &str,
    ) -> Result<Option<StructuredFactRevision>> {
        Ok(self
            .structured_fact_evidence(view, fact_id)?
            .map(|row| row.fact))
    }

    /// Resolve row and provenance in one SQLite read snapshot, retaining the
    /// exact local-or-ancestor grant that supplied the effective revision.
    pub fn structured_fact_evidence(
        &self,
        view: &StructuredPublicationView,
        fact_id: &str,
    ) -> Result<Option<StructuredFactEvidence>> {
        validate_structured_view(view)?;
        validate_id(fact_id)?;
        let tx = self.structured_read_transaction()?;
        let result = fact_evidence_in(&self.connection, view, fact_id)?;
        if let Some(tx) = tx {
            tx.commit()?;
        }
        Ok(result)
    }
}

fn fact_evidence_in(
    connection: &rusqlite::Connection,
    view: &StructuredPublicationView,
    fact_id: &str,
) -> Result<Option<StructuredFactEvidence>> {
    for (scope, publications) in publication_scopes(view) {
        if structured_scope_forgotten(connection, &scope)? {
            continue;
        }
        let mut latest: Option<(FactRevisionOrder, StructuredFactEvidence)> = None;
        for publication in publications {
            let mut q = connection.prepare(
                    "SELECT f.generation,f.entity_id,f.fact_kind,f.value_json,f.unit,f.condition_text,f.status,f.reason,f.successor_fact_id,f.source_id,p.raw_hash,p.delta_hash,p.prompt_version FROM conversation_fact_revisions_v3 f LEFT JOIN conversation_publications_v3 p ON p.project_id=f.project_id AND p.session_id=f.session_id AND p.epoch=f.epoch AND p.generation=f.generation AND p.publication_id=f.publication_id WHERE f.project_id=?1 AND f.session_id=?2 AND f.epoch=?3 AND f.generation<=?4 AND f.publication_id=?5 AND f.fact_id=?6"
                )?;
            for row in q.query_map(
                params![
                    scope.project_id,
                    scope.session_id,
                    scope.epoch,
                    scope.generation,
                    publication,
                    fact_id
                ],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(6)?,
                        StructuredFactRevision {
                            fact_id: fact_id.into(),
                            entity_id: r.get(1)?,
                            fact_kind: r.get(2)?,
                            value_json: r.get(3)?,
                            unit: r.get(4)?,
                            condition: r.get(5)?,
                            status: FactStatus::Proposed,
                            reason: r.get(7)?,
                            successor_fact_id: r.get(8)?,
                            source_id: r.get(9)?,
                        },
                        r.get::<_, Option<String>>(10)?,
                        r.get::<_, Option<String>>(11)?,
                        r.get::<_, Option<String>>(12)?,
                    ))
                },
            )? {
                let (generation, status, mut row, raw_hash, delta_hash, prompt_version) = row?;
                row.status = parse_status(&status)?;
                let order = (generation, source_sequence(&row.source_id));
                let row = StructuredFactEvidence {
                    fact: row,
                    publication: StructuredPublication {
                        scope: PublicationScope {
                            generation,
                            ..scope.clone()
                        },
                        publication_id: publication.clone(),
                        raw_hash: raw_hash
                            .ok_or(Error::InvalidInput("structured fact publication is absent"))?,
                        delta_hash: delta_hash
                            .ok_or(Error::InvalidInput("structured fact publication is absent"))?,
                        prompt_version: prompt_version
                            .ok_or(Error::InvalidInput("structured fact publication is absent"))?,
                    },
                };
                validate_publication(&row.publication)?;
                match &latest {
                    Some((previous, _)) if *previous > order => {}
                    Some((previous, previous_row)) if *previous == order => {
                        if previous_row != &row {
                            return Err(Error::InvalidInput(
                                "structured fact revision is ambiguous",
                            ));
                        }
                    }
                    _ => latest = Some((order, row)),
                }
            }
        }
        if let Some((_, row)) = latest {
            return Ok(Some(row));
        }
    }
    Ok(None)
}

pub(crate) fn publication_scopes(
    view: &StructuredPublicationView,
) -> Vec<(PublicationScope, &[String])> {
    std::iter::once((view.scope.clone(), view.visible_publication_ids.as_slice()))
        .chain(view.ancestors.iter().map(|a| {
            (
                PublicationScope {
                    project_id: view.scope.project_id.clone(),
                    session_id: a.session_id.clone(),
                    epoch: a.epoch,
                    generation: a.generation,
                },
                a.visible_publication_ids.as_slice(),
            )
        }))
        .collect()
}

type EntityRevisionNames = std::collections::BTreeMap<String, Vec<(String, String, String)>>;
type RankedEntityNames = std::collections::BTreeMap<String, (i64, Vec<(String, String, String)>)>;
type FactRevisionOrder = (i64, u64);
type RankedFactStatuses = std::collections::BTreeMap<String, (FactRevisionOrder, Vec<String>)>;

fn scoped_entity_revisions(
    connection: &rusqlite::Connection,
    scope: &PublicationScope,
    publication_ids: &[String],
) -> Result<EntityRevisionNames> {
    scoped_entity_revisions_with_budget(connection, scope, publication_ids, None)
}
fn scoped_entity_revisions_with_budget(
    connection: &rusqlite::Connection,
    scope: &PublicationScope,
    publication_ids: &[String],
    mut budget: Option<&mut HintBudget>,
) -> Result<EntityRevisionNames> {
    let mut revisions = RankedEntityNames::new();
    for publication_id in publication_ids {
        if let Some(budget) = budget.as_deref_mut() {
            budget.scan(publication_id.len())?;
        }
        let mut query = connection.prepare(
            "SELECT r.entity_id,r.generation,e.kind,r.canonical_name,r.subject_project \
             FROM conversation_entity_revisions_v3 r \
             JOIN conversation_entities_v3 e USING(entity_id) \
             WHERE r.project_id=?1 AND r.session_id=?2 AND r.epoch=?3 \
             AND r.generation<=?4 AND r.publication_id=?5",
        )?;
        for row in query.query_map(
            params![
                scope.project_id,
                scope.session_id,
                scope.epoch,
                scope.generation,
                publication_id
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )? {
            let (entity_id, generation, row_kind, name, project) = row?;
            if let Some(budget) = budget.as_deref_mut() {
                budget.scan(
                    entity_id
                        .len()
                        .saturating_add(row_kind.len())
                        .saturating_add(name.len())
                        .saturating_add(project.len()),
                )?;
            }
            match revisions.get_mut(&entity_id) {
                Some((current_generation, _)) if generation < *current_generation => {}
                Some((current_generation, rows)) if generation == *current_generation => {
                    rows.push((row_kind, name, project));
                }
                Some((current_generation, rows)) => {
                    *current_generation = generation;
                    rows.clear();
                    rows.push((row_kind, name, project));
                }
                None => {
                    revisions.insert(entity_id, (generation, vec![(row_kind, name, project)]));
                }
            }
        }
    }
    Ok(revisions
        .into_iter()
        .map(|(entity_id, (_, rows))| (entity_id, rows))
        .collect())
}

fn scoped_entity_aliases(
    connection: &rusqlite::Connection,
    scope: &PublicationScope,
    publication_ids: &[String],
) -> Result<std::collections::BTreeMap<String, Vec<String>>> {
    let mut aliases = std::collections::BTreeMap::<String, Vec<String>>::new();
    for publication_id in publication_ids {
        let mut query = connection.prepare(
            "SELECT entity_id,display_alias,match_alias FROM conversation_aliases_v3 \
             WHERE project_id=?1 AND session_id=?2 AND epoch=?3 AND generation<=?4 \
             AND publication_id=?5",
        )?;
        for row in query.query_map(
            params![
                scope.project_id,
                scope.session_id,
                scope.epoch,
                scope.generation,
                publication_id
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )? {
            let (entity_id, display, matched) = row?;
            aliases
                .entry(entity_id)
                .or_default()
                .extend([display, matched]);
        }
    }
    Ok(aliases)
}

fn entity_name_matches(kind: EntityKind, candidate: &str, query: &str) -> bool {
    match kind {
        EntityKind::Path
        | EntityKind::Function
        | EntityKind::ConfigurationKey
        | EntityKind::ErrorCode
        | EntityKind::Version => candidate == query,
        EntityKind::Person | EntityKind::Product | EntityKind::Other => {
            candidate.trim().to_lowercase() == query.trim().to_lowercase()
        }
    }
}

fn parse_entity_kind(kind: &str) -> Result<EntityKind> {
    match kind {
        "person" => Ok(EntityKind::Person),
        "product" => Ok(EntityKind::Product),
        "path" => Ok(EntityKind::Path),
        "function" => Ok(EntityKind::Function),
        "configuration_key" => Ok(EntityKind::ConfigurationKey),
        "error_code" => Ok(EntityKind::ErrorCode),
        "version" => Ok(EntityKind::Version),
        "other" => Ok(EntityKind::Other),
        _ => Err(Error::InvalidInput("structured entity kind")),
    }
}

/// Match a name using the same case and identifier boundaries as dictionary hints.
pub fn source_mentions(kind: EntityKind, source: &str, name: &str) -> bool {
    let general = matches!(
        kind,
        EntityKind::Person | EntityKind::Product | EntityKind::Other
    );
    let (source, name) = if general {
        (source.to_lowercase(), name.trim().to_lowercase())
    } else {
        (source.to_owned(), name.to_owned())
    };
    if name.is_empty() {
        return false;
    }
    source.match_indices(&name).any(|(start, _)| {
        // Japanese prose has no word-space separator. ASCII names and code
        // identifiers still require whole boundaries, never prefix matches.
        let adjacent = |c: char| {
            c.is_ascii_alphanumeric()
                || c == '_'
                || (!general && matches!(c, '.' | '/' | ':' | '-'))
        };
        source[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !adjacent(c))
            && source[start + name.len()..]
                .chars()
                .next()
                .is_none_or(|c| !adjacent(c))
    })
}

#[derive(Default)]
struct HintBudget {
    scanned: usize,
    compared: usize,
    retained: usize,
}
impl HintBudget {
    fn scan(&mut self, bytes: usize) -> Result<()> {
        self.scanned = self.scanned.saturating_add(bytes.saturating_add(1));
        if self.scanned > 4 * 1024 * 1024 {
            return Err(Error::InvalidInput("structured hint scan limit"));
        }
        Ok(())
    }
    fn matches(&mut self, kind: EntityKind, sources: &[&str], name: &str) -> Result<bool> {
        for source in sources {
            self.compared = self
                .compared
                .saturating_add(source.len().saturating_add(name.len()).saturating_add(1));
            if self.compared > 64 * 1024 * 1024 {
                return Err(Error::InvalidInput("structured hint comparison limit"));
            }
            if source_mentions(kind, source, name) {
                return Ok(true);
            }
        }
        Ok(false)
    }
    fn insert(
        &mut self,
        hints: &mut std::collections::BTreeMap<String, StructuredEntityHint>,
        entity: &StructuredEntityHint,
        alias: Option<String>,
    ) -> Result<()> {
        if !hints.contains_key(&entity.entity_id) {
            if hints.len() == 64 {
                return Err(Error::InvalidInput("structured hint identity limit"));
            }
            self.retained = self.retained.saturating_add(
                serde_json::to_vec(entity)
                    .map_err(std::io::Error::other)?
                    .len()
                    .saturating_add(1),
            );
            hints.insert(entity.entity_id.clone(), entity.clone());
        }
        if let Some(alias) = alias {
            let hint = hints.get_mut(&entity.entity_id).expect("inserted hint");
            if !hint.matching_aliases.contains(&alias) {
                if hint.matching_aliases.len() == 32 {
                    return Err(Error::InvalidInput("structured hint alias limit"));
                }
                self.retained = self.retained.saturating_add(
                    serde_json::to_vec(&alias)
                        .map_err(std::io::Error::other)?
                        .len()
                        .saturating_add(1),
                );
                hint.matching_aliases.push(alias);
                hint.matching_aliases.sort();
            }
        }
        if self.retained > 64 * 1024 {
            return Err(Error::InvalidInput("structured hint retained byte limit"));
        }
        Ok(())
    }
}

fn scoped_fact_matches(
    connection: &rusqlite::Connection,
    scope: &PublicationScope,
    publication_ids: &[String],
    entity_id: &str,
    fact_kind: &str,
    condition: &str,
) -> Result<std::collections::BTreeMap<String, (FactStatus, i64)>> {
    let mut facts = RankedFactStatuses::new();
    for publication_id in publication_ids {
        let mut query = connection.prepare(
            "SELECT fact_id,status,generation,source_id FROM conversation_fact_revisions_v3 \
             WHERE project_id=?1 AND session_id=?2 AND epoch=?3 AND generation<=?4 \
             AND publication_id=?5 AND entity_id=?6 AND fact_kind=?7 AND condition_text=?8",
        )?;
        for row in query.query_map(
            params![
                scope.project_id,
                scope.session_id,
                scope.epoch,
                scope.generation,
                publication_id,
                entity_id,
                fact_kind,
                condition
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )? {
            let (fact_id, status, generation, source) = row?;
            let generation = (generation, source_sequence(&source));
            match facts.get_mut(&fact_id) {
                Some((current_generation, _)) if generation < *current_generation => {}
                Some((current_generation, statuses)) if generation == *current_generation => {
                    statuses.push(status);
                }
                Some((current_generation, statuses)) => {
                    *current_generation = generation;
                    statuses.clear();
                    statuses.push(status);
                }
                None => {
                    facts.insert(fact_id, (generation, vec![status]));
                }
            }
        }
    }
    let mut current = std::collections::BTreeMap::new();
    for (fact_id, (generation, statuses)) in facts {
        let mut statuses = statuses.into_iter();
        let status = statuses
            .next()
            .ok_or(Error::InvalidInput("structured fact revision is absent"))?;
        if statuses.any(|other| other != status) {
            return Err(Error::InvalidInput("structured fact revision is ambiguous"));
        }
        current.insert(fact_id, (parse_status(&status)?, generation.0));
    }
    Ok(current)
}

fn structured_scope_forgotten(
    connection: &rusqlite::Connection,
    scope: &PublicationScope,
) -> Result<bool> {
    Ok(connection.query_row("SELECT EXISTS(SELECT 1 FROM memory_forgotten_sessions WHERE project_id=?1 AND session_id=?2)",params![scope.project_id,scope.session_id],|r|r.get(0))?)
}
fn source_sequence(source: &str) -> u64 {
    source
        .strip_prefix("source-v3:")
        .and_then(|s| s.rsplit(':').next())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

fn parse_status(value: &str) -> Result<FactStatus> {
    match value {
        "active" => Ok(FactStatus::Active),
        "proposed" => Ok(FactStatus::Proposed),
        "superseded" => Ok(FactStatus::Superseded),
        "needs_reconfirmation" => Ok(FactStatus::NeedsReconfirmation),
        _ => Err(Error::InvalidInput("invalid stored fact status")),
    }
}

pub(crate) fn validate_structured_view(view: &StructuredPublicationView) -> Result<()> {
    view.check_receipt_capacity(None)?;
    validate_publication_scope(&view.scope)?;
    let mut sessions = std::collections::BTreeSet::new();
    for ids in std::iter::once(&view.visible_publication_ids).chain(
        view.ancestors
            .iter()
            .map(|item| &item.visible_publication_ids),
    ) {
        if ids.iter().any(|id| validate_id(id).is_err())
            || ids.iter().collect::<std::collections::BTreeSet<_>>().len() != ids.len()
        {
            return Err(Error::InvalidInput("invalid structured publication view"));
        }
    }
    for ancestor in &view.ancestors {
        validate_metadata(&ancestor.session_id)?;
        if ancestor.epoch < 0
            || ancestor.generation < 0
            || ancestor.session_id == view.scope.session_id
            || !sessions.insert(&ancestor.session_id)
        {
            return Err(Error::InvalidInput("invalid structured ancestor view"));
        }
    }
    Ok(())
}

pub(crate) fn enable_structured_schema(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    let meta_exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='memory_schema_meta')",
        [], |row| row.get(0),
    )?;
    let v3_exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name LIKE 'conversation_%_v3')",
        [], |row| row.get(0),
    )?;
    if v3_exists && !meta_exists {
        return Err(Error::InvalidInput("structured memory schema conflict"));
    }
    if !meta_exists {
        tx.execute_batch("CREATE TABLE memory_schema_meta (component TEXT PRIMARY KEY NOT NULL, schema_version INTEGER NOT NULL);")?;
    }
    let version: Option<i64> = tx
        .query_row(
            "SELECT schema_version FROM memory_schema_meta WHERE component=?1",
            [STRUCTURED_MEMORY_COMPONENT],
            |row| row.get(0),
        )
        .optional()?;
    if v3_exists && version.is_none() {
        return Err(Error::InvalidInput(
            "structured memory schema component missing",
        ));
    }
    if let Some(version) = version {
        if version != STRUCTURED_MEMORY_SCHEMA_VERSION {
            return Err(Error::InvalidInput(
                "structured memory schema version mismatch",
            ));
        }
        verify_schema(tx)?;
        return Ok(());
    }
    tx.execute_batch(SCHEMA)?;
    tx.execute(
        "INSERT INTO memory_schema_meta(component, schema_version) VALUES (?1, ?2)",
        params![
            STRUCTURED_MEMORY_COMPONENT,
            STRUCTURED_MEMORY_SCHEMA_VERSION
        ],
    )?;
    verify_schema(tx)?;
    Ok(())
}

pub(crate) fn ensure_structured_schema(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    let version: Option<i64> = tx
        .query_row(
            "SELECT schema_version FROM memory_schema_meta WHERE component=?1",
            [STRUCTURED_MEMORY_COMPONENT],
            |row| row.get(0),
        )
        .optional()?;
    if version != Some(STRUCTURED_MEMORY_SCHEMA_VERSION) {
        return Err(Error::InvalidInput("structured memory is not enabled"));
    }
    verify_schema(tx)
}

pub(crate) fn insert_delta(
    tx: &rusqlite::Transaction<'_>,
    delta: &StructuredDelta,
    view: &StructuredPublicationView,
) -> Result<()> {
    validate_delta(delta)?;
    ensure_structured_schema(tx)?;
    validate_structured_view(view)?;
    validate_delta_authority(tx, delta, view)?;
    let p = &delta.publication;
    let inserted = tx.execute(
        "INSERT INTO conversation_publications_v3
         (project_id,session_id,epoch,generation,publication_id,raw_hash,delta_hash,prompt_version)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
         ON CONFLICT(project_id,session_id,epoch,generation,publication_id) DO NOTHING",
        params![
            p.scope.project_id,
            p.scope.session_id,
            p.scope.epoch,
            p.scope.generation,
            p.publication_id,
            p.raw_hash,
            p.delta_hash,
            p.prompt_version
        ],
    )?;
    if inserted == 0 {
        let equal: bool = tx.query_row(
            "SELECT raw_hash=?6 AND delta_hash=?7 AND prompt_version=?8
             FROM conversation_publications_v3 WHERE project_id=?1 AND session_id=?2 AND epoch=?3 AND generation=?4 AND publication_id=?5",
            params![p.scope.project_id, p.scope.session_id, p.scope.epoch, p.scope.generation,
                p.publication_id, p.raw_hash, p.delta_hash, p.prompt_version], |row| row.get(0),
        )?;
        if !equal {
            return Err(Error::InvalidInput("structured publication collision"));
        }
        // An existing publication is a replay, not permission to repair or
        // silently replace corrupted/missing child rows.
        return exact_delta_rows(tx, delta);
    }
    for entity in &delta.entities {
        tx.execute(
            "INSERT INTO conversation_entities_v3(entity_id,origin_project_id,origin_session_id,origin_epoch,origin_source_id,origin_ordinal,kind)
             VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(entity_id) DO NOTHING",
            params![entity.entity_id, entity.origin.project_id, entity.origin.session_id,
                entity.origin.epoch, entity.origin_source_id, entity.origin_ordinal, entity.kind.as_str()],
        )?;
    }
    for revision in &delta.entity_revisions {
        tx.execute(
            "INSERT INTO conversation_entity_revisions_v3(project_id,session_id,epoch,generation,publication_id,entity_id,source_id,canonical_name,description,subject_project)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10) ON CONFLICT DO NOTHING",
            params![p.scope.project_id,p.scope.session_id,p.scope.epoch,p.scope.generation,p.publication_id,
                revision.entity_id,revision.source_id,revision.canonical_name,revision.description,revision.subject_project],
        )?;
    }
    for alias in &delta.aliases {
        tx.execute(
            "INSERT INTO conversation_aliases_v3(project_id,session_id,epoch,generation,publication_id,entity_id,display_alias,match_alias,relation,source_id)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10) ON CONFLICT DO NOTHING",
            params![p.scope.project_id,p.scope.session_id,p.scope.epoch,p.scope.generation,p.publication_id,
                alias.entity_id,alias.display_alias,alias.match_alias,alias.relation,alias.source_id],
        )?;
    }
    for fact in &delta.facts {
        tx.execute(
            "INSERT INTO conversation_fact_revisions_v3(project_id,session_id,epoch,generation,publication_id,fact_id,entity_id,fact_kind,value_json,unit,condition_text,status,reason,successor_fact_id,source_id)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15) ON CONFLICT DO NOTHING",
            params![p.scope.project_id,p.scope.session_id,p.scope.epoch,p.scope.generation,p.publication_id,
                fact.fact_id,fact.entity_id,fact.fact_kind,fact.value_json,fact.unit,fact.condition,
                fact_status(fact.status),fact.reason,fact.successor_fact_id,fact.source_id],
        )?;
    }
    for dependency in &delta.dependencies {
        tx.execute("INSERT INTO conversation_fact_dependencies_v3(project_id,session_id,epoch,generation,publication_id,dependent_fact_id,dependency_fact_id,source_id) VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT DO NOTHING", params![p.scope.project_id,p.scope.session_id,p.scope.epoch,p.scope.generation,p.publication_id,dependency.dependent_fact_id,dependency.dependency_fact_id,dependency.source_id])?;
    }
    for relation in &delta.relations {
        tx.execute("INSERT INTO conversation_entity_relations_v3(project_id,session_id,epoch,generation,publication_id,source_entity_id,target_entity_id,relation,source_id) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9) ON CONFLICT DO NOTHING", params![p.scope.project_id,p.scope.session_id,p.scope.epoch,p.scope.generation,p.publication_id,relation.source_entity_id,relation.target_entity_id,relation.relation,relation.source_id])?;
    }
    exact_delta_rows(tx, delta)
}

pub(crate) fn verify_stored_delta(
    conn: &rusqlite::Connection,
    delta: &StructuredDelta,
) -> Result<()> {
    validate_delta(delta)?;
    let p = &delta.publication;
    let valid: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM conversation_publications_v3 WHERE project_id=?1 AND session_id=?2 AND epoch=?3 AND generation=?4 AND publication_id=?5 AND raw_hash=?6 AND delta_hash=?7 AND prompt_version=?8)",
        params![p.scope.project_id,p.scope.session_id,p.scope.epoch,p.scope.generation,p.publication_id,p.raw_hash,p.delta_hash,p.prompt_version], |r| r.get(0))?;
    if !valid {
        return Err(Error::InvalidInput(
            "structured publication readback mismatch",
        ));
    }
    exact_delta_rows(conn, delta)
}

pub(crate) fn exact_delta_rows(conn: &rusqlite::Connection, delta: &StructuredDelta) -> Result<()> {
    use serde_json::json;
    let p = &delta.publication;
    for e in &delta.entities {
        exact_rows(
            conn,
            "SELECT entity_id,origin_project_id,origin_session_id,origin_epoch,origin_source_id,origin_ordinal,kind FROM conversation_entities_v3 WHERE entity_id=?1",
            &[&e.entity_id],
            vec![json!([
                e.entity_id,
                e.origin.project_id,
                e.origin.session_id,
                e.origin.epoch,
                e.origin_source_id,
                e.origin_ordinal,
                e.kind
            ])],
        )?;
    }
    let scope: [&dyn rusqlite::ToSql; 5] = [
        &p.scope.project_id,
        &p.scope.session_id,
        &p.scope.epoch,
        &p.scope.generation,
        &p.publication_id,
    ];
    let table = |name: &str, columns: &str, expected: Vec<serde_json::Value>| {
        let sql = format!(
            "SELECT {columns} FROM {name} WHERE project_id=?1 AND session_id=?2 AND epoch=?3 AND generation=?4 AND publication_id=?5"
        );
        exact_rows(conn, &sql, &scope, expected)
    };
    table(
        "conversation_entity_revisions_v3",
        "entity_id,source_id,canonical_name,description,subject_project",
        delta
            .entity_revisions
            .iter()
            .map(|r| {
                json!([
                    r.entity_id,
                    r.source_id,
                    r.canonical_name,
                    r.description,
                    r.subject_project
                ])
            })
            .collect(),
    )?;
    table(
        "conversation_aliases_v3",
        "entity_id,display_alias,match_alias,relation,source_id",
        delta
            .aliases
            .iter()
            .map(|a| {
                json!([
                    a.entity_id,
                    a.display_alias,
                    a.match_alias,
                    a.relation,
                    a.source_id
                ])
            })
            .collect(),
    )?;
    table(
        "conversation_fact_revisions_v3",
        "fact_id,entity_id,fact_kind,value_json,unit,condition_text,status,reason,successor_fact_id,source_id",
        delta
            .facts
            .iter()
            .map(|f| {
                json!([
                    f.fact_id,
                    f.entity_id,
                    f.fact_kind,
                    f.value_json,
                    f.unit,
                    f.condition,
                    fact_status(f.status),
                    f.reason,
                    f.successor_fact_id,
                    f.source_id
                ])
            })
            .collect(),
    )?;
    table(
        "conversation_fact_dependencies_v3",
        "dependent_fact_id,dependency_fact_id,source_id",
        delta
            .dependencies
            .iter()
            .map(|d| json!([d.dependent_fact_id, d.dependency_fact_id, d.source_id]))
            .collect(),
    )?;
    table(
        "conversation_entity_relations_v3",
        "source_entity_id,target_entity_id,relation,source_id",
        delta
            .relations
            .iter()
            .map(|r| {
                json!([
                    r.source_entity_id,
                    r.target_entity_id,
                    r.relation,
                    r.source_id
                ])
            })
            .collect(),
    )?;
    Ok(())
}

fn exact_rows(
    conn: &rusqlite::Connection,
    sql: &str,
    params: &[&dyn rusqlite::ToSql],
    expected: Vec<serde_json::Value>,
) -> Result<()> {
    use rusqlite::types::ValueRef;
    let mut stmt = conn.prepare(sql)?;
    let count = stmt.column_count();
    let mut actual = Vec::new();
    let mut rows = stmt.query(params)?;
    while let Some(row) = rows.next()? {
        let mut cells = Vec::new();
        for i in 0..count {
            cells.push(match row.get_ref(i)? {
                ValueRef::Null => serde_json::Value::Null,
                ValueRef::Integer(n) => n.into(),
                ValueRef::Text(text) => std::str::from_utf8(text)
                    .map_err(|_| Error::InvalidInput("structured stored UTF-8"))?
                    .into(),
                _ => return Err(Error::InvalidInput("structured stored type")),
            });
        }
        actual.push(serde_json::Value::Array(cells).to_string());
    }
    let mut expected: Vec<_> = expected.iter().map(ToString::to_string).collect();
    actual.sort_unstable();
    expected.sort_unstable();
    if actual != expected {
        return Err(Error::InvalidInput("structured row readback mismatch"));
    }
    Ok(())
}

/// Existing identities are authorized by exact marker grants, never by the
/// global identity table or a model-provided identifier alone.
pub(crate) fn validate_delta_authority(
    conn: &rusqlite::Connection,
    delta: &StructuredDelta,
    view: &StructuredPublicationView,
) -> Result<()> {
    let scope = &delta.publication.scope;
    if scope.project_id != view.scope.project_id
        || scope.session_id != view.scope.session_id
        || scope.epoch != view.scope.epoch
        || scope.generation < view.scope.generation
    {
        return Err(Error::InvalidInput(
            "structured publication authority scope",
        ));
    }
    if structured_scope_forgotten(conn, scope)? {
        return Err(Error::SessionForgotten);
    }
    // Count the whole session, including older epochs and unpublished rows:
    // marker changes must not reset or bypass the storage ceiling.
    for (table, column, ids) in [
        (
            "conversation_entity_revisions_v3",
            "entity_id",
            delta
                .entity_revisions
                .iter()
                .map(|r| r.entity_id.as_str())
                .collect::<std::collections::BTreeSet<_>>(),
        ),
        (
            "conversation_fact_revisions_v3",
            "fact_id",
            delta.facts.iter().map(|r| r.fact_id.as_str()).collect(),
        ),
    ] {
        let count: i64 = conn.query_row(
            &format!(
                "SELECT COUNT(DISTINCT {column}) FROM {table} WHERE project_id=?1 AND session_id=?2"
            ),
            params![scope.project_id, scope.session_id],
            |r| r.get(0),
        )?;
        let mut added = 0;
        for id in ids {
            let exists: bool = conn.query_row(&format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE project_id=?1 AND session_id=?2 AND {column}=?3)"), params![scope.project_id, scope.session_id, id], |r| r.get(0))?;
            added += i64::from(!exists);
        }
        if count.saturating_add(added) > 65_536 {
            return Err(Error::InvalidInput("structured session identity limit"));
        }
    }
    let new_entities: std::collections::BTreeSet<_> = delta
        .entities
        .iter()
        .map(|e| e.entity_id.as_str())
        .collect();
    for e in &delta.entities {
        if e.origin.project_id != scope.project_id
            || e.origin.session_id != scope.session_id
            || e.origin.epoch != scope.epoch
            || !delta
                .entity_revisions
                .iter()
                .any(|r| r.entity_id == e.entity_id)
        {
            return Err(Error::InvalidInput("structured entity origin scope"));
        }
    }
    let entity_allowed = |id: &str| -> Result<()> {
        if new_entities.contains(id) || identity_visible(conn, view, id, false)? {
            Ok(())
        } else {
            Err(Error::InvalidInput(
                "structured entity outside exact grants",
            ))
        }
    };
    for id in delta
        .entity_revisions
        .iter()
        .map(|r| r.entity_id.as_str())
        .chain(delta.aliases.iter().map(|a| a.entity_id.as_str()))
        .chain(delta.facts.iter().map(|f| f.entity_id.as_str()))
        .chain(
            delta
                .relations
                .iter()
                .flat_map(|r| [r.source_entity_id.as_str(), r.target_entity_id.as_str()]),
        )
    {
        entity_allowed(id)?;
    }
    let new_facts: std::collections::BTreeSet<_> =
        delta.facts.iter().map(|f| f.fact_id.as_str()).collect();
    for fact in &delta.facts {
        let local_origin = delta.facts.iter().any(|origin| {
            origin.fact_id == fact.fact_id
                && origin.entity_id == fact.entity_id
                && origin.fact_kind == fact.fact_kind
                && origin.condition == fact.condition
                && stable_fact_id(
                    &origin.entity_id,
                    &origin.fact_kind,
                    &origin.condition,
                    &origin.source_id,
                )
                .is_ok_and(|id| id == origin.fact_id)
        });
        if delta.facts.iter().any(|other| {
            other.fact_id == fact.fact_id
                && (other.entity_id != fact.entity_id
                    || other.fact_kind != fact.fact_kind
                    || other.condition != fact.condition)
        }) {
            return Err(Error::InvalidInput(
                "structured fact identity changes within delta",
            ));
        }
        if fact.fact_id
            != stable_fact_id(
                &fact.entity_id,
                &fact.fact_kind,
                &fact.condition,
                &fact.source_id,
            )?
            && !local_origin
            && !identity_visible(conn, view, &fact.fact_id, true)?
        {
            return Err(Error::InvalidInput("structured fact outside exact grants"));
        }
        if identity_visible(conn, view, &fact.fact_id, true)? {
            validate_fact_identity(conn, view, fact)?;
        }
    }
    for id in delta
        .facts
        .iter()
        .filter_map(|f| f.successor_fact_id.as_deref())
        .chain(
            delta
                .dependencies
                .iter()
                .flat_map(|d| [d.dependent_fact_id.as_str(), d.dependency_fact_id.as_str()]),
        )
    {
        if !new_facts.contains(id) && !identity_visible(conn, view, id, true)? {
            return Err(Error::InvalidInput(
                "structured fact dependency outside exact grants",
            ));
        }
    }
    Ok(())
}

fn validate_fact_identity(
    conn: &rusqlite::Connection,
    view: &StructuredPublicationView,
    fact: &StructuredFactRevision,
) -> Result<()> {
    let scopes = std::iter::once((view.scope.clone(), &view.visible_publication_ids)).chain(
        view.ancestors.iter().map(|a| {
            (
                PublicationScope {
                    project_id: view.scope.project_id.clone(),
                    session_id: a.session_id.clone(),
                    epoch: a.epoch,
                    generation: a.generation,
                },
                &a.visible_publication_ids,
            )
        }),
    );
    for (scope, ids) in scopes {
        if structured_scope_forgotten(conn, &scope)? {
            continue;
        }
        for id in ids {
            let changed: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM conversation_fact_revisions_v3 WHERE project_id=?1 AND session_id=?2 AND epoch=?3 AND generation<=?4 AND publication_id=?5 AND fact_id=?6 AND (entity_id<>?7 OR fact_kind<>?8 OR condition_text<>?9))",
                params![scope.project_id,scope.session_id,scope.epoch,scope.generation,id,fact.fact_id,fact.entity_id,fact.fact_kind,fact.condition], |r|r.get(0))?;
            if changed {
                return Err(Error::InvalidInput(
                    "structured fact identity tuple changed",
                ));
            }
        }
    }
    Ok(())
}

fn identity_visible(
    conn: &rusqlite::Connection,
    view: &StructuredPublicationView,
    identity: &str,
    fact: bool,
) -> Result<bool> {
    let mut scopes = vec![(view.scope.clone(), &view.visible_publication_ids)];
    scopes.extend(view.ancestors.iter().map(|a| {
        (
            PublicationScope {
                project_id: view.scope.project_id.clone(),
                session_id: a.session_id.clone(),
                epoch: a.epoch,
                generation: a.generation,
            },
            &a.visible_publication_ids,
        )
    }));
    let sql = if fact {
        "SELECT EXISTS(SELECT 1 FROM conversation_fact_revisions_v3 WHERE project_id=?1 AND session_id=?2 AND epoch=?3 AND generation<=?4 AND publication_id=?5 AND fact_id=?6)"
    } else {
        "SELECT EXISTS(SELECT 1 FROM conversation_entity_revisions_v3 WHERE project_id=?1 AND session_id=?2 AND epoch=?3 AND generation<=?4 AND publication_id=?5 AND entity_id=?6)"
    };
    for (scope, ids) in scopes {
        if structured_scope_forgotten(conn, &scope)? {
            continue;
        }
        for id in ids {
            if conn.query_row(
                sql,
                params![
                    scope.project_id,
                    scope.session_id,
                    scope.epoch,
                    scope.generation,
                    id,
                    identity
                ],
                |r| r.get::<_, bool>(0),
            )? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn fact_status(value: FactStatus) -> &'static str {
    match value {
        FactStatus::Active => "active",
        FactStatus::Proposed => "proposed",
        FactStatus::Superseded => "superseded",
        FactStatus::NeedsReconfirmation => "needs_reconfirmation",
    }
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS memory_schema_meta (
  component TEXT PRIMARY KEY NOT NULL, schema_version INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS conversation_publications_v3 (
  project_id TEXT NOT NULL, session_id TEXT NOT NULL, epoch INTEGER NOT NULL,
  generation INTEGER NOT NULL, publication_id TEXT NOT NULL, raw_hash TEXT NOT NULL,
  delta_hash TEXT NOT NULL, prompt_version TEXT NOT NULL,
  PRIMARY KEY(project_id, session_id, epoch, generation, publication_id)
);
CREATE TABLE IF NOT EXISTS conversation_entities_v3 (
  entity_id TEXT PRIMARY KEY NOT NULL, origin_project_id TEXT NOT NULL,
  origin_session_id TEXT NOT NULL, origin_epoch INTEGER NOT NULL,
  origin_source_id TEXT NOT NULL, origin_ordinal INTEGER NOT NULL, kind TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS conversation_entity_revisions_v3 (
  project_id TEXT NOT NULL, session_id TEXT NOT NULL, epoch INTEGER NOT NULL,
  generation INTEGER NOT NULL, publication_id TEXT NOT NULL, entity_id TEXT NOT NULL,
  source_id TEXT NOT NULL, canonical_name TEXT NOT NULL, description TEXT NOT NULL,
  subject_project TEXT NOT NULL,
  PRIMARY KEY(project_id, session_id, epoch, generation, publication_id, entity_id, source_id),
  FOREIGN KEY(project_id, session_id, epoch, generation, publication_id)
    REFERENCES conversation_publications_v3(project_id, session_id, epoch, generation, publication_id),
  FOREIGN KEY(entity_id) REFERENCES conversation_entities_v3(entity_id)
);
CREATE TABLE IF NOT EXISTS conversation_aliases_v3 (
  project_id TEXT NOT NULL, session_id TEXT NOT NULL, epoch INTEGER NOT NULL,
  generation INTEGER NOT NULL, publication_id TEXT NOT NULL, entity_id TEXT NOT NULL,
  display_alias TEXT NOT NULL, match_alias TEXT NOT NULL, relation TEXT NOT NULL, source_id TEXT NOT NULL,
  PRIMARY KEY(project_id, session_id, epoch, generation, publication_id, entity_id, display_alias, source_id),
  FOREIGN KEY(project_id, session_id, epoch, generation, publication_id)
    REFERENCES conversation_publications_v3(project_id, session_id, epoch, generation, publication_id),
  FOREIGN KEY(entity_id) REFERENCES conversation_entities_v3(entity_id)
);
CREATE TABLE IF NOT EXISTS conversation_entity_relations_v3 (
  project_id TEXT NOT NULL, session_id TEXT NOT NULL, epoch INTEGER NOT NULL,
  generation INTEGER NOT NULL, publication_id TEXT NOT NULL, source_entity_id TEXT NOT NULL,
  target_entity_id TEXT NOT NULL, relation TEXT NOT NULL, source_id TEXT NOT NULL,
  PRIMARY KEY(project_id, session_id, epoch, generation, publication_id, source_entity_id, target_entity_id, relation, source_id),
  FOREIGN KEY(project_id, session_id, epoch, generation, publication_id)
    REFERENCES conversation_publications_v3(project_id, session_id, epoch, generation, publication_id),
  FOREIGN KEY(source_entity_id) REFERENCES conversation_entities_v3(entity_id),
  FOREIGN KEY(target_entity_id) REFERENCES conversation_entities_v3(entity_id)
);
CREATE TABLE IF NOT EXISTS conversation_fact_revisions_v3 (
  project_id TEXT NOT NULL, session_id TEXT NOT NULL, epoch INTEGER NOT NULL,
  generation INTEGER NOT NULL, publication_id TEXT NOT NULL, fact_id TEXT NOT NULL,
  entity_id TEXT NOT NULL, fact_kind TEXT NOT NULL, value_json TEXT NOT NULL, unit TEXT NOT NULL,
  condition_text TEXT NOT NULL, status TEXT NOT NULL, reason TEXT NOT NULL,
  successor_fact_id TEXT, source_id TEXT NOT NULL,
  PRIMARY KEY(project_id, session_id, epoch, generation, publication_id, fact_id, source_id),
  FOREIGN KEY(project_id, session_id, epoch, generation, publication_id)
    REFERENCES conversation_publications_v3(project_id, session_id, epoch, generation, publication_id),
  FOREIGN KEY(entity_id) REFERENCES conversation_entities_v3(entity_id)
);
CREATE TABLE IF NOT EXISTS conversation_fact_dependencies_v3 (
  project_id TEXT NOT NULL, session_id TEXT NOT NULL, epoch INTEGER NOT NULL,
  generation INTEGER NOT NULL, publication_id TEXT NOT NULL, dependent_fact_id TEXT NOT NULL,
  dependency_fact_id TEXT NOT NULL, source_id TEXT NOT NULL,
  PRIMARY KEY(project_id, session_id, epoch, generation, publication_id, dependent_fact_id, dependency_fact_id, source_id),
  FOREIGN KEY(project_id, session_id, epoch, generation, publication_id)
    REFERENCES conversation_publications_v3(project_id, session_id, epoch, generation, publication_id)
);
CREATE INDEX IF NOT EXISTS conversation_entities_v3_origin
  ON conversation_entities_v3(origin_project_id, origin_session_id, origin_epoch);
CREATE INDEX IF NOT EXISTS conversation_fact_revisions_v3_view
  ON conversation_fact_revisions_v3(project_id, session_id, epoch, generation, entity_id, fact_id);
";

#[derive(Clone, Copy)]
struct ColumnSpec {
    name: &'static str,
    ty: &'static str,
    nullable: bool,
    pk: i64,
}
const fn column(name: &'static str, ty: &'static str, nullable: bool, pk: i64) -> ColumnSpec {
    ColumnSpec {
        name,
        ty,
        nullable,
        pk,
    }
}
const TEXT: &str = "TEXT";
const INTEGER: &str = "INTEGER";

fn verify_schema(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    check_table(
        tx,
        "memory_schema_meta",
        &[
            column("component", TEXT, false, 1),
            column("schema_version", INTEGER, false, 0),
        ],
    )?;
    check_table(
        tx,
        "conversation_publications_v3",
        &scope_columns(&[
            column("publication_id", TEXT, false, 5),
            column("raw_hash", TEXT, false, 0),
            column("delta_hash", TEXT, false, 0),
            column("prompt_version", TEXT, false, 0),
        ]),
    )?;
    check_table(
        tx,
        "conversation_entities_v3",
        &[
            column("entity_id", TEXT, false, 1),
            column("origin_project_id", TEXT, false, 0),
            column("origin_session_id", TEXT, false, 0),
            column("origin_epoch", INTEGER, false, 0),
            column("origin_source_id", TEXT, false, 0),
            column("origin_ordinal", INTEGER, false, 0),
            column("kind", TEXT, false, 0),
        ],
    )?;
    check_table(
        tx,
        "conversation_entity_revisions_v3",
        &scope_columns(&[
            column("publication_id", TEXT, false, 5),
            column("entity_id", TEXT, false, 6),
            column("source_id", TEXT, false, 7),
            column("canonical_name", TEXT, false, 0),
            column("description", TEXT, false, 0),
            column("subject_project", TEXT, false, 0),
        ]),
    )?;
    check_table(
        tx,
        "conversation_aliases_v3",
        &scope_columns(&[
            column("publication_id", TEXT, false, 5),
            column("entity_id", TEXT, false, 6),
            column("display_alias", TEXT, false, 7),
            column("match_alias", TEXT, false, 0),
            column("relation", TEXT, false, 0),
            column("source_id", TEXT, false, 8),
        ]),
    )?;
    check_table(
        tx,
        "conversation_entity_relations_v3",
        &scope_columns(&[
            column("publication_id", TEXT, false, 5),
            column("source_entity_id", TEXT, false, 6),
            column("target_entity_id", TEXT, false, 7),
            column("relation", TEXT, false, 8),
            column("source_id", TEXT, false, 9),
        ]),
    )?;
    check_table(
        tx,
        "conversation_fact_revisions_v3",
        &scope_columns(&[
            column("publication_id", TEXT, false, 5),
            column("fact_id", TEXT, false, 6),
            column("entity_id", TEXT, false, 0),
            column("fact_kind", TEXT, false, 0),
            column("value_json", TEXT, false, 0),
            column("unit", TEXT, false, 0),
            column("condition_text", TEXT, false, 0),
            column("status", TEXT, false, 0),
            column("reason", TEXT, false, 0),
            column("successor_fact_id", TEXT, true, 0),
            column("source_id", TEXT, false, 7),
        ]),
    )?;
    check_table(
        tx,
        "conversation_fact_dependencies_v3",
        &scope_columns(&[
            column("publication_id", TEXT, false, 5),
            column("dependent_fact_id", TEXT, false, 6),
            column("dependency_fact_id", TEXT, false, 7),
            column("source_id", TEXT, false, 8),
        ]),
    )?;
    for table in [
        "conversation_entity_revisions_v3",
        "conversation_aliases_v3",
        "conversation_entity_relations_v3",
        "conversation_fact_revisions_v3",
        "conversation_fact_dependencies_v3",
    ] {
        check_publication_fk(tx, table)?;
    }
    for (table, from) in [
        ("conversation_entity_revisions_v3", "entity_id"),
        ("conversation_aliases_v3", "entity_id"),
        ("conversation_entity_relations_v3", "source_entity_id"),
        ("conversation_entity_relations_v3", "target_entity_id"),
        ("conversation_fact_revisions_v3", "entity_id"),
    ] {
        check_fk(tx, table, from, "conversation_entities_v3", "entity_id")?;
    }
    check_index(
        tx,
        "conversation_entities_v3",
        "conversation_entities_v3_origin",
        &["origin_project_id", "origin_session_id", "origin_epoch"],
    )?;
    check_index(
        tx,
        "conversation_fact_revisions_v3",
        "conversation_fact_revisions_v3_view",
        &[
            "project_id",
            "session_id",
            "epoch",
            "generation",
            "entity_id",
            "fact_id",
        ],
    )?;
    Ok(())
}

fn scope_columns(extra: &[ColumnSpec]) -> Vec<ColumnSpec> {
    let mut columns = vec![
        column("project_id", TEXT, false, 1),
        column("session_id", TEXT, false, 2),
        column("epoch", INTEGER, false, 3),
        column("generation", INTEGER, false, 4),
    ];
    columns.extend_from_slice(extra);
    columns
}
fn check_table(tx: &rusqlite::Transaction<'_>, table: &str, expected: &[ColumnSpec]) -> Result<()> {
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |row| row.get(0),
    )?;
    if !exists {
        return Err(Error::InvalidInput("structured memory schema conflict"));
    }
    let mut statement = tx.prepare(&format!("PRAGMA table_info({table})"))?;
    let actual = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)? != 0,
                row.get::<_, i64>(5)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if actual.len() != expected.len()
        || actual
            .iter()
            .zip(expected)
            .any(|((name, ty, not_null, pk), spec)| {
                name != spec.name || ty != spec.ty || *not_null == spec.nullable || *pk != spec.pk
            })
    {
        return Err(Error::InvalidInput("structured memory schema conflict"));
    }
    Ok(())
}
fn check_publication_fk(tx: &rusqlite::Transaction<'_>, table: &str) -> Result<()> {
    let expected = [
        "project_id",
        "session_id",
        "epoch",
        "generation",
        "publication_id",
    ]
    .into_iter()
    .map(|field| {
        (
            "conversation_publications_v3".to_owned(),
            field.to_owned(),
            Some(field.to_owned()),
        )
    })
    .collect::<Vec<_>>();
    foreign_key_groups(tx, table)?
        .into_values()
        .any(|group| group == expected)
        .then_some(())
        .ok_or(Error::InvalidInput("structured memory schema conflict"))
}
fn check_fk(
    tx: &rusqlite::Transaction<'_>,
    table: &str,
    from: &str,
    foreign_table: &str,
    to: &str,
) -> Result<()> {
    foreign_key_groups(tx, table)?
        .into_values()
        .any(|group| {
            group
                == vec![(
                    foreign_table.to_owned(),
                    from.to_owned(),
                    Some(to.to_owned()),
                )]
        })
        .then_some(())
        .ok_or(Error::InvalidInput("structured memory schema conflict"))
}
type ForeignKeyColumn = (String, String, Option<String>);

fn foreign_key_groups(
    tx: &rusqlite::Transaction<'_>,
    table: &str,
) -> Result<std::collections::BTreeMap<i64, Vec<ForeignKeyColumn>>> {
    let mut statement = tx.prepare(&format!("PRAGMA foreign_key_list({table})"))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut groups =
        std::collections::BTreeMap::<i64, Vec<(i64, String, String, Option<String>)>>::new();
    for (id, sequence, foreign_table, from, to, update, delete, matches) in rows {
        if update != "NO ACTION" || delete != "NO ACTION" || matches != "NONE" {
            return Err(Error::InvalidInput("structured memory schema conflict"));
        }
        groups
            .entry(id)
            .or_default()
            .push((sequence, foreign_table, from, to));
    }
    Ok(groups
        .into_iter()
        .map(|(id, mut group)| {
            group.sort_by_key(|row| row.0);
            (
                id,
                group
                    .into_iter()
                    .map(|(_, table, from, to)| (table, from, to))
                    .collect(),
            )
        })
        .collect())
}
fn check_index(
    tx: &rusqlite::Transaction<'_>,
    table: &str,
    index: &str,
    columns: &[&str],
) -> Result<()> {
    let mut list = tx.prepare(&format!("PRAGMA index_list({table})"))?;
    let exists = list
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(4)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .any(|(name, unique, partial)| name == index && unique == 0 && partial == 0);
    if !exists {
        return Err(Error::InvalidInput("structured memory schema conflict"));
    }
    let mut info = tx.prepare(&format!("PRAGMA index_info({index})"))?;
    let actual = info
        .query_map([], |row| row.get::<_, String>(2))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    actual
        .iter()
        .map(String::as_str)
        .eq(columns.iter().copied())
        .then_some(())
        .ok_or(Error::InvalidInput("structured memory schema conflict"))
}

fn validate_origin_scope(scope: &OriginScope) -> Result<()> {
    validate_metadata(&scope.project_id)?;
    validate_metadata(&scope.session_id)?;
    if scope.epoch < 0 {
        return Err(Error::InvalidInput("invalid structured origin epoch"));
    }
    Ok(())
}
fn validate_publication_scope(scope: &PublicationScope) -> Result<()> {
    validate_metadata(&scope.project_id)?;
    validate_metadata(&scope.session_id)?;
    if scope.epoch < 0 || scope.generation < 0 {
        return Err(Error::InvalidInput("invalid structured publication scope"));
    }
    Ok(())
}
fn validate_metadata(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 1024 {
        return Err(Error::InvalidInput("invalid structured metadata"));
    }
    Ok(())
}
/// The wire field is mandatory, but an empty string is the canonical
/// unconditional fact condition.
fn validate_condition(value: &str) -> Result<()> {
    if value.len() > 1024 {
        return Err(Error::InvalidInput("invalid structured condition"));
    }
    Ok(())
}
fn validate_id(value: &str) -> Result<()> {
    validate_metadata(value)
}
fn validate_hash(value: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(Error::InvalidInput("invalid structured hash"));
    }
    Ok(())
}
fn stable_id(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}
