#![cfg(unix)]
//! Temporary projects only; the synchronization repository is local.
use super::*;
use std::os::unix::fs::symlink;

fn project() -> (tempfile::TempDir, PathBuf) {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    for directory in [
        ".git",
        ".git/refs",
        "src",
        "node_modules/pkg",
        "target/debug",
    ] {
        fs::create_dir_all(root.join(directory)).unwrap();
    }
    fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(root.join(".git/config"), "[core]\n").unwrap();
    fs::write(root.join(".env"), "SECRET=1\n").unwrap();
    fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    fs::write(root.join("README.md"), "共有する\n").unwrap();
    fs::write(root.join("node_modules/pkg/index.js"), "x\n").unwrap();
    fs::write(root.join("target/debug/app"), "binary\n").unwrap();
    symlink("/etc/passwd", root.join("link")).unwrap();
    (temporary, root)
}

/// Every file under the project's own repository, with its bytes.
fn git_directory(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut found = Vec::new();
    let mut pending = vec![root.join(".git")];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                pending.push(entry.path());
            } else {
                found.push((entry.path(), fs::read(entry.path()).unwrap()));
            }
        }
    }
    found.sort();
    found
}

#[test]
fn only_shared_files_are_recorded_and_the_project_repository_is_untouched() {
    let (_temporary, root) = project();
    let state = tempfile::tempdir().unwrap();
    let paths = SyncPaths::under(state.path());
    assert!(!paths.overlaps(&root));
    let before = git_directory(&root);

    let repository = SyncRepository::open(paths).unwrap();
    let staged = repository
        .stage_from(&root, &ShareManifest::default())
        .unwrap();
    assert_eq!(staged.files, 2, "src/main.rs and README.md");
    assert_eq!(staged.skipped, vec![PathBuf::from("link")]);

    let commit = repository.commit("first").unwrap().expect("a first commit");
    let mut files = repository.files_at(&commit).unwrap();
    files.sort();
    assert_eq!(files, vec!["README.md".to_string(), "src/main.rs".into()]);

    // The project's own repository is byte-for-byte what it was.
    assert_eq!(git_directory(&root), before);
    // Nothing was written into the project either.
    assert!(!root.join(".polaris").exists());
}

#[test]
fn a_removal_leaves_the_shared_tree_and_an_unchanged_project_makes_no_commit() {
    let (_temporary, root) = project();
    let state = tempfile::tempdir().unwrap();
    let repository = SyncRepository::open(SyncPaths::under(state.path())).unwrap();
    let manifest = ShareManifest::default();
    repository.stage_from(&root, &manifest).unwrap();
    let first = repository.commit("first").unwrap().unwrap();

    // Staging again with no change records nothing new.
    repository.stage_from(&root, &manifest).unwrap();
    assert_eq!(repository.commit("again").unwrap(), None);
    assert_eq!(repository.head().as_deref(), Some(first.as_str()));

    fs::remove_file(root.join("README.md")).unwrap();
    repository.stage_from(&root, &manifest).unwrap();
    let second = repository.commit("removed").unwrap().expect("a change");
    assert_eq!(
        repository.files_at(&second).unwrap(),
        vec!["src/main.rs".to_string()]
    );

    // What changed between the two agreed states, and nothing else.
    assert_eq!(
        repository.changed_paths(Some(&first), &second).unwrap(),
        (Vec::new(), vec!["README.md".to_string()])
    );
    // Before anything has been applied there is nothing to remove.
    let (written, removed) = repository.changed_paths(None, &first).unwrap();
    assert_eq!(removed, Vec::<String>::new());
    assert!(written.contains(&"README.md".to_string()));
}
