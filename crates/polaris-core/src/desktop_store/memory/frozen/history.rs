//! Prove inherited copies against fixed parent publications, without a database.
use super::*;

#[derive(Serialize)]
pub(super) struct Proof {
    pub(super) run_count: u64,
    pub(super) inherited: u64,
    pub(super) all_terminal: bool,
    pub(super) ancestors: Vec<fork::FrozenAncestor>,
    pub(super) attachments: BTreeSet<u64>,
}

struct Frame<'a> {
    owner: FrozenMemory<'a>,
    proof: Proof,
    parts: Vec<RawEventV2>,
    tail_start: u64,
}

fn compare_copy<'a>(
    mut child: impl Iterator<Item = &'a RawEventV2>,
    epoch: u64,
    parent: impl Iterator<Item = &'a RawEventV2>,
    tail_start: u64,
) -> StoreResult<()> {
    for (index, source) in parent.enumerate() {
        let event = child
            .next()
            .ok_or(StoreError::Corrupt("Insufficient inherited source text"))?;
        if event.schema_version != 2
            || event.epoch != epoch
            || event.sequence != index as u64 + 1
            || event.turn_id != source.turn_id - tail_start + 1
            || event.starts_turn != source.starts_turn
            || serde_json::to_vec(&event.message)? != serde_json::to_vec(&source.message)?
        {
            return Err(StoreError::Corrupt(
                "Inherited source text does not match the pinned parent",
            ));
        }
    }
    if child.next().is_some() {
        return Err(StoreError::Corrupt("Excess inherited source text"));
    }
    Ok(())
}

pub(in crate::desktop_store) fn with_inherited_prompt_attachments<T>(
    writer: &Writer,
    published: &Published,
    read: impl FnOnce(BTreeSet<u64>) -> StoreResult<T>,
) -> StoreResult<T> {
    fork::validate(published)?;
    let Some(memory) = &published.state.conversation_memory else {
        return read(BTreeSet::new());
    };
    let mut ancestors = memory.ancestors.as_slice();
    if ancestors.is_empty() {
        return read(BTreeSet::new());
    }
    let work = bounded::ReadWork::default();
    let mut frames: Vec<Frame<'_>> = Vec::new();
    let mut retained = 0;
    loop {
        let (grant, remaining) = ancestors.split_last().ok_or(StoreError::Corrupt(
            "No pinned parent for inherited source text",
        ))?;
        let (owner, proof) =
            FrozenMemory::load_with_history(writer, work.clone(), grant, &mut |_| Ok(()))?;
        if !proof.all_terminal || proof.ancestors != remaining {
            return Err(StoreError::Corrupt(
                "Inherited source pinned-parent run or ancestor association",
            ));
        }
        let total = proof
            .inherited
            .checked_add(proof.run_count)
            .ok_or(StoreError::Overflow)?;
        let kept = total.min(crate::conversation_state::REAL_TURN_WINDOW as u64);
        let tail_start = total
            .checked_sub(kept)
            .and_then(|n| n.checked_add(1))
            .ok_or(StoreError::Overflow)?;
        let child_count = frames
            .last()
            .map_or(published.state.inherited_turn_count.get(), |f| {
                f.proof.inherited
            });
        if child_count != kept {
            return Err(StoreError::Corrupt("Inherited source turn count mismatch"));
        }
        let needs_parent = proof.inherited > 0 && tail_start <= proof.inherited;
        charge(
            &mut retained,
            &(&owner.scope, &owner.resources, &owner.summaries, &proof),
        )?;
        let parts = raw::history_parts(
            &owner.files,
            if needs_parent { proof.inherited } else { 0 },
            tail_start,
            bounded::RECORD.saturating_sub(retained.saturating_add(2)),
        )?;
        charge(&mut retained, &parts)?;
        for turn in tail_start..=total {
            crate::conversation_state::validate_complete_turn_events(
                parts.iter().filter(|event| event.turn_id == turn),
            )?;
        }
        let tail = || parts.iter().filter(|event| event.turn_id >= tail_start);
        if let Some(child) = frames.last() {
            compare_copy(
                child
                    .parts
                    .iter()
                    .filter(|event| event.turn_id <= child.proof.inherited),
                child.owner.files.marker.epoch.get(),
                tail(),
                tail_start,
            )?;
        } else {
            compare_copy(
                published
                    .raw
                    .iter()
                    .filter(|event| event.turn_id <= child_count),
                published.marker.epoch.get(),
                tail(),
                tail_start,
            )?;
        }
        frames.push(Frame {
            owner,
            proof,
            parts,
            tail_start,
        });
        if !needs_parent {
            break;
        }
        ancestors = remaining;
    }
    let mut inherited = BTreeSet::new();
    for frame in frames.iter().rev() {
        let next = frame
            .parts
            .iter()
            .filter(|event| event.turn_id >= frame.tail_start)
            .enumerate()
            .filter_map(|(index, event)| {
                (frame.proof.attachments.contains(&event.sequence)
                    || inherited.contains(&event.sequence))
                .then_some(index as u64 + 1)
            })
            .collect();
        inherited = next;
        frame.owner.files.owner.verify()?;
    }
    let result = read(inherited)?;
    for frame in &frames {
        frame.owner.files.owner.verify()?;
    }
    Ok(result)
}
