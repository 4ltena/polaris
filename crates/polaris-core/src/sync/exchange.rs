//! M7b-3: exchanging with the other devices and deciding what each change
//! means. Git performs the fetch and the three-way merge; this module decides
//! what may be merged mechanically, what is kept from both sides, and what a
//! model would have to look at. Nothing here writes into the project: the
//! result is a decision the apply owner carries out.
use super::repository::SyncRepository;
use serde::Serialize;
use std::io;

/// What one path's change turned into.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Resolution {
    /// One side changed it, both made the same change, or the two changes
    /// did not overlap.
    Merged,
    /// Both versions are kept; a person or a model decides later.
    BothKept { reason: Kept },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kept {
    /// Overlapping edits in a text file.
    Overlapping,
    /// A file neither side can merge by lines.
    Binary,
    /// One side deleted what the other changed.
    DeleteVersusEdit,
    /// A rename that cannot be matched to one source with certainty.
    AmbiguousRename,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Change {
    pub path: String,
    pub resolution: Resolution,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Exchange {
    /// The commit both sides now agree on, when there is one.
    pub merged_commit: Option<String>,
    pub changes: Vec<Change>,
    /// True when something is waiting for a person or a model.
    pub unresolved: bool,
}

impl Exchange {
    pub fn needs_attention(&self) -> Vec<&Change> {
        self.changes
            .iter()
            .filter(|change| matches!(change.resolution, Resolution::BothKept { .. }))
            .collect()
    }
}

impl SyncRepository {
    /// Sends the local commits. A rejected push means the other side moved
    /// first; the caller receives and merges, then sends again.
    pub fn send(&self, remote: &str) -> io::Result<()> {
        self.git_public(&["push", remote, "polaris:polaris"])
            .map(drop)
    }

    /// Brings in the other side's commits without touching the staging tree.
    pub fn receive(&self, remote: &str) -> io::Result<Option<String>> {
        self.git_public(&["fetch", "--quiet", remote, "polaris:refs/polaris/incoming"])?;
        Ok(self
            .git_public(&["rev-parse", "--verify", "--quiet", "refs/polaris/incoming"])
            .ok()
            .filter(|commit| !commit.is_empty()))
    }

    /// Merges what was received into the local branch. Mechanical merges are
    /// taken; anything Git leaves conflicted is kept from both sides.
    pub fn merge_received(&self) -> io::Result<Exchange> {
        let Some(incoming) = self
            .git_public(&["rev-parse", "--verify", "--quiet", "refs/polaris/incoming"])
            .ok()
            .filter(|commit| !commit.is_empty())
        else {
            return Ok(Exchange::default());
        };
        let before = self.head();
        if before.as_deref() == Some(incoming.as_str()) {
            return Ok(Exchange {
                merged_commit: before,
                ..Default::default()
            });
        }
        let Some(before) = before else {
            // Nothing local yet: this device simply takes what it received.
            self.git_public(&["update-ref", "refs/heads/polaris", &incoming])?;
            self.git_public(&["reset", "--hard", "--quiet", &incoming])?;
            let head = self.head();
            let changes = self
                .files_at(&incoming)?
                .into_iter()
                .map(|path| Change {
                    path,
                    resolution: Resolution::Merged,
                })
                .collect();
            return Ok(Exchange {
                merged_commit: head,
                changes,
                unresolved: false,
            });
        };
        // The two devices may have started their own histories before ever
        // meeting; the three-way merge then simply has an empty base.
        let merged = self
            .git_public(&[
                "merge",
                "--no-edit",
                "--no-ff",
                "--allow-unrelated-histories",
                &incoming,
            ])
            .is_ok();
        if merged {
            let head = self.head();
            let changes = self.changed_between(Some(&before), head.as_deref())?;
            return Ok(Exchange {
                merged_commit: head,
                changes,
                unresolved: false,
            });
        }
        // Git could not merge it by itself: keep both sides and stop.
        let conflicted = self.git_public(&["diff", "--name-only", "--diff-filter=U"])?;
        let changes = conflicted
            .lines()
            .map(|path| Change {
                path: path.to_owned(),
                resolution: Resolution::BothKept {
                    reason: self.kept_reason(path),
                },
            })
            .collect::<Vec<_>>();
        // The working state is left as the conflict, and the local branch is
        // not moved: nothing is applied to the project from here.
        let _ = self.git_public(&["merge", "--abort"]);
        Ok(Exchange {
            merged_commit: None,
            unresolved: !changes.is_empty(),
            changes,
        })
    }

    fn kept_reason(&self, path: &str) -> Kept {
        // `ls-files -u` lists the three stages; a missing stage means one side
        // removed it, which is never merged silently.
        let stages = self
            .git_public(&["ls-files", "-u", "--", path])
            .unwrap_or_default();
        let stage_numbers: Vec<&str> = stages
            .lines()
            .filter_map(|line| line.split_whitespace().nth(2))
            .collect();
        if !stage_numbers.contains(&"2") || !stage_numbers.contains(&"3") {
            return Kept::DeleteVersusEdit;
        }
        if self.is_binary(path) {
            return Kept::Binary;
        }
        Kept::Overlapping
    }

    fn is_binary(&self, path: &str) -> bool {
        self.git_public(&["diff", "--numstat", "--", path])
            .is_ok_and(|output| output.starts_with("-\t-"))
    }

    fn changed_between(
        &self,
        before: Option<&str>,
        after: Option<&str>,
    ) -> io::Result<Vec<Change>> {
        let (Some(before), Some(after)) = (before, after) else {
            return Ok(Vec::new());
        };
        Ok(self
            .git_public(&["diff", "--name-only", before, after])?
            .lines()
            .map(|path| Change {
                path: path.to_owned(),
                resolution: Resolution::Merged,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests;
