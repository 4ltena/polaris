//! Owner-aware additive index initialization before resource binding.
use super::*;
use polaris_memory::structured::{PublicationScope, StructuredPublicationView};

impl Writer {
    pub fn prepare_structured_memory_index(
        &self,
        target: &RunTarget,
        database: &Path,
        resources: &MemoryResources,
    ) -> StoreResult<()> {
        resources.validate()?;
        let p = self.snapshot()?;
        active(&p, target)?;
        if p.state.configuration.history_mode != HistoryMode::Structured10 {
            return Err(StoreError::RunConflict);
        }
        if p.state
            .conversation_memory
            .as_ref()
            .is_some_and(|saved| &saved.resources != resources)
        {
            return Err(StoreError::CasConflict("memory resources"));
        }
        let fresh = p
            .state
            .conversation_memory
            .as_ref()
            .is_none_or(|saved| saved.sources.is_empty() && saved.ancestors.is_empty());
        let mut db = if fresh && !database.try_exists()? {
            MemoryStore::open(database)
        } else {
            MemoryStore::open_existing(database)
        }
        .map_err(std::io::Error::other)?;
        if !fresh {
            return frozen::initialize(self, &mut db);
        }
        let scope = Scope {
            project_id: p.marker.project_id.as_str().into(),
            session_id: p.marker.session_id.as_str().into(),
            epoch: number(p.marker.epoch.get())?,
            generation: 0,
        };
        let prior = StructuredPublicationView {
            scope: PublicationScope {
                project_id: scope.project_id.clone(),
                session_id: scope.session_id.clone(),
                epoch: scope.epoch,
                generation: scope.generation,
            },
            visible_publication_ids: vec![],
            ancestors: vec![],
        };
        db.enable_structured_memory_with_receipt_stream(
            &PublishedView {
                scope,
                visible_ids: vec![],
                ancestors: vec![],
            },
            &prior,
            &mut |_, _, verifier| {
                verifier.verify_summaries(|_| {
                    Err(polaris_memory::Error::InvalidInput(
                        "unexpected fresh receipt",
                    ))
                })
            },
        )
        .map_err(std::io::Error::other)?;
        Ok(())
    }
}
