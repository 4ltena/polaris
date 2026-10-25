//! Related evidence through the same frozen raw owner and receipt transaction.
use super::*;
use crate::conversation_memory::{related, structured, structured_owner};
use crate::conversation_state::HistoryMode;
use polaris_memory::conversation::{RelatedSummaryCandidates, SourceReference};

impl StrictHistory {
    pub(crate) async fn search_related(
        &self,
        store: &ConversationStore,
        database: &Path,
    ) -> io::Result<String> {
        let expected = {
            let lock = store.lock()?;
            let snapshot = lock.snapshot()?;
            let query = accepted_query(&snapshot)?;
            // No raw proof, candidate ranking or embedding before this bounded
            // index preflight. It grants no authority to the subsequent result.
            MemoryStore::open_read_only(database)
                .map_err(io::Error::other)?
                .preflight_related_summary_scan(&published_view(&snapshot.state)?, query)
                .map_err(io::Error::other)?;
            snapshot
        };
        let query = accepted_query(&expected)?;
        let embedded = self.embed_related_query(query).await?;
        let lock = store.lock()?;
        lock.check_snapshot(&expected)?;
        let current = lock.snapshot()?;
        if accepted_query(&current)? != query {
            return Err(io::Error::other(
                "Admitted input changed during related search",
            ));
        }
        let parts = embedded.checked_parts(accepted_query(&current)?)?;
        let memory = MemoryStore::open_read_only(database).map_err(io::Error::other)?;
        // The receipt window covers reading the store. Ordering and rendering
        // read nothing from it, so both happen after it closes: holding a
        // verified snapshot open across a judge's network round trip would
        // buy nothing and risk the re-check that closes the window.
        let (status, candidates) =
            structured_owner::with_verified_receipts(store, &expected, &memory, |view| {
                let context = mandatory_context(store, &expected, &memory, view, query)?;
                super::super::observation::prepare_structured(&context)?.require_dispatch()?;
                let candidates = self
                    .verified_related_candidates(store, &expected, &memory, query, Some(parts))
                    .map_err(io::Error::other)?;
                Ok((context.status, candidates))
            })?;
        let candidates = related::judged(self.judge.as_deref(), query, candidates).await;
        related::render_candidates(status, candidates)
    }

    pub(super) fn append_related_keywords(
        &self,
        store: &ConversationStore,
        expected: &ConversationSnapshot,
        memory: &MemoryStore,
        query: &str,
        context: &mut structured::packing::StructuredContext,
    ) -> io::Result<()> {
        if !context.accepts_related() {
            return Ok(());
        }
        let result = self.verified_related_candidates(store, expected, memory, query, None);
        match result {
            Ok(candidates) => {
                for candidate in candidates.keyword {
                    context.append_keyword_summary(&candidate.pending)?;
                }
            }
            Err(polaris_memory::Error::RelatedSummaryBudgetExceeded(budget)) => {
                context.related_omission = Some(budget);
            }
            Err(error) => return Err(io::Error::other(error)),
        }
        Ok(())
    }

    fn verified_related_candidates(
        &self,
        store: &ConversationStore,
        expected: &ConversationSnapshot,
        memory: &MemoryStore,
        query: &str,
        embeddings: Option<&[QueryEmbedding]>,
    ) -> polaris_memory::Result<RelatedSummaryCandidates> {
        let view = published_view(&expected.state)?;
        let verify = |pending: &PendingSummary| {
            self.verify_related_summary(store, expected, pending)
                .map_err(polaris_memory::Error::Io)
        };
        match embeddings {
            Some(parts) => memory.related_summary_chunks_checked(&view, query, parts, verify),
            None => Ok(RelatedSummaryCandidates {
                keyword: memory.keyword_summary_candidates_checked(&view, query, verify)?,
                semantic: vec![],
            }),
        }
    }

    fn verify_related_summary(
        &self,
        store: &ConversationStore,
        expected: &ConversationSnapshot,
        pending: &PendingSummary,
    ) -> io::Result<()> {
        let invalid = || {
            io::Error::other(
                "The saved related summary response, source text, and resources do not match",
            )
        };
        let metadata = self.embedding_metadata();
        if pending.scope.project_id != expected.state.project_id
            || pending.model != SUMMARY_MODEL
            || pending.effort != SUMMARY_EFFORT
            || pending.prompt_version != SUMMARY_PROMPT_VERSION
            || pending.embedding.model != metadata.model
            || pending.embedding.revision != metadata.revision
            || pending.embedding.dimension != metadata.dimension
            || pending.embedding.input_hash != pending.summary_hash
        {
            return Err(invalid());
        }
        let parent;
        let owner = if pending.scope.session_id == expected.state.session_id {
            store
        } else {
            parent = ConversationStore::open(
                store.data_root()?,
                &expected.state.project_id,
                &pending.scope.session_id,
            )?;
            &parent
        };
        let generation = u64::try_from(pending.scope.generation)
            .ok()
            .and_then(|value| value.checked_sub(1))
            .ok_or_else(invalid)?;
        let source = owner.load_snapshot(generation, &pending.source.raw_hash)?;
        let saved_summary = verify_structured_summary_source(
            expected,
            owner,
            &source,
            &SourceReference {
                scope: pending.scope.clone(),
                summary_id: pending.id.clone(),
                source: pending.source.clone(),
            },
        )?;
        if pending.summary != saved_summary
            || pending.summary_hash != content_hash(saved_summary.as_bytes())
        {
            return Err(invalid());
        }
        Ok(())
    }
}

fn accepted_query(snapshot: &ConversationSnapshot) -> io::Result<&str> {
    if snapshot.state.history_mode != HistoryMode::Structured10 {
        return Err(io::Error::other(
            "Related search requires structured memory",
        ));
    }
    snapshot
        .events
        .iter()
        .rev()
        .find(|event| {
            event.epoch == snapshot.state.epoch
                && event.starts_turn
                && event.message.role == polaris_provider::Role::User
        })
        .map(|event| event.message.content.as_str())
        .filter(|query| !query.trim().is_empty())
        .ok_or_else(|| io::Error::other("No admitted input for related search"))
}

pub(super) fn mandatory_context(
    store: &ConversationStore,
    expected: &ConversationSnapshot,
    memory: &MemoryStore,
    view: &polaris_memory::structured::StructuredPublicationView,
    query: &str,
) -> io::Result<structured::packing::StructuredContext> {
    structured::packing::prepare_context(memory, expected, view, query, |publication, source| {
        structured::sources::resolve_source_from_snapshot(
            store,
            expected,
            memory,
            &publication.publication_id,
            source,
        )
    })
}
