//! macOS enforcement. Builds a Seatbelt profile at runtime and launches the
//! child by passing it to `/usr/bin/sandbox-exec`.
//!
//! Paths are not embedded in the profile body; they're passed via
//! `-D key=value` and `(param "KEY")`. This is so a path containing
//! whitespace or parentheses doesn't trip SBPL's quoting rules.

use std::path::Path;

use crate::policy::{SandboxMode, SandboxPolicy};

/// Does not consult `PATH`. This closes off the path where a same-named
/// binary on `PATH` could be substituted. In a situation where this binary
/// itself has been tampered with, the attacker already has root.
pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

fn readable_ancestors(policy: &SandboxPolicy) -> Vec<&Path> {
    policy
        .isolated_boundary()
        .into_iter()
        .flat_map(|boundary| {
            boundary
                .readable_roots
                .iter()
                .flat_map(|root| root.ancestors().skip(1))
        })
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Builds the SBPL profile body from the policy.
pub fn build_profile(policy: &SandboxPolicy) -> String {
    if let Some(boundary) = policy.isolated_boundary() {
        let mut p = String::from(
            "(version 1)\n(deny default)\n(allow process-fork process-exec)\n(allow signal (target same-sandbox))\n(allow sysctl-read (sysctl-name \"hw.pagesize\" \"hw.pagesize_compat\" \"hw.ncpu\" \"hw.memsize\" \"hw.activecpu\" \"kern.osrelease\" \"kern.osversion\" \"kern.ostype\"))\n(allow file-read* (literal \"/\" \"/dev/null\" \"/dev/urandom\" \"/dev/random\"))\n(allow file-write-data (literal \"/dev/null\"))\n",
        );
        // deny-default and named sysctl grants alone did not reject
        // KERN_PROCARGS2 on macOS. Explicitly deny other-process inspection;
        // self inspection remains useful and sees only the fixed child env.
        p.push_str("(deny process-info-pidinfo)\n(allow process-info-pidinfo (target self))\n");
        for i in 0..boundary.readable_roots.len() {
            p.push_str(&format!(
                "(allow file-read* (subpath (param \"READABLE_ROOT_{i}\")))\n"
            ));
        }
        // Toolchains canonicalize their executable/SDK paths. Metadata on exact
        // ancestors permits traversal without exposing directory listings/content.
        for i in 0..readable_ancestors(policy).len() {
            p.push_str(&format!(
                "(allow file-read-metadata (literal (param \"READABLE_ANCESTOR_{i}\")))\n"
            ));
        }
        if boundary.environment.is_some() {
            p.push_str("(allow file-read* file-write* (subpath (param \"ISOLATED_HOME\")))\n(allow file-read* file-write* (subpath (param \"ISOLATED_TMPDIR\")))\n");
        }
        if policy.mode() != SandboxMode::ReadOnly {
            for i in 0..policy.writable_roots().len() {
                p.push_str(&format!(
                    "(allow file-write* (subpath (param \"WRITABLE_ROOT_{i}\")))\n"
                ));
            }
        }
        // Even FullAccess retains file/process isolation. Network is a separate grant.
        if policy.mode() == SandboxMode::FullAccess {
            p.push_str("(allow network*)\n");
        }
        return p;
    }
    let mut p = String::from("(version 1)\n");

    if policy.mode() == SandboxMode::FullAccess {
        // Crosses the boundary but imposes no restriction. This isn't
        // branched into skipping profile construction altogether, so as to
        // keep the tested path and the production path identical.
        p.push_str("(allow default)\n");
        return p;
    }

    p.push_str("(deny default)\n");
    // The bare minimum a shell needs to behave as a shell.
    p.push_str("(allow process-fork)\n");
    p.push_str("(allow process-exec)\n");
    p.push_str("(allow signal (target same-sandbox))\n");
    if policy.mode() == SandboxMode::ReadOnly {
        p.push_str("(allow file-read* file-ioctl (literal \"/dev/ptmx\"))\n");
    } else {
        p.push_str("(allow file-read* file-write* file-ioctl (literal \"/dev/ptmx\"))\n");
    }
    p.push_str("(allow file-ioctl (regex #\"^/dev/ttys[0-9]+\"))\n");
    p.push_str("(allow file-read*)\n");
    // The Rust runtime needs this at startup. Before setting up the main
    // thread's guard page, it queries `sysconf(_SC_PAGESIZE)`, which on
    // macOS falls through to a sysctl (`hw.pagesize_compat`). If that's
    // denied, the page size can't be obtained, the subsequent mmap fails
    // with EINVAL, and
    // "failed to allocate a guard page: Invalid argument (os error 22)"
    // → `fatal runtime error` → SIGABRT follows. This happens *before a
    // single line of our own code runs*, so the `--confined-apply` helper
    // always crashed under both read-only and workspace-write, and that
    // abort arrived at the parent in the same shape as a policy-violation
    // denial (nonzero exit + stderr). Confirmed by measurement (pinned by
    // `polaris-cli/tests/confined_helper.rs`, which uses the real binary
    // and the real profile).
    //
    // Why this isn't narrowed further. Measurement showed that the helper
    // also launches with just
    // `(allow sysctl-read (sysctl-name "hw.pagesize" "hw.pagesize_compat"))`
    // (`hw.pagesize` alone is not enough). Even so, this doesn't narrow by
    // name, because this profile isn't dedicated to the helper. Any child
    // the `bash` tool launches also runs under this same profile, and it's
    // not unusual for one to query machine characteristics (`hw.ncpu`,
    // `hw.memsize`, `kern.osversion`, etc.). Allowing only those two names
    // would fix the helper, but Rust binaries and many other runtimes
    // launched from `bash` would keep crashing the same way (in fact,
    // without this line, even `sh -c 'polaris --help'` aborts).
    //
    // Why this can be said not to widen the boundary. `sysctl-read` only
    // reads machine characteristics; it grants no write permission at all.
    // What this profile protects is the write boundary, and reads are
    // already opened to the entire filesystem by the `(allow file-read*)`
    // directly above.
    p.push_str("(allow sysctl-read)\n");
    // Opens up writing to `/dev/null` only. `cmd > /dev/null` and
    // `cmd 2>/dev/null` are shell idioms in everyday use, and if the
    // redirection can't be opened, the shell falls over before ever
    // executing the command body (measured: `ls / >/dev/null && echo ok`
    // runs nothing at all). Worse, that failure reaches the model naming
    // "policy workspace-write (writable: <root>)", so the model doubts the
    // writable root and repeats the same failure over and over by changing
    // the path. This is exactly the case the spec calls out: "a denial
    // message that gives no purchase on the cause invites the same failure
    // to repeat, burning time and tokens".
    //
    // Measured (the real profile passed directly to `/usr/bin/sandbox-exec`):
    //
    // | grant added | `> /dev/null` | inside root | outside root | `rm /dev/null` |
    // | none (previous) | denied | ok | denied | denied |
    // | `file-write-data (literal "/dev/null")` | ok | ok | denied | denied |
    // | `file-write* (literal "/dev/null")` | ok | ok | denied | denied |
    //
    // `file-write-create` alone, and `file-write-mode` alone, both still
    // leave it as `Operation not permitted` and it cannot be opened.
    // `file-write-data` is the minimal right that lets the redirection
    // through, and unlike `file-write*` it grants neither unlink nor
    // setattr. The target is exactly the one entry `(literal "/dev/null")`,
    // and the same measurement confirmed that `/dev/zero`, `/dev/stdout`,
    // `/dev/stderr`, `/dev/fd/N`, and creating new nodes under `/dev`
    // all remain denied. Whether `/dev/stdout` and the like can be opened
    // requires a separate measurement — re-judging through the fdesc — so
    // this deliberately leaves it untouched here (M3's task).
    //
    // Emits this one line for read-only too. Writing to `/dev/null` is just
    // dropped by the kernel and changes nothing in the filesystem's state,
    // so it doesn't diminish what read-only protects (that nothing gets
    // changed). That an ordinary file write stays denied under read-only is
    // verified as a pair by `confine.rs`'s real-sandbox tests. Branching
    // here instead would mean `--sandbox read-only`'s `bash` alone keeps
    // producing the same misleading denial.
    p.push_str("(allow file-write-data (literal \"/dev/null\"))\n");

    let roots = policy.writable_roots();
    if !roots.is_empty() {
        p.push_str("(allow file-write*\n");
        for i in 0..roots.len() {
            p.push_str(&format!("  (subpath (param \"WRITABLE_ROOT_{i}\"))\n"));
        }
        p.push_str(")\n");
    }

    p
}

/// Builds the argv passed to `sandbox-exec`. The program itself and its
/// arguments go after `--`, so the boundary isn't ambiguous.
pub fn build_args(policy: &SandboxPolicy, program: &Path, args: &[String]) -> Vec<String> {
    let mut out = vec!["-p".to_string(), build_profile(policy)];
    if let Some(boundary) = policy.isolated_boundary() {
        out.push(format!(
            "-DISOLATED_WORKSPACE={}",
            boundary.workspace.display()
        ));
        for (i, root) in readable_ancestors(policy).iter().enumerate() {
            out.push(format!("-DREADABLE_ANCESTOR_{i}={}", root.display()));
        }
        if let Some(environment) = &boundary.environment {
            out.push(format!("-DISOLATED_HOME={}", environment.home.display()));
            out.push(format!(
                "-DISOLATED_TMPDIR={}",
                environment.tmpdir.display()
            ));
        }
        for (i, root) in boundary.readable_roots.iter().enumerate() {
            out.push(format!("-DREADABLE_ROOT_{i}={}", root.display()));
        }
    }
    for (i, root) in policy.writable_roots().iter().enumerate() {
        out.push(format!("-DWRITABLE_ROOT_{i}={}", root.display()));
    }
    out.push("--".to_string());
    out.push(program.display().to_string());
    out.extend(args.iter().cloned());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{SandboxMode, SandboxPolicy};

    fn workspace(roots: &[std::path::PathBuf]) -> SandboxPolicy {
        SandboxPolicy::new(SandboxMode::WorkspaceWrite, roots).expect("can't create policy")
    }

    #[test]
    fn the_profile_starts_closed() {
        // Without deny default, every subsequent allow becomes "add a
        // little on top of allow-everything-by-default", inverting what
        // the policy means.
        let dir = tempfile::tempdir().expect("temp dir");
        let p = build_profile(&workspace(&[dir.path().to_path_buf()]));
        assert!(p.contains("(deny default)"), "no default denial:\n{p}");
    }

    #[test]
    fn every_writable_root_gets_its_own_parameterised_subpath() {
        // Dropping even one root would silently shrink the set of places
        // that should be writable.
        let a = tempfile::tempdir().expect("temp dir");
        let b = tempfile::tempdir().expect("temp dir");
        let policy = workspace(&[a.path().to_path_buf(), b.path().to_path_buf()]);
        let p = build_profile(&policy);

        assert!(p.contains("(subpath (param \"WRITABLE_ROOT_0\"))"), "{p}");
        assert!(p.contains("(subpath (param \"WRITABLE_ROOT_1\"))"), "{p}");
        assert_eq!(
            p.matches("WRITABLE_ROOT_").count(),
            2,
            "root count and parameter count don't match:\n{p}"
        );
    }

    #[test]
    fn paths_never_appear_verbatim_in_the_profile_body() {
        // Writing a path directly into the body would break SBPL for a
        // path containing whitespace or parentheses. The value always
        // crosses through -D instead.
        let dir = tempfile::tempdir().expect("temp dir");
        let policy = workspace(&[dir.path().to_path_buf()]);
        let p = build_profile(&policy);
        let root = policy.writable_roots()[0].display().to_string();
        assert!(!p.contains(&root), "the path is embedded in the body:\n{p}");
    }

    #[test]
    fn read_only_grants_no_write_to_any_file_beyond_the_dev_null_sink() {
        // read-only can't hold a root (rejected by Task 1). What this
        // checks is that the write-permission clause itself doesn't
        // appear. If (allow file-write*) is left bare against an empty
        // root list, all writes are permitted.
        //
        // The one exception is the single `/dev/null` line. The kernel
        // just drops what's written and the filesystem's state doesn't
        // change, so this doesn't diminish read-only's property. Pinning
        // this down by counting lines is because "no additional
        // write-touching grant has appeared here" is the very substance of
        // what read-only means. `!contains("file-write*")` alone would let
        // an added grant using `file-write-data` slip through unnoticed.
        let policy = SandboxPolicy::new(SandboxMode::ReadOnly, &[]).expect("policy");
        let p = build_profile(&policy);
        assert!(!p.contains("file-write*"), "a write grant exists:\n{p}");
        let writes: Vec<&str> = p.lines().filter(|l| l.contains("file-write")).collect();
        assert_eq!(
            writes,
            vec!["(allow file-write-data (literal \"/dev/null\"))"],
            "read-only has a write grant other than /dev/null:\n{p}"
        );
        assert!(
            p.contains("(allow file-read*)"),
            "reading is not permitted:\n{p}"
        );
    }

    #[test]
    fn the_restrictive_profile_keeps_the_sysctl_read_grant() {
        // If this one line disappears, the Rust runtime can't query
        // `sysconf(_SC_PAGESIZE)` before main, the guard page's mmap fails
        // with EINVAL, and it SIGABRTs. In other words the real helper
        // becomes unable to launch, and that failure reaches the model in
        // the same shape as a policy-violation denial (nonzero exit +
        // stderr). The primary detector is
        // `polaris-cli/tests/confined_helper.rs`, which uses the real
        // binary, but this fails an accidental removal immediately at the
        // unit level.
        let dir = tempfile::tempdir().expect("temp dir");
        let ws = build_profile(&workspace(&[dir.path().to_path_buf()]));
        assert!(
            ws.contains("(allow sysctl-read)"),
            "workspace-write is missing sysctl-read:\n{ws}"
        );

        let ro = build_profile(&SandboxPolicy::new(SandboxMode::ReadOnly, &[]).expect("policy"));
        assert!(
            ro.contains("(allow sysctl-read)"),
            "read-only is missing sysctl-read:\n{ro}"
        );
    }

    #[test]
    fn full_access_still_produces_a_profile_so_the_path_is_the_same_one_we_test() {
        // full-access still crosses the boundary. Branching to skip
        // profile construction would make the tested path and the
        // production path different things.
        let policy = SandboxPolicy::new(SandboxMode::FullAccess, &[]).expect("policy");
        let p = build_profile(&policy);
        assert!(p.starts_with("(version 1)"), "{p}");
        assert!(p.contains("(allow default)"), "{p}");
    }

    #[test]
    fn args_pass_each_root_as_a_d_parameter_and_separate_the_command_with_dashdash() {
        let a = tempfile::tempdir().expect("temp dir");
        let b = tempfile::tempdir().expect("temp dir");
        let policy = workspace(&[a.path().to_path_buf(), b.path().to_path_buf()]);

        let args = build_args(
            &policy,
            std::path::Path::new("/bin/echo"),
            &["hello".to_string()],
        );

        assert_eq!(args[0], "-p", "the profile spec is not first: {args:?}");
        let root0 = policy.writable_roots()[0].display();
        assert!(
            args.iter()
                .any(|a| a == &format!("-DWRITABLE_ROOT_0={root0}")),
            "root 0 was not passed via -D: {args:?}"
        );
        let sep = args
            .iter()
            .position(|a| a == "--")
            .expect("no --: the boundary between command and arguments is ambiguous");
        assert_eq!(args[sep + 1], "/bin/echo");
        assert_eq!(args[sep + 2], "hello");
    }
}
