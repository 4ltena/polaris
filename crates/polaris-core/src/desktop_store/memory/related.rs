//! 保存済み応答と原文へ照合した関連要約候補。投入・分類・認可は変更しない。
use super::*;
use polaris_memory::conversation::{QueryEmbedding, RelatedSummaryCandidates};

impl MemorySnapshot {
    pub(crate) fn related_query(&self) -> StoreResult<&str> {
        if self.conversation.state.history_mode != HistoryMode::Structured10 {
            return Err(StoreError::Corrupt(
                "Related search requires structured memory",
            ));
        }
        self.conversation
            .events
            .iter()
            .rev()
            .find(|event| {
                event.epoch == self.conversation.state.epoch
                    && event.starts_turn
                    && event.message.role == polaris_provider::Role::User
            })
            .map(|event| event.message.content.as_str())
            .ok_or(StoreError::Corrupt(
                "No admitted input for related summarization",
            ))
    }
}

impl Writer {
    pub(crate) fn preflight_related_memory(
        &self,
        target: &RunTarget,
        expected: &MemorySnapshot,
        database: &Path,
    ) -> StoreResult<()> {
        self.check_memory_snapshot(target, expected)?;
        let view = expected.view()?;
        let query = expected.related_query()?;
        let db = MemoryStore::open_read_only(database).map_err(std::io::Error::other)?;
        db.preflight_related_summary_scan(&view, query)
            .map_err(std::io::Error::other)?;
        self.check_memory_snapshot(target, expected)
    }

    pub(crate) fn search_related_memory(
        &self,
        target: &RunTarget,
        expected: &MemorySnapshot,
        database: &Path,
        embedded: &crate::conversation_memory::related::EmbeddedRelatedQuery,
    ) -> StoreResult<String> {
        self.check_memory_snapshot(target, expected)?;
        let current = self.memory_snapshot(target)?;
        let parts = embedded.checked_parts(current.related_query()?)?;
        let db = MemoryStore::open_read_only(database).map_err(std::io::Error::other)?;
        frozen::with_receipts(self, target, expected, &db, |owners| {
            let context = self.prepare_structured_memory_from_receipts(expected, &db, owners)?;
            use polaris_desktop_protocol::snapshot::MemoryRetrievalStatus;
            if matches!(
                context.status,
                MemoryRetrievalStatus::ExtractionIncomplete
                    | MemoryRetrievalStatus::EvidenceBudgetExceeded
            ) {
                return Err(StoreError::Corrupt(
                    "Cannot prepare structured memory evidence",
                ));
            }
            let candidates =
                self.related_memory_candidates_from_receipts(expected, &db, owners, Some(parts))?;
            crate::conversation_memory::related::render_candidates(context.status, candidates)
                .map_err(Into::into)
        })
    }

    /// The accepted input and frozen owner marker determine the query and scope.
    /// This returns historical candidates, not a successful current-state lookup.
    /// Consumers must recheck the marker before dispatch and retain scoped IDs.
    pub fn prepare_related_memory(
        &self,
        target: &RunTarget,
        expected: &MemorySnapshot,
        database: &Path,
        embedding: QueryEmbedding,
    ) -> StoreResult<RelatedSummaryCandidates> {
        self.related_memory_candidates(
            target,
            expected,
            database,
            Some(std::slice::from_ref(&embedding)),
        )
    }

    pub(crate) fn prepare_structured_memory_with_related(
        &self,
        target: &RunTarget,
        expected: &MemorySnapshot,
        database: &Path,
    ) -> StoreResult<crate::conversation_memory::structured::packing::StructuredContext> {
        self.check_memory_snapshot(target, expected)?;
        expected.structured_view()?;
        expected.related_query()?;
        let db = MemoryStore::open(database).map_err(std::io::Error::other)?;
        frozen::with_receipts(self, target, expected, &db, |owners| {
            let mut context =
                self.prepare_structured_memory_from_receipts(expected, &db, owners)?;
            #[cfg(all(test, unix))]
            super::tests::after_mandatory_read();
            if context.accepts_related() {
                match self.related_memory_candidates_from_receipts(expected, &db, owners, None) {
                    Ok(candidates) => {
                        for candidate in &candidates.keyword {
                            context.append_keyword_summary(&candidate.pending)?;
                        }
                    }
                    Err(StoreError::RelatedSummaryBudget(budget)) => {
                        context.related_omission = Some(budget);
                    }
                    Err(error) => return Err(error),
                }
            }
            Ok(context)
        })
    }

    fn related_memory_candidates(
        &self,
        target: &RunTarget,
        expected: &MemorySnapshot,
        database: &Path,
        embeddings: Option<&[QueryEmbedding]>,
    ) -> StoreResult<RelatedSummaryCandidates> {
        let db = MemoryStore::open_read_only(database).map_err(std::io::Error::other)?;
        frozen::with_receipts(self, target, expected, &db, |owners| {
            self.related_memory_candidates_from_receipts(expected, &db, owners, embeddings)
        })
    }

    fn related_memory_candidates_from_receipts(
        &self,
        expected: &MemorySnapshot,
        db: &MemoryStore,
        owners: &[frozen::FrozenMemory<'_>],
        embeddings: Option<&[QueryEmbedding]>,
    ) -> StoreResult<RelatedSummaryCandidates> {
        let view = expected.view()?;
        let query = expected.related_query()?;
        let mut raw_bytes = 0u64;
        let verify = |pending: &PendingSummary| {
            let checked = || -> StoreResult<_> {
                let scope = polaris_memory::structured::PublicationScope {
                    project_id: pending.scope.project_id.clone(),
                    session_id: pending.scope.session_id.clone(),
                    epoch: pending.scope.epoch,
                    generation: pending.scope.generation,
                };
                let owner = owners
                    .iter()
                    .find(|owner| owner.matches(&scope))
                    .ok_or(StoreError::NotFound)?;
                let proof = owner.verify_summary(pending)?;
                Ok((owner, proof))
            };
            let (owner, proof) =
                checked().map_err(|e| polaris_memory::Error::Io(std::io::Error::other(e)))?;
            raw_bytes = raw_bytes
                .checked_add(proof.source.source.raw_offset.get())
                .filter(|bytes| *bytes <= 64 * 1024 * 1024)
                .ok_or(polaris_memory::Error::RelatedSummaryBudgetExceeded(
                    polaris_memory::conversation::RelatedSummaryBudget::RawProofWork,
                ))?;
            owner
                .summary_raw(proof)
                .map(|_| ())
                .map_err(|e| polaris_memory::Error::Io(std::io::Error::other(e)))
        };
        if embeddings.is_some_and(|parts| {
            parts.iter().any(|embedding| {
                owners.iter().any(|owner| {
                    embedding.model != owner.resources.embedding_model
                        || embedding.revision != owner.resources.embedding_revision
                        || embedding.dimension != i64::from(owner.resources.embedding_dimension)
                })
            })
        }) {
            return Err(StoreError::Corrupt(
                "Related summary embedding resource mismatch",
            ));
        }
        let result = match embeddings {
            Some(parts) => db.related_summary_chunks_checked(&view, query, parts, verify),
            None => db
                .keyword_summary_candidates_checked(&view, query, verify)
                .map(|keyword| RelatedSummaryCandidates {
                    keyword,
                    semantic: vec![],
                }),
        };
        result.map_err(|error| match error {
            polaris_memory::Error::RelatedSummaryBudgetExceeded(budget) => {
                StoreError::RelatedSummaryBudget(budget)
            }
            error => StoreError::Io(std::io::Error::other(error)),
        })
    }
}
