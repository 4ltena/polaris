//! Storage for a plain OpenAI API key at `~/.polaris/api_key.json`.
//! Completely independent of `Credentials`/`auth.json` — the codex OAuth
//! store is never read or written by this module, and vice versa.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::AuthError;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Stored {
    key: String,
}

/// The default storage location. `~/.polaris/api_key.json`.
pub fn default_path() -> Result<PathBuf, AuthError> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| AuthError::Io(std::io::Error::other("HOME is not set")))?;
    Ok(Path::new(&home).join(".polaris").join("api_key.json"))
}

/// Saves. Same atomic tmp+rename, 0600-both-ways pattern as
/// `store::save_to` — see that function's comments for why both the
/// open-time mode and the post-write `set_permissions` call are needed.
pub fn save_to(path: &Path, key: &str) -> Result<(), AuthError> {
    use std::io::Write;

    let mut protection = crate::protection::lock()?;
    let path = protection.register_store(path)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_vec_pretty(&Stored {
        key: key.to_string(),
    })
    .map_err(|_| AuthError::Decode("could not serialize the API key".into()))?;

    // Nonblocking open and regular-file checks precede truncation/body I/O.
    // The helper creates new files at 0600; fchmod also tightens reused tmp files.
    let mut f = crate::protection::open_regular(&tmp, true)?;
    protection.remember(&f.metadata()?)?;
    f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    f.set_len(0)?;
    f.write_all(&body)?;
    f.sync_all()?;
    drop(f);
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&tmp, &path)?;
    protection.register_store(&path)?;
    Ok(())
}

/// Reads. Nonexistence is not a failure. Corruption is.
pub fn load_from(path: &Path) -> Result<Option<String>, AuthError> {
    let mut protection = crate::protection::lock()?;
    let path = protection.register_store(path)?;
    let file = match crate::protection::open_regular(&path, false) {
        Ok(file) => file,
        Err(AuthError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    protection.remember(&file.metadata()?)?;
    let body = crate::protection::read_body(file)?;
    let stored: Stored = serde_json::from_slice(&body)
        .map_err(|_| AuthError::Decode("could not parse the API key".into()))?;
    Ok(Some(stored.key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_key_round_trips() {
        let dir = tempfile::tempdir().expect("temp dir");
        let p = dir.path().join("api_key.json");
        save_to(&p, "sk-example").expect("failed to save");
        let got = load_from(&p).expect("failed to read").expect("missing");
        assert_eq!(got, "sk-example");
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let got = load_from(&dir.path().join("nope.json")).expect("nonexistence is not a failure");
        assert!(got.is_none());
    }

    #[test]
    fn the_saved_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("temp dir");
        let p = dir.path().join("api_key.json");
        save_to(&p, "sk-example").expect("failed to save");
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

    #[test]
    fn overwriting_replaces_the_stored_key() {
        let dir = tempfile::tempdir().expect("temp dir");
        let p = dir.path().join("api_key.json");
        save_to(&p, "sk-first").expect("failed to save");
        save_to(&p, "sk-second").expect("failed to save");
        let got = load_from(&p).expect("failed to read").expect("missing");
        assert_eq!(got, "sk-second");
    }

    #[test]
    fn a_corrupt_file_is_an_error_not_a_silent_absence() {
        let dir = tempfile::tempdir().expect("temp dir");
        let p = dir.path().join("api_key.json");
        std::fs::write(&p, b"{ not json").expect("failed to write");
        let err = load_from(&p).expect_err("a corrupt file should fail");
        assert!(
            matches!(err, AuthError::Decode(_)),
            "corrupt file resulted in something other than Decode: {err:?}"
        );
    }
}
