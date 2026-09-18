//! Relevant state evidence, ordered before coverage/raw checks and context packing.

use super::identity::{IdentityEvidence, IdentitySelection, retrieve_identity};
use polaris_memory::{
    MemoryStore,
    structured::{
        FactStatus, StructuredFactRevision, StructuredPublication, StructuredPublicationView,
        retrieval::StructuredRetrievalEvidence,
    },
};
use std::{cmp::Reverse, collections::BTreeSet, io};

/// Host intermediate only: namesake rows are removed after disambiguation,
/// while explicit dependency closure remains indivisible. This is neither
/// model context nor permission to apply a current value.
pub fn retrieve_state_evidence(
    memory: &MemoryStore,
    view: &StructuredPublicationView,
    query: &str,
) -> io::Result<IdentityEvidence> {
    let mut selected = retrieve_identity(memory, view, query)?;
    let IdentitySelection::Resolved { related, .. } = &selected.selection else {
        // Ambiguous/no-hit classifications must not leak competing values to
        // a later semantic fallback or accidentally become a current answer.
        selected.evidence = None;
        return Ok(selected);
    };
    let evidence = selected
        .evidence
        .as_mut()
        .ok_or_else(|| io::Error::other("構造化記憶の識別根拠がありません"))?;
    if unknown_state_anchor(evidence, related, &selected.matched_aliases, query)? {
        selected.selection = IdentitySelection::NotFound;
        selected.evidence = None;
        return Ok(selected);
    }
    filter_and_rank(evidence, view, related, query)?;
    Ok(selected)
}

fn unknown_state_anchor(
    evidence: &StructuredRetrievalEvidence,
    related: &[String],
    matched_aliases: &BTreeSet<(String, String)>,
    query: &str,
) -> io::Result<bool> {
    let requested = explicit_identifiers(query)?;
    let mut exact = BTreeSet::new();
    let mut folded_names = BTreeSet::new();
    for fact in &evidence.facts {
        if related.contains(&fact.row.entity_id) {
            exact.insert(fact.row.fact_kind.clone());
            exact.insert(fact.row.condition.clone());
        }
    }
    for entity in &evidence.entities {
        if !related.contains(&entity.row.revision.entity_id) {
            continue;
        }
        let names = std::iter::once(entity.row.revision.canonical_name.as_str()).chain(
            matched_aliases
                .iter()
                .filter(|(id, _)| id == &entity.row.revision.entity_id)
                .map(|(_, name)| name.as_str()),
        );
        for name in names {
            if !polaris_memory::structured::source_mentions(entity.row.kind, query, name) {
                continue;
            }
            let mut identifiers = explicit_identifiers(name)?;
            identifiers.insert(name.to_owned());
            if matches!(
                entity.row.kind,
                polaris_memory::structured::EntityKind::Person
                    | polaris_memory::structured::EntityKind::Product
                    | polaris_memory::structured::EntityKind::Other
            ) {
                folded_names.extend(identifiers.into_iter().map(|s| s.to_lowercase()));
            } else {
                exact.extend(identifiers);
            }
        }
    }
    Ok(requested
        .iter()
        .any(|id| !exact.contains(id) && !folded_names.contains(&id.to_lowercase())))
}

/// Explicit code syntax is independent of existing rows. Ordinary prose words
/// are not guessed to be identifiers; backticks can name an otherwise plain key.
fn explicit_identifiers(text: &str) -> io::Result<BTreeSet<String>> {
    let mut result = BTreeSet::new();
    let mut rest = text;
    while let Some((_, after)) = rest.split_once('`') {
        let Some((identifier, after)) = after.split_once('`') else {
            break;
        };
        if !identifier.trim().is_empty() {
            result.insert(identifier.to_owned());
        }
        rest = after;
    }
    for token in text
        .split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | ':' | '-')))
    {
        let token = token.trim_end_matches(['.', ':']);
        if token.chars().any(|c| c.is_ascii_alphanumeric())
            && (token
                .chars()
                .any(|c| matches!(c, '_' | '.' | '/' | ':' | '-'))
                || token
                    .as_bytes()
                    .windows(2)
                    .any(|w| w[0].is_ascii_lowercase() && w[1].is_ascii_uppercase()))
        {
            result.insert(token.to_owned());
        }
    }
    if result.len() > 128 {
        return Err(io::Error::other("構造化問合せの識別子が上限を超えました"));
    }
    Ok(result)
}

fn filter_and_rank(
    evidence: &mut StructuredRetrievalEvidence,
    view: &StructuredPublicationView,
    related: &[String],
    query: &str,
) -> io::Result<()> {
    let mut entities: BTreeSet<_> = related.iter().cloned().collect();
    let mentions = |text: &str| {
        polaris_memory::structured::source_mentions(
            polaris_memory::structured::EntityKind::ConfigurationKey,
            query,
            text,
        )
    };
    let related_facts: Vec<_> = evidence
        .facts
        .iter()
        .filter(|f| entities.contains(&f.row.entity_id))
        .collect();
    let explicit_kinds: BTreeSet<_> = related_facts
        .iter()
        .filter(|f| mentions(&f.row.fact_kind))
        .map(|f| f.row.fact_kind.as_str())
        .collect();
    let explicit_conditions: BTreeSet<_> = related_facts
        .iter()
        .filter(|f| mentions(&f.row.condition))
        .map(|f| f.row.condition.as_str())
        .collect();
    let mut facts: BTreeSet<_> = evidence
        .facts
        .iter()
        .filter(|f| entities.contains(&f.row.entity_id))
        .filter(|f| {
            // An unresolved withdrawal keeps its original fact's kind; its
            // synthetic kind must not hide the absence of a current value.
            let subject = f
                .row
                .reason
                .strip_prefix("withdrawn_without_replacement:")
                .and_then(|id| evidence.facts.iter().find(|f| f.row.fact_id == id))
                .map_or(&f.row, |f| &f.row);
            (explicit_kinds.is_empty()
                || explicit_kinds.contains(f.row.fact_kind.as_str())
                || explicit_kinds.contains(subject.fact_kind.as_str()))
                && (explicit_conditions.is_empty()
                    || subject.condition.is_empty()
                    || explicit_conditions.contains(subject.condition.as_str()))
        })
        .map(|f| f.row.fact_id.clone())
        .collect();
    loop {
        let before = facts.len();
        for edge in &evidence.dependencies {
            let d = &edge.row;
            if facts.contains(&d.dependent_fact_id) || facts.contains(&d.dependency_fact_id) {
                facts.insert(d.dependent_fact_id.clone());
                facts.insert(d.dependency_fact_id.clone());
            }
        }
        for fact in &evidence.facts {
            if facts.contains(&fact.row.fact_id) {
                if let Some(id) = &fact.row.successor_fact_id {
                    facts.insert(id.clone());
                }
                if let Some(id) = fact
                    .row
                    .reason
                    .strip_prefix("withdrawn_without_replacement:")
                {
                    facts.insert(id.to_owned());
                }
            }
        }
        if facts.len() > 64 || entities.len() > 64 {
            return Err(io::Error::other("構造化記憶の関連状態が上限を超えました"));
        }
        if before == facts.len() {
            break;
        }
    }
    evidence.facts.retain(|f| facts.contains(&f.row.fact_id));
    if evidence.facts.len() != facts.len() {
        return Err(io::Error::other("構造化記憶の依存状態がありません"));
    }
    entities.extend(evidence.facts.iter().map(|f| f.row.entity_id.clone()));
    evidence
        .entities
        .retain(|e| entities.contains(&e.row.revision.entity_id));
    if entities.len() > 64 || evidence.entities.len() != entities.len() {
        return Err(io::Error::other("構造化記憶の関連辞書がありません"));
    }
    evidence
        .aliases
        .retain(|a| entities.contains(&a.row.entity_id));
    evidence.relations.retain(|r| {
        entities.contains(&r.row.source_entity_id) && entities.contains(&r.row.target_entity_id)
    });
    evidence
        .dependencies
        .retain(|d| facts.contains(&d.row.dependent_fact_id));

    // Generation numbers are comparable only inside the same scope. The
    // frozen local/ancestor order precedes generation for deterministic ties.
    let scope_rank = |p: &StructuredPublication| {
        if p.scope.project_id == view.scope.project_id
            && p.scope.session_id == view.scope.session_id
            && p.scope.epoch == view.scope.epoch
        {
            0
        } else {
            view.ancestors
                .iter()
                .position(|a| a.session_id == p.scope.session_id && a.epoch == p.scope.epoch)
                .map_or(usize::MAX, |i| i + 1)
        }
    };
    if evidence
        .facts
        .iter()
        .any(|f| scope_rank(&f.publication) == usize::MAX)
    {
        return Err(io::Error::other("構造化記憶の状態が許可scope外です"));
    }
    evidence.facts.sort_by(|a, b| {
        let key =
            |r: &polaris_memory::structured::retrieval::PublishedRow<StructuredFactRevision>| {
                (
                    fact_priority(&r.row),
                    scope_rank(&r.publication),
                    Reverse(r.publication.scope.generation),
                )
            };
        key(a)
            .cmp(&key(b))
            .then_with(|| a.row.fact_id.cmp(&b.row.fact_id))
    });
    Ok(())
}

pub(super) fn fact_priority(fact: &StructuredFactRevision) -> u8 {
    match fact.status {
        FactStatus::Active if fact.fact_kind == "unresolved" || fact.reason == "unresolved" => 0,
        FactStatus::NeedsReconfirmation => 1,
        FactStatus::Superseded if fact.successor_fact_id.is_none() => 2,
        FactStatus::Active => 3,
        FactStatus::Superseded => 4,
        FactStatus::Proposed => 5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_memory::structured::{
        EntityKind, PublicationScope, StructuredAncestorPublicationRange, StructuredEntityRevision,
        StructuredFactDependency,
        retrieval::{PublishedRow, RetrievedEntity},
    };

    fn publication(session: &str, generation: i64) -> StructuredPublication {
        StructuredPublication {
            scope: PublicationScope {
                project_id: "p".into(),
                session_id: session.into(),
                epoch: 0,
                generation,
            },
            publication_id: format!("{session}-{generation}"),
            raw_hash: "a".repeat(64),
            delta_hash: "b".repeat(64),
            prompt_version: "fixture".into(),
        }
    }
    fn view() -> StructuredPublicationView {
        StructuredPublicationView {
            scope: publication("child", 2).scope,
            visible_publication_ids: vec!["child-1".into(), "child-2".into()],
            ancestors: vec!["parent", "grandparent"]
                .into_iter()
                .map(|s| StructuredAncestorPublicationRange {
                    session_id: s.into(),
                    epoch: 0,
                    generation: 99,
                    visible_publication_ids: vec![format!("{s}-99")],
                })
                .collect(),
        }
    }
    fn fact(
        id: &str,
        entity: &str,
        session: &str,
        generation: i64,
        status: FactStatus,
    ) -> PublishedRow<StructuredFactRevision> {
        PublishedRow {
            publication: publication(session, generation),
            row: StructuredFactRevision {
                fact_id: id.into(),
                entity_id: entity.into(),
                fact_kind: "limit".into(),
                value_json: "3".into(),
                unit: "times".into(),
                condition: "production".into(),
                status,
                reason: "assert".into(),
                successor_fact_id: None,
                source_id: format!("source-v3:p:{session}:0:1"),
            },
        }
    }
    fn evidence(facts: Vec<PublishedRow<StructuredFactRevision>>) -> StructuredRetrievalEvidence {
        let ids: BTreeSet<_> = facts.iter().map(|f| f.row.entity_id.clone()).collect();
        StructuredRetrievalEvidence {
            entities: ids
                .into_iter()
                .map(|id| PublishedRow {
                    publication: publication("child", 1),
                    row: RetrievedEntity {
                        kind: EntityKind::Product,
                        revision: StructuredEntityRevision {
                            entity_id: id.clone(),
                            canonical_name: id,
                            description: "fixture".into(),
                            subject_project: "p".into(),
                            source_id: "source-v3:p:child:0:1".into(),
                        },
                    },
                })
                .collect(),
            facts,
            aliases: vec![],
            relations: vec![],
            dependencies: vec![],
        }
    }

    #[test]
    fn explicit_identifiers_preserve_code_case_and_exclude_matched_names() {
        let mut rows = evidence(vec![fact("limit", "Aster", "child", 1, FactStatus::Active)]);
        rows.entities[0].row.revision.canonical_name = "Aster.Engine".into();
        let related = ["Aster".into()];
        let aliases = BTreeSet::from([("Aster".into(), "Rigel.Legacy".into())]);
        assert!(
            !unknown_state_anchor(&rows, &related, &aliases, "旧rigel.legacyの `limit` を確認")
                .unwrap()
        );
        assert!(
            !unknown_state_anchor(&rows, &related, &aliases, "Aster.Engineの状態を確認。").unwrap()
        );
        assert!(
            unknown_state_anchor(&rows, &related, &aliases, "Aster.Engine unlisted.config")
                .unwrap()
        );
        rows.entities[0].row.kind = EntityKind::ConfigurationKey;
        assert!(!unknown_state_anchor(&rows, &related, &aliases, "Aster.Engine").unwrap());
        assert!(unknown_state_anchor(&rows, &related, &aliases, "aster.engine").unwrap());
        assert_eq!(
            explicit_identifiers(
                "文章 `未知設定` と missingValue、v0.12.0 /tmp/file.rs。ordinary words."
            )
            .unwrap(),
            BTreeSet::from([
                "未知設定".into(),
                "missingValue".into(),
                "v0.12.0".into(),
                "/tmp/file.rs".into()
            ])
        );
    }

    #[test]
    fn explicit_kind_and_condition_select_before_dependency_closure() {
        let mut original = evidence(vec![
            fact("limit-prod", "Aster", "child", 1, FactStatus::Superseded),
            fact("limit-dev", "Aster", "child", 1, FactStatus::Active),
            fact("color", "Aster", "child", 1, FactStatus::Active),
            fact("unresolved", "Aster", "child", 1, FactStatus::Active),
            fact(
                "plan",
                "Deployment",
                "child",
                1,
                FactStatus::NeedsReconfirmation,
            ),
        ]);
        original.facts[1].row.condition = "development".into();
        original.facts[2].row.fact_kind = "color".into();
        original.facts[2].row.value_json = json_string(&"blue ".repeat(1000));
        original.facts[3].row.fact_kind = "unresolved".into();
        original.facts[3].row.reason = "withdrawn_without_replacement:limit-prod".into();
        original.facts[4].row.fact_kind = "deployment".into();
        original.dependencies.push(PublishedRow {
            publication: publication("child", 1),
            row: StructuredFactDependency {
                dependent_fact_id: "plan".into(),
                dependency_fact_id: "limit-prod".into(),
                source_id: "source-v3:p:child:0:1".into(),
            },
        });
        let mut selected = original.clone();
        filter_and_rank(
            &mut selected,
            &view(),
            &["Aster".into()],
            "Asterのproduction limitを確認",
        )
        .unwrap();
        assert_eq!(
            selected
                .facts
                .iter()
                .map(|f| f.row.fact_id.as_str())
                .collect::<Vec<_>>(),
            ["unresolved", "plan", "limit-prod"]
        );
        assert_eq!(selected.dependencies.len(), 1);
        let mut unresolved = original.clone();
        filter_and_rank(
            &mut unresolved,
            &view(),
            &["Aster".into()],
            "Aster unresolvedを確認",
        )
        .unwrap();
        assert_eq!(
            unresolved
                .facts
                .iter()
                .map(|f| f.row.fact_id.as_str())
                .collect::<Vec<_>>(),
            ["unresolved", "plan", "limit-prod"]
        );
        // Partial code identifiers do not silently narrow unrelated facts.
        filter_and_rank(
            &mut original,
            &view(),
            &["Aster".into()],
            "Aster limit_extra production_copy",
        )
        .unwrap();
        assert_eq!(original.facts.len(), 5);
    }

    fn json_string(value: &str) -> String {
        serde_json::to_string(value).unwrap()
    }

    #[test]
    fn relevant_dependency_chain_is_indivisible_and_namesake_is_excluded() {
        let mut original = evidence(vec![
            fact("decision", "Aster", "child", 1, FactStatus::Active),
            fact("other", "Lumen", "child", 1, FactStatus::Active),
            fact(
                "plan",
                "Deployment",
                "child",
                1,
                FactStatus::NeedsReconfirmation,
            ),
            fact(
                "report",
                "Report",
                "child",
                1,
                FactStatus::NeedsReconfirmation,
            ),
        ]);
        for (dependent, dependency) in [("report", "plan"), ("plan", "decision")] {
            original.dependencies.push(PublishedRow {
                publication: publication("child", 1),
                row: StructuredFactDependency {
                    dependent_fact_id: dependent.into(),
                    dependency_fact_id: dependency.into(),
                    source_id: "source-v3:p:child:0:1".into(),
                },
            });
        }
        for reverse in [false, true] {
            let mut result = original.clone();
            if reverse {
                result.dependencies.reverse();
                result.facts.reverse();
            }
            filter_and_rank(&mut result, &view(), &["Aster".into()], "Aster").unwrap();
            assert_eq!(
                result
                    .facts
                    .iter()
                    .map(|f| f.row.fact_id.as_str())
                    .collect::<Vec<_>>(),
                vec!["plan", "report", "decision"]
            );
            assert_eq!(result.entities.len(), 3);
            assert_eq!(result.dependencies.len(), 2);
            assert!(
                !result
                    .entities
                    .iter()
                    .any(|e| e.row.revision.canonical_name == "Lumen")
            );
        }
        original.facts.retain(|f| f.row.fact_id != "report");
        assert!(filter_and_rank(&mut original, &view(), &["Aster".into()], "Aster").is_err());
    }

    #[test]
    fn rank_compares_generation_inside_scope_and_stable_id_for_equal_generation() {
        let mut rows = evidence(vec![
            fact("distant", "Aster", "grandparent", 99, FactStatus::Active),
            fact("parent", "Aster", "parent", 99, FactStatus::Active),
            fact("old-local", "Aster", "child", 1, FactStatus::Active),
            fact("z", "Aster", "child", 2, FactStatus::Active),
            fact("a", "Aster", "child", 2, FactStatus::Active),
        ]);
        filter_and_rank(&mut rows, &view(), &["Aster".into()], "Aster").unwrap();
        assert_eq!(
            rows.facts
                .iter()
                .map(|f| f.row.fact_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "z", "old-local", "parent", "distant"]
        );
        rows.facts.reverse();
        rows.facts
            .iter_mut()
            .find(|f| f.row.fact_id == "distant")
            .unwrap()
            .row
            .status = FactStatus::NeedsReconfirmation;
        filter_and_rank(&mut rows, &view(), &["Aster".into()], "Aster").unwrap();
        assert_eq!(rows.facts[0].row.fact_id, "distant");
        assert_eq!(rows.facts[1].row.fact_id, "a");
        rows.facts.push(fact(
            "withdrawn",
            "Aster",
            "child",
            1,
            FactStatus::Superseded,
        ));
        let mut replaced = fact("replaced", "Aster", "child", 1, FactStatus::Superseded);
        replaced.row.successor_fact_id = Some("a".into());
        rows.facts.push(replaced);
        filter_and_rank(&mut rows, &view(), &["Aster".into()], "Aster").unwrap();
        assert_eq!(
            rows.facts
                .iter()
                .map(|f| f.row.fact_id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "distant",
                "withdrawn",
                "a",
                "z",
                "old-local",
                "parent",
                "replaced"
            ],
        );
    }
}
