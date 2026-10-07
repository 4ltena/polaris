//! Reads the repository file ledger from Git or an explicitly prefetched listing.

use std::collections::HashSet;
use std::path::{Component, Path};
use std::process::Command;

pub fn repository_files(root: &Path, rust_only: bool) -> Vec<String> {
    // Policy-constrained runners acquire this listing separately. An invalid
    // supplied listing must fail, rather than fall back to a Git subprocess.
    let listing = if let Some(path) = std::env::var_os("POLARIS_TEST_REPOSITORY_FILE_LIST") {
        std::fs::read_to_string(path).expect("cannot read prefetched repository file listing")
    } else {
        let output = Command::new("git")
            .args(["ls-files", "--cached", "--others", "--exclude-standard"])
            .current_dir(root)
            .output()
            .expect("git ls-files runs");
        assert!(output.status.success(), "git ls-files failed");
        String::from_utf8(output.stdout).expect("utf-8 repository paths")
    };
    assert!(!listing.trim().is_empty(), "empty repository file listing");
    let mut seen = HashSet::new();
    listing
        .lines()
        .map(|path| {
            assert!(
                !path.is_empty()
                    && !path.contains('\0')
                    && Path::new(path)
                        .components()
                        .all(|part| matches!(part, Component::Normal(_))),
                "repository file listing must contain relative file paths"
            );
            assert!(seen.insert(path), "duplicate repository file listing entry");
            path.to_owned()
        })
        .filter(|path| !rust_only || path.ends_with(".rs"))
        .collect()
}
