//! Pure filename, capacity and inventory data checks; no store or lease is opened.
use super::*;

fn id() -> ManagementOperationIdV1 {
    ManagementOperationIdV1::from_str("00000000000000000000000000000007").unwrap()
}

#[test]
fn credential_rebind_store_names_keep_closed_kinds_and_record_identity() {
    let value = id();
    for (suffix, expected) in [
        (".json", EntryKind::Intent),
        (".no-intent.json", EntryKind::NoIntent),
        (".tmp", EntryKind::Temporary),
    ] {
        let (parsed, kind) = classify_name(&format!("{}{suffix}", value.as_hex())).unwrap();
        assert_eq!(parsed, value);
        assert!(kind == expected);
    }
    for name in [
        "0000000000000000000000000000000A.json",
        "000000000000000000000000000000007.json",
        "00000000000000000000000000000007.json.json",
        "../00000000000000000000000000000007.json",
        "00000000000000000000000000000007.unknown",
    ] {
        assert!(matches!(
            classify_name(name),
            Err(CredentialRebindErrorV1::Corrupt)
        ));
    }
    let record = super::super::data_tests::stored_record(value, false);
    assert_eq!(record_name(&record), format!("{}.json", value.as_hex()));
    let record = super::super::data_tests::no_intent_record(value);
    assert_eq!(
        record_name(&record),
        format!("{}.no-intent.json", value.as_hex())
    );
    assert_eq!(component("valid-entry").unwrap().to_bytes(), b"valid-entry");
    for name in ["", ".", "..", "part/name", "nul\0name"] {
        assert!(matches!(
            component(name),
            Err(CredentialRebindErrorV1::Corrupt)
        ));
    }
}

#[test]
fn credential_rebind_store_capacity_accepts_exact_and_refuses_each_overflow() {
    let exact = [
        MAX_ACTIVE as u64,
        MAX_ACTIVE_BYTES,
        MAX_TMP as u64,
        MAX_TMP_BYTES,
        MAX_ARCHIVE as u64,
        MAX_ARCHIVE_BYTES,
    ];
    let check = |values: [u64; 6]| {
        capacity(
            values[0] as usize,
            values[1],
            values[2] as usize,
            values[3],
            values[4] as usize,
            values[5],
        )
    };
    assert!(check([0; 6]).is_ok());
    assert!(check(exact).is_ok());
    for index in 0..6 {
        let mut exceeded = exact;
        exceeded[index] += 1;
        assert!(matches!(
            check(exceeded),
            Err(CredentialRebindErrorV1::Blocked("intent store capacity"))
        ));
    }
}

#[test]
fn credential_rebind_store_byte_sum_keeps_exact_total_and_refuses_overflow() {
    assert_eq!(add_bytes(5, b"abc").unwrap(), 8);
    assert_eq!(add_bytes(u64::MAX, b"").unwrap(), u64::MAX);
    assert_eq!(add_bytes(u64::MAX - 1, b"x").unwrap(), u64::MAX);
    assert!(matches!(
        add_bytes(u64::MAX, b"x"),
        Err(CredentialRebindErrorV1::Corrupt)
    ));
}

#[test]
fn credential_rebind_store_inventory_digest_binds_evidence_and_order() {
    // These are serialization data, never a physical identity or pinned store.
    let anchor = Anchor {
        device: 1,
        inode: 2,
        mode: 0o700,
        uid: 3,
    };
    let first = EntryEvidence::directory(anchor);
    let second = EntryEvidence::file(anchor, b"retained-record");
    let inventory = |reverse: bool| {
        let mut evidence = BTreeMap::new();
        let entries = if reverse {
            [("b", second.clone()), ("a", first.clone())]
        } else {
            [("a", first.clone()), ("b", second.clone())]
        };
        for (name, entry) in entries {
            evidence.insert(name.to_owned(), entry);
        }
        Inventory {
            evidence,
            records: BTreeMap::new(),
            counts: Counts::default(),
        }
    };
    let original = inventory(false);
    assert_eq!(original.digest(), inventory(true).digest());
    let mut changed = inventory(false);
    changed.evidence.get_mut("b").unwrap().hash = Some(ManagementEffectHashV1::digest(b"changed"));
    assert_ne!(original.digest(), changed.digest());
    changed = inventory(false);
    changed.evidence.get_mut("a").unwrap().anchor.inode += 1;
    assert_ne!(original.digest(), changed.digest());
    changed = inventory(false);
    changed.evidence.remove("a");
    assert_ne!(original.digest(), changed.digest());
}
