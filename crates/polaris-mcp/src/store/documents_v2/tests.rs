//! V2 credential and registry baseline envelope validation tests.
use super::*;
use std::collections::BTreeMap;

// Unix keeps exercising the public generators. Non-Unix uses explicit wire
// fixtures only for validation; the public generators are tested for refusal.
fn credential_fixture(
    migration_id: &str,
    source: Option<Secrets>,
) -> Result<CredentialDocumentV2, Error> {
    #[cfg(unix)]
    {
        CredentialDocumentV2::baseline(migration_id, source)
    }
    #[cfg(not(unix))]
    {
        CredentialDocumentV2::baseline_with_token(migration_id, source, "c".repeat(64))
    }
}

fn registry_fixture(
    migration_id: &str,
    payload: Option<Registry>,
    source_file_hash: Option<String>,
) -> Result<RegistryDocumentV2, Error> {
    #[cfg(unix)]
    {
        RegistryDocumentV2::baseline(migration_id, payload, source_file_hash)
    }
    #[cfg(not(unix))]
    {
        let token = "c".repeat(64);
        let receipt = baseline_receipt(
            migration_id,
            DocumentKindV2::Registry,
            presence(&payload),
            payload.as_ref().map_or(0, |value| value.revision),
            &token,
            registry_reference_hash(&payload)?,
            Some(RegistryBaselineProjectionV1 {
                source_file_hash,
                canonical_payload_hash: safe_hash(&payload)?,
            }),
        );
        let document = RegistryDocumentV2(RegistryEnvelopeWireV2 {
            schema_version: 2,
            random_token: token,
            receipt,
            payload,
        });
        document.validate()?;
        Ok(document)
    }
}

impl CredentialDocumentV2 {
    fn fixture_replacement(
        &self,
        operation_id: &str,
        plan_hash: &str,
        next: Secrets,
    ) -> Result<Self, Error> {
        #[cfg(unix)]
        {
            self.replacement(operation_id, plan_hash, next)
        }
        #[cfg(not(unix))]
        {
            let before = self.metadata()?;
            let token = "d".repeat(64);
            let payload = Some(next.0);
            let references = reference_hash(
                payload
                    .iter()
                    .flat_map(|value| value.values.keys().map(String::as_str)),
            )?;
            let receipt = managed_receipt(
                operation_id,
                plan_hash,
                before,
                &token,
                payload.as_ref().unwrap().revision,
                references,
                None,
            );
            let document = Self(SecretEnvelopeWireV2 {
                schema_version: 2,
                random_token: token,
                receipt,
                payload,
            });
            document.validate()?;
            Ok(document)
        }
    }
}

impl RegistryDocumentV2 {
    fn fixture_replacement(
        &self,
        operation_id: &str,
        plan_hash: &str,
        next: Registry,
    ) -> Result<Self, Error> {
        #[cfg(unix)]
        {
            self.replacement(operation_id, plan_hash, next)
        }
        #[cfg(not(unix))]
        {
            let before = self.metadata()?;
            let token = "d".repeat(64);
            let revision = next.revision;
            let payload = Some(next);
            let receipt = managed_receipt(
                operation_id,
                plan_hash,
                before,
                &token,
                revision,
                registry_reference_hash(&payload)?,
                Some(safe_hash(&payload)?),
            );
            let document = Self(RegistryEnvelopeWireV2 {
                schema_version: 2,
                random_token: token,
                receipt,
                payload,
            });
            document.validate()?;
            Ok(document)
        }
    }
}

fn secrets(revision: u64, value: &str) -> Secrets {
    Secrets::new(
        revision,
        BTreeMap::from([("owned_ref".into(), value.into())]),
    )
    .unwrap()
}
fn registry(revision: u64) -> Registry {
    Registry {
        schema_version: 1,
        revision,
        servers: vec![],
    }
}
fn mutate_secret(
    document: &CredentialDocumentV2,
    mutate: impl FnOnce(&mut serde_json::Value),
) -> Vec<u8> {
    let bytes = document.encode().unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    mutate(&mut value);
    serde_json::to_vec(&value).unwrap()
}

#[test]
fn credential_baseline_round_trips_preserved_values_revision_and_metadata() {
    let document = credential_fixture(&"a".repeat(64), Some(secrets(7, "owned-secret-a"))).unwrap();
    document
        .verify_preserved_source(Some(&secrets(7, "owned-secret-a")))
        .unwrap();
    let metadata = document.metadata().unwrap();
    assert_eq!(metadata.presence, DocumentPresenceV2::Present);
    assert_eq!(metadata.revision, 7);
    let restored = CredentialDocumentV2::decode(&document.encode().unwrap()).unwrap();
    assert_eq!(restored.metadata().unwrap(), metadata);
    restored
        .verify_preserved_source(Some(&secrets(7, "owned-secret-a")))
        .unwrap();
    let projected = restored.into_snapshot().unwrap();
    assert_eq!(projected.revision(), 7);
    assert_eq!(projected.resolve("owned_ref").unwrap(), "owned-secret-a");
}

#[test]
fn absent_baseline_has_explicit_zero_revision_and_absent_snapshot() {
    let first = credential_fixture(&"a".repeat(64), None).unwrap();
    let metadata = first.metadata().unwrap();
    assert_eq!(metadata.presence, DocumentPresenceV2::Absent);
    assert_eq!(metadata.revision, 0);
    first.verify_preserved_source(None).unwrap();
    assert!(first.into_snapshot().is_none());
    let registry = registry_fixture(&"a".repeat(64), None, None).unwrap();
    assert_eq!(
        registry.metadata().unwrap().presence,
        DocumentPresenceV2::Absent
    );
    assert!(
        RegistryDocumentV2::decode(&registry.encode().unwrap())
            .unwrap()
            .into_snapshot()
            .is_none()
    );
    assert!(registry_fixture(&"a".repeat(64), None, Some("b".repeat(64))).is_err());
}

#[test]
fn secret_receipt_is_independent_of_value_bytes_and_size_but_migration_compares_directly() {
    let first = CredentialDocumentV2::baseline_with_token(
        &"a".repeat(64),
        Some(secrets(7, "owned-short")),
        "c".repeat(64),
    )
    .unwrap();
    let second = CredentialDocumentV2::baseline_with_token(
        &"a".repeat(64),
        Some(secrets(7, &"owned-long-secret".repeat(100))),
        "c".repeat(64),
    )
    .unwrap();
    assert_eq!(first.metadata().unwrap(), second.metadata().unwrap());
    assert_eq!(first.receipt(), second.receipt());
    assert_ne!(
        first.encode().unwrap().len(),
        second.encode().unwrap().len()
    );
    assert!(
        second
            .verify_preserved_source(Some(&secrets(7, "owned-short")))
            .is_err()
    );
    let receipt = serde_json::to_string(first.receipt()).unwrap();
    for excluded in [
        "owned-short",
        "owned-long-secret",
        "source_file_hash",
        "canonical_payload_hash",
        "value_length",
    ] {
        assert!(!receipt.contains(excluded));
    }
    assert!(!format!("{first:?}").contains("owned-short"));
    assert!(
        first
            .verify_preserved_source(Some(&secrets(8, "owned-short")))
            .is_err()
    );
    assert!(first.verify_preserved_source(None).is_err());
}

#[test]
fn secret_reader_rejects_unknown_schema_fields_token_revision_and_reference_projection() {
    let document = credential_fixture(&"a".repeat(64), Some(secrets(7, "owned-secret"))).unwrap();
    for change in [
        |v: &mut serde_json::Value| {
            v["schema_version"] = 1.into();
        },
        |v: &mut serde_json::Value| {
            v["unknown"] = true.into();
        },
        |v: &mut serde_json::Value| {
            v["random_token"] = "d".repeat(64).into();
        },
        |v: &mut serde_json::Value| {
            v["payload"]["revision"] = 8.into();
        },
        |v: &mut serde_json::Value| {
            v["payload"]["values"]["another_ref"] = "owned-added".into();
        },
        |v: &mut serde_json::Value| {
            v["receipt"]["receipt"]["source_file_hash"] = "e".repeat(64).into();
        },
        |v: &mut serde_json::Value| {
            v["receipt"]["receipt"]["source_schema_version"] = serde_json::Value::Null;
        },
    ] {
        assert!(CredentialDocumentV2::decode(&mutate_secret(&document, change)).is_err());
    }
    // A same-reference value change has no value-derived receipt. The migration
    // owner must compare the reopened source bytes directly to reject this case.
    let changed = mutate_secret(&document, |v| {
        v["payload"]["values"]["owned_ref"] = "owned-different".into();
    });
    let changed = CredentialDocumentV2::decode(&changed).unwrap();
    assert_eq!(changed.metadata().unwrap(), document.metadata().unwrap());
    assert!(
        changed
            .verify_preserved_source(Some(&secrets(7, "owned-secret")))
            .is_err()
    );
}

#[test]
fn secret_reader_honors_exact_whole_document_byte_limit() {
    let document = credential_fixture(&"a".repeat(64), Some(secrets(7, "owned-secret"))).unwrap();
    let mut bytes = document.encode().unwrap().to_vec();
    bytes.resize(MAX_STORE_BYTES, b' ');
    assert!(CredentialDocumentV2::decode(&bytes).is_ok());
    bytes.push(b' ');
    assert!(matches!(
        CredentialDocumentV2::decode(&bytes),
        Err(Error::Limit)
    ));
    let oversized = credential_fixture(
        &"a".repeat(64),
        Some(secrets(7, &"a".repeat(MAX_STORE_BYTES))),
    )
    .unwrap();
    assert!(matches!(oversized.encode(), Err(Error::Limit)));
}

#[test]
fn credential_managed_replacement_binds_before_and_checked_next_revision_without_value_hashes() {
    let before = credential_fixture(&"a".repeat(64), Some(secrets(7, "owned-first"))).unwrap();
    let after = before
        .fixture_replacement(
            "owned-operation",
            &"b".repeat(64),
            secrets(8, "owned-second"),
        )
        .unwrap();
    let DocumentReceiptV2::Managed { receipt } = after.receipt() else {
        panic!("managed receipt required")
    };
    assert_eq!(receipt.before, before.metadata().unwrap());
    assert_eq!(receipt.after_revision, 8);
    assert_ne!(receipt.after_token, receipt.before.token);
    assert!(receipt.registry_payload_hash.is_none());
    let decoded = CredentialDocumentV2::decode(&after.encode().unwrap()).unwrap();
    decoded
        .verify_preserved_source(Some(&secrets(8, "owned-second")))
        .unwrap();
    assert!(
        before
            .fixture_replacement(
                "owned-operation",
                &"b".repeat(64),
                secrets(7, "owned-second")
            )
            .is_err()
    );
    assert!(
        before
            .fixture_replacement(
                "owned-operation",
                &"b".repeat(64),
                secrets(9, "owned-second")
            )
            .is_err()
    );
    assert!(
        before
            .fixture_replacement("owned/invalid", &"b".repeat(64), secrets(8, "owned-second"))
            .is_err()
    );
    assert!(
        before
            .fixture_replacement("owned-operation", "invalid", secrets(8, "owned-second"))
            .is_err()
    );
    let max = credential_fixture(&"a".repeat(64), Some(secrets(u64::MAX, "owned-first"))).unwrap();
    assert!(
        max.fixture_replacement(
            "owned-operation",
            &"b".repeat(64),
            secrets(1, "owned-second")
        )
        .is_err()
    );
    let mut tampered = mutate_secret(&after, |v| {
        v["receipt"]["receipt"]["before"]["receipt_hash"] = "invalid".into();
    });
    assert!(CredentialDocumentV2::decode(&tampered).is_err());
    tampered = mutate_secret(&after, |v| {
        v["receipt"]["receipt"]["after_token"] = v["receipt"]["receipt"]["before"]["token"].clone();
    });
    assert!(CredentialDocumentV2::decode(&tampered).is_err());
}

#[test]
fn absent_credential_baseline_can_start_at_revision_one_and_ensure_absent_keeps_present_receipt() {
    let before = credential_fixture(&"a".repeat(64), None).unwrap();
    let next = Secrets::new(1, BTreeMap::new()).unwrap();
    let after = before
        .fixture_replacement("owned-operation", &"b".repeat(64), next)
        .unwrap();
    assert_eq!(
        after.metadata().unwrap().presence,
        DocumentPresenceV2::Present
    );
    let snapshot = after.into_snapshot().unwrap();
    assert_eq!(snapshot.revision(), 1);
    assert_eq!(snapshot.references().count(), 0);
}

#[test]
fn registry_baseline_binds_source_hash_and_new_payload_and_projects_registry() {
    let document =
        registry_fixture(&"a".repeat(64), Some(registry(7)), Some("b".repeat(64))).unwrap();
    let DocumentReceiptV2::MigrationBaseline { receipt } = document.receipt() else {
        panic!("baseline required")
    };
    let projection = receipt.registry_projection.as_ref().unwrap();
    assert_eq!(
        projection.source_file_hash.as_deref(),
        Some("b".repeat(64).as_str())
    );
    assert_eq!(document.snapshot().unwrap().revision, 7);
    let decoded = RegistryDocumentV2::decode(&document.encode().unwrap()).unwrap();
    assert_eq!(decoded.metadata().unwrap(), document.metadata().unwrap());
    assert_eq!(decoded.into_snapshot().unwrap(), registry(7));
    assert!(registry_fixture(&"a".repeat(64), Some(registry(7)), None).is_err());
    assert!(registry_fixture(&"a".repeat(64), Some(registry(7)), Some("invalid".into())).is_err());
    assert!(registry_fixture("invalid", Some(registry(7)), Some("b".repeat(64))).is_err());
}

#[test]
fn registry_reader_rejects_changed_payload_and_managed_receipt_identity() {
    let before =
        registry_fixture(&"a".repeat(64), Some(registry(7)), Some("b".repeat(64))).unwrap();
    let after = before
        .fixture_replacement("owned-operation", &"c".repeat(64), registry(8))
        .unwrap();
    assert_eq!(after.metadata().unwrap().revision, 8);
    assert!(
        before
            .fixture_replacement("owned-operation", &"c".repeat(64), registry(9))
            .is_err()
    );
    let mut value: serde_json::Value = serde_json::from_slice(&after.encode().unwrap()).unwrap();
    value["payload"]["revision"] = 9.into();
    assert!(RegistryDocumentV2::decode(&serde_json::to_vec(&value).unwrap()).is_err());
    let mut value: serde_json::Value = serde_json::from_slice(&after.encode().unwrap()).unwrap();
    value["receipt"]["receipt"]["registry_payload_hash"] = "f".repeat(64).into();
    assert!(RegistryDocumentV2::decode(&serde_json::to_vec(&value).unwrap()).is_err());
    let decoded = RegistryDocumentV2::decode(&after.encode().unwrap()).unwrap();
    assert_eq!(decoded.snapshot().unwrap().revision, 8);
}

#[test]
fn document_metadata_rejects_absence_revision_and_malformed_receipt_pairs() {
    let doc = credential_fixture(&"a".repeat(64), None).unwrap();
    let mut metadata = doc.metadata().unwrap();
    metadata.revision = 1;
    assert!(metadata.validate().is_err());
    metadata.presence = DocumentPresenceV2::Present;
    metadata.receipt_hash = "invalid".into();
    assert!(metadata.validate().is_err());
    let invalid = DocumentMetadataV2 {
        kind: DocumentKindV2::Registry,
        presence: DocumentPresenceV2::Present,
        revision: 0,
        token: "b".repeat(64),
        receipt_hash: "c".repeat(64),
    };
    assert!(invalid.validate().is_err());
}

#[test]
fn rejected_secret_documents_wipe_values_built_before_the_parse_failure() {
    // Values precede the missing/invalid field so the parser allocates owned
    // secret copies before it rejects each fixture.
    for payload in [
        r#"{"schema_version":1,"values":{"owned_ref":"owned-secret"}}"#,
        r#"{"schema_version":1,"revision":7,"values":{"owned_ref":"owned-secret","bad":42}}"#,
        r#"{"schema_version":1,"values":{"owned_ref":"owned-first","owned_ref":"owned-last"}}"#,
        r#"{"schema_version":1,"values":{"owned_ref":"owned-secret"},"revision":"wrong"}"#,
    ] {
        super::super::STORED_SECRET_WIPES.with(|count| count.set(0));
        assert!(serde_json::from_str::<SecretDocument>(payload).is_err());
        assert!(super::super::STORED_SECRET_WIPES.with(|count| count.get()) >= 1);
    }
}

#[cfg(unix)]
#[test]
fn native_document_generators_produce_fresh_baseline_and_replacement_tokens() {
    let migration = "a".repeat(64);
    let plan = "b".repeat(64);
    let first = CredentialDocumentV2::baseline(&migration, None).unwrap();
    let second = CredentialDocumentV2::baseline(&migration, None).unwrap();
    assert_ne!(
        first.metadata().unwrap().token,
        second.metadata().unwrap().token
    );
    let after = first
        .replacement("operation", &plan, secrets(1, "synthetic"))
        .unwrap();
    assert_ne!(
        first.metadata().unwrap().token,
        after.metadata().unwrap().token
    );
    assert!(digest_id(&after.metadata().unwrap().token));

    let first = RegistryDocumentV2::baseline(&migration, None, None).unwrap();
    let second = RegistryDocumentV2::baseline(&migration, None, None).unwrap();
    assert_ne!(
        first.metadata().unwrap().token,
        second.metadata().unwrap().token
    );
    let after = first.replacement("operation", &plan, registry(1)).unwrap();
    assert_ne!(
        first.metadata().unwrap().token,
        after.metadata().unwrap().token
    );
    assert!(digest_id(&after.metadata().unwrap().token));
}

#[cfg(windows)]
#[test]
fn windows_mcp_document_unavailable_baselines_reject_and_wipe_consumed_values() {
    assert_eq!(fresh_token(), Err(Error::Protection));
    let migration = "a".repeat(64);
    assert!(matches!(
        CredentialDocumentV2::baseline(&migration, None),
        Err(Error::Protection)
    ));
    let wipes = super::super::STORED_SECRET_WIPES.with(|count| count.get());
    assert!(matches!(
        CredentialDocumentV2::baseline(&migration, Some(secrets(7, "synthetic-value"))),
        Err(Error::Protection)
    ));
    assert_eq!(
        super::super::STORED_SECRET_WIPES.with(|count| count.get()),
        wipes + 1
    );
    assert!(matches!(
        RegistryDocumentV2::baseline(&migration, None, None),
        Err(Error::Protection)
    ));
    assert!(matches!(
        RegistryDocumentV2::baseline(&migration, Some(registry(7)), Some("b".repeat(64))),
        Err(Error::Protection)
    ));
}

#[cfg(windows)]
#[test]
fn windows_mcp_document_unavailable_replacements_preserve_documents_and_wipe_next_values() {
    let migration = "a".repeat(64);
    let credential = credential_fixture(&migration, Some(secrets(7, "synthetic-before"))).unwrap();
    let credential = CredentialDocumentV2::decode(&credential.encode().unwrap()).unwrap();
    let before = credential.encode().unwrap();
    let wipes = super::super::STORED_SECRET_WIPES.with(|count| count.get());
    assert!(matches!(
        credential.replacement("operation", &"b".repeat(64), secrets(8, "synthetic-after")),
        Err(Error::Protection)
    ));
    assert_eq!(
        super::super::STORED_SECRET_WIPES.with(|count| count.get()),
        wipes + 1
    );
    assert_eq!(credential.encode().unwrap().as_slice(), before.as_slice());
    let document = registry_fixture(&migration, Some(registry(7)), Some("b".repeat(64))).unwrap();
    let document = RegistryDocumentV2::decode(&document.encode().unwrap()).unwrap();
    let before = document.encode().unwrap();
    assert!(matches!(
        document.replacement("operation", &"b".repeat(64), registry(8)),
        Err(Error::Protection)
    ));
    assert_eq!(document.encode().unwrap(), before);
}
