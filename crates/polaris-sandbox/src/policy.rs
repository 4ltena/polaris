//! Policy and writable roots. Roots are normalized at construction time.

use std::path::{Path, PathBuf};

use crate::SandboxError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    ReadOnly,
    WorkspaceWrite,
    FullAccess,
}

impl SandboxMode {
    /// The policy's name. The audit log and the denial message use the same spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            SandboxMode::ReadOnly => "read-only",
            SandboxMode::WorkspaceWrite => "workspace-write",
            SandboxMode::FullAccess => "full-access",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SandboxPolicy {
    mode: SandboxMode,
    writable_roots: Vec<PathBuf>,
}

impl SandboxPolicy {
    /// Roots are always normalized before being held. If a symlink appears
    /// partway through the path, the enforcement side judges the given path
    /// and the actual path to be different things and denies everything.
    /// macOS's `/tmp` and `/var` are exactly this case, so it isn't an edge
    /// case.
    pub fn new(mode: SandboxMode, roots: &[PathBuf]) -> Result<Self, SandboxError> {
        match mode {
            SandboxMode::ReadOnly if !roots.is_empty() => {
                return Err(SandboxError::NotEnforced(
                    "a writable root cannot be specified for read-only".into(),
                ));
            }
            SandboxMode::WorkspaceWrite if roots.is_empty() => {
                return Err(SandboxError::NotEnforced(
                    "workspace-write needs at least one writable root".into(),
                ));
            }
            SandboxMode::FullAccess if !roots.is_empty() => {
                return Err(SandboxError::NotEnforced(
                    "a writable root cannot be specified for full-access".into(),
                ));
            }
            _ => {}
        }

        // full-access doesn't hold onto roots. Holding them would let a value
        // that has no actual effect show up in the policy's description and
        // mislead the reader.
        let writable_roots = if mode == SandboxMode::FullAccess {
            Vec::new()
        } else {
            let mut canonical = Vec::with_capacity(roots.len());
            for r in roots {
                canonical.push(r.canonicalize()?);
            }
            canonical
        };

        Ok(Self {
            mode,
            writable_roots,
        })
    }

    pub fn mode(&self) -> SandboxMode {
        self.mode
    }

    pub fn writable_roots(&self) -> &[PathBuf] {
        &self.writable_roots
    }

    /// For denial messages. The spec requires including the denied path, the
    /// current policy, and the writable roots. This covers the latter two.
    pub fn describe(&self) -> String {
        if self.writable_roots.is_empty() {
            return self.mode.as_str().to_string();
        }
        let roots: Vec<String> = self
            .writable_roots
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        format!("{} (writable: {})", self.mode.as_str(), roots.join(", "))
    }

    /// Whether the normalized target path is inside any of the roots.
    /// `full-access` is always true, `read-only` is always false.
    pub fn contains(&self, canonical_target: &Path) -> bool {
        match self.mode {
            SandboxMode::FullAccess => true,
            SandboxMode::ReadOnly => false,
            SandboxMode::WorkspaceWrite => self
                .writable_roots
                .iter()
                .any(|r| canonical_target.starts_with(r)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_root_reached_through_a_symlink_is_stored_canonicalised() {
        // macOS's /tmp is a symlink to /private/tmp, and tempfile::tempdir()
        // is also created under it. Without normalization, the enforcement
        // side judges the given root and the actual path to not match and
        // falls back to "deny everything". Worse, that denial is
        // indistinguishable from a policy-violation denial, so an
        // acceptance-criteria test would pass for the wrong reason.
        let real = tempfile::tempdir().expect("temp dir");
        let link_parent = tempfile::tempdir().expect("temp dir");
        let link = link_parent.path().join("link-to-root");
        std::os::unix::fs::symlink(real.path(), &link).expect("symlink");

        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, std::slice::from_ref(&link))
            .expect("can't create policy");

        let stored = &policy.writable_roots()[0];
        assert_eq!(
            stored,
            &real.path().canonicalize().expect("canonicalize"),
            "the root was not normalized. The stored value {} is still the link",
            stored.display()
        );
        assert_ne!(stored, &link, "the link's path was stored verbatim");
    }

    #[test]
    fn workspace_write_requires_at_least_one_root() {
        // A workspace-write with zero roots means "can't write anywhere",
        // but that's indistinguishable from a caller's construction
        // omission. Don't silently accept a state that can't be told apart.
        let err = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[])
            .expect_err("zero roots went through");
        assert!(
            matches!(err, SandboxError::NotEnforced(_)),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn read_only_rejects_writable_roots() {
        // Letting a root be passed to read-only would make the policy's
        // name and its actual permissions disagree. That violates the
        // design goal of making the declaration and the enforcement the
        // same object.
        let dir = tempfile::tempdir().expect("temp dir");
        let err = SandboxPolicy::new(SandboxMode::ReadOnly, &[dir.path().to_path_buf()])
            .expect_err("a root went through for read-only");
        assert!(matches!(err, SandboxError::NotEnforced(_)), "{err}");
    }

    #[test]
    fn a_nonexistent_root_is_rejected_rather_than_silently_dropped() {
        // A nonexistent root can't be canonicalized. Silently dropping it
        // would mean nobody notices that a place they thought they could
        // write to has quietly gone missing.
        let missing = std::path::PathBuf::from("/definitely/not/here/polaris-test");
        let err = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[missing])
            .expect_err("a nonexistent root went through");
        assert!(matches!(err, SandboxError::Io(_)), "{err}");
    }

    #[test]
    fn full_access_needs_no_roots_and_keeps_none() {
        let policy =
            SandboxPolicy::new(SandboxMode::FullAccess, &[]).expect("can't create full-access");
        assert_eq!(policy.mode(), SandboxMode::FullAccess);
        assert!(policy.writable_roots().is_empty());
    }

    #[test]
    fn describe_names_the_mode_and_every_root() {
        // The denial message includes this string. Since the spec requires
        // "the denied path, the current policy, the writable roots", any
        // formatting that drops even one root would hand the model a wrong
        // map.
        let a = tempfile::tempdir().expect("temp dir");
        let b = tempfile::tempdir().expect("temp dir");
        let policy = SandboxPolicy::new(
            SandboxMode::WorkspaceWrite,
            &[a.path().to_path_buf(), b.path().to_path_buf()],
        )
        .expect("can't create policy");

        let s = policy.describe();
        assert!(s.contains("workspace-write"), "policy name is missing: {s}");
        for root in policy.writable_roots() {
            assert!(
                s.contains(&root.display().to_string()),
                "root {} is missing from the description: {s}",
                root.display()
            );
        }
    }

    #[test]
    fn full_access_rejects_writable_roots() {
        // Letting a root be passed to full-access would make the policy's
        // name and its actual permissions disagree. Reject it the same way
        // as read-only.
        let dir = tempfile::tempdir().expect("temp dir");
        let err = SandboxPolicy::new(SandboxMode::FullAccess, &[dir.path().to_path_buf()])
            .expect_err("a root went through for full-access");
        assert!(matches!(err, SandboxError::NotEnforced(_)), "{err}");
    }

    #[test]
    fn contains_includes_root_and_child_paths() {
        // The root itself, and child directories/files under it, are
        // contained. Sibling directories are not.
        let root = tempfile::tempdir().expect("temp dir");
        let root_path = root.path().canonicalize().expect("canonicalize");
        let policy = SandboxPolicy::new(
            SandboxMode::WorkspaceWrite,
            std::slice::from_ref(&root_path.clone()),
        )
        .expect("can't create policy");

        // The root itself is contained
        assert!(
            policy.contains(&root_path),
            "root itself should be contained"
        );

        // A child directory is contained
        let child = root_path.join("child");
        assert!(
            policy.contains(&child),
            "child directory should be contained"
        );

        // A sibling directory is not contained
        let sibling = root_path.parent().unwrap().join("sibling");
        assert!(
            !policy.contains(&sibling),
            "sibling directory should not be contained"
        );
    }

    #[test]
    fn contains_full_access_always_true() {
        // full-access returns true for any path.
        let policy =
            SandboxPolicy::new(SandboxMode::FullAccess, &[]).expect("can't create full-access");

        let arbitrary = Path::new("/arbitrary/path");
        assert!(
            policy.contains(arbitrary),
            "full-access should contain any path"
        );
    }

    #[test]
    fn contains_read_only_always_false() {
        // read-only returns false for any path.
        let policy =
            SandboxPolicy::new(SandboxMode::ReadOnly, &[]).expect("can't create read-only");

        let arbitrary = Path::new("/arbitrary/path");
        assert!(
            !policy.contains(arbitrary),
            "read-only should contain no paths"
        );
    }

    #[test]
    fn contains_resists_string_prefix_confusion() {
        // Path::starts_with compares by path component. <root>/work and
        // <root>/work-evil are different. This test guards against the
        // comparison being "simplified" into a plain string comparison.
        let root = tempfile::tempdir().expect("temp dir");
        let root_path = root.path().canonicalize().expect("canonicalize");

        // Actually create the work directory
        let work = root_path.join("work");
        std::fs::create_dir(&work).expect("create work dir");
        let work = work.canonicalize().expect("canonicalize work");

        // work-evil exists under the root, but isn't the root's path
        let work_evil = root_path.join("work-evil");

        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, std::slice::from_ref(&work))
            .expect("can't create policy");

        // Everything under work is contained
        assert!(policy.contains(&work), "work should be contained");
        let work_child = work.join("file.txt");
        assert!(
            policy.contains(&work_child),
            "work/child should be contained"
        );

        // work-evil is not contained
        assert!(
            !policy.contains(&work_evil),
            "work-evil should not be contained (string prefix confusion)"
        );
    }
}
