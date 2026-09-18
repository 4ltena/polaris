//! Structured extraction and recovery through the existing raw owner.
use super::*;
use crate::conversation_memory::{structured, structured_owner};
use crate::conversation_state::HistoryMode;

impl StrictHistory {
    pub(super) async fn publish_structured_work(
        &self,
        store: &ConversationStore,
        database: &Path,
        work: crate::conversation_memory::StructuredSummaryWork,
    ) -> io::Result<ConversationSnapshot> {
        let response = match work.response {
            Some(response) => response,
            None => {
                if !work.newly_started {
                    return Err(io::Error::other("構造化抽出の再送は許可されていません"));
                }
                let view = structured::sources::publication_view(&work.source.state)?;
                let response = self
                    .summarize_structured_turn(
                        &work.source,
                        work.turn,
                        &work.intent,
                        database,
                        &view,
                    )
                    .await?;
                crate::conversation_memory::save_structured_summary_response(
                    store,
                    database,
                    &work.source,
                    work.turn,
                    &response,
                )?;
                response
            }
        };
        let evidence = summary_evidence(&work.source, work.turn, &response.summary)?;
        let saved = MemoryStore::open(database)
            .map_err(io::Error::other)?
            .pending_summary(&evidence.scope, &evidence.id)
            .map_err(io::Error::other)?;
        let pending = match saved {
            Some(pending) => {
                let metadata = self.embedder.metadata();
                if pending.embedding.model != metadata.model
                    || pending.embedding.revision != metadata.revision
                    || pending.embedding.dimension != metadata.dimension
                {
                    return Err(io::Error::other(
                        "保存済み埋め込みと現在の資源が一致しません",
                    ));
                }
                pending
            }
            None => {
                self.embed_summary(&work.source, work.turn, response.summary)
                    .await?
            }
        };
        publish_summary_snapshot(store, database, &work.source, &pending)
    }

    /// Never starts an extraction. Run before appending interrupted-tool results.
    pub(crate) async fn recover_saved_structured_observed(
        &self,
        store: &ConversationStore,
        database: &Path,
        published: &(dyn Fn(&ConversationSnapshot) -> io::Result<()> + Send + Sync),
    ) -> io::Result<StrictRecovery> {
        loop {
            let snapshot = store.lock()?.snapshot()?;
            if snapshot.state.history_mode != HistoryMode::Structured10 {
                return Err(io::Error::other("構造化記憶の復旧対象ではありません"));
            }
            let work = match unsummarized_expired_turn(&snapshot)? {
                Some(turn) => {
                    structured_owner::resume_structured_summary(store, database, &snapshot, turn)?
                }
                None => None,
            };
            let Some(work) = work else {
                return Ok(StrictRecovery {
                    published_generation: snapshot.state.generation,
                    published_raw_hash: snapshot.raw_hash.clone(),
                    snapshot,
                });
            };
            let committed = self.publish_structured_work(store, database, work).await?;
            published(&committed)?;
        }
    }

    pub(super) fn prepare_structured_context(
        &self,
        store: &ConversationStore,
        database: &Path,
        expected: &ConversationSnapshot,
        query: &str,
    ) -> io::Result<structured::packing::StructuredContext> {
        let lock = store.lock()?;
        lock.check_snapshot(expected)?;
        let mut memory = MemoryStore::open(database).map_err(io::Error::other)?;
        memory
            .enable_structured_memory()
            .map_err(io::Error::other)?;
        let context = structured_owner::with_verified_receipts(store, expected, &memory, |view| {
            let mut context =
                related_history::mandatory_context(store, expected, &memory, view, query)?;
            self.append_related_keywords(store, expected, &memory, query, &mut context)?;
            Ok(context)
        })?;
        Ok(context)
    }
}
