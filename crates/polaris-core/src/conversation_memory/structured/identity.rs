//! Deterministic name and explicit-relation disambiguation for structured memory.

use polaris_memory::structured::{EntityKind, StructuredEntityHint, StructuredEntityRelation};
use std::{collections::BTreeSet, io};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentitySelection {
    NotFound,
    Ambiguous,
    /// Identity resolution only. This grants no permission to use a fact as
    /// the current value; coverage, fact states and raw evidence follow.
    Resolved {
        anchors: Vec<String>,
        related: Vec<String>,
    },
}

/// A host-owned intermediate result. Only the later evidence packer may turn
/// these rows into model context after checking coverage and raw provenance.
#[derive(Debug, Clone)]
pub struct IdentityEvidence {
    pub selection: IdentitySelection,
    pub evidence: Option<polaris_memory::structured::retrieval::StructuredRetrievalEvidence>,
    /// Exact display aliases matched by the storage layer's kind-aware matcher.
    pub matched_aliases: BTreeSet<(String, String)>,
}

pub fn retrieve_identity(
    memory: &polaris_memory::MemoryStore,
    view: &polaris_memory::structured::StructuredPublicationView,
    query: &str,
) -> io::Result<IdentityEvidence> {
    if query.trim().is_empty() {
        return Err(io::Error::other("structured query is empty"));
    }
    let hints = memory
        .structured_entity_hints(view, &[query])
        .map_err(io::Error::other)?;
    if hints.is_empty() {
        // No match does not waive frozen-publication or tombstone validation.
        memory
            .structured_retrieval_evidence(view, &[])
            .map_err(io::Error::other)?;
        return Ok(IdentityEvidence {
            selection: IdentitySelection::NotFound,
            evidence: None,
            matched_aliases: BTreeSet::new(),
        });
    }
    let ids = hints
        .iter()
        .map(|hint| hint.entity_id.clone())
        .collect::<Vec<_>>();
    let evidence = memory
        .structured_retrieval_evidence(view, &ids)
        .map_err(io::Error::other)?;
    let relations = evidence
        .relations
        .iter()
        .map(|relation| relation.row.clone())
        .collect::<Vec<_>>();
    let selection = select_identity(query, &hints, &relations)?;
    Ok(IdentityEvidence {
        selection,
        evidence: Some(evidence),
        matched_aliases: hints
            .into_iter()
            .flat_map(|h| {
                h.matching_aliases
                    .into_iter()
                    .map(move |alias| (h.entity_id.clone(), alias))
            })
            .collect(),
    })
}

fn select_identity(
    query: &str,
    hints: &[StructuredEntityHint],
    relations: &[StructuredEntityRelation],
) -> io::Result<IdentitySelection> {
    if query.len() > 64 * 1024 || hints.len() > 64 || relations.len() > 512 {
        return Err(io::Error::other("structured identity selection limit"));
    }
    if hints.is_empty() {
        return Ok(IdentitySelection::NotFound);
    }
    let normalized = query
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    let explicitly_old = |name: &str| {
        let name = name.to_lowercase();
        [
            "旧",
            "旧 ",
            "旧名",
            "旧名 ",
            "旧名称",
            "旧名称 ",
            "old ",
            "former ",
        ]
        .iter()
        .any(|prefix| {
            normalized
                .match_indices(&format!("{prefix}{name}"))
                .any(|(at, _)| {
                    !prefix.is_ascii()
                        || normalized[..at]
                            .chars()
                            .next_back()
                            .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_')
                })
        })
    };
    let historical: BTreeSet<_> = hints
        .iter()
        .filter(|hint| {
            hint.matching_aliases
                .iter()
                .any(|alias| explicitly_old(alias))
        })
        .map(|h| h.entity_id.clone())
        .collect();
    if historical.is_empty() && hints.iter().any(|hint| explicitly_old(&hint.name)) {
        return Ok(IdentitySelection::NotFound);
    }
    let explicit_code: BTreeSet<_> = hints
        .iter()
        .filter(|h| {
            matches!(
                h.kind,
                EntityKind::Path
                    | EntityKind::Function
                    | EntityKind::ConfigurationKey
                    | EntityKind::ErrorCode
                    | EntityKind::Version
            )
        })
        .map(|h| h.entity_id.clone())
        .collect();
    let candidates: BTreeSet<_> = if !historical.is_empty() {
        historical
    } else {
        hints.iter().map(|h| h.entity_id.clone()).collect()
    };
    let connected = |seed: &str| -> io::Result<BTreeSet<String>> {
        let mut found = BTreeSet::from([seed.to_owned()]);
        for relation in relations {
            if !matches!(
                relation.relation.as_str(),
                "owns" | "belongs_to" | "distinct_from"
            ) {
                return Err(io::Error::other("invalid structured entity relation"));
            }
            if relation.relation == "distinct_from" {
                continue;
            }
            if relation.source_entity_id == seed || relation.target_entity_id == seed {
                found.insert(relation.source_entity_id.clone());
                found.insert(relation.target_entity_id.clone());
                if found.len() > 64 {
                    return Err(io::Error::other("structured identity relation limit"));
                }
            }
        }
        Ok(found)
    };
    if let Some(code) = explicit_code.first() {
        // Only a shared direct owner disambiguates sibling identifiers. A
        // transitive component would incorrectly merge unrelated products
        // merely because they belong to the same organization.
        let owners = |id: &str| -> BTreeSet<String> {
            relations
                .iter()
                .filter_map(|relation| match relation.relation.as_str() {
                    "belongs_to" if relation.source_entity_id == id => {
                        Some(relation.target_entity_id.clone())
                    }
                    "owns" if relation.target_entity_id == id => {
                        Some(relation.source_entity_id.clone())
                    }
                    _ => None,
                })
                .collect()
        };
        connected(code)?;
        let mut common = owners(code);
        for other in explicit_code.iter().skip(1) {
            let next = owners(other);
            common.retain(|owner| next.contains(owner));
        }
        let named: BTreeSet<_> = candidates.difference(&explicit_code).cloned().collect();
        let selected: BTreeSet<_> = if named.is_empty() {
            common
        } else {
            common.intersection(&named).cloned().collect()
        };
        if selected.len() > 1
            || (selected.is_empty() && (!named.is_empty() || explicit_code.len() > 1))
        {
            return Ok(IdentitySelection::Ambiguous);
        }
        let mut anchors = explicit_code;
        anchors.extend(selected.intersection(&named).cloned());
        let mut related = anchors.clone();
        related.extend(selected);
        return Ok(IdentitySelection::Resolved {
            anchors: anchors.into_iter().collect(),
            related: related.into_iter().collect(),
        });
    }
    if candidates.len() != 1 {
        return Ok(IdentitySelection::Ambiguous);
    }
    let first = candidates.first().expect("nonempty hint set");
    let component = connected(first)?;
    Ok(IdentitySelection::Resolved {
        anchors: candidates.into_iter().collect(),
        related: component.into_iter().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn hint(id: &str, name: &str, aliases: &[&str], kind: EntityKind) -> StructuredEntityHint {
        StructuredEntityHint {
            entity_id: id.into(),
            kind,
            name: name.into(),
            subject_project: id.into(),
            matching_aliases: aliases.iter().map(|a| a.to_string()).collect(),
        }
    }
    fn relation(a: &str, b: &str, kind: &str) -> StructuredEntityRelation {
        StructuredEntityRelation {
            source_entity_id: a.into(),
            target_entity_id: b.into(),
            relation: kind.into(),
            source_id: "host-verified-source".into(),
        }
    }
    #[test]
    fn old_name_selects_renamed_entity_and_related_configuration_without_merging_namesakes() {
        let hints = vec![
            hint("A", "Aster", &["Lumen"], EntityKind::Product),
            hint("B", "Lumen", &[], EntityKind::Product),
        ];
        let relations = vec![
            relation("setting-A", "A", "belongs_to"),
            relation("A", "B", "distinct_from"),
        ];
        assert_eq!(
            select_identity("旧Lumenの本番再試行設定を反映して。", &hints, &relations).unwrap(),
            IdentitySelection::Resolved {
                anchors: vec!["A".into()],
                related: vec!["A".into(), "setting-A".into()]
            }
        );
        assert_eq!(
            select_identity("Lumenの設定を反映して。", &hints, &relations).unwrap(),
            IdentitySelection::Ambiguous
        );
        assert_eq!(
            select_identity("fold Lumen", &hints, &relations).unwrap(),
            IdentitySelection::Ambiguous
        );
        let shared_parent = vec![
            relation("A", "organization", "belongs_to"),
            relation("B", "organization", "belongs_to"),
        ];
        assert_eq!(
            select_identity("Lumen", &hints, &shared_parent).unwrap(),
            IdentitySelection::Ambiguous
        );
    }
    #[test]
    fn exact_configuration_anchor_resolves_only_its_explicit_product_relation() {
        let hints = vec![
            hint("A", "Aster", &["Lumen"], EntityKind::Product),
            hint("B", "Lumen", &[], EntityKind::Product),
            hint(
                "setting-A",
                "sync.retry_limit",
                &[],
                EntityKind::ConfigurationKey,
            ),
        ];
        let relations = vec![relation("setting-A", "A", "belongs_to")];
        assert_eq!(
            select_identity("Lumen sync.retry_limit", &hints, &relations).unwrap(),
            IdentitySelection::Resolved {
                anchors: vec!["A".into(), "setting-A".into()],
                related: vec!["A".into(), "setting-A".into()]
            }
        );
        assert_eq!(
            select_identity("Lumen", &hints[..2], &[]).unwrap(),
            IdentitySelection::Ambiguous
        );
        assert_eq!(
            select_identity("UnknownIdentifier", &[], &relations).unwrap(),
            IdentitySelection::NotFound
        );
    }

    #[test]
    fn sibling_identifiers_require_a_unique_direct_owner() {
        let hints = vec![
            hint("A", "Aster", &["Lumen"], EntityKind::Product),
            hint("B", "Lumen", &[], EntityKind::Product),
            hint(
                "key1",
                "sync.retry_limit",
                &[],
                EntityKind::ConfigurationKey,
            ),
            hint("key2", "sync.timeout", &[], EntityKind::ConfigurationKey),
        ];
        let relations = vec![
            relation("key1", "A", "belongs_to"),
            relation("A", "key2", "owns"),
        ];
        for query in [
            "Lumen sync.retry_limit sync.timeout",
            "旧Lumen sync.retry_limit sync.timeout",
        ] {
            assert_eq!(
                select_identity(query, &hints, &relations).unwrap(),
                IdentitySelection::Resolved {
                    anchors: vec!["A".into(), "key1".into(), "key2".into()],
                    related: vec!["A".into(), "key1".into(), "key2".into()],
                }
            );
        }
        let unrelated = vec![
            relation("key1", "A", "belongs_to"),
            relation("key2", "B", "belongs_to"),
            relation("A", "organization", "belongs_to"),
            relation("B", "organization", "belongs_to"),
        ];
        assert_eq!(
            select_identity("Lumen sync.retry_limit sync.timeout", &hints, &unrelated).unwrap(),
            IdentitySelection::Ambiguous
        );
        assert_eq!(
            select_identity("sync.retry_limit sync.timeout", &hints[2..], &relations).unwrap(),
            IdentitySelection::Resolved {
                anchors: vec!["key1".into(), "key2".into()],
                related: vec!["A".into(), "key1".into(), "key2".into()],
            }
        );
    }

    #[test]
    fn old_name_without_a_matching_alias_never_uses_the_current_namesake() {
        let current = vec![hint("B", "Lumen", &[], EntityKind::Product)];
        for query in ["旧Lumenの設定", "旧名 Lumen", "old Lumen", "former Lumen"] {
            assert_eq!(
                select_identity(query, &current, &[]).unwrap(),
                IdentitySelection::NotFound
            );
        }
        assert!(matches!(
            select_identity("Lumenの設定", &current, &[]).unwrap(),
            IdentitySelection::Resolved { .. }
        ));
    }

    #[test]
    fn sqlite_identity_pipeline_keeps_unknown_and_ambiguous_results_distinct() {
        use polaris_memory::structured::*;
        let mut memory = polaris_memory::MemoryStore::open(":memory:").unwrap();
        memory.enable_structured_memory().unwrap();
        let origin = OriginScope {
            project_id: "p".into(),
            session_id: "s".into(),
            epoch: 0,
        };
        let scope = PublicationScope {
            project_id: "p".into(),
            session_id: "s".into(),
            epoch: 0,
            generation: 1,
        };
        let source = "source-v3:p:s:0:1";
        let hash = "a".repeat(64);
        let mut delta = StructuredDelta {
            publication: StructuredPublication {
                scope: scope.clone(),
                publication_id: "publication".into(),
                raw_hash: hash.clone(),
                delta_hash: hash,
                prompt_version: "fixture".into(),
            },
            entities: vec![],
            entity_revisions: vec![],
            aliases: vec![],
            facts: vec![],
            dependencies: vec![],
            relations: vec![],
        };
        let mut ids = Vec::new();
        for (ordinal, (name, kind, project)) in [
            ("Aster", EntityKind::Product, "A"),
            ("Lumen", EntityKind::Product, "B"),
            ("sync.retry_limit", EntityKind::ConfigurationKey, "A"),
        ]
        .into_iter()
        .enumerate()
        {
            let id = stable_entity_id(&origin, source, ordinal as u32, kind).unwrap();
            delta.entities.push(StructuredEntity {
                entity_id: id.clone(),
                origin: origin.clone(),
                origin_source_id: source.into(),
                origin_ordinal: ordinal as u32,
                kind,
            });
            delta.entity_revisions.push(StructuredEntityRevision {
                entity_id: id.clone(),
                source_id: source.into(),
                canonical_name: name.into(),
                description: name.into(),
                subject_project: project.into(),
            });
            ids.push(id);
        }
        delta.aliases.push(StructuredAlias {
            entity_id: ids[0].clone(),
            display_alias: "Lumen".into(),
            match_alias: "lumen".into(),
            relation: "alias".into(),
            source_id: source.into(),
        });
        delta.relations.push(StructuredEntityRelation {
            source_entity_id: ids[2].clone(),
            target_entity_id: ids[0].clone(),
            relation: "belongs_to".into(),
            source_id: source.into(),
        });
        for (entity_id, value) in ids.iter().zip(["3", "99", "2"]) {
            delta.facts.push(StructuredFactRevision {
                fact_id: stable_fact_id(entity_id, "retry", "production", source).unwrap(),
                entity_id: entity_id.clone(),
                fact_kind: "retry".into(),
                value_json: value.into(),
                unit: "times".into(),
                condition: "production".into(),
                status: FactStatus::Active,
                reason: "assert".into(),
                successor_fact_id: None,
                source_id: source.into(),
            });
        }
        delta.publication.delta_hash = delta_hash(&delta).unwrap();
        let mut view = StructuredPublicationView {
            scope,
            visible_publication_ids: vec![],
            ancestors: vec![],
        };
        memory.insert_structured_delta(&delta, &view).unwrap();
        view.visible_publication_ids.push("publication".into());
        let old = retrieve_identity(&memory, &view, "旧Lumenの本番再試行設定を反映して。").unwrap();
        assert!(
            matches!(old.selection,IdentitySelection::Resolved{ref anchors,..} if anchors==&vec![ids[0].clone()])
        );
        assert_eq!(old.evidence.unwrap().entities.len(), 3);
        let ranked = super::super::retrieval::retrieve_state_evidence(
            &memory,
            &view,
            "旧Lumenの本番再試行設定を反映して。",
        )
        .unwrap()
        .evidence
        .unwrap();
        assert_eq!(ranked.entities.len(), 2);
        assert_eq!(ranked.facts.len(), 2);
        assert!(ranked.facts.iter().all(|f| f.row.value_json != "99"));
        for query in ["旧Lumen sync.retry_limit", "旧`Lumen` sync.retry_limit"] {
            let anchored =
                super::super::retrieval::retrieve_state_evidence(&memory, &view, query).unwrap();
            assert!(matches!(
                anchored.selection,
                IdentitySelection::Resolved { .. }
            ));
            assert_eq!(anchored.evidence.unwrap().facts.len(), 2);
        }
        assert!(
            ranked
                .entities
                .iter()
                .all(|e| e.row.revision.entity_id != ids[1])
        );
        let ambiguous =
            super::super::retrieval::retrieve_state_evidence(&memory, &view, "Lumenの設定")
                .unwrap();
        assert_eq!(ambiguous.selection, IdentitySelection::Ambiguous);
        assert!(ambiguous.evidence.is_none());
        assert_eq!(
            retrieve_identity(&memory, &view, "Lumenの設定")
                .unwrap()
                .selection,
            IdentitySelection::Ambiguous
        );
        let unknown = retrieve_identity(&memory, &view, "存在しない設定unlisted.value").unwrap();
        assert_eq!(unknown.selection, IdentitySelection::NotFound);
        assert!(unknown.evidence.is_none());
        assert!(retrieve_identity(&memory, &view, " \n ").is_err());
        let mut missing = view.clone();
        missing.visible_publication_ids.push("missing".into());
        assert!(retrieve_identity(&memory, &missing, "unlisted.value").is_err());
        memory.delete_session("p", "s").unwrap();
        assert!(retrieve_identity(&memory, &view, "unlisted.value").is_err());
    }
}
