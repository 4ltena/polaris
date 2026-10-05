//! Commands over the live host control plane; no desktop-state mirror.
use polaris_core::{
    approval::ApprovalPolicy,
    management_effect::{
        ManagementDenialReasonV1, ManagementEffectHashV1, ManagementEffectScopeV1,
        ManagementEffectStoreV1, ManagementInventoryIdV1, ManagementOperationIdV1,
        ManagementPermissionScopeV1, ManagementRewindDispositionV1, ManagementUnknownReasonV1,
        PrepareManagementEffectOutcomeV1, PrepareManagementEffectV1,
    },
    project_mutation::ProjectActivityLeaseV1,
    session::{
        ChildControlProjectionV1, RootDelegationControlV1, RootDelegationProjectionV1,
        RuntimeCancelEffectV1,
    },
    subagent_control::{CancelResultV1, ChildAttemptRefV1, ChildStateV1},
};
use polaris_sandbox::SandboxPolicy;
use polaris_skills::AgentType;
use std::{sync::Arc, time::Duration};

const MAX_ROWS: usize = 128;
const MAX_OUTPUT: usize = 32 * 1024;
const OWNER_TIMEOUT: Duration = Duration::from_secs(2);

/// The host supplies the bound run activity lease. Only the restricted
/// runtime journal API accepts this proof; source mutation remains excluded.
pub struct RuntimeMutationContext<'a> {
    pub effects: &'a ManagementEffectStoreV1,
    pub lease: &'a ProjectActivityLeaseV1,
    pub session_id: &'a str,
    pub generation: u64,
    pub accepted: bool,
}

fn safe(value: &str, limit: usize) -> String {
    crate::render::sanitize(value)
        .chars()
        .filter(|c| !c.is_control())
        .take(limit)
        .collect()
}

fn bounded(lines: Vec<String>) -> Result<Vec<String>, String> {
    if lines.len() > MAX_ROWS + 4
        || lines
            .iter()
            .try_fold(0usize, |n, line| n.checked_add(line.len()))
            .is_none_or(|n| n > MAX_OUTPUT)
    {
        return Err("表示の件数またはサイズが上限を超えています".into());
    }
    Ok(lines)
}

async fn snapshot(control: &RootDelegationControlV1) -> Result<RootDelegationProjectionV1, String> {
    let snapshot = tokio::time::timeout(OWNER_TIMEOUT, control.snapshot())
        .await
        .map_err(|_| "subagent ownerの読取りが時間切れです".to_owned())?;
    if snapshot.children.len() > MAX_ROWS {
        return Err("subagentの件数が表示上限を超えています".into());
    }
    Ok(snapshot)
}

fn child_line(child: &ChildControlProjectionV1) -> String {
    let state = match &child.state {
        ChildStateV1::Queued => "queued",
        ChildStateV1::Running => "running",
        ChildStateV1::Waiting { .. } => "waiting",
        ChildStateV1::Cancelling => "cancelling",
        ChildStateV1::Succeeded => "succeeded",
        ChildStateV1::Failed { .. } => "failed",
        ChildStateV1::Cancelled => "cancelled",
        ChildStateV1::OutcomeUnknown => "outcome_unknown",
    };
    format!(
        "{} / {}: {} role={} tier={} model={} effort={}",
        safe(&child.logical_child_id.0, 128),
        safe(&child.attempt_id.0, 128),
        state,
        safe(&child.role, 128),
        safe(&child.tier, 64),
        safe(&child.model, 128),
        safe(&child.effort, 64)
    )
}

/// IDs name one exact logical child and attempt; a retry is a different target.
/// The owner's wait API is a cursor-bound snapshot, not an assertion that a
/// running attempt has finished.
pub async fn subagents(
    control: Option<&Arc<RootDelegationControlV1>>,
    command: &str,
    mutation: Option<RuntimeMutationContext<'_>>,
) -> Result<Vec<String>, String> {
    if command.len() > 1024 || command.chars().any(|c| c.is_control()) {
        return Err("subagents入力が不正です".into());
    }
    let words = command.split_whitespace().collect::<Vec<_>>();
    let verb = words.first().copied().unwrap_or("list");
    if !matches!(verb, "list" | "wait" | "cancel")
        || (verb == "list" && words.len() > 1)
        || (verb != "list" && words.len() != 3)
    {
        return Err("使い方: /subagents list | wait <child-id> <attempt-id> | cancel <child-id> <attempt-id>".into());
    }
    let Some(control) = control else {
        return if verb == "list" {
            Ok(vec!["現在のrunにsubagent ownerはありません".into()])
        } else {
            Err("現在のrunにsubagent ownerがないため実行できません".into())
        };
    };
    let projection = snapshot(control).await?;
    if verb == "list" {
        let mut lines = vec![format!(
            "run={} attempt={} revision={}",
            safe(&projection.root_run_id.0, 128),
            safe(&projection.root_attempt_id.0, 128),
            projection.revision
        )];
        lines.extend(projection.children.iter().map(child_line));
        if projection.children.is_empty() {
            lines.push("subagentはありません".into());
        }
        return bounded(lines);
    }
    let child = projection
        .children
        .iter()
        .find(|child| child.logical_child_id.0 == words[1] && child.attempt_id.0 == words[2])
        .ok_or_else(|| "このrunの現在のchild/attemptと一致しません".to_owned())?;
    let reference = ChildAttemptRefV1 {
        logical_child_id: child.logical_child_id.clone(),
        attempt_id: child.attempt_id.clone(),
    };
    let binding = control.host_binding();
    if verb == "wait" {
        let waited = tokio::time::timeout(
            OWNER_TIMEOUT,
            control.wait_children(&binding, projection.revision, None),
        )
        .await
        .map_err(|_| "subagent waitが時間切れです".to_owned())??;
        let waited = waited.ok_or_else(|| "subagentの状態を確認できません".to_owned())?;
        let selected = waited
            .children
            .iter()
            .find(|child| {
                child.logical_child_id == reference.logical_child_id
                    && child.attempt_id == reference.attempt_id
            })
            .ok_or_else(|| "child/attemptが変更されています".to_owned())?;
        return bounded(vec![
            format!("revision={} cursor={}", waited.revision, waited.cursor),
            child_line(selected),
        ]);
    }
    if child.state.is_terminal() {
        return bounded(vec![
            child_line(child),
            "このattemptは既に終端状態です".into(),
        ]);
    }
    let mutation =
        mutation.ok_or_else(|| "停止要求の保存ownerを取得できないため実行できません".to_owned())?;
    if mutation.session_id != binding.session_id {
        return Err("停止要求のsession bindingが一致しません".into());
    }
    let cursor = mutation
        .effects
        .cursor()
        .map_err(|error| error.to_string())?;
    let request_bytes = serde_json::to_vec(&(
        &binding.session_id,
        &binding.root_run_id.0,
        &binding.root_attempt_id.0,
        &binding.active_task_id.0,
        &reference.logical_child_id.0,
        &reference.attempt_id.0,
        projection.revision,
    ))
    .map_err(|_| "停止要求のidentityを符号化できません".to_owned())?;
    let prepared = mutation
        .effects
        .prepare_runtime(
            mutation.lease,
            cursor,
            PrepareManagementEffectV1 {
                operation_id: ManagementOperationIdV1::random()
                    .map_err(|_| "停止要求のIDを予約できません".to_owned())?,
                inventory: ManagementInventoryIdV1::parse("D12")
                    .map_err(|error| error.to_string())?,
                scope: ManagementEffectScopeV1 {
                    project: mutation.effects.project_scope_hash(),
                    session: Some(ManagementEffectHashV1::digest(
                        binding.session_id.as_bytes(),
                    )),
                    generation: mutation.generation,
                    revision: projection.revision,
                    permission: ManagementPermissionScopeV1::LocalSession,
                    account_binding: None,
                },
                request_hash: ManagementEffectHashV1::digest(&request_bytes),
                rewind: ManagementRewindDispositionV1::IrreversibleLocal,
            },
        )
        .map_err(|error| error.to_string())?;
    let PrepareManagementEffectOutcomeV1::Prepared(prepared) = prepared else {
        return Err("停止要求に既存の効果があるため再実行しません".into());
    };
    if !mutation.accepted {
        mutation
            .effects
            .deny_runtime(
                mutation.lease,
                &prepared,
                ManagementDenialReasonV1::UserDeclined,
            )
            .map_err(|error| error.to_string())?;
        return Ok(vec!["停止要求を取り消しました".into()]);
    }
    let started = mutation
        .effects
        .start_runtime(mutation.lease, &prepared)
        .map_err(|error| error.to_string())?;
    let result = tokio::time::timeout(
        OWNER_TIMEOUT,
        control.request_child_cancel(&binding, projection.revision, reference),
    )
    .await;
    let ack = match result {
        Ok(Ok(ack)) => ack,
        _ => {
            mutation
                .effects
                .mark_outcome_unknown_runtime(
                    mutation.lease,
                    &started,
                    ManagementUnknownReasonV1::InterruptedAfterStart,
                )
                .map_err(|error| error.to_string())?;
            return Err("停止要求の結果は不明です。自動再実行しません".into());
        }
    };
    if ack.runtime_effect == RuntimeCancelEffectV1::Unavailable
        && !matches!(&ack.result, CancelResultV1::AlreadyTerminal { .. })
    {
        mutation
            .effects
            .mark_outcome_unknown_runtime(
                mutation.lease,
                &started,
                ManagementUnknownReasonV1::DetachedEffect,
            )
            .map_err(|error| error.to_string())?;
        return Err(
            "停止要求は記録しましたが実行handleがありません。結果は不明で、自動再実行しません"
                .into(),
        );
    }
    let receipt = serde_json::to_vec(&(
        &request_bytes,
        ack.revision,
        format!("{:?}", ack.result),
        format!("{:?}", ack.runtime_effect),
    ))
    .map_err(|_| "停止要求のreceiptを符号化できません".to_owned())?;
    mutation
        .effects
        .complete_started_runtime(
            mutation.lease,
            &started,
            ManagementEffectHashV1::digest(&receipt),
        )
        .map_err(|error| error.to_string())?;
    let delivery = match ack.runtime_effect {
        RuntimeCancelEffectV1::Requested => {
            "このattemptへ停止要求を届けました。終了状態は /subagents wait で確認してください"
        }
        RuntimeCancelEffectV1::Unavailable => {
            "停止要求は記録しましたが、実行handleはありません。終了は確認できません"
        }
    };
    Ok(vec![
        format!("停止要求 revision={}", ack.revision),
        delivery.into(),
    ])
}

pub fn roles(roles: &[AgentType], command: &str) -> Result<Vec<String>, String> {
    if !matches!(command.trim(), "" | "list") {
        return Err("使い方: /roles list。role bindingの変更ownerは利用できません".into());
    }
    if roles.len() > MAX_ROWS {
        return Err("roleの件数が表示上限を超えています".into());
    }
    let mut lines = vec!["現在のrunが使用するrole定義".into()];
    lines.extend(roles.iter().map(|role| {
        format!(
            "{}: tier={}..{} access={:?} turns={} wall={}s",
            safe(&role.name, 128),
            role.tier_min,
            role.tier,
            role.access,
            role.max_turns,
            role.wall_seconds
        )
    }));
    bounded(lines)
}

/// Only the next StopTracker is changed. No live run, provider, account or
/// sandbox capability is rebound by this process-local setting.
pub fn run_settings(
    command: &str,
    next_max_turns: &mut u32,
    startup_max_turns: u32,
) -> Result<Vec<String>, String> {
    if command.len() > 1024 || command.chars().any(char::is_control) {
        return Err("run settings入力が不正です".into());
    }
    let words = command.split_whitespace().collect::<Vec<_>>();
    match words.as_slice() {
        [] | ["settings"] => {}
        ["settings", "max-turns", number] => {
            let number = number
                .parse::<u32>()
                .map_err(|_| "max-turnsは整数です".to_owned())?;
            if number == 0 || number > startup_max_turns {
                return Err(format!(
                    "max-turnsは1..={startup_max_turns}です。起動時上限を増やすには --max-turns で再起動してください"
                ));
            }
            *next_max_turns = number;
        }
        _ => return Err("使い方: /run settings [max-turns <N>]".into()),
    }
    Ok(vec![format!(
        "次のrun: max-turns={}（起動時上限={}）",
        next_max_turns, startup_max_turns
    )])
}

pub async fn run_details(
    control: Option<&Arc<RootDelegationControlV1>>,
) -> Result<Vec<String>, String> {
    subagents(control, "list", None).await
}

pub fn sandbox(
    command: &str,
    sandbox: &SandboxPolicy,
    approval: ApprovalPolicy,
) -> Result<Vec<String>, String> {
    if !matches!(command.trim(), "" | "status") {
        return Err(
            "起動時sandboxの変更はできません。--sandbox を指定して再起動してください".into(),
        );
    }
    let description = sandbox.describe();
    if description.len() > 8192 {
        return Err("起動時sandboxの表示がサイズ上限を超えています".into());
    }
    bounded(vec![
        format!("sandbox: {}", safe(&description, 8192)),
        format!("approval: {approval:?}"),
        "変更は新しい起動にのみ適用されます".into(),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_core::{
        session::RootDelegationConfigV1,
        subagent_control::{
            ApprovalCapabilityV1, CapabilitySnapshotV1, McpCapabilityV1, RootAttemptId, RootRunId,
            SandboxCapabilityV1, TaskId,
        },
    };

    fn config() -> RootDelegationConfigV1 {
        RootDelegationConfigV1 {
            session_id: "session-1".into(),
            root_run_id: RootRunId::new("run-1").unwrap(),
            root_attempt_id: RootAttemptId::new("attempt-1").unwrap(),
            active_task_id: TaskId::new("task-1").unwrap(),
            parent_capabilities: CapabilitySnapshotV1 {
                sandbox: SandboxCapabilityV1::ReadOnly,
                write_roots: vec![],
                tool_names: vec!["read".into()],
                approval: ApprovalCapabilityV1::Never,
                mcp: McpCapabilityV1::ReadSnapshotsOnly,
            },
            model: "test".into(),
            effort: "auto".into(),
        }
    }

    fn control() -> Arc<RootDelegationControlV1> {
        RootDelegationControlV1::new(config()).unwrap()
    }

    #[tokio::test]
    async fn m3e_live_subagent_scope_and_absent_owner_are_explicit() {
        let control = control();
        assert!(
            subagents(Some(&control), "list", None)
                .await
                .unwrap()
                .iter()
                .any(|s| s == "subagentはありません")
        );
        let denied = subagents(Some(&control), "cancel foreign attempt-2", None)
            .await
            .unwrap_err();
        assert!(denied.contains("一致しません"));
        assert_eq!(control.snapshot().await.revision, 0);
        assert!(subagents(None, "wait child attempt", None).await.is_err());
        assert!(subagents(None, "list", None).await.unwrap()[0].contains("ありません"));
        use polaris_core::{
            rewind_workspace::WorkspaceOwner,
            subagent_control::{
                ChildAttemptId, ChildBindingV1, ChildControlPlaneV1, LogicalChildId,
                ParentBindingV1, RegisterChildV1,
            },
        };
        use std::os::unix::fs::PermissionsExt;
        let config = config();
        let reference = ChildAttemptRefV1 {
            logical_child_id: LogicalChildId::new("selected-child").unwrap(),
            attempt_id: ChildAttemptId::new("selected-attempt").unwrap(),
        };
        let binding = ChildBindingV1 {
            root_run_id: config.root_run_id.clone(),
            root_attempt_id: config.root_attempt_id.clone(),
            active_task_id: config.active_task_id.clone(),
            parent: ParentBindingV1::Root,
        };
        let mut plane = ChildControlPlaneV1::default();
        plane
            .register_before_start(RegisterChildV1 {
                logical_child_id: reference.logical_child_id.clone(),
                attempt_id: reference.attempt_id.clone(),
                binding: binding.clone(),
                role: "file-reader".into(),
                tier: "dwarf".into(),
                model: "test".into(),
                effort: "auto".into(),
                capabilities: config.parent_capabilities.clone(),
            })
            .unwrap();
        plane.mark_running(&reference, &binding).unwrap();
        let live = RootDelegationControlV1::new_live_with_persistence_hook_for_test(
            config,
            plane,
            0,
            Arc::new(|_| Ok(())),
        )
        .unwrap();
        let abort = live
            .install_runtime_attempt_for_test(&reference)
            .await
            .unwrap();
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let state = root.path().join("state");
        for path in [&project, &state] {
            std::fs::create_dir(path).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let owner = WorkspaceOwner::open(
            &project,
            &state.join("rewind-workspace"),
            Default::default(),
        )
        .unwrap();
        let lease = owner.try_mutation().unwrap();
        let effects = ManagementEffectStoreV1::open_at_state_dir(&project, &state, &lease).unwrap();
        drop(lease);
        let activity = owner.try_activity().unwrap();
        let context = || RuntimeMutationContext {
            effects: &effects,
            lease: &activity,
            session_id: "session-1",
            generation: 1,
            accepted: true,
        };
        let before = live.snapshot().await;
        assert!(
            subagents(
                Some(&live),
                "cancel foreign selected-attempt",
                Some(context())
            )
            .await
            .unwrap_err()
            .contains("一致しません")
        );
        assert_eq!(live.snapshot().await, before);
        assert!(effects.effects().unwrap().is_empty());
        assert!(!abort.is_aborted());
        let lines = subagents(
            Some(&live),
            "cancel selected-child selected-attempt",
            Some(context()),
        )
        .await
        .unwrap();
        assert!(
            lines
                .iter()
                .any(|line| line.contains("停止要求を届けました"))
        );
        assert!(abort.is_aborted());
        let after = live.snapshot().await;
        assert_eq!(after.revision, before.revision + 1);
        assert_eq!(after.children[0].state, ChildStateV1::Cancelling);
        let recorded = effects.effects().unwrap();
        assert_eq!(recorded.len(), 1);
        assert!(matches!(
            recorded[0].state,
            polaris_core::management_effect::ManagementEffectStateV1::Completed { .. }
        ));
        assert!(effects.blocker().unwrap().is_none());
    }

    #[tokio::test]
    async fn m3e_selected_wait_preserves_restart_unknown_without_replay() {
        use polaris_core::subagent_control::{
            ChildAttemptId, ChildBindingV1, ChildControlPlaneV1, LogicalChildId, ParentBindingV1,
            RegisterChildV1,
        };
        let config = config();
        let mut plane = ChildControlPlaneV1::default();
        plane
            .register_before_start(RegisterChildV1 {
                logical_child_id: LogicalChildId::new("selected-child").unwrap(),
                attempt_id: ChildAttemptId::new("selected-attempt").unwrap(),
                binding: ChildBindingV1 {
                    root_run_id: config.root_run_id.clone(),
                    root_attempt_id: config.root_attempt_id.clone(),
                    active_task_id: config.active_task_id.clone(),
                    parent: ParentBindingV1::Root,
                },
                role: "file-reader".into(),
                tier: "dwarf".into(),
                model: "test".into(),
                effort: "auto".into(),
                capabilities: config.parent_capabilities.clone(),
            })
            .unwrap();
        let control = RootDelegationControlV1::restore_with_persistence_hook(
            config,
            plane,
            0,
            Arc::new(|_| Ok(())),
        )
        .unwrap();
        let lines = subagents(Some(&control), "wait selected-child selected-attempt", None)
            .await
            .unwrap();
        assert!(
            lines
                .iter()
                .any(|line| line.contains("selected-child / selected-attempt")
                    && line.contains("outcome_unknown"))
        );
        let before = control.snapshot().await;
        assert!(
            subagents(Some(&control), "wait selected-child another-attempt", None)
                .await
                .is_err()
        );
        let cancelled = subagents(
            Some(&control),
            "cancel selected-child selected-attempt",
            None,
        )
        .await
        .unwrap();
        assert!(cancelled.iter().any(|line| line.contains("既に終端")));
        assert_eq!(control.snapshot().await, before);
    }

    #[test]
    fn m3e_next_run_limit_and_sandbox_preserve_startup_ceiling() {
        let mut next = 32;
        run_settings("settings max-turns 12", &mut next, 32).unwrap();
        let mut tracker = polaris_core::stop::StopTracker::new(next);
        for _ in 0..11 {
            assert!(tracker.observe_turn().is_none());
        }
        assert_eq!(
            tracker.observe_turn(),
            Some(polaris_core::stop::StopReason::MaxTurns)
        );
        assert!(run_settings("settings max-turns 33", &mut next, 32).is_err());
        assert_eq!(next, 12);
        let policy = SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[]).unwrap();
        assert!(
            sandbox("status", &policy, ApprovalPolicy::Never).unwrap()[0].contains("read-only")
        );
        assert!(sandbox("full-access", &policy, ApprovalPolicy::Never).is_err());
        assert_eq!(policy.mode(), polaris_sandbox::SandboxMode::ReadOnly);
        let roles = polaris_skills::builtin::agent_types();
        let lines = super::roles(&roles, "list").unwrap();
        assert!(lines.iter().any(|line| line.contains("file-reader")));
        assert!(super::roles(&roles, "bind file-reader other-account").is_err());
    }
}
