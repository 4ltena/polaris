//! Pure C10 store data checks; no owner, reservation or project lease is acquired.
use super::*;
use std::str::FromStr;

#[test]
fn c10_v2_count_and_byte_capacity_exact_and_each_plus_one() {
    let exact = Counts {
        active: MAX_ACTIVE - 1,
        active_bytes: MAX_ACTIVE_BYTES,
        temporary: MAX_TMP,
        temporary_bytes: MAX_TMP_BYTES,
        archive: MAX_ARCHIVE,
        archive_bytes: MAX_ARCHIVE_BYTES,
    };
    assert!(capacity(exact).is_ok());
    for mode in 0..6 {
        let mut over = exact;
        match mode {
            0 => over.active += 1,
            1 => over.active_bytes += 1,
            2 => over.temporary += 1,
            3 => over.temporary_bytes += 1,
            4 => over.archive += 1,
            _ => over.archive_bytes += 1,
        }
        assert!(capacity(over).is_err());
    }
}

#[test]
fn sync_owner_v2_names_keep_record_and_temporary_shape_closed() {
    let operation = Operation::from_str("00000000000000000000000000000007").unwrap();
    let name = record_name(operation);
    assert!(is_record_name(&name));
    assert!(matches!(classify_name(&name), Ok(NameKind::Record)));
    let suffix = "a".repeat(24);
    let temporary = format!(".{name}.2.{suffix}.tmp");
    match classify_name(&temporary).unwrap() {
        NameKind::Temporary { target, sequence } => {
            assert_eq!(target, name);
            assert_eq!(sequence, 2);
        }
        _ => panic!("a canonical temporary must retain its target and sequence"),
    }
    for bad in [
        format!(".{name}.0.{suffix}.tmp"),
        format!(".{name}.02.{suffix}.tmp"),
        format!(".{name}.2.{}.tmp", "a".repeat(23)),
        format!("{name}.json"),
        format!("../{name}"),
        format!("{}.json", "A".repeat(64)),
    ] {
        assert!(matches!(
            classify_name(&bad),
            Err(C10IntentStoreErrorV2::Corrupt)
        ));
    }
    assert_eq!(component("owner.lock").unwrap().to_bytes(), b"owner.lock");
    assert_eq!(crate_component("archive").unwrap().to_bytes(), b"archive");
    for bad in ["", ".", "..", "part/name", "nul\0name"] {
        assert!(matches!(
            component(bad),
            Err(C10IntentStoreErrorV2::Corrupt)
        ));
        assert!(matches!(
            crate_component(bad),
            Err(C10IntentStoreErrorV2::Corrupt)
        ));
    }
}

#[test]
fn sync_owner_v2_byte_sum_preserves_exact_values_and_refuses_overflow() {
    assert_eq!(add_bytes(5, b"abc").unwrap(), 8);
    assert_eq!(add_bytes(u64::MAX, b"").unwrap(), u64::MAX);
    assert_eq!(add_bytes(u64::MAX - 1, b"x").unwrap(), u64::MAX);
    assert!(matches!(
        add_bytes(u64::MAX, b"x"),
        Err(C10IntentStoreErrorV2::Corrupt)
    ));
}

#[test]
fn sync_owner_v2_inventory_digest_binds_retained_evidence() {
    // These are hash input data; no physical anchor or lease is asserted.
    let anchor = Anchor {
        device: 1,
        inode: 2,
        mode: 0o700,
        uid: 3,
    };
    let evidence = EntryEvidence {
        anchor,
        length: 8,
        hash: SyncDigestV1::digest(b"retained"),
    };
    let inventory = |reverse: bool| {
        let mut value = Inventory::empty();
        let entries = if reverse { ["b", "a"] } else { ["a", "b"] };
        for name in entries {
            value.evidence.insert(name.to_owned(), evidence.clone());
        }
        value
    };
    let original = inventory(false);
    assert_eq!(original.digest(), inventory(true).digest());
    let mut changed = inventory(false);
    changed.evidence.get_mut("a").unwrap().anchor.inode += 1;
    assert_ne!(original.digest(), changed.digest());
    changed = inventory(false);
    changed.evidence.get_mut("b").unwrap().hash = SyncDigestV1::digest(b"changed");
    assert_ne!(original.digest(), changed.digest());
    changed = inventory(false);
    changed.evidence.remove("a");
    assert_ne!(original.digest(), changed.digest());
}
