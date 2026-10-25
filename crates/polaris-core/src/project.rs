//! Resolves the project root.
//!
//! From M2 onward, the writable root is derived from the value this returns.
//! Using the working directory as the root outright would mean the writable
//! range shifts just because the process was launched from somewhere deep in
//! the repository. We don't walk up to `/` when no marker is found, because
//! doing so would make the writable root the entire filesystem.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

/// Marker directories. The nearest one wins.
const MARKERS: &[&str] = &[".git", ".polaris"];

pub fn resolve_root(start: &Path) -> PathBuf {
    resolve_root_below(
        start,
        std::env::var_os("HOME").map(PathBuf::from).as_deref(),
    )
}

/// The walk, with the home directory passed in so a test can name one.
///
/// **The search never adopts the home directory from below.** Both markers
/// exist there in ordinary setups: `polaris login` writes
/// `$HOME/.polaris/auth.json`, and a dotfiles repository puts `.git`
/// straight in `$HOME`. Without this stop, the first login silently turns
/// every markerless directory under home into a project whose root is
/// `$HOME` — so the default `workspace-write` sandbox would take the whole
/// home directory as writable, and one state directory, audit log and
/// memory store would serve every such directory at once. The same
/// directory would behave differently before and after that first login.
///
/// Home is still honoured when it is where the run actually started: that
/// is the caller naming it, not the search wandering into it.
pub(crate) fn resolve_root_below(start: &Path, home: Option<&Path>) -> PathBuf {
    let canonical = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());
    let home = home.map(|home| home.canonicalize().unwrap_or_else(|_| home.to_path_buf()));

    let mut cursor: &Path = &canonical;
    loop {
        // Home ends the walk. Reaching it means either the run started
        // there — and `canonical` is home, so returning it is the same
        // answer — or the search climbed out of the project, which is the
        // case this exists to stop. Nothing above home is a project root
        // either: a marker in `/Users` would otherwise capture every
        // account on the machine.
        if home.as_deref() == Some(cursor) {
            return canonical;
        }
        if MARKERS.iter().any(|m| cursor.join(m).exists()) {
            return cursor.to_path_buf();
        }
        match cursor.parent() {
            Some(p) => cursor = p,
            None => return canonical,
        }
    }
}

/// Builds a deterministic project identifier from a canonicalized path.
///
/// We don't use the standard library's `DefaultHasher`, since it doesn't
/// specify its algorithm and can change across Rust versions (which would
/// split the same project's audit log into a different directory). We
/// write out FNV-1a directly instead, to pin the algorithm.
pub fn project_id(canonical_path: &Path) -> String {
    const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = FNV_OFFSET_BASIS;
    for byte in canonical_path.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{hash:016x}")
}

/// Resolves the root used only for state and session storage.
///
/// An explicit `POLARIS_DATA_DIR` must be an absolute, non-empty path. It is
/// intentionally separate from `HOME`: authentication and credentials retain
/// their existing location under `HOME`.
fn storage_root(data_dir: Option<&OsStr>, home: Option<&OsStr>) -> std::io::Result<PathBuf> {
    match data_dir {
        Some(data_dir) if data_dir.is_empty() => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "POLARIS_DATA_DIR must not be empty",
        )),
        Some(data_dir) => {
            let root = PathBuf::from(data_dir);
            if !root.is_absolute() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "POLARIS_DATA_DIR must be an absolute path",
                ));
            }
            Ok(root)
        }
        None => home
            .map(|home| Path::new(home).join(".polaris"))
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "HOME is not set")),
    }
}

/// Serializes every test that swaps `POLARIS_DATA_DIR`. The variable is
/// process-wide, so a lock per test module excludes nothing: two modules
/// each holding their own mutex will still overwrite each other's
/// directory, and the loser sees its files vanish mid-test. One lock,
/// beside the variable it guards.
#[cfg(test)]
pub(crate) static DATA_DIR_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(crate) fn storage_root_from_env() -> std::io::Result<PathBuf> {
    let data_dir = std::env::var_os("POLARIS_DATA_DIR");
    let home = std::env::var_os("HOME");
    storage_root(data_dir.as_deref(), home.as_deref())
}

fn state_dir_at(storage_root: &Path, cwd: &Path) -> std::io::Result<PathBuf> {
    let root = resolve_root(cwd);
    let id = project_id(&root);
    let dir = storage_root.join("state").join(id);
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&dir)?;
    Ok(dir)
}

/// The project's state directory. By default this is
/// `~/.polaris/state/<project-id>/`; when `POLARIS_DATA_DIR` is set it is
/// `<POLARIS_DATA_DIR>/state/<project-id>/`. The directory is created if
/// missing. `<project-id>` is derived from the resolved project root
/// (`resolve_root`), not the working directory — see `project_id`'s docs on
/// why launch location must not split a project's state across directories.
pub fn state_dir(cwd: &Path) -> std::io::Result<PathBuf> {
    state_dir_at(&storage_root_from_env()?, cwd)
}

/// Compute the same project state location without creating or repairing it.
pub fn state_path(cwd: &Path) -> std::io::Result<PathBuf> {
    Ok(storage_root_from_env()?
        .join("state")
        .join(project_id(&resolve_root(cwd))))
}

fn sessions_dir_at(storage_root: &Path) -> std::io::Result<PathBuf> {
    let dir = storage_root.join("sessions");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// The directory holding every saved TUI conversation, across every
/// project: `~/.polaris/sessions/` by default or
/// `<POLARIS_DATA_DIR>/sessions/` when overridden, created if missing. Unlike
/// `state_dir`, this is not scoped to one project's hashed id — `/resume`
/// needs to list conversations from every directory polaris has ever run
/// in, not just the current one, so conversations live in one shared
/// pool and carry their originating directory as metadata instead (see
/// `polaris-tui::persist::SessionMeta`).
pub fn sessions_dir() -> std::io::Result<PathBuf> {
    sessions_dir_at(&storage_root_from_env()?)
}

fn desktop_store_root_at(storage_root: &Path) -> std::io::Result<PathBuf> {
    let dir = storage_root.join("desktop");
    std::fs::create_dir_all(&dir)?;
    // `DesktopRoot::open_owned` requires the directory to be this user's and
    // unreadable by anyone else, so the permissions are set rather than
    // hoped for: `create_dir_all` honours the umask, which may leave the
    // group and other bits on.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    // The same call also insists on an absolute canonical path.
    dir.canonicalize()
}

/// The durable store a desktop session writer opens:
/// `~/.polaris/desktop/` by default, or `<POLARIS_DATA_DIR>/desktop/`.
/// Created with the owner-only permissions the store itself checks for.
///
/// This is the one place the location is decided. The CLI resolves it here;
/// the macOS app still passes its own `--store-root` from Application
/// Support and has not been moved, so a recipe ledger written from the
/// terminal and one written from the app do not yet meet. The consequence
/// is that finished steps are not shared between the two entry points —
/// neither ever reuses the other's record wrongly.
pub fn desktop_store_root() -> std::io::Result<PathBuf> {
    desktop_store_root_at(&storage_root_from_env()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn new_project_state_is_private_without_repairing_existing_permissions() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let storage = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let state = state_dir_at(storage.path(), project.path()).unwrap();
        for path in [storage.path().join("state"), state.clone()] {
            assert_eq!(std::fs::metadata(path).unwrap().mode() & 0o7777, 0o700);
        }
        let before = std::fs::metadata(&state).unwrap();
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(state_dir_at(storage.path(), project.path()).unwrap(), state);
        let after = std::fs::metadata(&state).unwrap();
        assert_eq!((after.dev(), after.ino()), (before.dev(), before.ino()));
        assert_eq!(after.mode() & 0o7777, 0o755);
    }

    #[test]
    fn a_subdirectory_resolves_to_the_repository_root() {
        let root = tempfile::tempdir().expect("temp directory");
        std::fs::create_dir(root.path().join(".git")).expect("mkdir");
        let deep = root.path().join("crates/polaris-core/src");
        std::fs::create_dir_all(&deep).expect("mkdir");

        assert_eq!(
            resolve_root(&deep),
            root.path().canonicalize().expect("canonicalize")
        );
    }

    #[test]
    fn a_polaris_directory_also_marks_the_root() {
        // Not everyone uses git. If a `.polaris/` directory exists, treat it as the root.
        let root = tempfile::tempdir().expect("temp directory");
        std::fs::create_dir(root.path().join(".polaris")).expect("mkdir");
        let deep = root.path().join("a/b");
        std::fs::create_dir_all(&deep).expect("mkdir");

        assert_eq!(
            resolve_root(&deep),
            root.path().canonicalize().expect("canonicalize")
        );
    }

    #[test]
    fn the_nearest_marker_wins() {
        // For a nested repository, the inner one wins. Choosing the outer one
        // would make the writable root broader than intended.
        let outer = tempfile::tempdir().expect("temp directory");
        std::fs::create_dir(outer.path().join(".git")).expect("mkdir");
        let inner = outer.path().join("vendor/thing");
        std::fs::create_dir_all(inner.join(".git")).expect("mkdir");
        let deep = inner.join("src");
        std::fs::create_dir_all(&deep).expect("mkdir");

        assert_eq!(
            resolve_root(&deep),
            inner.canonicalize().expect("canonicalize")
        );
    }

    #[test]
    fn without_any_marker_the_starting_directory_is_the_root() {
        // Walking up to `/` when no marker is found would make the writable
        // root the entire filesystem. The walk must always stop.
        let dir = tempfile::tempdir().expect("temp directory");
        let deep = dir.path().join("x/y");
        std::fs::create_dir_all(&deep).expect("mkdir");

        assert_eq!(
            resolve_root(&deep),
            deep.canonicalize().expect("canonicalize")
        );
    }

    /// The search must not adopt home from below, for either marker.
    ///
    /// `polaris login` writes `$HOME/.polaris/auth.json`, and a dotfiles
    /// repository puts `.git` in `$HOME`. Either one used to make every
    /// markerless directory under home resolve to `$HOME`, which under the
    /// default `workspace-write` hands the sandbox the whole home
    /// directory. The same directory behaved differently before and after
    /// the first login.
    #[test]
    fn the_search_never_adopts_the_home_directory_from_below() {
        for marker in [".polaris", ".git"] {
            let home = tempfile::tempdir().expect("home");
            std::fs::create_dir_all(home.path().join(marker)).expect("marker");
            let work = home.path().join("work").join("deep");
            std::fs::create_dir_all(&work).expect("work");

            let resolved = resolve_root_below(&work, Some(home.path()));
            let expected = work.canonicalize().expect("canonical");
            assert_eq!(
                resolved, expected,
                "{marker} in home must not become the root of a directory below it"
            );
            assert_ne!(
                resolved,
                home.path().canonicalize().expect("canonical home"),
                "resolving to home would make the sandbox writable across it"
            );
        }
    }

    /// A run that started in home still gets home, and nothing above it.
    ///
    /// The stop returns the starting directory, which for this case *is*
    /// home — so the run is not left rootless. And a marker above home
    /// never wins: one in `/Users` would otherwise make every account on
    /// the machine one project.
    #[test]
    fn a_run_started_in_home_gets_home_and_never_anything_above_it() {
        let above = tempfile::tempdir().expect("above");
        std::fs::create_dir_all(above.path().join(".git")).expect("marker above home");
        let home = above.path().join("someone");
        std::fs::create_dir_all(&home).expect("home");
        let canonical = home.canonicalize().expect("canonical");

        // Home itself carries no marker, so without the stop the walk
        // would climb past it and adopt the directory holding every
        // account.
        assert_eq!(resolve_root_below(&home, Some(&home)), canonical);

        // And with a marker of its own it is still home, not something above.
        std::fs::create_dir_all(home.join(".polaris")).expect("marker");
        assert_eq!(resolve_root_below(&home, Some(&home)), canonical);
    }

    /// The stop applies to home alone. A real project between the start and
    /// home is still found, even though home lies above it.
    #[test]
    fn a_project_below_home_is_still_found() {
        let home = tempfile::tempdir().expect("home");
        std::fs::create_dir_all(home.path().join(".polaris")).expect("home marker");
        let project = home.path().join("code").join("thing");
        std::fs::create_dir_all(project.join(".git")).expect("project marker");
        let deep = project.join("src").join("inner");
        std::fs::create_dir_all(&deep).expect("deep");

        assert_eq!(
            resolve_root_below(&deep, Some(home.path())),
            project.canonicalize().expect("canonical project")
        );
    }

    #[test]
    fn project_id_is_deterministic_for_the_same_path() {
        let a = project_id(Path::new("/w/polaris"));
        let b = project_id(Path::new("/w/polaris"));
        assert_eq!(a, b);
    }

    #[test]
    fn project_id_differs_for_different_paths() {
        let a = project_id(Path::new("/w/polaris"));
        let b = project_id(Path::new("/w/other"));
        assert_ne!(a, b);
    }

    #[test]
    fn default_storage_paths_remain_under_home() {
        let home = tempfile::tempdir().expect("temp directory");
        let project = tempfile::tempdir().expect("temp directory");
        std::fs::create_dir(project.path().join(".git")).expect("mkdir");

        let storage = storage_root(None, Some(home.path().as_os_str())).expect("storage root");
        let state = state_dir_at(&storage, project.path()).expect("state dir");
        let sessions = sessions_dir_at(&storage).expect("sessions dir");
        let project_root = project.path().canonicalize().expect("canonicalize");

        assert_eq!(storage, home.path().join(".polaris"));
        assert_eq!(state, storage.join("state").join(project_id(&project_root)));
        assert_eq!(sessions, storage.join("sessions"));
        assert!(state.is_dir());
        assert!(sessions.is_dir());
    }

    #[test]
    fn explicit_data_dir_contains_state_and_sessions_without_changing_home() {
        let home = tempfile::tempdir().expect("temp directory");
        let data = tempfile::tempdir().expect("temp directory");
        let project = tempfile::tempdir().expect("temp directory");
        std::fs::create_dir(project.path().join(".git")).expect("mkdir");
        let home_before = std::env::var_os("HOME");

        let storage = storage_root(Some(data.path().as_os_str()), Some(home.path().as_os_str()))
            .expect("storage root");
        let state = state_dir_at(&storage, project.path()).expect("state dir");
        let sessions = sessions_dir_at(&storage).expect("sessions dir");
        let project_root = project.path().canonicalize().expect("canonicalize");

        assert_eq!(storage, data.path());
        assert_eq!(
            state,
            data.path().join("state").join(project_id(&project_root))
        );
        assert_eq!(sessions, data.path().join("sessions"));
        assert!(!home.path().join(".polaris").exists());
        assert_eq!(std::env::var_os("HOME"), home_before);
    }

    #[test]
    fn invalid_explicit_data_dir_is_rejected_without_home_fallback() {
        let home = tempfile::tempdir().expect("temp directory");
        for data_dir in [OsStr::new(""), OsStr::new("relative/data")] {
            let error = storage_root(Some(data_dir), Some(home.path().as_os_str()))
                .expect_err("invalid explicit data directory must fail");
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        }
        assert!(!home.path().join(".polaris").exists());
    }
}
