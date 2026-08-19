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
}
