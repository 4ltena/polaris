//! Memory-change effect tests for durable mutation and recovery.

use super::*;
use crate::{
    conversation_state::HistoryMode, project_mutation::ProjectMutationOwnerV1,
    session_store::PersistedSession,
};
use polaris_memory::{Record, management::MemoryChangeV1};

struct Fixture {
    _root: tempfile::TempDir,
    project: PathBuf,
    data: PathBuf,
    state: PathBuf,
    lease: ProjectMutationLeaseV1,
    effects: ManagementEffectStoreV1,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let project = root.path().join("project");
        fs::create_dir(&project).unwrap();
        fs::create_dir(project.join(".polaris")).unwrap();
        let data = root.path().join("data");
        let state = data
            .join("state")
            .join(crate::project::project_id(&project));
        fs::create_dir_all(&state).unwrap();
        let owner = ProjectMutationOwnerV1::at_store_root(&state.join("rewind-workspace")).unwrap();
        let lease = owner.try_mutation().unwrap();
        let effects = ManagementEffectStoreV1::open_at_state_dir(&project, &state, &lease).unwrap();
        Self {
            _root: root,
            project,
            data,
            state,
            lease,
            effects,
        }
    }
    fn owner<'a>(&'a self, active_session: Option<&'a str>) -> MemoryChangeOwnerV1<'a> {
        MemoryChangeOwnerV1 {
            effects: &self.effects,
            lease: &self.lease,
            project_root: &self.project,
            data_root: &self.data,
            active_session,
        }
    }
    fn request(
        &self,
        store: &MemoryStore,
        operation: u64,
        change: MemoryChangeV1,
    ) -> MemoryChangeRequestV1 {
        MemoryChangeRequestV1 {
            operation_id: format!("{operation:032x}"),
            project_id: self.project.to_str().unwrap().into(),
            expected_revision: store
                .project_revision(self.project.to_str().unwrap())
                .unwrap(),
            change,
        }
    }
    fn archive(&self, session: &str, project: &str) -> PathBuf {
        let archive = self
            .state
            .join("memory-archives")
            .join(format!("{session}.archive"));
        fs::create_dir_all(&archive).unwrap();
        fs::set_permissions(&archive, fs::Permissions::from_mode(0o700)).unwrap();
        let path = archive.join("original.json");
        fs::write(&path,serde_json::to_vec(&serde_json::json!({"version":1,"project_id":project,"session_id":session,"messages":[]})).unwrap()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        archive
    }
}

#[test]
fn m3c_owner_forget_removes_only_verified_session_and_durable_receipt() {
    let fixture = Fixture::new();
    let mut store = fixture.owner(None).open_store().unwrap();
    let project = fixture.project.to_str().unwrap();
    let session = PersistedSession::create(
        &fixture.data,
        &fixture.state.join("memory.sqlite3"),
        project,
        HistoryMode::Legacy,
        None,
    )
    .unwrap();
    let target = session.snapshot().unwrap().state.session_id;
    let raw = fixture.data.join("sessions-v2").join(&target);
    let archive = fixture.archive(&target, project);
    let other = fixture.archive("unrelated", project);
    let record = Record {
        project_id: project.into(),
        session_id: target.clone(),
        id: "record".into(),
        role: "user".into(),
        timestamp: "2026-10-04".into(),
        source: "test".into(),
        text: "original".into(),
    };
    let request = fixture.request(
        &store,
        1,
        MemoryChangeV1::Import {
            records: vec![record],
        },
    );
    let prepared = fixture.owner(None).prepare(&store, request).unwrap();
    fixture
        .owner(None)
        .apply(&mut store, &prepared, true)
        .unwrap()
        .unwrap();
    let request = fixture.request(
        &store,
        2,
        MemoryChangeV1::Forget {
            session_id: target.clone(),
        },
    );
    let prepared = fixture.owner(None).prepare(&store, request).unwrap();
    let receipt = fixture
        .owner(None)
        .apply(&mut store, &prepared, true)
        .unwrap()
        .unwrap();
    assert_eq!(receipt.records_changed, 1);
    assert!(!raw.exists());
    assert!(!archive.exists());
    assert!(other.join("original.json").exists());
    assert!(store.is_session_forgotten(project, &target).unwrap());
    assert_eq!(
        fixture
            .owner(None)
            .reconcile(&store, "00000000000000000000000000000002".parse().unwrap())
            .unwrap(),
        receipt
    );
}

#[test]
fn m3c_owner_forget_denies_foreign_archive_symlink_and_active_session() {
    let fixture = Fixture::new();
    let mut store = fixture.owner(None).open_store().unwrap();
    let archive = fixture.archive("foreign", "/foreign-project");
    let request = fixture.request(
        &store,
        1,
        MemoryChangeV1::Forget {
            session_id: "foreign".into(),
        },
    );
    let prepared = fixture.owner(None).prepare(&store, request).unwrap();
    assert!(
        fixture
            .owner(None)
            .apply(&mut store, &prepared, true)
            .is_err()
    );
    assert!(archive.join("original.json").exists());
    assert!(
        !store
            .is_session_forgotten(fixture.project.to_str().unwrap(), "foreign")
            .unwrap()
    );
    let target = fixture.archive("target", fixture.project.to_str().unwrap());
    let link = fixture.state.join("memory-archives/link.archive");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let request = fixture.request(
        &store,
        2,
        MemoryChangeV1::Forget {
            session_id: "link".into(),
        },
    );
    let prepared = fixture.owner(None).prepare(&store, request).unwrap();
    assert!(
        fixture
            .owner(None)
            .apply(&mut store, &prepared, true)
            .is_err()
    );
    assert!(target.join("original.json").exists());
    let request = fixture.request(
        &store,
        3,
        MemoryChangeV1::Forget {
            session_id: "active".into(),
        },
    );
    assert!(
        fixture
            .owner(Some("active"))
            .prepare(&store, request)
            .is_err()
    );
    assert_eq!(
        store
            .project_revision(fixture.project.to_str().unwrap())
            .unwrap(),
        0
    );
}
