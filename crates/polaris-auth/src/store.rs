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

    let mut protection = crate::protection::lock()?;
    let path = protection.register_store(path)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_vec_pretty(c)
        .map_err(|_| AuthError::Decode("could not serialize credentials".into()))?;

    // Nonblocking open and regular-file checks precede truncation/body I/O.
    // The helper creates new files at 0600; fchmod also tightens reused tmp files.
    let mut f = crate::protection::open_regular(&tmp, true)?;
    protection.remember(&f.metadata()?)?;
    f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    f.set_len(0)?;
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
    std::fs::rename(&tmp, &path)?;
    protection.register_store(&path)?;
    Ok(())
}

/// Reads. Nonexistence is not a failure. Corruption is.
pub fn load_from(path: &Path) -> Result<Option<Credentials>, AuthError> {
    let mut protection = crate::protection::lock()?;
    let path = protection.register_store(path)?;
    let file = match crate::protection::open_regular(&path, false) {
        Ok(file) => file,
        Err(AuthError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    protection.remember(&file.metadata()?)?;
    let body = crate::protection::read_body(file)?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|_| AuthError::Decode("could not parse credentials".into()))
}

/// Deletes. The return value is whether the file actually existed.
pub fn delete_at(path: &Path) -> Result<bool, AuthError> {
    let mut protection = crate::protection::lock()?;
    let path = protection.register_store(path)?;
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(AuthError::Io(e)),
    }
}

/// Metadata-only identity for confirmation of the default credential store.
/// Credential bytes are never copied into a preview or receipt.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct CredentialIdentity {
    device: u64,
    inode: u64,
    bytes: u64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

fn identity(file: &std::fs::File) -> Result<CredentialIdentity, AuthError> {
    use std::os::unix::fs::MetadataExt;
    let data = file.metadata()?;
    Ok(CredentialIdentity {
        device: data.dev(),
        inode: data.ino(),
        bytes: data.len(),
        changed_seconds: data.ctime(),
        changed_nanoseconds: data.ctime_nsec(),
    })
}

pub fn credential_identity(path: &Path) -> Result<Option<CredentialIdentity>, AuthError> {
    let mut protection = crate::protection::lock()?;
    let path = protection.register_store(path)?;
    match crate::protection::open_regular(&path, false) {
        Ok(file) => {
            protection.remember(&file.metadata()?)?;
            identity(&file).map(Some)
        }
        Err(AuthError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Reject a refresh/replacement since confirmation. Cooperating writers use
/// this same lock. A successful removal is durable before returning.
pub fn delete_exact(path: &Path, expected: &CredentialIdentity) -> Result<(), AuthError> {
    let mut protection = crate::protection::lock()?;
    let path = protection.register_store(path)?;
    let file = crate::protection::open_regular(&path, false)?;
    protection.remember(&file.metadata()?)?;
    if &identity(&file)? != expected {
        return Err(AuthError::Denied("stale_credential_store".into()));
    }
    std::fs::remove_file(&path)?;
    std::fs::File::open(path.parent().ok_or(AuthError::Protection)?)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn m3a_exact_logout_preserves_replaced_credentials() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("auth.json");
        save_to(&path, &sample()).unwrap();
        let expected = credential_identity(&path).unwrap().unwrap();
        let replacement = sample();
        save_to(&path, &replacement).unwrap();
        assert!(delete_exact(&path, &expected).is_err());
        assert_eq!(load_from(&path).unwrap().unwrap(), replacement);
        let current = credential_identity(&path).unwrap().unwrap();
        delete_exact(&path, &current).unwrap();
        assert!(credential_identity(&path).unwrap().is_none());
    }

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
