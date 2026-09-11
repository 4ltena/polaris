//! Sanitized snapshots, excluded secrets, source conflicts, and copy limits.
use super::*;
use std::os::unix::fs::symlink;

fn limits() -> Limits {
    Limits {
        max_entries: 64,
        max_files: 32,
        max_file_bytes: 1024,
        max_total_bytes: 4096,
        max_depth: 8,
    }
}
fn fixture() -> (TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let path = fs::canonicalize(temp.path()).unwrap();
    (temp, path)
}
fn empty() -> RegisteredSecrets {
    RegisteredSecrets::new([]).unwrap()
}

#[test]
fn copies_source_empty_directory_and_executable_without_setid() {
    let (_temp, source) = fixture();
    fs::create_dir(source.join("src")).unwrap();
    fs::create_dir(source.join("empty")).unwrap();
    fs::write(source.join("src/main.rs"), b"fn main() {}\n").unwrap();
    fs::write(source.join("run"), b"#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(source.join("run"), Permissions::from_mode(0o6755)).unwrap();
    let output = snapshot(&source, &empty(), limits()).unwrap();
    assert!(!output.path().starts_with(&source));
    assert_eq!(
        fs::read(output.path().join("src/main.rs")).unwrap(),
        b"fn main() {}\n"
    );
    assert!(output.path().join("empty").is_dir());
    assert_eq!(
        fs::metadata(output.path().join("run")).unwrap().mode() & 0o7777,
        0o755
    );
    assert_eq!(output.manifest.len(), 4);
    let entry = output
        .manifest
        .iter()
        .find(|e| e.relative_path == Path::new("run"))
        .unwrap();
    assert_eq!(
        entry.sha256,
        Some(Sha256::digest(b"#!/bin/sh\nexit 0\n").into())
    );
    assert_eq!(
        entry.identity,
        Identity::of(&fs::metadata(source.join("run")).unwrap())
    );
    assert!(output.exclusions.is_empty());
    let path = output.path().to_owned();
    drop(output);
    assert!(!path.exists());
}

#[test]
fn excludes_secret_names_registered_absent_paths_and_renamed_inodes() {
    let (_temp, source) = fixture();
    fs::write(source.join("ordinary.rs"), b"ordinary").unwrap();
    fs::write(source.join(".env"), b"dummy secret").unwrap();
    fs::write(source.join("custom-store"), b"dummy token").unwrap();
    let secrets =
        RegisteredSecrets::new([source.join("custom-store"), source.join("future-store")]).unwrap();
    fs::rename(source.join("custom-store"), source.join("innocent.txt")).unwrap();
    fs::write(source.join("future-store"), b"future dummy").unwrap();
    let output = snapshot(&source, &secrets, limits()).unwrap();
    assert_eq!(
        fs::read(output.path().join("ordinary.rs")).unwrap(),
        b"ordinary"
    );
    assert_eq!(output.exclusions.len(), 3);
    for name in [".env", "innocent.txt", "future-store"] {
        assert!(!output.path().join(name).exists());
    }
    assert!(
        output
            .exclusions
            .iter()
            .any(|e| e.relative_path == Path::new("innocent.txt")
                && e.reason == ExclusionReason::RegisteredSecret)
    );
}

#[test]
fn omits_arbitrary_hardlinks_symlinks_and_fifo() {
    let (_temp, source) = fixture();
    let (_outside, external) = fixture();
    fs::write(external.join("token"), b"dummy").unwrap();
    fs::hard_link(external.join("token"), source.join("alias.rs")).unwrap();
    symlink(external.join("token"), source.join("link.rs")).unwrap();
    symlink(&external, source.join("directory-link")).unwrap();
    let fifo = CString::new(source.join("pipe").as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    fs::write(source.join("ok"), b"ok").unwrap();
    let output = snapshot(&source, &empty(), limits()).unwrap();
    assert_eq!(fs::read(output.path().join("ok")).unwrap(), b"ok");
    assert_eq!(output.exclusions.len(), 4);
    for (name, reason) in [
        ("alias.rs", ExclusionReason::MultipleLinks),
        ("link.rs", ExclusionReason::Symlink),
        ("pipe", ExclusionReason::SpecialFile),
    ] {
        assert!(
            output
                .exclusions
                .iter()
                .any(|e| e.relative_path == Path::new(name) && e.reason == reason)
        );
        assert!(!output.path().join(name).exists());
    }
}

#[test]
fn enforces_each_explicit_bound() {
    let (_temp, source) = fixture();
    fs::write(source.join("one"), b"1234").unwrap();
    fs::write(source.join("two"), b"5678").unwrap();
    for bound in [
        Limits {
            max_file_bytes: 3,
            ..limits()
        },
        Limits {
            max_total_bytes: 7,
            ..limits()
        },
        Limits {
            max_files: 1,
            ..limits()
        },
        Limits {
            max_entries: 1,
            ..limits()
        },
    ] {
        assert_eq!(
            snapshot(&source, &empty(), bound).unwrap_err().kind,
            ErrorKind::Limit
        );
    }
    assert_eq!(
        snapshot(
            &source,
            &empty(),
            Limits {
                max_file_bytes: 4,
                max_total_bytes: 8,
                max_files: 2,
                max_entries: 2,
                ..limits()
            }
        )
        .unwrap()
        .manifest
        .len(),
        2
    );
    fs::create_dir(source.join("dir")).unwrap();
    assert_eq!(
        snapshot(
            &source,
            &empty(),
            Limits {
                max_depth: 0,
                ..limits()
            }
        )
        .unwrap_err()
        .kind,
        ErrorKind::Limit
    );
}

#[test]
fn rejects_root_replacement_after_fd_open() {
    let (_temp, parent) = fixture();
    let source = parent.join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("ok"), b"original").unwrap();
    let result = snapshot_inner(&source, &empty(), limits(), || {
        fs::rename(&source, parent.join("old")).unwrap();
        fs::create_dir(&source).unwrap();
        fs::write(source.join("ok"), b"replacement").unwrap();
    });
    assert_eq!(result.unwrap_err().kind, ErrorKind::SourceChanged);
    assert_eq!(fs::read(source.join("ok")).unwrap(), b"replacement");
}

#[test]
fn rejects_symlink_root_and_ancestor_without_following() {
    let (_temp, parent) = fixture();
    fs::create_dir(parent.join("real")).unwrap();
    fs::create_dir(parent.join("real/sub")).unwrap();
    symlink(parent.join("real"), parent.join("alias")).unwrap();
    assert!(snapshot(&parent.join("alias"), &empty(), limits()).is_err());
    assert!(snapshot(&parent.join("alias/sub"), &empty(), limits()).is_err());
}

#[test]
fn requires_absolute_registration_and_source() {
    assert_eq!(
        RegisteredSecrets::new([PathBuf::from("relative")])
            .unwrap_err()
            .kind,
        ErrorKind::InvalidPath
    );
    assert_eq!(
        snapshot(Path::new("relative"), &empty(), limits())
            .unwrap_err()
            .kind,
        ErrorKind::InvalidPath
    );
}

#[test]
fn refreshes_absent_registration_identity_before_copy() {
    let (_temp, source) = fixture();
    let (_outside, external) = fixture();
    let reserved = external.join("later");
    let secrets = RegisteredSecrets::new([reserved.clone()]).unwrap();
    fs::write(source.join("renamed.rs"), b"dummy").unwrap();
    // A registered symlink now identifies a single-link ordinary-named file.
    symlink(source.join("renamed.rs"), &reserved).unwrap();
    fs::write(source.join("ok"), b"ok").unwrap();
    let output = snapshot(&source, &secrets, limits()).unwrap();
    assert_eq!(fs::read(output.path().join("ok")).unwrap(), b"ok");
    assert!(!output.path().join("renamed.rs").exists());
    assert_eq!(
        output.exclusions[0].reason,
        ExclusionReason::RegisteredSecret
    );
}

#[test]
fn resolves_registered_symlink_ancestors_for_absent_paths() {
    let (_temp, parent) = fixture();
    let source = parent.join("source");
    fs::create_dir(&source).unwrap();
    symlink(&source, parent.join("alias")).unwrap();
    let secrets = RegisteredSecrets::new([parent.join("alias/future")]).unwrap();
    fs::create_dir(source.join("future")).unwrap();
    fs::write(source.join("future/plain.txt"), b"dummy").unwrap();
    fs::write(source.join("ok"), b"ok").unwrap();
    let output = snapshot(&source, &secrets, limits()).unwrap();
    assert!(output.path().join("ok").is_file());
    assert!(!output.path().join("future").exists());
}

#[test]
fn collects_create_modify_delete_and_mode_only_without_source_writes() {
    let (_temp, source) = fixture();
    for name in ["modify", "delete", "mode", "unchanged"] {
        fs::write(source.join(name), b"original").unwrap();
        fs::set_permissions(source.join(name), Permissions::from_mode(0o644)).unwrap();
    }
    let baseline = snapshot(&source, &empty(), limits()).unwrap();
    let original_metadata = fs::metadata(source.join("modify")).unwrap();
    fs::write(baseline.path().join("modify"), b"updated").unwrap();
    fs::remove_file(baseline.path().join("delete")).unwrap();
    fs::set_permissions(baseline.path().join("mode"), Permissions::from_mode(0o755)).unwrap();
    fs::create_dir(baseline.path().join("new-dir")).unwrap();
    fs::write(baseline.path().join("new-dir/new"), b"created").unwrap();
    let changes = collect_changes(&baseline, &empty(), limits()).unwrap();
    assert!(changes.is_applicable());
    assert_eq!(changes.changes.len(), 5);
    for (name, kind) in [
        ("modify", ChangeKind::Modified),
        ("delete", ChangeKind::Deleted),
        ("mode", ChangeKind::ModeChanged),
        ("new-dir", ChangeKind::Created),
        ("new-dir/new", ChangeKind::Created),
    ] {
        assert!(
            changes
                .changes
                .iter()
                .any(|change| change.relative_path == Path::new(name) && change.kind == kind)
        );
    }
    let modified = changes
        .changes
        .iter()
        .find(|change| change.relative_path == Path::new("modify"))
        .unwrap();
    assert_eq!(
        modified.after.as_ref().unwrap().sha256,
        Some(Sha256::digest(b"updated").into())
    );
    assert!(
        check_source_conflicts(&baseline, &changes, &empty(), limits())
            .unwrap()
            .is_empty()
    );
    assert_eq!(fs::read(source.join("modify")).unwrap(), b"original");
    assert_eq!(fs::read(source.join("delete")).unwrap(), b"original");
    assert_eq!(
        fs::metadata(source.join("mode")).unwrap().mode() & 0o777,
        0o644
    );
    assert!(!source.join("new-dir").exists());
    assert!(unchanged(
        &original_metadata,
        &fs::metadata(source.join("modify")).unwrap()
    ));
}

#[test]
fn directory_copy_permission_normalization_is_not_a_change() {
    let (_temp, source) = fixture();
    fs::create_dir(source.join("directory")).unwrap();
    fs::set_permissions(source.join("directory"), Permissions::from_mode(0o555)).unwrap();
    let baseline = snapshot(&source, &empty(), limits()).unwrap();
    assert!(
        collect_changes(&baseline, &empty(), limits())
            .unwrap()
            .changes
            .is_empty()
    );
    fs::set_permissions(
        baseline.path().join("directory"),
        Permissions::from_mode(0o750),
    )
    .unwrap();
    let changes = collect_changes(&baseline, &empty(), limits()).unwrap();
    assert_eq!(changes.changes.len(), 1);
    assert_eq!(changes.changes[0].kind, ChangeKind::ModeChanged);
    assert!(
        check_source_conflicts(&baseline, &changes, &empty(), limits())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn excluded_replacement_never_becomes_a_deletion() {
    let (_temp, source) = fixture();
    fs::create_dir(source.join("dir")).unwrap();
    fs::write(source.join("dir/child"), b"ordinary").unwrap();
    fs::write(source.join("link"), b"ordinary").unwrap();
    let baseline = snapshot(&source, &empty(), limits()).unwrap();
    fs::remove_file(baseline.path().join("dir/child")).unwrap();
    fs::remove_dir(baseline.path().join("dir")).unwrap();
    symlink(&source, baseline.path().join("dir")).unwrap();
    fs::remove_file(baseline.path().join("link")).unwrap();
    fs::hard_link(source.join("link"), baseline.path().join("link")).unwrap();
    let changes = collect_changes(&baseline, &empty(), limits()).unwrap();
    assert!(!changes.is_applicable());
    assert!(changes.changes.is_empty());
    assert_eq!(changes.exclusions.len(), 2);
    assert!(
        check_source_conflicts(&baseline, &changes, &empty(), limits())
            .unwrap()
            .iter()
            .all(|conflict| conflict.reason == ConflictReason::Excluded)
    );
}

#[test]
fn deleting_parent_of_omitted_secret_is_blocked() {
    let (_temp, source) = fixture();
    fs::create_dir(source.join("dir")).unwrap();
    fs::write(source.join("dir/.env"), b"dummy").unwrap();
    let baseline = snapshot(&source, &empty(), limits()).unwrap();
    assert!(
        collect_changes(&baseline, &empty(), limits())
            .unwrap()
            .is_applicable()
    );
    fs::remove_dir(baseline.path().join("dir")).unwrap();
    let changes = collect_changes(&baseline, &empty(), limits()).unwrap();
    assert!(changes.changes.is_empty());
    assert!(!changes.is_applicable());
    assert_eq!(changes.exclusions[0].relative_path, Path::new("dir/.env"));
}

#[test]
fn new_registration_blocks_even_absent_copy_deletion() {
    let (_temp, source) = fixture();
    fs::write(source.join("later-secret"), b"dummy").unwrap();
    let baseline = snapshot(&source, &empty(), limits()).unwrap();
    fs::remove_file(baseline.path().join("later-secret")).unwrap();
    let secrets = RegisteredSecrets::new([source.join("later-secret")]).unwrap();
    let changes = collect_changes(&baseline, &secrets, limits()).unwrap();
    assert!(changes.changes.is_empty());
    assert_eq!(
        changes.exclusions[0].reason,
        ExclusionReason::RegisteredSecret
    );
}

#[test]
fn source_creation_content_and_mode_conflicts_are_reported() {
    let (_temp, source) = fixture();
    for name in ["content", "mode"] {
        fs::write(source.join(name), b"initial").unwrap();
        fs::set_permissions(source.join(name), Permissions::from_mode(0o644)).unwrap();
    }
    let baseline = snapshot(&source, &empty(), limits()).unwrap();
    for name in ["new", "content", "mode"] {
        fs::write(baseline.path().join(name), b"child edit").unwrap();
    }
    let changes = collect_changes(&baseline, &empty(), limits()).unwrap();
    fs::write(source.join("new"), b"editor creation").unwrap();
    fs::write(source.join("content"), b"editor update").unwrap();
    fs::set_permissions(source.join("mode"), Permissions::from_mode(0o755)).unwrap();
    let conflicts = check_source_conflicts(&baseline, &changes, &empty(), limits()).unwrap();
    assert_eq!(conflicts.len(), 3);
    assert!(
        conflicts
            .iter()
            .any(|c| c.path == source.join("new") && c.reason == ConflictReason::CreationExists)
    );
    assert!(
        conflicts
            .iter()
            .any(|c| c.path == source.join("content") && c.reason == ConflictReason::EntryChanged)
    );
    assert!(
        conflicts
            .iter()
            .any(|c| c.path == source.join("mode") && c.reason == ConflictReason::EntryChanged)
    );
    assert_eq!(fs::read(source.join("new")).unwrap(), b"editor creation");
}

#[test]
fn source_directory_replacement_conflicts_even_with_identical_contents() {
    let (_temp, source) = fixture();
    fs::create_dir(source.join("dir")).unwrap();
    fs::write(source.join("dir/file"), b"initial").unwrap();
    let baseline = snapshot(&source, &empty(), limits()).unwrap();
    fs::write(baseline.path().join("dir/file"), b"child edit").unwrap();
    let changes = collect_changes(&baseline, &empty(), limits()).unwrap();
    fs::rename(source.join("dir"), source.join("old")).unwrap();
    fs::create_dir(source.join("dir")).unwrap();
    fs::write(source.join("dir/file"), b"initial").unwrap();
    let conflicts = check_source_conflicts(&baseline, &changes, &empty(), limits()).unwrap();
    assert!(
        conflicts
            .iter()
            .any(|c| c.path == source.join("dir") && c.reason == ConflictReason::AncestorReplaced)
    );
}

#[test]
fn detects_source_root_and_external_ancestor_replacement() {
    let (_temp, parent) = fixture();
    let outer = parent.join("outer");
    let source = outer.join("source");
    fs::create_dir(&outer).unwrap();
    fs::create_dir(&source).unwrap();
    fs::write(source.join("file"), b"initial").unwrap();
    let baseline = snapshot(&source, &empty(), limits()).unwrap();
    fs::write(baseline.path().join("file"), b"child edit").unwrap();
    let changes = collect_changes(&baseline, &empty(), limits()).unwrap();
    fs::rename(&outer, parent.join("old")).unwrap();
    fs::create_dir(&outer).unwrap();
    fs::rename(parent.join("old/source"), &source).unwrap();
    let conflicts = check_source_conflicts(&baseline, &changes, &empty(), limits()).unwrap();
    assert!(
        conflicts
            .iter()
            .any(|c| c.path == outer && c.reason == ConflictReason::AncestorReplaced)
    );
    fs::rename(&source, parent.join("old-source")).unwrap();
    fs::create_dir(&source).unwrap();
    let conflicts = check_source_conflicts(&baseline, &changes, &empty(), limits()).unwrap();
    assert!(
        conflicts
            .iter()
            .any(|c| c.path == source && c.reason == ConflictReason::RootReplaced)
    );
}

#[test]
fn rejects_replaced_copy_root() {
    let (_temp, source) = fixture();
    let (_saved, saved) = fixture();
    fs::write(source.join("file"), b"initial").unwrap();
    let baseline = snapshot(&source, &empty(), limits()).unwrap();
    fs::rename(baseline.path(), saved.join("old-copy")).unwrap();
    fs::create_dir(baseline.path()).unwrap();
    assert_eq!(
        collect_changes(&baseline, &empty(), limits())
            .unwrap_err()
            .kind,
        ErrorKind::SourceChanged
    );
}

#[test]
fn deletion_checks_unexpected_children_but_skips_unrelated_file_contents() {
    let (_temp, source) = fixture();
    fs::create_dir(source.join("dir")).unwrap();
    fs::write(source.join("unrelated"), b"small").unwrap();
    let baseline = snapshot(&source, &empty(), limits()).unwrap();
    fs::remove_dir(baseline.path().join("dir")).unwrap();
    let changes = collect_changes(&baseline, &empty(), limits()).unwrap();
    fs::write(source.join("unrelated"), [0u8; 8192]).unwrap();
    assert!(
        check_source_conflicts(&baseline, &changes, &empty(), limits())
            .unwrap()
            .is_empty()
    );
    fs::write(source.join("dir/editor-new"), b"editor").unwrap();
    let conflicts = check_source_conflicts(&baseline, &changes, &empty(), limits()).unwrap();
    assert!(
        conflicts
            .iter()
            .any(|c| c.path == source.join("dir/editor-new")
                && c.reason == ConflictReason::UnexpectedEntry)
    );
}

#[test]
fn rescans_enforce_limits_and_special_and_secret_exclusions() {
    let (_temp, source) = fixture();
    fs::write(source.join("file"), b"initial").unwrap();
    let baseline = snapshot(&source, &empty(), limits()).unwrap();
    fs::write(baseline.path().join("file"), [0u8; 2048]).unwrap();
    assert_eq!(
        collect_changes(&baseline, &empty(), limits())
            .unwrap_err()
            .kind,
        ErrorKind::Limit
    );
    fs::write(baseline.path().join("file"), b"child").unwrap();
    let changes = collect_changes(&baseline, &empty(), limits()).unwrap();
    fs::write(source.join("file"), [0u8; 2048]).unwrap();
    assert_eq!(
        check_source_conflicts(&baseline, &changes, &empty(), limits())
            .unwrap_err()
            .kind,
        ErrorKind::Limit
    );
    fs::write(baseline.path().join(".env"), b"dummy").unwrap();
    let pipe = CString::new(baseline.path().join("pipe").as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(pipe.as_ptr(), 0o600) }, 0);
    let blocked = collect_changes(&baseline, &empty(), limits()).unwrap();
    assert!(!blocked.is_applicable());
    assert_eq!(blocked.exclusions.len(), 2);
    assert!(
        blocked
            .exclusions
            .iter()
            .any(|e| e.reason == ExclusionReason::SpecialFile)
    );
    assert!(
        blocked
            .exclusions
            .iter()
            .any(|e| e.reason == ExclusionReason::SecretName)
    );
}

#[test]
fn newly_registered_baseline_inode_is_excluded_before_copy_size_or_hash_read() {
    let (_temp, source) = fixture();
    fs::write(source.join("ordinary"), b"dummy").unwrap();
    let baseline = snapshot(&source, &empty(), limits()).unwrap();
    fs::rename(source.join("ordinary"), source.join("registered")).unwrap();
    let secrets = RegisteredSecrets::new([source.join("registered")]).unwrap();
    fs::write(baseline.path().join("ordinary"), [0u8; 2048]).unwrap();
    // Would hit Limit if the protected copy reached the file-size/hash phase.
    let changes = collect_changes(&baseline, &secrets, limits()).unwrap();
    assert!(changes.changes.is_empty());
    assert_eq!(changes.exclusions.len(), 1);
    assert_eq!(changes.exclusions[0].relative_path, Path::new("ordinary"));
}

#[test]
fn baseline_exclusion_is_not_read_when_child_replaces_it_with_regular_file() {
    let (_temp, source) = fixture();
    symlink(source.join("missing"), source.join("ordinary")).unwrap();
    let baseline = snapshot(&source, &empty(), limits()).unwrap();
    fs::write(baseline.path().join("ordinary"), [0u8; 2048]).unwrap();
    let changes = collect_changes(&baseline, &empty(), limits()).unwrap();
    assert!(changes.changes.is_empty());
    assert!(!changes.is_applicable());
}

#[test]
fn source_special_permission_change_is_a_conflict() {
    let (_temp, source) = fixture();
    fs::create_dir(source.join("directory")).unwrap();
    fs::set_permissions(source.join("directory"), Permissions::from_mode(0o755)).unwrap();
    fs::write(source.join("directory/file"), b"initial").unwrap();
    let baseline = snapshot(&source, &empty(), limits()).unwrap();
    fs::write(baseline.path().join("directory/file"), b"child").unwrap();
    let changes = collect_changes(&baseline, &empty(), limits()).unwrap();
    // macOS sandbox may silently strip setid, so use a retained directory sticky
    // bit and assert the fixture really differs before testing conflict detection.
    fs::set_permissions(source.join("directory"), Permissions::from_mode(0o1755)).unwrap();
    assert_eq!(
        fs::metadata(source.join("directory")).unwrap().mode() & 0o7777,
        0o1755
    );
    let conflicts = check_source_conflicts(&baseline, &changes, &empty(), limits()).unwrap();
    assert!(conflicts.iter().any(|c| c.path == source.join("directory") && c.reason == ConflictReason::EntryChanged));
}

#[test]
fn owns_private_workspace_home_tmp_siblings_until_drop() {
    let (_temp, source) = fixture();
    fs::write(source.join("ordinary"), b"source").unwrap();
    let baseline = snapshot(&source, &empty(), limits()).unwrap();
    let private_root = baseline.path().parent().unwrap().to_owned();
    assert!(!private_root.starts_with(&source));
    for (path, name) in [
        (baseline.path(), "workspace"),
        (baseline.scratch_home(), "home"),
        (baseline.scratch_tmp(), "tmp"),
    ] {
        assert_eq!(path.parent(), Some(private_root.as_path()));
        assert_eq!(path.file_name(), Some(OsStr::new(name)));
        assert!(path.is_dir());
        assert_eq!(fs::metadata(path).unwrap().mode() & 0o777, 0o700);
    }
    assert_eq!(fs::metadata(&private_root).unwrap().mode() & 0o777, 0o700);
    assert_eq!(
        fs::read(baseline.path().join("ordinary")).unwrap(),
        b"source"
    );
    assert!(!baseline.scratch_home().join("ordinary").exists());
    assert!(!baseline.scratch_tmp().join("ordinary").exists());
    fs::write(baseline.scratch_home().join("cache"), b"scratch").unwrap();
    fs::write(baseline.scratch_tmp().join("transient"), b"scratch").unwrap();
    drop(baseline);
    assert!(!private_root.exists());
    assert_eq!(fs::read(source.join("ordinary")).unwrap(), b"source");
}

#[test]
fn scratch_siblings_are_not_scanned_but_same_names_inside_workspace_are() {
    let (_temp, source) = fixture();
    for directory in ["home", "tmp"] {
        fs::create_dir(source.join(directory)).unwrap();
        fs::write(source.join(directory).join("tracked"), b"original").unwrap();
    }
    let baseline = snapshot(&source, &empty(), limits()).unwrap();
    // These would exceed workspace bounds or create exclusions if scanned.
    fs::write(baseline.scratch_home().join("oversized"), [0u8; 8192]).unwrap();
    fs::write(baseline.scratch_home().join(".env"), b"dummy").unwrap();
    let fifo = CString::new(baseline.scratch_tmp().join("pipe").as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let untouched = collect_changes(&baseline, &empty(), limits()).unwrap();
    assert!(untouched.changes.is_empty());
    assert!(untouched.exclusions.is_empty());
    for directory in ["home", "tmp"] {
        fs::write(baseline.path().join(directory).join("tracked"), b"changed").unwrap();
    }
    let changes = collect_changes(&baseline, &empty(), limits()).unwrap();
    assert!(changes.is_applicable());
    assert_eq!(changes.changes.len(), 2);
    for directory in ["home", "tmp"] {
        assert!(changes.changes.iter().any(|change| change.relative_path
            == Path::new(directory).join("tracked")
            && change.kind == ChangeKind::Modified));
        assert_eq!(
            fs::read(source.join(directory).join("tracked")).unwrap(),
            b"original"
        );
    }
    assert!(
        check_source_conflicts(&baseline, &changes, &empty(), limits())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn excludes_git_history_before_open_without_blocking_ordinary_changes() {
    let (_temp, source) = fixture();
    fs::create_dir(source.join(".git")).unwrap();
    fs::create_dir(source.join(".git/objects")).unwrap();
    let history = b"DUMMY_HISTORY_SECRET_SENTINEL".repeat(100);
    fs::write(source.join(".git/objects/dummy"), &history).unwrap();
    fs::write(source.join(".git/config"), &history).unwrap();
    fs::write(source.join(".env"), b"DUMMY_HISTORY_SECRET_SENTINEL").unwrap();
    fs::write(source.join("ordinary"), b"initial").unwrap();
    // History exceeds max_file_bytes: succeeds only by excluding its tree before
    // any history file reaches the size/hash/copy phase. No actual Git is invoked.
    let baseline = snapshot(&source, &empty(), limits()).unwrap();
    assert_eq!(baseline.manifest.len(), 1);
    assert!(!baseline.path().join(".git").exists());
    assert!(!baseline.path().join(".env").exists());
    assert!(
        baseline
            .exclusions
            .iter()
            .any(|e| e.relative_path == Path::new(".git")
                && e.reason == ExclusionReason::RepositoryMetadata)
    );
    let unchanged_copy = collect_changes(&baseline, &empty(), limits()).unwrap();
    assert!(unchanged_copy.changes.is_empty());
    assert!(unchanged_copy.is_applicable());
    fs::write(baseline.path().join("ordinary"), b"changed").unwrap();
    // Child-created repository metadata is also omitted, not applied to source.
    fs::create_dir(baseline.path().join(".git")).unwrap();
    fs::write(baseline.path().join(".git/config"), &history).unwrap();
    let changes = collect_changes(&baseline, &empty(), limits()).unwrap();
    assert!(changes.is_applicable());
    assert_eq!(changes.changes.len(), 1);
    assert_eq!(changes.changes[0].relative_path, Path::new("ordinary"));
    assert!(
        changes
            .exclusions
            .iter()
            .any(|e| e.reason == ExclusionReason::RepositoryMetadata)
    );
    assert!(
        check_source_conflicts(&baseline, &changes, &empty(), limits())
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        fs::read(source.join(".git/objects/dummy")).unwrap(),
        history
    );
    assert_eq!(fs::read(source.join("ordinary")).unwrap(), b"initial");
}

#[test]
fn excludes_git_files_case_insensitively_and_never_emits_git_deletion() {
    for name in [".git", ".GIT", ".GiT"] {
        let (_temp, source) = fixture();
        fs::write(source.join(name), b"DUMMY_GITDIR_SENTINEL".repeat(100)).unwrap();
        fs::write(source.join("ok"), b"ordinary").unwrap();
        let baseline = snapshot(&source, &empty(), limits()).unwrap();
        assert!(!baseline.path().join(name).exists());
        assert_eq!(
            baseline.exclusions[0].reason,
            ExclusionReason::RepositoryMetadata
        );
        let changes = collect_changes(&baseline, &empty(), limits()).unwrap();
        assert!(changes.is_applicable());
        assert!(changes.changes.is_empty());
        // Different case from the registered source exclusion must work too.
        fs::write(baseline.path().join(".gIt"), [0u8; 2048]).unwrap();
        let changes = collect_changes(&baseline, &empty(), limits()).unwrap();
        assert!(changes.is_applicable());
        assert!(changes.changes.is_empty());
        assert_eq!(
            changes.exclusions[0].reason,
            ExclusionReason::RepositoryMetadata
        );
        assert!(
            check_source_conflicts(&baseline, &changes, &empty(), limits())
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn nested_git_metadata_cannot_be_removed_via_parent_deletion() {
    let (_temp, source) = fixture();
    fs::create_dir(source.join("nested")).unwrap();
    fs::create_dir(source.join("nested/.GiT")).unwrap();
    fs::write(source.join("nested/.GiT/history"), b"dummy sentinel").unwrap();
    let baseline = snapshot(&source, &empty(), limits()).unwrap();
    fs::remove_dir(baseline.path().join("nested")).unwrap();
    let changes = collect_changes(&baseline, &empty(), limits()).unwrap();
    assert!(changes.changes.is_empty());
    assert!(!changes.is_applicable());
    assert!(
        changes
            .exclusions
            .iter()
            .any(|e| e.relative_path == Path::new("nested")
                && e.reason == ExclusionReason::RepositoryMetadata)
    );
    let conflicts = check_source_conflicts(&baseline, &changes, &empty(), limits()).unwrap();
    assert!(
        conflicts
            .iter()
            .any(|c| c.path == source.join("nested") && c.reason == ConflictReason::Excluded)
    );
}

#[test]
fn git_exclusion_does_not_relax_symlink_or_secret_rejection() {
    let (_temp, source) = fixture();
    fs::write(source.join("ok"), b"ordinary").unwrap();
    let baseline = snapshot(&source, &empty(), limits()).unwrap();
    symlink(source.join("missing"), baseline.path().join(".git")).unwrap();
    fs::write(baseline.path().join(".env"), b"dummy").unwrap();
    let changes = collect_changes(&baseline, &empty(), limits()).unwrap();
    assert!(!changes.is_applicable());
    assert!(
        changes
            .exclusions
            .iter()
            .any(|e| e.reason == ExclusionReason::Symlink)
    );
    assert!(
        changes
            .exclusions
            .iter()
            .any(|e| e.reason == ExclusionReason::SecretName)
    );
    assert!(
        !check_source_conflicts(&baseline, &changes, &empty(), limits())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn git_directory_as_snapshot_root_is_rejected() {
    let (_temp, source) = fixture();
    fs::create_dir(source.join(".GIT")).unwrap();
    fs::write(source.join(".GIT/history"), b"dummy sentinel").unwrap();
    assert_eq!(
        snapshot(&source.join(".GIT"), &empty(), limits())
            .unwrap_err()
            .kind,
        ErrorKind::InvalidPath
    );
}

fn protected_capture_fixture(case: &str) {
    let home = tempfile::tempdir().unwrap();
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "isolated_workspace::tests::protected_collect_changes_fixture",
            "--nocapture",
        ])
        .env_clear()
        .env("HOME", home.path().canonicalize().unwrap())
        .env("POLARIS_PROTECTED_CAPTURE_FIXTURE", case)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stdout).contains("protected-capture-verified"));
}

#[test]
fn protected_collect_captures_mode_evidence_without_restoring_special_bits() {
    protected_capture_fixture("ordinary");
}

#[test]
fn protected_collect_refuses_post_snapshot_registered_old_inode_alias() {
    protected_capture_fixture("registered-alias");
}

#[test]
#[ignore = "subprocess fixture with isolated HOME and dummy credentials"]
fn protected_collect_changes_fixture() {
    let case = match std::env::var("POLARIS_PROTECTED_CAPTURE_FIXTURE") {
        Ok(case) if matches!(case.as_str(), "ordinary" | "registered-alias") => case,
        _ => return,
    };
    let (_temp, source) = fixture();
    fs::write(source.join("ordinary"), b"before").unwrap();
    fs::set_permissions(source.join("ordinary"), Permissions::from_mode(0o1755)).unwrap();
    assert_eq!(
        fs::metadata(source.join("ordinary")).unwrap().mode() & 0o7777,
        0o1755
    );
    let baseline = protected_snapshot(&source, limits()).unwrap();
    assert!(
        protected_collect_changes(&baseline, limits())
            .unwrap()
            .changes
            .is_empty()
    );
    fs::write(baseline.path().join("ordinary"), b"after").unwrap();
    if case == "ordinary" {
        fs::set_permissions(
            baseline.path().join("ordinary"),
            Permissions::from_mode(0o1755),
        )
        .unwrap();
        let captured = protected_collect_changes(&baseline, limits()).unwrap();
        assert!(captured.is_applicable());
        assert!(captured.exclusions.is_empty());
        assert_eq!(captured.changes.len(), 1);
        let change = &captured.changes[0];
        assert_eq!(change.relative_path, Path::new("ordinary"));
        assert_eq!(change.kind, ChangeKind::Modified);
        let before = change.before.as_ref().unwrap();
        let after = change.after.as_ref().unwrap();
        assert_eq!(before.observed_mode(), 0o1755);
        assert_eq!(after.observed_mode(), 0o1755);
        assert_eq!(before.mode, 0o755);
        assert_eq!(after.mode, 0o755);
        assert_eq!(after.sha256, Some(Sha256::digest(b"after").into()));
    } else {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let registered = home.join("custom-store");
        // Register only after snapshot creation. Moving then rotating leaves a
        // single-link old inode at an innocent copy path, with no secret name.
        polaris_auth::api_key::save_to(&registered, "DUMMY_OLD_CAPTURE_TOKEN").unwrap();
        let original = Identity::of(&fs::metadata(&registered).unwrap());
        let alias = baseline.path().join("ordinary-alias");
        fs::rename(&registered, &alias).unwrap();
        polaris_auth::api_key::save_to(&registered, "DUMMY_NEW_CAPTURE_TOKEN").unwrap();
        assert_eq!(Identity::of(&fs::metadata(&alias).unwrap()), original);
        assert_eq!(fs::metadata(&alias).unwrap().nlink(), 1);
        assert_ne!(Identity::of(&fs::metadata(&registered).unwrap()), original);
        // Empty caller state would miss this identity: exercise the wrapper's
        // fresh registry import, rather than name/link-based refusal.
        let unprotected = collect_changes(&baseline, &empty(), limits()).unwrap();
        assert!(unprotected.is_applicable());
        assert!(
            unprotected
                .changes
                .iter()
                .any(|c| c.relative_path == Path::new("ordinary-alias"))
        );
        let captured = protected_collect_changes(&baseline, limits()).unwrap();
        assert!(!captured.is_applicable());
        assert!(
            captured
                .exclusions
                .iter()
                .any(|e| e.relative_path == Path::new("ordinary-alias")
                    && e.reason == ExclusionReason::RegisteredSecret)
        );
        assert!(
            !captured
                .changes
                .iter()
                .any(|c| c.relative_path == Path::new("ordinary-alias"))
        );
    }
    assert_eq!(fs::read(source.join("ordinary")).unwrap(), b"before");
    assert_eq!(
        fs::metadata(source.join("ordinary")).unwrap().mode() & 0o7777,
        0o1755
    );
    println!("protected-capture-verified");
}
