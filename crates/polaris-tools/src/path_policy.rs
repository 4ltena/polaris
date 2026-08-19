//! Decides which paths reading is denied on. Leans toward avoiding missed
//! secrets rather than avoiding over-broad denials.
//!
//! All comparisons are done after lowercasing. macOS's default filesystem is
//! case-insensitive, so a spelling like `.SSH` or `.ENV` still reaches the
//! same underlying secret. Until the sandbox is in place, this function is
//! the only line of defense, so no drift toward under-denial is tolerated.
//!
//! There used to be a separate, independent function,
//! `polaris_core::secret_screen::is_excluded_path`, responsible for the path
//! exclusion judgment made before writing to the audit log (`polaris-tools`
//! cannot depend on `polaris-core`, so it could never be called directly
//! anyway). Having two public functions with a similar purpose sitting side
//! by side, with only one of them actually wired in, was itself the
//! dangerous part — whoever touched this next would use whichever one they
//! found, with a coin-flip's chance of picking the one that protected
//! nothing. Task 12 cross-checked the two, folded in the coverage that only
//! `is_excluded_path` had (bare-extension files, e.g. a name that is just
//! `.pem` with nothing else, which doesn't count as an extension), and then
//! deleted `is_excluded_path`. When this list grows in the future, fold the
//! change in here rather than creating a second implementation to maintain
//! in parallel.

use std::path::Path;

/// Deny if any of these directory names appear as a path component
/// (compared lowercase).
const DENIED_DIRS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    "gcloud",
    "keychains",
    ".docker",
    ".kube",
];

/// Deny if the file name exactly matches one of these (compared lowercase).
const DENIED_NAMES: &[&str] = &[
    ".env",
    "credentials",
    "id_rsa",
    "id_dsa",
    "id_ed25519",
    "id_ecdsa",
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".envrc",
    ".git-credentials",
    ".pgpass",
    ".my.cnf",
    "credentials.json",
    "service-account.json",
];

/// Deny if the extension is one of these (compared lowercase).
const DENIED_EXTS: &[&str] = &["pem", "key", "p12", "pfx", "pub", "p8", "jks", "keystore"];

pub fn is_denied(path: &Path) -> bool {
    for c in path.components() {
        let s = c.as_os_str().to_string_lossy().to_lowercase();
        if DENIED_DIRS.iter().any(|d| s == *d) {
            return true;
        }
    }

    if is_proc_environ(path) || is_gh_hosts_file(path) {
        return true;
    }

    let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_lowercase()) else {
        return false;
    };

    if DENIED_NAMES.iter().any(|n| name == *n) {
        return true;
    }
    // Also deny suffixed forms like `.env.local`. Doesn't sweep in
    // `environment.rs`.
    if name.starts_with(".env.") {
        return true;
    }
    // Deny only macOS keychain files. Don't sweep in an ordinary source
    // file that merely contains the word "keychain", like
    // `keychain_helpers.rs`, via a partial match.
    if name.ends_with(".keychain") || name.ends_with(".keychain-db") {
        return true;
    }
    // Judge by a suffix match on the file name itself, not
    // `Path::extension()`. `Path::extension()` treats a name that starts
    // with `.` and contains no other `.` (a bare `.pem`, for example) as a
    // hidden file and returns no extension, which misses a bare-extension
    // file that is itself key material
    // (`Path::new(".pem").extension()` returns `None` for `/tmp/x/.pem` —
    // a gap discovered in Task 12 by cross-checking against
    // `secret_screen::is_excluded_path`, which didn't have this hole
    // because it used a plain string suffix match. Folded in here).
    if DENIED_EXTS.iter().any(|e| name.ends_with(&format!(".{e}"))) {
        return true;
    }
    false
}

/// `/proc/<pid>/environ` (Linux) contains a process's environment variables
/// verbatim, including the harness's own `POLARIS_API_KEY`. However, `proc`
/// is also a directory name that can ordinarily show up in a source tree,
/// so this doesn't deny on the directory name alone; it combines that with
/// the file name being exactly `environ`.
fn is_proc_environ(path: &Path) -> bool {
    let is_environ = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase() == "environ")
        .unwrap_or(false);
    if !is_environ {
        return false;
    }
    path.components()
        .any(|c| c.as_os_str().to_string_lossy().to_lowercase() == "proc")
}

/// The GitHub CLI's `hosts.yml` contains a saved token in plain text. The
/// name `hosts.yml` alone is also used for other purposes (an Ansible
/// inventory, for example), so this denies only in combination with a `gh`
/// directory component.
fn is_gh_hosts_file(path: &Path) -> bool {
    let is_hosts_yml = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase() == "hosts.yml")
        .unwrap_or(false);
    if !is_hosts_yml {
        return false;
    }
    path.components()
        .any(|c| c.as_os_str().to_string_lossy().to_lowercase() == "gh")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn denies_secret_bearing_paths() {
        for p in [
            "/home/u/proj/.env",
            "/home/u/proj/.env.local",
            "/home/u/.ssh/id_ed25519",
            "/home/u/.ssh/known_hosts",
            "/home/u/.gnupg/secring.gpg",
            "/home/u/.aws/credentials",
            "/home/u/key.pem",
            "/home/u/cert.pub",
            "/home/u/Library/Keychains/login.keychain-db",
        ] {
            assert!(is_denied(Path::new(p)), "{p} should be denied");
        }
    }

    #[test]
    fn allows_ordinary_source_paths() {
        for p in [
            "/home/u/proj/src/main.rs",
            "/home/u/proj/Cargo.toml",
            "/home/u/proj/docs/env-setup.md",
            "/home/u/proj/environment.rs",
        ] {
            assert!(!is_denied(Path::new(p)), "{p} should be allowed");
        }
    }

    #[test]
    fn allows_benign_keychain_named_source() {
        // Don't sweep in an ordinary source file that merely contains
        // "keychain" via a partial match.
        assert!(!is_denied(Path::new(
            "/home/u/proj/crates/polaris-tools/src/keychain_helpers.rs"
        )));
    }

    #[test]
    fn denies_case_variants_on_case_insensitive_filesystems() {
        // macOS's default filesystem is case-insensitive. `known_hosts`
        // doesn't hit any exact-match rule, so this only passes if the
        // directory component's case-folding is actually in effect (with
        // `.SSH/id_rsa`, the exact-match rule for `id_rsa` alone would
        // already pass it, so it couldn't verify the directory-side
        // folding).
        for p in [
            "/home/u/.SSH/known_hosts",
            "/home/u/proj/.ENV",
            "/home/u/key.PEM",
        ] {
            assert!(
                is_denied(Path::new(p)),
                "{p} should be denied regardless of case"
            );
        }
    }

    #[test]
    fn denies_gcloud_and_aws_directories_in_isolation() {
        // Confirm this can deny on the directory name alone, without
        // relying on the file-name-side rule (`credentials`, etc.). Neither
        // file name here matches DENIED_NAMES.
        for p in [
            "/home/u/.config/gcloud/credentials.db",
            "/home/u/.aws/config",
        ] {
            assert!(is_denied(Path::new(p)), "{p} should be denied");
        }
    }

    // --- Important 4: coverage at least equal to secret_screen::is_excluded_path -------
    // Of the 9 paths the review measured as `is_denied == false`, verify 8
    // of them (excluding `/proc/self/environ`; `.zsh_history` is out of
    // scope because it's on neither list) one rule at a time.

    #[test]
    fn denies_netrc_npmrc_and_pypirc_by_exact_name() {
        for p in ["/home/u/.netrc", "/home/u/.npmrc", "/home/u/.pypirc"] {
            assert!(is_denied(Path::new(p)), "{p} should be denied");
        }
    }

    #[test]
    fn denies_envrc_and_git_credentials_by_exact_name() {
        for p in ["/home/u/.envrc", "/home/u/proj/.git-credentials"] {
            assert!(is_denied(Path::new(p)), "{p} should be denied");
        }
    }

    #[test]
    fn denies_pgpass_and_my_cnf_by_exact_name() {
        for p in ["/home/u/.pgpass", "/home/u/.my.cnf"] {
            assert!(is_denied(Path::new(p)), "{p} should be denied");
        }
    }

    #[test]
    fn denies_cloud_credential_json_files_by_exact_name() {
        for p in [
            "/home/u/creds/credentials.json",
            "/home/u/gcp/service-account.json",
        ] {
            assert!(is_denied(Path::new(p)), "{p} should be denied");
        }
    }

    #[test]
    fn denies_docker_and_kube_directories_in_isolation() {
        // Confirm this can deny on the directory name alone, without
        // relying on the file-name-side rule. `config.json` / `config`
        // match neither entry in DENIED_NAMES.
        for p in ["/home/u/.docker/config.json", "/home/u/.kube/config"] {
            assert!(is_denied(Path::new(p)), "{p} should be denied");
        }
    }

    #[test]
    fn denies_gh_hosts_file_under_gh_directory() {
        assert!(is_denied(Path::new("/home/u/.config/gh/hosts.yml")));
    }

    #[test]
    fn denies_proc_self_environ() {
        assert!(is_denied(Path::new("/proc/self/environ")));
        // Also deny for another process's pid.
        assert!(is_denied(Path::new("/proc/1234/environ")));
    }

    #[test]
    fn denies_additional_key_extensions() {
        for p in ["/home/u/key.p8", "/home/u/app.jks", "/home/u/app.keystore"] {
            assert!(is_denied(Path::new(p)), "{p} should be denied");
        }
    }

    #[test]
    fn denies_bare_dotfiles_named_after_a_sensitive_extension() {
        // `Path::extension()` treats a name that starts with `.` and
        // contains no other `.` (a bare-extension file like `.pem`) as a
        // hidden file and returns no extension. While `is_denied` relied on
        // this, it missed bare `.pem` / `.key` files that are themselves
        // key material (a gap Task 12 found by cross-checking against
        // `secret_screen::is_excluded_path`, and folded in here).
        assert_eq!(
            Path::new(".pem").extension(),
            None,
            "premise: extension() returns None"
        );
        for p in [
            "/home/u/.pem",
            "/home/u/.key",
            "/home/u/.pfx",
            "/home/u/.p12",
        ] {
            assert!(is_denied(Path::new(p)), "{p} should be denied");
        }
    }

    #[test]
    fn denies_id_dsa_by_exact_name() {
        // Confirm this can deny on the file-name-only rule even outside a
        // `.ssh` directory (without relying on the directory-side rule).
        assert!(is_denied(Path::new("/home/u/backup/id_dsa")));
    }

    #[test]
    fn allows_benign_files_resembling_the_new_rules() {
        // Confirm the new rules are judged only by exact match, prefix,
        // suffix, or directory component, and don't degrade into a partial
        // match.
        for p in [
            // ".netrc" etc. are exact file-name matches; don't sweep in an
            // ordinary file that merely contains that string.
            "/home/u/docs/netrc-setup.md",
            "/home/u/src/npmrc_loader.rs",
            // ".docker" / ".kube" are exact directory-component matches;
            // don't sweep in a confusingly-named different directory.
            "/home/u/proj/docker-compose/README.md",
            "/home/u/proj/src/kubeconfig_loader.rs",
            // "hosts.yml" is denied only under a `gh` directory.
            "/home/u/proj/ansible/hosts.yml",
            // "environ" is denied only under a `proc` directory.
            "/home/u/proj/environ.rs",
            "/home/u/proj/proc/build.rs",
            // The extension rule matches a dot-separated extension; it
            // doesn't deny just because the string appears somewhere in
            // the middle of a file name.
            "/home/u/proj/src/jks_parser.rs",
            "/home/u/proj/notes/keystore-migration.md",
        ] {
            assert!(!is_denied(Path::new(p)), "{p} should be allowed");
        }
    }
}
