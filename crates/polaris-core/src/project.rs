//! Resolves the project root.
//!
//! From M2 onward, the writable root is derived from the value this returns.
//! Using the working directory as the root outright would mean the writable
//! range shifts just because the process was launched from somewhere deep in
//! the repository. We don't walk up to `/` when no marker is found, because
//! doing so would make the writable root the entire filesystem.

use std::path::{Path, PathBuf};

/// Marker directories. The nearest one wins.
const MARKERS: &[&str] = &[".git", ".polaris"];

pub fn resolve_root(start: &Path) -> PathBuf {
    let canonical = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());

    let mut cursor: &Path = &canonical;
    loop {
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

/// The project's state directory: `~/.polaris/state/<project-id>/`, created
/// if missing. `<project-id>` is derived from the resolved project root
/// (`resolve_root`), not the working directory — see `project_id`'s docs on
/// why launch location must not split a project's state across directories.
pub fn state_dir(cwd: &Path) -> std::io::Result<PathBuf> {
    let root = resolve_root(cwd);
    let id = project_id(&root);

    let home = std::env::var_os("HOME")
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "HOME is not set"))?;
    let dir = Path::new(&home).join(".polaris").join("state").join(id);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// The directory holding every saved TUI conversation, across every
/// project: `~/.polaris/sessions/`, created if missing. Unlike
/// `state_dir`, this is not scoped to one project's hashed id — `/resume`
/// needs to list conversations from every directory polaris has ever run
/// in, not just the current one, so conversations live in one shared
/// pool and carry their originating directory as metadata instead (see
/// `polaris-tui::persist::SessionMeta`).
pub fn sessions_dir() -> std::io::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "HOME is not set"))?;
    let dir = Path::new(&home).join(".polaris").join("sessions");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    // `sessions_dir` reads `HOME`, which is process-global state — swapping
    // it races with any other test doing the same unless serialized.
    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn sessions_dir_is_shared_across_projects_not_hashed_per_project() {
        let home = tempfile::tempdir().expect("temp directory");
        let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os("HOME");
        // SAFETY: serialized by `HOME_LOCK`; restored before the guard drops.
        unsafe {
            std::env::set_var("HOME", home.path());
        }

        let dir = sessions_dir().expect("sessions dir");

        match prev {
            Some(p) => unsafe { std::env::set_var("HOME", p) },
            None => unsafe { std::env::remove_var("HOME") },
        }

        assert_eq!(dir, home.path().join(".polaris").join("sessions"));
        assert!(dir.is_dir());
    }
}
