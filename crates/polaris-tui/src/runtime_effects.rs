//! Runtime management requests use the bound activity lease, never source mutation.
use crate::runtime_commands::RuntimeMutationContext;
use polaris_core::{
    approval::{Approver, Decision},
    management_effect::{
        ManagementDenialReasonV1, ManagementEffectHashV1, ManagementEffectScopeV1,
        ManagementInventoryIdV1, ManagementOperationIdV1, ManagementPermissionScopeV1,
        ManagementRewindDispositionV1, ManagementUnknownReasonV1, PrepareManagementEffectOutcomeV1,
        PrepareManagementEffectV1, StartedManagementEffectV1,
    },
};
use std::cell::RefCell;

pub(crate) fn begin(
    context: &RuntimeMutationContext<'_>,
    inventory: &str,
    payload: &[u8],
) -> Result<Option<StartedManagementEffectV1>, String> {
    let prepared = context
        .effects
        .prepare_runtime(
            context.lease,
            context.effects.cursor().map_err(|e| e.to_string())?,
            PrepareManagementEffectV1 {
                operation_id: ManagementOperationIdV1::random().map_err(|e| e.to_string())?,
                inventory: ManagementInventoryIdV1::parse(inventory).map_err(|e| e.to_string())?,
                scope: ManagementEffectScopeV1 {
                    project: context.effects.project_scope_hash(),
                    session: Some(ManagementEffectHashV1::digest(
                        context.session_id.as_bytes(),
                    )),
                    generation: context.generation,
                    revision: context.generation,
                    permission: ManagementPermissionScopeV1::LocalSession,
                    account_binding: None,
                },
                request_hash: ManagementEffectHashV1::digest(payload),
                rewind: ManagementRewindDispositionV1::IrreversibleLocal,
            },
        )
        .map_err(|e| e.to_string())?;
    let PrepareManagementEffectOutcomeV1::Prepared(prepared) = prepared else {
        return Err("既存のruntime効果を再実行しません".into());
    };
    if !context.accepted {
        context
            .effects
            .deny_runtime(
                context.lease,
                &prepared,
                ManagementDenialReasonV1::UserDeclined,
            )
            .map_err(|e| e.to_string())?;
        return Ok(None);
    }
    context
        .effects
        .start_runtime(context.lease, &prepared)
        .map(Some)
        .map_err(|e| e.to_string())
}

pub(crate) fn finish(
    context: &RuntimeMutationContext<'_>,
    started: &StartedManagementEffectV1,
    verified: bool,
    receipt: &[u8],
) -> Result<(), String> {
    if verified {
        context
            .effects
            .complete_started_runtime(
                context.lease,
                started,
                ManagementEffectHashV1::digest(receipt),
            )
            .map_err(|e| e.to_string())?;
    } else {
        context
            .effects
            .mark_outcome_unknown_runtime(
                context.lease,
                started,
                ManagementUnknownReasonV1::InterruptedAfterStart,
            )
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Gate still decides whether to ask. An Allow never escapes without its
/// durable decision receipt; failures deny and stop the owning turn.
pub(crate) struct JournalledApprover<'a, A> {
    pub inner: A,
    pub context: RuntimeMutationContext<'a>,
    pub error: &'a RefCell<Option<String>>,
}
impl<A: Approver> Approver for JournalledApprover<'_, A> {
    fn ask(&mut self, reason: &str) -> Decision {
        let decision = self.inner.ask(reason);
        self.context.accepted = decision == Decision::Allow;
        let result = begin(&self.context, "D24", reason.as_bytes()).and_then(|started| {
            if let Some(started) = started {
                finish(&self.context, &started, true, b"approval-decision-allow")?;
            }
            Ok(())
        });
        if let Err(error) = result {
            *self.error.borrow_mut() = Some(error);
            return Decision::Deny;
        }
        decision
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approver::{KeyReader, TuiApprover};
    use polaris_core::{
        management_effect::{ManagementEffectStateV1, ManagementEffectStoreV1},
        rewind_workspace::WorkspaceOwner,
    };
    use ratatui::{Terminal, backend::TestBackend, crossterm::event::KeyCode};
    struct Key(KeyCode);
    impl KeyReader for Key {
        fn read_key(&mut self) -> std::io::Result<KeyCode> {
            Ok(self.0)
        }
    }
    #[test]
    fn m3e_tui_approval_and_interrupt_receipts_preserve_denial() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let state = temp.path().join("state");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir(&state).unwrap();
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
        let terminal = RefCell::new(Terminal::new(TestBackend::new(100, 24)).unwrap());
        let error = RefCell::new(None);
        for (key, expected) in [
            (KeyCode::Char('y'), Decision::Allow),
            (KeyCode::Char('n'), Decision::Deny),
        ] {
            let mut keys = Key(key);
            let mut approver = JournalledApprover {
                inner: TuiApprover {
                    terminal: &terminal,
                    reader: &mut keys,
                },
                context: RuntimeMutationContext {
                    effects: &effects,
                    lease: &activity,
                    session_id: "selected",
                    generation: 1,
                    accepted: false,
                },
                error: &error,
            };
            assert_eq!(approver.ask("run confined command"), expected);
        }
        assert!(error.borrow().is_none());
        let context = RuntimeMutationContext {
            effects: &effects,
            lease: &activity,
            session_id: "selected",
            generation: 1,
            accepted: true,
        };
        let stopped = begin(&context, "D23", b"exact-run").unwrap().unwrap();
        finish(&context, &stopped, true, b"cancelled-after-cleanup").unwrap();
        let started = begin(&context, "D23", b"failed-cleanup").unwrap().unwrap();
        finish(&context, &started, false, b"").unwrap();
        assert!(effects.blocker().unwrap().is_some());
        let mut keys = Key(KeyCode::Char('y'));
        let mut approver = JournalledApprover {
            inner: TuiApprover {
                terminal: &terminal,
                reader: &mut keys,
            },
            context,
            error: &error,
        };
        assert_eq!(approver.ask("must not allow after unknown"), Decision::Deny);
        assert!(error.borrow().is_some());
        assert!(
            effects
                .effects()
                .unwrap()
                .iter()
                .any(|e| matches!(e.state, ManagementEffectStateV1::Completed { .. }))
        );
    }
}
