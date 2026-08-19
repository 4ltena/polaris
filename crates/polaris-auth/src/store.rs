//! Storage for credentials. `~/.polaris/auth.json`, 0600, atomic writes.
//!
//! Never reads `~/.codex/auth.json`. Never copies it either.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::{AuthError, Credentials};

/// The default storage location. `~/.polaris/auth.json`.
pub fn default_path() -> Result<PathBuf, AuthError> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| AuthError::Io(std::io::Error::other("HOME is not set")))?;
    Ok(Path::new(&home).join(".polaris").join("auth.json"))
}

/// Saves. Creates the temp file newly at 0600 (when reopening an existing
/// tmp, tightens it with set_permissions), writes, then renames. rename is
/// atomic within the same directory, so even if we crash partway through,
/// the real file never gets replaced with half-written content.
pub fn save_to(path: &Path, c: &Credentials) -> Result<(), AuthError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_vec_pretty(c)
        .map_err(|e| AuthError::Decode(format!("could not serialize credentials: {e}")))?;

    // 0600 is guaranteed by both the open-time mode AND set_permissions.
    // This is not redundancy. The two do not reach the same postcondition by
    // separate routes — each covers only its own distinct path: mode covers
    // the "tmp is created fresh" path (an ordinary save_to call goes
    // through here almost every time), while set_permissions covers the "a
    // previous write was interrupted before rename, leaving a
    // loosely-permissioned tmp behind, and we're reopening it" path.
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        // mode(0o600) only matters on the fresh-creation path this covers.
        // Because open() embeds the mode into file creation at the instant
        // it creates the file, no window ever opens where the file exists
        // at the umask default (0644 in this environment) between creation
        // and the point where the later set_permissions takes effect. With
        // set_permissions alone tightening permissions after
        // write_all/sync_all/drop, that window from the instant of creation
        // to the moment it's tightened cannot be closed at all — this is a
        // target that writes a plaintext OAuth access_token/refresh_token,
        // and closing off that window matters.
        //
        // That said, this window is only observable through concurrent
        // access from another process, so within this file's tests, which
        // are single-threaded and sequential, removing this mode(0o600)
        // cannot be detected (confirmed by individual mutation
        // re-verification). This is the same kind of limitation as the
        // atomicity of tmp+rename being unobservable in tests; following
        // how polaris-sandbox's predicates are written to spell out "this
        // is a mitigation, not a guarantee," we spell out here too that
        // this is "untestable by nature," not "no test means it was
        // forgotten." This is never grounds for deleting this line.
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(&body)?;
    f.sync_all()?;
    drop(f);
    // set_permissions covers this path (reopening an existing tmp). The
    // open-time mode only takes effect on fresh creation and is powerless
    // when this path opens an existing file, so set_permissions taking
    // effect unconditionally is the only means of guaranteeing 0600 on this
    // path. Tested individually in
    // a_preexisting_loose_temp_file_is_still_corrected_to_owner_only.
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Reads. Nonexistence is not a failure. Corruption is.
pub fn load_from(path: &Path) -> Result<Option<Credentials>, AuthError> {
    let body = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(AuthError::Io(e)),
    };
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| AuthError::Decode(format!("could not parse {}: {e}", path.display())))
}

/// Deletes. The return value is whether the file actually existed.
pub fn delete_at(path: &Path) -> Result<bool, AuthError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(AuthError::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Credentials {
        Credentials {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            account_id: "acct".into(),
            expires_at: Some(1_800_000_000),
        }
    }

    #[test]
    fn a_saved_credential_round_trips() {
        let dir = tempfile::tempdir().expect("temp dir");
        let p = dir.path().join("auth.json");
        save_to(&p, &sample()).expect("failed to save");
        let got = load_from(&p).expect("failed to read").expect("missing");
        assert_eq!(got, sample());
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let got = load_from(&dir.path().join("nope.json")).expect("nonexistence is not a failure");
        assert!(got.is_none());
    }

    /// The credentials file is readable only by its owner. It must not be
    /// readable from another process.
    #[test]
    fn the_saved_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("temp dir");
        let p = dir.path().join("auth.json");
        save_to(&p, &sample()).expect("failed to save");
        let mode = std::fs::metadata(&p)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "permissions are not 0600: {:o}",
            mode & 0o777
        );
    }

    /// Permissions don't loosen even on an overwriting save. Becoming 0600
    /// the first time doesn't stop a hole opening up if the second
    /// recreates it at the default 0644.
    #[test]
    fn overwriting_keeps_the_file_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("temp dir");
        let p = dir.path().join("auth.json");
        save_to(&p, &sample()).expect("failed to save");
        let mut second = sample();
        second.access_token = "at2".into();
        save_to(&p, &second).expect("failed to save");
        let mode = std::fs::metadata(&p)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "permissions loosened on overwrite: {:o}",
            mode & 0o777
        );
        assert_eq!(
            load_from(&p)
                .expect("failed to read")
                .expect("missing")
                .access_token,
            "at2"
        );
    }

    /// When a temp file is already left behind with loose permissions (e.g.
    /// a previous write was interrupted before rename), the open-time mode
    /// has no effect from merely opening an existing file. Even so,
    /// set_permissions takes effect unconditionally, so the real file ends
    /// up at 0600.
    #[test]
    fn a_preexisting_loose_temp_file_is_still_corrected_to_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("temp dir");
        let p = dir.path().join("auth.json");
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, b"leftover").expect("failed to write");
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644))
            .expect("failed to set permissions");

        save_to(&p, &sample()).expect("failed to save");

        let mode = std::fs::metadata(&p)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "the reused temp file's loose permissions survived: {:o}",
            mode & 0o777
        );
    }

    /// A write goes to the temp file, then renames. Crashing before the
    /// rename leaves the old file intact. Here we check that the write
    /// target isn't the real file, by confirming that "even with a temp
    /// file left behind, the real file still reads back its old content."
    #[test]
    fn a_leftover_temp_file_does_not_disturb_the_stored_credentials() {
        let dir = tempfile::tempdir().expect("temp dir");
        let p = dir.path().join("auth.json");
        save_to(&p, &sample()).expect("failed to save");

        // Simulates the trace of an interrupted write.
        std::fs::write(dir.path().join("auth.json.tmp"), b"half-written").expect("failed to write");

        let got = load_from(&p).expect("failed to read").expect("missing");
        assert_eq!(
            got,
            sample(),
            "the real file was contaminated by the temp file"
        );
    }

    #[test]
    fn delete_reports_whether_a_file_was_there() {
        let dir = tempfile::tempdir().expect("temp dir");
        let p = dir.path().join("auth.json");
        assert!(
            !delete_at(&p).expect("delete shouldn't fail"),
            "said it deleted something that wasn't there"
        );
        save_to(&p, &sample()).expect("failed to save");
        assert!(
            delete_at(&p).expect("failed to delete"),
            "said it didn't delete something that was there"
        );
        assert!(!p.exists());
    }

    #[test]
    fn a_corrupt_file_is_an_error_not_a_silent_absence() {
        let dir = tempfile::tempdir().expect("temp dir");
        let p = dir.path().join("auth.json");
        std::fs::write(&p, b"{ not json").expect("failed to write");
        let err = load_from(&p).expect_err("a corrupt file should fail");
        assert!(
            matches!(err, AuthError::Decode(_)),
            "corrupt file resulted in something other than Decode: {err:?}"
        );
    }
}
