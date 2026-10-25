//! Source-ordered fact transitions bound to published entity and fact identities.

use super::*;
use polaris_memory::structured::{StructuredFactDependency, validate_fact_transition};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FactCandidate {
    pub source_id: String,
    evidence_span: Span,
    speech_act: String,
    pub ordinal: u32,
    entity_kind: EntityKind,
    entity_name: String,
    #[serde(default)]
    subject_project: Option<String>,
    fact_kind: String,
    value: serde_json::Value,
    unit: String,
    condition: String,
    relation: FactRelation,
    evidence_text: String,
    #[serde(default)]
    depends_on: Vec<FactReference>,
}

/// Descriptions only: IDs and the target's current state come from the host.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FactReference {
    entity_kind: EntityKind,
    entity_name: String,
    subject_project: String,
    fact_kind: String,
    condition: String,
}

#[derive(Default)]
pub(super) struct FactUpdates {
    pub rows: BTreeMap<(String, String), StructuredFactRevision>,
    current: BTreeMap<String, StructuredFactRevision>,
    pub dependencies: Vec<StructuredFactDependency>,
    ordinals: BTreeSet<(String, u32)>,
}

impl FactUpdates {
    pub fn apply(
        &mut self,
        c: FactCandidate,
        sources: &BTreeMap<&str, &StructuredPromptSource>,
        user_inputs: &BTreeSet<u64>,
        resolver: &dyn StructuredResolver,
        entity: &EntityResolver<'_>,
        default_project: &str,
    ) -> io::Result<()> {
        let source = checked_source(sources, &c.source_id, &c.evidence_span, &c.evidence_text)?;
        let project = c.subject_project.as_deref().unwrap_or(default_project);
        validate_candidate_fields(
            &c.speech_act,
            &[
                &c.entity_name,
                project,
                &c.fact_kind,
                &c.unit,
                &c.evidence_text,
            ],
        )?;
        if c.condition.len() > MAX_METADATA_BYTES
            || c.depends_on.len() > 32
            || !self.ordinals.insert((c.source_id.clone(), c.ordinal))
        {
            return Err(io::Error::other("Invalid structured fact limit or ordinal"));
        }
        let value_json = serde_json::to_string(&c.value)?;
        if value_json.len() > 16 * 1024 {
            return Err(io::Error::other("Structured fact value exceeds 16 KiB"));
        }
        let entity_id = entity(c.entity_kind, &c.entity_name, project)?.ok_or_else(|| {
            io::Error::other("The structured fact entity is missing or ambiguous")
        })?;
        let authoritative = source.role == Role::User
            && user_inputs.contains(&source.sequence)
            && matches!(
                c.speech_act.as_str(),
                "assert" | "adopt" | "correct" | "withdraw"
            );
        let mut row = StructuredFactRevision {
            fact_id: stable_fact_id(&entity_id, &c.fact_kind, &c.condition, &c.source_id)
                .map_err(io::Error::other)?,
            entity_id,
            fact_kind: c.fact_kind,
            value_json,
            unit: c.unit,
            condition: c.condition,
            status: FactStatus::Proposed,
            reason: "unconfirmed".into(),
            successor_fact_id: None,
            source_id: c.source_id.clone(),
        };
        if !authoritative || c.relation == FactRelation::Propose {
            if !c.depends_on.is_empty() {
                return Err(io::Error::other(
                    "Cannot create dependencies from an unadopted utterance",
                ));
            }
            row.reason = if c.speech_act == "propose" {
                "proposed"
            } else {
                "unconfirmed"
            }
            .into();
            return self.record_new(row, FactRelation::Propose);
        }
        match c.relation {
            FactRelation::Assert | FactRelation::Unresolved | FactRelation::DependsOn => {
                if c.speech_act == "withdraw" {
                    return Err(io::Error::other(
                        "A retraction utterance cannot be used for adoption",
                    ));
                }
                if c.relation == FactRelation::Unresolved && !c.value.is_null() {
                    return Err(io::Error::other("An unresolved fact cannot have a value"));
                }
                if c.relation == FactRelation::DependsOn && c.depends_on.is_empty() {
                    return Err(io::Error::other("Missing dependency target"));
                }
                // A second assertion cannot silently replace an existing decision.
                if self
                    .current_target(resolver, &row.entity_id, &row.fact_kind, &row.condition)?
                    .is_some()
                {
                    return Err(io::Error::other(
                        "An existing decision requires an explicit correction",
                    ));
                }
                row.status = FactStatus::Active;
                row.reason = if c.relation == FactRelation::Unresolved {
                    "unresolved"
                } else {
                    &c.speech_act
                }
                .into();
                self.bind_dependencies(&row, &c.depends_on, resolver, entity, &c.source_id)?;
                self.record_new(row, c.relation)?;
            }
            FactRelation::Replace | FactRelation::Withdraw => {
                if (c.relation == FactRelation::Withdraw
                    && (c.speech_act != "withdraw" || !c.value.is_null()))
                    || (c.relation == FactRelation::Replace
                        && !matches!(c.speech_act.as_str(), "adopt" | "correct"))
                    || (c.relation == FactRelation::Withdraw && !c.depends_on.is_empty())
                {
                    return Err(io::Error::other(
                        "The correction or retraction utterance does not match the candidate",
                    ));
                }
                let mut previous = self
                    .current_target(resolver, &row.entity_id, &row.fact_kind, &row.condition)?
                    .ok_or_else(|| io::Error::other("The fact targeted for correction or retraction is missing or ambiguous"))?;
                if c.relation == FactRelation::Withdraw && previous.fact_kind == "unresolved" {
                    return Err(io::Error::other(
                        "The target decision has already been retracted",
                    ));
                }
                if c.relation == FactRelation::Withdraw {
                    row.unit = previous.unit.clone();
                }
                validate_fact_transition(Some(previous.status), FactStatus::Superseded, c.relation)
                    .map_err(io::Error::other)?;
                previous.status = FactStatus::Superseded;
                previous.reason = if c.relation == FactRelation::Withdraw {
                    "withdrawn"
                } else {
                    "replaced"
                }
                .into();
                previous.source_id = c.source_id.clone();
                previous.successor_fact_id =
                    (c.relation == FactRelation::Replace).then(|| row.fact_id.clone());
                let old_id = previous.fact_id.clone();
                self.record(previous)?;
                row.status = FactStatus::Active;
                if c.relation == FactRelation::Withdraw {
                    row.fact_kind = "unresolved".into();
                    row.fact_id = stable_fact_id(
                        &row.entity_id,
                        &row.fact_kind,
                        &row.condition,
                        &c.source_id,
                    )
                    .map_err(io::Error::other)?;
                    row.reason = format!("withdrawn_without_replacement:{old_id}");
                } else {
                    row.reason = "replacement".into();
                }
                self.bind_dependencies(&row, &c.depends_on, resolver, entity, &c.source_id)?;
                self.record_new(
                    row,
                    if c.relation == FactRelation::Withdraw {
                        FactRelation::Unresolved
                    } else {
                        FactRelation::Replace
                    },
                )?;
                self.invalidate_dependents(resolver, &old_id, &c.source_id)?;
            }
            FactRelation::Propose => unreachable!(),
        }
        if self.dependencies.len() > 32 {
            return Err(io::Error::other(
                "Structured dependencies exceed 32 entries",
            ));
        }
        Ok(())
    }

    fn current_target(
        &self,
        resolver: &dyn StructuredResolver,
        entity: &str,
        kind: &str,
        condition: &str,
    ) -> io::Result<Option<StructuredFactRevision>> {
        let mut rows: BTreeMap<_, _> = resolver
            .facts(entity, kind, condition)?
            .into_iter()
            .map(|r| (r.fact_id.clone(), r))
            .collect();
        for row in self
            .current
            .values()
            .filter(|r| r.entity_id == entity && r.fact_kind == kind && r.condition == condition)
        {
            rows.insert(row.fact_id.clone(), row.clone());
        }
        if kind != "unresolved" {
            let mut unresolved: BTreeMap<_, _> = resolver
                .facts(entity, "unresolved", condition)?
                .into_iter()
                .map(|r| (r.fact_id.clone(), r))
                .collect();
            for row in self.current.values().filter(|r| {
                r.entity_id == entity && r.fact_kind == "unresolved" && r.condition == condition
            }) {
                unresolved.insert(row.fact_id.clone(), row.clone());
            }
            for row in unresolved
                .into_values()
                .filter(|r| r.status == FactStatus::Active)
            {
                let Some(old_id) = row.reason.strip_prefix("withdrawn_without_replacement:") else {
                    continue;
                };
                let old = self
                    .current
                    .get(old_id)
                    .cloned()
                    .map(Ok)
                    .unwrap_or_else(|| {
                        resolver.fact(old_id).and_then(|r| {
                            r.ok_or_else(|| {
                                io::Error::other(
                                    "The retraction source fact is not in the allowed view",
                                )
                            })
                        })
                    })?;
                if old.entity_id == entity && old.fact_kind == kind && old.condition == condition {
                    rows.insert(row.fact_id.clone(), row);
                }
            }
        }
        let mut current = rows.into_values().filter(|r| {
            matches!(
                r.status,
                FactStatus::Active | FactStatus::NeedsReconfirmation
            )
        });
        let result = current.next();
        if current.next().is_some() {
            return Err(io::Error::other(
                "The current structured fact value is ambiguous",
            ));
        }
        Ok(result)
    }

    fn bind_dependencies(
        &mut self,
        row: &StructuredFactRevision,
        references: &[FactReference],
        resolver: &dyn StructuredResolver,
        entity: &EntityResolver<'_>,
        source: &str,
    ) -> io::Result<()> {
        for reference in references {
            validate_candidate_fields(
                "assert",
                &[
                    &reference.entity_name,
                    &reference.subject_project,
                    &reference.fact_kind,
                ],
            )?;
            if reference.condition.len() > MAX_METADATA_BYTES {
                return Err(io::Error::other("Dependency conditions exceed the limit"));
            }
            let target_entity = entity(
                reference.entity_kind,
                &reference.entity_name,
                &reference.subject_project,
            )?
            .ok_or_else(|| io::Error::other("The dependency entity is missing or ambiguous"))?;
            let target = self
                .current_target(
                    resolver,
                    &target_entity,
                    &reference.fact_kind,
                    &reference.condition,
                )?
                .filter(|r| r.status == FactStatus::Active)
                .ok_or_else(|| io::Error::other("The dependency fact is invalid"))?;
            if target.fact_id == row.fact_id {
                return Err(io::Error::other("Self-dependency is not allowed"));
            }
            self.dependencies.push(StructuredFactDependency {
                dependent_fact_id: row.fact_id.clone(),
                dependency_fact_id: target.fact_id,
                source_id: source.to_owned(),
            });
        }
        if self.dependencies.len() > 32 {
            return Err(io::Error::other(
                "Structured dependencies exceed 32 entries",
            ));
        }
        Ok(())
    }

    fn record_new(
        &mut self,
        row: StructuredFactRevision,
        relation: FactRelation,
    ) -> io::Result<()> {
        validate_fact_transition(None, row.status, relation).map_err(io::Error::other)?;
        if self.current.contains_key(&row.fact_id) {
            return Err(io::Error::other("Duplicate facts for the same source"));
        }
        self.record(row)
    }
    fn record(&mut self, row: StructuredFactRevision) -> io::Result<()> {
        self.current.insert(row.fact_id.clone(), row.clone());
        self.rows
            .insert((row.fact_id.clone(), row.source_id.clone()), row);
        if self.rows.len() > 64 {
            return Err(io::Error::other(
                "Structured fact revisions exceed 64 entries",
            ));
        }
        Ok(())
    }

    fn invalidate_dependents(
        &mut self,
        resolver: &dyn StructuredResolver,
        root: &str,
        source: &str,
    ) -> io::Result<()> {
        let mut queue = vec![root.to_owned()];
        let mut visited = BTreeSet::new();
        while let Some(id) = queue.pop() {
            if !visited.insert(id.clone()) {
                continue;
            }
            if visited.len() > 64 {
                return Err(io::Error::other("Dependency updates exceed 64 entries"));
            }
            let mut ids: BTreeSet<_> = resolver.dependents(&id)?.into_iter().collect();
            ids.extend(
                self.dependencies
                    .iter()
                    .filter(|d| d.dependency_fact_id == id)
                    .map(|d| d.dependent_fact_id.clone()),
            );
            for dependent in ids {
                let mut row = match self.current.get(&dependent) {
                    Some(row) => row.clone(),
                    None => resolver.fact(&dependent)?.ok_or_else(|| {
                        io::Error::other("The dependency fact is not in the allowed view")
                    })?,
                };
                if row.status == FactStatus::Active {
                    validate_fact_transition(
                        Some(row.status),
                        FactStatus::NeedsReconfirmation,
                        FactRelation::DependsOn,
                    )
                    .map_err(io::Error::other)?;
                    row.status = FactStatus::NeedsReconfirmation;
                    row.reason = "dependency_changed".into();
                    row.source_id = source.into();
                    self.record(row)?;
                    queue.push(dependent);
                }
            }
        }
        Ok(())
    }
}
