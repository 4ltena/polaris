//! Execution admission for saved conversations after a workspace rewind.
//!
//! Reading a v2 conversation is deliberately weaker than running it. A
//! prepared child becomes runnable only after its matching workspace
//! transaction has committed and its source evidence still verifies.

use crate::conversation_state::{ConversationStateV2, ConversationStore, RewindBranchMetadataV1};
use crate::rewind_workspace::{ManifestId, WorkspaceOwner};
use crate::session_store::PersistedSession;
use polaris_sandbox::{SandboxMode, SandboxPolicy};
use std::io;
use std::{collections::BTreeSet, fs, path::Path};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateDiagnostic {
    pub id: String,
    pub reason: String,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// One budget covers transaction evidence and transitive conversation provenance.
#[derive(Debug)]
pub struct PublicationReadBudgetV1 {
    pub max_entries: usize,
    pub max_file_bytes: usize,
    pub max_total_bytes: usize,
    bytes_read: usize,
    read_failed: bool,
}
impl Default for PublicationReadBudgetV1 {
    fn default() -> Self {
        Self {
            max_entries: 4096,
            max_file_bytes: 4 * 1024 * 1024,
            max_total_bytes: 32 * 1024 * 1024,
            bytes_read: 0,
            read_failed: false,
        }
    }
}
impl PublicationReadBudgetV1 {
    pub fn evidence_read_failed(&self) -> bool {
        self.read_failed
    }
    pub(crate) fn paths(&mut self, path: &Path) -> io::Result<Vec<std::path::PathBuf>> {
        let result = self.paths_inner(path);
        if result.is_err() {
            self.read_failed = true;
        }
        result
    }
    fn paths_inner(&mut self, path: &Path) -> io::Result<Vec<std::path::PathBuf>> {
        let directory = crate::project_mutation::open_directory_nofollow(path)?;
        Ok(
            crate::project_mutation::directory_names_bounded(&directory, self.max_entries)?
                .into_iter()
                .map(|name| path.join(name))
                .collect(),
        )
    }
    pub(crate) fn read(&mut self, path: &Path) -> io::Result<Vec<u8>> {
        let result = self.read_inner(path);
        if result.is_err() {
            self.read_failed = true;
        }
        result
    }
    fn read_inner(&mut self, path: &Path) -> io::Result<Vec<u8>> {
        use std::{
            ffi::CString,
            io::Read,
            os::{
                fd::{AsRawFd, FromRawFd},
                unix::{ffi::OsStrExt, fs::MetadataExt},
            },
        };
        let parent = crate::project_mutation::open_directory_nofollow(
            path.parent()
                .ok_or_else(|| invalid("保存ファイルの親がありません"))?,
        )?;
        let parent_info = parent.metadata()?;
        if parent_info.uid() != unsafe { libc::geteuid() } || parent_info.mode() & 0o7077 != 0 {
            return Err(invalid("保存ディレクトリが非公開ではありません"));
        }
        let name = CString::new(
            path.file_name()
                .ok_or_else(|| invalid("保存ファイル名がありません"))?
                .as_bytes(),
        )
        .map_err(io::Error::other)?;
        let protection = polaris_auth::protection::with_protected_paths(Clone::clone)
            .map_err(|_| invalid("保護対象を確認できません"))?;
        if protection.paths().contains(path) {
            return Err(invalid("保護対象を読めません"));
        }
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let file = unsafe { fs::File::from_raw_fd(fd) };
        let info = file.metadata()?;
        let max = self
            .max_file_bytes
            .min(self.max_total_bytes.saturating_sub(self.bytes_read));
        if !info.is_file()
            || info.uid() != unsafe { libc::geteuid() }
            || info.nlink() != 1
            || info.mode() & 0o7177 != 0
            || info.len() > max as u64
            || protection
                .identities()
                .contains(&polaris_auth::protection::ProtectedIdentity {
                    dev: info.dev(),
                    ino: info.ino(),
                })
        {
            return Err(invalid(
                "公開証拠が保護対象、不正なファイル、または読取上限を超えています",
            ));
        }
        let mut body = Vec::new();
        file.take(max as u64 + 1).read_to_end(&mut body)?;
        if body.len() > max {
            return Err(invalid("公開証拠の読取上限を超えました"));
        }
        self.bytes_read = self
            .bytes_read
            .checked_add(body.len())
            .ok_or_else(|| invalid("公開証拠の合計上限を超えました"))?;
        Ok(body)
    }
}

/// Uses the exact publication/provenance validators with descriptor-bounded reads.
pub fn published_branch_bounded(
    owner: &WorkspaceOwner,
    child: &ConversationStore,
    reads: &mut PublicationReadBudgetV1,
) -> io::Result<Option<RewindBranchMetadataV1>> {
    published_branch_with_reads(owner, child, &mut Some(reads))
}

fn publication_read(
    reads: &mut Option<&mut PublicationReadBudgetV1>,
    path: &Path,
) -> io::Result<Vec<u8>> {
    match reads.as_deref_mut() {
        Some(reads) => reads.read(path),
        None => fs::read(path),
    }
}

fn published_branch_with_reads(
    owner: &WorkspaceOwner,
    child: &ConversationStore,
    reads: &mut Option<&mut PublicationReadBudgetV1>,
) -> io::Result<Option<RewindBranchMetadataV1>> {
    let snapshot = child
        .lock()?
        .snapshot_with_reader(&mut |path| publication_read(reads, path))?;
    let candidate_id = &snapshot.state.session_id;
    let referenced = owner
        .has_branch_transaction_candidate_bounded(candidate_id, reads)
        .map_err(io::Error::other)?;
    let Some(metadata) =
        child.rewind_branch_metadata_with_reader(&mut |path| publication_read(reads, path))?
    else {
        return if referenced {
            Err(invalid("候補会話の分岐metadataがありません"))
        } else {
            Ok(None)
        };
    };
    let source = ConversationStore::open_with_reader(
        child.data_root()?,
        &snapshot.state.project_id,
        &metadata.origin_session_id,
        &mut |path| publication_read(reads, path),
    )?;
    let verified = ConversationStore::validate_rewind_branch_candidate_with_reader(
        &source,
        child,
        &mut |path| publication_read(reads, path),
    )?;
    let pre: ManifestId = verified
        .workspace_pre_manifest_id
        .parse()
        .map_err(io::Error::other)?;
    let transaction = owner
        .committed_branch_transaction_for_candidate_bounded(candidate_id, reads)
        .map_err(io::Error::other)?;
    if transaction.target_pre != pre {
        return Err(invalid("候補会話のworkspace transactionが一致しません"));
    }
    let activity = owner.try_activity_quiescent().map_err(io::Error::other)?;
    match owner
        .verify_rewind_management_with_activity(
            &activity,
            &transaction.expected_post,
            &transaction.target_pre,
        )
        .map_err(io::Error::other)?
    {
        crate::management_effect::ManagementRewindCrossingV1::Clear => {}
        crate::management_effect::ManagementRewindCrossingV1::Blocked { .. } => {
            return Err(invalid("候補会話は管理effectを越えて巻き戻せません"));
        }
    }
    Ok(Some(verified))
}

/// Opens a saved candidate without the legacy unbounded snapshot on open.
pub fn published_session_bounded(
    owner: &WorkspaceOwner,
    data_root: &Path,
    project_id: &str,
    session_id: &str,
    reads: &mut PublicationReadBudgetV1,
) -> io::Result<Option<RewindBranchMetadataV1>> {
    let child =
        ConversationStore::open_with_reader(data_root, project_id, session_id, &mut |path| {
            reads.read(path)
        })?;
    published_branch_bounded(owner, &child, reads)
}

/// Checks publication evidence without comparing the current workspace tree.
/// The picker uses this to hide failed children while retaining ordinary
/// sessions that the user may still inspect read-only.
pub fn published_branch(
    owner: &WorkspaceOwner,
    child: &ConversationStore,
) -> io::Result<Option<RewindBranchMetadataV1>> {
    published_branch_with_reads(owner, child, &mut None)
}

/// Enumerates unavailable candidates and unattributable directories without
/// binding a legacy head or changing saved conversation evidence.
pub fn candidate_diagnostics(
    owner: &WorkspaceOwner,
    data_root: &Path,
    project_id: &str,
) -> Vec<CandidateDiagnostic> {
    candidate_diagnostics_inner(Some(owner), data_root, project_id)
}

/// Reports saved child IDs when the project's rewind store is missing. Their
/// publication evidence cannot be verified, but the sessions themselves are
/// left untouched so a later recovery can inspect them.
pub fn candidate_diagnostics_without_workspace(
    data_root: &Path,
    project_id: &str,
) -> Vec<CandidateDiagnostic> {
    candidate_diagnostics_inner(None, data_root, project_id)
}

fn candidate_diagnostics_inner(
    owner: Option<&WorkspaceOwner>,
    data_root: &Path,
    project_id: &str,
) -> Vec<CandidateDiagnostic> {
    let mut diagnostics = Vec::new();
    let mut transaction_ids: BTreeSet<String> = match owner {
        Some(owner) => match owner.branch_transaction_candidate_ids() {
            Ok(ids) => ids.into_iter().collect(),
            Err(error) => {
                diagnostics.push(CandidateDiagnostic {
                    id: "<transaction>".into(),
                    reason: error.to_string(),
                });
                BTreeSet::new()
            }
        },
        None => BTreeSet::new(),
    };
    let root = data_root.join("sessions-v2");
    match fs::symlink_metadata(&root) {
        Ok(info) if info.file_type().is_symlink() || !info.is_dir() => {
            diagnostics.push(CandidateDiagnostic {
                id: "<sessions-v2>".into(),
                reason: "セッション保存先が安全なディレクトリではありません".into(),
            });
            return diagnostics;
        }
        Err(error) if owner.is_none() && error.kind() == io::ErrorKind::NotFound => {
            return diagnostics;
        }
        Err(error) => {
            diagnostics.push(CandidateDiagnostic {
                id: "<sessions-v2>".into(),
                reason: error.to_string(),
            });
            return diagnostics;
        }
        Ok(_) => {}
    }
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) => {
            diagnostics.push(CandidateDiagnostic {
                id: "<sessions-v2>".into(),
                reason: error.to_string(),
            });
            return diagnostics;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                diagnostics.push(CandidateDiagnostic {
                    id: "<sessions-v2>".into(),
                    reason: error.to_string(),
                });
                continue;
            }
        };
        let id = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let referenced = transaction_ids.remove(&id);
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                diagnostics.push(CandidateDiagnostic {
                    id,
                    reason: error.to_string(),
                });
                continue;
            }
        };
        if file_type.is_symlink() || !file_type.is_dir() {
            diagnostics.push(CandidateDiagnostic {
                id,
                reason: "セッション項目が安全なディレクトリではありません".into(),
            });
            continue;
        }
        let metadata_path = path.join("rewind-branch-v1.json");
        let has_metadata = match fs::symlink_metadata(&metadata_path) {
            Ok(_) => true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => {
                diagnostics.push(CandidateDiagnostic {
                    id,
                    reason: error.to_string(),
                });
                continue;
            }
        };
        let state_path = path.join("state.json");
        let state = (|| -> io::Result<ConversationStateV2> {
            let info = match fs::symlink_metadata(&state_path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Err(invalid("state.jsonがありません"));
                }
                other => other?,
            };
            if info.file_type().is_symlink() || !info.is_file() {
                return Err(invalid("セッションstateが安全な通常ファイルではありません"));
            }
            let state: ConversationStateV2 = serde_json::from_slice(&fs::read(&state_path)?)?;
            state.validate()?;
            if state.session_id != id {
                return Err(invalid("セッションstateのIDがディレクトリと一致しません"));
            }
            Ok(state)
        })();
        let state = match state {
            Ok(state) => state,
            Err(error) => {
                diagnostics.push(CandidateDiagnostic {
                    id,
                    reason: error.to_string(),
                });
                continue;
            }
        };
        if state.project_id != project_id {
            if referenced {
                diagnostics.push(CandidateDiagnostic {
                    id,
                    reason: "候補transactionのprojectが一致しません".into(),
                });
            }
            continue;
        }
        if !has_metadata && !referenced {
            continue;
        }
        let published = match owner {
            Some(owner) => ConversationStore::open(data_root, project_id, &id).and_then(|child| {
                let branch = published_branch(owner, &child)?
                    .ok_or_else(|| invalid("候補会話の公開証拠がありません"))?;
                validate_published_branch_head_read_only(owner, &child, &branch)
            }),
            None => Err(invalid("巻き戻しworkspace証拠がありません")),
        };
        if let Err(error) = published {
            diagnostics.push(CandidateDiagnostic {
                id,
                reason: error.to_string(),
            });
        }
    }
    for id in transaction_ids {
        diagnostics.push(CandidateDiagnostic {
            id,
            reason: "候補transactionの子会話がありません".into(),
        });
    }
    diagnostics.sort_by(|left, right| left.id.cmp(&right.id));
    diagnostics
}

fn validate_published_branch_head_read_only(
    owner: &WorkspaceOwner,
    child: &ConversationStore,
    branch: &RewindBranchMetadataV1,
) -> io::Result<()> {
    let lock = child.lock()?;
    let snapshot = lock.snapshot()?;
    let expected = if snapshot.state.raw_commit_offset == branch.branch_initial_raw_offset
        && snapshot.state.raw_commit_hash == branch.branch_initial_raw_hash
        && snapshot.raw_offset == branch.branch_initial_raw_offset
        && snapshot.raw_hash == branch.branch_initial_raw_hash
    {
        Some(branch.workspace_pre_manifest_id.clone())
    } else {
        let binding = child.workspace_head_binding()?.filter(|binding| {
            binding.generation == snapshot.state.generation
                && binding.raw_commit_offset == snapshot.raw_offset
                && binding.raw_commit_hash == snapshot.raw_hash
        });
        if let Some(binding) = binding {
            Some(binding.workspace_manifest_id)
        } else {
            lock.list_rewind_boundaries()?
                .last()
                .and_then(|boundary| boundary.completion.as_ref())
                .filter(|completion| {
                    completion.completed_raw_offset == snapshot.raw_offset
                        && completion.completed_raw_hash == snapshot.raw_hash
                })
                .map(|completion| completion.workspace_post_manifest_id.clone())
        }
    }
    .ok_or_else(|| invalid("候補会話の現在headを作業ファイルへ結び付けられません"))?;
    let manifest: ManifestId = expected.parse().map_err(io::Error::other)?;
    if owner
        .matches_manifest(&manifest)
        .map_err(io::Error::other)?
    {
        Ok(())
    } else {
        Err(invalid("候補会話と現在の作業ファイルが一致しません"))
    }
}

/// Requires both branch publication and a workspace matching the current raw
/// head. Legacy first binding retains its explicit owner-controlled path.
pub fn validate_execution(
    owner: &WorkspaceOwner,
    persistence: &PersistedSession,
) -> io::Result<()> {
    let snapshot = persistence.snapshot()?;
    let branch = published_branch(owner, &persistence.store)?;
    let pending = owner
        .pending_legacy_binding_for(&snapshot.state.session_id)
        .map_err(io::Error::other)?;
    let binding = persistence.store.workspace_head_binding()?;
    let binding_is_stale = binding.as_ref().is_some_and(|binding| {
        binding.generation != snapshot.state.generation
            || binding.raw_commit_offset != snapshot.raw_offset
            || binding.raw_commit_hash != snapshot.raw_hash
    });
    let expected = if let Some(branch) = branch.as_ref()
        && snapshot.state.raw_commit_offset == branch.branch_initial_raw_offset
        && snapshot.state.raw_commit_hash == branch.branch_initial_raw_hash
        && snapshot.raw_offset == branch.branch_initial_raw_offset
        && snapshot.raw_hash == branch.branch_initial_raw_hash
    {
        Some(branch.workspace_pre_manifest_id.clone())
    } else {
        if let Some(binding) = binding.filter(|binding| {
            !pending
                && binding.generation == snapshot.state.generation
                && binding.raw_commit_offset == snapshot.raw_offset
                && binding.raw_commit_hash == snapshot.raw_hash
        }) {
            Some(binding.workspace_manifest_id)
        } else {
            persistence
                .list_rewind_boundaries()?
                .last()
                .and_then(|boundary| boundary.completion.as_ref())
                .filter(|completion| {
                    completion.completed_raw_offset == snapshot.raw_offset
                        && completion.completed_raw_hash == snapshot.raw_hash
                })
                .map(|completion| completion.workspace_post_manifest_id.clone())
        }
    };
    let expected = match expected {
        Some(expected) => expected,
        None if branch.is_some() => {
            return Err(invalid(
                "この会話の保存境界と現在のファイル状態を照合できません",
            ));
        }
        None if binding_is_stale && !pending => {
            return Err(invalid(
                "会話の保存境界と作業ファイルの結び付けが一致しません",
            ));
        }
        None => {
            let root = owner.project_root().to_path_buf();
            let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root])
                .map_err(io::Error::other)?;
            owner
                .bind_first_legacy_head(&policy, &snapshot.state.session_id, |manifest, permit| {
                    persistence.store.repair_pending_workspace_head_binding(
                        &snapshot,
                        &manifest.to_string(),
                        permit,
                    )
                })
                .map_err(io::Error::other)?
                .to_string()
        }
    };
    let manifest: ManifestId = expected.parse().map_err(io::Error::other)?;
    if owner
        .matches_manifest(&manifest)
        .map_err(io::Error::other)?
    {
        Ok(())
    } else {
        Err(invalid(
            "保存済み会話と現在の作業ファイルが一致しません。別の会話を選んでください",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rewind_workspace::RewindLimits;
    use crate::{
        conversation_state::RewindEffectStatusV1, conversation_state::RewindEligibilityV1,
    };
    use polaris_provider::Message;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn diagnostics_keep_unattributable_id_and_skip_normal_session() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let data_root = temp.path().join("data");
        fs::create_dir(&project).unwrap();
        fs::create_dir(&data_root).unwrap();
        let state = temp.path().join("state");
        fs::create_dir(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        let owner = WorkspaceOwner::open(
            &project,
            &state.join("rewind-workspace"),
            RewindLimits::default(),
        )
        .unwrap();
        let normal_id = "00000000-0000-4000-8000-000000000001";
        let normal = ConversationStateV2::new("project".into(), normal_id.into()).unwrap();
        ConversationStore::create(&data_root, normal).unwrap();
        let orphan_id = "00000000-0000-4000-8000-000000000002";
        fs::create_dir(data_root.join("sessions-v2").join(orphan_id)).unwrap();

        let diagnostics = candidate_diagnostics(&owner, &data_root, "project");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].id, orphan_id);
        assert!(diagnostics[0].reason.contains("state.json"));
    }

    #[test]
    fn diagnostics_without_workspace_keep_child_ids_and_create_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let data_root = temp.path().join("data");
        fs::create_dir(&data_root).unwrap();
        let normal_id = "00000000-0000-4000-8000-000000000021";
        ConversationStore::create(
            &data_root,
            ConversationStateV2::new("project".into(), normal_id.into()).unwrap(),
        )
        .unwrap();
        let child_id = "00000000-0000-4000-8000-000000000022";
        let child = ConversationStore::create(
            &data_root,
            ConversationStateV2::new("project".into(), child_id.into()).unwrap(),
        )
        .unwrap();
        let metadata_path = child.directory().join("rewind-branch-v1.json");
        fs::write(&metadata_path, b"{}").unwrap();
        let orphan_id = "00000000-0000-4000-8000-000000000023";
        fs::create_dir(data_root.join("sessions-v2").join(orphan_id)).unwrap();
        let before = fs::read(&metadata_path).unwrap();

        let diagnostics = candidate_diagnostics_without_workspace(&data_root, "project");
        assert_eq!(diagnostics.len(), 2);
        assert!(
            diagnostics
                .iter()
                .any(|item| { item.id == child_id && item.reason.contains("workspace証拠") })
        );
        assert!(
            diagnostics
                .iter()
                .any(|item| { item.id == orphan_id && item.reason.contains("state.json") })
        );
        assert_eq!(fs::read(&metadata_path).unwrap(), before);
        assert!(!temp.path().join("rewind-workspace").exists());
    }

    #[test]
    fn branch_publication_requires_committed_transaction() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let data_root = temp.path().join("data");
        fs::create_dir(&project).unwrap();
        fs::create_dir(&data_root).unwrap();
        fs::write(project.join("file.txt"), b"before").unwrap();
        let state = temp.path().join("state");
        fs::create_dir(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        let owner = WorkspaceOwner::open(
            &project,
            &state.join("rewind-workspace"),
            RewindLimits::default(),
        )
        .unwrap();
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[project.clone()]).unwrap();
        let pre = owner.capture(&policy).unwrap();
        let source_id = "00000000-0000-4000-8000-000000000010";
        let source = ConversationStore::create(
            &data_root,
            ConversationStateV2::new("project".into(), source_id.into()).unwrap(),
        )
        .unwrap();
        let mut source_lock = source.lock().unwrap();
        let boundary = source_lock
            .begin_rewind_boundary(pre.to_string(), vec![])
            .unwrap();
        source_lock.append(Message::user("turn"), true).unwrap();
        source_lock
            .append(Message::assistant("answer"), false)
            .unwrap();
        let snapshot = source_lock.snapshot().unwrap();
        source_lock
            .publish(&snapshot, snapshot.state.clone())
            .unwrap();
        fs::write(project.join("file.txt"), b"after").unwrap();
        let post = owner.capture(&policy).unwrap();
        source_lock
            .finish_rewind_boundary(
                &boundary.boundary_id,
                post.to_string(),
                RewindEligibilityV1::Eligible,
                RewindEffectStatusV1::NoExternalMutation,
            )
            .unwrap();
        let rejected_id = "00000000-0000-4000-8000-000000000011";
        let rejected = source_lock
            .prepare_rewind_branch(&data_root, &boundary.boundary_id, rejected_id)
            .unwrap();
        drop(source_lock);
        assert!(published_branch(&owner, &rejected).is_err());
        assert!(
            published_branch_bounded(&owner, &rejected, &mut PublicationReadBudgetV1::default())
                .is_err()
        );
        let restored = owner.restore_for_branch(&post, &pre, rejected_id).unwrap();
        assert!(published_branch(&owner, &rejected).is_err());
        owner.rollback(&restored.transaction_id).unwrap();
        assert!(published_branch(&owner, &rejected).is_err());
        assert!(
            candidate_diagnostics(&owner, &data_root, "project")
                .iter()
                .any(|diagnostic| diagnostic.id == rejected_id)
        );

        let accepted_id = "00000000-0000-4000-8000-000000000012";
        let accepted = source
            .lock()
            .unwrap()
            .prepare_rewind_branch(&data_root, &boundary.boundary_id, accepted_id)
            .unwrap();
        let restored = owner.restore_for_branch(&post, &pre, accepted_id).unwrap();
        owner.commit(&restored.transaction_id).unwrap();
        let active_execution = owner.try_activity_quiescent().unwrap();
        assert_eq!(
            published_branch(&owner, &accepted)
                .unwrap()
                .unwrap()
                .origin_session_id,
            source_id
        );
        drop(active_execution);
        let bounded =
            published_branch_bounded(&owner, &accepted, &mut PublicationReadBudgetV1::default())
                .unwrap()
                .unwrap();
        assert_eq!(bounded.origin_session_id, source_id);
        let mut file_limit = PublicationReadBudgetV1::default();
        file_limit.max_file_bytes = 1;
        assert!(published_branch_bounded(&owner, &accepted, &mut file_limit).is_err());
        assert!(file_limit.evidence_read_failed());
        let mut entry_limit = PublicationReadBudgetV1::default();
        entry_limit.max_entries = 0;
        assert!(published_branch_bounded(&owner, &accepted, &mut entry_limit).is_err());
        assert!(entry_limit.evidence_read_failed());
        let diagnostics = candidate_diagnostics(&owner, &data_root, "project");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.id == rejected_id)
        );
        assert!(
            !diagnostics
                .iter()
                .any(|diagnostic| diagnostic.id == accepted_id)
        );
        fs::write(project.join("file.txt"), b"external").unwrap();
        let branch = published_branch(&owner, &accepted).unwrap().unwrap();
        assert!(validate_published_branch_head_read_only(&owner, &accepted, &branch).is_err());
    }
}
