//! Approved runtime publication, registry CAS, and management-effect fault checks.

use super::*;
use crate::management_effect::{
    ManagementEffectStateV1, ManagementPermissionScopeV1, ManagementRewindDispositionV1,
    mcp_change::{set_local_source_before_save_hook, set_local_source_fault},
};

#[test]
fn approved_copy_completes_c28_before_c27_and_preserves_unrelated_registration() {
    if isolated(concat!(
        module_path!(),
        "::approved_copy_completes_c28_before_c27_and_preserves_unrelated_registration"
    )) {
        return;
    }
    let fixture = Fixture::new();
    let source_before = snapshot(&fixture.source);
    let secrets = fixture.home.join(".polaris/mcp/secrets.json");
    let secrets_before = fs::read(&secrets).unwrap();
    let preparation = fixture.prepare();
    let preview = preparation.preview().clone();
    let (lease, effects) = fixture.effects();
    let receipt = preparation
        .apply(&fixture.host, &effects, &lease, &fixture.project)
        .unwrap();
    assert_eq!(receipt.cwd, preview.new_cwd);
    assert_eq!(receipt.server_id, SERVER);
    assert_eq!(receipt.registry_revision, 2);
    assert_eq!(receipt.protocol_profile, ProtocolProfile::Auto2024Compat);
    let registry = fixture.registry();
    let current = &registry.servers[0];
    let before = &fixture.before.servers[0];
    assert_eq!(current.server_id, before.server_id);
    assert_eq!(current.enabled_projects, before.enabled_projects);
    assert_eq!(current.capabilities, before.capabilities);
    assert_eq!(current.timeouts, before.timeouts);
    assert_eq!(registry.servers[1..], fixture.before.servers[1..]);
    assert_eq!(fs::read(&secrets).unwrap(), secrets_before);
    assert_eq!(
        fs::read(preview.old_cwd.join("retained-old-runtime")).unwrap(),
        b"old"
    );
    let Destination::Stdio {
        command,
        args,
        cwd,
        sandbox_profile,
        secret_env,
        readable_roots,
        executable_sha256,
        script,
        script_sha256,
    } = &current.transport
    else {
        panic!("stdio retained")
    };
    assert_eq!(command, &fixture.node);
    assert_eq!(cwd, &receipt.cwd);
    assert_eq!(
        args,
        &[receipt.cwd.join("build/index.js").to_str().unwrap()]
    );
    assert_eq!(sandbox_profile, "loopback");
    assert_eq!(
        secret_env,
        &BTreeMap::from([("UNITY_DUMMY".into(), "unity-dummy".into())])
    );
    assert_eq!(readable_roots, &[fixture.node.clone()]);
    assert_eq!(
        executable_sha256.as_deref(),
        Some(preview.node_sha256.as_str())
    );
    assert_eq!(
        script.as_deref(),
        Some(receipt.cwd.join("build/index.js").as_path())
    );
    assert_eq!(
        script_sha256.as_deref(),
        Some(preview.entry_sha256.as_str())
    );
    fixture.host.validate(current).unwrap();
    assert!(!receipt.cwd.join("node_modules/.bin").exists());
    assert_eq!(snapshot(&fixture.source), source_before);
    let left = fs::metadata(receipt.cwd.join(PAIR[0])).unwrap();
    let right = fs::metadata(receipt.cwd.join(PAIR[1])).unwrap();
    assert_eq!(left.nlink(), 1);
    assert_eq!(right.nlink(), 1);
    assert_ne!((left.dev(), left.ino()), (right.dev(), right.ino()));
    assert_eq!(left.len(), 9_817_266);
    assert_eq!(right.len(), 9_817_266);
    for relative in PAIR {
        assert_eq!(
            digest(&receipt.cwd.join(relative)),
            preview.hardlink_pair_sha256
        );
        assert_ne!(
            fs::metadata(fixture.source.join(relative)).unwrap().ino(),
            fs::metadata(receipt.cwd.join(relative)).unwrap().ino()
        );
    }
    let manifest: serde_json::Value = serde_json::from_slice(
        &fs::read(receipt.cwd.join(".polaris-unity-runtime-v1.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["source_manifest_sha256"], preview.manifest_sha256);
    assert_eq!(manifest["preset"], "unity-local-v2");
    let mut records = effects.effects().unwrap();
    records.sort_by_key(|effect| effect.last_sequence);
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].request.inventory.encoded(), "C28");
    assert_eq!(records[1].request.inventory.encoded(), "C27");
    for record in records {
        assert!(matches!(
            record.state,
            ManagementEffectStateV1::Completed { .. }
        ));
        assert_eq!(
            record.request.scope.permission,
            ManagementPermissionScopeV1::System
        );
        assert_eq!(record.request.scope.account_binding, None);
        assert_eq!(
            record.request.rewind,
            ManagementRewindDispositionV1::IrreversibleLocal
        );
    }
    assert!(effects.blocker().unwrap().is_none());
}

#[test]
fn source_copy_race_leaves_unknown_c28_and_never_starts_registry_change() {
    if isolated(concat!(
        module_path!(),
        "::source_copy_race_leaves_unknown_c28_and_never_starts_registry_change"
    )) {
        return;
    }
    let fixture = Fixture::new();
    let preparation = fixture.prepare();
    let destination = preparation.preview().new_cwd.clone();
    let changed = fixture.source.join("build/index.js");
    super::super::set_unity_copy_fault(Some(Box::new(move || {
        fs::write(changed, b"racing").unwrap();
    })));
    let (lease, effects) = fixture.effects();
    assert!(
        preparation
            .apply(&fixture.host, &effects, &lease, &fixture.project)
            .is_err()
    );
    let records = effects.effects().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].request.inventory.encoded(), "C28");
    assert!(matches!(
        records[0].state,
        ManagementEffectStateV1::OutcomeUnknown { .. }
    ));
    assert!(!destination.exists());
    assert_eq!(fixture.registry(), fixture.before);
    let retained: Vec<_> = fs::read_dir(destination.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_str().unwrap().starts_with(".partial-"))
        .collect();
    assert_eq!(retained.len(), 1);
    assert!(retained[0].to_str().unwrap().starts_with(".partial-"));
}

#[test]
fn stale_registry_refuses_before_copy_and_keeps_the_concurrent_registration() {
    if isolated(concat!(
        module_path!(),
        "::stale_registry_refuses_before_copy_and_keeps_the_concurrent_registration"
    )) {
        return;
    }
    let mut fixture = Fixture::new();
    let preparation = fixture.prepare();
    let destination = preparation.preview().new_cwd.clone();
    fixture.replace_registry(|server| {
        server.enabled_projects.insert("concurrent-project".into());
    });
    let (lease, effects) = fixture.effects();
    assert!(
        preparation
            .apply(&fixture.host, &effects, &lease, &fixture.project)
            .is_err()
    );
    assert_eq!(fixture.registry(), fixture.before);
    assert!(effects.effects().unwrap().is_empty());
    assert!(!destination.exists());
}

#[test]
fn registry_change_during_copy_retains_the_runtime_and_marks_c27_unknown() {
    if isolated(concat!(
        module_path!(),
        "::registry_change_during_copy_retains_the_runtime_and_marks_c27_unknown"
    )) {
        return;
    }
    let fixture = Fixture::new();
    let preparation = fixture.prepare();
    let destination = preparation.preview().new_cwd.clone();
    let home = fixture.home.clone();
    super::super::set_unity_copy_fault(Some(Box::new(move || {
        let store = Store::open_existing(&home).unwrap();
        let mut next = store.load_registry().unwrap().unwrap();
        let previous = next.revision;
        next.revision += 1;
        next.servers[0]
            .enabled_projects
            .insert("concurrent-project".into());
        store.save_registry(previous, &next).unwrap();
    })));
    let (lease, effects) = fixture.effects();
    assert!(
        preparation
            .apply(&fixture.host, &effects, &lease, &fixture.project)
            .is_err()
    );
    assert!(destination.join("build/index.js").is_file());
    let records = effects.effects().unwrap();
    assert_eq!(records.len(), 2);
    let c28 = records
        .iter()
        .find(|record| record.request.inventory.encoded() == "C28")
        .unwrap();
    let c27 = records
        .iter()
        .find(|record| record.request.inventory.encoded() == "C27")
        .unwrap();
    assert!(matches!(
        c28.state,
        ManagementEffectStateV1::Completed { .. }
    ));
    assert!(matches!(
        c27.state,
        ManagementEffectStateV1::OutcomeUnknown { .. }
    ));
    let registry = fixture.registry();
    assert_eq!(registry.revision, fixture.before.revision + 1);
    assert!(
        registry.servers[0]
            .enabled_projects
            .contains("concurrent-project")
    );
    assert_eq!(
        registry.servers[0].transport,
        fixture.before.servers[0].transport
    );
    assert_eq!(registry.servers[1], fixture.before.servers[1]);
}

#[test]
fn registry_cas_conflict_after_c27_start_is_unknown_and_retains_concurrent_data() {
    if isolated(concat!(
        module_path!(),
        "::registry_cas_conflict_after_c27_start_is_unknown_and_retains_concurrent_data"
    )) {
        return;
    }
    let fixture = Fixture::new();
    let preparation = fixture.prepare();
    let destination = preparation.preview().new_cwd.clone();
    let home = fixture.home.clone();
    set_local_source_before_save_hook(Some(Box::new(move || {
        let store = Store::open_existing(&home).unwrap();
        let mut next = store.load_registry().unwrap().unwrap();
        let previous = next.revision;
        next.revision += 1;
        next.servers[1]
            .enabled_projects
            .insert("concurrent-unrelated-project".into());
        store.save_registry(previous, &next).unwrap();
    })));
    let (lease, effects) = fixture.effects();
    assert!(
        preparation
            .apply(&fixture.host, &effects, &lease, &fixture.project)
            .is_err()
    );
    let records = effects.effects().unwrap();
    assert_eq!(records.len(), 2);
    let c28 = records
        .iter()
        .find(|record| record.request.inventory.encoded() == "C28")
        .unwrap();
    let c27 = records
        .iter()
        .find(|record| record.request.inventory.encoded() == "C27")
        .unwrap();
    assert!(matches!(
        c28.state,
        ManagementEffectStateV1::Completed { .. }
    ));
    assert!(matches!(
        c27.state,
        ManagementEffectStateV1::OutcomeUnknown { .. }
    ));
    assert!(destination.join("build/index.js").is_file());
    let registry = fixture.registry();
    assert_eq!(
        registry.revision,
        fixture.before.revision + 1,
        "CAS must not save a second revision"
    );
    assert_eq!(registry.servers[0], fixture.before.servers[0]);
    assert!(
        registry.servers[1]
            .enabled_projects
            .contains("concurrent-unrelated-project")
    );
    assert!(effects.blocker().unwrap().is_some());
}

#[test]
fn effect_fault_boundaries_never_retry_copy_or_registry_save() {
    if isolated(concat!(
        module_path!(),
        "::effect_fault_boundaries_never_retry_copy_or_registry_save"
    )) {
        return;
    }
    let mut fixtures = Vec::new();
    for point in [
        "c28_before_prepare",
        "c28_after_prepare",
        "c28_after_start",
        "c28_after_publish",
        "c28_complete",
        "c28_after_complete",
        "c27_before_prepare",
        "c27_after_prepare",
        "c27_after_start",
        "c27_after_save",
        "c27_complete",
        "c27_after_complete",
    ] {
        let fixture = Fixture::new();
        let preparation = fixture.prepare();
        let destination = preparation.preview().new_cwd.clone();
        let (lease, effects) = fixture.effects();
        set_local_source_fault(Some(point));
        assert!(
            preparation
                .apply(&fixture.host, &effects, &lease, &fixture.project)
                .is_err(),
            "point {point}"
        );
        let records = effects.effects().unwrap();
        let state = |inventory: &str| {
            records
                .iter()
                .find(|effect| effect.request.inventory.encoded() == inventory)
                .map(|effect| &effect.state)
        };
        let published = !matches!(
            point,
            "c28_before_prepare" | "c28_after_prepare" | "c28_after_start"
        );
        assert_eq!(destination.exists(), published, "point {point}");
        if point == "c28_before_prepare" {
            assert!(records.is_empty());
        } else if point == "c28_after_prepare" {
            assert_eq!(state("C28"), Some(&ManagementEffectStateV1::Prepared));
        } else if matches!(
            point,
            "c28_after_start" | "c28_after_publish" | "c28_complete"
        ) {
            assert!(
                matches!(
                    state("C28"),
                    Some(ManagementEffectStateV1::OutcomeUnknown { .. })
                ),
                "point {point}"
            );
        } else {
            assert!(
                matches!(
                    state("C28"),
                    Some(ManagementEffectStateV1::Completed { .. })
                ),
                "point {point}"
            );
        }
        match point {
            "c27_after_prepare" => {
                assert_eq!(state("C27"), Some(&ManagementEffectStateV1::Prepared))
            }
            "c27_after_start" | "c27_after_save" | "c27_complete" => assert!(matches!(
                state("C27"),
                Some(ManagementEffectStateV1::OutcomeUnknown { .. })
            )),
            "c27_after_complete" => assert!(matches!(
                state("C27"),
                Some(ManagementEffectStateV1::Completed { .. })
            )),
            _ => assert!(state("C27").is_none(), "point {point} must not start C27"),
        }
        let registry = fixture.registry();
        if matches!(
            point,
            "c27_after_save" | "c27_complete" | "c27_after_complete"
        ) {
            assert_eq!(registry.revision, fixture.before.revision + 1);
            let Destination::Stdio { cwd, .. } = &registry.servers[0].transport else {
                unreachable!()
            };
            assert_eq!(cwd, &destination);
        } else {
            assert_eq!(registry, fixture.before, "point {point}");
        }
        if published {
            let siblings: Vec<_> = fs::read_dir(destination.parent().unwrap())
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| {
                    path.file_name()
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .starts_with("runtime-")
                })
                .collect();
            assert_eq!(
                siblings,
                vec![destination],
                "one apply must not publish retries or delete the unknown artifact at {point}"
            );
        }
        fixtures.push(fixture);
    }
}
