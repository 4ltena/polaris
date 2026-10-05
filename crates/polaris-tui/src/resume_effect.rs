//! Exact saved-session selection before a host activates a prepared candidate.
//! The existing project lease and effect journal retain hashes of both identities.

#[cfg(test)]
use crate::sessions::parse_legacy_messages;
use crate::{
    persist,
    sessions::{SessionListLimits, read_bounded_session_file},
};
use polaris_core::{
    management_effect::{
        ManagementDenialReasonV1, ManagementEffectHashV1, ManagementEffectScopeV1,
        ManagementEffectStateV1, ManagementEffectStoreV1, ManagementInventoryIdV1,
        ManagementOperationIdV1, ManagementPermissionScopeV1, ManagementRewindDispositionV1,
        ManagementUnknownReasonV1, PrepareManagementEffectOutcomeV1, PrepareManagementEffectV1,
        PreparedManagementEffectV1,
    },
    project_mutation::ProjectMutationLeaseV1,
    session_store::PersistedSession,
};
use serde::Serialize;
use std::path::Path;

#[derive(Clone, Copy, Debug, Serialize)]
pub enum ResumeInventoryV1 {
    D07,
    #[cfg(test)]
    F17,
}
impl ResumeInventoryV1 {
    fn id(self) -> &'static str {
        match self {
            Self::D07 => "D07",
            #[cfg(test)]
            Self::F17 => "F17",
        }
    }
}

#[derive(Clone, Copy)]
pub enum ResumeSubjectV1<'a> {
    Persisted(&'a PersistedSession),
    #[cfg(test)]
    Legacy {
        path: &'a Path,
        project_id: &'a str,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ResumeIdentityV1 {
    project: ManagementEffectHashV1,
    session: ManagementEffectHashV1,
    kind: u8,
    epoch: u64,
    generation: u64,
    saved_state: ManagementEffectHashV1,
    raw: ManagementEffectHashV1,
}

impl ResumeIdentityV1 {
    /// Bind the exact preimage/postimage returned by the existing workflow CAS.
    pub(crate) fn published_snapshot(
        snapshot: &polaris_core::conversation_state::ConversationSnapshot,
    ) -> Result<Self, String> {
        Ok(Self {
            project: ManagementEffectHashV1::digest(snapshot.state.project_id.as_bytes()),
            session: ManagementEffectHashV1::digest(snapshot.state.session_id.as_bytes()),
            kind: 1,
            epoch: snapshot.state.epoch,
            generation: snapshot.state.generation,
            saved_state: ManagementEffectHashV1::digest(
                &serde_json::to_vec(&snapshot.state).map_err(|e| e.to_string())?,
            ),
            raw: ManagementEffectHashV1::from_hex(&snapshot.raw_hash).map_err(|e| e.to_string())?,
        })
    }
}

impl ResumeSubjectV1<'_> {
    /// Full owner validation is kept; bounded descriptor reads enclose the same
    /// writer lock and bind the validator result to exactly those saved bytes.
    pub fn identity(self) -> Result<ResumeIdentityV1, String> {
        let limits = SessionListLimits::default();
        let mut total = 0;
        match self {
            Self::Persisted(persistence) => {
                let lock = persistence.store.lock().map_err(|e| e.to_string())?;
                let directory = persistence.store.directory();
                let state = read_bounded_session_file(
                    &directory.join("state.json"),
                    limits.max_file_bytes,
                    &mut total,
                    limits.max_total_bytes,
                )
                .map_err(|e| e.to_string())?;
                let raw = read_bounded_session_file(
                    &directory.join("raw.jsonl"),
                    limits.max_file_bytes,
                    &mut total,
                    limits.max_total_bytes,
                )
                .map_err(|e| e.to_string())?;
                drop(lock);
                // Preserve the attached persistence cursor and tombstone checks.
                let snapshot = persistence.snapshot().map_err(|e| e.to_string())?;
                let parsed: polaris_core::conversation_state::ConversationStateV2 =
                    serde_json::from_slice(&state).map_err(|e| e.to_string())?;
                if snapshot.state != parsed
                    || snapshot.raw_hash != polaris_core::conversation_state::content_hash(&raw)
                    || snapshot.raw_offset != raw.len() as u64
                {
                    return Err("選択した保存セッションが変更されました".into());
                }
                let project = persist::project_identity(Path::new(&snapshot.state.project_id))
                    .map_err(|e| e.to_string())?;
                if project != snapshot.state.project_id
                    || directory.file_name().and_then(|n| n.to_str())
                        != Some(snapshot.state.session_id.as_str())
                {
                    return Err("選択したセッションのプロジェクトまたはIDが一致しません".into());
                }
                Ok(ResumeIdentityV1 {
                    project: ManagementEffectHashV1::digest(project.as_bytes()),
                    session: ManagementEffectHashV1::digest(snapshot.state.session_id.as_bytes()),
                    kind: 1,
                    epoch: snapshot.state.epoch,
                    generation: snapshot.state.generation,
                    saved_state: ManagementEffectHashV1::digest(&state),
                    raw: ManagementEffectHashV1::digest(&raw),
                })
            }
            #[cfg(test)]
            Self::Legacy { path, project_id } => {
                let project =
                    persist::project_identity(Path::new(project_id)).map_err(|e| e.to_string())?;
                if project != project_id
                    || path.extension().and_then(|s| s.to_str()) != Some("jsonl")
                {
                    return Err("旧セッションのプロジェクトまたは保存形式が一致しません".into());
                }
                let id = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .ok_or("旧セッションIDが不正です")?;
                polaris_memory::management::validate_session_id(id).map_err(|e| e.to_string())?;
                let meta = read_bounded_session_file(
                    &path.with_extension("meta.json"),
                    64 * 1024,
                    &mut total,
                    limits.max_total_bytes,
                )
                .map_err(|e| e.to_string())?;
                let parsed: persist::SessionMeta = serde_json::from_slice(&meta)
                    .map_err(|_| "旧セッションのメタデータが不正です")?;
                let saved_project =
                    persist::project_identity(Path::new(&parsed.cwd)).map_err(|e| e.to_string())?;
                if saved_project != project {
                    return Err("別プロジェクトの保存セッションは選択できません".into());
                }
                let raw = read_bounded_session_file(
                    path,
                    limits.max_file_bytes,
                    &mut total,
                    limits.max_total_bytes,
                )
                .map_err(|e| e.to_string())?;
                parse_legacy_messages(&raw, limits.max_messages).map_err(|e| e.to_string())?;
                Ok(ResumeIdentityV1 {
                    project: ManagementEffectHashV1::digest(project.as_bytes()),
                    session: ManagementEffectHashV1::digest(id.as_bytes()),
                    kind: 0,
                    epoch: 0,
                    generation: 0,
                    saved_state: ManagementEffectHashV1::digest(&meta),
                    raw: ManagementEffectHashV1::digest(&raw),
                })
            }
        }
    }
}

pub struct PreparedResumeSelectionV1 {
    from: Option<ResumeIdentityV1>,
    to: ResumeIdentityV1,
    effect: PreparedManagementEffectV1,
}
pub struct ResumeSelectionOwnerV1<'a> {
    pub effects: &'a ManagementEffectStoreV1,
    pub lease: &'a ProjectMutationLeaseV1,
}

impl ResumeSelectionOwnerV1<'_> {
    pub fn prepare(
        &self,
        inventory: ResumeInventoryV1,
        previous: Option<ResumeSubjectV1<'_>>,
        selected: ResumeSubjectV1<'_>,
    ) -> Result<PreparedResumeSelectionV1, String> {
        if self.lease.store_root() != self.effects.state_dir().join("rewind-workspace") {
            return Err("セッション選択のproject leaseが一致しません".into());
        }
        let from = previous.map(ResumeSubjectV1::identity).transpose()?;
        let to = selected.identity()?;
        if to.project != self.effects.project_scope_hash()
            || from.as_ref().is_some_and(|from| from.project != to.project)
        {
            return Err("セッション選択のプロジェクトが一致しません".into());
        }
        let request_hash = ManagementEffectHashV1::digest(
            &serde_json::to_vec(&(inventory, &from, &to)).map_err(|e| e.to_string())?,
        );
        let request = PrepareManagementEffectV1 {
            operation_id: ManagementOperationIdV1::random().map_err(|e| e.to_string())?,
            inventory: ManagementInventoryIdV1::parse(inventory.id()).map_err(|e| e.to_string())?,
            scope: ManagementEffectScopeV1 {
                project: to.project,
                session: Some(to.session),
                generation: to.epoch,
                revision: to.generation,
                permission: ManagementPermissionScopeV1::LocalSession,
                account_binding: None,
            },
            request_hash,
            rewind: ManagementRewindDispositionV1::IrreversibleLocal,
        };
        let effect = match self
            .effects
            .prepare(
                self.lease,
                self.effects.cursor().map_err(|e| e.to_string())?,
                request,
            )
            .map_err(|e| e.to_string())?
        {
            PrepareManagementEffectOutcomeV1::Prepared(effect) => effect,
            PrepareManagementEffectOutcomeV1::Existing(_) => {
                return Err("既存のセッション選択結果は未確定です".into());
            }
        };
        Ok(PreparedResumeSelectionV1 { from, to, effect })
    }

    /// A caller has already restored and validated the candidate workspace.
    /// Activate only that exact candidate while this EX lease remains held.
    pub fn execute(
        &self,
        prepared: &PreparedResumeSelectionV1,
        previous: Option<ResumeSubjectV1<'_>>,
        selected: ResumeSubjectV1<'_>,
        accepted: bool,
        activate: impl FnOnce() -> Result<(), String>,
    ) -> Result<bool, String> {
        if !accepted {
            self.effects
                .deny(
                    self.lease,
                    &prepared.effect,
                    ManagementDenialReasonV1::UserDeclined,
                )
                .map_err(|e| e.to_string())?;
            return Ok(false);
        }
        let unchanged = (|| {
            Ok::<_, String>(
                previous.map(ResumeSubjectV1::identity).transpose()? == prepared.from
                    && selected.identity()? == prepared.to,
            )
        })();
        match unchanged {
            Ok(true) => {}
            Ok(false) => {
                self.effects
                    .deny(
                        self.lease,
                        &prepared.effect,
                        ManagementDenialReasonV1::StaleRequest,
                    )
                    .map_err(|e| e.to_string())?;
                return Err("選択したセッションまたは現在の会話が変更されました".into());
            }
            Err(error) => {
                self.effects
                    .deny(
                        self.lease,
                        &prepared.effect,
                        ManagementDenialReasonV1::InvalidState,
                    )
                    .map_err(|e| e.to_string())?;
                return Err(error);
            }
        }
        let started = self
            .effects
            .start(self.lease, &prepared.effect)
            .map_err(|e| e.to_string())?;
        if let Err(error) = activate() {
            self.effects
                .mark_outcome_unknown(
                    self.lease,
                    &started,
                    ManagementUnknownReasonV1::UnclassifiedEffect,
                )
                .map_err(|e| e.to_string())?;
            return Err(error);
        }
        let evidence = ManagementEffectHashV1::digest(
            &serde_json::to_vec(&(
                prepared.effect.operation_id(),
                prepared.effect.request().request_hash,
                &prepared.from,
                &prepared.to,
            ))
            .map_err(|e| e.to_string())?,
        );
        if let Err(error) = self
            .effects
            .complete_started(self.lease, &started, evidence)
        {
            if self.effects.effect(prepared.effect.operation_id()).map_err(|e|e.to_string())?.is_some_and(|effect|matches!(effect.state,ManagementEffectStateV1::Completed {evidence_hash} if evidence_hash==evidence)) {return Ok(true);}
            self.effects
                .mark_outcome_unknown(
                    self.lease,
                    &started,
                    ManagementUnknownReasonV1::ReceiptPersistenceFailed,
                )
                .map_err(|e| e.to_string())?;
            return Err(format!(
                "セッション選択の結果は不明です。再選択前に管理操作を確認してください: {error}"
            ));
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_core::rewind_workspace::{RewindLimits, WorkspaceOwner};
    use polaris_provider::Message;
    #[test]
    fn m3c_resume_selection_records_exact_normal_and_rejects_stale_or_foreign() {
        const CHILD: &str = "POLARIS_M3C_RESUME_EFFECT_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let home = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
            let output=std::process::Command::new(std::env::current_exe().unwrap()).args(["resume_effect::tests::m3c_resume_selection_records_exact_normal_and_rejects_stale_or_foreign","--exact","--nocapture"]).env(CHILD,"1").env("HOME",home.path()).env_remove("POLARIS_DATA_DIR").output().unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
        polaris_auth::protection::use_trusted_home(&home).unwrap();
        let project = home.join("project");
        std::fs::create_dir(&project).unwrap();
        let sessions = home.join(".polaris/sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let state = polaris_core::project::state_dir(&project).unwrap();
        let workspace = WorkspaceOwner::for_project(&project, RewindLimits::default()).unwrap();
        let lease = workspace.try_mutation().unwrap();
        let effects = ManagementEffectStoreV1::open_at_state_dir(&project, &state, &lease).unwrap();
        let make = |id: &str| {
            let path = sessions.join(format!("{id}.jsonl"));
            persist::write_meta_if_absent(
                &path.with_extension("meta.json"),
                &persist::SessionMeta {
                    cwd: project.to_str().unwrap().into(),
                    started_at_millis: 1,
                },
            )
            .unwrap();
            persist::append_message(&path, &Message::user("saved body")).unwrap();
            path
        };
        let old = make("old");
        let target = make("target");
        let old_subject = ResumeSubjectV1::Legacy {
            path: &old,
            project_id: project.to_str().unwrap(),
        };
        let selected_subject = ResumeSubjectV1::Legacy {
            path: &target,
            project_id: project.to_str().unwrap(),
        };
        let owner = ResumeSelectionOwnerV1 {
            effects: &effects,
            lease: &lease,
        };
        let prepared = owner
            .prepare(ResumeInventoryV1::D07, Some(old_subject), selected_subject)
            .unwrap();
        let activated = std::cell::Cell::new(false);
        assert!(
            owner
                .execute(&prepared, Some(old_subject), selected_subject, true, || {
                    assert!(matches!(
                        effects
                            .effect(prepared.effect.operation_id())
                            .unwrap()
                            .unwrap()
                            .state,
                        ManagementEffectStateV1::EffectStarted
                    ));
                    activated.set(true);
                    Ok(())
                })
                .unwrap()
        );
        assert!(activated.get());
        assert!(effects.blocker().unwrap().is_none());
        let stale = owner
            .prepare(ResumeInventoryV1::F17, Some(old_subject), selected_subject)
            .unwrap();
        persist::append_message(&target, &Message::assistant("changed after selection")).unwrap();
        assert!(
            owner
                .execute(
                    &stale,
                    Some(old_subject),
                    selected_subject,
                    true,
                    || panic!("stale target activated")
                )
                .is_err()
        );
        assert!(effects.blocker().unwrap().is_none());
        let foreign = home.join("foreign");
        std::fs::create_dir(&foreign).unwrap();
        assert!(
            owner
                .prepare(
                    ResumeInventoryV1::D07,
                    Some(old_subject),
                    ResumeSubjectV1::Legacy {
                        path: &target,
                        project_id: foreign.to_str().unwrap()
                    }
                )
                .is_err()
        );
    }
}
