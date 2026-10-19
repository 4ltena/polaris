//! Canonical migration tree projection and namespace-change regression tests.
use super::*;
use crate::entry_gate::physical_directory;
use std::{
    ffi::CString,
    fs::{self, OpenOptions},
    io::Write,
    os::{
        fd::AsRawFd,
        unix::{
            ffi::OsStrExt,
            fs::{OpenOptionsExt, PermissionsExt, symlink},
        },
    },
    path::{Path, PathBuf},
};

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap().join("owned-tree");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        Self { _temp: temp, root }
    }
    fn directory(&self, path: &str) {
        let path = self.root.join(path);
        fs::create_dir_all(&path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    fn file(&self, path: &Path, bytes: &[u8]) {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(self.root.join(path))
            .unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
    }
    fn replace(&self, path: &str, bytes: &[u8]) {
        let mut file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(self.root.join(path))
            .unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
    }
    // Test-only inventory. Production root migration must supply its own
    // complete, descriptor-pinned enumeration and final ancestor rechecks.
    fn projection(&self, tree: ProjectionTreeV2) -> Result<String, Error> {
        fn walk(root: &Path, at: &Path, paths: &mut Vec<PathBuf>) {
            for entry in fs::read_dir(at).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                paths.push(path.strip_prefix(root).unwrap().to_owned());
                if entry.file_type().unwrap().is_dir() {
                    walk(root, &path, paths);
                }
            }
        }
        let mut paths = Vec::new();
        walk(&self.root, &self.root, &mut paths);
        paths.sort_by(|a, b| a.as_os_str().as_bytes().cmp(b.as_os_str().as_bytes()));
        let mut projection = NonSecretTreeProjectionV2::new(tree);
        for path in paths {
            let relative_parent = path.parent().unwrap();
            let parent_path = if relative_parent.as_os_str().is_empty() {
                self.root.clone()
            } else {
                self.root.join(relative_parent)
            };
            let parent = physical_directory(&parent_path).unwrap();
            projection.read_child(
                path.as_os_str().as_bytes(),
                &parent,
                path.file_name().unwrap().as_bytes(),
            )?;
        }
        projection.finish()
    }
}

#[test]
fn secret_value_and_length_do_not_change_projection() {
    let fixture = Fixture::new();
    fixture.file(Path::new("secrets.json"), br#"{"value":"short"}"#);
    fixture.file(Path::new("servers.json"), b"public registry");
    let original = fixture.projection(ProjectionTreeV2::SourceV1).unwrap();
    fixture.replace(
        "secrets.json",
        br#"{"value":"a much longer unrelated credential"}"#,
    );
    assert_eq!(
        original,
        fixture.projection(ProjectionTreeV2::SourceV1).unwrap()
    );
    fs::remove_file(fixture.root.join("secrets.json")).unwrap();
    assert_ne!(
        original,
        fixture.projection(ProjectionTreeV2::SourceV1).unwrap()
    );
}

#[test]
fn only_activation_epoch_is_excluded_from_projection() {
    let fixture = Fixture::new();
    fixture.directory("ledger");
    fixture.file(Path::new("ledger/head.json"), b"public head");
    fixture.file(Path::new("ledger/history.jsonl"), b"");
    let without_epoch = fixture.projection(ProjectionTreeV2::ActivationV2).unwrap();
    fixture.file(Path::new("ledger/epoch.json"), b"first final epoch");
    assert_eq!(
        without_epoch,
        fixture.projection(ProjectionTreeV2::ActivationV2).unwrap()
    );
    fixture.replace(
        "ledger/epoch.json",
        b"another epoch, separately bound by external intent",
    );
    assert_eq!(
        without_epoch,
        fixture.projection(ProjectionTreeV2::ActivationV2).unwrap()
    );
    assert!(fixture.projection(ProjectionTreeV2::SourceV1).is_err());
}

#[test]
fn public_content_size_and_mode_are_bound() {
    let fixture = Fixture::new();
    fixture.directory("servers/example");
    fixture.file(Path::new("servers/example/tool"), b"first");
    let first = fixture.projection(ProjectionTreeV2::SourceV1).unwrap();
    fixture.replace("servers/example/tool", b"other");
    let second = fixture.projection(ProjectionTreeV2::SourceV1).unwrap();
    assert_ne!(first, second);
    fixture.replace("servers/example/tool", b"other with another size");
    let third = fixture.projection(ProjectionTreeV2::SourceV1).unwrap();
    assert_ne!(second, third);
    fs::set_permissions(
        fixture.root.join("servers/example/tool"),
        fs::Permissions::from_mode(0o400),
    )
    .unwrap();
    assert_ne!(
        third,
        fixture.projection(ProjectionTreeV2::SourceV1).unwrap()
    );
}

#[test]
fn unknown_root_and_protocol_temps_are_blockers() {
    for path in [
        "unrecognized.json",
        "secrets.json.tmp",
        "servers.json.tmp",
        "ledger/head.pending",
        "ledger/extra.json",
        "owner.lock",
    ] {
        let fixture = Fixture::new();
        if path.starts_with("ledger/") {
            fixture.directory("ledger");
        }
        fixture.file(Path::new(path), b"");
        assert!(
            fixture.projection(ProjectionTreeV2::ActivationV2).is_err(),
            "{path}"
        );
    }
    let source = Fixture::new();
    source.file(Path::new("owner.lock"), b"");
    assert!(source.projection(ProjectionTreeV2::SourceV1).is_ok());
    source.replace("owner.lock", b"unexpected content");
    assert!(source.projection(ProjectionTreeV2::SourceV1).is_err());
}

#[test]
fn symlink_hardlink_special_and_protection_anomalies_fail_closed() {
    let linked = Fixture::new();
    linked.file(Path::new("servers.json"), b"public");
    symlink("servers.json", linked.root.join("secrets.json")).unwrap();
    assert!(linked.projection(ProjectionTreeV2::SourceV1).is_err());
    fs::remove_file(linked.root.join("secrets.json")).unwrap();
    fs::hard_link(
        linked.root.join("servers.json"),
        linked.root.join("secrets.json"),
    )
    .unwrap();
    assert!(linked.projection(ProjectionTreeV2::SourceV1).is_err());
    let fifo = Fixture::new();
    let root = physical_directory(&fifo.root).unwrap();
    let name = CString::new("secrets.json").unwrap();
    assert_eq!(
        unsafe { libc::mkfifoat(root.as_raw_fd(), name.as_ptr(), 0o600) },
        0
    );
    assert!(fifo.projection(ProjectionTreeV2::SourceV1).is_err());
    let mode = Fixture::new();
    mode.file(Path::new("secrets.json"), b"fixture only");
    fs::set_permissions(
        mode.root.join("secrets.json"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert!(mode.projection(ProjectionTreeV2::SourceV1).is_err());
    let directory = Fixture::new();
    directory.directory("servers");
    fs::set_permissions(
        directory.root.join("servers"),
        fs::Permissions::from_mode(0o777),
    )
    .unwrap();
    assert!(directory.projection(ProjectionTreeV2::SourceV1).is_err());
}

#[test]
fn duplicate_out_of_order_escaped_and_mismatched_paths_poison_partial_hash() {
    let fixture = Fixture::new();
    fixture.file(Path::new("catalog.json"), b"public");
    fixture.file(Path::new("servers.json"), b"public");
    let root = physical_directory(&fixture.root).unwrap();
    for bad in [
        b"catalog.json".as_slice(),
        b"../catalog.json",
        b"/catalog.json",
        b"catalog.json/",
        b"unknown/catalog.json",
        b"secrets.json",
    ] {
        let mut projection = NonSecretTreeProjectionV2::new(ProjectionTreeV2::SourceV1);
        projection
            .read_child(b"servers.json", &root, b"servers.json")
            .unwrap();
        assert!(projection.read_child(bad, &root, b"catalog.json").is_err());
        assert!(projection.finish().is_err());
    }
    let mut duplicate = NonSecretTreeProjectionV2::new(ProjectionTreeV2::SourceV1);
    duplicate
        .read_child(b"catalog.json", &root, b"catalog.json")
        .unwrap();
    assert!(
        duplicate
            .read_child(b"catalog.json", &root, b"catalog.json")
            .is_err()
    );
    assert!(duplicate.finish().is_err());
}

#[test]
fn package_directories_and_non_utf8_names_use_exact_byte_order() {
    let fixture = Fixture::new();
    fixture.directory("servers/example/foo");
    fixture.file(Path::new("servers/example/foo/inside"), b"nested");
    fixture.file(
        Path::new("servers/example/foo-bar"),
        b"sibling sorts before nested child",
    );
    // APFS rejects a raw non-UTF-8 filename. The portable classifier preserves
    // those bytes; physical raw-name coverage belongs to the Linux fixture.
    assert!(matches!(
        classify(ProjectionTreeV2::SourceV1, b"servers/example/\xff-name"),
        Ok(Role::PackageNode)
    ));
    #[cfg(target_os = "linux")]
    {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};
        let raw = OsString::from_vec(b"servers/example/\xff-name".to_vec());
        fixture.file(Path::new(&raw), b"raw byte name");
    }
    fixture.file(Path::new("servers/example/z-name"), b"ordered sibling");
    assert!(fixture.projection(ProjectionTreeV2::SourceV1).is_ok());
    let parent = physical_directory(&fixture.root.join("servers/example")).unwrap();
    let mut hash = NonSecretTreeProjectionV2::new(ProjectionTreeV2::SourceV1);
    hash.read_child(b"servers/example/z-name", &parent, b"z-name")
        .unwrap();
    assert!(
        hash.read_child(b"servers/example/foo-bar", &parent, b"foo-bar")
            .is_err()
    );
}

#[test]
fn unregistered_materialized_trees_are_not_limited_by_registry_count() {
    let fixture = Fixture::new();
    for index in 0..40 {
        fixture.directory(&format!("servers/materialized{index:02}"));
        fixture.file(
            Path::new(&format!("servers/materialized{index:02}/payload")),
            b"package",
        );
    }
    assert!(fixture.projection(ProjectionTreeV2::SourceV1).is_ok());
}

#[test]
fn streamed_package_file_exceeds_json_canonicalization_limit() {
    let fixture = Fixture::new();
    fixture.directory("servers/example");
    fixture.file(
        Path::new("servers/example/payload"),
        &vec![b'a'; 1024 * 1024],
    );
    let before = fixture.projection(ProjectionTreeV2::SourceV1).unwrap();
    fixture.replace("servers/example/payload", &vec![b'b'; 1024 * 1024]);
    assert_ne!(
        before,
        fixture.projection(ProjectionTreeV2::SourceV1).unwrap()
    );
}

#[test]
fn named_replacement_after_streaming_is_rejected_without_partial_hash() {
    use std::{cell::Cell, rc::Rc};
    let fixture = Fixture::new();
    fixture.file(Path::new("catalog.json"), b"same public bytes");
    let root = physical_directory(&fixture.root).unwrap();
    let path = fixture.root.clone();
    let reached = Rc::new(Cell::new(false));
    let observed = reached.clone();
    AFTER_CHILD_READ.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            observed.set(true);
            fs::rename(path.join("catalog.json"), path.join("owned-old-catalog")).unwrap();
            let mut replacement = OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(path.join("catalog.json"))
                .unwrap();
            replacement.write_all(b"same public bytes").unwrap();
            replacement.sync_all().unwrap();
        }));
    });
    let mut projection = NonSecretTreeProjectionV2::new(ProjectionTreeV2::SourceV1);
    assert!(
        projection
            .read_child(b"catalog.json", &root, b"catalog.json")
            .is_err()
    );
    assert!(reached.get());
    assert!(projection.finish().is_err());
    assert_eq!(
        fs::read(fixture.root.join("catalog.json")).unwrap(),
        b"same public bytes"
    );
    assert_eq!(
        fs::read(fixture.root.join("owned-old-catalog")).unwrap(),
        b"same public bytes"
    );
}
