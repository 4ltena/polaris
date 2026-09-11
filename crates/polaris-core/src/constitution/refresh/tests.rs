//! Request-boundary instruction reloads and protected source refusal tests.
use super::*;
use std::os::unix::fs::{PermissionsExt, symlink};

fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    (dir, root)
}
fn rules(path: &Path, text: &str) {
    fs::write(
        path,
        format!("## Always on\n{text}\n## Other\nnot injected"),
    )
    .unwrap();
}

#[test]
fn refresh_create_replace_delete_and_unchanged_preserve_scope_and_budget() {
    let (_dir, root) = fixture();
    let global = root.join("global/AGENTS.md");
    let local = root.join("AGENTS.md");
    let source = AgentsRefresh::new(Some(&global), &root).unwrap();
    assert_eq!(source.read().unwrap(), "");
    fs::create_dir(root.join("global")).unwrap();
    rules(&global, "Global rule.");
    rules(&local, "Local rule.");
    let text = source.read().unwrap();
    assert_eq!(text, "Global rule.\nLocal rule.");
    rules(&root.join("replacement"), "Changed rule.");
    fs::rename(root.join("replacement"), &local).unwrap();
    assert_eq!(source.read().unwrap(), "Global rule.\nChanged rule.");
    fs::write(
        &local,
        "## Always on\nChanged rule.\n## Other\nchanged outside section",
    )
    .unwrap();
    assert_eq!(source.read().unwrap(), "Global rule.\nChanged rule.");
    fs::create_dir(root.join("nested")).unwrap();
    rules(&root.join("nested/AGENTS.md"), "Must not inject.");
    fs::remove_file(&local).unwrap();
    assert_eq!(source.read().unwrap(), "Global rule.");
    rules(&global, &"global rule\n".repeat(500));
    rules(&local, &"project rule\n".repeat(500));
    let expected = super::super::load_from(Some(&global), &root);
    assert_eq!(source.read().unwrap(), expected);
    assert!(crate::budget::count_tokens(&expected) <= super::super::CONSTITUTION_LIMIT);
}

#[test]
fn refresh_rejects_symlink_ancestors_leaf_dangling_hardlink_and_special_files() {
    let (_dir, root) = fixture();
    let real = root.join("real");
    fs::create_dir(&real).unwrap();
    rules(&real.join("AGENTS.md"), "Secret sentinel.");
    symlink(&real, root.join("alias")).unwrap();
    assert!(AgentsRefresh::new(None, &root.join("alias")).is_err());
    let source = AgentsRefresh::new(None, &root).unwrap();
    for target in [real.join("AGENTS.md"), root.join("absent")] {
        symlink(target, root.join("AGENTS.md")).unwrap();
        assert!(source.read().is_err());
        fs::remove_file(root.join("AGENTS.md")).unwrap();
    }
    fs::hard_link(real.join("AGENTS.md"), root.join("AGENTS.md")).unwrap();
    assert!(source.read().is_err());
    fs::remove_file(root.join("AGENTS.md")).unwrap();
    let fifo = CString::new(root.join("AGENTS.md").as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    assert!(source.read().is_err());
}

#[test]
fn refresh_rejects_invalid_utf8_excess_unreadable_and_detected_races() {
    let (_dir, root) = fixture();
    let path = root.join("AGENTS.md");
    let source = AgentsRefresh::new(None, &root).unwrap();
    for body in [vec![0xff], vec![b'x'; MAX_BYTES as usize + 1]] {
        fs::write(&path, body).unwrap();
        assert!(source.read().is_err());
    }
    rules(&path, "Valid.");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    assert!(source.read().is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(
        source
            .read_checked(|| rules(&path, "Changed during read."))
            .is_err()
    );
    assert_eq!(source.read().unwrap(), "Changed during read.");
    fs::remove_file(&path).unwrap();
    assert!(
        source
            .read_checked(|| rules(&path, "Created during read."))
            .is_err()
    );
}

#[test]
fn refresh_rejects_registered_secret_old_inode_before_utf8_decoding() {
    let (_dir, root) = fixture();
    let secret = root.join("registered.json");
    polaris_auth::api_key::save_to(&secret, "synthetic-only").unwrap();
    let original = fs::metadata(&secret).unwrap();
    let path = root.join("AGENTS.md");
    fs::rename(&secret, &path).unwrap();
    // A moved, single-link old inode must remain protected even after rotation.
    polaris_auth::api_key::save_to(&secret, "rotated-synthetic-only").unwrap();
    assert_eq!(identity(&original), identity(&fs::metadata(&path).unwrap()));
    assert_eq!(fs::metadata(&path).unwrap().nlink(), 1);
    assert!(AgentsRefresh::new(None, &root).unwrap().read().is_err());
    // Keep registered identities alive for concurrent tests; registry never evicts.
    let _ = _dir.keep();
}

#[test]
fn refresh_rejects_rebound_root_and_secret_named_ancestor() {
    let (_dir, root) = fixture();
    let project = root.join("project");
    fs::create_dir(&project).unwrap();
    rules(&project.join("AGENTS.md"), "Original.");
    let source = AgentsRefresh::new(None, &project).unwrap();
    fs::rename(&project, root.join("old")).unwrap();
    fs::create_dir(&project).unwrap();
    rules(&project.join("AGENTS.md"), "Replacement.");
    assert!(source.read().is_err());
    fs::create_dir(root.join(".ssh")).unwrap();
    rules(&root.join(".ssh/AGENTS.md"), "Denied.");
    assert!(
        AgentsRefresh::new(None, &root.join(".ssh"))
            .unwrap()
            .read()
            .is_err()
    );
}

#[test]
fn refresh_concurrent_readers_have_owned_snapshots_and_do_not_read_copy() {
    let (_dir, root) = fixture();
    rules(&root.join("AGENTS.md"), "Host.");
    let source = AgentsRefresh::new(None, &root).unwrap();
    let old = source.read().unwrap();
    fs::create_dir(root.join("copy")).unwrap();
    rules(&root.join("copy/AGENTS.md"), "Copy must not apply.");
    std::thread::scope(|scope| {
        for _ in 0..4 {
            let source = &source;
            scope.spawn(move || assert_eq!(source.read().unwrap(), "Host."));
        }
    });
    rules(&root.join("AGENTS.md"), "Host changed.");
    assert_eq!(old, "Host.");
    assert_eq!(source.read().unwrap(), "Host changed.");
}
