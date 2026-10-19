//! Explicit recovery never resumes a send or reconstructs a confirm token.
use super::*;
#[cfg(any(unix, test))]
use crate::sync::repository::c10::C10RestoreReadbackV1;

impl SyncPushOwnerV1 {
    #[cfg(unix)]
    pub(super) fn recovery_snapshot(
        &self,
        lease: &ProjectMutationLeaseV1,
        operation: Operation,
    ) -> Result<ParticipationSnapshotV2> {
        let c08 = C08IdleReadV1::inspect_at_state_dir(&self.project, &self.state, lease)?;
        let store = C10IntentStoreV2::inspect_at_state_dir(&self.state, lease)?;
        self.recovery_snapshot_from_store(lease, operation, &store, c08.management())
    }
    #[cfg(unix)]
    pub(super) fn recovery_snapshot_from_store(
        &self,
        lease: &ProjectMutationLeaseV1,
        operation: Operation,
        store: &C10IntentStoreV2<'_>,
        management: &ManagementEffectInspectOpenV1,
    ) -> Result<ParticipationSnapshotV2> {
        let pending = store.pending_history_for_operation(operation)?;
        let stored = match pending {
            Some(history) => Some(history),
            None => store.lookup(operation)?,
        };
        let Some(history) = stored else {
            return self.snapshot();
        };
        let SyncRemoteIntentStateV2::Completed { receipt } = history.current().state() else {
            return self.snapshot();
        };
        // This raw inspection is only a derivation step. The strict exact-CAS
        // reader below must validate the actual pending source and target.
        let raw = participation::C12GenerationOwnerV1::open(&self.state, lease)
            .map_err(|_| SyncPushErrorV1::Protection)?
            .current_body()
            .map_err(|_| SyncPushErrorV1::Protection)?
            .ok_or(SyncPushErrorV1::Protection)?;
        if raw.len() > participation::MAX_PARTICIPATION_BYTES_V2 {
            return Err(SyncPushErrorV1::Protection);
        }
        let digest = SyncDigestV1::digest(&raw);
        let identity = history.current().identity();
        if digest != identity.prior_participation_digest && digest != receipt.participation_digest {
            return Err(SyncPushErrorV1::Stale);
        }
        let actual: ParticipationV2 =
            serde_json::from_slice(&raw).map_err(|_| SyncPushErrorV1::Protection)?;
        require_part_identity(&actual, identity)?;
        self.require_binding(&actual.credential_binding)?;
        let next = next_participation(&actual, &receipt.intended_commit);
        if SyncDigestV1::digest(&next.canonical_bytes()?) != receipt.participation_digest
            || digest == receipt.participation_digest && actual != next
        {
            return Err(SyncPushErrorV1::Protection);
        }
        let mut strict = if store.active_records().any(|h| h == &history) {
            let capability = C10CompletedParticipationRecoveryV1 {
                owner: self,
                lease,
                store,
                management,
                history: history.clone(),
                target: next,
            };
            participation::inspect_c10_completed_publication(&capability)?
        } else {
            participation::inspect_rebind_publication(
                &self.state,
                lease,
                Some(identity.prior_participation_digest),
                &next,
            )?
        };
        if strict.exact_digest != Some(digest) {
            return Err(SyncPushErrorV1::Stale);
        }
        let part = match strict.classification {
            ParticipationReadV2::Current(part)
            | ParticipationReadV2::StaleCredentialBindingOnly(part) => part,
            _ => return Err(SyncPushErrorV1::Protection),
        };
        require_part_identity(&part, identity)?;
        self.require_binding(&part.credential_binding)?;
        strict.classification = ParticipationReadV2::Current(part);
        self.verify_state()?;
        Ok(strict)
    }

    #[cfg(unix)]
    pub(super) fn publish_completed_participation(
        &self,
        lease: &ProjectMutationLeaseV1,
        history: &SyncRemoteIntentHistoryV2,
        next: &ParticipationV2,
    ) -> Result<SyncDigestV1> {
        let c08 = C08IdleReadV1::inspect_at_state_dir(&self.project, &self.state, lease)?;
        let store = C10IntentStoreV2::inspect_at_state_dir(&self.state, lease)?;
        let capability = C10CompletedParticipationRecoveryV1 {
            owner: self,
            lease,
            store: &store,
            management: c08.management(),
            history: history.clone(),
            target: next.clone(),
        };
        Ok(participation::publish_c10_completed_publication(
            &capability,
        )?)
    }

    #[cfg(unix)]
    pub fn recover(self, operation: Operation) -> Result<SyncPushOutcomeV1> {
        self.host.require_recovery_invocation(operation)?;
        let lease = self.lease()?;
        self.admit(&lease, Some(operation), true)?;
        let management = self.management(&lease)?;
        let effect = management
            .effect(operation)?
            .ok_or(SyncPushErrorV1::Protection)?;
        let mut store = C10IntentStoreV2::inspect_at_state_dir(&self.state, &lease)?;
        if let Some(pending) = store.pending_history_for_operation(operation)? {
            validate_pair_identity(&pending, &effect, self.project_digest())?;
            store.resume_pending(operation)?;
        }
        let Some(mut history) = store.lookup(operation)? else {
            // Prepared and started/no-intent may only be explicitly abandoned.
            return Ok(SyncPushOutcomeV1::OutcomeUnknown {
                operation_id: operation,
            });
        };
        validate_pair_identity(&history, &effect, self.project_digest())?;
        if !store
            .active_records()
            .any(|r| r.operation_id() == operation)
        {
            store.archived_terminal_pair_readback(operation, &effect)?;
            return terminal_outcome(&history);
        }
        let mut reservation = store.reserve_operation(operation)?;
        drop(store);
        let part = require_current(self.recovery_snapshot(&lease, operation)?)?;
        require_part_identity(&part, history.current().identity())?;
        if let SyncRemoteIntentStateV2::Completed { .. } = history.current().state() {
            let unknown = management.unknown_effect(operation)?;
            return match self.finish(
                &lease,
                &mut reservation,
                &history,
                &management,
                (None, unknown.as_ref()),
                &part,
            ) {
                Ok(result) => Ok(result),
                Err(_) => Ok(SyncPushOutcomeV1::PartialComplete {
                    operation_id: operation,
                }),
            };
        }
        if let SyncRemoteIntentStateV2::ReconciledAbandon { .. } = history.current().state() {
            return self.finish_abandon(&lease, &mut reservation, &history, &management);
        }
        let unknown = management
            .unknown_effect(operation)?
            .ok_or(SyncPushErrorV1::Protection)?;
        let repository =
            C10LocalRepositoryV1::inspect(&self.state, self.credentials.git_identity())?;
        let C10RepositoryInspectionV1::Existing(repository) = repository else {
            // No repair/init is inferred from LocalPrepared.
            if matches!(
                history.current().state(),
                SyncRemoteIntentStateV2::LocalPrepared { .. }
            ) {
                self.publish_unknown(
                    &lease,
                    &mut reservation,
                    &history,
                    SyncUnknownReasonV2::RestartedAfterLocalPrepared,
                    None,
                )?;
            }
            return Ok(SyncPushOutcomeV1::OutcomeUnknown {
                operation_id: operation,
            });
        };
        let mut repository = *repository;
        let mut readback = None;
        match history.current().state() {
            SyncRemoteIntentStateV2::LocalPrepared { .. } => {
                // Only actual object fsync/reopen can retain a newly created B.
                let current = repository.head_for_recovery()?;
                let inspection =
                    C10LocalRepositoryV1::inspect(&self.state, self.credentials.git_identity())?;
                if let C10RepositoryInspectionV1::Existing(candidate) = inspection {
                    if let Ok((reopened, proof)) = (*candidate).durable_current_after_unknown(
                        &self.project,
                        &management,
                        &unknown,
                        &lease,
                    ) {
                        repository = reopened;
                        readback = Some(proof);
                    } else if repository.head_for_recovery()? != current {
                        return Err(SyncPushErrorV1::Stale);
                    }
                }
                history = self.publish_unknown(
                    &lease,
                    &mut reservation,
                    &history,
                    SyncUnknownReasonV2::RestartedAfterLocalPrepared,
                    readback.take(),
                )?;
            }
            SyncRemoteIntentStateV2::BeforeSend { .. } => {
                history = self.publish_unknown(
                    &lease,
                    &mut reservation,
                    &history,
                    SyncUnknownReasonV2::RestartedAfterBeforeSend,
                    None,
                )?;
            }
            SyncRemoteIntentStateV2::OutcomeUnknown { .. } => {}
            _ => return Err(SyncPushErrorV1::Protection),
        }
        let identity = history.current().identity().clone();
        let prepared = local_prepared(&history)?;
        let intended = match history.current().state() {
            SyncRemoteIntentStateV2::OutcomeUnknown { unknown } => unknown
                .intended_commit
                .clone()
                .or_else(|| {
                    unknown
                        .created_commit_proof
                        .as_ref()
                        .map(|p| p.commit().clone())
                })
                .or_else(|| {
                    if prepared.payload == prepared.head_tree
                        && let SyncPriorHeadV2::Present { commit } = &prepared.prior_head
                    {
                        return Some(commit.clone());
                    }
                    None
                }),
            _ => None,
        };
        let Some(intended) = intended else {
            return Ok(SyncPushOutcomeV1::OutcomeUnknown {
                operation_id: operation,
            });
        };
        let subject = recovery_summary(&identity, &part, SyncPushActionV1::ReconcileKnownHead);
        let mut transport = C10CasTransportV1::new(
            identity.remote.clone(),
            OwnerNetwork {
                owner: &self,
                repository: &repository,
                binding: &identity.account_binding,
                subject: &subject,
                payload: None,
                invocation: InvocationV1::Recovery,
                participation: Some(identity.prior_participation_digest),
            },
        );
        if transport.observe_exact()?
            != (ExpectedSyncRemoteOidV1::Present {
                oid: intended.clone(),
            })
        {
            return Ok(SyncPushOutcomeV1::OutcomeUnknown {
                operation_id: operation,
            });
        }
        // Exact fetch only. This operation can never call send_once.
        transport.fetch_exact_for_recovery(&operation)?;
        drop(transport);
        let expected_current = repository.head_for_recovery()?;
        let cap = super::super::C10ExactRecoveryFetchV1 {
            identity: identity.clone(),
            intended: intended.clone(),
        };
        let (repository, actual) = repository.repair_fetched_after_unknown(
            &self.project,
            &management,
            &unknown,
            &lease,
            &expected_current,
            cap,
        )?;
        if let SyncRemoteIntentStateV2::OutcomeUnknown { unknown } = history.current().state()
            && let Some(proof) = &unknown.created_commit_proof
        {
            proof.require_actual_readback(&actual)?;
        }
        if actual.commit() != &intended || actual.descriptor() != &identity.payload {
            return Err(SyncPushErrorV1::Protection);
        }
        let observed = C10CasTransportV1::new(
            identity.remote.clone(),
            OwnerNetwork {
                owner: &self,
                repository: &repository,
                binding: &identity.account_binding,
                subject: &subject,
                payload: None,
                invocation: InvocationV1::Recovery,
                participation: Some(identity.prior_participation_digest),
            },
        )
        .observe_exact()?;
        if observed
            != (ExpectedSyncRemoteOidV1::Present {
                oid: intended.clone(),
            })
        {
            return Ok(SyncPushOutcomeV1::OutcomeUnknown {
                operation_id: operation,
            });
        }
        history = history.completed(receipt_for(&identity, &part, &intended)?)?;
        self.publish(&lease, &mut reservation, &history)?;
        match self.finish(
            &lease,
            &mut reservation,
            &history,
            &management,
            (None, Some(&unknown)),
            &part,
        ) {
            Ok(result) => Ok(result),
            Err(_) => Ok(SyncPushOutcomeV1::PartialComplete {
                operation_id: operation,
            }),
        }
    }

    #[cfg(not(unix))]
    pub fn recover(self, _operation: Operation) -> Result<SyncPushOutcomeV1> {
        Err(SyncPushErrorV1::UnavailablePlatform)
    }

    #[cfg(unix)]
    pub fn abandon(self, operation: Operation) -> Result<SyncPushOutcomeV1> {
        let lease = self.lease()?;
        self.admit(&lease, Some(operation), true)?;
        let management = self.management(&lease)?;
        let effect = management
            .effect(operation)?
            .ok_or(SyncPushErrorV1::Protection)?;
        self.host.confirm_abandon(
            operation,
            SyncDigestV1::from_hex(&effect.request.request_hash.as_hex())?,
        )?;
        let mut store = C10IntentStoreV2::inspect_at_state_dir(&self.state, &lease)?;
        if let Some(pending) = store.pending_history_for_operation(operation)? {
            validate_pair_identity(&pending, &effect, self.project_digest())?;
            store.resume_pending(operation)?;
        }
        let stored = store.lookup(operation)?;
        if matches!(effect.state, State::Denied { .. }) && stored.is_none() {
            return Ok(SyncPushOutcomeV1::Denied {
                operation_id: operation,
            });
        }
        if matches!(effect.state, State::Prepared) && stored.is_none() {
            drop(store);
            let prepared = management
                .prepared_effect(operation)?
                .ok_or(SyncPushErrorV1::Protection)?;
            management.deny(&lease, &prepared, ManagementDenialReasonV1::UserDeclined)?;
            return Ok(SyncPushOutcomeV1::Denied {
                operation_id: operation,
            });
        }
        if let Some(history) = &stored {
            validate_pair_identity(history, &effect, self.project_digest())?;
            if matches!(
                history.current().state(),
                SyncRemoteIntentStateV2::Completed { .. }
            ) {
                return Err(SyncPushErrorV1::Blocked);
            }
            if !store
                .active_records()
                .any(|r| r.operation_id() == operation)
            {
                store.archived_terminal_pair_readback(operation, &effect)?;
                return terminal_outcome(history);
            }
        }
        let mut reservation = store.reserve_operation(operation)?;
        let inventory_digest = store.inventory_digest();
        drop(store);
        let unknown = management
            .unknown_effect(operation)?
            .ok_or(SyncPushErrorV1::Protection)?;
        let Some(mut history) = stored else {
            // Recreate the original complete request from unchanged actual
            // P/H/A/R. An opaque hash is never decoded or guessed.
            let fresh = self.preview_operation(
                operation,
                InvocationV1::Abandon {
                    request_hash: SyncDigestV1::from_hex(&effect.request.request_hash.as_hex())?,
                },
            )?;
            if request(&fresh.identity, effect.request.scope.revision)? != effect.request {
                return Err(SyncPushErrorV1::Stale);
            }
            let evidence = NoIntentAbandonEvidenceV2 {
                identity: fresh.identity,
                prior_head: fresh.prepared.prior_head.clone(),
                current_head: fresh.prepared.prior_head,
                agreed_commit: fresh.prepared.agreed_commit,
                prior_descriptor: fresh.prepared.head_tree.clone(),
                current_descriptor: fresh.prepared.head_tree,
                prior_index_digest: fresh.prepared.prior_index_digest,
                current_index_digest: fresh.prepared.prior_index_digest,
                prior_staging_digest: fresh.prepared.prior_staging_digest,
                current_staging_digest: fresh.prepared.prior_staging_digest,
                last_observation: observation(&fresh.prepared.expected_remote),
                active_inventory_digest: inventory_digest,
                confirmation: SyncAbandonConfirmationV2::ExplicitUserConfirmation,
            };
            let history = SyncRemoteIntentHistoryV2::no_intent_abandon(evidence)?;
            self.publish(&lease, &mut reservation, &history)?;
            return self.finish_abandon(&lease, &mut reservation, &history, &management);
        };
        if matches!(
            history.current().state(),
            SyncRemoteIntentStateV2::ReconciledAbandon { .. }
        ) {
            return self.finish_abandon(&lease, &mut reservation, &history, &management);
        }
        let identity = history.current().identity().clone();
        let part = require_current(self.snapshot()?)?;
        require_part_identity(&part, &identity)?;
        if self.snapshot()?.exact_digest != Some(identity.prior_participation_digest) {
            return Err(SyncPushErrorV1::Stale);
        }
        let mut repository =
            C10LocalRepositoryV1::inspect(&self.state, self.credentials.git_identity())?;
        let mut created = None;
        if matches!(
            history.current().state(),
            SyncRemoteIntentStateV2::LocalPrepared { .. }
        ) {
            if let C10RepositoryInspectionV1::Existing(existing) = repository {
                let current = existing.head_for_recovery()?;
                let candidate =
                    C10LocalRepositoryV1::inspect(&self.state, self.credentials.git_identity())?;
                if let C10RepositoryInspectionV1::Existing(candidate) = candidate {
                    match (*candidate).durable_current_after_unknown(
                        &self.project,
                        &management,
                        &unknown,
                        &lease,
                    ) {
                        Ok((reopened, proof)) => {
                            repository = C10RepositoryInspectionV1::Existing(Box::new(reopened));
                            created = Some(proof);
                        }
                        Err(_) => {
                            if existing.head_for_recovery()? != current {
                                return Err(SyncPushErrorV1::Stale);
                            }
                            repository = C10RepositoryInspectionV1::Existing(existing);
                        }
                    }
                } else {
                    return Err(SyncPushErrorV1::Protection);
                }
            }
            history = self.publish_unknown(
                &lease,
                &mut reservation,
                &history,
                SyncUnknownReasonV2::AbandonRequested,
                created,
            )?;
        } else if matches!(
            history.current().state(),
            SyncRemoteIntentStateV2::BeforeSend { .. }
        ) {
            history = self.publish_unknown(
                &lease,
                &mut reservation,
                &history,
                SyncUnknownReasonV2::AbandonRequested,
                None,
            )?;
        }
        let prepared = local_prepared(&history)?;
        let SyncRemoteIntentStateV2::OutcomeUnknown {
            unknown: stored_unknown,
        } = history.current().state()
        else {
            return Err(SyncPushErrorV1::Protection);
        };
        let expected_current = match &repository {
            C10RepositoryInspectionV1::Absent(value) => value.snapshot_clean()?.head().clone(),
            C10RepositoryInspectionV1::Existing(value) => value.head_for_recovery()?,
        };
        let restored = match repository {
            C10RepositoryInspectionV1::Absent(value) => {
                value.restore_absent_after_unknown(&self.project, &management, &unknown, &lease)?
            }
            C10RepositoryInspectionV1::Existing(value) => {
                (*value)
                    .restore_after_unknown(
                        &self.project,
                        &management,
                        &unknown,
                        &lease,
                        &expected_current,
                    )?
                    .1
            }
        };
        let evidence = restore_evidence(
            &history,
            &prepared,
            &expected_current,
            &restored,
            stored_unknown.last_observation.clone(),
        )?;
        history = history.reconciled_abandon(evidence)?;
        self.publish(&lease, &mut reservation, &history)?;
        self.finish_abandon(&lease, &mut reservation, &history, &management)
    }

    #[cfg(not(unix))]
    pub fn abandon(self, _operation: Operation) -> Result<SyncPushOutcomeV1> {
        Err(SyncPushErrorV1::UnavailablePlatform)
    }

    #[cfg(unix)]
    fn finish_abandon(
        &self,
        lease: &ProjectMutationLeaseV1,
        reservation: &mut C10IntentReservationV2,
        history: &SyncRemoteIntentHistoryV2,
        management: &ManagementEffectStoreV1,
    ) -> Result<SyncPushOutcomeV1> {
        if !matches!(
            history.current().state(),
            SyncRemoteIntentStateV2::ReconciledAbandon { .. }
        ) {
            return Err(SyncPushErrorV1::Protection);
        }
        let actual = self.snapshot()?;
        if actual.exact_digest != Some(history.current().identity().prior_participation_digest) {
            return Err(SyncPushErrorV1::Stale);
        }
        let evidence = management_hash(history.terminal_evidence_hash()?)?;
        if let Some(unknown) = management.unknown_effect(history.operation_id())? {
            management.reconcile_abandon(lease, &unknown, evidence)?;
        }
        let effect = management
            .effect(history.operation_id())?
            .ok_or(SyncPushErrorV1::Protection)?;
        let mut store = C10IntentStoreV2::inspect_at_state_dir(&self.state, lease)?;
        let proof = store.terminal_readback(reservation, &effect, &actual)?;
        store.archive_terminal(reservation, proof)?;
        Ok(SyncPushOutcomeV1::ReconciledAbandon {
            operation_id: history.operation_id(),
        })
    }
}

#[cfg(any(unix, test))]
fn local_prepared(history: &SyncRemoteIntentHistoryV2) -> Result<SyncLocalPreparedV2> {
    history.validate()?;
    match history.records()[0].state() {
        SyncRemoteIntentStateV2::LocalPrepared { prepared } => Ok((**prepared).clone()),
        _ => Err(SyncPushErrorV1::Protection),
    }
}
#[cfg(any(unix, test))]
fn require_current(snapshot: ParticipationSnapshotV2) -> Result<ParticipationV2> {
    match snapshot.classification {
        ParticipationReadV2::Current(part) => Ok(part),
        _ => Err(SyncPushErrorV1::Credential),
    }
}
#[cfg(any(unix, test))]
pub(super) fn require_part_identity(
    part: &ParticipationV2,
    identity: &SyncIntentIdentityV2,
) -> Result<()> {
    part.validate()?;
    if part.inner().share.share_id != identity.share_id.as_str()
        || part.credential_binding != identity.account_binding
        || part
            .inner()
            .share
            .production_remote()
            .map_err(|_| SyncPushErrorV1::Protection)?
            != identity.remote
    {
        return Err(SyncPushErrorV1::Stale);
    }
    Ok(())
}
#[cfg(any(unix, test))]
fn observation(remote: &ExpectedSyncRemoteOidV1) -> SyncRemoteObservationV2 {
    match remote {
        ExpectedSyncRemoteOidV1::Missing => SyncRemoteObservationV2::Missing,
        ExpectedSyncRemoteOidV1::Present { oid } => {
            SyncRemoteObservationV2::Present { oid: oid.clone() }
        }
    }
}
#[cfg(any(unix, test))]
fn terminal_outcome(history: &SyncRemoteIntentHistoryV2) -> Result<SyncPushOutcomeV1> {
    match history.current().state() {
        SyncRemoteIntentStateV2::Completed { receipt } => Ok(SyncPushOutcomeV1::Completed {
            operation_id: history.operation_id(),
            commit: receipt.intended_commit.clone(),
        }),
        SyncRemoteIntentStateV2::ReconciledAbandon { .. } => {
            Ok(SyncPushOutcomeV1::ReconciledAbandon {
                operation_id: history.operation_id(),
            })
        }
        _ => Err(SyncPushErrorV1::Protection),
    }
}
#[cfg(any(unix, test))]
fn recovery_summary(
    identity: &SyncIntentIdentityV2,
    part: &ParticipationV2,
    action: SyncPushActionV1,
) -> SyncPushConfirmationSummaryV1 {
    SyncPushConfirmationSummaryV1 {
        operation_id: identity.operation_id,
        request_hash: identity.request_hash,
        project: identity.project,
        share_id: identity.share_id.clone(),
        remote_label: identity.remote.redacted_label(),
        remote_ref: identity.remote_ref.clone(),
        manifest_digest: part.inner().manifest.digest(),
        payload_digest: identity.payload.digest(),
        file_count: identity.payload.entry_count(),
        excluded_count: 0,
        expected_remote: identity.expected_remote_oid.clone(),
        account_registration_id: format!(
            "{}:{}",
            identity.account_binding.credential_item_id,
            identity.account_binding.generation()
        ),
        action,
    }
}
#[cfg(any(unix, test))]
fn restore_evidence(
    history: &SyncRemoteIntentHistoryV2,
    prepared: &SyncLocalPreparedV2,
    current: &SyncPriorHeadV2,
    actual: &C10RestoreReadbackV1,
    last: SyncRemoteObservationV2,
) -> Result<RestoredLocalAbandonEvidenceV2> {
    let identity = history.current().identity();
    if actual.operation_id() != history.operation_id()
        || actual.request_hash() != identity.request_hash
        || actual.expected_current() != current
        || actual.restored_head() != &prepared.prior_head
        || actual.descriptor() != &prepared.head_tree
        || actual.index_digest() != prepared.prior_index_digest
        || actual.staging_digest() != prepared.prior_staging_digest
        || actual.objects_digest() == zero_digest_v2()
    {
        return Err(SyncPushErrorV1::Protection);
    }
    let SyncRemoteIntentStateV2::OutcomeUnknown { unknown } = history.current().state() else {
        return Err(SyncPushErrorV1::Protection);
    };
    let created = match unknown.previous_state {
        SyncPreviousStateV2::BeforeSend => unknown.intended_commit.clone(),
        SyncPreviousStateV2::LocalPrepared => unknown
            .created_commit_proof
            .as_ref()
            .map(|p| p.commit().clone()),
    };
    Ok(RestoredLocalAbandonEvidenceV2 {
        identity: identity.clone(),
        prior_head: prepared.prior_head.clone(),
        expected_current_head: current.clone(),
        created_commit: created,
        restored_head: actual.restored_head().clone(),
        prior_descriptor: prepared.head_tree.clone(),
        reopened_descriptor: actual.descriptor().clone(),
        prior_index_digest: prepared.prior_index_digest,
        restored_index_digest: actual.index_digest(),
        prior_staging_digest: prepared.prior_staging_digest,
        restored_staging_digest: actual.staging_digest(),
        durable_restore_digest: actual.objects_digest(),
        last_observation: last,
        participation_digest: identity.prior_participation_digest,
        confirmation: SyncAbandonConfirmationV2::ExplicitUserConfirmation,
    })
}
