//! Stage the helper binary outside the writable roots.
//!
//! `current_exe()` normally lives inside the build output, i.e. inside the
//! workspace. If someone who can write to the workspace replaces it, the
//! next mutation operation executes the replaced code under confinement.
//! For M4's `read write`-typed subagent, this is exactly a path to
//! obtaining a permission its type was never granted.

use std::path::{Path, PathBuf};

use crate::SandboxError;
use crate::policy::SandboxPolicy;

/// Stages the running binary and returns its location.
pub fn staged_helper(policy: &SandboxPolicy, state_dir: &Path) -> Result<PathBuf, SandboxError> {
    let exe = std::env::current_exe()?;
    staged_helper_from(policy, state_dir, &exe)
}

/// The body, made so tests can substitute the actual binary.
pub fn staged_helper_from(
    policy: &SandboxPolicy,
    state_dir: &Path,
    exe: &Path,
) -> Result<PathBuf, SandboxError> {
    let canonical = exe.canonicalize()?;

    let inside_writable = policy
        .writable_roots()
        .iter()
        .any(|r| canonical.starts_with(r));
    if !inside_writable {
        return Ok(canonical);
    }

    std::fs::create_dir_all(state_dir)?;
    let dest = state_dir.join("polaris-helper");

    // Always re-copy if the content has changed. Continuing to use a stale
    // copy means the helper you thought you fixed doesn't work, and since
    // the symptom just looks like "it's not fixed", the cause is hard to
    // see. Compare by content, not by size and mtime.
    let need_copy = match std::fs::read(&dest) {
        Ok(existing) => existing != std::fs::read(&canonical)?,
        Err(_) => true,
    };
    if need_copy {
        std::fs::copy(&canonical, &dest)?;
        #[cfg(unix)]
        {
            // 0700 protects against other users, not against a confined
            // subagent running under the same UID. What stops that is the
            // verification that follows immediately (confirming the staged
            // location is outside the writable roots), and this mode is
            // defense in depth, not the primary control. "Hardening" it
            // further in the future would not add to what is protected
            // here.
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o700))?;
        }
    }

    let dest_canonical = dest.canonicalize()?;

    // If the staged location itself is inside a writable root (including
    // resolution through a symlink), this function's whole reason for
    // existing inverts. Rather than relying on the caller to pass a good
    // state_dir, verify our own output here. The next caller is Task 12's
    // CLI wiring, and after that M4's subagent scheduler, and getting this
    // wrong there means creating arbitrary code execution under
    // confinement.
    if let Some(root) = policy
        .writable_roots()
        .iter()
        .find(|r| dest_canonical.starts_with(r))
    {
        return Err(SandboxError::NotEnforced(format!(
            "the staged location {} is inside the writable root {}",
            dest_canonical.display(),
            root.display()
        )));
    }

    Ok(dest_canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{SandboxMode, SandboxPolicy};

    #[test]
    fn a_helper_inside_a_writable_root_is_copied_out_of_it() {
        // This is what blocks M4's privilege escalation. Don't leave a
        // state where someone who can write to the workspace can replace
        // the helper.
        let root = tempfile::tempdir().expect("temp dir");
        let state = tempfile::tempdir().expect("temp dir");

        let fake_exe = root.path().join("polaris");
        std::fs::write(&fake_exe, b"#!/bin/sh\nexit 0\n").expect("can't write");

        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("policy");

        let staged = staged_helper_from(&policy, state.path(), &fake_exe).expect("can't stage");

        assert!(
            !staged.starts_with(policy.writable_roots()[0].as_path()),
            "the staged location is inside the writable root: {}",
            staged.display()
        );
        assert!(staged.exists(), "no file at the staged location");
    }

    #[test]
    fn a_helper_already_outside_every_root_is_used_as_is() {
        // Don't make an unnecessary copy. There's no need to copy an
        // already installed binary every time.
        let root = tempfile::tempdir().expect("temp dir");
        let elsewhere = tempfile::tempdir().expect("temp dir");
        let state = tempfile::tempdir().expect("temp dir");

        let exe = elsewhere.path().join("polaris");
        std::fs::write(&exe, b"x").expect("can't write");

        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("policy");

        let staged = staged_helper_from(&policy, state.path(), &exe).expect("can't resolve");
        assert_eq!(staged, exe.canonicalize().expect("canonicalize"));
    }

    #[test]
    fn a_state_dir_inside_a_writable_root_is_rejected() {
        // If state_dir is inside the root, the copy's destination ends up
        // inside it too. That would directly invert this function's
        // purpose, so reject it ourselves instead of relying on the
        // caller's choice.
        let root = tempfile::tempdir().expect("temp dir");
        let state = root.path().join("state");
        let fake_exe = root.path().join("polaris");
        std::fs::write(&fake_exe, b"x").expect("can't write");

        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("policy");

        let err = staged_helper_from(&policy, &state, &fake_exe)
            .expect_err("a state_dir inside the writable root went through");
        assert!(matches!(err, SandboxError::NotEnforced(_)), "{err}");
    }

    #[test]
    fn a_state_dir_reached_through_a_symlink_into_a_writable_root_is_rejected() {
        // Even when state_dir's own path is outside the writable root, if
        // following a symlink lands inside the root, the same problem
        // occurs. This pins down that the comparison must canonicalize
        // first rather than doing a string-level starts_with.
        let root = tempfile::tempdir().expect("temp dir");
        let real_state = root.path().join("state");
        std::fs::create_dir_all(&real_state).expect("can't create");

        let link_parent = tempfile::tempdir().expect("temp dir");
        let state_link = link_parent.path().join("state-link");
        std::os::unix::fs::symlink(&real_state, &state_link).expect("symlink");

        let fake_exe = root.path().join("polaris");
        std::fs::write(&fake_exe, b"x").expect("can't write");

        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("policy");

        let err = staged_helper_from(&policy, &state_link, &fake_exe)
            .expect_err("a writable root reached through a symlink went through");
        assert!(matches!(err, SandboxError::NotEnforced(_)), "{err}");
    }

    #[test]
    fn a_stale_staged_copy_is_refreshed_when_the_source_changes() {
        // If the content changed but an old copy keeps being used, the
        // helper you thought you fixed doesn't work. And the symptom just
        // looks like "it's not fixed", making the cause hard to see.
        let root = tempfile::tempdir().expect("temp dir");
        let state = tempfile::tempdir().expect("temp dir");
        let exe = root.path().join("polaris");

        std::fs::write(&exe, b"version-1").expect("can't write");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("policy");
        let first = staged_helper_from(&policy, state.path(), &exe).expect("stage");
        assert_eq!(std::fs::read(&first).expect("can't read"), b"version-1");

        std::fs::write(&exe, b"version-2").expect("can't write");
        let second = staged_helper_from(&policy, state.path(), &exe).expect("stage");
        assert_eq!(
            std::fs::read(&second).expect("can't read"),
            b"version-2",
            "an old copy is being used"
        );
    }
}
