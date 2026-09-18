//! Structured publication through the CLI/TUI's existing v2 raw owner.

use super::*;
use crate::conversation_state::HistoryMode;
use polaris_memory::structured::StructuredDelta;
use structured::{StructuredIntent, ValidatedStructuredResponse};

#[derive(Debug)]
pub struct StructuredSummaryWork {
    pub source: ConversationSnapshot,
    pub turn: u64,
    pub intent: StructuredIntent,
    pub response: Option<ValidatedStructuredResponse>,
    /// An ephemeral result of this call's durable intent write; never a saved grant.
    pub newly_started: bool,
}

// Small fixture collector only. Production verifies and drops one receipt at a time.
#[cfg(test)]
fn visible_deltas(
    store: &ConversationStore,
    state: &ConversationStateV2,
) -> io::Result<Vec<StructuredDelta>> {
    structured::sources::publication_view(state)?;
    let mut deltas = Vec::new();
    for id in &state.visible_structured_publication_ids {
        deltas.push(store.structured_publication_receipt(state.epoch, state.generation, id)?);
    }
    for ancestor in &state.ancestors {
        if ancestor.visible_structured_publication_ids.is_empty() {
            continue;
        }
        let parent =
            ConversationStore::open(store.data_root()?, &state.project_id, &ancestor.session_id)?;
        for id in &ancestor.visible_structured_publication_ids {
            deltas.push(parent.structured_publication_receipt(
                ancestor.epoch,
                ancestor.generation,
                id,
            )?);
        }
    }
    Ok(deltas)
}

fn read_receipt_delta(
    store: &ConversationStore,
    owner: &ConversationStateV2,
    scope: &polaris_memory::structured::PublicationScope,
    id: &str,
) -> polaris_memory::Result<StructuredDelta> {
    let convert = |value| {
        u64::try_from(value)
            .map_err(|_| polaris_memory::Error::InvalidInput("構造化記憶の範囲が不正です"))
    };
    if scope.project_id != owner.project_id {
        return Err(polaris_memory::Error::InvalidInput(
            "構造化記憶のprojectが一致しません",
        ));
    }
    let epoch = convert(scope.epoch)?;
    let generation = convert(scope.generation)?;
    if scope.session_id == owner.session_id {
        store
            .structured_publication_receipt(epoch, generation, id)
            .map_err(polaris_memory::Error::Io)
    } else {
        // The DB verifier has checked this ancestor's tombstone before entry.
        ConversationStore::open(store.data_root()?, &scope.project_id, &scope.session_id)?
            .structured_publication_receipt(epoch, generation, id)
            .map_err(polaris_memory::Error::Io)
    }
}

pub fn begin_structured_summary(
    store: &ConversationStore,
    database: &Path,
    expected: &ConversationSnapshot,
    turn: u64,
) -> io::Result<StructuredSummaryWork> {
    structured_summary_work(store, database, expected, turn, true)?
        .ok_or_else(|| io::Error::other("構造化抽出の開始結果がありません"))
}

pub(super) fn resume_structured_summary(
    store: &ConversationStore,
    database: &Path,
    expected: &ConversationSnapshot,
    turn: u64,
) -> io::Result<Option<StructuredSummaryWork>> {
    structured_summary_work(store, database, expected, turn, false)
}

fn structured_summary_work(
    store: &ConversationStore,
    database: &Path,
    expected: &ConversationSnapshot,
    turn: u64,
    allow_new: bool,
) -> io::Result<Option<StructuredSummaryWork>> {
    let lock = store.lock()?;
    lock.check_snapshot(expected)?;
    number(
        expected
            .state
            .generation
            .checked_add(1)
            .ok_or_else(|| io::Error::other("保存世代が上限です"))?,
    )?;
    let structured_view = structured::sources::publication_view(&expected.state)?;
    let mut memory = MemoryStore::open(database).map_err(io::Error::other)?;
    memory
        .enable_structured_memory()
        .map_err(io::Error::other)?;
    let mut work = None;
    memory
        .with_conversation_marker(&published_view(&expected.state)?, || {
            // The outer writer transaction prevents forget/index changes while this
            // read connection checks receipts and the raw owner persists the intent.
            let verification = MemoryStore::open_read_only(database)?;
            verification.with_verified_structured_receipts(
                &structured_view,
                &mut |scope, id| read_receipt_delta(store, &expected.state, scope, id),
                || {
                    if !allow_new && store.bound_structured_attempt(expected, turn)?.is_none() {
                        return Ok(());
                    }
                    let (receipt, newly_started) = lock.begin_structured_attempt(expected, turn)?;
                    if !newly_started && receipt.response.is_none() {
                        return Err(polaris_memory::Error::InvalidInput(
                            "構造化抽出の結果が不明です。再送しません",
                        ));
                    }
                    work = Some(StructuredSummaryWork {
                        source: expected.clone(),
                        turn,
                        intent: receipt.intent,
                        response: receipt.response.map(|saved| ValidatedStructuredResponse {
                            summary: saved.summary,
                            delta: saved.delta,
                            coverage: saved.coverage,
                        }),
                        newly_started,
                    });
                    Ok(())
                },
            )
        })
        .map_err(io::Error::other)?;
    Ok(work)
}

/// Caller holds the raw owner lock. Nested reads share this exact index snapshot.
pub(super) fn with_verified_receipts<T>(
    store: &ConversationStore,
    expected: &ConversationSnapshot,
    memory: &MemoryStore,
    read: impl FnOnce(&polaris_memory::structured::StructuredPublicationView) -> io::Result<T>,
) -> io::Result<T> {
    let view = structured::sources::publication_view(&expected.state)?;
    memory
        .with_verified_structured_receipts(
            &view,
            &mut |scope, id| read_receipt_delta(store, &expected.state, scope, id),
            || read(&view).map_err(polaris_memory::Error::Io),
        )
        .map_err(io::Error::other)
}

/// Called inside the existing marker transaction, before raw or workflow writes.
pub(crate) fn reject_unpublished_structured_attempt(
    store: &ConversationStore,
    snapshot: &ConversationSnapshot,
) -> io::Result<()> {
    if snapshot.state.history_mode != HistoryMode::Structured10 {
        return Ok(());
    }
    let next = snapshot.expired_turns().into_iter().find(|turn| {
        !snapshot
            .state
            .visible_summary_ids
            .iter()
            .any(|id| id.starts_with(&format!("strict10-{turn}-")))
    });
    if let Some(turn) = next
        && store
            .read_structured_attempt(snapshot.state.epoch, turn)?
            .is_some()
    {
        return Err(io::Error::other(
            "未公開の構造化抽出を復旧してから会話を更新してください",
        ));
    }
    Ok(())
}

pub fn save_structured_summary_response(
    store: &ConversationStore,
    database: &Path,
    expected: &ConversationSnapshot,
    turn: u64,
    response: &ValidatedStructuredResponse,
) -> io::Result<()> {
    let lock = store.lock()?;
    lock.check_snapshot(expected)?;
    let mut memory = MemoryStore::open(database).map_err(io::Error::other)?;
    memory
        .with_conversation_marker(&published_view(&expected.state)?, || {
            lock.save_structured_attempt_response(expected, turn, response)
                .map_err(polaris_memory::Error::Io)
        })
        .map_err(io::Error::other)
}

pub(super) fn publish_structured_summary_snapshot(
    store: &ConversationStore,
    database: &Path,
    expected: &ConversationSnapshot,
    pending: &PendingSummary,
) -> io::Result<ConversationSnapshot> {
    let turn = u64::try_from(pending.source.start_turn)
        .map_err(|_| io::Error::other("構造化要約turnが不正です"))?;
    let mut lock = store.lock()?;
    lock.check_structured_turn(expected, turn)?;
    // Reopen the existing result, never create an intent during publication.
    let receipt = store
        .bound_structured_attempt(expected, turn)?
        .ok_or_else(|| io::Error::other("構造化抽出の保存記録がありません"))?;
    let response = receipt
        .response
        .ok_or_else(|| io::Error::other("構造化抽出の応答がありません"))?;
    let evidence = summary_evidence(expected, turn, &response.summary)?;
    if expected.state.history_mode != HistoryMode::Structured10
        || pending.scope != evidence.scope
        || pending.id != evidence.id
        || pending.source != evidence.source
        || pending.summary != response.summary
    {
        return Err(io::Error::other(
            "構造化要約の公開内容が保存応答と一致しません",
        ));
    }
    let mut memory = MemoryStore::open(database).map_err(io::Error::other)?;
    memory
        .enable_structured_memory()
        .map_err(io::Error::other)?;
    memory
        .insert_pending_summary(pending)
        .map_err(io::Error::other)?;
    let mut next = expected.state.clone();
    next.visible_summary_ids.push(pending.id.clone());
    next.visible_structured_publication_ids
        .push(response.delta.publication.publication_id.clone());
    next.structured_coverage = Some(response.coverage);
    let mut view = published_view(&next)?;
    view.scope.generation = pending.scope.generation;
    memory
        .publish_pending_with_structured_reader(
            &view,
            std::slice::from_ref(&pending.id),
            &response.delta,
            &structured::sources::publication_view(&expected.state)?,
            &mut |scope, id| read_receipt_delta(store, &expected.state, scope, id),
            || {
                lock.publish(expected, next)
                    .map_err(polaris_memory::Error::Io)
            },
        )
        .map_err(io::Error::other)?;
    lock.snapshot()
}

#[cfg(test)]
mod tests;
