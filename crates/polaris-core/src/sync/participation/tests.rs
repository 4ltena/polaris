//! Temporary state directories only.
use super::*;

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn m2a_generation_and_pending_reproduce_frozen_bytes_and_exact_body_hash() {
    let base_bytes = include_bytes!(
        "../../../tests/fixtures/apply-intent-v1-capacity-positive/participation-base-marker.v1.json"
    );
    let target_bytes = include_bytes!(
        "../../../tests/fixtures/apply-intent-v1-capacity-positive/participation-target-marker.v1.json"
    );
    let pending_bytes = include_bytes!(
        "../../../tests/fixtures/apply-intent-v1-capacity-positive/participation-pending.v1.json"
    );
    let base = ParticipationGenerationV1::from_canonical_bytes(base_bytes).unwrap();
    let target = ParticipationGenerationV1::from_canonical_bytes(target_bytes).unwrap();
    let pending = C12PendingV1::from_canonical_bytes(pending_bytes).unwrap();
    assert_eq!(base.canonical_bytes().unwrap(), base_bytes);
    assert_eq!(target.canonical_bytes().unwrap(), target_bytes);
    assert_eq!(pending.canonical_bytes().unwrap(), pending_bytes);
    assert_eq!(base.marker_hash().unwrap(), pending.base_marker_hash);
    assert_eq!(target.marker_hash().unwrap(), pending.target_marker_hash);
    assert_eq!(pending.target_marker, target);
    assert_eq!(
        target.filename().unwrap(),
        format!(
            "g00000000000000000009-{}.v1.json",
            target.mutation_id.unwrap().as_hex()
        )
    );
    let body = include_bytes!(
        "../../../tests/fixtures/apply-intent-v1-capacity-positive/participation-target.v1.json"
    );
    pending.verify_target_body(Some(body)).unwrap();
    assert!(pending.verify_target_body(None).is_err());
    let mut changed = body.to_vec();
    changed.push(b' ');
    assert!(pending.verify_target_body(Some(&changed)).is_err());
    for case in 0..6 {
        let mut changed = pending.clone();
        match case {
            0 => changed.base_generation += 1,
            1 => changed.target_body_bytes = 0,
            2 => changed.publication = ParticipationPublicationV1::Write,
            3 => changed.target_marker.mutation_id = None,
            4 => changed.target_marker_hash = pending.base_marker_hash,
            5 => changed.target_body_hash = None,
            _ => unreachable!(),
        }
        assert!(changed.canonical_bytes().is_err(), "case {case}");
    }
    let raw = String::from_utf8(pending_bytes.to_vec()).unwrap();
    for changed in [
        raw.replacen(
            "\"schema_version\":1",
            "\"schema_version\":1,\"schema_version\":1",
            1,
        ),
        raw.replacen(
            "\"schema_version\":1",
            "\"schema_version\":1,\"foreign\":null",
            1,
        ),
        format!("{raw} "),
    ] {
        assert!(C12PendingV1::from_canonical_bytes(changed.as_bytes()).is_err());
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn m2a_capacity_matches_independent_components_and_exact_floor() {
    let base = ParticipationGenerationV1::from_canonical_bytes(include_bytes!("../../../tests/fixtures/apply-intent-v1-capacity-positive/participation-base-marker.v1.json")).unwrap();
    let body = include_bytes!(
        "../../../tests/fixtures/apply-intent-v1-capacity-positive/participation-target.v1.json"
    );
    let bound = c12_mutation_capacity_upper_bound(
        &base,
        ParticipationPublicationV1::Apply,
        Some(body),
        4096,
        false,
    )
    .unwrap();
    assert_eq!(
        bound,
        c12_mutation_capacity_upper_bound(
            &base,
            ParticipationPublicationV1::Write,
            Some(body),
            4096,
            false,
        )
        .unwrap(),
        "normal and ApplyIntent use the same inventory capacity function"
    );
    assert_eq!(
        bound,
        C12MutationCapacityV1 {
            pending_final_bytes: 1_056_768,
            body_publication_temporary_bytes: 4096,
            generation_final_bytes: 4096,
            generation_temporary_bytes: 4096,
            baseline_staging_bytes: 0,
            directory_overhead_bytes: 1_048_576,
            required_bytes: 2_117_632,
        }
    );
    let required = bound.required_bytes + crate::apply_intent::FREE_SPACE_FLOOR_BYTES;
    bound.check_available(required).unwrap();
    assert!(matches!(
        bound.check_available(required - 1),
        Err(C12MutationErrorV1::ResourceUnavailable { .. })
    ));
    let initialized = c12_mutation_capacity_upper_bound(
        &base,
        ParticipationPublicationV1::Write,
        Some(body),
        4096,
        true,
    )
    .unwrap();
    assert_eq!(initialized.baseline_staging_bytes, 2 * 1_048_576 + 8192);
    let leave = c12_mutation_capacity_upper_bound(
        &base,
        ParticipationPublicationV1::Leave,
        None,
        4096,
        false,
    )
    .unwrap();
    assert_eq!(leave.body_publication_temporary_bytes, 0);
    assert_eq!(leave.pending_final_bytes, 1_048_576 + 4096);
    assert!(
        c12_mutation_capacity_upper_bound(
            &base,
            ParticipationPublicationV1::Apply,
            Some(body),
            0,
            false
        )
        .is_err()
    );
    let mut overflow = bound;
    overflow.required_bytes = u64::MAX;
    assert!(matches!(
        overflow.check_available(u64::MAX),
        Err(C12MutationErrorV1::Overflow)
    ));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn m2a_baseline_migration_preserves_exact_legacy_bytes_and_requires_project_lease() {
    use crate::project_mutation::ProjectMutationOwnerV1;
    use generations_v1::{C12GenerationOwnerV1, C12InspectionV1};
    for body in [None, Some(b"{ \"legacy\":true }\n".as_slice())] {
        let state = tempfile::tempdir().unwrap();
        let lock =
            ProjectMutationOwnerV1::at_store_root(&state.path().join("rewind-workspace")).unwrap();
        let lease = lock.try_mutation().unwrap();
        if let Some(body) = body {
            fs::create_dir(state.path().join("sync")).unwrap();
            fs::write(path(state.path()), body).unwrap();
        }
        let owner = C12GenerationOwnerV1::open(state.path(), &lease).unwrap();
        assert_eq!(
            owner.inspect().unwrap(),
            C12InspectionV1::Legacy {
                participation_hash: body.map(participation_bytes_hash),
            }
        );
        let base = owner
            .initialize_for_mutation(ParticipationPublicationV1::Write, Some(b"target"))
            .unwrap();
        assert_eq!(base.generation, 0);
        assert_eq!(base.participation_hash, body.map(participation_bytes_hash));
        assert_eq!(fs::read(path(state.path())).ok().as_deref(), body);
        assert_eq!(
            owner.inspect().unwrap(),
            C12InspectionV1::Committed {
                marker: base.clone(),
                pending: None
            }
        );
        assert!(
            owner
                .initialize_for_mutation(ParticipationPublicationV1::Write, Some(b"target"))
                .is_err()
        );
        let other = tempfile::tempdir().unwrap();
        let foreign =
            ProjectMutationOwnerV1::at_store_root(&other.path().join("rewind-workspace")).unwrap();
        assert!(
            C12GenerationOwnerV1::open(state.path(), &foreign.try_mutation().unwrap()).is_err()
        );
        drop(owner);
        let reopened = C12GenerationOwnerV1::open(state.path(), &lease).unwrap();
        assert_eq!(
            reopened.inspect().unwrap(),
            C12InspectionV1::Committed {
                marker: base,
                pending: None
            }
        );
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn m2a_immutable_chain_rejects_body_rollback_gap_fork_duplicate_and_id_reuse() {
    use crate::{
        management_effect::ManagementEffectHashV1 as Hash, project_mutation::ProjectMutationOwnerV1,
    };
    use generations_v1::{C12GenerationOwnerV1, C12InspectionV1};
    use std::os::unix::fs::PermissionsExt;
    for case in 0..6 {
        let state = tempfile::tempdir().unwrap();
        let lock =
            ProjectMutationOwnerV1::at_store_root(&state.path().join("rewind-workspace")).unwrap();
        let lease = lock.try_mutation().unwrap();
        let owner = C12GenerationOwnerV1::open(state.path(), &lease).unwrap();
        let base = owner
            .initialize_for_mutation(ParticipationPublicationV1::Write, Some(b"one"))
            .unwrap();
        let generations = state
            .path()
            .join("sync/participation-generations-v1/generations");
        let one = ParticipationGenerationV1 {
            schema_version: 1,
            generation: 1,
            mutation_id: Some(Hash::digest(b"one")),
            participation_hash: Some(participation_bytes_hash(b"one")),
            previous_marker_hash: Some(base.marker_hash().unwrap()),
            publication: ParticipationPublicationV1::Write,
        };
        let write_marker = |marker: &ParticipationGenerationV1| {
            let file = generations.join(marker.filename().unwrap());
            fs::write(&file, marker.canonical_bytes().unwrap()).unwrap();
            fs::set_permissions(file, fs::Permissions::from_mode(0o600)).unwrap();
        };
        write_marker(&one);
        fs::write(path(state.path()), b"one").unwrap();
        assert_eq!(
            owner.inspect().unwrap(),
            C12InspectionV1::Committed {
                marker: one.clone(),
                pending: None
            }
        );
        match case {
            0 => {
                fs::remove_file(path(state.path())).unwrap();
            }
            1 => {
                fs::remove_file(generations.join(one.filename().unwrap())).unwrap();
                let mut gap = one.clone();
                gap.generation = 2;
                write_marker(&gap);
            }
            2 => {
                let mut fork = one.clone();
                fork.generation = 2;
                fork.mutation_id = Some(Hash::digest(b"two"));
                fork.previous_marker_hash = Some(base.marker_hash().unwrap());
                write_marker(&fork);
            }
            3 => {
                let mut duplicate = one.clone();
                duplicate.mutation_id = Some(Hash::digest(b"other"));
                write_marker(&duplicate);
            }
            4 => {
                let mut reused = one.clone();
                reused.generation = 2;
                reused.previous_marker_hash = Some(one.marker_hash().unwrap());
                write_marker(&reused);
            }
            5 => {
                fs::write(generations.join("foreign"), b"foreign").unwrap();
            }
            _ => unreachable!(),
        }
        assert!(owner.inspect().is_err(), "case {case}");
        assert!(
            owner
                .initialize_for_mutation(ParticipationPublicationV1::Write, Some(b"two"))
                .is_err(),
            "case {case}"
        );
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn m2a_baseline_collision_partial_and_symlink_evidence_are_retained() {
    use crate::project_mutation::ProjectMutationOwnerV1;
    use generations_v1::C12GenerationOwnerV1;
    use std::os::unix::fs::{PermissionsExt, symlink};
    for case in 0..4 {
        let state = tempfile::tempdir().unwrap();
        let lock =
            ProjectMutationOwnerV1::at_store_root(&state.path().join("rewind-workspace")).unwrap();
        let lease = lock.try_mutation().unwrap();
        fs::create_dir(state.path().join("sync")).unwrap();
        let root = state.path().join("sync/participation-generations-v1");
        if case == 3 {
            let target = state.path().join("foreign");
            fs::create_dir(&target).unwrap();
            fs::write(target.join("held"), b"held").unwrap();
            symlink(target, &root).unwrap();
        } else {
            fs::create_dir(&root).unwrap();
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
            if case > 0 {
                fs::write(
                    root.join("baseline.v1.json"),
                    if case == 1 {
                        b"partial".as_slice()
                    } else {
                        b"{}".as_slice()
                    },
                )
                .unwrap();
            }
        }
        let owner = C12GenerationOwnerV1::open(state.path(), &lease).unwrap();
        assert!(owner.inspect().is_err());
        assert!(
            owner
                .initialize_for_mutation(ParticipationPublicationV1::Write, Some(b"new"))
                .is_err()
        );
        assert!(fs::symlink_metadata(&root).is_ok());
        if case == 3 {
            assert_eq!(fs::read(root.join("held")).unwrap(), b"held");
        }
        if case == 1 {
            assert_eq!(fs::read(root.join("baseline.v1.json")).unwrap(), b"partial");
        }
        assert!(!path(state.path()).exists());
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn m2a_tree_bytes(root: &std::path::Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    fn visit(
        root: &std::path::Path,
        current: &std::path::Path,
        rows: &mut Vec<(std::path::PathBuf, Vec<u8>)>,
    ) {
        use std::os::unix::fs::MetadataExt;
        let metadata = fs::symlink_metadata(current).unwrap();
        let mut value = serde_json::to_vec(&(
            metadata.is_dir(),
            metadata.uid(),
            metadata.gid(),
            metadata.mode(),
            metadata.nlink(),
        ))
        .unwrap();
        if !metadata.is_dir() {
            value.extend_from_slice(&fs::read(current).unwrap());
        }
        rows.push((current.strip_prefix(root).unwrap().to_path_buf(), value));
        if !metadata.is_dir() {
            return;
        }
        for entry in fs::read_dir(current).unwrap() {
            let path = entry.unwrap().path();
            visit(root, &path, rows);
        }
    }
    let mut rows = Vec::new();
    visit(root, root, &mut rows);
    rows.sort();
    rows
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn m2a_normal_entrypoints_use_shared_lease_and_unique_immutable_generations() {
    use crate::project_mutation::ProjectMutationOwnerV1;
    let state = p2a_private_state();
    let joined = join(
        state.path(),
        link("share-0001"),
        ShareManifest::default(),
        1000,
    )
    .unwrap();
    let after_join = m2a_tree_bytes(state.path());
    assert!(
        join(
            state.path(),
            link("share-0001"),
            ShareManifest::default(),
            2000
        )
        .is_err()
    );
    assert!(
        join(
            state.path(),
            link("share-0002"),
            ShareManifest::default(),
            1000
        )
        .is_err()
    );
    assert_eq!(m2a_tree_bytes(state.path()), after_join);
    let mut updated = joined.clone();
    updated.agreed_commit = Some("a".repeat(40));
    write(state.path(), &updated).unwrap();
    let updated_body = fs::read(path(state.path())).unwrap();
    assert_eq!(
        read(state.path()).unwrap(),
        ParticipationReadV2::LegacyParticipationOnly(updated.clone())
    );
    let lock =
        ProjectMutationOwnerV1::at_store_root(&state.path().join("rewind-workspace")).unwrap();
    let lease = lock.try_mutation().unwrap();
    let before = m2a_tree_bytes(state.path());
    assert!(write(state.path(), &joined).is_err());
    assert!(leave(state.path()).is_err());
    assert_eq!(m2a_tree_bytes(state.path()), before);
    drop(lease);
    assert!(leave(state.path()).unwrap());
    assert!(!leave(state.path()).unwrap());
    let mut records = fs::read_dir(
        state
            .path()
            .join("sync/participation-generations-v1/generations"),
    )
    .unwrap()
    .map(|entry| {
        ParticipationGenerationV1::from_canonical_bytes(&fs::read(entry.unwrap().path()).unwrap())
            .unwrap()
    })
    .collect::<Vec<_>>();
    records.sort_by_key(|r| r.generation);
    assert_eq!(records.len(), 4);
    assert_eq!(
        records.iter().map(|r| r.publication).collect::<Vec<_>>(),
        [
            ParticipationPublicationV1::Baseline,
            ParticipationPublicationV1::Join,
            ParticipationPublicationV1::Write,
            ParticipationPublicationV1::Leave,
        ]
    );
    let ids = records
        .iter()
        .filter_map(|r| r.mutation_id)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(ids.len(), 3);
    assert_eq!(
        records[2].participation_hash,
        Some(participation_bytes_hash(&updated_body))
    );
    assert!(records[3].participation_hash.is_none());
    // An old body never authorizes a fresh join when retained history is newer.
    fs::write(
        path(state.path()),
        serde_json::to_vec_pretty(&joined).unwrap(),
    )
    .unwrap();
    let rollback = m2a_tree_bytes(state.path());
    assert!(
        join(
            state.path(),
            link("share-0001"),
            ShareManifest::default(),
            1000
        )
        .is_err()
    );
    assert!(write(state.path(), &updated).is_err());
    assert!(leave(state.path()).is_err());
    assert_eq!(m2a_tree_bytes(state.path()), rollback);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn m2a_capacity_refuses_before_baseline_pending_and_mutation_id_publication() {
    use crate::project_mutation::ProjectMutationOwnerV1;
    use generations_v1::C12GenerationOwnerV1;
    for publication in [
        ParticipationPublicationV1::Join,
        ParticipationPublicationV1::Write,
        ParticipationPublicationV1::Leave,
    ] {
        for legacy in [false, true] {
            let state = tempfile::tempdir().unwrap();
            let lock =
                ProjectMutationOwnerV1::at_store_root(&state.path().join("rewind-workspace"))
                    .unwrap();
            let lease = lock.try_mutation().unwrap();
            let owner = C12GenerationOwnerV1::open(state.path(), &lease).unwrap();
            if legacy {
                fs::create_dir(state.path().join("sync")).unwrap();
                fs::write(path(state.path()), b"legacy").unwrap();
            }
            let base = ParticipationGenerationV1 {
                schema_version: 1,
                generation: 0,
                mutation_id: None,
                participation_hash: legacy.then(|| participation_bytes_hash(b"legacy")),
                previous_marker_hash: None,
                publication: ParticipationPublicationV1::Baseline,
            };
            let target =
                (publication != ParticipationPublicationV1::Leave).then_some(b"target".as_slice());
            let capacity =
                c12_mutation_capacity_upper_bound(&base, publication, target, 4096, true).unwrap();
            let threshold = capacity.required_bytes + crate::apply_intent::FREE_SPACE_FLOOR_BYTES;
            let before = m2a_tree_bytes(state.path());
            owner.set_capacity(Some((4096, threshold - 1)));
            assert!(matches!(
                owner.publish_normal(publication, target),
                Err(C12MutationErrorV1::ResourceUnavailable { .. })
            ));
            assert_eq!(m2a_tree_bytes(state.path()), before);
            assert!(
                !state
                    .path()
                    .join("sync/participation-generations-v1")
                    .exists()
            );
            owner.set_capacity(Some((4096, threshold)));
            let marker = owner.publish_normal(publication, target).unwrap();
            assert_eq!(marker.generation, 1);
            assert!(marker.mutation_id.is_some());
            assert_eq!(fs::read(path(state.path())).ok().as_deref(), target);
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn m2a_enospc_and_edquot_resume_same_pending_at_every_publication_and_cleanup_stage() {
    use crate::project_mutation::ProjectMutationOwnerV1;
    use generations_v1::{C12GenerationOwnerV1, C12InspectionV1, C12PendingPositionV1};
    for errno in [libc::ENOSPC, libc::EDQUOT] {
        for publication in [
            ParticipationPublicationV1::Write,
            ParticipationPublicationV1::Leave,
        ] {
            for stage in [
                "pending-control",
                "pending-target",
                "pending-publish",
                "body-temporary",
                "body-publish",
                "body-sync",
                "generation-temporary",
                "generation-publish",
                "generation-sync",
                "cleanup-body",
                "cleanup-operation",
                "cleanup-directory",
            ] {
                if publication == ParticipationPublicationV1::Leave
                    && matches!(stage, "pending-target" | "body-temporary" | "cleanup-body")
                {
                    continue;
                }
                let state = tempfile::tempdir().unwrap();
                let lock =
                    ProjectMutationOwnerV1::at_store_root(&state.path().join("rewind-workspace"))
                        .unwrap();
                let lease = lock.try_mutation().unwrap();
                fs::create_dir(state.path().join("sync")).unwrap();
                fs::write(path(state.path()), b"base").unwrap();
                let owner = C12GenerationOwnerV1::open(state.path(), &lease).unwrap();
                let target = (publication != ParticipationPublicationV1::Leave)
                    .then_some(b"target".as_slice());
                let base = owner.initialize_for_mutation(publication, target).unwrap();
                owner.inject_failure(stage, errno);
                assert!(
                    matches!(owner.publish_normal(publication, target), Err(C12MutationErrorV1::Io(error)) if error.raw_os_error() == Some(errno)),
                    "{publication:?}/{stage}/{errno}"
                );
                let inspection = owner.inspect().unwrap();
                let id = match &inspection {
                    C12InspectionV1::Committed {
                        marker,
                        pending: None,
                    } => {
                        assert!(stage.starts_with("pending-"));
                        assert_eq!(marker, &base);
                        assert_eq!(fs::read(path(state.path())).unwrap(), b"base");
                        None
                    }
                    C12InspectionV1::Committed {
                        marker,
                        pending: Some((pending, position)),
                    } => {
                        let expected = match stage {
                            "body-temporary" | "body-publish" => C12PendingPositionV1::Base,
                            "body-sync" | "generation-temporary" | "generation-publish" => {
                                C12PendingPositionV1::Target
                            }
                            _ => C12PendingPositionV1::Committed,
                        };
                        assert_eq!(position, &expected, "{stage}");
                        assert_eq!(
                            marker.generation,
                            if expected == C12PendingPositionV1::Committed {
                                1
                            } else {
                                0
                            }
                        );
                        Some(pending.target_marker.mutation_id.unwrap())
                    }
                    C12InspectionV1::CleanupCommitted { marker } => {
                        assert_eq!(stage, "cleanup-directory");
                        Some(marker.mutation_id.unwrap())
                    }
                    _ => panic!("unexpected state: {inspection:?}"),
                };
                drop(owner);
                let reopened = C12GenerationOwnerV1::open(state.path(), &lease).unwrap();
                let committed = if id.is_some() {
                    reopened.resume_pending().unwrap()
                } else {
                    reopened.publish_normal(publication, target).unwrap()
                };
                if let Some(id) = id {
                    assert_eq!(committed.mutation_id, Some(id));
                }
                assert_eq!(committed.generation, 1);
                assert_eq!(fs::read(path(state.path())).ok().as_deref(), target);
                assert_eq!(
                    reopened.inspect().unwrap(),
                    C12InspectionV1::Committed {
                        marker: committed,
                        pending: None
                    }
                );
            }
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn m2a_baseline_write_stops_retain_legacy_and_never_publish_a_partial_root() {
    use crate::project_mutation::ProjectMutationOwnerV1;
    use generations_v1::{C12GenerationOwnerV1, C12InspectionV1};
    for errno in [libc::ENOSPC, libc::EDQUOT] {
        for stage in ["baseline-marker", "baseline-record", "baseline-publish"] {
            let state = tempfile::tempdir().unwrap();
            let lock =
                ProjectMutationOwnerV1::at_store_root(&state.path().join("rewind-workspace"))
                    .unwrap();
            let lease = lock.try_mutation().unwrap();
            fs::create_dir(state.path().join("sync")).unwrap();
            fs::write(path(state.path()), b"legacy exact\n").unwrap();
            let owner = C12GenerationOwnerV1::open(state.path(), &lease).unwrap();
            owner.inject_failure(stage, errno);
            assert!(
                matches!(owner.publish_normal(ParticipationPublicationV1::Write, Some(b"target")), Err(C12MutationErrorV1::Io(error)) if error.raw_os_error() == Some(errno))
            );
            assert!(
                !state
                    .path()
                    .join("sync/participation-generations-v1")
                    .exists()
            );
            assert_eq!(fs::read(path(state.path())).unwrap(), b"legacy exact\n");
            assert_eq!(
                owner.inspect().unwrap(),
                C12InspectionV1::Legacy {
                    participation_hash: Some(participation_bytes_hash(b"legacy exact\n"))
                }
            );
            assert_eq!(
                owner
                    .publish_normal(ParticipationPublicationV1::Write, Some(b"target"))
                    .unwrap()
                    .generation,
                1
            );
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn m2a_pending_live_shortage_and_different_mutation_preserve_all_evidence() {
    use crate::project_mutation::ProjectMutationOwnerV1;
    use generations_v1::{C12GenerationOwnerV1, C12InspectionV1};
    let state = tempfile::tempdir().unwrap();
    let lock =
        ProjectMutationOwnerV1::at_store_root(&state.path().join("rewind-workspace")).unwrap();
    let lease = lock.try_mutation().unwrap();
    let owner = C12GenerationOwnerV1::open(state.path(), &lease).unwrap();
    owner.inject_failure("body-temporary", libc::ENOSPC);
    assert!(
        owner
            .publish_normal(ParticipationPublicationV1::Write, Some(b"target"))
            .is_err()
    );
    let C12InspectionV1::Committed {
        pending: Some((pending, _)),
        ..
    } = owner.inspect().unwrap()
    else {
        panic!("pending");
    };
    let before = m2a_tree_bytes(state.path());
    owner.set_capacity(Some((4096, 0)));
    assert!(matches!(
        owner.resume_pending(),
        Err(C12MutationErrorV1::ResourceUnavailable { .. })
    ));
    assert!(
        owner
            .publish_normal(ParticipationPublicationV1::Write, Some(b"different"))
            .is_err()
    );
    assert!(
        owner
            .publish_normal(ParticipationPublicationV1::Leave, None)
            .is_err()
    );
    assert_eq!(m2a_tree_bytes(state.path()), before);
    owner.set_capacity(None);
    assert_eq!(
        owner.resume_pending().unwrap().mutation_id,
        pending.target_marker.mutation_id
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn m2a_apply_uses_reserved_operation_id_and_rejects_reuse_before_pending() {
    use crate::{
        management_effect::ManagementEffectHashV1 as Hash, project_mutation::ProjectMutationOwnerV1,
    };
    use generations_v1::{C12GenerationOwnerV1, C12InspectionV1};
    let state = tempfile::tempdir().unwrap();
    let lock =
        ProjectMutationOwnerV1::at_store_root(&state.path().join("rewind-workspace")).unwrap();
    let lease = lock.try_mutation().unwrap();
    let owner = C12GenerationOwnerV1::open(state.path(), &lease).unwrap();
    let base = owner
        .initialize_for_mutation(ParticipationPublicationV1::Apply, Some(b"apply target"))
        .unwrap();
    let key = Hash::digest(b"reserved operation");
    let pending = owner
        .publish_apply_pending(&base, b"apply target", key)
        .unwrap();
    assert_eq!(pending.target_marker.mutation_id, Some(key));
    let before = m2a_tree_bytes(state.path());
    assert!(
        owner
            .publish_normal(ParticipationPublicationV1::Write, Some(b"apply target"))
            .is_err()
    );
    assert_eq!(m2a_tree_bytes(state.path()), before);
    let committed = owner.resume_pending().unwrap();
    assert_eq!(committed.mutation_id, Some(key));
    assert_eq!(
        owner.inspect().unwrap(),
        C12InspectionV1::Committed {
            marker: committed.clone(),
            pending: None
        }
    );
    let before = m2a_tree_bytes(state.path());
    assert!(
        owner
            .publish_apply_pending(&committed, b"different target", key)
            .is_err()
    );
    assert_eq!(m2a_tree_bytes(state.path()), before);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn m2a_pending_owned_partial_temporary_recovers_but_foreign_bytes_block() {
    use crate::project_mutation::ProjectMutationOwnerV1;
    use generations_v1::{C12GenerationOwnerV1, C12InspectionV1};
    for foreign in [false, true] {
        let state = tempfile::tempdir().unwrap();
        let lock =
            ProjectMutationOwnerV1::at_store_root(&state.path().join("rewind-workspace")).unwrap();
        let lease = lock.try_mutation().unwrap();
        let owner = C12GenerationOwnerV1::open(state.path(), &lease).unwrap();
        owner.inject_failure("generation-publish", libc::EDQUOT);
        assert!(
            owner
                .publish_normal(ParticipationPublicationV1::Write, Some(b"target"))
                .is_err()
        );
        let C12InspectionV1::Committed {
            pending: Some((pending, _)),
            ..
        } = owner.inspect().unwrap()
        else {
            panic!("pending");
        };
        let temporary = state
            .path()
            .join("sync/participation-generations-v1/generations")
            .join(format!(
                ".{}.tmp",
                pending.target_marker.filename().unwrap()
            ));
        let canonical = pending.target_marker.canonical_bytes().unwrap();
        fs::write(
            &temporary,
            if foreign {
                b"foreign".as_slice()
            } else {
                &canonical[..37]
            },
        )
        .unwrap();
        let before = m2a_tree_bytes(state.path());
        drop(owner);
        let owner = C12GenerationOwnerV1::open(state.path(), &lease).unwrap();
        if foreign {
            assert!(owner.resume_pending().is_err());
            assert_eq!(m2a_tree_bytes(state.path()), before);
        } else {
            assert_eq!(owner.resume_pending().unwrap(), pending.target_marker);
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn m2a_old_pending_reappearance_is_a_byte_invariant_block() {
    use crate::project_mutation::ProjectMutationOwnerV1;
    use generations_v1::{C12GenerationOwnerV1, C12InspectionV1};
    use std::os::unix::fs::PermissionsExt;
    let state = tempfile::tempdir().unwrap();
    let lock =
        ProjectMutationOwnerV1::at_store_root(&state.path().join("rewind-workspace")).unwrap();
    let lease = lock.try_mutation().unwrap();
    let owner = C12GenerationOwnerV1::open(state.path(), &lease).unwrap();
    owner.inject_failure("body-temporary", libc::ENOSPC);
    assert!(
        owner
            .publish_normal(ParticipationPublicationV1::Write, Some(b"one"))
            .is_err()
    );
    let C12InspectionV1::Committed {
        pending: Some((old, _)),
        ..
    } = owner.inspect().unwrap()
    else {
        panic!("pending");
    };
    owner.resume_pending().unwrap();
    let two = owner
        .publish_normal(ParticipationPublicationV1::Write, Some(b"two"))
        .unwrap();
    assert_eq!(two.generation, 2);
    let directory = state
        .path()
        .join("sync/participation-generations-v1/pending.v1");
    fs::create_dir(&directory).unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    for (name, bytes) in [
        ("operation.v1.json", old.canonical_bytes().unwrap()),
        ("target-body.v1.json", b"one".to_vec()),
    ] {
        let file = directory.join(name);
        fs::write(&file, bytes).unwrap();
        fs::set_permissions(file, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let before = m2a_tree_bytes(state.path());
    assert!(owner.inspect().is_err());
    assert!(owner.resume_pending().is_err());
    assert!(
        owner
            .publish_normal(ParticipationPublicationV1::Write, Some(b"three"))
            .is_err()
    );
    assert_eq!(m2a_tree_bytes(state.path()), before);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn m2a_body_partial_temporary_and_remaining_capacity_keep_the_same_pending_id() {
    use crate::project_mutation::ProjectMutationOwnerV1;
    use generations_v1::{C12GenerationOwnerV1, C12InspectionV1};
    for stage in ["body-publish", "generation-temporary"] {
        for foreign in [false, true] {
            let state = tempfile::tempdir().unwrap();
            let lock =
                ProjectMutationOwnerV1::at_store_root(&state.path().join("rewind-workspace"))
                    .unwrap();
            let lease = lock.try_mutation().unwrap();
            let owner = C12GenerationOwnerV1::open(state.path(), &lease).unwrap();
            owner.inject_failure(stage, libc::ENOSPC);
            assert!(
                owner
                    .publish_normal(ParticipationPublicationV1::Write, Some(b"target"))
                    .is_err()
            );
            let C12InspectionV1::Committed {
                marker,
                pending: Some((pending, _)),
            } = owner.inspect().unwrap()
            else {
                panic!("pending");
            };
            if stage == "body-publish" {
                let temporary = state.path().join("sync").join(format!(
                    ".participation-{}.tmp",
                    pending.target_marker.mutation_id.unwrap().as_hex()
                ));
                fs::write(
                    temporary,
                    if foreign {
                        b"foreign".as_slice()
                    } else {
                        b"tar".as_slice()
                    },
                )
                .unwrap();
            } else if foreign {
                continue;
            }
            let bound = c12_mutation_capacity_upper_bound(
                &marker,
                ParticipationPublicationV1::Write,
                Some(b"target"),
                4096,
                false,
            )
            .unwrap();
            let remaining = bound.generation_final_bytes
                + bound.generation_temporary_bytes
                + bound.directory_overhead_bytes
                + if stage == "body-publish" {
                    bound.body_publication_temporary_bytes
                } else {
                    0
                };
            let threshold = remaining + crate::apply_intent::FREE_SPACE_FLOOR_BYTES;
            let before = m2a_tree_bytes(state.path());
            owner.set_capacity(Some((4096, threshold - 1)));
            assert!(matches!(
                owner.resume_pending(),
                Err(C12MutationErrorV1::ResourceUnavailable { .. })
            ));
            assert_eq!(m2a_tree_bytes(state.path()), before);
            owner.set_capacity(Some((4096, threshold)));
            if foreign {
                assert!(owner.resume_pending().is_err());
                assert_eq!(m2a_tree_bytes(state.path()), before);
            } else {
                assert_eq!(
                    owner.resume_pending().unwrap().mutation_id,
                    pending.target_marker.mutation_id
                );
            }
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn m2a_zero_digest_fields_are_refused_in_generation_and_pending() {
    use crate::management_effect::ManagementEffectHashV1 as Hash;
    let zero = Hash::from_hex(&"0".repeat(64)).unwrap();
    let base = ParticipationGenerationV1::from_canonical_bytes(include_bytes!("../../../tests/fixtures/apply-intent-v1-capacity-positive/participation-base-marker.v1.json")).unwrap();
    let pending = C12PendingV1::from_canonical_bytes(include_bytes!(
        "../../../tests/fixtures/apply-intent-v1-capacity-positive/participation-pending.v1.json"
    ))
    .unwrap();
    for case in 0..3 {
        let mut changed = base.clone();
        match case {
            0 => changed.mutation_id = Some(zero),
            1 => changed.participation_hash = Some(zero),
            2 => changed.previous_marker_hash = Some(zero),
            _ => unreachable!(),
        }
        assert!(changed.canonical_bytes().is_err());
        assert!(
            ParticipationGenerationV1::from_canonical_bytes(&serde_json::to_vec(&changed).unwrap())
                .is_err()
        );
        let mut changed = pending.clone();
        match case {
            0 => changed.base_marker_hash = zero,
            1 => changed.target_marker_hash = zero,
            2 => changed.target_body_hash = Some(zero),
            _ => unreachable!(),
        }
        assert!(changed.canonical_bytes().is_err());
        assert!(
            C12PendingV1::from_canonical_bytes(&serde_json::to_vec(&changed).unwrap()).is_err()
        );
    }
    let mut baseline = base;
    baseline.generation = 0;
    baseline.mutation_id = None;
    baseline.previous_marker_hash = None;
    baseline.publication = ParticipationPublicationV1::Baseline;
    baseline.participation_hash = Some(zero);
    assert!(baseline.canonical_bytes().is_err());
    baseline.participation_hash = None;
    baseline.canonical_bytes().unwrap();
}

fn link(share: &str) -> ShareLink {
    ShareLink {
        remote: "ssh://git@example.invalid/sync.git".into(),
        share_id: share.into(),
        invitation_id: "invite-0001".into(),
        expires_unix_seconds: 2_000,
    }
}

#[test]
fn joining_records_the_share_without_a_credential_and_refuses_a_spent_invitation() {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let state = p2a_private_state();
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let state = tempfile::tempdir().unwrap();
    assert_eq!(read(state.path()).unwrap(), ParticipationReadV2::Missing);

    let participation = join(
        state.path(),
        link("share-0001"),
        ShareManifest::default(),
        1_000,
    )
    .unwrap();
    assert_eq!(
        read(state.path()).unwrap(),
        ParticipationReadV2::LegacyParticipationOnly(participation.clone())
    );
    assert_eq!(participation.agreed_commit, None);
    let body = fs::read_to_string(path(state.path())).unwrap();
    assert!(!body.contains("password") && !body.contains("token"));

    // An invitation that has run out cannot be used later.
    assert!(
        join(
            state.path(),
            link("share-0001"),
            ShareManifest::default(),
            2_000
        )
        .is_err()
    );
    // One project belongs to one share.
    assert!(
        join(
            state.path(),
            link("share-0002"),
            ShareManifest::default(),
            1_000
        )
        .is_err()
    );
    assert_eq!(
        read(state.path())
            .unwrap()
            .participation()
            .unwrap()
            .share
            .share_id,
        "share-0001"
    );

    assert!(leave(state.path()).unwrap());
    assert!(!leave(state.path()).unwrap());
    assert_eq!(read(state.path()).unwrap(), ParticipationReadV2::Missing);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn p2a_private_state() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let state = tempfile::tempdir().unwrap();
    fs::set_permissions(state.path(), fs::Permissions::from_mode(0o700)).unwrap();
    state
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn p2a_participation_v2() -> ParticipationV2 {
    use polaris_auth::git_credentials::{
        AuthenticationTypeV1, ExecutableIdentityV1, OptionalAttributeHashV1,
    };
    let remote =
        CanonicalHttpsRemoteV1::parse("https://example.invalid:443/team/project.git").unwrap();
    let executable = |path: &str| ExecutableIdentityV1 {
        path: PathBuf::from(path),
        device: 1,
        inode: 2,
        owner_uid: unsafe { libc::geteuid() },
        mode: 0o700,
        sha256: "a".repeat(64),
    };
    ParticipationV2 {
        schema_version: 2,
        participation: Participation {
            share: ShareLink {
                remote: remote.transport_url(),
                ..link("share-0001")
            },
            manifest: ShareManifest::default(),
            joined_unix_seconds: 1_000,
            agreed_commit: None,
            applied_commit: None,
            unresolved: Vec::new(),
        },
        credential_binding: GitCredentialBindingV1 {
            namespace: "macos-keychain.internet-password.v1".into(),
            credential_item_id: "1".repeat(64),
            credential_item_version: "2".repeat(64),
            remote,
            account: "selected-account".into(),
            authentication_type: AuthenticationTypeV1::HttpBasic,
            security_domain: OptionalAttributeHashV1::Absent,
            access_group: OptionalAttributeHashV1::Absent,
            synchronizable: false,
            generation: 1,
            git: executable("/disposable/fixture/git"),
            askpass: executable("/disposable/fixture/polaris-git-askpass"),
        },
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn p2a_store_bytes(state: &Path, bytes: &[u8]) {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(state.join("sync")).unwrap();
    fs::set_permissions(state.join("sync"), fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(path(state), bytes).unwrap();
    fs::set_permissions(path(state), fs::Permissions::from_mode(0o600)).unwrap();
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn p2a_strict_read_distinguishes_missing_legacy_stale_and_verified_current() {
    let state = p2a_private_state();
    assert_eq!(
        read_snapshot(state.path()).unwrap(),
        ParticipationSnapshotV2 {
            classification: ParticipationReadV2::Missing,
            exact_digest: None,
        }
    );
    let v2 = p2a_participation_v2();
    let legacy_bytes = serde_json::to_vec_pretty(v2.inner()).unwrap();
    p2a_store_bytes(state.path(), &legacy_bytes);
    assert_eq!(
        read_snapshot(state.path()).unwrap(),
        ParticipationSnapshotV2 {
            classification: ParticipationReadV2::LegacyParticipationOnly(v2.inner().clone()),
            exact_digest: Some(SyncDigestV1::digest(&legacy_bytes)),
        }
    );
    let bytes = v2.canonical_bytes().unwrap();
    p2a_store_bytes(state.path(), &bytes);
    assert_eq!(
        read(state.path()).unwrap(),
        ParticipationReadV2::StaleCredentialBindingOnly(v2.clone())
    );
    assert_eq!(
        read_with_binding_verifier(state.path(), |_| false)
            .unwrap()
            .classification,
        ParticipationReadV2::StaleCredentialBindingOnly(v2.clone())
    );
    assert_eq!(
        read_with_binding_verifier(state.path(), |binding| binding == &v2.credential_binding)
            .unwrap(),
        ParticipationSnapshotV2 {
            classification: ParticipationReadV2::Current(v2),
            exact_digest: Some(SyncDigestV1::digest(&bytes))
        }
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn p2a_wire_uses_typed_endpoint_and_roundtrips_all_participation_fields() {
    let mut v2 = p2a_participation_v2();
    v2.participation.agreed_commit = Some("a".repeat(40));
    v2.participation.applied_commit = Some("b".repeat(40));
    v2.participation.unresolved = vec!["src/main.rs".into()];
    let bytes = v2.canonical_bytes().unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("https://"));
    let object: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(object["participation"]["share"]["remote"].is_object());
    assert_eq!(
        serde_json::from_slice::<ParticipationV2>(&bytes).unwrap(),
        v2
    );
    let debug = format!("{v2:?}");
    for omitted in [
        "example.invalid",
        "team/project.git",
        "selected-account",
        &v2.participation.share.remote,
    ] {
        assert!(!debug.contains(omitted));
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn p2a_corrupt_schema_or_secret_never_becomes_missing_legacy_or_stale() {
    let state = p2a_private_state();
    let v2 = p2a_participation_v2();
    let clean = serde_json::to_value(&v2).unwrap();
    let changes: Vec<(&str, serde_json::Value)> = vec![
        ("schema_version", serde_json::json!(1)),
        ("unknown", serde_json::json!("secret")),
    ];
    for (field, value) in changes {
        let mut changed = clean.clone();
        changed[field] = value;
        p2a_store_bytes(state.path(), &serde_json::to_vec(&changed).unwrap());
        assert!(
            read_with_binding_verifier(state.path(), |_| panic!(
                "structural corruption must precede runtime lookup"
            ))
            .is_err()
        );
    }
    for (field, value) in [
        (
            "credential_item_id",
            serde_json::json!("raw-persistent-reference"),
        ),
        ("credential_item_version", serde_json::json!("raw-secret")),
        ("generation", serde_json::json!(0)),
        ("synchronizable", serde_json::json!(true)),
        ("password", serde_json::json!("raw-secret")),
    ] {
        let mut changed = clean.clone();
        changed["credential_binding"][field] = value;
        p2a_store_bytes(state.path(), &serde_json::to_vec(&changed).unwrap());
        assert!(
            read_with_binding_verifier(state.path(), |_| panic!("invalid stored binding")).is_err()
        );
    }
    for remote in [
        "https://user:SECRET@example.invalid/x.git",
        "https://user@example.invalid/x.git",
        "https://example.invalid/x.git?token=SECRET",
        "ssh://git:SECRET@example.invalid/x.git",
        "unknown://example.invalid/x.git",
    ] {
        let mut legacy = v2.inner().clone();
        legacy.share.remote = remote.into();
        p2a_store_bytes(state.path(), &serde_json::to_vec(&legacy).unwrap());
        let error = read(state.path()).unwrap_err();
        assert!(!format!("{error:?} {error}").contains("SECRET"));
    }
    for bytes in [b"{".as_slice(), b"null", b"[]", b"{}", b"{} {}"] {
        p2a_store_bytes(state.path(), bytes);
        assert!(read(state.path()).is_err());
    }
    let mut unknown_share = v2.inner().clone();
    unknown_share.share.share_id = "short".into();
    p2a_store_bytes(state.path(), &serde_json::to_vec(&unknown_share).unwrap());
    assert!(read(state.path()).is_err());
    let duplicate = b"{\"schema_version\":2,\"schema_version\":2}";
    p2a_store_bytes(state.path(), duplicate);
    assert!(read(state.path()).is_err());
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn p2a_strict_read_enforces_size_mode_single_link_and_nofollow() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let state = p2a_private_state();
    let legacy = p2a_participation_v2().participation;
    let mut body = serde_json::to_vec(&legacy).unwrap();
    body.resize(MAX_PARTICIPATION_BYTES_V2, b' ');
    p2a_store_bytes(state.path(), &body);
    assert!(matches!(
        read(state.path()).unwrap(),
        ParticipationReadV2::LegacyParticipationOnly(_)
    ));
    body.push(b' ');
    p2a_store_bytes(state.path(), &body);
    assert!(read(state.path()).is_err());
    p2a_store_bytes(state.path(), &serde_json::to_vec(&legacy).unwrap());
    fs::set_permissions(path(state.path()), fs::Permissions::from_mode(0o644)).unwrap();
    assert!(read(state.path()).is_err());
    fs::set_permissions(path(state.path()), fs::Permissions::from_mode(0o600)).unwrap();
    let hardlink = state.path().join("held");
    fs::hard_link(path(state.path()), &hardlink).unwrap();
    assert!(read(state.path()).is_err());
    fs::remove_file(path(state.path())).unwrap();
    symlink(&hardlink, path(state.path())).unwrap();
    assert!(read(state.path()).is_err());
    fs::remove_file(path(state.path())).unwrap();
    fs::create_dir(path(state.path())).unwrap();
    assert!(read(state.path()).is_err());
    let other = p2a_private_state();
    let linked_state = p2a_private_state();
    symlink(other.path(), linked_state.path().join("sync")).unwrap();
    assert!(read(linked_state.path()).is_err());
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn p2a_legacy_policy_is_classified_without_rewriting_or_runtime_lookup() {
    let state = p2a_private_state();
    let mut legacy = p2a_participation_v2().participation;
    for remote in [
        "ssh://git@example.invalid/project.git",
        "git@example.invalid:project.git",
        "file:///tmp/project.git",
    ] {
        legacy.share.remote = remote.into();
        // Expiry does not erase the invitation accepted at the recorded join.
        let bytes = serde_json::to_vec_pretty(&legacy).unwrap();
        p2a_store_bytes(state.path(), &bytes);
        let snapshot =
            read_with_binding_verifier(state.path(), |_| panic!("legacy has no runtime binding"))
                .unwrap();
        assert_eq!(
            snapshot.classification,
            ParticipationReadV2::LegacyParticipationOnly(legacy.clone())
        );
        assert_eq!(fs::read(path(state.path())).unwrap(), bytes);
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn p2a_cas_rejects_stale_digest_foreign_lease_and_corruption_before_writes() {
    use crate::project_mutation::ProjectMutationOwnerV1;
    let state = p2a_private_state();
    let next = p2a_participation_v2();
    let bytes = serde_json::to_vec_pretty(next.inner()).unwrap();
    p2a_store_bytes(state.path(), &bytes);
    let lock =
        ProjectMutationOwnerV1::at_store_root(&state.path().join("rewind-workspace")).unwrap();
    let lease = lock.try_mutation().unwrap();
    let before = m2a_tree_bytes(state.path());
    assert!(matches!(
        publish_v2(state.path(), &lease, None, &next),
        Err(ParticipationErrorV2::StaleExpectedDigest)
    ));
    assert!(matches!(
        publish_v2(
            state.path(),
            &lease,
            Some(SyncDigestV1::digest(b"different")),
            &next
        ),
        Err(ParticipationErrorV2::StaleExpectedDigest)
    ));
    assert_eq!(m2a_tree_bytes(state.path()), before);
    let foreign = p2a_private_state();
    let foreign_lock =
        ProjectMutationOwnerV1::at_store_root(&foreign.path().join("rewind-workspace")).unwrap();
    let foreign_lease = foreign_lock.try_mutation().unwrap();
    assert!(
        publish_v2(
            state.path(),
            &foreign_lease,
            Some(SyncDigestV1::digest(&bytes)),
            &next
        )
        .is_err()
    );
    assert_eq!(m2a_tree_bytes(state.path()), before);
    p2a_store_bytes(state.path(), b"{}");
    let corrupt_before = m2a_tree_bytes(state.path());
    assert!(
        publish_v2(
            state.path(),
            &lease,
            Some(SyncDigestV1::digest(b"{}")),
            &next
        )
        .is_err()
    );
    assert_eq!(m2a_tree_bytes(state.path()), corrupt_before);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn p2a_cas_preserves_c12_receipts_and_v2_binding_through_historical_writes() {
    use crate::project_mutation::ProjectMutationOwnerV1;
    let state = p2a_private_state();
    let next = p2a_participation_v2();
    let joined = join(
        state.path(),
        next.inner().share.clone(),
        ShareManifest::default(),
        1_000,
    )
    .unwrap();
    let prior = read_snapshot(state.path()).unwrap().exact_digest;
    let lock =
        ProjectMutationOwnerV1::at_store_root(&state.path().join("rewind-workspace")).unwrap();
    let lease = lock.try_mutation().unwrap();
    let digest = publish_v2(state.path(), &lease, prior, &next).unwrap();
    assert_eq!(
        digest,
        SyncDigestV1::digest(&next.canonical_bytes().unwrap())
    );
    let owner = C12GenerationOwnerV1::open(state.path(), &lease).unwrap();
    assert!(
        matches!(owner.inspect().unwrap(), C12InspectionV1::Committed { marker, pending: None }
        if marker.generation == 2 && marker.publication == ParticipationPublicationV1::Write)
    );
    drop(lease);
    let mut changed = joined;
    changed.agreed_commit = Some("a".repeat(40));
    write(state.path(), &changed).unwrap();
    let held = read(state.path()).unwrap();
    assert!(
        matches!(held, ParticipationReadV2::StaleCredentialBindingOnly(value)
        if value.participation == changed && value.credential_binding == next.credential_binding)
    );
    assert!(leave(state.path()).unwrap());
    assert_eq!(read(state.path()).unwrap(), ParticipationReadV2::Missing);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn p2a_cas_fault_keeps_exact_old_body_and_resumes_existing_c12_pending() {
    use crate::project_mutation::ProjectMutationOwnerV1;
    let state = p2a_private_state();
    let next = p2a_participation_v2();
    let bytes = serde_json::to_vec_pretty(next.inner()).unwrap();
    p2a_store_bytes(state.path(), &bytes);
    let lock =
        ProjectMutationOwnerV1::at_store_root(&state.path().join("rewind-workspace")).unwrap();
    let lease = lock.try_mutation().unwrap();
    let owner = C12GenerationOwnerV1::open(state.path(), &lease).unwrap();
    owner.inject_failure("body-publish", libc::EIO);
    let expected = Some(SyncDigestV1::digest(&bytes));
    assert!(publish_v2_under_owner(&owner, expected, &next).is_err());
    assert_eq!(fs::read(path(state.path())).unwrap(), bytes);
    assert!(matches!(
        owner.inspect().unwrap(),
        C12InspectionV1::Committed {
            pending: Some(_),
            ..
        }
    ));
    assert_eq!(
        publish_v2_under_owner(&owner, expected, &next).unwrap(),
        SyncDigestV1::digest(&next.canonical_bytes().unwrap())
    );
    assert!(matches!(
        owner.inspect().unwrap(),
        C12InspectionV1::Committed { pending: None, .. }
    ));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn p2a_c12_target_retains_schema_and_binding_and_rejects_identity_changes() {
    let original = p2a_participation_v2();
    let source = original.canonical_bytes().unwrap();
    let mut next = original.inner().clone();
    next.agreed_commit = Some("a".repeat(40));
    next.applied_commit = Some("b".repeat(40));
    next.unresolved = vec!["src/conflict.rs".into()];
    let target = updated_participation_bytes_v2(&source, &next).unwrap();
    let held: ParticipationV2 = serde_json::from_slice(&target).unwrap();
    assert_eq!(held.schema_version, 2);
    assert_eq!(held.participation, next);
    assert_eq!(held.credential_binding, original.credential_binding);
    assert!(matches!(
        decode_participation_v2(&target).unwrap(),
        ParticipationReadV2::StaleCredentialBindingOnly(_)
    ));

    let mut other = next.clone();
    other.share.share_id = "share-0002".into();
    assert!(updated_participation_bytes_v2(&source, &other).is_err());
    other = next.clone();
    other.share.remote = "https://other.invalid:443/team/project.git".into();
    assert!(updated_participation_bytes_v2(&source, &other).is_err());
    let mut foreign_binding: serde_json::Value = serde_json::from_slice(&source).unwrap();
    foreign_binding["credential_binding"]["remote"]["host"] = serde_json::json!("other.invalid");
    assert!(
        updated_participation_bytes_v2(&serde_json::to_vec(&foreign_binding).unwrap(), &next)
            .is_err()
    );
    assert!(updated_participation_bytes_v2(b"{}", &next).is_err());
    let mut oversized = next.clone();
    oversized.unresolved = vec!["a".repeat(4096); 33];
    assert!(updated_participation_bytes_v2(&source, &oversized).is_err());

    let legacy_source = serde_json::to_vec_pretty(original.inner()).unwrap();
    let legacy_target = updated_participation_bytes_v2(&legacy_source, &next).unwrap();
    assert_eq!(
        decode_participation_v2(&legacy_target).unwrap(),
        ParticipationReadV2::LegacyParticipationOnly(next)
    );
    assert_eq!(
        SyncDigestV1::digest(&source),
        SyncDigestV1::digest(&original.canonical_bytes().unwrap())
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn p2a_strict_generation_read_rejects_lost_bodies_and_false_empty_state() {
    use crate::project_mutation::ProjectMutationOwnerV1;
    for publication in [
        ParticipationPublicationV1::Write,
        ParticipationPublicationV1::Apply,
    ] {
        let state = p2a_private_state();
        let joined = join(
            state.path(),
            link("share-0001"),
            ShareManifest::default(),
            1_000,
        )
        .unwrap();
        let lock =
            ProjectMutationOwnerV1::at_store_root(&state.path().join("rewind-workspace")).unwrap();
        let lease = lock.try_mutation().unwrap();
        let owner = C12GenerationOwnerV1::open(state.path(), &lease).unwrap();
        let mut changed = joined;
        changed.agreed_commit = Some("a".repeat(40));
        let target = serde_json::to_vec(&changed).unwrap();
        if publication == ParticipationPublicationV1::Write {
            owner.publish_normal(publication, Some(&target)).unwrap();
        } else {
            let C12InspectionV1::Committed {
                marker,
                pending: None,
            } = owner.inspect().unwrap()
            else {
                panic!("committed join");
            };
            owner
                .publish_apply_pending(
                    &marker,
                    &target,
                    crate::management_effect::ManagementEffectHashV1::digest(b"disposable apply"),
                )
                .unwrap();
            owner.resume_pending().unwrap();
        }
        assert!(read(state.path()).is_ok());
        fs::remove_file(path(state.path())).unwrap();
        let before = m2a_tree_bytes(state.path());
        assert!(
            read_snapshot(state.path()).is_err(),
            "{publication:?} cannot become Missing after body loss"
        );
        assert_eq!(
            m2a_tree_bytes(state.path()),
            before,
            "diagnostic inspection never repairs"
        );
    }

    let state = p2a_private_state();
    assert_eq!(read(state.path()).unwrap(), ParticipationReadV2::Missing);
    let joined = join(
        state.path(),
        link("share-0001"),
        ShareManifest::default(),
        1_000,
    )
    .unwrap();
    leave(state.path()).unwrap();
    assert_eq!(read(state.path()).unwrap(), ParticipationReadV2::Missing);
    p2a_store_bytes(state.path(), &serde_json::to_vec(&joined).unwrap());
    let before = m2a_tree_bytes(state.path());
    assert!(
        read_snapshot(state.path()).is_err(),
        "a Leave marker cannot admit an unexpected body"
    );
    assert_eq!(m2a_tree_bytes(state.path()), before);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn p2a_cas_retries_each_post_rename_stop_only_for_exact_source_and_target() {
    use crate::project_mutation::ProjectMutationOwnerV1;
    for had_source in [false, true] {
        for stage in [
            "body-sync",
            "generation-temporary",
            "generation-publish",
            "generation-sync",
            "cleanup-body",
            "cleanup-operation",
        ] {
            let state = p2a_private_state();
            let next = p2a_participation_v2();
            let old = serde_json::to_vec_pretty(next.inner()).unwrap();
            if had_source {
                p2a_store_bytes(state.path(), &old);
            }
            let expected = had_source.then(|| SyncDigestV1::digest(&old));
            let lock =
                ProjectMutationOwnerV1::at_store_root(&state.path().join("rewind-workspace"))
                    .unwrap();
            let lease = lock.try_mutation().unwrap();
            let owner = C12GenerationOwnerV1::open(state.path(), &lease).unwrap();
            owner.inject_failure(stage, libc::EIO);
            assert!(
                publish_v2_under_owner(&owner, expected, &next).is_err(),
                "{stage}"
            );
            assert_eq!(
                fs::read(path(state.path())).unwrap(),
                next.canonical_bytes().unwrap()
            );
            let pending_before = m2a_tree_bytes(state.path());
            assert!(
                read_snapshot(state.path()).is_err(),
                "partial publication blocks normal reads: {stage}"
            );
            assert_eq!(m2a_tree_bytes(state.path()), pending_before);
            assert!(
                publish_v2_under_owner(&owner, Some(SyncDigestV1::digest(b"other source")), &next)
                    .is_err()
            );
            let mut other_target = next.clone();
            other_target.participation.agreed_commit = Some("a".repeat(40));
            assert!(publish_v2_under_owner(&owner, expected, &other_target).is_err());
            assert_eq!(
                m2a_tree_bytes(state.path()),
                pending_before,
                "mismatch never resumes or creates evidence"
            );
            assert_eq!(
                publish_v2_under_owner(&owner, expected, &next).unwrap(),
                SyncDigestV1::digest(&next.canonical_bytes().unwrap())
            );
            assert!(matches!(
                owner.inspect().unwrap(),
                C12InspectionV1::Committed { pending: None, .. }
            ));
            assert!(
                matches!(read(state.path()).unwrap(), ParticipationReadV2::StaleCredentialBindingOnly(value) if value == next)
            );
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn p2a_cas_never_promotes_an_old_pending_without_explicit_cas_source() {
    use crate::project_mutation::ProjectMutationOwnerV1;
    for had_source in [false, true] {
        let state = p2a_private_state();
        let next = p2a_participation_v2();
        let old = serde_json::to_vec_pretty(next.inner()).unwrap();
        if had_source {
            p2a_store_bytes(state.path(), &old);
        }
        let expected = had_source.then(|| SyncDigestV1::digest(&old));
        let lock =
            ProjectMutationOwnerV1::at_store_root(&state.path().join("rewind-workspace")).unwrap();
        let lease = lock.try_mutation().unwrap();
        let owner = C12GenerationOwnerV1::open(state.path(), &lease).unwrap();
        owner.inject_failure("body-sync", libc::EIO);
        assert!(
            owner
                .publish_normal(
                    ParticipationPublicationV1::Write,
                    Some(&next.canonical_bytes().unwrap())
                )
                .is_err()
        );
        let C12InspectionV1::Committed {
            pending: Some((pending, _)),
            ..
        } = owner.inspect().unwrap()
        else {
            panic!("pending write");
        };
        assert_eq!(pending.cas_source, None);
        let before = m2a_tree_bytes(state.path());
        assert!(publish_v2_under_owner(&owner, expected, &next).is_err());
        assert_eq!(m2a_tree_bytes(state.path()), before);
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn p2a_legacy_join_and_first_write_enforce_serialized_body_limit_before_changes() {
    use crate::project_mutation::ProjectMutationOwnerV1;
    let state = p2a_private_state();
    let lock =
        ProjectMutationOwnerV1::at_store_root(&state.path().join("rewind-workspace")).unwrap();
    let lease = lock.try_mutation().unwrap();
    let owner = C12GenerationOwnerV1::open(state.path(), &lease).unwrap();
    let mut oversized = p2a_participation_v2().participation;
    oversized.manifest.excluded = vec!["a".repeat(255); 550];
    let before = m2a_tree_bytes(state.path());
    assert!(join_under_owner(&owner, oversized.clone()).is_err());
    assert!(write_under_owner(&owner, &oversized).is_err());
    assert_eq!(m2a_tree_bytes(state.path()), before);
    assert_eq!(read(state.path()).unwrap(), ParticipationReadV2::Missing);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn normal_join_does_not_publish_past_orphan_apply_staging() {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let state = p2a_private_state();
    let lock = crate::project_mutation::ProjectMutationOwnerV1::at_store_root(
        &state.path().join("rewind-workspace"),
    )
    .unwrap();
    drop(lock);
    let root = state.path().join("apply-intents-v1");
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    for name in ["active", "archive", "staging"] {
        let directory = root.join(name);
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
    }
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(root.join("staging/interrupted"))
        .unwrap();
    let before = m2a_tree_bytes(state.path());
    assert!(
        join(
            state.path(),
            link("share-0001"),
            ShareManifest::default(),
            1_000
        )
        .is_err()
    );
    assert_eq!(before, m2a_tree_bytes(state.path()));
    assert!(matches!(
        read(state.path()).unwrap(),
        ParticipationReadV2::Missing
    ));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn p3_c10_cleanup_fixture() -> (
    tempfile::TempDir,
    crate::project_mutation::ProjectMutationOwnerV1,
    ParticipationV2,
    SyncDigestV1,
) {
    use crate::project_mutation::ProjectMutationOwnerV1;
    let state = p2a_private_state();
    let prior = p2a_participation_v2();
    let prior_bytes = prior.canonical_bytes().unwrap();
    let expected = SyncDigestV1::digest(&prior_bytes);
    p2a_store_bytes(state.path(), &prior_bytes);
    let mut target = prior;
    target.participation.agreed_commit = Some("a".repeat(40));
    let lock =
        ProjectMutationOwnerV1::at_store_root(&state.path().join("rewind-workspace")).unwrap();
    {
        let lease = lock.try_mutation().unwrap();
        let owner = C12GenerationOwnerV1::open(state.path(), &lease).unwrap();
        owner.inject_failure("cleanup-directory", libc::EIO);
        assert!(matches!(
            publish_v2_under_owner(&owner, Some(expected), &target),
            Err(ParticipationErrorV2::Publication(C12MutationErrorV1::Io(error)))
                if error.raw_os_error() == Some(libc::EIO)
        ));
        assert!(matches!(
            owner.inspect().unwrap(),
            C12InspectionV1::CleanupCommitted { .. }
        ));
    }
    (state, lock, target, expected)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn p3_c10_completed_cleanup_reads_exact_target_and_only_removes_empty_directory() {
    use std::os::unix::fs::MetadataExt;
    let (state, lock, target, expected) = p3_c10_cleanup_fixture();
    let lease = lock.try_mutation().unwrap();
    let owner = C12GenerationOwnerV1::open(state.path(), &lease).unwrap();
    let bytes = target.canonical_bytes().unwrap();
    let digest = SyncDigestV1::digest(&bytes);
    let before = m2a_tree_bytes(state.path());
    let file_ids: Vec<_> = before
        .iter()
        .filter_map(|(name, _)| {
            let metadata = fs::symlink_metadata(state.path().join(name)).unwrap();
            metadata
                .is_file()
                .then(|| (name.clone(), metadata.dev(), metadata.ino()))
        })
        .collect();
    let (marker, actual) = owner
        .inspect_c10_completed_cleanup(&target, digest)
        .unwrap();
    assert_eq!(marker.generation, 1);
    assert_eq!(marker.publication, ParticipationPublicationV1::Write);
    assert_eq!(actual, bytes);
    assert_eq!(
        m2a_tree_bytes(state.path()),
        before,
        "inspection is read only"
    );
    assert!(matches!(
        owner.inspect_exact_v2_cas(Some(expected), &bytes),
        Err(C12MutationErrorV1::CorruptEvidence(
            "CAS source proof retired"
        ))
    ));
    assert!(inspect_rebind_publication(state.path(), &lease, Some(expected), &target).is_err());
    assert!(publish_v2_under_owner(&owner, Some(expected), &target).is_err());
    assert_eq!(
        m2a_tree_bytes(state.path()),
        before,
        "generic CAS stays strict"
    );
    owner.cleanup_c10_completed_target(&target, digest).unwrap();
    assert!(
        !state
            .path()
            .join("sync/participation-generations-v1/pending.v1")
            .exists()
    );
    assert_eq!(
        owner.inspect().unwrap(),
        C12InspectionV1::Committed {
            marker,
            pending: None
        }
    );
    let snapshot = read_snapshot(state.path()).unwrap();
    assert_eq!(snapshot.exact_digest, Some(digest));
    assert_eq!(
        snapshot.classification,
        ParticipationReadV2::StaleCredentialBindingOnly(target)
    );
    for (name, device, inode) in file_ids {
        let metadata = fs::symlink_metadata(state.path().join(&name)).unwrap();
        assert_eq!(
            (metadata.dev(), metadata.ino()),
            (device, inode),
            "{}",
            name.display()
        );
        let old = &before
            .iter()
            .find(|(old_name, _)| old_name == &name)
            .unwrap()
            .1;
        let after = m2a_tree_bytes(state.path());
        assert_eq!(
            &after
                .iter()
                .find(|(new_name, _)| new_name == &name)
                .unwrap()
                .1,
            old
        );
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn p3_c10_completed_cleanup_rejects_changed_full_binding_target_and_digest_without_writes() {
    let (state, lock, target, _) = p3_c10_cleanup_fixture();
    let lease = lock.try_mutation().unwrap();
    let owner = C12GenerationOwnerV1::open(state.path(), &lease).unwrap();
    let before = m2a_tree_bytes(state.path());
    for case in 0..6 {
        let mut changed = target.clone();
        match case {
            0 => changed.participation.agreed_commit = Some("b".repeat(40)),
            1 => changed.credential_binding.generation += 1,
            2 => changed.credential_binding.credential_item_version = "3".repeat(64),
            3 => changed.credential_binding.account = "other-account".into(),
            4 => changed.schema_version = 1,
            5 => {}
            _ => unreachable!(),
        }
        let digest = if case == 5 {
            SyncDigestV1::digest(b"different receipt target")
        } else {
            changed
                .canonical_bytes()
                .map(|bytes| SyncDigestV1::digest(&bytes))
                .unwrap_or_else(|_| SyncDigestV1::digest(b"invalid target"))
        };
        assert!(
            owner
                .inspect_c10_completed_cleanup(&changed, digest)
                .is_err(),
            "case {case}"
        );
        assert!(
            owner
                .cleanup_c10_completed_target(&changed, digest)
                .is_err(),
            "case {case}"
        );
        assert_eq!(m2a_tree_bytes(state.path()), before, "case {case}");
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn p3_c10_completed_cleanup_requires_write_marker_and_complete_generation_chain() {
    for case in 0..4 {
        let (state, lock, target, _) = p3_c10_cleanup_fixture();
        let lease = lock.try_mutation().unwrap();
        let owner = C12GenerationOwnerV1::open(state.path(), &lease).unwrap();
        let C12InspectionV1::CleanupCommitted { mut marker } = owner.inspect().unwrap() else {
            panic!("cleanup fixture");
        };
        let marker_path = state
            .path()
            .join("sync/participation-generations-v1/generations")
            .join(marker.filename().unwrap());
        match case {
            0 => {
                marker.publication = ParticipationPublicationV1::Join;
                fs::write(&marker_path, marker.canonical_bytes().unwrap()).unwrap();
                assert!(matches!(
                    owner.inspect().unwrap(),
                    C12InspectionV1::CleanupCommitted { .. }
                ));
            }
            1 => {
                marker.publication = ParticipationPublicationV1::Apply;
                fs::write(&marker_path, marker.canonical_bytes().unwrap()).unwrap();
                assert!(matches!(
                    owner.inspect().unwrap(),
                    C12InspectionV1::CleanupCommitted { .. }
                ));
            }
            2 => {
                marker.previous_marker_hash = Some(participation_bytes_hash(b"foreign previous"));
                fs::write(&marker_path, marker.canonical_bytes().unwrap()).unwrap();
            }
            3 => {
                fs::remove_file(&marker_path).unwrap();
            }
            _ => unreachable!(),
        }
        let before = m2a_tree_bytes(state.path());
        let digest = SyncDigestV1::digest(&target.canonical_bytes().unwrap());
        assert!(
            owner
                .inspect_c10_completed_cleanup(&target, digest)
                .is_err(),
            "case {case}"
        );
        assert!(
            owner.cleanup_c10_completed_target(&target, digest).is_err(),
            "case {case}"
        );
        assert_eq!(m2a_tree_bytes(state.path()), before, "case {case}");
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn p3_c10_completed_cleanup_rejects_private_file_and_foreign_inventory_violations() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    for case in 0..7 {
        let (state, lock, target, _) = p3_c10_cleanup_fixture();
        let root = state.path().join("sync/participation-generations-v1");
        match case {
            0 => {
                fs::set_permissions(path(state.path()), fs::Permissions::from_mode(0o644)).unwrap()
            }
            1 => fs::hard_link(path(state.path()), state.path().join("duplicate-body")).unwrap(),
            2 => {
                let body = fs::read(path(state.path())).unwrap();
                fs::write(state.path().join("symlink-body"), body).unwrap();
                fs::remove_file(path(state.path())).unwrap();
                symlink(state.path().join("symlink-body"), path(state.path())).unwrap();
            }
            3 => fs::write(root.join("pending.v1/foreign"), b"foreign").unwrap(),
            4 => fs::write(root.join("foreign"), b"foreign").unwrap(),
            5 => fs::set_permissions(root.join("pending.v1"), fs::Permissions::from_mode(0o755))
                .unwrap(),
            6 => fs::write(root.join("generations/.foreign.tmp"), b"foreign").unwrap(),
            _ => unreachable!(),
        }
        let lease = lock.try_mutation().unwrap();
        let owner = C12GenerationOwnerV1::open(state.path(), &lease).unwrap();
        let before = m2a_tree_bytes(state.path());
        let digest = SyncDigestV1::digest(&target.canonical_bytes().unwrap());
        assert!(
            owner
                .inspect_c10_completed_cleanup(&target, digest)
                .is_err(),
            "case {case}"
        );
        assert!(
            owner.cleanup_c10_completed_target(&target, digest).is_err(),
            "case {case}"
        );
        assert_eq!(m2a_tree_bytes(state.path()), before, "case {case}");
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn p3_c10_completed_cleanup_rechecks_pending_before_deletion() {
    let (state, lock, target, _) = p3_c10_cleanup_fixture();
    let lease = lock.try_mutation().unwrap();
    let owner = C12GenerationOwnerV1::open(state.path(), &lease).unwrap();
    let digest = SyncDigestV1::digest(&target.canonical_bytes().unwrap());
    owner
        .inspect_c10_completed_cleanup(&target, digest)
        .unwrap();
    fs::write(
        state
            .path()
            .join("sync/participation-generations-v1/pending.v1/operation.v1.json"),
        b"foreign operation",
    )
    .unwrap();
    let before = m2a_tree_bytes(state.path());
    assert!(owner.cleanup_c10_completed_target(&target, digest).is_err());
    assert_eq!(m2a_tree_bytes(state.path()), before);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn p3_c10_completed_cleanup_never_admits_live_legacy_pending_or_legacy_body() {
    use crate::project_mutation::ProjectMutationOwnerV1;
    let state = p2a_private_state();
    let target = p2a_participation_v2();
    let bytes = target.canonical_bytes().unwrap();
    let digest = SyncDigestV1::digest(&bytes);
    let lock =
        ProjectMutationOwnerV1::at_store_root(&state.path().join("rewind-workspace")).unwrap();
    let lease = lock.try_mutation().unwrap();
    let owner = C12GenerationOwnerV1::open(state.path(), &lease).unwrap();
    owner.inject_failure("body-sync", libc::EIO);
    assert!(
        owner
            .publish_normal(ParticipationPublicationV1::Write, Some(&bytes))
            .is_err()
    );
    let before = m2a_tree_bytes(state.path());
    assert!(
        owner
            .inspect_c10_completed_cleanup(&target, digest)
            .is_err()
    );
    assert!(owner.cleanup_c10_completed_target(&target, digest).is_err());
    assert!(owner.inspect_exact_v2_cas(None, &bytes).is_err());
    assert_eq!(m2a_tree_bytes(state.path()), before);

    let (state, lock, target, _) = p3_c10_cleanup_fixture();
    let lease = lock.try_mutation().unwrap();
    let owner = C12GenerationOwnerV1::open(state.path(), &lease).unwrap();
    let C12InspectionV1::CleanupCommitted { mut marker } = owner.inspect().unwrap() else {
        panic!("cleanup fixture");
    };
    let legacy = serde_json::to_vec(target.inner()).unwrap();
    fs::write(path(state.path()), &legacy).unwrap();
    marker.participation_hash = Some(participation_bytes_hash(&legacy));
    fs::write(
        state
            .path()
            .join("sync/participation-generations-v1/generations")
            .join(marker.filename().unwrap()),
        marker.canonical_bytes().unwrap(),
    )
    .unwrap();
    assert!(matches!(
        owner.inspect().unwrap(),
        C12InspectionV1::CleanupCommitted { .. }
    ));
    let before = m2a_tree_bytes(state.path());
    let digest = SyncDigestV1::digest(&target.canonical_bytes().unwrap());
    assert!(
        owner
            .inspect_c10_completed_cleanup(&target, digest)
            .is_err()
    );
    assert!(owner.cleanup_c10_completed_target(&target, digest).is_err());
    assert_eq!(m2a_tree_bytes(state.path()), before);
}
