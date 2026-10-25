//! `bash` tool. Runs a command inside a confined `/bin/sh`.
//!
//! It does not go through the predicate. Because it can execute arbitrary
//! code, what it will touch cannot be determined ahead of time. Attempt it,
//! and if it is refused, return the reason to the model.

use std::path::Path;

use polaris_sandbox::{SandboxPolicy, run_confined};

use crate::ToolError;

/// Cap on the output returned. `grep` and `find` pass through here, so large
/// output is the normal case, not an exception.
pub const MAX_OUTPUT_BYTES: usize = 32 * 1024;

pub fn run(policy: &SandboxPolicy, command: &str) -> Result<String, ToolError> {
    let outcome = run_confined(
        policy,
        Path::new("/bin/sh"),
        &["-c".to_string(), command.to_string()],
        None,
    )?;

    let combined = if outcome.stderr.trim().is_empty() {
        outcome.stdout
    } else {
        format!("{}{}", outcome.stdout, outcome.stderr)
    };
    let body = truncate(&combined);

    if outcome.status == 0 {
        return Ok(body);
    }

    Err(ToolError::CommandFailed {
        status: outcome.status,
        policy: policy.describe(),
        detail: body,
    })
}

/// A note naming the program that exists, when the one asked for does not.
///
/// A shell says `python: command not found` and stops there, and the caller
/// spends a round trip discovering the name is `python3` — 4 of 12 recorded
/// runs of the repair task did exactly that, at $0.0152 a round trip. The
/// note costs about twelve tokens, so the arithmetic was obvious.
///
/// **It measured worse.** n=12 each: $0.1433 against $0.1322, with requests
/// rising from 12.8 to 13.9 and the spread widening from $0.0136 to $0.0334.
/// Telling the model about `python3` sent it somewhere it had not been going.
/// Left here, unused, with its numbers: the arithmetic is still right and the
/// measurement still says no, which is the only pair of facts worth keeping.
///
/// Only an exact prefix match on a real entry in PATH counts. A guess that
/// names something that does not exist would cost the round trip it was
/// meant to save, and then some.
#[cfg_attr(not(test), allow(dead_code))]
fn near_miss(body: &str) -> String {
    let Some(missing) = body
        .lines()
        .find_map(|line| {
            line.split(": command not found")
                .next()
                .filter(|_| line.contains(": command not found"))
        })
        .and_then(|head| head.rsplit(':').next())
        .map(str::trim)
        .filter(|name| {
            !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        })
    else {
        return String::new();
    };
    let Some(path) = std::env::var_os("PATH") else {
        return String::new();
    };
    let found: Vec<String> = std::env::split_paths(&path)
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flatten()
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with(missing) && name != missing)
        .take(4)
        .collect();
    if found.is_empty() {
        return String::new();
    }
    let mut named: Vec<String> = found;
    named.sort();
    named.dedup();
    format!("\n{missing} is missing, but {} exists.", named.join("、"))
}

/// When the output exceeds the cap, cut it at a character boundary and state
/// in the body that it was truncated. An unmarked partial result is a wrong
/// answer presented as a complete one.
fn truncate(s: &str) -> String {
    if s.len() <= MAX_OUTPUT_BYTES {
        return s.to_string();
    }
    let mut end = MAX_OUTPUT_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[output exceeded {} bytes and was truncated here. Re-run with a narrower scope if you need more.]",
        &s[..end],
        MAX_OUTPUT_BYTES
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_sandbox::{SandboxMode, SandboxPolicy};

    fn workspace(root: &std::path::Path) -> SandboxPolicy {
        SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.to_path_buf()]).expect("policy")
    }

    #[test]
    fn stdout_comes_back() {
        let dir = tempfile::tempdir().expect("temp dir");
        let out = run(&workspace(dir.path()), "echo hello").expect("failed");
        assert!(out.contains("hello"), "{out}");
    }

    #[test]
    fn a_failing_command_returns_its_stderr_and_its_exit_code() {
        // Returning only the exit code leaves the model unable to tell what
        // went wrong, so it repeats the same command.
        let dir = tempfile::tempdir().expect("temp dir");
        let err = run(&workspace(dir.path()), "echo failure reason >&2; exit 3")
            .expect_err("failure returned as success");
        let msg = err.to_string();
        assert!(msg.contains("failure reason"), "stderr is missing: {msg}");
        assert!(msg.contains('3'), "exit code is missing: {msg}");
    }

    #[test]
    fn a_write_outside_the_root_is_denied_and_the_message_names_the_policy() {
        let root = tempfile::tempdir().expect("temp dir");
        let outside = tempfile::tempdir().expect("temp dir");
        let target = outside
            .path()
            .canonicalize()
            .expect("canonicalize")
            .join("pwned.txt");

        let err = run(
            &workspace(root.path()),
            &format!("echo pwned > {}", target.display()),
        )
        .expect_err("write outside the root succeeded");

        assert!(!target.exists(), "the file was created");
        assert!(
            err.to_string().contains("workspace-write"),
            "policy is not conveyed: {err}"
        );
    }

    #[test]
    fn a_write_inside_the_root_succeeds() {
        let root = tempfile::tempdir().expect("temp dir");
        let policy = workspace(root.path());
        let target = policy.writable_roots()[0].join("ok.txt");
        run(&policy, &format!("echo ok > {}", target.display())).expect("write inside failed");
        assert!(target.exists());
    }

    #[test]
    fn oversized_output_is_truncated_and_says_so() {
        // Exercise the cap itself. A test that only checks "it wasn't
        // truncated" with short input would keep passing no matter how many
        // digits the cap changed by.
        let dir = tempfile::tempdir().expect("temp dir");
        let out = run(
            &workspace(dir.path()),
            &format!("head -c {} /dev/zero | tr '\\0' 'a'", MAX_OUTPUT_BYTES * 2),
        )
        .expect("failed");

        assert!(out.len() < MAX_OUTPUT_BYTES * 2, "not truncated");
        assert!(
            out.contains("truncated"),
            "the body doesn't say it was truncated"
        );
    }

    #[test]
    fn truncation_lands_on_a_character_boundary_of_the_input() {
        // The original assertion was `out.is_char_boundary(out.len())`. That
        // is true for any Rust `String` (`len()` is always a char boundary),
        // so it checked nothing. What actually made it pass was a side
        // effect: cutting without backing off to a boundary makes
        // `&s[..end]` panic. An implementation rewritten to avoid the panic
        // (e.g. returning empty from `get()`) or one that backs off to the
        // wrong boundary would still sail through this test even though it
        // claims to check the "boundary".
        //
        // Match the shape used on the audit-log side (the test of the same
        // name in `polaris_core::audit`). Check that the truncated body is a
        // prefix of the input, that its cut point lands on a character
        // boundary of the input, and that it comes right up to just short of
        // the cap. This targets the pure function directly rather than a
        // real command because this is a property of `truncate` itself, not
        // of the child process (the path through the child is covered
        // separately by `oversized_output_is_truncated_and_says_so`).
        //
        // "€" is 3 bytes. MAX_OUTPUT_BYTES is not a multiple of 3, so
        // cutting naively at byte MAX_OUTPUT_BYTES always lands
        // mid-character.
        let input = "€".repeat(MAX_OUTPUT_BYTES / 3 + 10);
        assert!(
            input.len() > MAX_OUTPUT_BYTES,
            "premise broken: does not exceed the cap"
        );
        assert!(
            !input.is_char_boundary(MAX_OUTPUT_BYTES),
            "premise broken: the cap lands exactly on a character boundary, so backing off would have no effect"
        );

        let out = truncate(&input);
        // The marker starts on the line after the body. The input has no
        // newline, so the first line is the body.
        let body = out.split('\n').next().expect("body is missing");

        assert!(
            input.starts_with(body),
            "the truncated body is not a prefix of the input (a different string was returned)"
        );
        assert!(
            input.is_char_boundary(body.len()),
            "the cut point does not land on a character boundary of the input: byte {}",
            body.len()
        );
        assert!(
            body.len() <= MAX_OUTPUT_BYTES,
            "returned more than the cap: {} bytes",
            body.len()
        );
        // A UTF-8 character is at most 4 bytes, so backing off to a boundary
        // moves at most 3 bytes. An implementation that cuts well short of
        // this (e.g. returning an empty string) fails here.
        assert!(
            body.len() > MAX_OUTPUT_BYTES - 4,
            "cutting well short of the cap: {} bytes",
            body.len()
        );
        assert!(
            out.contains("truncated"),
            "the body doesn't say it was truncated"
        );
    }
}

#[cfg(test)]
mod near_miss_tests {
    use super::near_miss;

    /// The shell says a name does not exist; this says which one does.
    /// Without it the caller spends a round trip finding out — measured at
    /// 4 of 12 runs of one task, at $0.0152 a round trip.
    #[test]
    fn a_missing_name_is_answered_with_the_one_that_exists() {
        let said = near_miss("/bin/sh: python: command not found");
        assert!(
            said.contains("python3"),
            "python3 is on PATH here: {said:?}"
        );
        assert!(said.contains("python is missing"));
    }

    /// Nothing is said unless a real entry on PATH starts with the missing
    /// name. A guess that names something absent costs the round trip it
    /// was meant to save.
    #[test]
    fn nothing_is_said_when_there_is_no_real_near_miss() {
        assert_eq!(near_miss("/bin/sh: zqxjkv: command not found"), "");
        assert_eq!(near_miss("some ordinary failure"), "");
        // The name itself existing is not a near miss.
        assert_eq!(near_miss("/bin/sh: : command not found"), "");
    }

    /// The name is taken from the shell's own words, and anything that is
    /// not a plain program name is left alone rather than shell-globbed.
    #[test]
    fn only_a_plain_program_name_is_looked_up() {
        assert_eq!(near_miss("/bin/sh: ../../etc/x: command not found"), "");
        assert_eq!(near_miss("/bin/sh: py thon: command not found"), "");
    }
}
