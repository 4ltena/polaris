//! Linux enforcement. Applies a landlock ruleset to the process itself,
//! from inside the child.
//!
//! `restrict_self()` is per-thread and one-way, inherited by threads and
//! children created after the call. Calling it from the running harness's
//! own body would permanently restrict that thread, dragging all of its
//! subsequent work down with it. The only place it's called is inside
//! `Command::pre_exec`.

use crate::SandboxError;
use crate::policy::{SandboxMode, SandboxPolicy};

/// The practical floor. ABI 1 (kernel 5.13) can't express a rename or link
/// that crosses directories, so it can't handle the "write to a temp file,
/// then replace via rename" save pattern that editors and many tools use.
/// ABI 2 (5.19) is taken as the floor.
const REQUIRED_ABI: landlock::ABI = landlock::ABI::V2;

/// Applies the policy to the current process (i.e. the already-forked
/// child).
///
/// `full-access` applies nothing at all, because not restricting is the
/// policy. The caller still launches the child even for `full-access`,
/// though — so as to keep the tested path and the production path
/// identical.
pub fn apply_to_current_process(policy: &SandboxPolicy) -> Result<(), SandboxError> {
    use landlock::{
        Access, AccessFs, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr,
        RulesetCreatedAttr, RulesetStatus,
    };

    if policy.mode() == SandboxMode::FullAccess {
        return Ok(());
    }

    // Below ABI V2 (kernel below 5.19), there's no `AccessFs::Refer`
    // (rename/link crossing directories), and with the default best-effort
    // this one item alone gets silently dropped, falling back to
    // `PartiallyEnforced`. This makes just this one `HardRequirement`, so
    // that on a kernel that can't satisfy even part of the request,
    // `handle_access` fails immediately, right there. The error carries,
    // from the landlock side, which right is missing, so that's folded
    // straight into the report.
    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(REQUIRED_ABI))
        .map_err(|e| {
            SandboxError::NotEnforced(format!(
                "the kernel does not satisfy the rights required by landlock {REQUIRED_ABI:?}: {e}"
            ))
        })?
        .set_compatibility(CompatLevel::BestEffort)
        .create()
        .map_err(|e| SandboxError::NotEnforced(format!("can't create the ruleset: {e}")))?;

    // Reads are allowed across the board. The difference between read-only
    // and workspace-write lies only on the write side.
    ruleset = ruleset
        .add_rule(PathBeneath::new(
            PathFd::new("/")
                .map_err(|e| SandboxError::NotEnforced(format!("can't open /: {e}")))?,
            AccessFs::from_read(REQUIRED_ABI),
        ))
        .map_err(|e| SandboxError::NotEnforced(format!("can't add the read rule: {e}")))?;

    // Opens up writing to `/dev/null` only. Same reason as the macOS side
    // (see the long comment in `macos::build_profile`). With landlock too,
    // `cmd > /dev/null` becomes
    // `/bin/sh: 1: cannot create /dev/null: Permission denied`, and the
    // command body never runs once the redirection can't be opened.
    //
    // The right granted is `AccessFs::WriteFile` alone. What `>` requires is
    // opening an existing file for writing, and this is the single right
    // that governs that (`Truncate`, which governs `O_TRUNC`, is an ABI V3
    // right, and this ruleset only handles V2 rights, so it doesn't come
    // into play). Passing `AccessFs::from_all` would open up unlink
    // (`RemoveFile`) and creating new entries in the same location as well,
    // so that isn't used. The rule's target is the single file `/dev/null`;
    // even though `PathBeneath` is used, it isn't a directory, so this
    // doesn't spill over to other nodes under `/dev`.
    //
    // Adding this for read-only too is for the same reason as macOS. What's
    // written is discarded, and the filesystem's state doesn't change.
    ruleset = ruleset
        .add_rule(PathBeneath::new(
            PathFd::new("/dev/null")
                .map_err(|e| SandboxError::NotEnforced(format!("can't open /dev/null: {e}")))?,
            AccessFs::WriteFile,
        ))
        .map_err(|e| SandboxError::NotEnforced(format!("can't add the /dev/null rule: {e}")))?;

    for root in policy.writable_roots() {
        ruleset = ruleset
            .add_rule(PathBeneath::new(
                PathFd::new(root).map_err(|e| {
                    SandboxError::NotEnforced(format!("can't open {}: {e}", root.display()))
                })?,
                AccessFs::from_all(REQUIRED_ABI),
            ))
            .map_err(|e| SandboxError::NotEnforced(format!("can't add the write rule: {e}")))?;
    }

    let status = ruleset
        .restrict_self()
        .map_err(|e| SandboxError::NotEnforced(format!("restrict_self failed: {e}")))?;

    // Neither the state where nothing was applied, nor the state where only
    // part was applied, is a denial. Letting this through would make a
    // state that isn't protected (or is only partly protected) look
    // identical to a state that's fully protected. `HardRequirement` catches
    // an ABI-side shortfall at `handle_access` time, but to also close off
    // the possibility of falling into `PartiallyEnforced` for some other
    // reason (for example, a path where `restrict_self` itself is blocked
    // by seccomp and only partially takes effect), this rejects anything
    // other than `FullyEnforced` outright.
    if status.ruleset != RulesetStatus::FullyEnforced {
        return Err(SandboxError::NotEnforced(format!(
            "the kernel did not fully enforce landlock {REQUIRED_ABI:?} ({:?}). \
             either the kernel is too old, seccomp is blocking it, or the feature is only partially available",
            status.ruleset
        )));
    }

    Ok(())
}

#[cfg(test)]
mod linux_enforcement {
    use crate::policy::{SandboxMode, SandboxPolicy};

    /// Launches `/bin/sh -c script` under the policy (a child that has been
    /// run through `apply_to_current_process` after fork but before exec),
    /// and returns the exit status. If the application itself fails
    /// (`pre_exec` returns `Err`), that comes back to the parent as
    /// `Command`'s own launch failure, so this panics here. For each test's
    /// diagnosis, "could it write / could it not" is the real subject
    /// rather than a launch failure, so a launch failure is treated as
    /// breakage unrelated to what the test intends.
    fn run_under_policy(policy: &SandboxPolicy, script: &str) -> std::process::ExitStatus {
        let policy_for_child = policy.clone();
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.arg("-c").arg(script);
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(move || {
                crate::linux::apply_to_current_process(&policy_for_child)
                    .map_err(|e| std::io::Error::other(e.to_string()))
            });
        }
        cmd.status().expect("can't launch")
    }

    #[test]
    fn a_write_outside_the_root_is_denied_by_the_real_kernel() {
        let root = tempfile::tempdir().expect("temp dir");
        let outside = tempfile::tempdir().expect("temp dir");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("policy");

        let target = outside.path().join("should-not-exist.txt");
        let status = run_under_policy(&policy, &format!("echo pwned > {}", target.display()));

        assert!(!status.success(), "a write outside the root succeeded");
        assert!(
            !target.exists(),
            "the file was created: {}",
            target.display()
        );
    }

    #[test]
    fn a_write_inside_the_writable_root_succeeds() {
        // A test that only checks denial outside the root misses the
        // "deny everything" failure mode (even deleting the write rule
        // entirely, or weakening it to read-access, would let this test
        // keep passing). Confirm as a pair that a write inside the root
        // actually goes through, making "denies everything" detectable.
        let root = tempfile::tempdir().expect("temp dir");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("policy");

        let target = root.path().join("should-exist.txt");
        let status = run_under_policy(&policy, &format!("echo ok > {}", target.display()));

        assert!(status.success(), "a write inside the root was denied");
        assert!(
            target.exists(),
            "the file was not created: {}",
            target.display()
        );
        let content = std::fs::read_to_string(&target)
            .expect("can't read a file that should have been writable");
        assert_eq!(
            content.trim(),
            "ok",
            "what was written and what was read differ"
        );
    }

    #[test]
    fn read_only_denies_any_write() {
        // read-only can't hold even one writable root (Task 1's
        // constraint). What this checks is that "can't write anywhere" in
        // that state is actually reflected on the enforcement side too.
        let scratch = tempfile::tempdir().expect("temp dir");
        let policy = SandboxPolicy::new(SandboxMode::ReadOnly, &[]).expect("policy");

        let target = scratch.path().join("should-not-exist.txt");
        let status = run_under_policy(&policy, &format!("echo pwned > {}", target.display()));

        assert!(!status.success(), "a write succeeded despite read-only");
        assert!(
            !target.exists(),
            "the file was created: {}",
            target.display()
        );
    }
}
