//! Temporary projects only; the apply owner is exercised through the copy.
use super::*;

fn limits() -> Limits {
    Limits {
        max_entries: 256,
        max_files: 128,
        max_file_bytes: 64 * 1024,
        max_total_bytes: 1024 * 1024,
        max_depth: 8,
    }
}

fn tree(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    for (path, body) in files {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }
    (temporary, root)
}

#[test]
fn a_file_the_shared_history_never_held_is_left_alone() {
    // The agreed tree does not hold `notes.md`, but nobody removed it either:
    // it is this device's own file, written after the last exchange.
    let (_project, project_root) = tree(&[("src/main.rs", "古い\n"), ("notes.md", "手元の新規\n")]);
    let (_agreed, agreed) = tree(&[("src/main.rs", "新しい\n")]);
    let manifest = ShareManifest::default();
    let secrets = RegisteredSecrets::new([]).unwrap();
    let changed = Changed {
        written: vec!["src/main.rs".into()],
        removed: Vec::new(),
    };
    let (snapshot, shaped) = shape_copy(
        &project_root,
        &agreed,
        &manifest,
        &changed,
        &secrets,
        limits(),
    )
    .unwrap();

    assert_eq!(shaped.removed, Vec::<PathBuf>::new());
    assert_eq!(
        fs::read_to_string(snapshot.path().join("notes.md")).unwrap(),
        "手元の新規\n",
    );
}

#[test]
fn the_agreed_tree_decides_shared_files_only_and_the_project_is_not_written() {
    let (_project, project_root) = tree(&[
        ("src/main.rs", "古い\n"),
        ("local-only.md", "手元だけ\n"),
        ("gone.md", "消える\n"),
    ]);
    // Something the manifest never shares must survive untouched.
    fs::create_dir_all(project_root.join(".polaris")).unwrap();
    fs::write(project_root.join(".polaris/state.json"), "{}").unwrap();
    let (_agreed, agreed) = tree(&[("src/main.rs", "新しい\n"), ("added.md", "追加\n")]);

    let manifest = ShareManifest {
        version: 1,
        excluded: vec!["local-only.md".into()],
    };
    let secrets = RegisteredSecrets::new([]).unwrap();
    let changed = Changed {
        written: vec!["src/main.rs".into(), "added.md".into()],
        removed: vec!["gone.md".into()],
    };
    let (snapshot, shaped) = shape_copy(
        &project_root,
        &agreed,
        &manifest,
        &changed,
        &secrets,
        limits(),
    )
    .unwrap();

    let mut written = shaped.written.clone();
    written.sort();
    assert_eq!(
        written,
        vec![PathBuf::from("added.md"), PathBuf::from("src/main.rs")]
    );
    assert_eq!(shaped.removed, vec![PathBuf::from("gone.md")]);

    // The copy holds the agreement; the project still holds what it had.
    let copy = snapshot.path();
    assert_eq!(
        fs::read_to_string(copy.join("src/main.rs")).unwrap(),
        "新しい\n"
    );
    assert!(!copy.join("gone.md").exists());
    assert_eq!(
        fs::read_to_string(copy.join("local-only.md")).unwrap(),
        "手元だけ\n",
        "an excluded file is never replaced or removed"
    );
    assert_eq!(
        fs::read_to_string(project_root.join("src/main.rs")).unwrap(),
        "古い\n",
        "shaping the copy must not write to the project"
    );
    assert!(project_root.join("gone.md").exists());
    assert!(project_root.join(".polaris/state.json").exists());

    // What the apply owner would carry out: the two writes and the removal.
    let changes = planned_changes(&snapshot, &secrets, limits()).unwrap();
    let mut paths: Vec<String> = changes
        .changes
        .iter()
        .map(|change| change.relative_path.to_string_lossy().into_owned())
        .collect();
    paths.sort();
    assert_eq!(
        paths,
        vec![
            "added.md".to_string(),
            "gone.md".into(),
            "src/main.rs".into()
        ]
    );
}
