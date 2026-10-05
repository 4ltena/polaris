//! Durable next-run instructions in the existing TUI admission family.
//! A live handle is never reconstructed at restart; recovery does not resend.
use std::collections::BTreeMap;
const MAX_INSTRUCTION_BYTES: usize = 4096; // Existing SavedInstruction acceptance bound.
use polaris_core::{
    conversation_state::ConversationSnapshot,
    desktop_store::SessionManagerStore,
    management_effect::{
        ManagementEffectHashV1, ManagementEffectScopeV1, ManagementEffectStateV1,
        ManagementEffectStoreV1, ManagementInventoryIdV1, ManagementOperationIdV1,
        ManagementPermissionScopeV1, ManagementRewindDispositionV1, ManagementUnknownReasonV1,
        PrepareManagementEffectOutcomeV1, PrepareManagementEffectV1, StartedManagementEffectV1,
    },
    project_mutation::ProjectActivityLeaseV1,
    session::Session,
    session_manager::{
        AdmissionId, AdmissionState, AdmissionV1, DestinationMode, NextRunnableV1, RunId,
        SessionId, SessionManagerV1, SubmissionPayloadV1, SubmitOutcomeV1, SubmitRequestV1,
    },
    session_store::PersistedSession,
};

/// Process-local proof of an explicitly accepted queued instruction. The text
/// is durable in SessionManager; this non-Clone handle is consumed at dispatch.
#[derive(Debug)]
pub struct QueuedInstructionV1 {
    admission_id: AdmissionId,
    parent_session: SessionId,
    payload_hash: String,
    text: String,
}

impl QueuedInstructionV1 {
    pub fn admission_id(&self) -> &AdmissionId {
        &self.admission_id
    }
    pub fn parent_session(&self) -> &SessionId {
        &self.parent_session
    }
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// The caller starts the real agent only after this durable reservation.
#[derive(Debug)]
pub struct AppliedInstructionV1 {
    pub admission_id: AdmissionId,
    pub session_id: SessionId,
    pub run_id: RunId,
}

fn handle(admission: &AdmissionV1) -> QueuedInstructionV1 {
    QueuedInstructionV1 {
        admission_id: admission.id.clone(),
        parent_session: admission.parent_session.clone(),
        payload_hash: admission.payload_hash.clone(),
        text: admission.payload.text.clone(),
    }
}

fn require_handle(
    manager: &SessionManagerV1,
    instruction: &QueuedInstructionV1,
) -> Result<AdmissionV1, String> {
    let admission = manager
        .status(&instruction.admission_id)
        .map_err(|e| e.to_string())?
        .clone();
    if admission.parent_session != instruction.parent_session
        || admission.payload_hash != instruction.payload_hash
        || admission.payload.text != instruction.text
        || admission.payload.route != DestinationMode::Current
    {
        return Err("指示のID・本文・保存先が受理時と一致しません".into());
    }
    Ok(admission)
}

fn prepare_effect(
    effects: &ManagementEffectStoreV1,
    activity: &ProjectActivityLeaseV1,
    operation: ManagementOperationIdV1,
    inventory: &str,
    snapshot: &ConversationSnapshot,
    request_hash: ManagementEffectHashV1,
) -> Result<StartedManagementEffectV1, String> {
    let prepared = effects
        .prepare_runtime(
            activity,
            effects.cursor().map_err(|e| e.to_string())?,
            PrepareManagementEffectV1 {
                operation_id: operation,
                inventory: ManagementInventoryIdV1::parse(inventory).map_err(|e| e.to_string())?,
                scope: ManagementEffectScopeV1 {
                    project: effects.project_scope_hash(),
                    session: Some(ManagementEffectHashV1::digest(
                        snapshot.state.session_id.as_bytes(),
                    )),
                    generation: snapshot.state.generation,
                    revision: snapshot.state.generation,
                    permission: ManagementPermissionScopeV1::LocalSession,
                    account_binding: None,
                },
                request_hash,
                rewind: ManagementRewindDispositionV1::IrreversibleLocal,
            },
        )
        .map_err(|e| e.to_string())?;
    let PrepareManagementEffectOutcomeV1::Prepared(prepared) = prepared else {
        return Err("この指示操作は既に保存されています。自動再実行しません".into());
    };
    effects
        .start_runtime(activity, &prepared)
        .map_err(|e| e.to_string())
}

fn receipt_counts(
    manager: &SessionManagerV1,
    persistence: &PersistedSession,
    snapshot: &ConversationSnapshot,
) -> Result<BTreeMap<SessionId, usize>, String> {
    let mut counts = BTreeMap::new();
    for id in manager.sessions.keys() {
        let count = if id.as_str() == snapshot.state.session_id {
            snapshot.state.admission_receipts.len()
        } else {
            match PersistedSession::open(
                &persistence.data_root,
                &persistence.database,
                &snapshot.state.project_id,
                id.as_str(),
            ) {
                Ok(saved) => saved
                    .snapshot()
                    .map_err(|e| e.to_string())?
                    .state
                    .admission_receipts
                    .len(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
                Err(error) => return Err(error.to_string()),
            }
        };
        counts.insert(id.clone(), count);
    }
    Ok(counts)
}

/// Saves acceptance before returning a visible acknowledgement. The command
/// targets the captured current session and is applied at its next run boundary.
pub fn receive(
    store: &SessionManagerStore,
    manager: &mut SessionManagerV1,
    persistence: &PersistedSession,
    effects: &ManagementEffectStoreV1,
    activity: &ProjectActivityLeaseV1,
    operation: ManagementOperationIdV1,
    text: &str,
) -> Result<QueuedInstructionV1, String> {
    if text.trim().is_empty()
        || text.len() > MAX_INSTRUCTION_BYTES
        || text.contains('\0')
        || crate::is_secret_command(text)
    {
        return Err("指示は秘密コマンドを含まない1〜4096 bytesの本文です".into());
    }
    let snapshot = persistence.snapshot().map_err(|e| e.to_string())?;
    let current = SessionId::new(snapshot.state.session_id.clone()).map_err(|e| e.to_string())?;
    let id = AdmissionId::new(format!("instruction-{}", operation.as_hex()))
        .map_err(|e| e.to_string())?;
    if let Some(existing) = manager.admissions.get(&id) {
        if existing.parent_session != current
            || existing.payload.text != text
            || existing.payload.route != DestinationMode::Current
        {
            return Err("同じ指示IDが別の本文・会話に使われています".into());
        }
        let effect = effects
            .effect(operation)
            .map_err(|e| e.to_string())?
            .ok_or("指示の管理効果を確認できません")?;
        if !matches!(effect.state, ManagementEffectStateV1::Completed { .. })
            || effect.request.inventory.encoded() != "D21"
            || effect.request.scope.session
                != Some(ManagementEffectHashV1::digest(current.as_str().as_bytes()))
            || effect.request.request_hash
                != ManagementEffectHashV1::digest(existing.payload_hash.as_bytes())
            || !matches!(existing.state, AdmissionState::Reserved)
        {
            return Err("この指示は処理済みか受付結果が不明です。自動再実行しません".into());
        }
        return Ok(handle(existing));
    }
    let goal_revision = snapshot
        .state
        .goal
        .as_ref()
        .map_or(0, |goal| goal.revision.get());
    let mut next = manager.clone();
    let before = manager.revision;
    next.observe_session_revisions(
        &current,
        next.revision,
        snapshot.state.generation,
        goal_revision,
    )
    .map_err(|e| e.to_string())?;
    let SubmitOutcomeV1::Accepted { admission, .. } = next
        .submit(
            SubmitRequestV1 {
                parent_session: current.clone(),
                payload: SubmissionPayloadV1 {
                    text: text.to_owned(),
                    attachment_ids: Vec::new(),
                    route: DestinationMode::Current,
                    expected_session_revision: snapshot.state.generation,
                    expected_goal_revision: goal_revision,
                },
                admission_id: Some(id),
            },
            next.revision,
        )
        .map_err(|e| e.to_string())?;
    let counts = receipt_counts(&next, persistence, &snapshot)?;
    next.check_receipt_capacity(&counts)
        .map_err(|e| e.to_string())?;
    let started = prepare_effect(
        effects,
        activity,
        operation,
        "D21",
        &snapshot,
        ManagementEffectHashV1::digest(admission.payload_hash.as_bytes()),
    )?;
    if let Err(error) = store.persist_observed_submission(
        before,
        &next,
        &counts,
        &current,
        snapshot.state.generation,
        goal_revision,
    ) {
        effects
            .mark_outcome_unknown_runtime(
                activity,
                &started,
                ManagementUnknownReasonV1::ReceiptPersistenceFailed,
            )
            .map_err(|e| e.to_string())?;
        return Err(format!(
            "指示の受付結果は不明です。再送せず保存状態を確認してください: {error}"
        ));
    }
    *manager = next;
    let receipt = serde_json::to_vec(&(
        admission.id.as_str(),
        &admission.payload_hash,
        manager.revision,
    ))
    .map_err(|e| e.to_string())?;
    if let Err(error) = effects.complete_started_runtime(
        activity,
        &started,
        ManagementEffectHashV1::digest(&receipt),
    ) {
        let _ = effects.mark_outcome_unknown_runtime(
            activity,
            &started,
            ManagementUnknownReasonV1::ReceiptPersistenceFailed,
        );
        return Err(format!(
            "指示の受付receiptを完了できません。再送しません: {error}"
        ));
    }
    Ok(handle(&admission))
}

fn persist_transition(
    store: &SessionManagerStore,
    manager: &mut SessionManagerV1,
    next: SessionManagerV1,
) -> Result<(), String> {
    store
        .persist(manager.revision, &next)
        .map_err(|e| e.to_string())?;
    *manager = next;
    Ok(())
}

/// Consumes a live accepted handle. It does not recover a crashed run or create
/// a second admission; provider dispatch remains the caller's next boundary.
/// Checks queue ownership before installing turn authority or publishing a rewind boundary.
pub fn preflight(
    manager: &SessionManagerV1,
    session: &Session,
    instruction: &QueuedInstructionV1,
) -> Result<(), String> {
    let persistence = session
        .persistence
        .as_ref()
        .ok_or("指示の適用にはv2会話が必要です")?;
    let snapshot = persistence.snapshot().map_err(|e| e.to_string())?;
    if snapshot.state.session_id != instruction.parent_session.as_str() {
        return Err("指示を別の会話へ適用できません".into());
    }
    let admission = require_handle(manager, &instruction)?;
    if !matches!(admission.state, AdmissionState::Reserved) {
        return Err("この指示は適用済みか復旧が必要です。再送しません".into());
    }
    if manager.queue.first() != Some(&instruction.admission_id)
        || manager
            .sessions
            .get(&instruction.parent_session)
            .is_none_or(|s| s.active_run.is_some())
        || manager
            .outcome_unknown_blocker(&instruction.parent_session)
            .is_some()
    {
        return Err("先行入力・実行中run・結果不明の処理後に指示を適用してください".into());
    }
    Ok(())
}

pub fn apply(
    store: &SessionManagerStore,
    manager: &mut SessionManagerV1,
    session: &mut Session,
    instruction: QueuedInstructionV1,
) -> Result<AppliedInstructionV1, String> {
    preflight(manager, session, &instruction)?;
    let persistence = session
        .persistence
        .as_ref()
        .ok_or("指示の適用にはv2会話が必要です")?;
    let snapshot = persistence.snapshot().map_err(|e| e.to_string())?;
    if snapshot.state.session_id != instruction.parent_session.as_str() {
        return Err("指示を別の会話へ適用できません".into());
    }
    let admission = require_handle(manager, &instruction)?;
    if !matches!(admission.state, AdmissionState::Reserved) {
        return Err("この指示は適用済みか復旧が必要です。再送しません".into());
    }
    if manager.queue.first() != Some(&instruction.admission_id)
        || manager
            .sessions
            .get(&instruction.parent_session)
            .is_none_or(|s| s.active_run.is_some())
        || manager
            .outcome_unknown_blocker(&instruction.parent_session)
            .is_some()
    {
        return Err("先行入力・実行中run・結果不明の処理後に指示を適用してください".into());
    }
    let goal_revision = snapshot
        .state
        .goal
        .as_ref()
        .map_or(0, |goal| goal.revision.get());
    let mut next = manager.clone();
    next.observe_session_revisions(
        &instruction.parent_session,
        next.revision,
        snapshot.state.generation,
        goal_revision,
    )
    .map_err(|e| e.to_string())?;
    store
        .persist_observed(
            manager.revision,
            &next,
            &instruction.parent_session,
            snapshot.state.generation,
            goal_revision,
        )
        .map_err(|e| e.to_string())?;
    *manager = next;
    let mut next = manager.clone();
    let decided = if admission.payload.expected_session_revision != snapshot.state.generation
        || admission.payload.expected_goal_revision != goal_revision
    {
        next.decide_context_changed(&admission.id, next.revision, admission.revision)
    } else {
        next.decide(
            &admission.id,
            next.revision,
            admission.revision,
            None,
            false,
            false,
        )
    }
    .map_err(|e| e.to_string())?;
    persist_transition(store, manager, next)?;
    let mut next = manager.clone();
    let ready = next
        .prepare_destination(&admission.id, next.revision, decided.revision)
        .map_err(|e| e.to_string())?;
    persist_transition(store, manager, next)?;
    let appended = session
        .push_admitted_user(
            &instruction.text,
            &instruction.admission_id,
            &instruction.payload_hash,
        )
        .map_err(|e| e.to_string())?;
    if !appended {
        return Err("指示は原文へ既に保存されています。再送しません".into());
    }
    let mut next = manager.clone();
    let applied = next
        .record_input_appended(
            &admission.id,
            next.revision,
            ready.revision,
            &instruction.parent_session,
        )
        .map_err(|e| e.to_string())?;
    persist_transition(store, manager, next)?;
    let mut next = manager.clone();
    next.queue_run(&admission.id, next.revision, applied.revision)
        .map_err(|e| e.to_string())?;
    persist_transition(store, manager, next)?;
    let mut next = manager.clone();
    let NextRunnableV1::Start {
        admission_id,
        session_id,
        run_id,
    } = next.start_next(next.revision).map_err(|e| e.to_string())?
    else {
        return Err("指示のrunを予約できません".into());
    };
    if admission_id != instruction.admission_id || session_id != instruction.parent_session {
        return Err("指示と予約runのidentityが一致しません".into());
    }
    persist_transition(store, manager, next)?;
    Ok(AppliedInstructionV1 {
        admission_id,
        session_id,
        run_id,
    })
}

/// Cancels only this queued, unappended instruction, never a running root.
pub fn cancel_queue(
    store: &SessionManagerStore,
    manager: &mut SessionManagerV1,
    persistence: &PersistedSession,
    effects: &ManagementEffectStoreV1,
    activity: &ProjectActivityLeaseV1,
    instruction: QueuedInstructionV1,
) -> Result<(), String> {
    let admission = require_handle(manager, &instruction)?;
    if !matches!(admission.state, AdmissionState::Reserved) {
        return Err("実行・適用済みの指示はqueued取消の対象外です".into());
    }
    let snapshot = persistence.snapshot().map_err(|e| e.to_string())?;
    if snapshot.state.session_id != instruction.parent_session.as_str() {
        return Err("取消対象の会話が一致しません".into());
    }
    let mut next = manager.clone();
    next.cancel(&admission.id, next.revision, admission.revision)
        .map_err(|e| e.to_string())?;
    let operation = ManagementOperationIdV1::random().map_err(|e| e.to_string())?;
    let request = serde_json::to_vec(&(admission.id.as_str(), &admission.payload_hash))
        .map_err(|e| e.to_string())?;
    let started = prepare_effect(
        effects,
        activity,
        operation,
        "D10",
        &snapshot,
        ManagementEffectHashV1::digest(&request),
    )?;
    if let Err(error) = persist_transition(store, manager, next) {
        effects
            .mark_outcome_unknown_runtime(
                activity,
                &started,
                ManagementUnknownReasonV1::ReceiptPersistenceFailed,
            )
            .map_err(|e| e.to_string())?;
        return Err(error);
    }
    effects
        .complete_started_runtime(
            activity,
            &started,
            ManagementEffectHashV1::digest(
                &serde_json::to_vec(&(admission.id.as_str(), manager.revision, "queued_cancelled"))
                    .map_err(|e| e.to_string())?,
            ),
        )
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_core::{conversation_state::HistoryMode, rewind_workspace::WorkspaceOwner};
    use std::{fs, os::unix::fs::PermissionsExt};

    #[test]
    fn m3e_instruction_accept_apply_once_and_exact_session_denial() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let state = temp.path().join("state");
        let data = temp.path().join("data");
        for path in [&project, &state, &data] {
            fs::create_dir(path).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let workspace = WorkspaceOwner::open(
            &project,
            &state.join("rewind-workspace"),
            Default::default(),
        )
        .unwrap();
        let lease = workspace.try_mutation().unwrap();
        let effects = ManagementEffectStoreV1::open_at_state_dir(&project, &state, &lease).unwrap();
        drop(lease);
        let activity = workspace.try_activity_quiescent().unwrap();
        let project_id = crate::persist::project_identity(&project).unwrap();
        let persistence = PersistedSession::create(
            &data,
            &state.join("memory.sqlite3"),
            &project_id,
            HistoryMode::Legacy,
            None,
        )
        .unwrap();
        let snapshot = persistence.snapshot().unwrap();
        let current = SessionId::new(snapshot.state.session_id.clone()).unwrap();
        let (store, mut manager) = crate::open_tui_admission_owner(&state, &current).unwrap();
        let mut session = Session::new();
        session.attach(persistence.clone()).unwrap();
        session.push_user("前runの入力");
        let operation = ManagementOperationIdV1::random().unwrap();
        let instruction = receive(
            &store,
            &mut manager,
            &persistence,
            &effects,
            &activity,
            operation,
            "次のrunでは変更理由を示す",
        )
        .unwrap();
        let id = instruction.admission_id().clone();
        let received = store.load().unwrap().unwrap();
        assert_eq!(
            received.status(&id).unwrap().payload.text,
            instruction.text()
        );
        assert!(matches!(
            received.status(&id).unwrap().state,
            AdmissionState::Reserved
        ));
        assert!(
            persistence
                .snapshot()
                .unwrap()
                .state
                .admission_receipts
                .is_empty()
        );

        let exact = receive(
            &store,
            &mut manager,
            &persistence,
            &effects,
            &activity,
            operation,
            instruction.text(),
        )
        .unwrap();
        let foreign = PersistedSession::create(
            &data,
            &state.join("memory.sqlite3"),
            &project_id,
            HistoryMode::Legacy,
            None,
        )
        .unwrap();
        let mut foreign_session = Session::new();
        foreign_session.attach(foreign).unwrap();
        let before = manager.revision;
        assert!(apply(&store, &mut manager, &mut foreign_session, exact).is_err());
        assert_eq!(manager.revision, before);

        let cancelled = receive(
            &store,
            &mut manager,
            &persistence,
            &effects,
            &activity,
            ManagementOperationIdV1::random().unwrap(),
            "この指示は取消する",
        )
        .unwrap();
        let cancelled_id = cancelled.admission_id().clone();
        cancel_queue(
            &store,
            &mut manager,
            &persistence,
            &effects,
            &activity,
            cancelled,
        )
        .unwrap();
        assert!(matches!(
            manager.status(&cancelled_id).unwrap().state,
            AdmissionState::Terminal {
                status: polaris_core::session_manager::TerminalStatus::Cancelled,
                ..
            }
        ));

        // The previous turn may publish more raw events after acceptance.
        session.push_assistant("前のrunが完了した", Vec::new());
        let applied = apply(&store, &mut manager, &mut session, instruction).unwrap();
        assert_eq!(applied.admission_id, id);
        assert_eq!(applied.session_id, current);
        assert_eq!(applied.run_id, RunId::derive(&id));
        assert!(matches!(
            manager.status(&id).unwrap().state,
            AdmissionState::Running { .. }
        ));
        let snapshot = persistence.snapshot().unwrap();
        assert_eq!(snapshot.state.admission_receipts.len(), 1);
        assert_eq!(
            snapshot
                .events
                .iter()
                .filter(|event| event.message.content == "次のrunでは変更理由を示す")
                .count(),
            1
        );
        assert!(
            receive(
                &store,
                &mut manager,
                &persistence,
                &effects,
                &activity,
                operation,
                "次のrunでは変更理由を示す"
            )
            .is_err()
        );
        let before = manager.revision;
        let effect_count = effects.effects().unwrap().len();
        assert!(
            receive(
                &store,
                &mut manager,
                &persistence,
                &effects,
                &activity,
                ManagementOperationIdV1::random().unwrap(),
                "/mcp secret set value"
            )
            .is_err()
        );
        assert_eq!(manager.revision, before);
        assert_eq!(effects.effects().unwrap().len(), effect_count);
        assert!(effects.blocker().unwrap().is_none());
    }
}
