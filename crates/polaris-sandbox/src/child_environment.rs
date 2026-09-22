//! Native child environment only; broker requests do not carry this contract.
//! Values are fetched only for explicitly allowed names, never by enumerating
//! the parent's credentials. This does not isolate files, inherited FDs, or
//! secrets deliberately stored in an allowed variable in legacy mode.
//! Isolated children receive only fixed values and registered scratch paths.

use std::ffi::{OsStr, OsString};
use std::process::Command;

const ALLOWED: &[&str] = &[
    "PATH",
    "HOME",
    "TMPDIR",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "LC_COLLATE",
    "LC_MESSAGES",
    "LC_MONETARY",
    "LC_NUMERIC",
    "LC_TIME",
    "TZ",
    "TERM",
];

pub(crate) fn is_runtime_variable(name: &str) -> bool {
    ALLOWED.contains(&name)
        || [
            "LD_", "DYLD_", "_RLD_", "PYTHON", "NODE_", "RUBY", "PERL", "BASH", "GIT_", "POLARIS_",
            "LC_", "DOTNET_", "RUSTC_", "CARGO_", "JDK_", "JAVA_", "_JAVA_",
        ]
        .iter()
        .any(|prefix| name.starts_with(prefix))
        || matches!(
            name,
            "ENV"
                | "ZDOTDIR"
                | "SHELLOPTS"
                | "BASHOPTS"
                | "IFS"
                | "CDPATH"
                | "CLASSPATH"
                | "SSL_CERT_FILE"
                | "SSL_CERT_DIR"
                | "OPENSSL_CONF"
                | "OPENSSL_MODULES"
        )
}

fn select(mut lookup: impl FnMut(&str) -> Option<OsString>) -> Vec<(&'static str, OsString)> {
    ALLOWED
        .iter()
        .filter_map(|&name| lookup(name).map(|value| (name, value)))
        .collect()
}

pub(crate) fn apply(
    policy: &crate::SandboxPolicy,
    command: &mut Command,
) -> Result<(), crate::SandboxError> {
    apply_policy_with(policy, command, |name| std::env::var_os(OsStr::new(name)))
}

fn apply_policy_with(
    policy: &crate::SandboxPolicy,
    command: &mut Command,
    lookup: impl FnMut(&str) -> Option<OsString>,
) -> Result<(), crate::SandboxError> {
    if let Some(boundary) = policy.isolated_boundary() {
        let environment = boundary.environment.as_ref().ok_or_else(|| {
            crate::SandboxError::NotEnforced("isolated environment is not configured".into())
        })?;
        environment.validate(&boundary.workspace)?;
        // Do not call lookup, even for previously allowed names: their values
        // can contain credentials or select host startup/configuration paths.
        let path = std::env::join_paths(
            environment
                .runtime_bin_dirs
                .iter()
                .map(std::path::PathBuf::as_path)
                .chain([
                    std::path::Path::new("/usr/bin"),
                    std::path::Path::new("/bin"),
                ]),
        )
        .map_err(|_| crate::SandboxError::NotEnforced("invalid runtime PATH".into()))?;
        command
            .env_clear()
            .env("PATH", path)
            .env("HOME", &environment.home)
            .env("TMPDIR", &environment.tmpdir)
            .env("LANG", "C")
            .env("LC_ALL", "C")
            .env("TZ", "UTC")
            .env("TERM", "dumb")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null");
    } else {
        apply_with(command, lookup);
        // The isolated branch above pins a writable TMPDIR and takes git's
        // host configuration out of reach. This branch used to inherit the
        // parent's TMPDIR from `ALLOWED` and set no `GIT_*` at all — and
        // under `workspace-write` only the writable roots are writable, so
        // that inherited directory is not.
        //
        // What that cost is measurable. `/usr/bin/python3` and
        // `/usr/bin/git` are xcrun shims: unable to create
        // `$TMPDIR/xcrun_db-*`, they fall back to `xcodebuild` and print
        // about 827 bytes of DVTToolchain errors per invocation, and git
        // additionally fails on `~/.gitconfig` and exits 128. All of it is
        // attached to the tool result, enters the history, and is re-billed
        // on every later request of that turn. Across the recorded parity
        // sessions that is 452,696 bytes — 8.2% of every tool-result byte
        // ever sent, and about a third of the billed input of the one task
        // that runs shell commands.
        //
        // It also makes a result lie: a run whose assertions all printed
        // OK opened with "the command failed with exit code 128" because
        // `git status` died on a file it was never going to be allowed to
        // read.
        // The other half is the writable root, not an environment variable:
        // `apply_with` above already passes the parent's `TMPDIR` through,
        // and `build_sandbox_policy` now grants it, so the shims can write
        // their cache and stop narrating the failure.
        command
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null");
    }
    Ok(())
}

fn apply_with(command: &mut Command, lookup: impl FnMut(&str) -> Option<OsString>) {
    command.env_clear().envs(select(lookup));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copied_runtime_path_is_fixed_and_inherited_by_restricted_children() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let workspace = root.join("workspace");
        let home = root.join("home");
        let tmpdir = root.join("tmp");
        let runtime = root.join("runtime");
        let bin = runtime.join("bin");
        for dir in [&workspace, &home, &tmpdir, &runtime, &bin] {
            std::fs::create_dir(dir).unwrap();
        }
        for mode in [
            crate::SandboxMode::ReadOnly,
            crate::SandboxMode::WorkspaceWrite,
            crate::SandboxMode::FullAccess,
        ] {
            let policy =
                crate::SandboxPolicy::isolated(mode, &workspace, std::slice::from_ref(&runtime))
                    .unwrap()
                    .with_isolated_sibling_environment(&home, &tmpdir)
                    .unwrap()
                    .with_isolated_runtime_bins(std::slice::from_ref(&bin))
                    .unwrap();
            let child = policy.restrict(crate::SandboxMode::ReadOnly, &[]).unwrap();
            for policy in [&policy, &child] {
                let mut command = Command::new("/bin/sh");
                command
                    .env("PATH", "synthetic-secret")
                    .env("NODE_OPTIONS", "synthetic-secret");
                apply_policy_with(policy, &mut command, |_| panic!("parent lookup")).unwrap();
                let actual: std::collections::BTreeMap<_, _> = command
                    .get_envs()
                    .map(|(k, v)| (k.to_os_string(), v.unwrap().to_os_string()))
                    .collect();
                let expected = std::env::join_paths([
                    bin.as_path(),
                    std::path::Path::new("/usr/bin"),
                    std::path::Path::new("/bin"),
                ])
                .unwrap();
                assert_eq!(actual.get(OsStr::new("PATH")), Some(&expected));
                assert_eq!(
                    actual.get(OsStr::new("HOME")),
                    Some(&home.clone().into_os_string())
                );
                assert!(!actual.contains_key(OsStr::new("NODE_OPTIONS")));
                assert_eq!(actual.len(), 9);
            }
        }
    }

    #[test]
    fn isolated_environment_is_fixed_without_reading_even_allowlisted_parent_values() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let tmpdir = root.path().join("tmp");
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(&tmpdir).unwrap();
        for mode in [
            crate::SandboxMode::ReadOnly,
            crate::SandboxMode::WorkspaceWrite,
            crate::SandboxMode::FullAccess,
        ] {
            let policy = crate::SandboxPolicy::isolated(mode, root.path(), &[])
                .unwrap()
                .with_isolated_environment(&home, &tmpdir)
                .unwrap();
            let mut command = Command::new("/bin/sh");
            for name in ALLOWED.iter().copied().chain([
                "ORDINARY_LABEL",
                "POLARIS_API_KEY",
                "BASH_ENV",
                "GIT_CONFIG_NOSYSTEM",
                "GIT_CONFIG_GLOBAL",
            ]) {
                command.env(name, "synthetic-secret");
            }
            apply_policy_with(&policy, &mut command, |_| {
                panic!("parent environment consulted")
            })
            .unwrap();
            let actual: std::collections::BTreeMap<_, _> = command
                .get_envs()
                .map(|(key, value)| (key.to_os_string(), value.unwrap().to_os_string()))
                .collect();
            let expected: std::collections::BTreeMap<_, _> = [
                ("PATH", OsString::from("/usr/bin:/bin")),
                ("HOME", home.canonicalize().unwrap().into_os_string()),
                ("TMPDIR", tmpdir.canonicalize().unwrap().into_os_string()),
                ("LANG", OsString::from("C")),
                ("LC_ALL", OsString::from("C")),
                ("TZ", OsString::from("UTC")),
                ("TERM", OsString::from("dumb")),
                ("GIT_CONFIG_NOSYSTEM", OsString::from("1")),
                ("GIT_CONFIG_GLOBAL", OsString::from("/dev/null")),
            ]
            .into_iter()
            .map(|(key, value)| (OsString::from(key), value))
            .collect();
            assert_eq!(actual, expected);
            assert_eq!(policy.mode(), mode);
        }
    }

    #[test]
    fn isolated_environment_refuses_unconfigured_or_missing_scratch() {
        let root = tempfile::tempdir().unwrap();
        let policy =
            crate::SandboxPolicy::isolated(crate::SandboxMode::ReadOnly, root.path(), &[]).unwrap();
        let mut command = Command::new("/bin/sh");
        assert!(
            apply_policy_with(&policy, &mut command, |_| panic!(
                "fallback consulted parent"
            ))
            .is_err()
        );
        let home = root.path().join("home");
        let tmpdir = root.path().join("tmp");
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(&tmpdir).unwrap();
        let policy = policy.with_isolated_environment(&home, &tmpdir).unwrap();
        std::fs::remove_dir(&tmpdir).unwrap();
        assert!(
            apply_policy_with(&policy, &mut command, |_| panic!(
                "fallback consulted parent"
            ))
            .is_err()
        );
    }

    #[test]
    fn selects_only_explicit_names_without_changing_the_source() {
        let source: std::collections::BTreeMap<_, _> = [
            ("PATH", "/usr/bin:/bin"),
            ("HOME", "/synthetic/home"),
            ("TMPDIR", "/synthetic/tmp"),
            ("LANG", "C"),
            ("LC_CTYPE", "C"),
            ("ORDINARY_LABEL", "sentinel"),
            ("POLARIS_API_KEY", "fake-key"),
            ("POLARIS_SANDBOX_BROKER_TOKEN", "fake-token"),
            ("POLARIS_SANDBOX_BROKER", "/synthetic/socket"),
            ("BASH_ENV", "/synthetic/startup"),
            ("ENV", "/synthetic/startup"),
            ("LD_PRELOAD", "/synthetic/loader"),
            ("DYLD_INSERT_LIBRARIES", "/synthetic/loader"),
            ("LC_UNLISTED", "sentinel"),
        ]
        .into_iter()
        .map(|(k, v)| (k, OsString::from(v)))
        .collect();
        let before = source.clone();
        let mut command = Command::new("/bin/sh");
        command.env("PRESET_SECRET", "sentinel");
        apply_with(&mut command, |name| source.get(name).cloned());
        let actual: std::collections::BTreeMap<_, _> = command
            .get_envs()
            .map(|(k, v)| (k.to_os_string(), v.unwrap().to_os_string()))
            .collect();
        let expected = ["PATH", "HOME", "TMPDIR", "LANG", "LC_CTYPE"]
            .into_iter()
            .map(|name| (OsString::from(name), source[name].clone()))
            .collect();
        assert_eq!(actual, expected);
        assert_eq!(source, before);
    }

    #[test]
    fn missing_values_are_not_invented() {
        assert!(select(|_| None).is_empty());
    }
}

#[cfg(test)]
mod workspace_environment_tests {
    use super::*;
    use crate::{SandboxMode, SandboxPolicy};

    /// Under `workspace-write` git is kept away from the host's
    /// configuration, as it already is under isolation. Without this,
    /// every `git` the model ran printed "unable to access
    /// '~/.gitconfig'" into the tool result and exited 128 — a verdict
    /// that contradicted its own body, on top of the bytes.
    #[test]
    fn a_workspace_write_child_cannot_reach_the_hosts_git_configuration() {
        let root = tempfile::tempdir().expect("no temp dir");
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()])
            .expect("policy");
        let mut command = Command::new("/usr/bin/true");
        apply_policy_with(&policy, &mut command, |_| None).expect("apply");

        let seen: std::collections::HashMap<_, _> = command
            .get_envs()
            .filter_map(|(key, value)| value.map(|value| (key.to_owned(), value.to_owned())))
            .collect();
        assert_eq!(
            seen.get(OsStr::new("GIT_CONFIG_NOSYSTEM"))
                .map(|v| v.as_os_str()),
            Some(OsStr::new("1"))
        );
        assert_eq!(
            seen.get(OsStr::new("GIT_CONFIG_GLOBAL"))
                .map(|v| v.as_os_str()),
            Some(OsStr::new("/dev/null"))
        );
    }
}
