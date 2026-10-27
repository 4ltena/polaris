//! Turning the Jev decider on from the onboarding screen.
//!
//! The user's `config.toml` is appended to, never rewritten: existing
//! settings and comments survive byte for byte. Anything the file already
//! says about `decision` wins, and a file that does not parse is left alone,
//! because appending a second `[decision]` table would make every later
//! start fail on a duplicate key.

use std::io::Write;
use std::path::Path;

/// What enabling did to the file. Only `Appended` and `Created` changed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnableOutcome {
    Appended,
    Created,
    /// `decision` is already configured in some form; nothing was changed.
    AlreadyConfigured,
    /// The file does not parse as TOML; nothing was changed.
    Unparsable,
    /// The file is a symlink; it is not replaced by a regular file.
    Symlink,
}

/// The user layer `crate::config::load` reads: `$HOME/.polaris/config.toml`.
pub fn user_config_path() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(|home| {
        std::path::PathBuf::from(home)
            .join(".polaris")
            .join("config.toml")
    })
}

pub const JEV_SECTION: &str = "[decision]\nenabled = true\nbackend = \"jev\"\n";

pub fn enable_jev(config_path: &Path) -> std::io::Result<EnableOutcome> {
    let original = match std::fs::symlink_metadata(config_path) {
        Ok(metadata) if metadata.file_type().is_symlink() => return Ok(EnableOutcome::Symlink),
        Ok(metadata) if !metadata.is_file() => {
            return Err(std::io::Error::other("config.toml is not a regular file"));
        }
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let (body, outcome) = match &original {
        None => (JEV_SECTION.to_owned(), EnableOutcome::Created),
        Some(_) => {
            let text = std::fs::read_to_string(config_path)?;
            let Ok(table) = text.parse::<toml::Table>() else {
                return Ok(EnableOutcome::Unparsable);
            };
            if table.contains_key("decision") {
                return Ok(EnableOutcome::AlreadyConfigured);
            }
            let separator = if text.is_empty() || text.ends_with('\n') {
                "\n"
            } else {
                "\n\n"
            };
            let body = if text.is_empty() {
                JEV_SECTION.to_owned()
            } else {
                format!("{text}{separator}{JEV_SECTION}")
            };
            (body, EnableOutcome::Appended)
        }
    };
    // The result must load, or the next start would refuse the whole config.
    body.parse::<toml::Table>()
        .map_err(|_| std::io::Error::other("the appended config.toml would not parse"))?;

    let parent = config_path
        .parent()
        .ok_or_else(|| std::io::Error::other("config.toml has no parent directory"))?;
    std::fs::create_dir_all(parent)?;
    let tmp = config_path.with_file_name("config.toml.polaris-tmp");
    match std::fs::symlink_metadata(&tmp) {
        Ok(metadata) if metadata.is_file() => std::fs::remove_file(&tmp)?,
        Ok(_) => {
            return Err(std::io::Error::other(
                "config.toml.polaris-tmp is not a regular file",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let written = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            let mode = original
                .as_ref()
                .map_or(0o600, |metadata| metadata.permissions().mode() & 0o777);
            options.mode(mode).custom_flags(libc::O_NOFOLLOW);
            let mut file = options.open(&tmp)?;
            file.set_permissions(std::fs::Permissions::from_mode(mode))?;
            file.write_all(body.as_bytes())?;
            file.sync_all()?;
        }
        #[cfg(not(unix))]
        {
            let mut file = options.open(&tmp)?;
            file.write_all(body.as_bytes())?;
            file.sync_all()?;
        }
        // ponytail: no file lock; a writer that lands between this check and
        // the rename is lost. Add an advisory lock if two writers become real.
        let unchanged = match (&original, std::fs::symlink_metadata(config_path)) {
            (None, Err(error)) => error.kind() == std::io::ErrorKind::NotFound,
            (Some(before), Ok(now)) => {
                now.is_file()
                    && now.len() == before.len()
                    && now.modified()? == before.modified()?
            }
            _ => false,
        };
        if !unchanged {
            return Err(std::io::Error::other(
                "config.toml changed while it was being updated",
            ));
        }
        std::fs::rename(&tmp, config_path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written.map(|()| outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, text).unwrap();
        (dir, path)
    }

    #[test]
    fn appends_after_existing_settings_and_keeps_them_byte_for_byte() {
        let original = "# owner note\n[embedding]\nruntime = \"/x\"\n";
        let (_dir, path) = fixture(original);
        assert_eq!(enable_jev(&path).unwrap(), EnableOutcome::Appended);
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.starts_with(original));
        assert_eq!(&after[original.len()..], format!("\n{JEV_SECTION}"));
        let loaded = crate::config::try_load_from(Some(&path), None).unwrap();
        assert!(loaded.decision.enabled);
        assert!(!path.with_file_name("config.toml.polaris-tmp").exists());
    }

    #[test]
    fn a_file_without_a_final_newline_still_gets_a_valid_table() {
        let original = "[embedding]\nruntime = \"/x\"";
        let (_dir, path) = fixture(original);
        assert_eq!(enable_jev(&path).unwrap(), EnableOutcome::Appended);
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.starts_with(original));
        assert!(
            crate::config::try_load_from(Some(&path), None)
                .unwrap()
                .decision
                .enabled
        );
    }

    #[test]
    fn a_missing_file_is_created_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert_eq!(enable_jev(&path).unwrap(), EnableOutcome::Created);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), JEV_SECTION);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn any_existing_decision_setting_or_an_unparsable_file_is_left_untouched() {
        for (text, expected) in [
            (
                "[decision]\nenabled = false\n",
                EnableOutcome::AlreadyConfigured,
            ),
            (
                "decision.enabled = true\n",
                EnableOutcome::AlreadyConfigured,
            ),
            ("[decision.fallback]\n", EnableOutcome::AlreadyConfigured),
            (
                "decision = { enabled = false }\n",
                EnableOutcome::AlreadyConfigured,
            ),
            (
                "[ decision ]\nenabled = false\n",
                EnableOutcome::AlreadyConfigured,
            ),
            ("[embedding\nbroken", EnableOutcome::Unparsable),
        ] {
            let (_dir, path) = fixture(text);
            assert_eq!(enable_jev(&path).unwrap(), expected, "{text}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), text, "{text}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_config_is_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("dotfiles-config.toml");
        std::fs::write(&target, "[embedding]\n").unwrap();
        let link = dir.path().join("config.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(enable_jev(&link).unwrap(), EnableOutcome::Symlink);
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "[embedding]\n");
    }

    #[cfg(unix)]
    #[test]
    fn the_original_mode_is_kept() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, path) = fixture("[embedding]\n");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        enable_jev(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }
}
