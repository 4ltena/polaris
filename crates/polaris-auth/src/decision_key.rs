//! Storage for the decider's API key at `~/.polaris/decision_keys.json`.
//! Kept out of `.env` files and `config.toml`: the same owner-only store
//! as `api_key.rs`, registered with the same protection.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::AuthError;

/// Deliberately no `Debug`: a stray `{:?}` must not print the key.
#[cfg(unix)]
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    jev: String,
}

/// The default storage location. `~/.polaris/decision_keys.json`.
#[cfg(unix)]
pub fn default_path() -> Result<PathBuf, AuthError> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| AuthError::Io(std::io::Error::other("HOME is not set")))?;
    Ok(Path::new(&home).join(".polaris").join("decision_keys.json"))
}

/// Saves the Jev key. Same atomic tmp+rename, 0600-both-ways pattern as
/// `api_key::save_to`.
#[cfg(unix)]
pub fn save_jev_to(path: &Path, key: &str) -> Result<(), AuthError> {
    use std::io::Write;

    let mut protection = crate::protection::lock()?;
    let path = protection.register_store(path)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_vec_pretty(&Stored {
        jev: key.to_string(),
    })
    .map_err(|_| AuthError::Decode("could not serialize the decider key".into()))?;

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

/// Reads the Jev key. Nonexistence is not a failure. Corruption is, and its
/// message never carries the file's contents.
#[cfg(unix)]
pub fn load_jev_from(path: &Path) -> Result<Option<String>, AuthError> {
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
        .map_err(|_| AuthError::Decode("could not parse the decider key file".into()))?;
    Ok(Some(stored.jev).filter(|key| !key.is_empty()))
}

#[cfg(not(unix))]
pub fn default_path() -> Result<PathBuf, AuthError> {
    Err(AuthError::Protection)
}

#[cfg(not(unix))]
pub fn save_jev_to(_path: &Path, _key: &str) -> Result<(), AuthError> {
    Err(AuthError::Protection)
}

#[cfg(not(unix))]
pub fn load_jev_from(_path: &Path) -> Result<Option<String>, AuthError> {
    Err(AuthError::Protection)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_saved_key_round_trips_owner_only() {
        let dir = tempfile::tempdir().expect("temp dir");
        let p = dir.path().join("decision_keys.json");
        save_jev_to(&p, "jev-sentinel-0123").expect("failed to save");
        assert_eq!(
            load_jev_from(&p).expect("read").as_deref(),
            Some("jev-sentinel-0123")
        );
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
        assert!(!p.with_extension("json.tmp").exists());
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert!(
            load_jev_from(&dir.path().join("nope.json"))
                .expect("absence")
                .is_none()
        );
    }

    #[test]
    fn overwriting_replaces_the_stored_key() {
        let dir = tempfile::tempdir().expect("temp dir");
        let p = dir.path().join("decision_keys.json");
        save_jev_to(&p, "first").expect("save");
        save_jev_to(&p, "second").expect("save");
        assert_eq!(load_jev_from(&p).expect("read").as_deref(), Some("second"));
    }

    #[test]
    fn a_corrupt_file_fails_without_echoing_its_contents() {
        let dir = tempfile::tempdir().expect("temp dir");
        let p = dir.path().join("decision_keys.json");
        std::fs::write(&p, b"{ \"jev\": jev-sentinel-0123").expect("write");
        let err = load_jev_from(&p).expect_err("corrupt");
        assert!(matches!(err, AuthError::Decode(_)), "{err}");
        assert!(!err.to_string().contains("jev-sentinel-0123"));
    }
}
