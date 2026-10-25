//! Keep validated frozen owners locked through the additive SQLite commit.
use super::*;

pub(in crate::desktop_store::memory) fn initialize(
    writer: &Writer,
    db: &mut MemoryStore,
) -> StoreResult<()> {
    let p = writer.snapshot()?;
    let resources = &memory(&p)?.resources;
    let (targets, prior) = grants(&p)?;
    let view = PublishedView {
        scope: Scope {
            project_id: prior.scope.project_id.clone(),
            session_id: prior.scope.session_id.clone(),
            epoch: prior.scope.epoch,
            generation: prior.scope.generation,
        },
        visible_ids: targets[0].summary_ids.clone(),
        ancestors: targets[1..]
            .iter()
            .map(|source| {
                Ok(polaris_memory::conversation::AncestorRange {
                    session_id: source.marker.session_id.as_str().into(),
                    epoch: number(source.marker.epoch.get())?,
                    generation: number(source.generation.get())?,
                    visible_ids: source.summary_ids.clone(),
                })
            })
            .collect::<StoreResult<_>>()?,
    };
    let work = bounded::ReadWork::default();
    let mut parents = Vec::new();
    let mut owner_error = None;
    let result = db.enable_structured_memory_with_receipt_stream(
        &view,
        &prior,
        &mut |scope, ids, verifier| {
            let checked = || -> StoreResult<()> {
                let source = targets
                    .iter()
                    .find(|source| {
                        source.marker.project_id.as_str() == scope.project_id
                            && source.marker.session_id.as_str() == scope.session_id
                            && number(source.marker.epoch.get()).ok() == Some(scope.epoch)
                            && number(source.generation.get()).ok() == Some(scope.generation)
                            && source.structured_ids == ids
                    })
                    .ok_or(StoreError::Corrupt("Pinned owner scope at initialization"))?;
                let owner = FrozenMemory::load(writer, work.clone(), source, &mut |delta| {
                    verifier.submit_structured(delta)
                })?;
                if &owner.resources != resources {
                    return Err(StoreError::Corrupt(
                        "Pinned ancestor memory resource mismatch",
                    ));
                }
                verifier
                    .verify_summaries(|pending| {
                        owner.verify_summary(pending).map(|_| ()).map_err(|error| {
                            polaris_memory::Error::Io(std::io::Error::other(error))
                        })
                    })
                    .map_err(std::io::Error::other)?;
                if source.marker.session_id != p.marker.session_id {
                    parents.push(owner.files.owner.into_parent()?);
                }
                Ok(())
            }();
            checked.map_err(|error| {
                owner_error = Some(error);
                polaris_memory::Error::InvalidInput("initialization owner verification failed")
            })
        },
    );
    if let Some(error) = owner_error {
        return Err(error);
    }
    result.map_err(std::io::Error::other)?;
    Ok(())
}
