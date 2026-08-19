//! `edit` tool. Goes through the same confined path as `write`.

use std::path::Path;

use polaris_sandbox::{Mutation, SandboxPolicy};

use crate::ToolError;

pub fn edit(
    policy: &SandboxPolicy,
    helper: &Path,
    path: &Path,
    old: &str,
    new: &str,
) -> Result<String, ToolError> {
    let mutation = Mutation::Edit {
        path: path.to_path_buf(),
        old: old.to_string(),
        new: new.to_string(),
    };
    crate::write::run_mutation(policy, helper, &mutation, path)
}

#[cfg(test)]
mod tests {
    use polaris_sandbox::{SandboxMode, SandboxPolicy};

    use crate::edit::edit;

    /// Uses only `/bin/sh`, for the same reason as `write.rs`'s
    /// `success_helper`. A stand-in that just writes stdin straight to
    /// `target` and reports `edited`.
    fn success_helper(dir: &std::path::Path, target: &std::path::Path) -> std::path::PathBuf {
        let p = dir.join("fake-edit-helper-success");
        std::fs::write(
            &p,
            format!(
                "#!/bin/sh\nset -e\ncat > {}\necho edited\n",
                target.display()
            ),
        )
        .expect("can't write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        p
    }

    /// Discards stdin (reading it fully to avoid a deadlock) and just exits
    /// non-zero after writing a fixed reason to stderr. The actual
    /// match-count judgment (zero matches, multiple matches, how overlaps
    /// are counted) is already covered by the unit tests for
    /// `helper::apply` from Task 7. What we want to confirm here is only the
    /// wiring: that the child's stderr reaches the tool's error rather than
    /// being collapsed into a bare exit code.
    fn failure_helper(dir: &std::path::Path) -> std::path::PathBuf {
        let p = dir.join("fake-edit-helper-failure");
        std::fs::write(
            &p,
            "#!/bin/sh\ncat > /dev/null\necho 'replacement target matched in 2 places' 1>&2\nexit 1\n",
        )
        .expect("can't write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        p
    }

    #[test]
    fn an_edit_inside_the_root_succeeds_through_the_confined_helper() {
        let root = tempfile::tempdir().expect("temp dir");
        let helper_dir = tempfile::tempdir().expect("temp dir");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("policy");

        let target = policy.writable_roots()[0].join("f.txt");
        let helper = success_helper(helper_dir.path(), &target);

        let msg = edit(&policy, &helper, &target, "xxx", "yyy").expect("failed");

        // As on the write side, the stand-in helper doesn't parse JSON, so
        // what ends up in the file isn't the replacement result but the
        // serialized mutation that rode in on stdin. Confirm that both old
        // and new reached the child.
        let written = std::fs::read_to_string(&target).expect("can't read");
        assert!(
            written.contains("xxx"),
            "old did not reach the child: {written}"
        );
        assert!(
            written.contains("yyy"),
            "new did not reach the child: {written}"
        );
        assert!(!msg.trim().is_empty(), "the result description is empty");
    }

    #[test]
    fn a_failing_edit_returns_the_child_s_reason_rather_than_a_bare_exit_code() {
        // Returning just "exit 1" leaves the model unable to tell what to
        // fix, so it repeats the same failure — wasting a round trip and
        // tokens.
        let root = tempfile::tempdir().expect("temp dir");
        let helper_dir = tempfile::tempdir().expect("temp dir");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("policy");
        let helper = failure_helper(helper_dir.path());

        let target = policy.writable_roots()[0].join("f.txt");

        let err = edit(&policy, &helper, &target, "xxx", "yyy")
            .expect_err("the failure helper succeeded");
        assert!(
            err.to_string().contains("2 places"),
            "the reason is not conveyed: {err}"
        );
    }
}
