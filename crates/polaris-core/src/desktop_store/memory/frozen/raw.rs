//! Canonical raw JSONL is framed by newline; retain only a requested event/turn.
use super::*;
use std::io::BufRead;

fn walk(
    files: &Files<'_>,
    offset: u64,
    expected_hash: &str,
    mut accept: impl FnMut(&RawEventV2, u64, &Sha256, &[u8]) -> StoreResult<()>,
) -> StoreResult<()> {
    if offset > files.marker.raw_offset.get() || !hash(expected_hash) {
        return Err(StoreError::Corrupt("Pinned source range"));
    }
    files.work.charge(offset)?;
    let file = files.raw()?;
    if file.metadata()?.len() < offset {
        return Err(StoreError::Corrupt("Insufficient pinned source text"));
    }
    let mut reader = BufReader::with_capacity(bounded::BUFFER, file.take(offset));
    let mut digest = Sha256::new();
    let (mut bytes, mut sequence, mut turn) = (0u64, 0u64, 0u64);
    loop {
        let mut line = Vec::new();
        let count = (&mut reader)
            .take(bounded::RAW_RECORD as u64 + 1)
            .read_until(b'\n', &mut line)?;
        if count == 0 {
            break;
        }
        if count > bounded::RAW_RECORD || !line.ends_with(b"\n") {
            return Err(StoreError::Corrupt("Pinned source line limit or newline"));
        }
        let event: RawEventV2 = serde_json::from_slice(&line[..line.len() - 1])?;
        let canonical = serde_json::to_vec(&event)?;
        if canonical != line[..line.len() - 1]
            || event.schema_version != 2
            || event.sequence != sequence.checked_add(1).ok_or(StoreError::Overflow)?
            || event.epoch != files.marker.epoch.get()
            || event.turn_id == 0
            || (event.starts_turn && event.message.role != Role::User)
            || (sequence == 0 && !event.starts_turn)
            || (sequence > 0
                && if event.starts_turn {
                    turn.checked_add(1) != Some(event.turn_id)
                } else {
                    turn != event.turn_id
                })
        {
            return Err(StoreError::Corrupt("Pinned source order or turn boundary"));
        }
        bytes = bytes
            .checked_add(count as u64)
            .ok_or(StoreError::Overflow)?;
        digest.update(&line);
        accept(&event, bytes, &digest, &canonical)?;
        sequence = event.sequence;
        turn = event.turn_id;
    }
    if bytes != offset || format!("{:x}", digest.finalize()) != expected_hash {
        return Err(StoreError::Corrupt("Pinned source prefix hash"));
    }
    files.owner.verify()?;
    disk::same(
        &files.raw,
        &files.owner.directory.open(disk::RAW, false, false)?,
    )?;
    Ok(())
}

pub(super) fn validate(files: &Files<'_>, c: &Collector<'_>) -> StoreResult<BTreeSet<u64>> {
    let mut offsets = BTreeMap::new();
    let mut coverage = BTreeMap::new();
    let mut turns = BTreeMap::new();
    let mut used = c.used;
    for attempt in c.attempts.values() {
        let s = &attempt.source;
        if let Some(prior) = offsets.insert(s.raw_offset.get(), s.raw_hash.as_str()) {
            if prior != s.raw_hash {
                return Err(StoreError::Corrupt("Pinned source boundary hash conflict"));
            }
        } else {
            charge(&mut used, &(s.raw_offset, &s.raw_hash))?;
        }
        turns.insert(s.turn.get(), (&s.turn_hash, s.raw_offset.get()));
        charge(&mut used, &(s.turn, &s.turn_hash, s.raw_offset))?;
        if let Some(boundary) = &attempt.coverage {
            if let Some(prior) = coverage.insert(boundary.last_source_sequence, boundary) {
                if prior != boundary {
                    return Err(StoreError::Corrupt("Pinned source coverage conflict"));
                }
            } else {
                charge(&mut used, boundary)?;
            }
        }
    }
    let (mut seen_offsets, mut seen_coverage, mut seen_bindings, mut seen_turns) =
        (0usize, 0usize, 0usize, 0usize);
    let mut prompts = prompt::Raw::new(c, used);
    let (mut turn, mut turn_end) = (0u64, 0u64);
    let mut turn_digest = Sha256::new();
    let finish_turn =
        |turn: u64, end: u64, digest: &Sha256, count: &mut usize| -> StoreResult<()> {
            if let Some((expected, before)) = turns.get(&turn) {
                let mut done = digest.clone();
                done.update(b"]");
                if end > *before || format!("{:x}", done.finalize()) != **expected {
                    return Err(StoreError::Corrupt("Pinned source turn hash"));
                }
                *count += 1;
            }
            Ok(())
        };
    walk(
        files,
        files.marker.raw_offset.get(),
        &files.marker.raw_hash,
        |event, offset, digest, canonical| {
            prompts.event(event)?;
            if event.starts_turn {
                if turn > 0 {
                    finish_turn(turn, turn_end, &turn_digest, &mut seen_turns)?;
                }
                turn_digest = Sha256::new();
                turn_digest.update(b"[");
                turn = event.turn_id;
            } else {
                turn_digest.update(b",");
            }
            turn_digest.update(canonical);
            turn_end = offset;
            if let Some(expected) = offsets.get(&offset) {
                if format!("{:x}", digest.clone().finalize()) != *expected {
                    return Err(StoreError::Corrupt("Pinned source prefix"));
                }
                seen_offsets += 1;
            }
            if let Some(expected) = coverage.get(&event.sequence) {
                if expected.raw_offset != offset
                    || expected.prefix_hash != format!("{:x}", digest.clone().finalize())
                {
                    return Err(StoreError::Corrupt("Pinned source coverage boundary"));
                }
                seen_coverage += 1;
            }
            if let Some(expected) = c.bindings.get(&event.sequence) {
                if event.message.role != expected.role
                    || content_hash(event.message.content.as_bytes()) != expected.content_hash
                {
                    return Err(StoreError::Corrupt("Pinned source content"));
                }
                if expected.requires_user_input
                    && (!event.starts_turn || event.message.role != Role::User)
                {
                    return Err(StoreError::Corrupt("Pinned source user input evidence"));
                }
                seen_bindings += 1;
            }
            Ok(())
        },
    )?;
    let attachments = prompts.finish()?;
    if turn > 0 {
        finish_turn(turn, turn_end, &turn_digest, &mut seen_turns)?;
    }
    if seen_offsets != offsets.len()
        || seen_coverage != coverage.len()
        || seen_bindings != c.bindings.len()
        || seen_turns != turns.len()
    {
        return Err(StoreError::Corrupt(
            "Missing pinned source citation boundary",
        ));
    }
    Ok(attachments)
}

pub(super) fn history_parts(
    files: &Files<'_>,
    inherited: u64,
    tail_start: u64,
    retention_limit: usize,
) -> StoreResult<Vec<RawEventV2>> {
    let mut events = Vec::new();
    let mut bytes = 2usize;
    walk(
        files,
        files.marker.raw_offset.get(),
        &files.marker.raw_hash,
        |event, _, _, encoded| {
            if event.turn_id <= inherited || event.turn_id >= tail_start {
                bytes = bytes
                    .checked_add(encoded.len() + usize::from(!events.is_empty()))
                    .filter(|n| *n <= retention_limit)
                    .ok_or(StoreError::Overflow)?;
                events.push(event.clone());
            }
            Ok(())
        },
    )?;
    Ok(events)
}

pub(super) fn event(files: &Files<'_>, source: &SourceStamp, id: &str) -> StoreResult<RawEventV2> {
    let prefix = format!(
        "source-v3:{}:{}:{}:",
        files.marker.project_id.as_str(),
        files.marker.session_id.as_str(),
        files.marker.epoch.get()
    );
    let sequence = id
        .strip_prefix(&prefix)
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|n| *n > 0 && id == format!("{prefix}{n}"))
        .ok_or(StoreError::NotFound)?;
    let mut found = None;
    walk(
        files,
        source.raw_offset.get(),
        &source.raw_hash,
        |event, _, _, _| {
            if event.sequence == sequence {
                found = Some(event.clone());
            }
            Ok(())
        },
    )?;
    found.ok_or(StoreError::NotFound)
}

pub(super) fn turn(files: &Files<'_>, source: &SourceStamp) -> StoreResult<Vec<RawEventV2>> {
    let mut events = Vec::new();
    let mut bytes = 2usize;
    walk(
        files,
        source.raw_offset.get(),
        &source.raw_hash,
        |event, _, _, encoded| {
            if event.turn_id == source.turn.get() {
                bytes = bytes
                    .checked_add(encoded.len() + 1)
                    .filter(|n| *n <= bounded::RAW_RECORD)
                    .ok_or(StoreError::Overflow)?;
                events.push(event.clone());
            }
            Ok(())
        },
    )?;
    if events.is_empty() || content_hash(&serde_json::to_vec(&events)?) != source.turn_hash {
        return Err(StoreError::Corrupt("Pinned source selected turn"));
    }
    Ok(events)
}
