//! M7a-3: whether spare quota may be spent on refactoring nobody asked for.
//! The answer is almost always no: it needs a fresh, verified reading for
//! every model the work would use, a scope the person allowed, and budgets
//! that still have room. The result is written to separate files and is not
//! merged until the person approves, so a refusal here costs nothing.
use serde::Serialize;

/// Never spend more than this share of what is left before the reset.
pub const MAX_SHARE_PERCENT: u32 = 25;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum Decision {
    Start,
    Refuse { reason: Refusal },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    /// The person did not allow work that was not asked for.
    NotAllowed,
    /// A model this work would use has no verified, fresh reading.
    QuotaUnknown,
    /// What is left is needed for the work the person is doing.
    NoSpare,
    /// A run is going; its quota is not spare.
    Busy,
    /// No candidate worth doing.
    NothingToDo,
}

/// What the owner knows when a run has just ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observed {
    pub allowed_by_person: bool,
    /// Every model the refactoring would use has a known, fresh reading.
    pub quota_known_for_every_model: bool,
    /// Share of the window still unused, 0–100.
    pub remaining_percent: u32,
    /// The share the person's own work is expected to need.
    pub reserved_percent: u32,
    pub run_active: bool,
    pub candidates: usize,
}

pub fn decide(observed: &Observed) -> Decision {
    let refuse = |reason| Decision::Refuse { reason };
    if !observed.allowed_by_person {
        return refuse(Refusal::NotAllowed);
    }
    // An unknown reading is not spare quota. With the shipped profiles this
    // is what stops the whole path today, and that is the intended state
    // until a real account verifies one.
    if !observed.quota_known_for_every_model {
        return refuse(Refusal::QuotaUnknown);
    }
    if observed.run_active {
        return refuse(Refusal::Busy);
    }
    if observed.candidates == 0 {
        return refuse(Refusal::NothingToDo);
    }
    let spare = observed
        .remaining_percent
        .saturating_sub(observed.reserved_percent);
    if spare < MAX_SHARE_PERCENT {
        return refuse(Refusal::NoSpare);
    }
    Decision::Start
}

pub mod from_quota;

#[cfg(test)]
mod tests;

/// Where a refactoring proposal is written, and why it is only ever
/// written.
///
/// Spare-quota work is work nobody asked for, so its result never reaches
/// the person's tree. It lands in the project's own state directory, is
/// stamped with what it was decided from, and is applied — if ever — by a
/// separate act of approval. Nothing here edits a source file, and the
/// saved file is deliberately not in a format anything applies
/// automatically.
pub mod proposal {
    use super::Observed;
    use serde::Serialize;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
    pub struct Saved {
        pub schema_version: u32,
        /// Always false when written. Nothing in this crate sets it true;
        /// applying a proposal is a separate act with its own approval.
        pub applied: bool,
        /// What the decision saw. A proposal read later is only as good as
        /// the reading it was made from, and the reading goes stale.
        pub allowed_by_person: bool,
        pub remaining_percent: u32,
        pub proposals: Vec<crate::similarity::Proposal>,
    }

    /// Writes `proposals` beside the project's other state, under a name
    /// that says what it is. Returns the path written.
    ///
    /// Refuses to write an empty set: a file that says nothing invites the
    /// reader to think the analysis found nothing, when it may equally
    /// never have run.
    pub fn save(
        state_dir: &std::path::Path,
        observed: &Observed,
        proposals: Vec<crate::similarity::Proposal>,
    ) -> std::io::Result<std::path::PathBuf> {
        if proposals.is_empty() {
            return Err(std::io::Error::other("提案が空のため保存しません"));
        }
        let saved = Saved {
            schema_version: 1,
            applied: false,
            allowed_by_person: observed.allowed_by_person,
            remaining_percent: observed.remaining_percent,
            proposals,
        };
        let directory = state_dir.join("spare");
        std::fs::create_dir_all(&directory)?;
        let path = directory.join("proposals.json");
        // Written whole, then moved into place: a reader must never see a
        // half-written proposal and act on it.
        let temporary = directory.join("proposals.json.partial");
        std::fs::write(&temporary, serde_json::to_vec_pretty(&saved)?)?;
        std::fs::rename(&temporary, &path)?;
        Ok(path)
    }

    /// Reads back what was saved, or `None` when nothing has been.
    pub fn load(state_dir: &std::path::Path) -> std::io::Result<Option<Saved>> {
        let path = state_dir.join("spare").join("proposals.json");
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod proposal_tests {
    use super::*;
    use crate::similarity::{Proposal, Suggestion};

    fn observed() -> Observed {
        Observed {
            allowed_by_person: true,
            quota_known_for_every_model: true,
            remaining_percent: 80,
            reserved_percent: 50,
            run_active: false,
            candidates: 1,
        }
    }

    fn proposals() -> Vec<Proposal> {
        vec![Proposal {
            left: "a".into(),
            right: "b".into(),
            suggestion: Suggestion::SameWorkInTwoPlaces,
            because: "同じ名前を9割共有している".into(),
        }]
    }

    #[test]
    fn a_saved_proposal_is_never_marked_applied_and_reads_back_whole() {
        let dir = tempfile::tempdir().expect("a state directory");
        assert_eq!(
            proposal::load(dir.path()).expect("a read"),
            None,
            "nothing saved is not an error"
        );
        let path = proposal::save(dir.path(), &observed(), proposals()).expect("a write");
        assert!(
            path.starts_with(dir.path()),
            "written inside the state tree"
        );
        let held = proposal::load(dir.path())
            .expect("a read")
            .expect("what was written");
        assert!(
            !held.applied,
            "work nobody asked for never reaches the person's tree on its own"
        );
        assert_eq!(held.proposals, proposals());
        // The reading the decision was made from travels with it: a
        // proposal is only as good as the quota it was judged against.
        assert_eq!(held.remaining_percent, 80);
        assert!(held.allowed_by_person);
        // Nothing partial is left behind for a reader to pick up.
        assert!(!dir.path().join("spare/proposals.json.partial").exists());
    }

    #[test]
    fn an_empty_set_is_refused_rather_than_written() {
        // A file that says nothing invites the reader to conclude the
        // analysis found nothing, when it may equally never have run.
        let dir = tempfile::tempdir().expect("a state directory");
        assert!(proposal::save(dir.path(), &observed(), Vec::new()).is_err());
        assert_eq!(proposal::load(dir.path()).expect("a read"), None);
    }
}
