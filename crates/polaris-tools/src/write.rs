//! `write` tool. The actual write happens inside a confined child process.
//!
//! `std::fs::write` isn't called in-process because OS enforcement only
//! takes effect at a process boundary. Writing in-process would leave only
//! this crate's path judgment as protection, and any error in that
//! judgment becomes a write outside the range, directly.

use std::path::Path;

use polaris_sandbox::{Mutation, SandboxPolicy, run_confined};

use crate::ToolError;

pub fn write(
    policy: &SandboxPolicy,
    helper: &Path,
    path: &Path,
    content: &str,
) -> Result<String, ToolError> {
    let mutation = Mutation::Write {
        path: path.to_path_buf(),
        content: content.to_string(),
    };
    run_mutation(policy, helper, &mutation, path)
}

/// The launch and result interpretation shared by `write` and `edit`.
pub(crate) fn run_mutation(
    policy: &SandboxPolicy,
    helper: &Path,
    mutation: &Mutation,
    path: &Path,
) -> Result<String, ToolError> {
    let payload = serde_json::to_string(mutation).map_err(|e| {
        ToolError::Io(std::io::Error::other(format!(
            "can't serialize the operation: {e}"
        )))
    })?;

    let outcome = run_confined(
        policy,
        helper,
        &["--confined-apply".to_string()],
        Some(&payload),
    )?;

    if outcome.status == 0 {
        return Ok(outcome.stdout.trim().to_string());
    }

    // Labeling every non-zero exit as "the policy denied it" would deliver
    // even an ordinary failure — like an `edit` whose marker doesn't match —
    // to the model as a sandbox denial. The model would start doubting the
    // policy and searching elsewhere, throwing away a round trip that a
    // changed marker alone would have fixed — exactly the waste the denial
    // message exists to prevent.
    //
    // The material needed to tell the two apart exists only on the child's
    // side (errno doesn't cross the process boundary), so the judgment is
    // made by `helper::apply`, and only the conclusion arrives as a marker
    // on stderr. Without the marker, this is treated as a denial as before.
    // A helper that fails to launch, or any unanticipated shape of failure,
    // falls through to this side, so the conservative default doesn't
    // change.
    if let Some(reason) = polaris_sandbox::helper::request_problem(&outcome.stderr) {
        return Err(ToolError::MutationFailed {
            path: path.display().to_string(),
            detail: reason.to_string(),
        });
    }

    Err(ToolError::WriteDenied {
        path: path.display().to_string(),
        policy: policy.describe(),
        detail: outcome.stderr.trim().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use polaris_sandbox::{SandboxMode, SandboxPolicy};

    use crate::write::write;

    /// A minimal stand-in helper that just writes stdin straight to
    /// `target`.
    ///
    /// This differs from the real helper (which takes `--confined-apply`
    /// and interprets JSON as a `Mutation`). What we want to confirm here
    /// is three things about the tool layer: that it puts the mutation on
    /// stdin, that it launches as a confined child, and how it interprets
    /// the child's exit status and stdout/stderr. The JSON semantics
    /// (match-count judgment, etc.) are already covered by the unit tests
    /// for `helper::apply` from Task 7. Bringing python3 into this would
    /// mean, on this machine, `/usr/bin/python3` doesn't exist and
    /// resolves to a user-installed 3.11 under `/Library/Frameworks`,
    /// saddling a test whose subject is the sandbox with an unrelated
    /// dependency on where the interpreter happens to live. `/bin/sh` and
    /// `cat` alone are enough.
    ///
    /// `set -e` is required. A redirection failure (a sandbox denial) is
    /// detected by the shell before `cat` itself launches, but on its own
    /// that is treated as just one non-special builtin command failing,
    /// and the script proceeds to the next line, `echo wrote`. That would
    /// turn what should have been a denied attempt into "succeeded and
    /// said wrote", letting a test meant to verify the acceptance criteria
    /// pass for the wrong reason. `set -e` makes the denial propagate all
    /// the way through as the script's own non-zero exit.
    fn success_helper(dir: &std::path::Path, target: &std::path::Path) -> std::path::PathBuf {
        let p = dir.join("fake-helper-success");
        std::fs::write(
            &p,
            format!(
                "#!/bin/sh\nset -e\ncat > {}\necho wrote\n",
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

    #[test]
    fn a_write_inside_the_root_succeeds_through_the_confined_helper() {
        let root = tempfile::tempdir().expect("temp dir");
        let helper_dir = tempfile::tempdir().expect("temp dir");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("policy");

        let target = policy.writable_roots()[0].join("out.txt");
        let helper = success_helper(helper_dir.path(), &target);

        let msg = write(&policy, &helper, &target, "hello").expect("failed");

        // The stand-in helper doesn't parse JSON, so what ends up in the
        // file isn't `content` itself but the serialized mutation that rode
        // in on stdin. What we want to see is the wiring — that it really
        // rode in on stdin and reached the child — not the helper's
        // semantics (per the brief's correction, a weak assertion is
        // enough).
        let written = std::fs::read_to_string(&target).expect("can't read");
        assert!(
            written.contains("hello"),
            "the content did not ride on stdin and reach the child: {written}"
        );
        assert!(!msg.trim().is_empty(), "the result description is empty");
    }

    #[test]
    fn a_write_outside_the_root_is_denied_by_the_real_sandbox_through_the_tool() {
        // Acceptance criterion 3. Without a mock, actually attempt a write
        // through the `write` tool and observe the denial. A path that
        // hits the shared launch helper (run_confined) directly does not
        // satisfy the criterion.
        let root = tempfile::tempdir().expect("temp dir");
        let outside = tempfile::tempdir().expect("temp dir");
        let helper_dir = tempfile::tempdir().expect("temp dir");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("policy");

        let target = outside
            .path()
            .canonicalize()
            .expect("canonicalize")
            .join("pwned.txt");
        let helper = success_helper(helper_dir.path(), &target);

        let err = write(&policy, &helper, &target, "hello")
            .expect_err("a write outside the root succeeded");

        assert!(
            !target.exists(),
            "the file was created: {}",
            target.display()
        );
        // The denial message includes the denied path, the policy, and the
        // writable root.
        let msg = err.to_string();
        assert!(
            msg.contains(&target.display().to_string()),
            "path is missing: {msg}"
        );
        assert!(msg.contains("workspace-write"), "policy is missing: {msg}");
    }

    #[test]
    fn a_helper_that_cannot_be_confined_is_an_error_not_a_silent_success() {
        // Don't allow a state where a write succeeds despite failing to be
        // confined. On macOS, what actually launches is
        // `/usr/bin/sandbox-exec`, and a helper that doesn't exist shows up
        // as an exec failure inside the confined child. So `run_confined`
        // itself returns not an `Err` but an `Ok(Outcome)` with a non-zero
        // `status`, and `write`'s ordinary denial path — which interprets
        // that as a non-zero exit — catches it here (observed:
        // `sandbox-exec: execvp() ... failed: No such file or directory`
        // lands in `detail`).
        let root = tempfile::tempdir().expect("temp dir");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("policy");
        let missing = root.path().join("no-such-helper");
        let target = policy.writable_roots()[0].join("x.txt");

        assert!(write(&policy, &missing, &target, "hello").is_err());
        assert!(!target.exists());
    }
}
