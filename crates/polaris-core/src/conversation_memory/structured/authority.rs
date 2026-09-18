//! Decision authority comes from verified raw user-input boundaries, not role alone.
use super::*;

pub(super) fn user_inputs(snapshot: &ConversationSnapshot) -> BTreeSet<u64> {
    snapshot
        .events
        .iter()
        .filter(|event| {
            event.epoch == snapshot.state.epoch
                && event.starts_turn
                && event.message.role == Role::User
        })
        .map(|event| event.sequence)
        .collect()
}

/// Requirements for every cited source. Frozen readers retain these booleans
/// beside their existing bounded raw bindings rather than retaining the delta.
/// Call after validate_delta; the source owner must still verify actual origins.
pub(crate) fn source_requirements<'a>(
    delta: &'a StructuredDelta,
) -> io::Result<BTreeMap<&'a str, bool>> {
    let mut requirements = BTreeMap::new();
    let mut require = |id: &'a str, user: bool| {
        requirements
            .entry(id)
            .and_modify(|prior| *prior |= user)
            .or_insert(user);
    };
    for entity in &delta.entities {
        require(&entity.origin_source_id, false);
    }
    for revision in &delta.entity_revisions {
        let first_observation = delta.entities.iter().any(|entity| {
            entity.entity_id == revision.entity_id && entity.origin_source_id == revision.source_id
        });
        require(&revision.source_id, !first_observation);
    }
    for alias in &delta.aliases {
        require(&alias.source_id, true);
    }
    for fact in &delta.facts {
        // An untrusted proposal cannot overwrite a pre-existing decision ID.
        let observation = fact.status == FactStatus::Proposed
            && fact.successor_fact_id.is_none()
            && matches!(fact.reason.as_str(), "proposed" | "unconfirmed")
            && fact.fact_id
                == stable_fact_id(
                    &fact.entity_id,
                    &fact.fact_kind,
                    &fact.condition,
                    &fact.source_id,
                )
                .map_err(io::Error::other)?;
        require(&fact.source_id, !observation);
    }
    for dependency in &delta.dependencies {
        require(&dependency.source_id, true);
    }
    for relation in &delta.relations {
        require(&relation.source_id, true);
    }
    Ok(requirements)
}

pub(super) fn validate_sources(
    snapshot: &ConversationSnapshot,
    intent: &StructuredIntent,
    delta: &StructuredDelta,
) -> io::Result<()> {
    let sources: BTreeMap<_, _> = intent
        .sources
        .iter()
        .map(|source| (source.source_id.as_str(), source))
        .collect();
    let inputs = user_inputs(snapshot);
    for (id, requires_user) in source_requirements(delta)? {
        let source = sources
            .get(id)
            .ok_or_else(|| io::Error::other("保存済み構造化応答の出典が抽出範囲外です"))?;
        if requires_user && !inputs.contains(&source.sequence) {
            return Err(io::Error::other(
                "構造化状態の変更に利用者入力の原文根拠がありません",
            ));
        }
    }
    Ok(())
}
