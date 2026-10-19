//! Durable external migration intent history, binding and replacement tests.
use super::super::super::test_fixture::GuardFixture;
use super::*;
use polaris_mcp::{migration_observation::RetirePreparedRecordV2, store::ledger_v1::LedgerEpochV1};
use std::os::unix::fs::PermissionsExt;

fn chain(fixture: &GuardFixture) -> Vec<ExternalMigrationIntentV2> {
    let stage = fixture.stage();
    let initial = ExternalMigrationIntentV2::RetirePrepared {
        record: RetirePreparedRecordV2 {
            binding: stage.retired.binding.clone(),
        },
    };
    let retired = ExternalMigrationIntentV2::Retired {
        record: stage.retired.clone(),
    };
    let epoch = LedgerEpochV1 {
        protocol_version: 2,
        store_identity: stage.retired.binding.store_identity.clone(),
        random_epoch_id: stage.random_epoch_id.clone(),
        migration_id: stage.retired.binding.migration_id.clone(),
        canonical_parent_anchor: stage.canonical_parent_anchor,
        tombstone_anchor: stage.retired.tombstone.clone(),
        retired_source_anchor: stage.retired.retired_source.clone(),
        active_v2_root_anchor: stage.staging_anchor,
        activation_tree_hash: "6".repeat(64),
    };
    let staged = ExternalMigrationIntentV2::Staging {
        record: stage.clone(),
    };
    let prepared = staged
        .prepared_activation(
            stage.canonical_parent_anchor,
            &epoch,
            &epoch.encode().unwrap(),
        )
        .unwrap();
    let active = prepared
        .active(stage.canonical_parent_anchor, stage.staging_anchor)
        .unwrap();
    vec![initial, retired, staged, prepared, active]
}
fn paths(fixture: &GuardFixture) -> (std::path::PathBuf, String) {
    let (home, id, _, _) = fixture.guard.verified_binding().unwrap();
    (home.join(".polaris"), id.into())
}

#[test]
fn intent_file_preserves_exact_phase_history_and_reopens_without_activating_gate() {
    let fixture = GuardFixture::new(&"a".repeat(64));
    let records = chain(&fixture);
    let mut owner = ExternalIntentFileV2::create(&fixture.guard, records[0].clone()).unwrap();
    owner.revalidate(&fixture.guard).unwrap();
    let (parent, id) = paths(&fixture);
    for (index, next) in records.iter().enumerate().skip(1) {
        owner = owner.advance(&fixture.guard, next.clone()).unwrap();
        assert_eq!(owner.record(), next);
        assert_eq!(
            std::fs::read(parent.join(head_name(&id))).unwrap(),
            next.encode().unwrap()
        );
        assert_eq!(
            std::fs::read(parent.join(archive_name(&id, PHASES[index - 1]))).unwrap(),
            records[index - 1].encode().unwrap()
        );
        for pin in owner.history.iter().chain([&owner.head]) {
            assert_eq!(EntryAnchorV2::of(&pin.file.metadata().unwrap()).mode, 0o600);
        }
        let reopened = ExternalIntentFileV2::open(&fixture.guard).unwrap();
        assert_eq!(reopened.record(), next);
        reopened.revalidate(&fixture.guard).unwrap();
        // The next phase uses descriptors reopened read-only after a restart.
        owner = reopened;
    }
    // An Active observation in this file is never an entry activation grant.
    fixture.guard.verify_migrating().unwrap();
    assert!(!parent.join("mcp-v2").exists());
}

#[test]
fn intent_file_rejects_existing_head_skipped_phase_and_foreign_binding() {
    let fixture = GuardFixture::new(&"a".repeat(64));
    let records = chain(&fixture);
    let owner = ExternalIntentFileV2::create(&fixture.guard, records[0].clone()).unwrap();
    assert!(ExternalIntentFileV2::create(&fixture.guard, records[0].clone()).is_err());
    assert!(owner.advance(&fixture.guard, records[2].clone()).is_err());
    let owner = ExternalIntentFileV2::open(&fixture.guard).unwrap();
    assert_eq!(owner.record(), &records[0]);
    let other = GuardFixture::new(&"b".repeat(64));
    assert!(ExternalIntentFileV2::create(&other.guard, records[0].clone()).is_err());
    assert!(ExternalIntentFileV2::create(&other.guard, records[1].clone()).is_err());
    let (parent, id) = paths(&other);
    assert!(!parent.join(head_name(&id)).exists());
}

#[test]
fn intent_file_rejects_head_replacement_payload_changes_and_unsafe_links() {
    for mutation in 0..4 {
        let fixture = GuardFixture::new(&"a".repeat(64));
        let records = chain(&fixture);
        let owner = ExternalIntentFileV2::create(&fixture.guard, records[0].clone()).unwrap();
        let (parent, id) = paths(&fixture);
        let head = parent.join(head_name(&id));
        match mutation {
            0 => std::fs::write(&head, b"foreign bytes").unwrap(),
            1 => {
                std::fs::rename(&head, parent.join("saved-original")).unwrap();
                std::fs::write(&head, records[0].encode().unwrap()).unwrap();
                std::fs::set_permissions(&head, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
            2 => std::fs::hard_link(&head, parent.join("alias")).unwrap(),
            _ => std::fs::set_permissions(&head, std::fs::Permissions::from_mode(0o644)).unwrap(),
        }
        assert!(owner.revalidate(&fixture.guard).is_err());
        assert!(owner.advance(&fixture.guard, records[1].clone()).is_err());
        fixture.guard.verify_migrating().unwrap();
    }
}

#[test]
fn intent_file_rejects_missing_or_changed_history_and_future_candidates_on_restart() {
    for mutation in 0..3 {
        let fixture = GuardFixture::new(&"a".repeat(64));
        let records = chain(&fixture);
        let owner = ExternalIntentFileV2::create(&fixture.guard, records[0].clone())
            .unwrap()
            .advance(&fixture.guard, records[1].clone())
            .unwrap();
        let (parent, id) = paths(&fixture);
        let old = parent.join(archive_name(&id, PHASES[0]));
        match mutation {
            0 => std::fs::rename(&old, parent.join("saved-old")).unwrap(),
            1 => std::fs::write(&old, b"foreign history").unwrap(),
            _ => {
                let candidate = parent.join(archive_name(&id, PHASES[1]));
                std::fs::write(&candidate, records[2].encode().unwrap()).unwrap();
                std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o600))
                    .unwrap();
            }
        }
        assert!(owner.revalidate(&fixture.guard).is_err());
        assert!(ExternalIntentFileV2::open(&fixture.guard).is_err());
        assert_eq!(
            std::fs::read(parent.join(head_name(&id))).unwrap(),
            records[1].encode().unwrap()
        );
    }
}

#[test]
fn intent_exchange_retains_competing_head_or_candidate_and_restart_refuses_it() {
    for replace_candidate in [false, true] {
        let fixture = GuardFixture::new(&"a".repeat(64));
        let records = chain(&fixture);
        let owner = ExternalIntentFileV2::create(&fixture.guard, records[0].clone()).unwrap();
        let (parent, id) = paths(&fixture);
        let archive = parent.join(archive_name(&id, PHASES[0]));
        let head = parent.join(head_name(&id));
        let target = if replace_candidate {
            archive.clone()
        } else {
            head.clone()
        };
        let saved = parent.join("saved-raced-original");
        BEFORE_EXCHANGE.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                std::fs::rename(&target, saved).unwrap();
                std::fs::write(&target, b"competing object").unwrap();
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
                Ok(())
            }))
        });
        assert!(owner.advance(&fixture.guard, records[1].clone()).is_err());
        let retained = if replace_candidate { head } else { archive };
        assert_eq!(std::fs::read(retained).unwrap(), b"competing object");
        assert!(parent.join("saved-raced-original").exists());
        assert!(ExternalIntentFileV2::open(&fixture.guard).is_err());
        fixture.guard.verify_migrating().unwrap();
    }
}

#[test]
fn intent_interruption_after_durable_candidate_blocks_restart_without_repair() {
    let fixture = GuardFixture::new(&"a".repeat(64));
    let records = chain(&fixture);
    let owner = ExternalIntentFileV2::create(&fixture.guard, records[0].clone()).unwrap();
    BEFORE_EXCHANGE.with(|slot| *slot.borrow_mut() = Some(Box::new(|| Err(Error::Storage))));
    assert!(matches!(
        owner.advance(&fixture.guard, records[1].clone()),
        Err(Error::Storage)
    ));
    let (parent, id) = paths(&fixture);
    assert_eq!(
        std::fs::read(parent.join(head_name(&id))).unwrap(),
        records[0].encode().unwrap()
    );
    assert_eq!(
        std::fs::read(parent.join(archive_name(&id, PHASES[0]))).unwrap(),
        records[1].encode().unwrap()
    );
    assert!(ExternalIntentFileV2::open(&fixture.guard).is_err());
    fixture.guard.verify_migrating().unwrap();
}

#[test]
fn intent_admits_actual_mount_before_writing_any_body() {
    use std::os::unix::fs::MetadataExt;
    let fixture = GuardFixture::new(&"a".repeat(64));
    let initial = chain(&fixture)[0].clone();
    let parent_inode = fixture.guard.parent.metadata().unwrap().ino();
    let reached = std::rc::Rc::new(std::cell::Cell::new(false));
    let hit = reached.clone();
    MOUNT_CHECK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move |parent, file| {
            assert_eq!(parent.metadata().unwrap().ino(), parent_inode);
            assert!(file.metadata().unwrap().is_file());
            assert_eq!(file.metadata().unwrap().len(), 0);
            hit.set(true);
            Err(Error::Protection)
        }))
    });
    assert!(matches!(
        ExternalIntentFileV2::create(&fixture.guard, initial),
        Err(Error::Protection)
    ));
    assert!(reached.get());
    let (parent, id) = paths(&fixture);
    assert_eq!(std::fs::read(parent.join(head_name(&id))).unwrap(), b"");
    fixture.guard.verify_migrating().unwrap();
}
