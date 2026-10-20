//! Ordered polling phases for the single `RunExecution` owner.

use super::*;
use crate::desktop_store::Published;

pub(super) struct StepContext {
    now: u64,
    published: Published,
    policy_revision: DecimalU64,
    run_policy_matches: bool,
}

impl RunExecution {
    pub(super) fn step_context(
        &self,
        store: &Writer,
        target: &RunTarget,
    ) -> Result<StepContext, StoreError> {
        let now = now_ms()?;
        let published = store.snapshot()?;
        let policy_revision = published.state.policy_revision;
        let run_policy_matches = published.state.runs.iter().any(|run| {
            run.run.run_id == target.run_id
                && run.run.attempt_id == target.attempt_id
                && run.policy_revision == policy_revision
                && !run.run.state.is_terminal()
        });
        Ok(StepContext {
            now,
            published,
            policy_revision,
            run_policy_matches,
        })
    }

    pub(super) fn step_mcp(
        &mut self,
        store: &mut Writer,
        target: &RunTarget,
        context: &StepContext,
    ) -> Result<(), StoreError> {
        if let Some(mcp) = &mut self.mcp {
            let result = mcp.step(store, target, &context.published, &mut self.approval_events);
            if result.is_err() || mcp.terminal_stop() {
                self.cancel();
            }
            result?;
        }
        Ok(())
    }

    pub(super) fn step_registration(
        &mut self,
        store: &mut Writer,
        target: &RunTarget,
        context: &StepContext,
    ) -> Result<(), StoreError> {
        if let Some(registration) = &mut self.registration {
            let stopped = self.closed || self.disconnected.load(Ordering::Acquire);
            registration.step(
                store,
                target,
                &context.published,
                &mut self.approval_events,
                stopped,
            )?;
        }
        Ok(())
    }

    pub(super) fn step_awaiting_approvals(
        &mut self,
        store: &mut Writer,
        target: &RunTarget,
        context: &StepContext,
    ) -> Result<(), StoreError> {
        let mut cursor = 0;
        while cursor < self.awaiting.len() {
            let waiting = &self.awaiting[cursor];
            let cancelled = self.closed
                || self.disconnected.load(Ordering::Acquire)
                || waiting.request.cancelled.load(Ordering::Acquire);
            let revoked = !context.run_policy_matches
                || waiting.approval.policy_revision != context.policy_revision
                || !self.policy_matches(&waiting.request.command.policy);
            let expired = !waiting.deadline.valid();
            if cancelled || revoked || expired {
                store.invalidate_approval(&waiting.approval.approval_id)?;
                let waiting = self.awaiting.remove(cursor);
                self.approval_events
                    .push(EventBody::ApprovalExpired(ApprovalExpired {
                        approval_id: waiting.approval.approval_id,
                        reason: if cancelled {
                            ExpiryReason::Cancelled
                        } else if revoked {
                            ExpiryReason::PolicyRevoked
                        } else {
                            ExpiryReason::Expired
                        },
                    }));
                let _ = waiting
                    .request
                    .reply
                    .send(Arc::new(ExecutionResult::not_started(
                        "execution approval is no longer valid".into(),
                    )));
                continue;
            }
            let Some(decision) = waiting.decision else {
                cursor += 1;
                continue;
            };
            if decision == ApprovalDecision::Deny {
                let waiting = self.awaiting.remove(cursor);
                let _ = waiting
                    .request
                    .reply
                    .send(Arc::new(ExecutionResult::not_started(
                        "execution approval denied".into(),
                    )));
                continue;
            }
            // 一回限りの許可消費とintent保存を終えるまで、workerを作らない。
            let hash = waiting
                .request
                .command
                .binding_hash()
                .map_err(|_| StoreError::RunConflict)?;
            let receipt = store.consume_approval_intent(
                target,
                &waiting.approval.approval_id,
                &waiting.approval.operation_id,
                OPERATION_KIND,
                &waiting.request.command.policy.describe(),
                &hash,
                now_ms()?,
            )?;
            if receipt != IntentReceipt::NewlyPublished {
                return Err(StoreError::RunConflict);
            }
            let waiting = self.awaiting.remove(cursor);
            self.start_request(
                waiting.request,
                waiting.reservation,
                target,
                waiting.approval.operation_id,
                waiting.deadline,
                false,
            );
        }
        Ok(())
    }

    pub(super) fn step_new_requests(
        &mut self,
        store: &mut Writer,
        target: &RunTarget,
        context: &StepContext,
    ) -> Result<(), StoreError> {
        for _ in 0..QUANTUM {
            let Ok(request) = self.incoming.try_recv() else {
                break;
            };
            if self.closed
                || self.disconnected.load(Ordering::Acquire)
                || !context.run_policy_matches
                || request.cancelled.load(Ordering::Acquire)
                || self.slots.len() + self.awaiting.len() >= CAPACITY
            {
                let _ = request.reply.send(Arc::new(ExecutionResult::failed(
                    "execution rejected before approval".into(),
                )));
                continue;
            }
            // Classify once: a later grant expiry must reject at start, never
            // reclassify a stamped mutation as an ordinary approval request.
            let automatic = request.is_authorized_automatic_for(&target.run_id);
            if ((request.has_mutation_authorization() || request.has_execution_authorization())
                && !automatic)
                || !self.port.permits_request(&request, &target.run_id)
            {
                let _ = request.reply.send(Arc::new(ExecutionResult::not_started(
                    "folder execution authorization is no longer valid".into(),
                )));
                continue;
            }
            if !self.policy_matches(&request.command.policy) {
                let _ = request.reply.send(Arc::new(ExecutionResult::not_started(
                    "execution policy escapes prepared workspace".into(),
                )));
                continue;
            }
            if let Err(error) = ensure_reclaimer() {
                let _ = request.reply.send(Arc::new(ExecutionResult::failed(error)));
                continue;
            }
            let Some(reservation) = Reservation::acquire() else {
                let _ = request.reply.send(Arc::new(ExecutionResult::failed(
                    "service execution capacity exhausted".into(),
                )));
                continue;
            };
            self.next_operation = self
                .next_operation
                .checked_add(1)
                .ok_or(StoreError::Overflow)?;
            let operation = execution_operation_id(target, self.next_operation)?;
            if automatic {
                if store.record_intent(target, operation.clone())? != IntentReceipt::NewlyPublished
                {
                    return Err(StoreError::RunConflict);
                }
                let deadline = StartDeadline {
                    unix_ms: context.now.checked_add(5_000).ok_or(StoreError::Overflow)?,
                    monotonic: std::time::Instant::now() + std::time::Duration::from_secs(5),
                };
                self.start_request(request, reservation, target, operation, deadline, true);
                continue;
            }
            let approval = PendingApproval {
                approval_id: ApprovalId::new(format!("approval-{}", operation.as_str()))
                    .map_err(|_| StoreError::Overflow)?,
                run_id: target.run_id.clone(),
                attempt_id: target.attempt_id.clone(),
                operation_id: operation,
                operation: OPERATION_KIND.into(),
                scope: request.command.policy.describe(),
                payload_hash: request
                    .command
                    .binding_hash()
                    .map_err(|_| StoreError::RunConflict)?,
                policy_revision: context.policy_revision,
                expires_at_unix_ms: DecimalU64::new(
                    context
                        .now
                        .checked_add(APPROVAL_TTL_MS)
                        .ok_or(StoreError::Overflow)?,
                ),
                state: PendingApprovalState::Pending,
                display: ApprovalDisplay {
                    title: "コマンド実行".into(),
                    description: format!(
                        "{}\n{}",
                        excerpt(
                            &format!(
                                "{} {:?}",
                                request.command.program.display(),
                                request.command.args
                            ),
                            1000,
                        ),
                        excerpt(
                            &request.command.input_description().unwrap_or_default(),
                            1000,
                        ),
                    ),
                    choices: vec![ApprovalDecision::Allow, ApprovalDecision::Deny],
                },
            };
            let deadline = StartDeadline {
                unix_ms: approval.expires_at_unix_ms.get(),
                monotonic: std::time::Instant::now()
                    + std::time::Duration::from_millis(APPROVAL_TTL_MS),
            };
            store.publish_approval(approval.clone(), now_ms()?)?;
            self.approval_events
                .push(EventBody::ApprovalRequested(approval.clone()));
            self.awaiting.push(AwaitingExecution {
                request,
                approval,
                decision: None,
                reservation,
                deadline,
            });
        }
        Ok(())
    }

    pub(super) fn step_finished_slots(
        &mut self,
        store: &mut Writer,
        target: &RunTarget,
    ) -> Result<(), StoreError> {
        for _ in 0..QUANTUM.min(self.slots.len()) {
            self.cursor %= self.slots.len();
            let owned = &self.slots[self.cursor];
            self.cursor += 1;
            let operation = {
                let mut slot = owned.lock().unwrap_or_else(|e| e.into_inner());
                debug_assert_eq!(&slot.target, target);
                slot.poll();
                (slot.worker.is_none()
                    && slot.pending.is_none()
                    && slot.result.is_some()
                    && !slot.recorded)
                    .then(|| slot.operation.clone())
            };
            if let Some(operation) = operation {
                // fsync中にも独立回収スレッドがslotを取得できるようにする。
                store.record_operation_result(
                    target,
                    &operation,
                    ResultId::new(format!("result-{}", operation.as_str()))
                        .map_err(|_| StoreError::Overflow)?,
                )?;
                owned.lock().unwrap_or_else(|e| e.into_inner()).recorded = true;
            }
        }
        Ok(())
    }
}
