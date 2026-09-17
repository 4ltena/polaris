//! M7b-4: putting an agreed shared tree into the project. Nothing here writes
//! to the project directly: it builds an isolated copy, shapes that copy to
//! the agreed tree, and hands the difference to the existing apply owner,
//! which keeps the recovery copy, the journal and the protected-path rules.
use super::share::ShareManifest;
use crate::isolated_workspace::{ChangeSet, Limits, RegisteredSecrets, Snapshot, snapshot};
use std::path::{Path, PathBuf};
use std::{fs, io};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Shaped {
    pub written: Vec<PathBuf>,
    pub removed: Vec<PathBuf>,
    /// Shared files whose directory does not exist in the project yet. The
    /// apply owner carries file contents only, so these are reported instead
    /// of being applied: the directory is created by the person, or by a
    /// later version of this path.
    pub deferred: Vec<PathBuf>,
}

/// What one agreed state changed against the last one applied, as the paths
/// to write and the paths to remove. Built from the shared history, never
/// from the current contents of the staging copy: a path nobody changed is
/// not touched, so this device's own files stay where they are.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Changed {
    pub written: Vec<String>,
    pub removed: Vec<String>,
}

/// Builds the copy the apply owner will compare against the project. Only the
/// changed shared paths are shaped; everything else in the copy is left
/// exactly as the project has it.
pub fn shape_copy(
    project_root: &Path,
    agreed_tree: &Path,
    manifest: &ShareManifest,
    changed: &Changed,
    secrets: &RegisteredSecrets,
    limits: Limits,
) -> io::Result<(Snapshot, Shaped)> {
    let snapshot = snapshot(project_root, secrets, limits)
        .map_err(|_| io::Error::other("the project could not be copied"))?;
    let copy = snapshot.path().to_owned();
    let mut shaped = Shaped::default();

    for relative in &changed.written {
        let relative = PathBuf::from(relative);
        if !manifest.shares(&relative) {
            continue;
        }
        let source = agreed_tree.join(&relative);
        let Ok(kind) = fs::symlink_metadata(&source).map(|data| data.file_type()) else {
            continue;
        };
        if !kind.is_file() {
            continue;
        }
        let target = copy.join(&relative);
        if !target.parent().is_some_and(Path::is_dir) {
            shaped.deferred.push(relative);
            continue;
        }
        fs::copy(&source, &target)?;
        shaped.written.push(relative);
    }

    // Only a path the other device actually removed is removed here.
    for relative in &changed.removed {
        let relative = PathBuf::from(relative);
        if !manifest.shares(&relative) {
            continue;
        }
        let target = copy.join(&relative);
        if fs::symlink_metadata(&target).is_ok_and(|data| data.file_type().is_file()) {
            fs::remove_file(&target)?;
            shaped.removed.push(relative);
        }
    }
    Ok((snapshot, shaped))
}

/// The difference the apply owner would carry out. Kept separate from the
/// application itself so a caller can show it first.
pub fn planned_changes(
    snapshot: &Snapshot,
    secrets: &RegisteredSecrets,
    limits: Limits,
) -> io::Result<ChangeSet> {
    crate::isolated_workspace::collect_changes(snapshot, secrets, limits)
        .map_err(|_| io::Error::other("the shared changes could not be collected"))
}

/// Puts the agreed tree into the project through the existing apply owner,
/// which keeps a recovery copy and refuses anything protected. The caller
/// must have resolved every kept-from-both-sides path first: this applies
/// what the shared history holds, nothing else.
pub fn apply_agreed(
    project_root: &Path,
    agreed_tree: &Path,
    manifest: &ShareManifest,
    changed: &Changed,
    limits: Limits,
    recovery_parent: &Path,
) -> io::Result<(crate::workspace_apply::ApplyReport, Shaped)> {
    let secrets = polaris_auth::protection::with_protected_paths(|registered| {
        RegisteredSecrets::from_auth_registry(registered).map_err(|_| ())
    })
    .map_err(|_| io::Error::other("保護状態を確認できません"))?
    .map_err(|_| io::Error::other("保護状態を確認できません"))?;
    let (snapshot, shaped) = shape_copy(
        project_root,
        agreed_tree,
        manifest,
        changed,
        &secrets,
        limits,
    )?;
    let changes = planned_changes(&snapshot, &secrets, limits)?;
    if !changes.is_applicable() {
        return Err(io::Error::other(
            "共有された変更をこのprojectへ適用できません",
        ));
    }
    Ok((
        crate::workspace_apply::protected_apply(&snapshot, &changes, limits, recovery_parent),
        shaped,
    ))
}

#[cfg(test)]
mod tests;
