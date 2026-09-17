//! Host-owned fork seed and immutable, exact ancestor publication grants.
use super::*;
use polaris_memory::structured::{
    PublicationScope, StructuredAncestorPublicationRange, StructuredPublicationView,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FrozenAncestor {
    pub marker: Marker,
    pub generation: DecimalU64,
    pub summary_ids: Vec<String>,
    pub structured_ids: Vec<String>,
}

pub(super) fn validate(p: &Published) -> StoreResult<()> {
    let Some(m) = &p.state.conversation_memory else {
        return if p.state.inherited_turn_count.get() == 0 {
            Ok(())
        } else {
            Err(StoreError::Corrupt("継承原文の記憶owner不在"))
        };
    };
    if m.ancestors.len() > 1_024
        || (p.state.inherited_turn_count.get() > 0 && m.ancestors.is_empty())
    {
        return Err(StoreError::Corrupt("祖先記憶の範囲"));
    }
    let mut seen = BTreeSet::new();
    for ancestor in &m.ancestors {
        let marker = &ancestor.marker;
        if marker.schema_version != 3
            || marker.deleted
            || marker.project_id != p.marker.project_id
            || marker.session_id == p.marker.session_id
            || marker.epoch.get() == 0
            || marker.content_revision > marker.session_revision
            || marker.sidecar_offset.get() == 0
            || !hash(&marker.raw_hash)
            || !hash(&marker.sidecar_hash)
            || !seen.insert((&marker.session_id, marker.epoch))
            || ancestor.generation.get() != ancestor.summary_ids.len() as u64
            || ancestor.structured_ids.len() > ancestor.summary_ids.len()
            || !valid_ids(&ancestor.summary_ids, "strict10-")
            || !valid_ids(&ancestor.structured_ids, "structured10-")
        {
            return Err(StoreError::Corrupt("固定祖先の識別子・公開範囲"));
        }
    }
    capacity(p, None, None)
}

/// Reserve both exact IDs before persisting a new model request intent. Replays
/// reuse their existing reservation and therefore do not call this function.
pub(super) fn reserve(p: &Published, turn: u64, raw_hash: &str) -> StoreResult<()> {
    if !hash(raw_hash) || turn == 0 {
        return Err(StoreError::Corrupt("記憶の新規許可範囲"));
    }
    let summary = format!("strict10-{turn}-{}", &raw_hash[..16]);
    let structured = (p.state.configuration.history_mode == HistoryMode::Structured10)
        .then(|| format!("structured10-{turn}-{}", &raw_hash[..16]));
    capacity(p, Some(&summary), structured.as_deref())
}

fn capacity(
    p: &Published,
    next_summary: Option<&str>,
    next_structured: Option<&str>,
) -> StoreResult<()> {
    let m = memory(p)?;
    // v2 and v3 each retain their existing capacity; combining them would halve it.
    let mut count = m.sources.len();
    let mut bytes = m.sources.iter().map(|s| s.summary_id.len()).sum::<usize>();
    for ancestor in &m.ancestors {
        count = count.saturating_add(ancestor.summary_ids.len());
        for id in &ancestor.summary_ids {
            bytes = bytes.saturating_add(id.len());
        }
    }
    if let Some(id) = next_summary {
        if m.sources.iter().any(|source| source.summary_id == id) {
            return Err(StoreError::RunConflict);
        }
        count = count.saturating_add(1);
        bytes = bytes.saturating_add(id.len());
    }
    if count > 65_536 || bytes > 4 * 1024 * 1024 {
        return Err(StoreError::Overflow);
    }
    StructuredPublicationView {
        scope: PublicationScope {
            project_id: p.marker.project_id.as_str().into(),
            session_id: p.marker.session_id.as_str().into(),
            epoch: number(m.epoch.get())?,
            generation: number(m.generation.get())?,
        },
        visible_publication_ids: m
            .sources
            .iter()
            .filter_map(|s| s.structured_publication_id.clone())
            .collect(),
        ancestors: m
            .ancestors
            .iter()
            .map(|a| {
                Ok(StructuredAncestorPublicationRange {
                    session_id: a.marker.session_id.as_str().into(),
                    epoch: number(a.marker.epoch.get())?,
                    generation: number(a.generation.get())?,
                    visible_publication_ids: a.structured_ids.clone(),
                })
            })
            .collect::<StoreResult<_>>()?,
    }
    .check_receipt_capacity(next_structured)
    .map_err(std::io::Error::other)?;
    Ok(())
}

fn valid_ids(ids: &[String], prefix: &str) -> bool {
    let mut seen = BTreeSet::new();
    ids.iter().all(|id| {
        seen.insert(id)
            && id.strip_prefix(prefix).is_some_and(|rest| {
                rest.split_once('-').is_some_and(|(turn, digest)| {
                    turn.parse::<u64>()
                        .is_ok_and(|n| n > 0 && n.to_string() == turn)
                        && digest.len() == 16
                        && digest.bytes().all(|b| b.is_ascii_hexdigit())
                })
            })
    })
}

pub(in crate::desktop_store) fn validate_transition(
    before: &Sidecar,
    after: &Sidecar,
) -> StoreResult<()> {
    fn ancestors(state: &Sidecar) -> &[FrozenAncestor] {
        state
            .conversation_memory
            .as_ref()
            .map(|m| m.ancestors.as_slice())
            .unwrap_or_default()
    }
    if before.inherited_turn_count != after.inherited_turn_count
        || ancestors(before) != ancestors(after)
    {
        return Err(StoreError::Corrupt("継承原文・固定祖先の変更"));
    }
    Ok(())
}

pub(in crate::desktop_store) fn fork_seed(
    parent: &Published,
    session: &SessionId,
    artifact_hash: &str,
) -> StoreResult<(Sidecar, Vec<RawEventV2>)> {
    let m = memory(parent)?;
    if parent.marker.deleted {
        return Err(StoreError::Deleted);
    }
    if session == &parent.marker.session_id
        || m.ancestors.iter().any(|a| &a.marker.session_id == session)
    {
        return Err(StoreError::TargetMismatch);
    }
    if !matches!(
        parent.state.configuration.history_mode,
        HistoryMode::Strict10 | HistoryMode::Structured10
    ) || parent.state.runs.iter().any(|r| !r.run.state.is_terminal())
    {
        return Err(StoreError::RunConflict);
    }
    let snapshot = project(
        parent,
        parent.raw.clone(),
        parent.marker.raw_offset,
        parent.marker.raw_hash.clone(),
    )?;
    let turns = snapshot.active_turns();
    let kept = &turns[turns
        .len()
        .saturating_sub(crate::conversation_state::REAL_TURN_WINDOW)..];
    for turn in kept {
        snapshot.validate_complete_turn(*turn)?;
    }
    let mut raw = Vec::new();
    let mut turn_id = 0;
    for event in &parent.raw {
        if kept.first().is_some_and(|first| event.turn_id >= *first) {
            if event.starts_turn {
                turn_id += 1;
            }
            raw.push(RawEventV2 {
                schema_version: 2,
                sequence: raw.len() as u64 + 1,
                epoch: 1,
                turn_id,
                starts_turn: event.starts_turn,
                message: event.message.clone(),
            });
        }
    }
    let mut ancestors = m.ancestors.clone();
    ancestors.push(FrozenAncestor {
        marker: parent.marker.clone(),
        generation: m.generation,
        summary_ids: m.sources.iter().map(|s| s.summary_id.clone()).collect(),
        structured_ids: m
            .sources
            .iter()
            .filter_map(|s| s.structured_publication_id.clone())
            .collect(),
    });
    let workflow = parent
        .state
        .workflow
        .as_ref()
        .map(|saved| {
            let mut child = saved.clone();
            child.gates = saved.gates.for_fork(artifact_hash);
            child.validate()?;
            Ok::<_, StoreError>(child)
        })
        .transpose()?;
    Ok((
        Sidecar {
            inherited_turn_count: DecimalU64::new(turn_id),
            conversation_memory: Some(SavedMemory {
                schema_version: 1,
                epoch: DecimalU64::new(1),
                resources: m.resources.clone(),
                generation: DecimalU64::new(0),
                sources: vec![],
                attempts: vec![],
                ancestors,
            }),
            session_revision: DecimalU64::new(0),
            draft: polaris_desktop_protocol::snapshot::Draft {
                draft_revision: DecimalU64::new(0),
                text: String::new(),
                attachment_ids: vec![],
            },
            configuration: parent.state.configuration.clone(),
            role_bindings: parent.state.role_bindings.clone(),
            tier_bindings: parent.state.tier_bindings.clone(),
            // A fork starts without the parent's goal or its pending
            // instructions: neither is carried into a new session.
            goal: None,
            instructions: vec![],
            policy_revision: parent.state.policy_revision,
            tasks: vec![],
            runs: vec![],
            requests: vec![],
            unresolved_approvals: vec![],
            approval_records: vec![],
            source_applies: vec![],
            children: vec![],
            workflow,
        },
        raw,
    ))
}
