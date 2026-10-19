//! Migration tree inventory, inode replacement and directory-boundary tests.
use super::*;
use crate::entry_gate::physical_directory;
use std::{
    cell::Cell,
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
    rc::Rc,
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
    fn directory(&self, relative: &str) {
        let mut path = self.root.clone();
        for component in relative.split('/') {
            path.push(component);
            if !path.exists() {
                fs::create_dir(&path).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            }
        }
    }
    fn file(&self, relative: &Path, bytes: &[u8]) {
        write_new(&self.root.join(relative), bytes);
    }
    fn replace(&self, relative: &str, bytes: &[u8]) {
        let mut file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(self.root.join(relative))
            .unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
    }
    fn inspect(&self, tree: ProjectionTreeV2) -> Result<CompleteTreeProjectionV2, Error> {
        CompleteTreeProjectionV2::inspect(&physical_directory(&self.root).unwrap(), tree)
    }
}
fn write_new(path: &Path, bytes: &[u8]) {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
}

struct ClearHooks;
impl Drop for ClearHooks {
    fn drop(&mut self) {
        AFTER_ENUMERATION_EOF.with(|slot| *slot.borrow_mut() = None);
        AFTER_PROJECTION.with(|slot| *slot.borrow_mut() = None);
        MOUNT_IDENTITY_OVERRIDE.with(|slot| *slot.borrow_mut() = None);
        DIRECTORY_IDENTITY_OVERRIDE.with(|slot| *slot.borrow_mut() = None);
        PROJECTION_READ_POINT.with(|slot| *slot.borrow_mut() = None);
    }
}

#[test]
fn same_mount_comparator_accepts_distinct_owned_sibling_without_claiming_root_binding() {
    let fixture = Fixture::new();
    let sibling_path = fixture
        ._temp
        .path()
        .canonicalize()
        .unwrap()
        .join("distinct-sibling");
    fs::create_dir(&sibling_path).unwrap();
    fs::set_permissions(&sibling_path, fs::Permissions::from_mode(0o700)).unwrap();
    let sibling = physical_directory(&sibling_path).unwrap();
    let inventory = fixture.inspect(ProjectionTreeV2::SourceV1).unwrap();
    let root_anchor = EntryAnchorV2::of(&inventory.directories[0].metadata);
    let sibling_anchor = EntryAnchorV2::of(&sibling.metadata().unwrap());
    assert_ne!(
        (root_anchor.device, root_anchor.inode),
        (sibling_anchor.device, sibling_anchor.inode)
    );
    inventory.verify_same_mount(&sibling).unwrap();
    assert!(inventory.verify_root_mount_binding(&sibling).is_err());
    inventory
        .verify_same_mount(&inventory.directories[0].file)
        .unwrap();
    inventory
        .verify_before_copy()
        .unwrap()
        .verify_after_copy()
        .unwrap();
}

#[test]
fn same_mount_comparator_rejects_injected_candidate_or_held_root_mount_drift() {
    // Only the returned native mount identity is injected; no privileged mount
    // or unmount is performed and both physical descriptors remain unchanged.
    let fixture = Fixture::new();
    let sibling_path = fixture
        ._temp
        .path()
        .canonicalize()
        .unwrap()
        .join("distinct-sibling");
    fs::create_dir(&sibling_path).unwrap();
    fs::set_permissions(&sibling_path, fs::Permissions::from_mode(0o700)).unwrap();
    let sibling = physical_directory(&sibling_path).unwrap();
    let inventory = fixture.inspect(ProjectionTreeV2::SourceV1).unwrap();
    let sibling_anchor = EntryAnchorV2::of(&sibling.metadata().unwrap());
    let _clear = ClearHooks;
    for selected_fd in [
        sibling.as_raw_fd(),
        inventory.directories[0].file.as_raw_fd(),
    ] {
        let reached = Rc::new(Cell::new(false));
        let observed = reached.clone();
        MOUNT_IDENTITY_OVERRIDE.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move |file, identity| {
                Ok(if file.as_raw_fd() == selected_fd {
                    observed.set(true);
                    injected_different_mount(identity)
                } else {
                    identity
                })
            }));
        });
        assert!(inventory.verify_same_mount(&sibling).is_err());
        assert!(reached.get());
    }
    assert_eq!(
        EntryAnchorV2::of(&sibling.metadata().unwrap()),
        sibling_anchor
    );
}

#[test]
fn frontier_uses_complete_relative_byte_order_instead_of_depth_first_order() {
    let fixture = Fixture::new();
    fixture.directory("servers/example/foo");
    fixture.file(Path::new("servers/example/foo/child"), b"nested bytes");
    fixture.file(Path::new("servers/example/foo-bar"), b"sibling bytes");
    fixture.file(Path::new("servers/example/z-name"), b"last bytes");
    let inventory = fixture.inspect(ProjectionTreeV2::SourceV1).unwrap();
    let paths: Vec<&[u8]> = inventory
        .entries
        .iter()
        .map(|entry| entry.relative_path.as_slice())
        .collect();
    assert_eq!(
        paths,
        [
            b"servers".as_slice(),
            b"servers/example",
            b"servers/example/foo",
            b"servers/example/foo-bar",
            b"servers/example/foo/child",
            b"servers/example/z-name",
        ]
    );
    // Independently supplied order must produce the same projection.
    let mut projection = NonSecretTreeProjectionV2::new(ProjectionTreeV2::SourceV1);
    for entry in &inventory.entries {
        projection
            .read_child(
                &entry.relative_path,
                &inventory.directories[entry.parent].file,
                &entry.name,
            )
            .unwrap();
    }
    assert_eq!(inventory.digest(), projection.finish().unwrap());
    inventory.verify_before_retire().unwrap();
}

#[test]
fn final_root_enumeration_rejects_an_added_empty_directory_after_eof() {
    let fixture = Fixture::new();
    fixture.file(Path::new("catalog.json"), b"public");
    let inventory = fixture.inspect(ProjectionTreeV2::SourceV1).unwrap();
    let path = fixture.root.clone();
    let reached = Rc::new(Cell::new(false));
    let observed = reached.clone();
    let mut root_calls = 0;
    let _clear = ClearHooks;
    AFTER_ENUMERATION_EOF.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move |relative| {
            if !relative.is_empty() {
                return false;
            }
            root_calls += 1;
            if root_calls != 2 {
                return false;
            }
            fs::create_dir(path.join("servers")).unwrap();
            fs::set_permissions(path.join("servers"), fs::Permissions::from_mode(0o700)).unwrap();
            observed.set(true);
            true
        }));
    });
    assert!(inventory.verify_before_retire().is_err());
    assert!(reached.get());
    assert!(fixture.root.join("servers").is_dir());
}

#[test]
fn final_nested_enumeration_rejects_an_added_empty_package_directory() {
    let fixture = Fixture::new();
    fixture.directory("servers/example");
    fixture.file(Path::new("servers/example/payload"), b"public");
    let inventory = fixture.inspect(ProjectionTreeV2::SourceV1).unwrap();
    let path = fixture.root.join("servers/example/late");
    let reached = Rc::new(Cell::new(false));
    let observed = reached.clone();
    let mut calls = 0;
    let _clear = ClearHooks;
    AFTER_ENUMERATION_EOF.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move |relative| {
            if relative != b"servers/example" {
                return false;
            }
            calls += 1;
            if calls != 2 {
                return false;
            }
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            observed.set(true);
            true
        }));
    });
    assert!(inventory.verify_before_copy().is_err());
    assert!(reached.get());
    assert!(fixture.root.join("servers/example/late").is_dir());
}

#[test]
fn named_ancestor_replacement_with_identical_children_fails_closed() {
    let fixture = Fixture::new();
    fixture.directory("servers/example");
    fixture.file(Path::new("servers/example/payload"), b"same public bytes");
    let inventory = fixture.inspect(ProjectionTreeV2::SourceV1).unwrap();
    let path = fixture.root.clone();
    let retained = fixture
        ._temp
        .path()
        .canonicalize()
        .unwrap()
        .join("retained-example");
    let retained_for_hook = retained.clone();
    let reached = Rc::new(Cell::new(false));
    let observed = reached.clone();
    let _clear = ClearHooks;
    AFTER_PROJECTION.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            fs::rename(path.join("servers/example"), &retained_for_hook).unwrap();
            fs::create_dir(path.join("servers/example")).unwrap();
            fs::set_permissions(
                path.join("servers/example"),
                fs::Permissions::from_mode(0o700),
            )
            .unwrap();
            write_new(&path.join("servers/example/payload"), b"same public bytes");
            observed.set(true);
        }));
    });
    assert!(inventory.verify_before_retire().is_err());
    assert!(reached.get());
    assert_ne!(
        fs::metadata(&retained).unwrap().ino(),
        fs::metadata(fixture.root.join("servers/example"))
            .unwrap()
            .ino()
    );
    assert_eq!(
        fs::read(retained.join("payload")).unwrap(),
        b"same public bytes"
    );
    assert_eq!(
        fs::read(fixture.root.join("servers/example/payload")).unwrap(),
        b"same public bytes"
    );
}

#[test]
fn late_file_rewrite_and_same_byte_named_replacement_fail_after_projection() {
    for replace_named in [false, true] {
        let fixture = Fixture::new();
        fixture.file(Path::new("catalog.json"), b"old bytes");
        let inventory = fixture.inspect(ProjectionTreeV2::SourceV1).unwrap();
        let path = fixture.root.clone();
        let retained = fixture
            ._temp
            .path()
            .canonicalize()
            .unwrap()
            .join("retained-catalog");
        let reached = Rc::new(Cell::new(false));
        let observed = reached.clone();
        let _clear = ClearHooks;
        AFTER_PROJECTION.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                if replace_named {
                    fs::rename(path.join("catalog.json"), &retained).unwrap();
                    write_new(&path.join("catalog.json"), b"old bytes");
                } else {
                    let mut file = OpenOptions::new()
                        .write(true)
                        .truncate(true)
                        .open(path.join("catalog.json"))
                        .unwrap();
                    file.write_all(b"new bytes").unwrap();
                    file.sync_all().unwrap();
                }
                observed.set(true);
            }));
        });
        assert!(inventory.verify_before_copy().is_err());
        assert!(reached.get());
        assert_eq!(
            fs::read(fixture.root.join("catalog.json")).unwrap(),
            if replace_named {
                b"old bytes"
            } else {
                b"new bytes"
            }
        );
    }
}

#[test]
fn copy_capability_opens_exact_nodes_and_rechecks_complete_inventory_at_close() {
    let fixture = Fixture::new();
    fixture.directory("servers/example/empty");
    fixture.file(Path::new("servers/example/payload"), b"copy bytes");
    let inventory = fixture.inspect(ProjectionTreeV2::SourceV1).unwrap();
    let copy = inventory.verify_before_copy().unwrap();
    let paths: Vec<_> = copy
        .entries()
        .map(|entry| {
            let file = entry.open_readonly().unwrap();
            assert_eq!(EntryAnchorV2::of(&file.metadata().unwrap()), entry.anchor());
            assert_eq!(entry.is_directory(), file.metadata().unwrap().is_dir());
            entry.relative_path().to_owned()
        })
        .collect();
    assert!(paths.contains(&b"servers/example/empty".to_vec()));
    copy.verify_after_copy().unwrap();

    let copy = inventory.verify_before_copy().unwrap();
    fixture.directory("servers/example/late-empty");
    assert!(copy.verify_after_copy().is_err());
}

#[test]
fn unregistered_forty_server_trees_and_empty_directories_are_retained() {
    let fixture = Fixture::new();
    for index in 0..40 {
        fixture.directory(&format!("servers/materialized{index:02}/empty"));
        fixture.file(
            Path::new(&format!("servers/materialized{index:02}/payload")),
            b"unregistered package bytes",
        );
    }
    let inventory = fixture.inspect(ProjectionTreeV2::SourceV1).unwrap();
    assert_eq!(inventory.entries.len(), 121);
    assert_eq!(inventory.directories.len(), 82);
    inventory.verify_before_retire().unwrap();
}

#[test]
fn secret_value_and_size_are_not_projected_but_presence_and_protection_are() {
    let fixture = Fixture::new();
    fixture.file(Path::new("secrets.json"), br#"{"value":"short"}"#);
    fixture.file(Path::new("servers.json"), b"public registry");
    let before = fixture.inspect(ProjectionTreeV2::SourceV1).unwrap();
    let digest = before.digest().to_owned();
    fixture.replace(
        "secrets.json",
        br#"{"value":"an unrelated and longer synthetic credential"}"#,
    );
    assert!(before.verify_before_retire().is_err());
    assert_eq!(
        digest,
        fixture
            .inspect(ProjectionTreeV2::SourceV1)
            .unwrap()
            .digest()
    );
    fs::set_permissions(
        fixture.root.join("secrets.json"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert!(fixture.inspect(ProjectionTreeV2::SourceV1).is_err());
    fs::remove_file(fixture.root.join("secrets.json")).unwrap();
    assert_ne!(
        digest,
        fixture
            .inspect(ProjectionTreeV2::SourceV1)
            .unwrap()
            .digest()
    );
}

#[test]
fn epoch_is_excluded_from_digest_but_remains_an_inventoried_protected_node() {
    let fixture = Fixture::new();
    fixture.directory("ledger");
    fixture.file(Path::new("ledger/head.json"), b"public head");
    fixture.file(Path::new("ledger/history.jsonl"), b"");
    fixture.file(Path::new("ledger/epoch.json"), b"first epoch");
    let first = fixture.inspect(ProjectionTreeV2::ActivationV2).unwrap();
    let digest = first.digest().to_owned();
    fixture.replace("ledger/epoch.json", b"separately bound final epoch");
    assert!(first.verify_before_retire().is_err());
    let current = fixture.inspect(ProjectionTreeV2::ActivationV2).unwrap();
    assert_eq!(digest, current.digest());
    assert!(
        current
            .entries
            .iter()
            .any(|entry| entry.relative_path == b"ledger/epoch.json")
    );
    fs::set_permissions(
        fixture.root.join("ledger/epoch.json"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert!(fixture.inspect(ProjectionTreeV2::ActivationV2).is_err());
}

#[test]
fn symlink_hardlink_fifo_unknown_and_unsafe_directory_are_rejected_without_repair() {
    let linked = Fixture::new();
    linked.file(Path::new("servers.json"), b"public");
    symlink("servers.json", linked.root.join("secrets.json")).unwrap();
    assert!(linked.inspect(ProjectionTreeV2::SourceV1).is_err());
    assert!(
        fs::symlink_metadata(linked.root.join("secrets.json"))
            .unwrap()
            .is_symlink()
    );
    fs::remove_file(linked.root.join("secrets.json")).unwrap();
    fs::hard_link(
        linked.root.join("servers.json"),
        linked.root.join("secrets.json"),
    )
    .unwrap();
    assert!(linked.inspect(ProjectionTreeV2::SourceV1).is_err());
    assert_eq!(
        fs::metadata(linked.root.join("secrets.json"))
            .unwrap()
            .nlink(),
        2
    );

    let fifo = Fixture::new();
    let root = physical_directory(&fifo.root).unwrap();
    let name = CString::new("secrets.json").unwrap();
    assert_eq!(
        unsafe { libc::mkfifoat(root.as_raw_fd(), name.as_ptr(), 0o600) },
        0
    );
    assert!(fifo.inspect(ProjectionTreeV2::SourceV1).is_err());
    assert!(fifo.root.join("secrets.json").exists());

    let unknown = Fixture::new();
    unknown.file(Path::new("secrets.json.tmp"), b"owned fixture bytes");
    assert!(unknown.inspect(ProjectionTreeV2::SourceV1).is_err());
    assert_eq!(
        fs::read(unknown.root.join("secrets.json.tmp")).unwrap(),
        b"owned fixture bytes"
    );

    let mode = Fixture::new();
    mode.directory("servers/example");
    fs::set_permissions(
        mode.root.join("servers/example"),
        fs::Permissions::from_mode(0o777),
    )
    .unwrap();
    assert!(mode.inspect(ProjectionTreeV2::SourceV1).is_err());
    assert_eq!(
        fs::metadata(mode.root.join("servers/example"))
            .unwrap()
            .mode()
            & 0o7777,
        0o777
    );
}

fn snapshot(root: &Path) -> Vec<(Vec<u8>, EntryAnchorV2, Vec<u8>)> {
    fn walk(root: &Path, directory: &Path, result: &mut Vec<(Vec<u8>, EntryAnchorV2, Vec<u8>)>) {
        for entry in fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).unwrap();
            result.push((
                path.strip_prefix(root)
                    .unwrap()
                    .as_os_str()
                    .as_bytes()
                    .to_owned(),
                EntryAnchorV2::of(&metadata),
                if metadata.is_file() {
                    fs::read(&path).unwrap()
                } else {
                    Vec::new()
                },
            ));
            if metadata.is_dir() {
                walk(root, &path, result);
            }
        }
    }
    let mut result = Vec::new();
    walk(root, root, &mut result);
    result.sort_by(|a, b| a.0.cmp(&b.0));
    result
}

#[test]
fn inventory_and_repeated_verification_create_no_files_and_preserve_all_bytes() {
    let fixture = Fixture::new();
    fixture.directory("servers/example/empty");
    fixture.file(
        Path::new("servers/example/payload"),
        b"public package bytes",
    );
    fixture.file(Path::new("secrets.json"), b"synthetic private bytes");
    fixture.file(Path::new("owner.lock"), b"");
    let before = snapshot(&fixture.root);
    let root_before = EntryAnchorV2::of(&fs::metadata(&fixture.root).unwrap());
    let inventory = fixture.inspect(ProjectionTreeV2::SourceV1).unwrap();
    inventory.verify_before_retire().unwrap();
    inventory
        .verify_before_copy()
        .unwrap()
        .verify_after_copy()
        .unwrap();
    assert_eq!(snapshot(&fixture.root), before);
    assert_eq!(
        EntryAnchorV2::of(&fs::metadata(&fixture.root).unwrap()),
        root_before
    );
}

#[test]
fn raw_component_validation_preserves_non_utf8_bytes_without_aliases() {
    assert!(valid_name(b"\xff-name"));
    assert!(valid_name(b"ordinary"));
    assert!(valid_name(b"-leading"));
    for name in [b"".as_slice(), b".", b"..", b"a/b", b"a\0b"] {
        assert!(!valid_name(name));
    }
}

// APFS rejects physical non-UTF-8 names with EILSEQ. This is actual Linux FD
// enumeration coverage and must not be reported as executed on macOS.
#[cfg(target_os = "linux")]
#[test]
fn physical_non_utf8_package_filename_is_retained_and_ordered_on_linux() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    let fixture = Fixture::new();
    fixture.directory("servers/example");
    let raw = OsString::from_vec(b"servers/example/\xff-name".to_vec());
    fixture.file(Path::new(&raw), b"raw package bytes");
    let inventory = fixture.inspect(ProjectionTreeV2::SourceV1).unwrap();
    assert_eq!(
        inventory.entries.last().unwrap().relative_path,
        b"servers/example/\xff-name"
    );
    inventory.verify_before_retire().unwrap();
}

fn injected_different_mount(mut identity: MountIdentityV2) -> MountIdentityV2 {
    identity.inject_difference();
    identity
}

#[test]
fn native_descriptor_mount_identity_matches_unmounted_children() {
    // This observes the platform FD API on an ordinary fixture. It does not
    // perform or prove rejection of an actual mount/bind-mount operation.
    let fixture = Fixture::new();
    fixture.directory("servers/example");
    fixture.file(Path::new("servers/example/payload"), b"public bytes");
    let root = physical_directory(&fixture.root).unwrap();
    let child = physical_directory(&fixture.root.join("servers/example")).unwrap();
    let payload = open_child(&child, b"payload").unwrap();
    let root_identity = platform_mount_identity(&root).unwrap();
    assert!(platform_mount_identity(&child).unwrap() == root_identity);
    assert!(platform_mount_identity(&payload).unwrap() == root_identity);
    fixture
        .inspect(ProjectionTreeV2::SourceV1)
        .unwrap()
        .verify_before_retire()
        .unwrap();
}

#[test]
fn injected_directory_alias_and_root_cycle_are_rejected_before_descent() {
    // Privileged real alias/cycle mounting is not performed. The deterministic
    // identity injection exercises the exact production seen(dev,ino) check.
    for cycle_to_root in [false, true] {
        let fixture = Fixture::new();
        fixture.directory("servers/alpha");
        fixture.directory("servers/beta");
        let aliased = fs::metadata(fixture.root.join(if cycle_to_root {
            "servers/alpha"
        } else {
            "servers/beta"
        }))
        .unwrap();
        let destination = fs::metadata(if cycle_to_root {
            fixture.root.clone()
        } else {
            fixture.root.join("servers/alpha")
        })
        .unwrap();
        let selected = (aliased.dev(), aliased.ino());
        let identity = (destination.dev(), destination.ino());
        let reached = Rc::new(Cell::new(false));
        let observed = reached.clone();
        let _clear = ClearHooks;
        DIRECTORY_IDENTITY_OVERRIDE.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move |original| {
                if original == selected {
                    observed.set(true);
                    identity
                } else {
                    original
                }
            }));
        });
        assert!(fixture.inspect(ProjectionTreeV2::SourceV1).is_err());
        assert!(reached.get());
        assert!(fixture.root.join("servers/alpha").is_dir());
        assert!(fixture.root.join("servers/beta").is_dir());
    }
}

#[test]
fn injected_external_mount_subtree_is_rejected_without_changed_file_metadata() {
    // Injected mount identity only; no actual mount, unmount or privilege use.
    let fixture = Fixture::new();
    fixture.directory("servers/example");
    fixture.file(Path::new("servers/example/payload"), b"public bytes");
    let original = fs::metadata(fixture.root.join("servers/example")).unwrap();
    let selected = (original.dev(), original.ino());
    let reached = Rc::new(Cell::new(false));
    let observed = reached.clone();
    let _clear = ClearHooks;
    MOUNT_IDENTITY_OVERRIDE.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move |file, identity| {
            let metadata = file.metadata().unwrap();
            Ok(if (metadata.dev(), metadata.ino()) == selected {
                observed.set(true);
                injected_different_mount(identity)
            } else {
                identity
            })
        }));
    });
    assert!(fixture.inspect(ProjectionTreeV2::SourceV1).is_err());
    assert!(reached.get());
    assert!(stable(
        &original,
        &fs::metadata(fixture.root.join("servers/example")).unwrap()
    ));
    assert_eq!(
        fs::read(fixture.root.join("servers/example/payload")).unwrap(),
        b"public bytes"
    );
}

#[test]
fn injected_unavailable_mount_identity_fails_closed_without_fallback() {
    let fixture = Fixture::new();
    fixture.file(Path::new("catalog.json"), b"public bytes");
    let original = snapshot(&fixture.root);
    let reached = Rc::new(Cell::new(false));
    let observed = reached.clone();
    let _clear = ClearHooks;
    MOUNT_IDENTITY_OVERRIDE.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move |_, _| {
            observed.set(true);
            Err(Error::Protection)
        }));
    });
    assert!(fixture.inspect(ProjectionTreeV2::SourceV1).is_err());
    assert!(reached.get());
    assert_eq!(snapshot(&fixture.root), original);
}

#[test]
fn injected_root_held_child_held_and_child_named_mount_drift_fail_revalidation() {
    // Independent held/named descriptors keep the same actual inode and bytes.
    // Only the private returned mount identity changes through a test hook.
    for variant in 0..3 {
        let fixture = Fixture::new();
        fixture.directory("servers/example");
        fixture.file(Path::new("servers/example/payload"), b"public bytes");
        let inventory = fixture.inspect(ProjectionTreeV2::SourceV1).unwrap();
        let directory = if variant == 0 {
            &inventory.directories[0]
        } else {
            inventory
                .directories
                .iter()
                .find(|pin| pin.relative_path == b"servers/example")
                .unwrap()
        };
        let selected = (directory.metadata.dev(), directory.metadata.ino());
        let held_fd = directory.file.as_raw_fd();
        let original = snapshot(&fixture.root);
        let reached = Rc::new(Cell::new(false));
        let observed = reached.clone();
        let _clear = ClearHooks;
        MOUNT_IDENTITY_OVERRIDE.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move |file, identity| {
                let metadata = file.metadata().unwrap();
                let is_selected = (metadata.dev(), metadata.ino()) == selected
                    && if variant == 2 {
                        file.as_raw_fd() != held_fd
                    } else {
                        file.as_raw_fd() == held_fd
                    };
                Ok(if is_selected {
                    observed.set(true);
                    injected_different_mount(identity)
                } else {
                    identity
                })
            }));
        });
        assert!(inventory.verify_before_retire().is_err());
        assert!(reached.get());
        assert_eq!(snapshot(&fixture.root), original);
    }
}

#[test]
fn injected_regular_file_mount_drift_rejects_copy_descriptor_and_closing_verification() {
    let fixture = Fixture::new();
    fixture.directory("servers/example");
    fixture.file(Path::new("servers/example/payload"), b"public bytes");
    let inventory = fixture.inspect(ProjectionTreeV2::SourceV1).unwrap();
    let copy = inventory.verify_before_copy().unwrap();
    let payload = copy
        .entries()
        .find(|entry| entry.relative_path() == b"servers/example/payload")
        .unwrap();
    let original = fs::metadata(fixture.root.join("servers/example/payload")).unwrap();
    let selected = (original.dev(), original.ino());
    let reached = Rc::new(Cell::new(false));
    let observed = reached.clone();
    let _clear = ClearHooks;
    MOUNT_IDENTITY_OVERRIDE.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move |file, identity| {
            let metadata = file.metadata().unwrap();
            Ok(if (metadata.dev(), metadata.ino()) == selected {
                observed.set(true);
                injected_different_mount(identity)
            } else {
                identity
            })
        }));
    });
    assert!(payload.open_readonly().is_err());
    assert!(copy.verify_after_copy().is_err());
    assert!(reached.get());
    assert!(stable(
        &original,
        &fs::metadata(fixture.root.join("servers/example/payload")).unwrap()
    ));
}

#[test]
fn injected_named_root_mount_replacement_fails_the_external_readback_seam() {
    let fixture = Fixture::new();
    fixture.file(Path::new("catalog.json"), b"public bytes");
    let inventory = fixture.inspect(ProjectionTreeV2::SourceV1).unwrap();
    // A distinct caller-supplied FD models the fresh parent/name read-back.
    let named = physical_directory(&fixture.root).unwrap();
    inventory.verify_root_mount_binding(&named).unwrap();
    let selected_fd = named.as_raw_fd();
    let original = EntryAnchorV2::of(&named.metadata().unwrap());
    let reached = Rc::new(Cell::new(false));
    let observed = reached.clone();
    let _clear = ClearHooks;
    MOUNT_IDENTITY_OVERRIDE.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move |file, identity| {
            Ok(if file.as_raw_fd() == selected_fd {
                observed.set(true);
                injected_different_mount(identity)
            } else {
                identity
            })
        }));
    });
    assert!(inventory.verify_root_mount_binding(&named).is_err());
    assert!(reached.get());
    assert_eq!(EntryAnchorV2::of(&named.metadata().unwrap()), original);
}

#[test]
fn injected_external_mount_on_exact_hash_fd_is_rejected_before_read_in_capture_and_reverify() {
    // The hook changes only the newly opened exact projection-read FD. All
    // directory, generic preflight and named read-back descriptors stay normal.
    // This models an external mounted read object without privilege/mount use.
    for reverify in [false, true] {
        let fixture = Fixture::new();
        fixture.file(Path::new("catalog.json"), b"public bytes");
        let existing = if reverify {
            Some(fixture.inspect(ProjectionTreeV2::SourceV1).unwrap())
        } else {
            None
        };
        let original = snapshot(&fixture.root);
        let selected_fd = Rc::new(Cell::new(None::<i32>));
        let reached = Rc::new(Cell::new(false));
        let attempted_read = Rc::new(Cell::new(false));
        let surrounding_checks = Rc::new(Cell::new(0usize));
        let _clear = ClearHooks;
        let selected_for_read = selected_fd.clone();
        let attempted_for_read = attempted_read.clone();
        PROJECTION_READ_POINT.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move |point, file| match point {
                ProjectionReadPointV2::Opened => selected_for_read.set(Some(file.as_raw_fd())),
                ProjectionReadPointV2::BeforeHash => attempted_for_read.set(true),
            }));
        });
        let selected_for_mount = selected_fd.clone();
        let reached_for_mount = reached.clone();
        let surrounding_for_mount = surrounding_checks.clone();
        MOUNT_IDENTITY_OVERRIDE.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move |file, identity| {
                Ok(if selected_for_mount.get() == Some(file.as_raw_fd()) {
                    reached_for_mount.set(true);
                    injected_different_mount(identity)
                } else {
                    surrounding_for_mount.set(surrounding_for_mount.get() + 1);
                    identity
                })
            }));
        });
        let rejected = match &existing {
            Some(inventory) => inventory.verify_before_retire().is_err(),
            None => fixture.inspect(ProjectionTreeV2::SourceV1).is_err(),
        };
        assert!(rejected);
        assert!(selected_fd.get().is_some());
        assert!(reached.get());
        assert!(surrounding_checks.get() > 0);
        assert!(!attempted_read.get());
        assert_eq!(snapshot(&fixture.root), original);
    }
}

#[test]
fn source_inventory_rejects_package_secret_before_public_hash_read() {
    use std::os::unix::ffi::OsStrExt;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("source");
    for dir in [&path, &path.join("servers"), &path.join("servers/example")] {
        std::fs::create_dir(dir).unwrap();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let secret = path.join("servers/example/.ENV");
    std::fs::write(&secret, b"fixture-secret").unwrap();
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
    let identity = std::fs::metadata(&secret).unwrap().ino();
    PROJECTION_READ_POINT.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move |point, file| {
            if point == ProjectionReadPointV2::BeforeHash {
                assert_ne!(
                    file.metadata().unwrap().ino(),
                    identity,
                    "protected payload reached public hash"
                );
            }
        }))
    });
    let parent = File::open(root.path()).unwrap();
    let file = child_bytes(
        &parent,
        path.file_name().unwrap().as_bytes(),
        libc::O_RDONLY | libc::O_DIRECTORY,
    )
    .unwrap();
    let result = CompleteTreeProjectionV2::inspect_source(&file, &path);
    PROJECTION_READ_POINT.with(|slot| *slot.borrow_mut() = None);
    assert!(matches!(result, Err(Error::Protection)));
}
