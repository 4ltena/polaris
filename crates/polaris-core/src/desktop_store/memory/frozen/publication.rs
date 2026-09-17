//! Validate frozen owners while publishing only the current session's marker.
use super::*;
use std::cell::RefCell;

#[cfg(test)]
thread_local! {
    static BEFORE_MARKER: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
}
#[cfg(test)]
pub(in crate::desktop_store::memory) fn before_marker(check: impl FnOnce() + 'static) {
    BEFORE_MARKER.with(|slot| *slot.borrow_mut() = Some(Box::new(check)));
}

pub(in crate::desktop_store::memory) fn publish(
    writer: &mut Writer,
    db: &mut MemoryStore,
    view: &PublishedView,
    pending_id: &String,
    delta: Option<&StructuredDelta>,
    next: Published,
) -> StoreResult<()> {
    let p = writer.snapshot()?;
    let resources = &memory(&p)?.resources;
    let (targets, prior) = grants(&p)?;
    let work = bounded::ReadWork::default();
    // The callbacks run sequentially. Parent guards outlive both DB phases and
    // marker publication; the current session retains its exclusive Writer.
    let writer = RefCell::new(writer);
    let mut parents = BTreeMap::new();
    let mut owner_error = None;
    let mut scanner =
        |scope: &PublicationScope,
         ids: &[String],
         verifier: &mut polaris_memory::conversation::PublicationReceiptVerifier<'_>| {
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
                    .ok_or(StoreError::Corrupt("公開時の固定owner範囲"))?;
                let writer = writer.borrow();
                let owner = FrozenMemory::load(&writer, work.clone(), source, &mut |delta| {
                    verifier.submit_structured(delta)
                })?;
                if &owner.resources != resources {
                    return Err(StoreError::Corrupt("固定祖先の記憶資源不一致"));
                }
                verifier
                    .verify_summaries(|pending| {
                        owner.verify_summary(pending).map(|_| ()).map_err(|error| {
                            polaris_memory::Error::Io(std::io::Error::other(error))
                        })
                    })
                    .map_err(std::io::Error::other)?;
                if source.marker.session_id != p.marker.session_id {
                    // Keep the validated owner locked across the index commit
                    // and marker publication without rereading its raw files.
                    parents.insert(
                        source.marker.session_id.clone(),
                        owner.files.owner.into_parent()?,
                    );
                }
                Ok(())
            }();
            checked.map_err(|error| {
                owner_error = Some(error);
                polaris_memory::Error::InvalidInput("frozen publication owner verification failed")
            })
        };
    let commit = || {
        #[cfg(test)]
        BEFORE_MARKER.with(|slot| {
            if let Some(check) = slot.borrow_mut().take() {
                check();
            }
        });
        writer
            .borrow_mut()
            .commit(next, false)
            .map_err(|error| polaris_memory::Error::Io(std::io::Error::other(error)))
    };
    let result = if let Some(delta) = delta {
        db.publish_pending_with_structured_receipt_stream(
            view,
            std::slice::from_ref(pending_id),
            delta,
            &prior,
            &mut scanner,
            commit,
        )
    } else {
        db.publish_pending_with_receipt_stream(
            view,
            std::slice::from_ref(pending_id),
            &prior,
            &mut scanner,
            commit,
        )
    };
    if let Some(error) = owner_error {
        return Err(error);
    }
    result.map_err(std::io::Error::other)?;
    Ok(())
}
