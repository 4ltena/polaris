//! M7b-4: putting an agreed shared tree into the project. Nothing here writes
//! to the project directly: it builds an isolated copy, shapes that copy to
//! the agreed tree, and hands the difference to the existing apply owner,
//! which keeps the recovery copy, the journal and the protected-path rules.
use super::share::ShareManifest;
#[cfg(unix)]
use crate::isolated_workspace::snapshot;
use crate::isolated_workspace::{ChangeSet, Limits, RegisteredSecrets, Snapshot};
#[cfg(unix)]
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

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
#[cfg(unix)]
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
#[cfg(unix)]
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
#[cfg(unix)]
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
    .map_err(|_| io::Error::other("Cannot verify protection state"))?
    .map_err(|_| io::Error::other("Cannot verify protection state"))?;
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
            "Cannot apply shared changes to this project",
        ));
    }
    Ok((
        crate::workspace_apply::protected_apply(&snapshot, &changes, limits, recovery_parent),
        shaped,
    ))
}

/// C12's durable lifecycle. The caller supplies the retained agreed revision
/// and exact participation body; the shared owner derives admission evidence.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[allow(clippy::too_many_arguments)]
pub fn apply_agreed_m2a(
    owner: &crate::apply_owner::ApplyOwnerV1,
    admission: crate::apply_owner::C12ApplyAdmissionV1,
    project_root: &Path,
    agreed_tree: &Path,
    manifest: &ShareManifest,
    changed: &Changed,
    limits: Limits,
    recovery_parent: &crate::workspace_apply::RecoveryParent,
) -> io::Result<(crate::workspace_apply::ApplyReport, Shaped)> {
    let (snapshot, shaped, changes) =
        polaris_auth::protection::with_protected_paths(|registered| {
            let secrets = RegisteredSecrets::from_auth_registry(registered)
                .map_err(|_| io::Error::other("Cannot verify protection state"))?;
            let (snapshot, shaped) = shape_copy(
                project_root,
                agreed_tree,
                manifest,
                changed,
                &secrets,
                limits,
            )?;
            if !shaped.deferred.is_empty() {
                return Err(io::Error::other(
                    "Shared changes requiring new directories remain",
                ));
            }
            let changes = planned_changes(&snapshot, &secrets, limits)?;
            Ok::<_, io::Error>((snapshot, shaped, changes))
        })
        .map_err(|_| io::Error::other("Cannot verify protection state"))??;
    let report = owner
        .apply_c12(admission, &snapshot, &changes, limits, recovery_parent)
        .map_err(io::Error::other)?;
    Ok((report, shaped))
}

#[cfg(all(test, unix))]
mod tests;

#[cfg(not(unix))]
fn unavailable() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "shared apply is unavailable on this platform",
    )
}

#[cfg(not(unix))]
pub fn shape_copy(
    _project_root: &Path,
    _agreed_tree: &Path,
    _manifest: &ShareManifest,
    _changed: &Changed,
    _secrets: &RegisteredSecrets,
    _limits: Limits,
) -> io::Result<(Snapshot, Shaped)> {
    Err(unavailable())
}

#[cfg(not(unix))]
pub fn planned_changes(
    _snapshot: &Snapshot,
    _secrets: &RegisteredSecrets,
    _limits: Limits,
) -> io::Result<ChangeSet> {
    Err(unavailable())
}

#[cfg(not(unix))]
pub fn apply_agreed(
    _project_root: &Path,
    _agreed_tree: &Path,
    _manifest: &ShareManifest,
    _changed: &Changed,
    _limits: Limits,
    _recovery_parent: &Path,
) -> io::Result<(crate::workspace_apply::ApplyReport, Shaped)> {
    Err(unavailable())
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn windows_sync_apply_refuses_existing_and_missing_trees_without_changes() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let agreed = root.path().join("agreed");
        let recovery = root.path().join("recovery");
        for path in [&project, &agreed, &recovery] {
            std::fs::create_dir(path).unwrap();
        }
        std::fs::write(project.join("file.rs"), b"before").unwrap();
        std::fs::write(agreed.join("file.rs"), b"after").unwrap();
        let missing = root.path().join("missing");
        let changed = Changed {
            written: vec!["file.rs".into()],
            removed: vec!["local.txt".into()],
        };
        let limits = Limits {
            max_entries: 8,
            max_files: 4,
            max_file_bytes: 128,
            max_total_bytes: 256,
            max_depth: 2,
        };
        for (project, agreed, recovery) in [
            (&project, &agreed, &recovery),
            (&missing, &missing, &missing),
        ] {
            assert_eq!(
                apply_agreed(
                    project,
                    agreed,
                    &ShareManifest::default(),
                    &changed,
                    limits,
                    recovery,
                )
                .unwrap_err()
                .kind(),
                io::ErrorKind::Unsupported
            );
        }
        assert_eq!(std::fs::read(project.join("file.rs")).unwrap(), b"before");
        assert_eq!(std::fs::read(agreed.join("file.rs")).unwrap(), b"after");
        assert_eq!(std::fs::read_dir(project).unwrap().count(), 1);
        assert_eq!(std::fs::read_dir(agreed).unwrap().count(), 1);
        assert_eq!(std::fs::read_dir(recovery).unwrap().count(), 0);
        assert!(!missing.exists());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 3);
    }
}
