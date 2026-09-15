//! Bounded, source-backed rows for one frozen structured-memory retrieval.

use super::*;
use serde::de::DeserializeOwned;
use std::collections::{BTreeMap, BTreeSet};

const MAX_ENTITIES: usize = 64;
const MAX_FACTS: usize = 64;
const MAX_SCAN_ROWS: usize = 16_384;
const MAX_SCAN_BYTES: usize = 4 * 1024 * 1024;
const MAX_RETAINED_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishedRow<T> {
    pub row: T,
    pub publication: StructuredPublication,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetrievedEntity {
    pub kind: EntityKind,
    #[serde(flatten)]
    pub revision: StructuredEntityRevision,
}

/// These are historical evidence, never instructions or an execution grant.
/// Query disambiguation, raw verification and context packing happen above
/// this storage layer; a complete bounded set is returned or nothing is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredRetrievalEvidence {
    pub entities: Vec<PublishedRow<RetrievedEntity>>,
    pub facts: Vec<PublishedRow<StructuredFactRevision>>,
    pub aliases: Vec<PublishedRow<StructuredAlias>>,
    pub relations: Vec<PublishedRow<StructuredEntityRelation>>,
    pub dependencies: Vec<PublishedRow<StructuredFactDependency>>,
}

struct ScopedRow<T> {
    scope_rank: usize,
    evidence: PublishedRow<T>,
}

#[derive(Default)]
struct ScanBudget {
    rows: usize,
    bytes: usize,
}
impl ScanBudget {
    fn charge(&mut self, bytes: usize) -> Result<()> {
        self.rows = self.rows.saturating_add(1);
        self.bytes = self.bytes.saturating_add(bytes);
        if self.rows > MAX_SCAN_ROWS || self.bytes > MAX_SCAN_BYTES {
            return Err(Error::InvalidInput("structured retrieval scan limit"));
        }
        Ok(())
    }
}

impl MemoryStore {
    pub fn structured_retrieval_evidence(
        &self,
        view: &StructuredPublicationView,
        entity_ids: &[String],
    ) -> Result<StructuredRetrievalEvidence> {
        validate_structured_view(view)?;
        let seeds: BTreeSet<_> = entity_ids.iter().cloned().collect();
        if seeds.len() != entity_ids.len() || seeds.len() > MAX_ENTITIES {
            return Err(Error::InvalidInput("structured retrieval seed limit"));
        }
        for id in &seeds {
            validate_id(id)?;
        }
        let transaction = self.structured_read_transaction()?;
        let tx = &self.connection;
        let mut budget = ScanBudget::default();
        let mut grants = Vec::new();
        for (rank, (scope, ids)) in publication_scopes(view).into_iter().enumerate() {
            if structured_scope_forgotten(tx, &scope)? {
                return Err(Error::InvalidInput("structured retrieval scope forgotten"));
            }
            for id in ids {
                budget.charge(id.len())?;
                let publication = tx.query_row(
                    "SELECT generation,raw_hash,delta_hash,prompt_version FROM conversation_publications_v3 WHERE project_id=?1 AND session_id=?2 AND epoch=?3 AND generation<=?4 AND publication_id=?5",
                    params![scope.project_id,scope.session_id,scope.epoch,scope.generation,id],
                    |r| Ok(StructuredPublication { scope: PublicationScope { generation: r.get(0)?, ..scope.clone() }, publication_id: id.clone(), raw_hash: r.get(1)?, delta_hash: r.get(2)?, prompt_version: r.get(3)? }),
                ).optional()?.ok_or(Error::InvalidInput("structured retrieval publication is absent"))?;
                validate_publication(&publication)?;
                grants.push((rank, publication));
            }
        }
        if seeds.is_empty() {
            // Validate exact publication grants and tombstones even on no-hit,
            // without charging unrelated fact rows to a nonexistent match.
            if let Some(transaction) = transaction {
                transaction.commit()?;
            }
            return Ok(StructuredRetrievalEvidence {
                entities: vec![],
                facts: vec![],
                aliases: vec![],
                relations: vec![],
                dependencies: vec![],
            });
        }
        let entities = latest(
            read_rows::<RetrievedEntity>(
                tx,
                &grants,
                &mut budget,
                "conversation_entity_revisions_v3",
                "json_object('entity_id',entity_id,'source_id',source_id,'canonical_name',canonical_name,'description',description,'subject_project',subject_project,'kind',(SELECT kind FROM conversation_entities_v3 e WHERE e.entity_id=r.entity_id))",
            )?,
            |e| &e.revision.entity_id,
            |e| &e.revision.source_id,
        )?;
        let facts = latest(
            read_rows::<StructuredFactRevision>(
                tx,
                &grants,
                &mut budget,
                "conversation_fact_revisions_v3",
                "json_object('fact_id',fact_id,'entity_id',entity_id,'fact_kind',fact_kind,'value_json',value_json,'unit',unit,'condition',condition_text,'status',status,'reason',reason,'successor_fact_id',successor_fact_id,'source_id',source_id)",
            )?,
            |f| &f.fact_id,
            |f| &f.source_id,
        )?;
        let aliases = read_rows::<StructuredAlias>(
            tx,
            &grants,
            &mut budget,
            "conversation_aliases_v3",
            "json_object('entity_id',entity_id,'display_alias',display_alias,'match_alias',match_alias,'relation',relation,'source_id',source_id)",
        )?;
        let relations = read_rows::<StructuredEntityRelation>(
            tx,
            &grants,
            &mut budget,
            "conversation_entity_relations_v3",
            "json_object('source_entity_id',source_entity_id,'target_entity_id',target_entity_id,'relation',relation,'source_id',source_id)",
        )?;
        let dependencies = effective_dependencies(read_rows::<StructuredFactDependency>(
            tx,
            &grants,
            &mut budget,
            "conversation_fact_dependencies_v3",
            "json_object('dependent_fact_id',dependent_fact_id,'dependency_fact_id',dependency_fact_id,'source_id',source_id)",
        )?)?;
        for id in &seeds {
            if !entities.contains_key(id) {
                return Err(Error::InvalidInput("structured retrieval entity is absent"));
            }
        }
        let mut selected_entities = seeds.clone();
        let mut selected_relations = Vec::new();
        for relation in relations {
            let row = &relation.evidence.row;
            if !matches!(
                row.relation.as_str(),
                "owns" | "belongs_to" | "distinct_from"
            ) {
                return Err(Error::InvalidInput(
                    "structured retrieval relation is invalid",
                ));
            }
            if seeds.contains(&row.source_entity_id) || seeds.contains(&row.target_entity_id) {
                selected_entities.insert(row.source_entity_id.clone());
                selected_entities.insert(row.target_entity_id.clone());
                selected_relations.push(relation.evidence);
            }
        }
        let mut selected_facts: BTreeSet<_> = facts
            .iter()
            .filter(|(_, f)| selected_entities.contains(&f.row.entity_id))
            .map(|(id, _)| id.clone())
            .collect();
        loop {
            check_identity_caps(&selected_entities, &selected_facts)?;
            let before = selected_facts.len();
            for dependency in &dependencies {
                let d = &dependency.row;
                if selected_facts.contains(&d.dependent_fact_id)
                    || selected_facts.contains(&d.dependency_fact_id)
                {
                    selected_facts.insert(d.dependent_fact_id.clone());
                    selected_facts.insert(d.dependency_fact_id.clone());
                    check_identity_caps(&selected_entities, &selected_facts)?;
                }
            }
            if before == selected_facts.len() {
                break;
            }
        }
        for id in &selected_facts {
            let fact = facts.get(id).ok_or(Error::InvalidInput(
                "structured retrieval dependency is absent",
            ))?;
            selected_entities.insert(fact.row.entity_id.clone());
        }
        check_identity_caps(&selected_entities, &selected_facts)?;
        let entities = selected_entities
            .iter()
            .map(|id| {
                entities.get(id).cloned().ok_or(Error::InvalidInput(
                    "structured retrieval related entity is absent",
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let evidence = StructuredRetrievalEvidence {
            entities,
            facts: selected_facts.iter().map(|id| facts[id].clone()).collect(),
            aliases: aliases
                .into_iter()
                .filter(|r| selected_entities.contains(&r.evidence.row.entity_id))
                .map(|r| r.evidence)
                .collect(),
            relations: selected_relations,
            dependencies: dependencies
                .into_iter()
                .filter(|r| selected_facts.contains(&r.row.dependent_fact_id))
                .collect(),
        };
        if serde_json::to_vec(&evidence)
            .map_err(std::io::Error::other)?
            .len()
            > MAX_RETAINED_BYTES
        {
            return Err(Error::InvalidInput(
                "structured retrieval retained byte limit",
            ));
        }
        if let Some(transaction) = transaction {
            transaction.commit()?;
        }
        Ok(evidence)
    }
}

fn check_identity_caps(entities: &BTreeSet<String>, facts: &BTreeSet<String>) -> Result<()> {
    if entities.len() > MAX_ENTITIES || facts.len() > MAX_FACTS {
        Err(Error::InvalidInput("structured retrieval identity limit"))
    } else {
        Ok(())
    }
}

/// `table` and `projection` are internal SQL literals, never user/model input.
fn read_rows<T: DeserializeOwned>(
    tx: &rusqlite::Connection,
    grants: &[(usize, StructuredPublication)],
    budget: &mut ScanBudget,
    table: &str,
    projection: &str,
) -> Result<Vec<ScopedRow<T>>> {
    let sql = format!(
        "SELECT {projection} FROM {table} r WHERE project_id=?1 AND session_id=?2 AND epoch=?3 AND generation=?4 AND publication_id=?5 ORDER BY source_id,{projection}"
    );
    let mut query = tx.prepare(&sql)?;
    let mut out = Vec::new();
    for (rank, publication) in grants {
        let p = &publication.scope;
        for row in query.query_map(
            params![
                p.project_id,
                p.session_id,
                p.epoch,
                p.generation,
                publication.publication_id
            ],
            |r| r.get::<_, String>(0),
        )? {
            let body = row?;
            budget.charge(
                body.len().saturating_add(
                    serde_json::to_vec(publication)
                        .map_err(std::io::Error::other)?
                        .len(),
                ),
            )?;
            let row = serde_json::from_str(&body)
                .map_err(|_| Error::InvalidInput("structured retrieval row is invalid"))?;
            out.push(ScopedRow {
                scope_rank: *rank,
                evidence: PublishedRow {
                    row,
                    publication: publication.clone(),
                },
            });
        }
    }
    Ok(out)
}

fn latest<T: PartialEq>(
    rows: Vec<ScopedRow<T>>,
    id: impl Fn(&T) -> &String,
    source: impl Fn(&T) -> &String,
) -> Result<BTreeMap<String, PublishedRow<T>>> {
    let mut selected: BTreeMap<String, ScopedRow<T>> = BTreeMap::new();
    for candidate in rows {
        let key = id(&candidate.evidence.row).clone();
        if let Some(previous) = selected.get(&key) {
            let order = |r: &ScopedRow<T>| {
                (
                    std::cmp::Reverse(r.scope_rank),
                    r.evidence.publication.scope.generation,
                    source_sequence(source(&r.evidence.row)),
                )
            };
            match order(&candidate).cmp(&order(previous)) {
                std::cmp::Ordering::Less => continue,
                std::cmp::Ordering::Equal => {
                    if previous.evidence != candidate.evidence {
                        return Err(Error::InvalidInput(
                            "structured retrieval revision is ambiguous",
                        ));
                    }
                    continue;
                }
                std::cmp::Ordering::Greater => {}
            }
        }
        selected.insert(key, candidate);
    }
    Ok(selected
        .into_iter()
        .map(|(id, row)| (id, row.evidence))
        .collect())
}

fn effective_dependencies(
    rows: Vec<ScopedRow<StructuredFactDependency>>,
) -> Result<Vec<PublishedRow<StructuredFactDependency>>> {
    let mut groups: BTreeMap<String, Vec<ScopedRow<StructuredFactDependency>>> = BTreeMap::new();
    for row in rows {
        groups
            .entry(row.evidence.row.dependent_fact_id.clone())
            .or_default()
            .push(row);
    }
    let mut effective = Vec::new();
    for rows in groups.into_values() {
        let order = |r: &ScopedRow<StructuredFactDependency>| {
            (
                std::cmp::Reverse(r.scope_rank),
                r.evidence.publication.scope.generation,
                source_sequence(&r.evidence.row.source_id),
            )
        };
        let highest = rows.iter().map(order).max();
        let selected: Vec<_> = rows
            .into_iter()
            .filter(|r| Some(order(r)) == highest)
            .map(|r| r.evidence)
            .collect();
        if let Some(first) = selected.first()
            && selected.iter().any(|row| {
                row.publication != first.publication || row.row.source_id != first.row.source_id
            })
        {
            return Err(Error::InvalidInput(
                "structured dependency revision is ambiguous",
            ));
        }
        effective.extend(selected);
    }
    Ok(effective)
}
