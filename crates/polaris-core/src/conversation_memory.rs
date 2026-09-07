//! Publication boundary between the raw session marker and conversation indexes.
//! Does not generate summaries, call models, or change the active context.

use crate::conversation_state::{ConversationSnapshot, ConversationStateV2, ConversationStore};
use polaris_memory::{
    MemoryStore,
    conversation::{AncestorRange, PendingSummary, PublishedView, Scope},
};
use std::{io, path::Path};

mod strict_history;
pub use strict_history::{
    EmbeddingModel, LocalStdioEmbedder, PreparedHistory, ResolvedConversationSource,
    StrictEmbedder, StrictHistory, StrictRecovery, StrictSummaryProvider, SummaryRequest,
    read_source, resolve_conversation_uri,
};

fn number(value: u64) -> io::Result<i64> {
    value
        .try_into()
        .map_err(|_| io::Error::other("保存世代・ターン番号がSQLite上限を超えました"))
}

pub fn published_view(state: &ConversationStateV2) -> io::Result<PublishedView> {
    state.validate()?;
    Ok(PublishedView {
        scope: Scope {
            project_id: state.project_id.clone(),
            session_id: state.session_id.clone(),
            epoch: number(state.epoch)?,
            generation: number(state.generation)?,
        },
        visible_ids: state.visible_summary_ids.clone(),
        ancestors: state
            .ancestors
            .iter()
            .map(|ancestor| {
                Ok(AncestorRange {
                    session_id: ancestor.session_id.clone(),
                    epoch: number(ancestor.epoch)?,
                    generation: number(ancestor.generation)?,
                    visible_ids: ancestor.visible_summary_ids.clone(),
                })
            })
            .collect::<io::Result<_>>()?,
    })
}

/// Publish one expired turn after the caller has generated/embedded it without
/// holding a lock. Conflicts fail without discarding raw history. An existing
/// identical pending row is reused after interruption; marker publication is
/// enclosed by the SQLite tombstone check, including against legacy forget.
pub fn publish_summary(
    store: &ConversationStore,
    database: &Path,
    expected: &ConversationSnapshot,
    pending: &PendingSummary,
) -> io::Result<()> {
    publish_summary_snapshot(store, database, expected, pending).map(|_| ())
}

fn publish_summary_snapshot(
    store: &ConversationStore,
    database: &Path,
    expected: &ConversationSnapshot,
    pending: &PendingSummary,
) -> io::Result<ConversationSnapshot> {
    let mut lock = store.lock()?;
    lock.check_snapshot(expected)?;
    let generation = expected
        .state
        .generation
        .checked_add(1)
        .ok_or_else(|| io::Error::other("保存世代が上限です"))?;
    let expected_scope = Scope {
        project_id: expected.state.project_id.clone(),
        session_id: expected.state.session_id.clone(),
        epoch: number(expected.state.epoch)?,
        generation: number(generation)?,
    };
    let turn: u64 = pending
        .source
        .start_turn
        .try_into()
        .map_err(|_| io::Error::other("原文ターン番号が不正です"))?;
    if pending.scope != expected_scope
        || pending.source.start_turn != pending.source.end_turn
        || !expected.expired_turns().contains(&turn)
        || pending.source.raw_hash != expected.raw_hash
        || expected.state.visible_summary_ids.contains(&pending.id)
    {
        return Err(io::Error::other(
            "要約の世代・原文範囲が処理対象と一致しません",
        ));
    }
    expected.validate_complete_turn(turn)?;
    lock.save_snapshot(expected)?;
    let mut memory = MemoryStore::open(database).map_err(io::Error::other)?;
    memory
        .insert_pending_summary(pending)
        .map_err(io::Error::other)?;
    let mut next = expected.state.clone();
    next.visible_summary_ids.push(pending.id.clone());
    let mut view = published_view(&next)?;
    view.scope.generation = number(generation)?;
    memory
        .publish_pending(&view, std::slice::from_ref(&pending.id), || {
            lock.publish(expected, next)
                .map_err(polaris_memory::Error::Io)
        })
        .map_err(io::Error::other)?;
    lock.snapshot()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation_state::content_hash;
    use polaris_memory::conversation::{
        ConversationQuery, EmbeddingMetadata, QueryEmbedding, SourceMetadata,
    };
    use polaris_provider::Message;
    const ID: &str = "00000000-0000-4000-8000-000000000001";
    fn setup(root: &Path) -> (ConversationStore, ConversationSnapshot, PendingSummary) {
        let store = ConversationStore::create(
            root,
            ConversationStateV2::new("p".into(), ID.into()).unwrap(),
        )
        .unwrap();
        let mut lock = store.lock().unwrap();
        for turn in 1..=11 {
            lock.append(Message::user(format!("fact {turn}")), true)
                .unwrap();
        }
        let snapshot = lock.snapshot().unwrap();
        drop(lock);
        let summary = r#"{"facts":["fact 1"],"decisions":[],"constraints":[],"corrections":[],"open_items":[],"source_turn_ids":[1]}"#.to_string();
        let item = PendingSummary {
            scope: Scope {
                project_id: "p".into(),
                session_id: ID.into(),
                epoch: 0,
                generation: 1,
            },
            id: "summary-1".into(),
            source: SourceMetadata {
                id: "source-1".into(),
                start_turn: 1,
                end_turn: 1,
                raw_hash: snapshot.raw_hash.clone(),
            },
            summary_hash: content_hash(summary.as_bytes()),
            embedding: EmbeddingMetadata {
                model: "fixture".into(),
                revision: "v1".into(),
                dimension: 2,
                input_hash: content_hash(summary.as_bytes()),
                values: vec![1.0, 0.0],
            },
            summary,
            model: "gpt-6-astra".into(),
            effort: "medium".into(),
            prompt_version: "v1".into(),
        };
        (store, snapshot, item)
    }
    #[test]
    fn indexed_marker_roundtrip_and_legacy_forget_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let (store, snapshot, item) = setup(dir.path());
        let database = dir.path().join("memory.sqlite3");
        publish_summary(&store, &database, &snapshot, &item).unwrap();
        let current = store.lock().unwrap().snapshot().unwrap();
        assert_eq!(current.state.generation, 1);
        assert_eq!(current.state.recent_turn_ids, (2..=11).collect::<Vec<_>>());
        assert_eq!(current.events.len(), 11);
        let view = published_view(&current.state).unwrap();
        let query = || ConversationQuery {
            published: &view,
            keywords: "fact",
            embedding: QueryEmbedding {
                model: "fixture".into(),
                revision: "v1".into(),
                dimension: 2,
                values: vec![1.0, 0.0],
            },
        };
        let mut memory = MemoryStore::open(&database).unwrap();
        assert_eq!(memory.search_conversation(query()).unwrap().len(), 1);
        memory.delete_session("p", ID).unwrap();
        assert!(memory.search_conversation(query()).is_err());
        assert!(
            memory
                .with_conversation_source(&view, "source-1", |_| Ok(()))
                .is_err()
        );
        // The legacy tombstone blocks a later pending generation even though raw
        // recovery remains on disk; no marker generation advances.
        let mut next = item.clone();
        next.id = "summary-again".into();
        next.source.id = "source-again".into();
        next.scope.generation = 2;
        assert!(publish_summary(&store, &database, &current, &next).is_err());
        assert_eq!(
            store.lock().unwrap().snapshot().unwrap().state.generation,
            1
        );
    }
    #[test]
    fn pending_retry_reuses_result_and_conflict_preserves_raw() {
        let dir = tempfile::tempdir().unwrap();
        let (store, snapshot, item) = setup(dir.path());
        let database = dir.path().join("memory.sqlite3");
        // Simulate interruption after index insertion but before marker rename.
        MemoryStore::open(&database)
            .unwrap()
            .insert_pending_summary(&item)
            .unwrap();
        assert!(
            store
                .lock()
                .unwrap()
                .snapshot()
                .unwrap()
                .state
                .visible_summary_ids
                .is_empty()
        );
        publish_summary(&store, &database, &snapshot, &item).unwrap();
        assert!(publish_summary(&store, &database, &snapshot, &item).is_err());
        let current = store.lock().unwrap().snapshot().unwrap();
        assert_eq!(current.state.generation, 1);
        assert_eq!(current.events.len(), 11);
        assert_eq!(current.state.visible_summary_ids, vec!["summary-1"]);
    }
    #[test]
    fn wrong_turn_or_scope_does_not_advance_marker() {
        let dir = tempfile::tempdir().unwrap();
        let (store, snapshot, mut item) = setup(dir.path());
        let database = dir.path().join("memory.sqlite3");
        item.source.start_turn = 11;
        item.source.end_turn = 11;
        assert!(publish_summary(&store, &database, &snapshot, &item).is_err());
        item.source.start_turn = 1;
        item.source.end_turn = 1;
        item.scope.session_id = "other".into();
        assert!(publish_summary(&store, &database, &snapshot, &item).is_err());
        assert_eq!(
            store.lock().unwrap().snapshot().unwrap().state.generation,
            0
        );
        assert!(!database.exists());
    }
}
