//! Launching a process under confinement. Dispatches to a per-platform
//! implementation here.
//!
//! The returned `Outcome` is "the result of the child having run". When the
//! child could not be confined, this returns `SandboxError::NotEnforced`
//! instead of `Outcome`. Mixing the two would leave the caller unable to
//! detect that an unconfined child ran.
//!
//! Two design notes (added in fix round 1):
//!
//! - Stdin is written from a separate thread. If the parent thread
//!   synchronously finishes writing the child's stdin before it starts
//!   reading stdout via `wait_with_output`, then for a child that reads and
//!   writes back as it goes (`cat` and the like), both pipes fill up and
//!   each side deadlocks waiting for the other to drain. Even if the
//!   writer thread receives `BrokenPipe`, that just means the child exited
//!   before finishing reading its input, and is no reason to discard the
//!   child's real `Outcome` (exit code and output).
//! - Detecting "application failed" differs in mechanism by platform.
//!   macOS distinguishes it via `sandbox-exec`'s exit status
//!   (`classify_apply_failure`); Linux via a sentinel errno loaded onto the
//!   error the `pre_exec` closure returns (`classify_spawn_error`). See
//!   each function's comment for the details of both.
//!
//! What fix round 2 corrected: on the path where `wait_with_output` returns
//! an error, an early `?` return was used without joining the writer
//! thread's `JoinHandle`. Dropping it without joining leaves the thread
//! alive with no way to detect it (detached). Now `wait_with_output`'s
//! result is received into a variable without `?`, the writer thread is
//! always joined, and only after that does it decide what to return based
//! on that result.
//!
//! Known limitations (unresolved, deliberately left as-is):
//!
//! - Linux's sentinel errno can only carry to the parent "the fact that"
//!   `pre_exec` failed. The real error content `apply_to_current_process`
//!   returns (the kernel is too old / can't create the ruleset / the root
//!   has vanished, etc.) can't reach the parent, because of constraints in
//!   fork's notification path. The `NotEnforced` wording can say only that
//!   something happened, not why.
//! - What `classify_apply_failure` (macOS) can catch is only two known
//!   shapes: the `sandbox_apply` string, and SIGABRT with both outputs
//!   empty. If a shape exists where `sandbox-exec` silently fails to apply
//!   through some other path, this currently returns `None` (i.e.
//!   `Ok(Outcome)`) for it. This is coverage of what could be confirmed on
//!   real hardware, not a claim of completeness.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::SandboxError;
use crate::policy::SandboxPolicy;

#[derive(Debug)]
pub struct Outcome {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Launches `program` under confinement. If `stdin` is given, it's streamed
/// into the child's standard input.
pub fn run_confined(
    policy: &SandboxPolicy,
    program: &Path,
    args: &[String],
    stdin: Option<&str>,
) -> Result<Outcome, SandboxError> {
    let broker = crate::broker::from_environment()?;
    run_confined_with_broker(policy, program, args, stdin, broker.as_ref())
}

/// Executes through an explicitly supplied broker configuration when present.
/// Keeping this separate from `run_confined` makes the protocol testable
/// without changing process-global environment variables.
pub(crate) fn run_confined_with_broker(
    policy: &SandboxPolicy,
    program: &Path,
    args: &[String],
    stdin: Option<&str>,
    broker: Option<&crate::broker::BrokerConfig>,
) -> Result<Outcome, SandboxError> {
    if let Some(broker) = broker {
        return crate::broker::run(broker, policy, program, args, stdin);
    }

    let mut cmd = build_command(policy, program, args)?;

    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(classify_spawn_error)?;

    // Stdin is written from a separate thread, and the parent thread
    // devotes itself to draining stdout/stderr via wait_with_output.
    // Doing the writing and the draining sequentially on the same thread
    // deadlocks against a program that "reads and writes back as it goes"
    // (cat and the like).
    let writer = stdin.map(|s| {
        let mut child_stdin = child
            .stdin
            .take()
            .expect("stdin was opened with Stdio::piped(), so taking it should succeed");
        let payload = s.to_owned();
        std::thread::spawn(move || -> std::io::Result<()> {
            match child_stdin.write_all(payload.as_bytes()) {
                Ok(()) => Ok(()),
                // The child exiting before it finishes reading its input is
                // not a failure on this side. The child's real Outcome is
                // fetched separately by the wait_with_output side. Turning
                // this into an Err here would crush that real Outcome.
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
                Err(e) => Err(e),
            }
            // child_stdin is dropped here, closing the write end and
            // signaling EOF to the child.
        })
    });

    // wait_with_output internally drops self.stdin, but since it was
    // already take()n above when piped, the drop here is a no-op.
    //
    // The point is not using `?` here. If `wait_with_output` itself takes
    // the error-returning path (rare but possible) and does an early
    // return, the `writer`'s `JoinHandle` gets dropped before reaching the
    // join below, and the writer thread survives unjoined and undetectable
    // (detached). The result is received into a variable first, the writer
    // thread is always joined, and only then does it decide what to
    // return based on that result.
    let wait_result = child.wait_with_output();

    // Before leaving the call, always join the writer thread regardless of
    // whether wait_with_output succeeded or failed.
    let writer_join_result = writer.map(|handle| handle.join());

    let out = wait_result?;

    if let Some(join_result) = writer_join_result {
        match join_result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(SandboxError::Io(e)),
            Err(_) => {
                return Err(SandboxError::Io(std::io::Error::other(
                    "the thread writing stdin panicked",
                )));
            }
        }
    }

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    // Cut it off here if confinement couldn't be applied. Regardless of
    // whether the child ran, there's no guarantee that whatever ran was
    // actually confined.
    if let Some(detail) = classify_apply_failure(&out.status, &stdout, &stderr) {
        return Err(SandboxError::NotEnforced(detail));
    }

    Ok(Outcome {
        status: out.status.code().unwrap_or(-1),
        stdout,
        stderr,
    })
}

#[cfg(target_os = "macos")]
fn build_command(
    policy: &SandboxPolicy,
    program: &Path,
    args: &[String],
) -> Result<Command, SandboxError> {
    let mut cmd = Command::new(crate::macos::SANDBOX_EXEC);
    cmd.args(crate::macos::build_args(policy, program, args));
    Ok(cmd)
}

/// The sentinel errno signaling that applying confinement failed inside
/// pre_exec.
///
/// It's well beyond the range of Linux errno values that actually exist
/// (even counting the extended ones, up to roughly 133, around
/// `ERFKILL`/`EHWPOISON`), so it doesn't collide with a real exec-failure
/// errno. The value itself carries no meaning (it's just `b"pola"` turned
/// into a number).
///
/// Why this value was designed to be loaded via `from_raw_os_error`: as
/// verified in a `rust:1.96` Linux container, for an `io::Error` returned
/// by the `pre_exec` closure that doesn't carry a `raw_os_error()` (a
/// custom error built via `io::Error::other(msg)` and the like), the
/// message is lost entirely on the notification path from the forked
/// child to the parent (errno transfer over a self-pipe), and confirmed
/// that on the parent side it gets rounded down uniformly to `EINVAL`
/// (22, os error 22) regardless of the custom error's actual kind
/// (reproduced across all 5 kinds of custom error tried, including
/// `io::Error::other("...")`, and the same across 3 repeated runs). Since
/// EINVAL is also a real errno that execve itself can return, using it to
/// judge an application failure risks colliding with a genuine exec
/// failure. By contrast, the same verification confirmed that a value
/// loaded via `from_raw_os_error(n)` crosses to the parent unmodified
/// regardless of whether it's a real errno (200, 999, 65536, i32::MAX, and
/// -1 all round-tripped identically).
#[cfg(target_os = "linux")]
const SANDBOX_APPLY_FAILURE_ERRNO: i32 = 0x706f_6c61;

#[cfg(target_os = "linux")]
fn build_command(
    policy: &SandboxPolicy,
    program: &Path,
    args: &[String],
) -> Result<Command, SandboxError> {
    use std::os::unix::process::CommandExt;

    let mut cmd = Command::new(program);
    cmd.args(args);

    let policy_for_child = policy.clone();
    // SAFETY: pre_exec only runs in the child, after fork and before exec.
    // The restrict_self called here is per-thread and one-way, so it has
    // no effect on the parent's threads. The function called does involve
    // memory allocation, but since this child execs immediately afterward,
    // the window under async-signal-safety constraints is short, and the
    // landlock crate itself is designed for this usage.
    //
    // The error returned here can't preserve the original message, for the
    // reason the doc on `SANDBOX_APPLY_FAILURE_ERRNO` explains. Only the
    // sentinel errno is loaded onto it.
    unsafe {
        cmd.pre_exec(move || {
            crate::linux::apply_to_current_process(&policy_for_child)
                .map_err(|_| std::io::Error::from_raw_os_error(SANDBOX_APPLY_FAILURE_ERRNO))
        });
    }
    Ok(cmd)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn build_command(
    _policy: &SandboxPolicy,
    _program: &Path,
    _args: &[String],
) -> Result<Command, SandboxError> {
    // In an environment with no enforcement delegate, this doesn't launch
    // an unconfined child. "There's no sandbox, so run it through
    // unguarded" is not on the table in this design.
    Err(SandboxError::UnsupportedPlatform)
}

/// Splits the `io::Error` returned by `Command::spawn()` into a failure
/// from confinement not being applicable (`NotEnforced`) and any other
/// launch failure (`Io`; the binary is missing, etc.).
///
/// This branch only has meaning on Linux. On macOS, `sandbox-exec`'s
/// launch failure is an ordinary spawn failure — the binary itself is
/// missing, and so on — and whether application succeeded is judged by
/// `classify_apply_failure` from the exit status after the child has
/// actually run.
#[cfg(target_os = "linux")]
fn classify_spawn_error(e: std::io::Error) -> SandboxError {
    if e.raw_os_error() == Some(SANDBOX_APPLY_FAILURE_ERRNO) {
        SandboxError::NotEnforced(format!(
            "applying landlock failed inside pre_exec (detected via sentinel errno \
             {SANDBOX_APPLY_FAILURE_ERRNO}). The original error content is lost, since it \
             doesn't cross fork's notification path."
        ))
    } else {
        SandboxError::Io(e)
    }
}

#[cfg(not(target_os = "linux"))]
fn classify_spawn_error(e: std::io::Error) -> SandboxError {
    SandboxError::Io(e)
}

/// Detects, from the exit status and the output, that "applying the
/// sandbox itself failed".
///
/// This is a different event from a denial caused by a policy violation.
/// On macOS, this makes a two-part judgment:
///
/// 1. When `sandbox-exec` fails to apply the profile, it sometimes writes a
///    line containing `sandbox_apply:` to stderr. However, this is an
///    undocumented, English-hardcoded string, and it doesn't always appear
///    (see 2).
/// 2. Running a nested `sandbox-exec` under an outer policy that doesn't
///    allow `file-read*` was confirmed on real hardware, 3 out of 3 times,
///    to crash with `SIGABRT` (signal 6) — as a failure of the application
///    itself — with both stdout and stderr coming back empty
///    (`/usr/bin/sandbox-exec -f <an outer profile that disallows
///    file-read*> /usr/bin/sandbox-exec -p <the inner profile> -- ...`).
///    As a fallback for when the string match from 1 can't be relied on,
///    this also looks at the exit status itself.
///
/// The reason "both outputs are empty" is added as a condition, rather
/// than relying on SIGABRT alone, is to avoid misjudging a program that
/// legitimately aborts (for example, a binary built with panic=abort that
/// writes a message before crashing) as an application failure. An
/// ordinary failing command (the equivalent of `false`) exits normally,
/// leaving `ExitStatusExt::signal()` as `None`, so it's never touched by
/// this at all.
#[cfg(target_os = "macos")]
fn classify_apply_failure(
    status: &std::process::ExitStatus,
    stdout: &str,
    stderr: &str,
) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;

    if stderr.contains("sandbox_apply") {
        return Some(format!("sandbox-exec could not apply the policy: {stderr}"));
    }

    const SIGABRT: i32 = 6;
    if status.signal() == Some(SIGABRT) && stdout.is_empty() && stderr.is_empty() {
        return Some(
            "sandbox-exec exited with SIGABRT and produced no output at all. This matches the \
             known shape of a nested sandbox-exec failing to apply (application failure under an \
             outer policy that disallows file-read*)"
                .to_string(),
        );
    }

    None
}

/// On Linux, an application failure has already been caught by
/// `classify_spawn_error` (by the time execution reaches this function,
/// `pre_exec` has already succeeded, meaning landlock's `FullyEnforced` is
/// already confirmed). Whatever the child does from here on is the
/// child's own result, not a confinement failure.
#[cfg(not(target_os = "macos"))]
fn classify_apply_failure(
    _status: &std::process::ExitStatus,
    _stdout: &str,
    _stderr: &str,
) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{SandboxMode, SandboxPolicy};

    #[test]
    fn a_command_inside_the_root_succeeds_and_its_output_comes_back() {
        let root = tempfile::tempdir().expect("temp dir");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("policy");

        let target = policy.writable_roots()[0].join("inside.txt");
        let out = run_confined(
            &policy,
            std::path::Path::new("/bin/sh"),
            &["-c".into(), format!("echo ok > {}", target.display())],
            None,
        )
        .expect("confined execution itself failed");

        assert_eq!(out.status, 0, "the write to the inside failed: {out:?}");
        assert_eq!(
            std::fs::read_to_string(&target).expect("can't read").trim(),
            "ok"
        );
    }

    #[test]
    fn a_write_outside_the_root_is_denied_by_the_real_sandbox() {
        // The foundation for acceptance criterion 3. Without a mock,
        // actually attempt a write and observe the denial. Confirmation
        // through the tool is done separately by Task 8.
        let root = tempfile::tempdir().expect("temp dir");
        let outside = tempfile::tempdir().expect("temp dir");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("policy");

        let target = outside
            .path()
            .canonicalize()
            .expect("canonicalize")
            .join("nope.txt");
        let out = run_confined(
            &policy,
            std::path::Path::new("/bin/sh"),
            &["-c".into(), format!("echo pwned > {}", target.display())],
            None,
        )
        .expect("confined execution itself failed");

        assert_ne!(out.status, 0, "a write outside the root succeeded: {out:?}");
        assert!(
            !target.exists(),
            "the file was created: {}",
            target.display()
        );
    }

    /// That a redirect to `/dev/null` goes through under the real sandbox.
    ///
    /// A test that looks at the profile body (macOS) or the ruleset
    /// construction (Linux) as a string or a structure would let through a
    /// grant that is syntactically correct but doesn't actually take
    /// effect. This one runs a real `/bin/sh` under real confinement and
    /// checks, via the exit code, whether the redirect could be opened.
    ///
    /// Appending `&& echo SURVIVED` is the crux. If the redirect can't be
    /// opened, the shell crashes without ever executing the body, so
    /// without confirming via stdout whether the command actually ran,
    /// there's no way to distinguish a denial from "it ran but produced no
    /// output".
    #[test]
    fn a_redirect_to_dev_null_is_allowed_and_the_command_itself_still_runs() {
        let root = tempfile::tempdir().expect("temp dir");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("policy");

        let out = run_confined(
            &policy,
            std::path::Path::new("/bin/sh"),
            &["-c".into(), "echo hi > /dev/null && echo SURVIVED".into()],
            None,
        )
        .expect("confined execution itself failed");

        assert_eq!(
            out.status, 0,
            "`> /dev/null` was denied. A shell idiom reaches the model as a \
             policy denial: {out:?}"
        );
        assert_eq!(
            out.stdout.trim(),
            "SURVIVED",
            "the body did not run: {out:?}"
        );

        // Confirm the `2>` side goes through the same path too. There
        // shouldn't be a state where only one of them passes, but the
        // model actually writes both.
        let out = run_confined(
            &policy,
            std::path::Path::new("/bin/sh"),
            &["-c".into(), "echo hi 2>/dev/null && echo SURVIVED".into()],
            None,
        )
        .expect("confined execution itself failed");
        assert_eq!(out.status, 0, "`2>/dev/null` was denied: {out:?}");
    }

    /// `/dev/null` can be opened even under read-only. At the same time,
    /// within this same test, check as a pair that it hasn't diminished
    /// read-only's property — writing to an ordinary file stays denied. A
    /// test that checked only one side would miss the failure mode of
    /// "meant to open up `/dev/null` and accidentally opened up writing
    /// altogether".
    #[test]
    fn read_only_can_discard_output_but_still_cannot_write_a_real_file() {
        let scratch = tempfile::tempdir().expect("temp dir");
        let policy = SandboxPolicy::new(SandboxMode::ReadOnly, &[]).expect("policy");

        let out = run_confined(
            &policy,
            std::path::Path::new("/bin/sh"),
            &["-c".into(), "echo hi > /dev/null && echo SURVIVED".into()],
            None,
        )
        .expect("confined execution itself failed");
        assert_eq!(
            out.status, 0,
            "`> /dev/null` was denied under read-only: {out:?}"
        );
        assert_eq!(
            out.stdout.trim(),
            "SURVIVED",
            "the body did not run: {out:?}"
        );

        let target = scratch
            .path()
            .canonicalize()
            .expect("canonicalize")
            .join("should-not-exist.txt");
        let out = run_confined(
            &policy,
            std::path::Path::new("/bin/sh"),
            &["-c".into(), format!("echo pwned > {}", target.display())],
            None,
        )
        .expect("confined execution itself failed");

        assert_ne!(
            out.status, 0,
            "a write succeeded despite read-only: {out:?}"
        );
        assert!(
            !target.exists(),
            "the file was created: {}",
            target.display()
        );
    }

    #[test]
    fn stdin_reaches_the_child() {
        // The write / edit helper receives the operation via stdin. If
        // this doesn't pass, the mutation operation cannot happen at all.
        let root = tempfile::tempdir().expect("temp dir");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("policy");

        let out = run_confined(
            &policy,
            std::path::Path::new("/bin/cat"),
            &[],
            Some("abcde"),
        )
        .expect("confined execution itself failed");

        assert_eq!(out.status, 0, "{out:?}");
        assert_eq!(out.stdout.trim(), "abcde");
    }

    #[test]
    fn full_access_still_crosses_the_boundary() {
        // Even a non-restricting policy still launches a child. Branching
        // to direct execution here would make the tested path and the
        // production path different things.
        let policy = SandboxPolicy::new(SandboxMode::FullAccess, &[]).expect("policy");
        let out = run_confined(
            &policy,
            std::path::Path::new("/bin/sh"),
            &["-c".into(), "echo crossed".into()],
            None,
        )
        .expect("confined execution itself failed");
        assert_eq!(out.status, 0);
        assert_eq!(out.stdout.trim(), "crossed");
    }

    /// finding 1: 5MB far exceeds the OS pipe buffer (typically around
    /// 64KB). `/bin/cat` writes back to stdout as it reads, so an
    /// implementation where the parent synchronously finishes writing
    /// stdin before starting to read stdout leaves both pipes full,
    /// deadlocking forever with each side waiting for the other to drain.
    /// This is wrapped in a timeout, so that even if it does deadlock, the
    /// test process itself can still move on via `recv_timeout`.
    #[test]
    fn a_large_stdin_payload_does_not_deadlock() {
        let root = tempfile::tempdir().expect("temp dir");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("policy");
        let payload = "x".repeat(5 * 1024 * 1024);

        let (tx, rx) = std::sync::mpsc::channel();
        let payload_for_thread = payload.clone();
        std::thread::spawn(move || {
            let result = run_confined(
                &policy,
                std::path::Path::new("/bin/cat"),
                &[],
                Some(&payload_for_thread),
            );
            // It's fine if the receiver has already given up via timeout.
            let _ = tx.send(result);
        });

        let out = rx
            .recv_timeout(std::time::Duration::from_secs(15))
            .expect("run_confined hung (suspected deadlock)")
            .expect("confined execution itself failed");

        assert_eq!(out.status, 0, "{}", out.status);
        assert_eq!(
            out.stdout.len(),
            payload.len(),
            "the amount received differs"
        );
        assert_eq!(out.stdout, payload);
    }

    /// finding 2: If the child exits before finishing reading its input,
    /// the writer side can receive `BrokenPipe`. This is not a failure on
    /// the caller's side, and the child's real `Outcome` (exit code and
    /// output) must still reach the caller regardless.
    #[test]
    fn a_child_that_exits_without_reading_stdin_still_returns_its_real_outcome() {
        let root = tempfile::tempdir().expect("temp dir");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("policy");
        let payload = "x".repeat(5 * 1024 * 1024);

        let out = run_confined(
            &policy,
            std::path::Path::new("/bin/sh"),
            &["-c".into(), "sleep 0.2; exit 7".into()],
            Some(&payload),
        )
        .expect(
            "confined execution itself failed (BrokenPipe is being turned into an Err instead of an Outcome)",
        );

        assert_eq!(out.status, 7, "{out:?}");
    }

    /// Confirms a cross-cutting constraint: the equivalent of `false`
    /// (normal exit, no signal, no output) must never be touched by
    /// either the SIGABRT judgment finding 4 introduces below, or finding
    /// 3's sentinel errno. The bash tool routes real commands through
    /// here.
    #[test]
    fn an_ordinary_failing_command_stays_an_outcome_not_a_sandbox_failure() {
        let root = tempfile::tempdir().expect("temp dir");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("policy");

        let out = run_confined(&policy, std::path::Path::new("/usr/bin/false"), &[], None)
            .expect("an ordinary failing command was treated as a confinement failure");

        assert_ne!(out.status, 0, "{out:?}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_sandbox_that_failed_to_apply_is_an_error_not_a_denial() {
        // Returning an application failure as an Outcome would leave the
        // caller unable to distinguish it from a policy violation. If it
        // can't be distinguished, nobody notices that an unconfined child
        // ran. This tests classify_apply_failure directly. "Normal exit"
        // is obtained from a real process, rather than relying on
        // hand-assembling one via from_raw.
        let ordinary = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 1"])
            .status()
            .expect("can't launch");

        assert!(
            classify_apply_failure(
                &ordinary,
                "",
                "sandbox-exec: sandbox_apply: Operation not permitted"
            )
            .is_some(),
            "failed to detect an application failure"
        );
        assert!(
            classify_apply_failure(&ordinary, "", "sh: /nope: Operation not permitted").is_none(),
            "an ordinary denial was misjudged as an application failure"
        );
        assert!(classify_apply_failure(&ordinary, "", "").is_none());
    }

    /// finding 4: A real case where the `sandbox_apply` string can't be
    /// relied on. If the outer policy doesn't allow `file-read*`, a nested
    /// `sandbox-exec` crashes with SIGABRT (signal 6), and both stdout and
    /// stderr come back empty (reproduced 3/3 times on real hardware). A
    /// judgment based on string matching alone would let this pass
    /// straight through as `Ok(Outcome)`, falling toward the "dangerous
    /// direction" where nobody notices whether an unconfined child ran.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_nested_sandbox_apply_failure_with_empty_output_is_classified_via_signal() {
        let outer = tempfile::NamedTempFile::new().expect("temp file");
        std::fs::write(
            outer.path(),
            "(version 1)\n(deny default)\n(allow process-fork)\n(allow process-exec)\n",
        )
        .expect("can't write");

        let out = std::process::Command::new(crate::macos::SANDBOX_EXEC)
            .args([
                "-f",
                outer.path().to_str().expect("path"),
                crate::macos::SANDBOX_EXEC,
                "-p",
                "(version 1)(allow default)",
                "--",
                "/bin/echo",
                "hi",
            ])
            .output()
            .expect("the launch itself failed");

        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stdout.is_empty(), "unexpected stdout: {stdout}");
        assert!(
            stderr.is_empty(),
            "unexpected stderr: {stderr} (the very case for why string matching can't be relied on has broken down)"
        );

        assert!(
            classify_apply_failure(&out.status, &stdout, &stderr).is_some(),
            "failed to detect an application failure that exited with SIGABRT while stderr stayed empty"
        );
    }

    /// Confirms finding 4's precision: judging by SIGABRT alone risks
    /// misjudging even a program that legitimately aborts (and writes
    /// something out first) as an application failure. Confirms that when
    /// the output isn't empty, this doesn't judge it as an application
    /// failure even under SIGABRT.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_child_that_aborts_after_writing_output_is_not_misclassified_as_apply_failure() {
        let out = std::process::Command::new("/bin/sh")
            .args(["-c", "echo real-crash 1>&2; kill -ABRT $$"])
            .output()
            .expect("can't launch");

        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !stderr.is_empty(),
            "the premise doesn't hold: stderr was empty"
        );

        assert!(
            classify_apply_failure(&out.status, &stdout, &stderr).is_none(),
            "misjudged a genuine SIGABRT with output as an application failure"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_pre_exec_apply_failure_is_not_enforced_not_io() {
        // If the root vanishes after the policy is built, pre_exec's
        // apply_to_current_process fails for a genuine reason (PathFd::new's
        // ENOENT). Confirms that this returns NotEnforced rather than
        // SandboxError::Io. Leaving it as Io would make "confinement
        // couldn't be applied" indistinguishable from "the binary wasn't
        // found".
        let root = tempfile::tempdir().expect("temp dir");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("policy");
        std::fs::remove_dir_all(root.path()).expect("can't remove");

        let err = run_confined(
            &policy,
            std::path::Path::new("/bin/echo"),
            &["hi".to_string()],
            None,
        )
        .expect_err("confined execution succeeded despite the root having vanished");

        assert!(
            matches!(err, SandboxError::NotEnforced(_)),
            "the application failure came back as a different type than NotEnforced: {err:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_missing_binary_on_linux_is_io_not_not_enforced() {
        // The control case. A spawn failure when the binary doesn't exist
        // is not "confinement couldn't be applied". It must not collide
        // with the sentinel errno and turn into NotEnforced.
        let root = tempfile::tempdir().expect("temp dir");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("policy");

        let err = run_confined(
            &policy,
            std::path::Path::new("/definitely/does/not/exist/polaris-test-binary"),
            &[],
            None,
        )
        .expect_err("launching a nonexistent binary succeeded");

        assert!(
            matches!(err, SandboxError::Io(_)),
            "the failure for a nonexistent binary came back as a different type than Io: {err:?}"
        );
    }
}
