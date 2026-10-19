//! Temporary projects only; the apply owner is exercised through the copy.
use super::*;

#[test]
fn m2a_agreed_tree_adapter_joins_the_owner_and_refuses_deferred_files_before_intent() {
    use crate::apply_intent::{RevisionKindV1, RevisionV1};
    use crate::apply_owner::{ApplyOwnerV1, C12ApplyAdmissionV1};
    use crate::management_effect::ManagementOperationIdV1;
    use crate::project_mutation::ProjectMutationOwnerV1;
    use crate::sync::{participation::Participation, share::ShareLink};
    use crate::workspace_apply::RecoveryParent;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    for deferred in [true, false] {
        let target = std::env::var_os("CARGO_TARGET_DIR").expect("isolated target");
        let temporary = tempfile::Builder::new()
            .prefix("c12-adapter-")
            .tempdir_in(target)
            .unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let project = root.join("project");
        let state = root.join("state");
        let agreed = root.join("agreed");
        let recovery = root.join("recovery");
        for path in [&project, &state, &agreed, &recovery, &state.join("sync")] {
            fs::create_dir(path).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        // The fixture is its own project even beneath the repository's target.
        fs::create_dir(project.join(".git")).unwrap();
        assert_eq!(crate::project::resolve_root(&project), project);
        fs::write(project.join("main.rs"), b"before\n").unwrap();
        fs::write(agreed.join("main.rs"), b"after\n").unwrap();
        let revision = RevisionV1 {
            kind: RevisionKindV1::GitSha1,
            value: "c".repeat(40),
        };
        let manifest = ShareManifest::default();
        let mut participation = Participation {
            share: ShareLink {
                remote: "file:///test-repository".into(),
                share_id: "project-1".into(),
                invitation_id: "invite-1".into(),
                expires_unix_seconds: u64::MAX,
            },
            manifest: manifest.clone(),
            joined_unix_seconds: 1,
            agreed_commit: Some(revision.value.clone()),
            applied_commit: None,
            unresolved: vec![],
        };
        fs::write(
            state.join("sync/participation.json"),
            serde_json::to_vec(&participation).unwrap(),
        )
        .unwrap();
        fs::set_permissions(
            state.join("sync/participation.json"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        participation.applied_commit = participation.agreed_commit.clone();
        crate::rewind_workspace::WorkspaceOwner::open(
            &project,
            &state.join("rewind-workspace"),
            Default::default(),
        )
        .unwrap();
        let lease_owner =
            ProjectMutationOwnerV1::at_store_root(&state.join("rewind-workspace")).unwrap();
        let owner = ApplyOwnerV1::with_lease(&project, &state, lease_owner.try_mutation().unwrap())
            .unwrap();
        let parent_file = fs::File::open(&recovery).unwrap();
        let meta = parent_file.metadata().unwrap();
        let parent =
            RecoveryParent::pin(&parent_file, (meta.dev(), meta.ino()), &recovery).unwrap();
        let mut changed = Changed {
            written: vec!["main.rs".into()],
            removed: vec![],
        };
        if deferred {
            fs::create_dir(agreed.join("missing")).unwrap();
            fs::write(agreed.join("missing/new.rs"), b"new\n").unwrap();
            changed.written.push("missing/new.rs".into());
        }
        let result = apply_agreed_m2a(
            &owner,
            C12ApplyAdmissionV1 {
                management_operation_id: ManagementOperationIdV1::random().unwrap(),
                agreed_revision: revision.clone(),
                staged_revision: revision,
                previous_applied_revision: None,
                target_participation: serde_json::to_vec(&participation).unwrap(),
            },
            &project,
            &agreed,
            &manifest,
            &changed,
            limits(),
            &parent,
        );
        if deferred {
            assert!(result.is_err());
            assert_eq!(fs::read(project.join("main.rs")).unwrap(), b"before\n");
            assert!(!state.join("apply-intents-v1").exists());
            assert!(!state.join("management-effects-v1").exists());
            assert_eq!(fs::read_dir(&recovery).unwrap().count(), 0);
        } else {
            let (report, shaped) = result.unwrap();
            assert!(report.failure.is_none());
            assert!(shaped.deferred.is_empty());
            assert_eq!(fs::read(project.join("main.rs")).unwrap(), b"after\n");
            assert!(owner.active().unwrap().is_none());
            assert_eq!(
                fs::read_dir(state.join("apply-intents-v1/archive"))
                    .unwrap()
                    .count(),
                1
            );
        }
    }
}

fn limits() -> Limits {
    Limits {
        max_entries: 256,
        max_files: 128,
        max_file_bytes: 64 * 1024,
        max_total_bytes: 1024 * 1024,
        max_depth: 8,
    }
}

fn tree(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    for (path, body) in files {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }
    (temporary, root)
}

#[test]
fn a_file_the_shared_history_never_held_is_left_alone() {
    // The agreed tree does not hold `notes.md`, but nobody removed it either:
    // it is this device's own file, written after the last exchange.
    let (_project, project_root) = tree(&[("src/main.rs", "古い\n"), ("notes.md", "手元の新規\n")]);
    let (_agreed, agreed) = tree(&[("src/main.rs", "新しい\n")]);
    let manifest = ShareManifest::default();
    let secrets = RegisteredSecrets::new([]).unwrap();
    let changed = Changed {
        written: vec!["src/main.rs".into()],
        removed: Vec::new(),
    };
    let (snapshot, shaped) = shape_copy(
        &project_root,
        &agreed,
        &manifest,
        &changed,
        &secrets,
        limits(),
    )
    .unwrap();

    assert_eq!(shaped.removed, Vec::<PathBuf>::new());
    assert_eq!(
        fs::read_to_string(snapshot.path().join("notes.md")).unwrap(),
        "手元の新規\n",
    );
}

#[test]
fn the_agreed_tree_decides_shared_files_only_and_the_project_is_not_written() {
    let (_project, project_root) = tree(&[
        ("src/main.rs", "古い\n"),
        ("local-only.md", "手元だけ\n"),
        ("gone.md", "消える\n"),
    ]);
    // Something the manifest never shares must survive untouched.
    fs::create_dir_all(project_root.join(".polaris")).unwrap();
    fs::write(project_root.join(".polaris/state.json"), "{}").unwrap();
    let (_agreed, agreed) = tree(&[("src/main.rs", "新しい\n"), ("added.md", "追加\n")]);

    let manifest = ShareManifest {
        version: 1,
        excluded: vec!["local-only.md".into()],
    };
    let secrets = RegisteredSecrets::new([]).unwrap();
    let changed = Changed {
        written: vec!["src/main.rs".into(), "added.md".into()],
        removed: vec!["gone.md".into()],
    };
    let (snapshot, shaped) = shape_copy(
        &project_root,
        &agreed,
        &manifest,
        &changed,
        &secrets,
        limits(),
    )
    .unwrap();

    let mut written = shaped.written.clone();
    written.sort();
    assert_eq!(
        written,
        vec![PathBuf::from("added.md"), PathBuf::from("src/main.rs")]
    );
    assert_eq!(shaped.removed, vec![PathBuf::from("gone.md")]);

    // The copy holds the agreement; the project still holds what it had.
    let copy = snapshot.path();
    assert_eq!(
        fs::read_to_string(copy.join("src/main.rs")).unwrap(),
        "新しい\n"
    );
    assert!(!copy.join("gone.md").exists());
    assert_eq!(
        fs::read_to_string(copy.join("local-only.md")).unwrap(),
        "手元だけ\n",
        "an excluded file is never replaced or removed"
    );
    assert_eq!(
        fs::read_to_string(project_root.join("src/main.rs")).unwrap(),
        "古い\n",
        "shaping the copy must not write to the project"
    );
    assert!(project_root.join("gone.md").exists());
    assert!(project_root.join(".polaris/state.json").exists());

    // What the apply owner would carry out: the two writes and the removal.
    let changes = planned_changes(&snapshot, &secrets, limits()).unwrap();
    let mut paths: Vec<String> = changes
        .changes
        .iter()
        .map(|change| change.relative_path.to_string_lossy().into_owned())
        .collect();
    paths.sort();
    assert_eq!(
        paths,
        vec![
            "added.md".to_string(),
            "gone.md".into(),
            "src/main.rs".into()
        ]
    );
}
