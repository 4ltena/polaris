//! `run.details`: one immutable details snapshot, paged by UTF-8-safe fragments.
//! Reading never changes the store, the run or the request ledger.
use super::*;
use polaris_core::desktop_store::RunDetails;
use polaris_desktop_protocol::run_details::{
    MAX_FRAGMENT_BYTES, MAX_FRAGMENTS, MAX_PAGE_TEXT_BYTES, RunDetailFragment, RunDetailsPage,
    RunDetailsRequest,
};

const DETAILS_TTL: Duration = Duration::from_secs(30);

pub(super) struct Details {
    id: String,
    target: RunTarget,
    details: RunDetails,
    pub(super) expires: Instant,
}

impl Engine {
    pub(super) fn run_details(
        &mut self,
        params: &RunDetailsRequest,
    ) -> Result<RunDetailsPage, ProtocolError> {
        let target = RunTarget {
            run_id: params.run_id.clone(),
            attempt_id: params.attempt_id.clone(),
        };
        let (entry, offset) = match (&params.snapshot_id, &params.cursor) {
            (None, None) => {
                let details = self.store.project_run_details(&target).map_err(|e| {
                    // A projection mismatch fails this read only, never the engine.
                    error(match e {
                        StoreError::NotFound => ErrorCode::NotFound,
                        _ => ErrorCode::CapabilityUnavailable,
                    })
                })?;
                let id = self.id("details");
                self.details = Some(Details {
                    id,
                    target,
                    details,
                    expires: Instant::now() + DETAILS_TTL,
                });
                (0, 0)
            }
            (Some(id), Some(cursor)) => {
                let held = self
                    .details
                    .as_ref()
                    .filter(|d| &d.id == id && d.target == target && Instant::now() < d.expires)
                    .ok_or_else(|| error(ErrorCode::RevisionConflict))?;
                cursor
                    .strip_prefix(&format!("{id}:"))
                    .and_then(|rest| rest.split_once(':'))
                    .and_then(|(e, o)| Some((e.parse::<usize>().ok()?, o.parse::<usize>().ok()?)))
                    .filter(|(e, o)| {
                        held.details.entries.get(*e).is_some_and(|entry| {
                            *o < entry.text.len() && entry.text.is_char_boundary(*o)
                        }) && cursor == &format!("{id}:{e}:{o}")
                    })
                    .ok_or_else(|| error(ErrorCode::RevisionConflict))?
            }
            _ => return Err(error(ErrorCode::InvalidRequest)),
        };
        let held = self.details.as_ref().expect("details snapshot");
        let entries = &held.details.entries;
        let (mut e, mut o) = (entry, offset);
        let mut fragments = Vec::new();
        let mut total = 0;
        while e < entries.len() && fragments.len() < MAX_FRAGMENTS {
            let text = &entries[e].text;
            let mut end =
                (o + (MAX_PAGE_TEXT_BYTES - total).min(MAX_FRAGMENT_BYTES)).min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            if end == o && o < text.len() {
                break;
            }
            fragments.push(RunDetailFragment {
                entry_id: entries[e].id.clone(),
                kind: entries[e].kind,
                offset: DecimalU64::new(o as u64),
                total_bytes: DecimalU64::new(text.len() as u64),
                text: text[o..end].to_owned(),
            });
            total += end - o;
            (e, o) = if end == text.len() {
                (e + 1, 0)
            } else {
                (e, end)
            };
        }
        Ok(RunDetailsPage {
            snapshot_id: held.id.clone(),
            run_id: held.target.run_id.clone(),
            attempt_id: held.target.attempt_id.clone(),
            session_revision: held.details.session_revision,
            accepted_revision: held.details.accepted_revision,
            run_state: held.details.run_state,
            history_gap: held.details.history_gap,
            fragments,
            next_cursor: (e < entries.len()).then(|| format!("{}:{e}:{o}", held.id)),
        })
    }
}
