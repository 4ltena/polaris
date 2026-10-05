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
    let state = tempfile::tempdir().unwrap();
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
    updated.agreed_commit = Some("agreed".into());
    write(state.path(), &updated).unwrap();
    assert_eq!(read(state.path()), Some(updated.clone()));
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
        Some(participation_bytes_hash(
            &serde_json::to_vec_pretty(&updated).unwrap()
        ))
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
    let state = tempfile::tempdir().unwrap();
    assert!(read(state.path()).is_none());

    let participation = join(
        state.path(),
        link("share-0001"),
        ShareManifest::default(),
        1_000,
    )
    .unwrap();
    assert_eq!(read(state.path()), Some(participation.clone()));
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
    assert_eq!(read(state.path()).unwrap().share.share_id, "share-0001");

    assert!(leave(state.path()).unwrap());
    assert!(!leave(state.path()).unwrap());
    assert!(read(state.path()).is_none());
}
