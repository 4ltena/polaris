//! Pure manifest value, observation and change comparisons.

use super::{PreviewChange, PreviewChangeKind, StoredEntry, StoredExclusionReason, StoredManifest};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

impl StoredEntry {
    pub(super) fn same_value(&self, other: &Self) -> bool {
        self.path == other.path
            && self.kind == other.kind
            && self.mode == other.mode
            && self.bytes == other.bytes
            && self.hash == other.hash
    }

    pub(super) fn same_observation(&self, other: &Self) -> bool {
        self.same_value(other)
            && self.device == other.device
            && self.inode == other.inode
            && self.mtime == other.mtime
            && self.ctime == other.ctime
    }
}

impl StoredManifest {
    pub(super) fn entries_map(&self) -> BTreeMap<PathBuf, &StoredEntry> {
        self.entries
            .iter()
            .map(|entry| (entry.path.clone(), entry))
            .collect()
    }
}

pub(super) fn plan_changes(before: &StoredManifest, after: &StoredManifest) -> Vec<PreviewChange> {
    let before = before.entries_map();
    let after = after.entries_map();
    let paths: BTreeSet<_> = before.keys().chain(after.keys()).cloned().collect();
    paths
        .into_iter()
        .filter_map(|path| {
            let kind = match (before.get(&path), after.get(&path)) {
                (None, Some(_)) => PreviewChangeKind::Created,
                (Some(_), None) => PreviewChangeKind::Deleted,
                (Some(left), Some(right)) if left.same_value(right) => return None,
                (Some(left), Some(right)) if left.kind != right.kind => {
                    PreviewChangeKind::TypeChanged
                }
                (Some(left), Some(right))
                    if left.hash == right.hash && left.bytes == right.bytes =>
                {
                    PreviewChangeKind::ModeChanged
                }
                (Some(_), Some(_)) => PreviewChangeKind::Modified,
                (None, None) => return None,
            };
            Some(PreviewChange {
                relative_path: path,
                kind,
            })
        })
        .collect()
}

pub(super) fn same_value(left: &StoredManifest, right: &StoredManifest) -> bool {
    left.project_root == right.project_root
        && left.root_device == right.root_device
        && left.root_inode == right.root_inode
        && left.entries.len() == right.entries.len()
        && left
            .entries
            .iter()
            .zip(&right.entries)
            .all(|(left, right)| left.same_value(right))
        && important_exclusions(left) == important_exclusions(right)
}

pub(super) fn same_observation(left: &StoredManifest, right: &StoredManifest) -> bool {
    same_value(left, right)
        && left
            .entries
            .iter()
            .zip(&right.entries)
            .all(|(left, right)| left.same_observation(right))
}

pub(super) fn important_exclusions(manifest: &StoredManifest) -> Vec<&Path> {
    manifest
        .exclusions
        .iter()
        .filter(|entry| entry.reason == StoredExclusionReason::RepositoryMetadata)
        .map(|entry| entry.path.as_path())
        .collect()
}
