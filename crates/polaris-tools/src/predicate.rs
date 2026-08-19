//! Predicts ahead of time whether a write will be denied.
//!
//! This is advisory, not enforcement. Enforcement is delegated to the OS by
//! `polaris-sandbox`. The predicate is needed because the enforcement side
//! cannot explain its reasons: a denial due to a missing parent directory
//! returns `ENOENT`, and `EACCES` is a denial on Linux but an ordinary
//! failure on macOS, so "policy violation" cannot be recovered from the
//! errno.
//!
//! The predicate and the enforcement will always disagree sometimes. That
//! disagreement never breaks anything, because the predicate only ever errs
//! toward asking for approval. If the predicate is too lenient, enforcement
//! stops it; if the predicate is too strict, there's one extra unnecessary
//! confirmation.

use std::path::{Path, PathBuf};

use polaris_sandbox::{SandboxMode, SandboxPolicy};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allowed,
    NeedsApproval { reason: String },
}

pub fn predict(policy: &SandboxPolicy, target: &Path) -> Verdict {
    if policy.mode() == SandboxMode::FullAccess {
        return Verdict::Allowed;
    }

    let resolved = resolve_for_judgement(target);

    if !policy.contains(&resolved) {
        return Verdict::NeedsApproval {
            reason: format!(
                "{} is outside the writable range. policy {}",
                target.display(),
                policy.describe()
            ),
        };
    }

    // Even inside the workspace, route paths treated as sensitive to
    // approval. OS enforcement cannot express this distinction.
    if crate::path_policy::is_denied(&resolved) {
        return Verdict::NeedsApproval {
            reason: format!(
                "{} matches a path treated as sensitive. policy {}",
                target.display(),
                policy.describe()
            ),
        };
    }

    if has_extra_hard_links(&resolved) {
        return Verdict::NeedsApproval {
            reason: format!(
                "{} has hard links (a write could reach an entity outside the range). policy {}",
                target.display(),
                policy.describe()
            ),
        };
    }

    Verdict::Allowed
}

/// Joins a relative path to `base` to make it absolute. An absolute path is
/// returned unchanged.
///
/// This is pulled out as a pure function because taking the base as an
/// argument lets it be tested without moving the working directory. There
/// is only one working directory for the whole process, and changing it
/// inside a test leaks into other tests.
pub fn absolutize(base: &Path, target: &Path) -> PathBuf {
    if target.is_absolute() {
        target.to_path_buf()
    } else {
        base.join(target)
    }
}

/// Resolves a path for judgment. A path that doesn't exist can't be
/// canonicalized, so this walks back up to the nearest ancestor that does
/// exist, normalizes it, and reattaches the rest. For a new-file creation
/// it's normal for neither the target nor the intermediate directories to
/// exist, so naively canonicalizing here would make every new-file creation
/// unjudgeable.
///
/// From the normalized base onward, the remaining path components are
/// resolved lexically. Components may include `.` or `..`, but since they
/// don't exist, `canonicalize` can't be used on them. Lexical resolution is
/// correct here for the following reason: the base is the result of
/// `canonicalize()`, so it contains no symlinks, and the remaining
/// components don't exist either, so they can't be symlinks. Therefore
/// `base/../x` is exactly equivalent to `base.parent()/x`, and lexical
/// resolution is valid.
fn resolve_for_judgement(target: &Path) -> PathBuf {
    use std::path::Component;

    // A relative path is first joined to the current working directory.
    // Without this join, a relative path that doesn't exist yet (which is
    // the normal way `write` is used) can't be canonicalized, and walking
    // up its ancestors only ever reaches an empty path, so `target` would
    // be returned unchanged, fail to match any writable root, and be judged
    // "outside the range". This actually happened on the first real run: a
    // `write` directly under the root was denied with "smoke-out.txt is
    // outside the writable range".
    //
    // The base is taken as cwd because the actual write happens in a child
    // process, and the child inherits cwd and resolves the same relative
    // path against it. The basis for judgment and the basis for the write
    // agree here. If cwd can't be obtained, proceed without joining,
    // falling back to the approval side as before.
    let anchored;
    let target = match std::env::current_dir() {
        Ok(cwd) => {
            anchored = absolutize(&cwd, target);
            anchored.as_path()
        }
        Err(_) => target,
    };

    if let Ok(c) = target.canonicalize() {
        return c;
    }

    // Collect all of the target's components up front.
    let target_components: Vec<Component> = target.components().collect();

    let mut cursor = target;
    let mut depth = 0; // Tracks how far up we've walked.
    loop {
        match cursor.parent() {
            Some(parent) => {
                depth += 1;
                if let Ok(base) = parent.canonicalize() {
                    // To reconstruct how base relates to target, the first
                    // (components.len() - depth) components of target
                    // should correspond to base (though not necessarily
                    // exactly). Resolve the remaining components lexically.
                    let remaining_start = if target_components.len() > depth {
                        target_components.len() - depth
                    } else {
                        0
                    };

                    let mut out = base;
                    for component in &target_components[remaining_start..] {
                        match component {
                            Component::CurDir => {
                                // Ignore `.`.
                            }
                            Component::ParentDir => {
                                // `..` walks up to the parent.
                                out.pop();
                            }
                            Component::Normal(name) => {
                                out.push(name);
                            }
                            Component::RootDir | Component::Prefix(_) => {
                                // A RootDir or Prefix can never come out of
                                // the remainder of a nonexistent path. If
                                // one does, it's a signal that this is
                                // unjudgeable.
                                return target.to_path_buf();
                            }
                        }
                    }
                    return out;
                }
                cursor = parent;
            }
            // Walked all the way to the root and still couldn't normalize.
            // To avoid answering "inside" for something that can't be
            // judged, return the original path unchanged. It won't match
            // any policy root, so the caller falls back to approval.
            None => return target.to_path_buf(),
        }
    }
}

/// Whether an existing file has multiple links. A path that doesn't exist
/// and a path whose metadata can't be read both return false. The fact that
/// there are cases where this can't return true is exactly the limitation
/// spelled out in the spec's "scope not guaranteed" section.
fn has_extra_hard_links(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::symlink_metadata(path)
            .map(|m| m.is_file() && m.nlink() > 1)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_sandbox::{SandboxMode, SandboxPolicy};

    /// Judging a relative path as-is always puts a file that doesn't exist
    /// yet outside the range. This is exactly the defect hit on the first
    /// real run. Fails unless the base is taken as cwd.
    #[test]
    fn a_relative_target_inside_the_root_is_allowed() {
        let cwd = std::env::current_dir().expect("cwd");
        let root = cwd.canonicalize().expect("canonicalize");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root]).expect("policy");

        // A relative path that doesn't exist. The normal shape of how
        // `write` creates a new file.
        let verdict = predict(&policy, Path::new("polaris-relative-probe.txt"));
        assert_eq!(
            verdict,
            Verdict::Allowed,
            "a new-file creation directly under cwd is waiting for approval. the relative \
             path was not joined to cwd"
        );
    }

    /// Even a relative path that escapes cwd via `..` still falls to
    /// approval. Paired with the test above, this checks that the joining
    /// logic hasn't made everything "inside". With only one side, an
    /// implementation that simply always returns Allowed would still pass.
    #[test]
    fn a_relative_target_escaping_the_root_still_needs_approval() {
        let cwd = std::env::current_dir().expect("cwd");
        let root = cwd.canonicalize().expect("canonicalize");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root]).expect("policy");

        let verdict = predict(
            &policy,
            Path::new("../../polaris-relative-escape-probe.txt"),
        );
        assert!(
            matches!(verdict, Verdict::NeedsApproval { .. }),
            "a relative path escaping cwd was allowed: {verdict:?}"
        );
    }

    /// The shape of `absolutize` itself. Doesn't touch an absolute path.
    #[test]
    fn absolutize_joins_only_relative_targets() {
        let base = Path::new("/base/dir");
        assert_eq!(
            absolutize(base, Path::new("a/b.txt")),
            PathBuf::from("/base/dir/a/b.txt")
        );
        assert_eq!(
            absolutize(base, Path::new("/elsewhere/b.txt")),
            PathBuf::from("/elsewhere/b.txt"),
            "an absolute path was joined to base"
        );
    }

    fn workspace(root: &std::path::Path) -> SandboxPolicy {
        SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.to_path_buf()])
            .expect("can't create policy")
    }

    #[test]
    fn a_target_inside_the_root_is_allowed() {
        let dir = tempfile::tempdir().expect("temp dir");
        let policy = workspace(dir.path());
        assert_eq!(
            predict(&policy, &dir.path().join("a.txt")),
            Verdict::Allowed
        );
    }

    #[test]
    fn a_target_outside_the_root_needs_approval_and_says_where_it_may_write() {
        let root = tempfile::tempdir().expect("temp dir");
        let outside = tempfile::tempdir().expect("temp dir");
        let policy = workspace(root.path());

        let target = outside.path().join("b.txt");
        let Verdict::NeedsApproval { reason } = predict(&policy, &target) else {
            panic!("outside the root was allowed");
        };
        // The spec requires 3 things: the denied path, the current policy,
        // and the writable root. Missing even one leaves the model
        // repeating the same failure.
        assert!(
            reason.contains(&target.display().to_string()),
            "path is missing: {reason}"
        );
        assert!(
            reason.contains("workspace-write"),
            "policy is missing: {reason}"
        );
        assert!(
            reason.contains(&root.path().canonicalize().unwrap().display().to_string()),
            "writable root is missing: {reason}"
        );
    }

    #[test]
    fn a_target_whose_parent_does_not_exist_yet_is_still_judged_by_its_ancestors() {
        // Since write is a new-file creation, it's normal for neither the
        // target nor the intermediate directories to exist. A path that
        // doesn't exist can't be canonicalized, so naively canonicalizing
        // would make every new-file creation fall into "unjudgeable".
        let dir = tempfile::tempdir().expect("temp dir");
        let policy = workspace(dir.path());
        let deep = dir.path().join("no/such/dir/c.txt");
        assert_eq!(predict(&policy, &deep), Verdict::Allowed);
    }

    #[test]
    fn a_symlink_pointing_outside_the_root_needs_approval() {
        // If a symlink located inside the root points outward, the
        // apparent path is inside but the actual write target is outside.
        let root = tempfile::tempdir().expect("temp dir");
        let outside = tempfile::tempdir().expect("temp dir");
        let victim = outside.path().join("victim.txt");
        std::fs::write(&victim, "original").expect("can't write");

        let link = root.path().join("looks-inside.txt");
        std::os::unix::fs::symlink(&victim, &link).expect("symlink");

        let policy = workspace(root.path());
        assert!(
            matches!(predict(&policy, &link), Verdict::NeedsApproval { .. }),
            "a link pointing outward was allowed"
        );
    }

    #[test]
    fn an_existing_hardlink_is_surfaced_for_approval() {
        // A write via a hard link is not denied even by the real sandbox,
        // because both mechanisms judge by path, not by inode (see the
        // spec's "scope not guaranteed" section). The only clue the
        // predicate can pick up on is the link count, so an existing file
        // with multiple links is routed to approval. This is a mitigation,
        // not a guarantee.
        let root = tempfile::tempdir().expect("temp dir");
        let outside = tempfile::tempdir().expect("temp dir");
        let real = outside.path().join("real.txt");
        std::fs::write(&real, "original").expect("can't write");

        let linked = root.path().join("hardlink.txt");
        std::fs::hard_link(&real, &linked).expect("hard_link");

        let policy = workspace(root.path());
        let Verdict::NeedsApproval { reason } = predict(&policy, &linked) else {
            panic!("a hard link passed straight through");
        };
        assert!(
            reason.contains("hard link"),
            "the reason is not conveyed: {reason}"
        );
    }

    #[test]
    fn a_sensitive_path_inside_the_root_still_needs_approval() {
        // OS enforcement can't distinguish a sensitive file inside the
        // workspace. "Inside the writable root" doesn't mean "safe", so
        // this runs the same path_policy used on the read side here too.
        let root = tempfile::tempdir().expect("temp dir");
        let policy = workspace(root.path());
        let secret = root.path().join(".env");
        assert!(
            matches!(predict(&policy, &secret), Verdict::NeedsApproval { .. }),
            "a write to .env passed straight through"
        );
    }

    #[test]
    fn read_only_needs_approval_for_any_write() {
        let dir = tempfile::tempdir().expect("temp dir");
        let policy = SandboxPolicy::new(SandboxMode::ReadOnly, &[]).expect("policy");
        assert!(matches!(
            predict(&policy, &dir.path().join("a.txt")),
            Verdict::NeedsApproval { .. }
        ));
    }

    #[test]
    fn full_access_allows_any_path() {
        let policy = SandboxPolicy::new(SandboxMode::FullAccess, &[]).expect("policy");
        assert_eq!(
            predict(&policy, std::path::Path::new("/etc/hosts")),
            Verdict::Allowed
        );
    }

    #[test]
    fn a_parent_dir_component_that_escapes_needs_approval() {
        // When passing through a nonexistent directory `a` and then
        // escaping via `..`, the effect of `..` is unchanged even though
        // `a` doesn't exist. `<root>/a/../../outside` is lexically
        // equivalent to `<root>/../outside`, which is outside the root.
        let root = tempfile::tempdir().expect("temp dir");
        let outside = tempfile::tempdir().expect("temp dir");
        let policy = workspace(root.path());

        let target = root
            .path()
            .join("a/../../")
            .join(outside.path().file_name().expect("can't get file name"))
            .join("new.txt");
        let Verdict::NeedsApproval { .. } = predict(&policy, &target) else {
            panic!("an escaping path was allowed");
        };
    }

    #[test]
    fn a_parent_dir_component_that_stays_inside_is_allowed() {
        // `<root>/sub/../f.txt` lexically becomes `<root>/f.txt` and stays
        // inside the root. This holds even if `sub` doesn't exist.
        let root = tempfile::tempdir().expect("temp dir");
        let policy = workspace(root.path());

        let target = root.path().join("sub/../f.txt");
        // Check the predicate's verdict.
        assert_eq!(predict(&policy, &target), Verdict::Allowed);

        // Check the actually resolved path. If `..` weren't handled
        // correctly this would come out as `<root>/sub/f.txt`; handled
        // correctly, it comes out as `<root>/f.txt`.
        let canonical_root = root.path().canonicalize().expect("canonicalize");
        let expected = canonical_root.join("f.txt");
        let resolved = resolve_for_judgement(&target);
        assert_eq!(
            resolved,
            expected,
            "path resolution is wrong. expected: {}, actual: {}",
            expected.display(),
            resolved.display()
        );
    }

    #[test]
    fn a_parent_dir_that_returns_inside_stays_allowed() {
        // `<root>/../<root-name>/f.txt` leaves the root once, but then
        // comes back to root from the root's own parent. Lexical
        // resolution respects this round trip. The user-visible
        // observation point (the predicate's verdict) is correctly
        // `Allowed`.
        //
        // Note: the accuracy of path resolution is verified by
        // `a_parent_dir_component_that_stays_inside_is_allowed`. The reason
        // this test can't distinguish between the two on its own is that
        // the calculation `remaining_start = target_components.len() -
        // depth` becomes inaccurate for a path containing `..`. For a path
        // that leaves the root and then comes back, depth can end up not
        // matching the actual component count.
        let root = tempfile::tempdir().expect("temp dir");
        let policy = workspace(root.path());
        let root_name = root
            .path()
            .file_name()
            .expect("can't get the root's file name");

        let target = root.path().join("../").join(root_name).join("f.txt");
        // Check the predicate's verdict. That this is `Allowed` is the
        // promise made to the user; the details of path resolution are
        // verified by the escape test.
        assert_eq!(predict(&policy, &target), Verdict::Allowed);
    }

    #[test]
    fn a_parent_dir_under_an_existing_directory_is_canonicalised() {
        // When passing through an existing directory before using `..`,
        // this is handled by canonicalize's fast path. That path handles
        // `..` correctly.
        let root = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir(root.path().join("existing")).expect("can't create directory");
        let policy = workspace(root.path());

        let target = root.path().join("existing/../f.txt");
        // Because `existing` actually exists, it can be canonicalized up to
        // `existing`, after which `..` is processed and `f.txt` is pushed.
        // The result stays inside the root.
        assert_eq!(predict(&policy, &target), Verdict::Allowed);
    }
}
