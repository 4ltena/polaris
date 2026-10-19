//! Public migration observation identity and named-home replacement tests.
#![cfg(any(target_os = "macos", target_os = "linux"))]
use polaris_mcp::migration_observation::{
    CompleteTreeProjectionV2, MigrationParentObservationV2, ProjectionTreeV2, SourceDocumentsV1,
};
use std::{
    fs::File,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
};

fn directory(path: &Path) -> File {
    std::fs::create_dir(path).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    File::open(path).unwrap()
}

#[test]
fn public_observation_closes_home_parent_names_and_preserves_child_creation() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let _home = directory(&home);
    let parent_path = home.join(".polaris");
    let parent = directory(&parent_path);
    let view = MigrationParentObservationV2::inspect(&home, &parent).unwrap();
    directory(&home.join("new-child"));
    view.verify_current().unwrap();
    assert_eq!(view.logical_home(), home);
    std::fs::rename(&parent_path, home.join("old-parent")).unwrap();
    directory(&parent_path);
    std::fs::write(parent_path.join("sentinel"), b"foreign parent").unwrap();
    assert!(view.verify_current().is_err());
    assert_eq!(
        std::fs::read(parent_path.join("sentinel")).unwrap(),
        b"foreign parent"
    );
}

#[test]
fn public_observation_rejects_foreign_descriptor_home_alias_and_mode_drift() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let _home = directory(&home);
    let parent = directory(&home.join(".polaris"));
    let foreign = directory(&root.path().join("foreign"));
    assert!(MigrationParentObservationV2::inspect(&home, &foreign).is_err());
    let alias = root.path().join("alias");
    std::os::unix::fs::symlink(&home, &alias).unwrap();
    assert!(MigrationParentObservationV2::inspect(&alias, &parent).is_err());
    let view = MigrationParentObservationV2::inspect(&home, &parent).unwrap();
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(view.verify_current().is_err());
}

#[test]
fn public_observation_rejects_named_home_replacement_without_rebinding() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let _home = directory(&home);
    let parent = directory(&home.join(".polaris"));
    let view = MigrationParentObservationV2::inspect(&home, &parent).unwrap();
    std::fs::rename(&home, root.path().join("retained-home")).unwrap();
    directory(&home);
    directory(&home.join(".polaris"));
    assert!(view.verify_current().is_err());
}

#[test]
fn public_observation_accepts_existing_nonwritable_parent_and_rejects_writable_parent() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let _home = directory(&home);
    let path = home.join(".polaris");
    let parent = directory(&path);
    for mode in [0o700, 0o750, 0o755] {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        let view = MigrationParentObservationV2::inspect(&home, &parent).unwrap();
        view.verify_current().unwrap();
        for writable in [0o775, 0o777] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(writable)).unwrap();
            assert!(view.verify_current().is_err());
            assert!(MigrationParentObservationV2::inspect(&home, &parent).is_err());
        }
    }
}

#[test]
fn public_document_inventory_observations_are_read_only_and_close_late_changes() {
    let root = tempfile::tempdir().unwrap();
    let source_path = root.path().join("source");
    let source = directory(&source_path);
    let before = source.metadata().unwrap();
    let inventory = CompleteTreeProjectionV2::inspect(&source, ProjectionTreeV2::SourceV1).unwrap();
    let documents = SourceDocumentsV1::inspect(&source).unwrap();
    inventory.verify_current().unwrap();
    documents.verify_current().unwrap();
    assert!(documents.verified_registry().unwrap().is_none());
    assert_eq!(std::fs::read_dir(&source_path).unwrap().count(), 0);
    let after = source.metadata().unwrap();
    assert_eq!(
        (
            before.dev(),
            before.ino(),
            before.mtime(),
            before.mtime_nsec()
        ),
        (after.dev(), after.ino(), after.mtime(), after.mtime_nsec())
    );
    let foreign = directory(&root.path().join("foreign"));
    assert!(inventory.verify_root_mount_binding(&foreign).is_err());
    std::fs::write(source_path.join("servers.json"), b"unexpected source").unwrap();
    assert!(inventory.verify_current().is_err());
    assert!(documents.verify_current().is_err());
    assert_eq!(
        std::fs::read(source_path.join("servers.json")).unwrap(),
        b"unexpected source"
    );
}
