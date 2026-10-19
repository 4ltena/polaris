//! Historical migration read admission and live entry-authority closure tests.
use super::super::test_fixture::GuardFixture;
use super::*;
use polaris_mcp::migration_observation::ProjectionTreeV2;
use std::{io::Read, os::unix::fs::PermissionsExt, path::PathBuf};

fn directory(path: &std::path::Path) {
    std::fs::create_dir(path).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}
fn stage(fixture: &GuardFixture) -> (StagingRecordV2, CompleteTreeProjectionV2, PathBuf) {
    let mut record = fixture.stage();
    let (home, _, _, _) = fixture.guard.verified_binding().unwrap();
    let path = home
        .join(".polaris")
        .join(record.retired.binding.layout.staging.component());
    directory(&path);
    directory(&path.join("servers"));
    directory(&path.join("servers/example"));
    std::fs::write(path.join("servers/example/index.js"), b"retained stage").unwrap();
    std::fs::set_permissions(
        path.join("servers/example/index.js"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let file = File::open(&path).unwrap();
    record.staging_anchor = EntryAnchorV2::of(&file.metadata().unwrap());
    record.canonical_parent_anchor = EntryAnchorV2::of(&fixture.guard.parent.metadata().unwrap());
    let inventory =
        CompleteTreeProjectionV2::inspect(&file, ProjectionTreeV2::ActivationV2).unwrap();
    (record, inventory, path)
}

#[test]
fn core_read_admission_keeps_historical_view_separate_from_live_entry_authority() {
    let fixture = GuardFixture::new(&"a".repeat(64));
    let (record, inventory, path) = stage(&fixture);
    let view = inspect_mapped_stage(&fixture.guard, &record, &inventory).unwrap();
    let (home, _, _, _) = fixture.guard.verified_binding().unwrap();
    let gate = home
        .join(".polaris")
        .join(super::super::entry_gate::GATE_FILE);
    std::fs::write(&gate, b"invalid gate").unwrap();
    assert!(fixture.guard.verified_binding().is_err());
    view.verify_current().unwrap();
    let mut bytes = Vec::new();
    view.open_readonly(&view.logical_root().join("servers/example/index.js"))
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, b"retained stage");
    assert!(inspect_mapped_stage(&fixture.guard, &record, &inventory).is_err());
    assert!(!view.logical_root().exists());
    assert_eq!(
        std::fs::read(path.join("servers/example/index.js")).unwrap(),
        b"retained stage"
    );
}

#[test]
fn core_read_admission_closes_late_gate_change_after_mapped_observation() {
    let fixture = GuardFixture::new(&"a".repeat(64));
    let (record, inventory, path) = stage(&fixture);
    let (home, _, _, _) = fixture.guard.verified_binding().unwrap();
    let gate = home
        .join(".polaris")
        .join(super::super::entry_gate::GATE_FILE);
    let reached = std::rc::Rc::new(std::cell::Cell::new(false));
    let observed = reached.clone();
    AFTER_OBSERVATION.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            std::fs::write(gate, b"invalid gate").unwrap();
            observed.set(true);
        }))
    });
    assert!(inspect_mapped_stage(&fixture.guard, &record, &inventory).is_err());
    assert!(reached.get());
    assert_eq!(
        std::fs::read(path.join("servers/example/index.js")).unwrap(),
        b"retained stage"
    );
}

#[test]
fn core_read_admission_rejects_other_manifest_and_id_before_pin_observation() {
    let fixture = GuardFixture::new(&"a".repeat(64));
    let (record, registry) = fixture.empty_store_record();
    validate_unregistered_pins(&fixture.guard, &record, None, &registry, &BTreeMap::new()).unwrap();
    for id in ["a".repeat(64), "b".repeat(64)] {
        let other = GuardFixture::new(&id);
        let (mut different, other_registry) = other.empty_store_record();
        // Align the parent to isolate live id/manifest binding. The DTO remains
        // valid independently; its serialized observations grant no authority.
        different.binding.canonical_parent_anchor = record.binding.canonical_parent_anchor;
        if id != record.binding.migration_id {
            different.binding.entry_manifest_hash = record.binding.entry_manifest_hash.clone();
        }
        different.validate().unwrap();
        assert!(
            validate_unregistered_pins(
                &fixture.guard,
                &different,
                None,
                &other_registry,
                &BTreeMap::new()
            )
            .is_err()
        );
    }
}

#[test]
fn core_read_admission_rejects_other_parent_with_matching_id_and_manifest() {
    let fixture = GuardFixture::new(&"a".repeat(64));
    let (record, registry) = fixture.empty_store_record();
    let other = GuardFixture::new(&"a".repeat(64));
    let (mut different, _) = other.empty_store_record();
    different.binding.entry_manifest_hash = record.binding.entry_manifest_hash.clone();
    different.validate().unwrap();
    assert!(
        validate_unregistered_pins(
            &fixture.guard,
            &different,
            None,
            &registry,
            &BTreeMap::new()
        )
        .is_err()
    );
}

#[test]
fn core_read_admission_closes_late_gate_change_after_pin_observation() {
    let fixture = GuardFixture::new(&"a".repeat(64));
    let (record, registry) = fixture.empty_store_record();
    let (home, _, _, _) = fixture.guard.verified_binding().unwrap();
    let gate = home
        .join(".polaris")
        .join(super::super::entry_gate::GATE_FILE);
    let reached = std::rc::Rc::new(std::cell::Cell::new(false));
    let observed = reached.clone();
    AFTER_OBSERVATION.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            std::fs::write(gate, b"invalid gate").unwrap();
            observed.set(true);
        }))
    });
    assert!(
        validate_unregistered_pins(&fixture.guard, &record, None, &registry, &BTreeMap::new())
            .is_err()
    );
    assert!(reached.get());
}
