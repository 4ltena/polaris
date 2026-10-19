//! Fail-closed capacity accounting for durable rewind candidates.
//!
//! This module intentionally never removes an interrupted candidate.  It runs
//! after the project mutation lease and source writer lock are held, once the
//! immutable candidate plan is available, and before a child directory is
//! created.

use crate::{
    conversation_state::{ConversationStateV2, RewindBranchMetadataV1},
    project_mutation::ProjectMutationLeaseV1,
    rewind_workspace::WorkspaceOwner,
};
use std::{collections::BTreeSet, fs, io, os::unix::fs::MetadataExt, path::Path};

pub const MAX_RETAINED_CANDIDATES_PER_PROJECT: usize = 128;
pub const MAX_CANDIDATE_FILE_BYTES_PER_PROJECT: u64 = 2 * 1024 * 1024 * 1024;
pub const CANDIDATE_OVERHEAD_BYTES: u64 = 1024 * 1024;
pub const FREE_SPACE_RESERVE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SCAN_DEPTH: usize = 64;
const MAX_SCAN_ENTRIES: usize = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CandidateCapacity {
    pub retained_candidates: usize,
    pub candidate_file_bytes: u64,
    pub reservation_bytes: u64,
    pub available_bytes: u64,
}

/// Checks the project-wide rewind-candidate quota and the filesystem reserve.
///
/// `estimated_candidate_file_bytes` is the serialized raw/state/metadata
/// payload from the immutable candidate plan.  The fixed overhead reserves
/// directory and marker growth without creating any child evidence.
pub fn preflight_rewind_candidate_capacity(
    workspace: &WorkspaceOwner,
    lease: &ProjectMutationLeaseV1,
    data_root: &Path,
    project_id: &str,
    estimated_candidate_file_bytes: u64,
) -> io::Result<CandidateCapacity> {
    workspace
        .require_quiescent_with_lease(lease)
        .map_err(workspace_error)?;
    let available_bytes = filesystem_available_bytes(&data_root.join("sessions-v2"))?;
    preflight_with_available_bytes(
        workspace,
        data_root,
        project_id,
        estimated_candidate_file_bytes,
        available_bytes,
    )
}

fn preflight_with_available_bytes(
    workspace: &WorkspaceOwner,
    data_root: &Path,
    project_id: &str,
    estimated_candidate_file_bytes: u64,
    available_bytes: u64,
) -> io::Result<CandidateCapacity> {
    if project_id.is_empty() {
        return Err(invalid("rewind候補のproject IDが不正です"));
    }
    let reservation_bytes =
        require_available_capacity(estimated_candidate_file_bytes, available_bytes)?;

    let transaction_ids: BTreeSet<String> = workspace
        .branch_transaction_candidate_ids()
        .map_err(workspace_error)?
        .into_iter()
        .collect();
    let root = data_root.join("sessions-v2");
    let root_meta = fs::symlink_metadata(&root)?;
    if root_meta.file_type().is_symlink() || !root_meta.is_dir() {
        return Err(invalid(
            "sessions-v2保存先が安全なディレクトリではありません",
        ));
    }

    let mut accounted_ids = BTreeSet::new();
    let mut file_bytes = 0_u64;
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() || !file_type.is_dir() {
            return Err(invalid("sessions-v2に安全でない項目があります"));
        }
        let session_id = entry
            .file_name()
            .into_string()
            .map_err(|_| invalid("セッションIDがUTF-8ではありません"))?;
        if !is_session_id(&session_id) {
            return Err(invalid("セッションIDが不正です"));
        }

        let inspection = inspect_session(&root, &path, &session_id, project_id, &transaction_ids)?;
        if inspection.project_id != project_id {
            if transaction_ids.contains(&session_id) {
                return Err(invalid("rewind transactionの候補projectが一致しません"));
            }
            continue;
        }
        if inspection.is_candidate {
            let mut scanned_entries = 0;
            let bytes = regular_file_bytes(&path, 0, &mut scanned_entries)?;
            if !accounted_ids.insert(session_id) {
                return Err(invalid("rewind候補セッションが重複しています"));
            }
            file_bytes = file_bytes
                .checked_add(bytes)
                .ok_or_else(|| invalid("rewind候補ファイル容量が上限です"))?;
        }
    }

    // A transaction can survive the crash before its candidate directory is
    // created. It still consumes an evidence slot and must never be ignored.
    for id in transaction_ids {
        if !accounted_ids.contains(&id) {
            accounted_ids.insert(id);
        }
    }
    let retained_candidates = accounted_ids.len();
    if retained_candidates >= MAX_RETAINED_CANDIDATES_PER_PROJECT {
        return Err(io::Error::new(
            io::ErrorKind::StorageFull,
            "rewind候補数が上限です",
        ));
    }
    let resulting_bytes = file_bytes
        .checked_add(reservation_bytes)
        .ok_or_else(|| invalid("rewind候補ファイル容量が上限です"))?;
    if resulting_bytes > MAX_CANDIDATE_FILE_BYTES_PER_PROJECT {
        return Err(io::Error::new(
            io::ErrorKind::StorageFull,
            "rewind候補ファイル容量が上限です",
        ));
    }
    Ok(CandidateCapacity {
        retained_candidates,
        candidate_file_bytes: file_bytes,
        reservation_bytes,
        available_bytes,
    })
}

struct SessionInspection {
    project_id: String,
    is_candidate: bool,
}

fn inspect_session(
    root: &Path,
    directory: &Path,
    session_id: &str,
    current_project_id: &str,
    transaction_ids: &BTreeSet<String>,
) -> io::Result<SessionInspection> {
    let metadata_path = directory.join("rewind-branch-v1.json");
    let metadata = read_optional_json::<RewindBranchMetadataV1>(&metadata_path)?;
    if let Some(metadata) = &metadata {
        validate_candidate_metadata(metadata)?;
    }
    let is_candidate = metadata.is_some() || transaction_ids.contains(session_id);
    let state_path = directory.join("state.json");
    let state = read_optional_json::<ConversationStateV2>(&state_path)?;

    let project_id = match (state, metadata.as_ref()) {
        (Some(state), Some(metadata)) => {
            state.validate()?;
            if state.session_id != session_id {
                return Err(invalid("rewind候補stateのsession IDが一致しません"));
            }
            if origin_project_id(root, metadata)? != state.project_id {
                return Err(invalid("rewind候補の分岐元projectが一致しません"));
            }
            state.project_id
        }
        (Some(state), None) => {
            state.validate()?;
            if state.session_id != session_id {
                return Err(invalid("セッションstateのsession IDが一致しません"));
            }
            state.project_id
        }
        (None, Some(_metadata)) if transaction_ids.contains(session_id) => {
            current_project_id.into()
        }
        (None, Some(metadata)) => origin_project_id(root, metadata)?,
        (None, None) if transaction_ids.contains(session_id) => current_project_id.into(),
        (None, None) => {
            return Err(invalid("未完了または所属不明のセッションがあります"));
        }
    };
    Ok(SessionInspection {
        project_id,
        is_candidate,
    })
}

fn origin_project_id(root: &Path, metadata: &RewindBranchMetadataV1) -> io::Result<String> {
    let origin = root.join(&metadata.origin_session_id);
    let origin_meta = fs::symlink_metadata(&origin)?;
    if origin_meta.file_type().is_symlink() || !origin_meta.is_dir() {
        return Err(invalid(
            "rewind候補の分岐元が安全なセッションではありません",
        ));
    }
    let origin_state = read_required_json::<ConversationStateV2>(&origin.join("state.json"))?;
    origin_state.validate()?;
    if origin_state.session_id != metadata.origin_session_id {
        return Err(invalid("rewind候補の分岐元session IDが一致しません"));
    }
    Ok(origin_state.project_id)
}

fn validate_candidate_metadata(metadata: &RewindBranchMetadataV1) -> io::Result<()> {
    if metadata.schema_version != 1
        || !is_session_id(&metadata.origin_session_id)
        || !is_opaque_id(&metadata.boundary_id)
        || !is_opaque_id(&metadata.workspace_pre_manifest_id)
        || !is_opaque_id(&metadata.workspace_post_manifest_id)
        || !is_hash(&metadata.branch_initial_raw_hash)
        || (metadata.branch_initial_raw_offset == 0
            && metadata.branch_initial_raw_hash != crate::conversation_state::content_hash(&[]))
        || metadata
            .visible_memory_raw_object_ids
            .iter()
            .any(|id| !is_hash(id))
        || metadata
            .visible_memory_raw_object_ids
            .iter()
            .collect::<BTreeSet<_>>()
            .len()
            != metadata.visible_memory_raw_object_ids.len()
    {
        return Err(invalid("rewind候補metadataが不正です"));
    }
    Ok(())
}

fn read_optional_json<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<Option<T>> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_file() || meta.nlink() != 1 {
                return Err(invalid("rewind候補内に安全でないファイルがあります"));
            }
            Ok(Some(serde_json::from_slice(&fs::read(path)?)?))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn read_required_json<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<T> {
    read_optional_json(path)?.ok_or_else(|| invalid("rewind候補のstateがありません"))
}

fn regular_file_bytes(
    directory: &Path,
    depth: usize,
    scanned_entries: &mut usize,
) -> io::Result<u64> {
    if depth > MAX_SCAN_DEPTH {
        return Err(invalid("rewind候補ディレクトリの深さが上限です"));
    }
    let mut total = 0_u64;
    for entry in fs::read_dir(directory)? {
        *scanned_entries = scanned_entries
            .checked_add(1)
            .ok_or_else(|| invalid("rewind候補走査数が上限です"))?;
        if *scanned_entries > MAX_SCAN_ENTRIES {
            return Err(invalid("rewind候補走査数が上限です"));
        }
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            return Err(invalid("rewind候補内のsymlinkは使用できません"));
        }
        if file_type.is_dir() {
            total = total
                .checked_add(regular_file_bytes(&path, depth + 1, scanned_entries)?)
                .ok_or_else(|| invalid("rewind候補ファイル容量が上限です"))?;
        } else if file_type.is_file() {
            let meta = fs::symlink_metadata(&path)?;
            if meta.nlink() != 1 {
                return Err(invalid("rewind候補内のhard linkは使用できません"));
            }
            total = total
                .checked_add(meta.len())
                .ok_or_else(|| invalid("rewind候補ファイル容量が上限です"))?;
        } else {
            return Err(invalid("rewind候補内に通常ファイル以外があります"));
        }
    }
    Ok(total)
}

fn require_available_capacity(
    estimated_candidate_file_bytes: u64,
    available_bytes: u64,
) -> io::Result<u64> {
    let reservation_bytes = estimated_candidate_file_bytes
        .checked_add(CANDIDATE_OVERHEAD_BYTES)
        .ok_or_else(|| invalid("rewind候補の予約容量が上限です"))?;
    let required_free_bytes = reservation_bytes
        .checked_add(FREE_SPACE_RESERVE_BYTES)
        .ok_or_else(|| invalid("rewind候補の空き容量予約が上限です"))?;
    if available_bytes < required_free_bytes {
        return Err(io::Error::new(
            io::ErrorKind::StorageFull,
            "rewind候補用の空き容量が不足しています",
        ));
    }
    Ok(reservation_bytes)
}

fn filesystem_available_bytes(path: &Path) -> io::Result<u64> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| invalid("sessions-v2保存先が不正です"))?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    #[cfg(any(target_os = "macos", target_pointer_width = "32"))]
    let blocks = u64::from(stat.f_bavail);
    #[cfg(all(not(target_os = "macos"), target_pointer_width = "64"))]
    let blocks = stat.f_bavail;
    let unit = stat.f_frsize;
    #[cfg(target_pointer_width = "32")]
    let unit = u64::from(unit);
    blocks
        .checked_mul(unit)
        .ok_or_else(|| invalid("sessions-v2空き容量が上限です"))
}

fn is_session_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

fn is_hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn is_opaque_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.bytes().any(|byte| byte.is_ascii_control())
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn workspace_error(error: crate::rewind_workspace::WorkspaceError) -> io::Error {
    io::Error::other(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation_state::{ConversationStore, RewindEffectStatusV1, RewindEligibilityV1};
    use crate::rewind_workspace::RewindLimits;
    use polaris_provider::Message;
    use polaris_sandbox::{SandboxMode, SandboxPolicy};
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    const PROJECT_ID: &str = "project";
    const ORIGIN_ID: &str = "00000000-0000-4000-8000-000000000001";

    struct CapacityFixture {
        _temp: tempfile::TempDir,
        project: PathBuf,
        data_root: PathBuf,
        workspace: WorkspaceOwner,
        origin: ConversationStore,
    }

    impl CapacityFixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            fs::set_permissions(
                temp.path(),
                <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
            )
            .unwrap();
            let project = temp.path().join("project");
            let data_root = temp.path().join("data");
            fs::create_dir(&project).unwrap();
            fs::create_dir(&data_root).unwrap();
            let workspace = WorkspaceOwner::open(
                &project,
                &temp.path().join("rewind-workspace"),
                RewindLimits::default(),
            )
            .unwrap();
            let origin = ConversationStore::create(
                &data_root,
                ConversationStateV2::new(PROJECT_ID.into(), ORIGIN_ID.into()).unwrap(),
            )
            .unwrap();
            Self {
                _temp: temp,
                project,
                data_root,
                workspace,
                origin,
            }
        }

        fn candidate_id(index: u64) -> String {
            format!("00000000-0000-4000-8000-{index:012x}")
        }

        fn metadata_only_candidate(&self, index: u64) -> PathBuf {
            let directory = self
                .data_root
                .join("sessions-v2")
                .join(Self::candidate_id(index));
            fs::create_dir(&directory).unwrap();
            let metadata = RewindBranchMetadataV1 {
                schema_version: 1,
                boundary_id: "boundary".into(),
                origin_session_id: ORIGIN_ID.into(),
                origin_generation: 0,
                workspace_pre_manifest_id: "pre".into(),
                workspace_post_manifest_id: "post".into(),
                branch_initial_raw_offset: 0,
                branch_initial_raw_hash: crate::conversation_state::content_hash(&[]),
                visible_memory_raw_object_ids: Vec::new(),
            };
            fs::write(
                directory.join("rewind-branch-v1.json"),
                serde_json::to_vec(&metadata).unwrap(),
            )
            .unwrap();
            directory
        }

        fn preflight(&self) -> io::Result<CandidateCapacity> {
            let lease = self.workspace.try_mutation().unwrap();
            preflight_rewind_candidate_capacity(
                &self.workspace,
                &lease,
                &self.data_root,
                PROJECT_ID,
                0,
            )
        }
    }

    #[test]
    fn reservation_uses_exact_threshold() {
        let required = 17 + CANDIDATE_OVERHEAD_BYTES + FREE_SPACE_RESERVE_BYTES;
        assert_eq!(
            require_available_capacity(17, required).unwrap(),
            17 + CANDIDATE_OVERHEAD_BYTES
        );
        assert_eq!(
            require_available_capacity(17, required - 1)
                .unwrap_err()
                .kind(),
            io::ErrorKind::StorageFull
        );
    }

    #[test]
    fn session_id_validation_rejects_path_traversal() {
        assert!(!is_session_id("../00000000-0000-4000-8000-000000000000"));
        assert!(is_session_id("00000000-0000-4000-8000-000000000000"));
    }

    #[test]
    fn incomplete_unattributable_session_blocks_the_scan() {
        let root = tempfile::tempdir().unwrap();
        let id = "00000000-0000-4000-8000-000000000000";
        let directory = root.path().join(id);
        fs::create_dir(&directory).unwrap();
        assert!(inspect_session(root.path(), &directory, id, "project", &BTreeSet::new()).is_err());
    }

    #[test]
    fn transaction_referenced_empty_child_is_attributed_to_the_project() {
        let root = tempfile::tempdir().unwrap();
        let id = "00000000-0000-4000-8000-000000000000";
        let directory = root.path().join(id);
        fs::create_dir(&directory).unwrap();
        let transaction_ids = BTreeSet::from([id.to_owned()]);
        let inspection =
            inspect_session(root.path(), &directory, id, "project", &transaction_ids).unwrap();
        assert_eq!(inspection.project_id, "project");
        assert!(inspection.is_candidate);
    }

    #[test]
    fn candidate_symlink_blocks_byte_accounting() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        fs::write(&target, b"evidence").unwrap();
        let candidate = root.path().join("candidate");
        fs::create_dir(&candidate).unwrap();
        symlink(&target, candidate.join("raw.jsonl")).unwrap();
        assert!(regular_file_bytes(&candidate, 0, &mut 0).is_err());
    }

    #[test]
    fn foreign_normal_session_does_not_consume_candidate_scan() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let data_root = root.path().join("data");
        fs::create_dir(&project).unwrap();
        fs::create_dir(&data_root).unwrap();
        let workspace = WorkspaceOwner::open(
            &project,
            &root.path().join("rewind-workspace"),
            RewindLimits::default(),
        )
        .unwrap();
        let foreign = ConversationStore::create(
            &data_root,
            ConversationStateV2::new(
                "foreign".into(),
                "00000000-0000-4000-8000-000000000044".into(),
            )
            .unwrap(),
        )
        .unwrap();
        symlink(root.path(), foreign.directory().join("foreign-link")).unwrap();

        let capacity = preflight_with_available_bytes(
            &workspace,
            &data_root,
            "local",
            0,
            FREE_SPACE_RESERVE_BYTES + CANDIDATE_OVERHEAD_BYTES,
        )
        .unwrap();
        assert_eq!(capacity.retained_candidates, 0);
        assert_eq!(capacity.candidate_file_bytes, 0);
    }

    #[test]
    fn metadata_only_candidates_allow_the_128th_and_reject_the_129th() {
        let fixture = CapacityFixture::new();
        for index in 2..=128 {
            fixture.metadata_only_candidate(index);
        }
        let available = FREE_SPACE_RESERVE_BYTES + CANDIDATE_OVERHEAD_BYTES;
        let capacity = preflight_with_available_bytes(
            &fixture.workspace,
            &fixture.data_root,
            PROJECT_ID,
            0,
            available,
        )
        .unwrap();
        assert_eq!(capacity.retained_candidates, 127);

        fixture.metadata_only_candidate(129);
        assert_eq!(
            preflight_with_available_bytes(
                &fixture.workspace,
                &fixture.data_root,
                PROJECT_ID,
                0,
                available,
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::StorageFull
        );
    }

    #[test]
    fn retained_file_bytes_enforce_the_two_gib_boundary_without_allocating_it() {
        let fixture = CapacityFixture::new();
        let directory = fixture.metadata_only_candidate(2);
        let metadata_bytes = fs::metadata(directory.join("rewind-branch-v1.json"))
            .unwrap()
            .len();
        let file = fs::File::create(directory.join("partial.raw")).unwrap();
        let reservation = CANDIDATE_OVERHEAD_BYTES + 17;
        file.set_len(MAX_CANDIDATE_FILE_BYTES_PER_PROJECT - reservation - metadata_bytes)
            .unwrap();
        let available = FREE_SPACE_RESERVE_BYTES + reservation;
        let capacity = preflight_with_available_bytes(
            &fixture.workspace,
            &fixture.data_root,
            PROJECT_ID,
            17,
            available,
        )
        .unwrap();
        assert_eq!(
            capacity.candidate_file_bytes + reservation,
            MAX_CANDIDATE_FILE_BYTES_PER_PROJECT
        );

        file.set_len(file.metadata().unwrap().len() + 1).unwrap();
        assert_eq!(
            preflight_with_available_bytes(
                &fixture.workspace,
                &fixture.data_root,
                PROJECT_ID,
                17,
                available,
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::StorageFull
        );
    }

    #[test]
    fn interrupted_candidate_creation_remains_in_the_next_preflight() {
        let fixture = CapacityFixture::new();
        let mut source = fixture.origin.lock().unwrap();
        let boundary = source
            .begin_rewind_boundary("pre".into(), Vec::new())
            .unwrap();
        source.append(Message::user("captured"), true).unwrap();
        source.append(Message::assistant("done"), false).unwrap();
        let snapshot = source.snapshot().unwrap();
        source.publish(&snapshot, snapshot.state.clone()).unwrap();
        source
            .finish_rewind_boundary(
                &boundary.boundary_id,
                "post".into(),
                RewindEligibilityV1::Eligible,
                RewindEffectStatusV1::NoExternalMutation,
            )
            .unwrap();
        let plan = source
            .plan_rewind_branch(&boundary.boundary_id, &CapacityFixture::candidate_id(2))
            .unwrap();
        crate::conversation_state::FAIL_POINT
            .with(|point| point.set(Some("rewind_candidate_after_metadata")));
        let creation = source.create_rewind_branch_from_plan(&fixture.data_root, plan);
        crate::conversation_state::FAIL_POINT.with(|point| point.set(None));
        assert!(creation.is_err());
        drop(source);

        let child = fixture
            .data_root
            .join("sessions-v2")
            .join(CapacityFixture::candidate_id(2));
        assert!(child.join("rewind-branch-v1.json").is_file());
        assert!(!child.join("state.json").exists());
        let capacity = fixture.preflight().unwrap();
        assert_eq!(capacity.retained_candidates, 1);
        assert!(capacity.candidate_file_bytes > 0);
    }

    #[test]
    fn rolled_back_transaction_and_partial_child_count_once_after_a_competing_lease() {
        let fixture = CapacityFixture::new();
        let policy = SandboxPolicy::new(
            SandboxMode::WorkspaceWrite,
            std::slice::from_ref(&fixture.project),
        )
        .unwrap();
        fs::write(fixture.project.join("file.txt"), b"pre").unwrap();
        let pre = fixture.workspace.capture(&policy).unwrap();
        fs::write(fixture.project.join("file.txt"), b"post").unwrap();
        let post = fixture.workspace.capture(&policy).unwrap();
        let candidate_id = CapacityFixture::candidate_id(2);
        let transaction = fixture
            .workspace
            .restore_for_branch(&post, &pre, &candidate_id)
            .unwrap();
        fixture
            .workspace
            .rollback(&transaction.transaction_id)
            .unwrap();

        let lease = fixture.workspace.try_mutation().unwrap();
        std::thread::scope(|scope| {
            assert!(
                scope
                    .spawn(|| fixture.workspace.try_mutation().is_err())
                    .join()
                    .unwrap()
            );
        });
        drop(lease);
        let capacity = fixture.preflight().unwrap();
        assert_eq!(capacity.retained_candidates, 1);
        assert_eq!(capacity.candidate_file_bytes, 0);

        let partial = fixture.data_root.join("sessions-v2").join(&candidate_id);
        fs::create_dir(&partial).unwrap();
        fs::write(partial.join("partial.raw"), b"evidence").unwrap();
        let capacity = fixture.preflight().unwrap();
        assert_eq!(capacity.retained_candidates, 1);
        assert_eq!(capacity.candidate_file_bytes, b"evidence".len() as u64);
    }
}
