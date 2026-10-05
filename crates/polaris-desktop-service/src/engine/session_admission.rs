//! Durable admission adapter for the existing desktop `run.start` surface.
//!
//! The wire request is intentionally unchanged.  This owner records each
//! accepted draft before the session writer appends it and advances the same
//! admission after the writer commit.  A replay therefore resumes the saved
//! transition instead of classifying or appending the input again.

use polaris_core::{
    desktop_store::{SessionManagerStore, StoreError},
    session_manager::{
        AdmissionError, AdmissionId, AdmissionState, DestinationMode, NextRunnableV1,
        RoutingJudgementV1, SessionId, SessionManagerV1, SubmissionPayloadV1, SubmitOutcomeV1,
        SubmitRequestV1, TerminalStatus,
    },
};
#[cfg(test)]
use polaris_desktop_protocol::snapshot::Draft;
use polaris_desktop_protocol::{
    ids::{AdmissionId as WireAdmissionId, DecimalU64, SessionId as WireSessionId},
    orchestration::{
        ManagedSessionKindV2, ManagedSessionStateV2, ManagedSessionSummaryV2,
        SessionManagerSummaryV2, SidechatStateV2,
    },
    request::InputRoute,
    response::{
        InputAdmissionState, InputRouteReason, InputStatusResult, InputSubmitted, SessionListEntry,
        SessionListResult,
    },
};
#[cfg(test)]
use sha2::{Digest, Sha256};

/// Single root-scoped owner.  Every mutation persists through a revision CAS
/// before the caller performs the corresponding external effect.
pub(super) struct AdmissionOwner {
    store: SessionManagerStore,
    state: SessionManagerV1,
    deferred_observation: Option<(SessionId, u64, u64)>,
    #[cfg(test)]
    current: SessionId,
}

impl AdmissionOwner {
    pub(super) fn open(
        root: &polaris_core::desktop_store::DesktopRoot,
        session: &polaris_desktop_protocol::ids::SessionId,
        session_revision: DecimalU64,
        goal_revision: DecimalU64,
    ) -> Result<Self, StoreError> {
        let store = root.session_manager()?;
        let current = SessionId::new(session.as_str())
            .map_err(|_| StoreError::Corrupt("session manager current session"))?;
        let mut state = match store.load()? {
            Some(state) => state,
            None => {
                let initial = SessionManagerV1::new(current.clone());
                store.initialize(&initial)?;
                initial
            }
        };
        if !state.sessions.contains_key(&current) {
            return Err(StoreError::TargetMismatch);
        }

        // A process may have crashed after sending the classifier request.  A
        // restart never sends it again: the model owns the current fallback.
        let before = state.revision;
        if state
            .recover_classifying()
            .map_err(admission_restore_error)?
            != 0
        {
            store.persist(before, &state)?;
        }
        // A durable decision may be the last commit before a crash. Complete
        // only the local destination transition; never issue the classifier
        // request again.
        let decided = state
            .admissions
            .values()
            .filter(|admission| matches!(admission.state, AdmissionState::Decided { .. }))
            .map(|admission| admission.id.clone())
            .collect::<Vec<_>>();
        for id in decided {
            let admission = state.status(&id).map_err(admission_restore_error)?.clone();
            let before = state.revision;
            state
                .prepare_destination(&id, before, admission.revision)
                .map_err(admission_restore_error)?;
            store.persist(before, &state)?;
        }

        let expected = state.revision;
        let mut deferred_observation = None;
        let before_observation = state.clone();
        state
            .observe_session_revisions(
                &current,
                expected,
                session_revision.get(),
                goal_revision.get(),
            )
            .map_err(admission_restore_error)?;
        if state.revision != expected {
            match store.persist_observed(
                expected,
                &state,
                &current,
                session_revision.get(),
                goal_revision.get(),
            ) {
                Ok(()) => {}
                Err(StoreError::Capacity(_)) => {
                    state = before_observation;
                    deferred_observation =
                        Some((current.clone(), session_revision.get(), goal_revision.get()));
                }
                Err(error) => return Err(error),
            }
        }
        Ok(Self {
            store,
            state,
            deferred_observation,
            #[cfg(test)]
            current,
        })
    }

    #[cfg(test)]
    pub(super) fn admit_current(
        &mut self,
        draft: &Draft,
        session_revision: DecimalU64,
        goal_revision: DecimalU64,
        request_key: &str,
    ) -> Result<AdmissionId, StoreError> {
        self.observe(session_revision, goal_revision)?;
        let request = SubmitRequestV1 {
            parent_session: self.current.clone(),
            payload: SubmissionPayloadV1 {
                text: draft.text.clone(),
                attachment_ids: draft
                    .attachment_ids
                    .iter()
                    .map(|id| id.as_str().to_owned())
                    .collect(),
                route: DestinationMode::Auto,
                expected_session_revision: session_revision.get(),
                expected_goal_revision: goal_revision.get(),
            },
            admission_id: Some(
                AdmissionId::new(format!(
                    "adm-legacy-{:x}",
                    Sha256::digest(request_key.as_bytes())
                ))
                .map_err(admission_store_error)?,
            ),
        };
        let before = self.state.revision;
        let SubmitOutcomeV1::Accepted {
            admission,
            idempotent,
        } = self
            .state
            .submit(request, before)
            .map_err(admission_store_error)?;
        if !idempotent {
            self.store.persist_submission(
                before,
                &self.state,
                &std::collections::BTreeMap::from([(self.current.clone(), 0)]),
            )?;
        }
        self.route_current_fallback(&admission.id)?;
        Ok(admission.id)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "admission CAS revisions and payload fields are independently validated"
    )]
    pub(super) fn submit_text(
        &mut self,
        parent: &WireSessionId,
        expected_manager_revision: DecimalU64,
        expected_session_revision: DecimalU64,
        expected_goal_revision: DecimalU64,
        observed_session_revision: DecimalU64,
        observed_goal_revision: DecimalU64,
        route: InputRoute,
        attachment_ids: &[polaris_desktop_protocol::ids::AttachmentId],
        admission_id: Option<&WireAdmissionId>,
        text: &str,
        receipt_counts: &std::collections::BTreeMap<SessionId, usize>,
    ) -> Result<(AdmissionId, bool), StoreError> {
        let parent = SessionId::new(parent.as_str()).map_err(admission_store_error)?;
        let request = SubmitRequestV1 {
            parent_session: parent.clone(),
            payload: SubmissionPayloadV1 {
                text: text.to_owned(),
                attachment_ids: attachment_ids
                    .iter()
                    .map(|id| id.as_str().to_owned())
                    .collect(),
                route: match route {
                    InputRoute::Current => DestinationMode::Current,
                    InputRoute::New => DestinationMode::New,
                    InputRoute::Auto => DestinationMode::Auto,
                },
                expected_session_revision: expected_session_revision.get(),
                expected_goal_revision: expected_goal_revision.get(),
            },
            admission_id: admission_id
                .map(|id| AdmissionId::new(id.as_str()).map_err(admission_store_error))
                .transpose()?,
        };
        let prospective_id = request
            .derived_admission_id()
            .map_err(admission_store_error)?;
        if self.state.status(&prospective_id).is_ok() {
            let SubmitOutcomeV1::Accepted {
                admission,
                idempotent,
            } = self
                .state
                .submit(request, expected_manager_revision.get())
                .map_err(admission_store_error)?;
            debug_assert!(idempotent);
            return Ok((admission.id, false));
        }
        if observed_session_revision != expected_session_revision
            || observed_goal_revision != expected_goal_revision
        {
            return Err(StoreError::CasConflict("submission context"));
        }
        let before = expected_manager_revision.get();
        if self.state.revision != before {
            return Err(StoreError::CasConflict("session manager"));
        }
        let original = self.state.clone();
        let accepted = (|| {
            self.state
                .observe_session_revisions(
                    &parent,
                    before,
                    expected_session_revision.get(),
                    expected_goal_revision.get(),
                )
                .map_err(admission_store_error)?;
            let SubmitOutcomeV1::Accepted {
                admission,
                idempotent,
            } = self
                .state
                .submit(request, self.state.revision)
                .map_err(admission_store_error)?;
            if !idempotent {
                self.store.persist_observed_submission(
                    before,
                    &self.state,
                    receipt_counts,
                    &parent,
                    expected_session_revision.get(),
                    expected_goal_revision.get(),
                )?;
            }
            Ok::<_, StoreError>((admission, idempotent))
        })();
        let (admission, idempotent) = match accepted {
            Ok(result) => result,
            Err(error) => {
                self.state = original;
                return Err(error);
            }
        };
        if idempotent {
            return Ok((admission.id, false));
        }
        if self
            .deferred_observation
            .as_ref()
            .is_some_and(|(session, _, _)| session == &parent)
        {
            self.deferred_observation = None;
        }
        if route == InputRoute::Auto {
            let saved = self
                .state
                .status(&admission.id)
                .map_err(admission_store_error)?
                .clone();
            let before = self.state.revision;
            self.state
                .begin_classification(&saved.id, before, saved.revision)
                .map_err(admission_store_error)?;
            self.store.persist(before, &self.state)?;
            Ok((saved.id, true))
        } else {
            self.route(&admission.id, None)?;
            Ok((admission.id, false))
        }
    }

    pub(super) fn finish_classification(
        &mut self,
        id: &AdmissionId,
        judgement: Option<RoutingJudgementV1>,
    ) -> Result<(), StoreError> {
        let admission = self
            .state
            .status(id)
            .map_err(admission_store_error)?
            .clone();
        if !matches!(admission.state, AdmissionState::Classifying) {
            return Ok(());
        }
        let before = self.state.revision;
        self.state
            .decide(id, before, admission.revision, judgement, false, false)
            .map_err(admission_store_error)?;
        self.store.persist(before, &self.state)?;
        let admission = self
            .state
            .status(id)
            .map_err(admission_store_error)?
            .clone();
        let before = self.state.revision;
        self.state
            .prepare_destination(id, before, admission.revision)
            .map_err(admission_store_error)?;
        self.store.persist(before, &self.state)
    }

    pub(super) fn context_changed(&mut self, id: &AdmissionId) -> Result<(), StoreError> {
        let admission = self
            .state
            .status(id)
            .map_err(admission_store_error)?
            .clone();
        let before = self.state.revision;
        self.state
            .decide_context_changed(id, before, admission.revision)
            .map_err(admission_store_error)?;
        self.store.persist(before, &self.state)?;
        let admission = self
            .state
            .status(id)
            .map_err(admission_store_error)?
            .clone();
        let before = self.state.revision;
        self.state
            .prepare_destination(id, before, admission.revision)
            .map_err(admission_store_error)?;
        self.store.persist(before, &self.state)
    }

    pub(super) fn admission(
        &self,
        id: &AdmissionId,
    ) -> Result<polaris_core::session_manager::AdmissionV1, StoreError> {
        self.state
            .status(id)
            .cloned()
            .map_err(admission_store_error)
    }

    pub(super) fn sidechat_session(&self) -> Option<WireSessionId> {
        self.state.session_list().into_iter().find_map(|session| {
            matches!(
                session.init,
                polaris_core::session_manager::SessionInitV1::EmptySidechat { .. }
            )
            .then(|| WireSessionId::new(session.id.as_str()).ok())
            .flatten()
        })
    }

    pub(super) fn recovery_admissions(&self) -> Vec<polaris_core::session_manager::AdmissionV1> {
        self.state.admissions.values().cloned().collect()
    }

    pub(super) fn observe_session(
        &mut self,
        session: &SessionId,
        session_revision: DecimalU64,
        goal_revision: DecimalU64,
    ) -> Result<(), StoreError> {
        let before = self.state.revision;
        self.state
            .observe_session_revisions(session, before, session_revision.get(), goal_revision.get())
            .map_err(admission_restore_error)?;
        if self.state.revision != before {
            self.store.persist_observed(
                before,
                &self.state,
                session,
                session_revision.get(),
                goal_revision.get(),
            )?;
        }
        Ok(())
    }

    /// Resume only the pre-dispatch Reserved crash window. Once Classifying is
    /// durable, restart recovery never calls the classifier again.
    pub(super) fn resume_reserved(&mut self, id: &AdmissionId) -> Result<bool, StoreError> {
        let admission = self
            .state
            .status(id)
            .map_err(admission_store_error)?
            .clone();
        if !matches!(admission.state, AdmissionState::Reserved) {
            return Ok(false);
        }
        if let Some((session, session_revision, goal_revision)) = self
            .deferred_observation
            .clone()
            .filter(|(session, _, _)| session == &admission.parent_session)
        {
            let before = self.state.revision;
            let original = self.state.clone();
            let resumed = (|| {
                self.state
                    .observe_session_revisions(&session, before, session_revision, goal_revision)
                    .map_err(admission_store_error)?;
                let before_transition = self.state.revision;
                if session_revision != admission.payload.expected_session_revision
                    || goal_revision != admission.payload.expected_goal_revision
                {
                    self.state
                        .decide_context_changed(id, before_transition, admission.revision)
                        .map_err(admission_store_error)?;
                } else if admission.payload.route == DestinationMode::Auto {
                    self.state
                        .begin_classification(id, before_transition, admission.revision)
                        .map_err(admission_store_error)?;
                } else {
                    self.state
                        .decide(
                            id,
                            before_transition,
                            admission.revision,
                            None,
                            false,
                            false,
                        )
                        .map_err(admission_store_error)?;
                }
                self.store.persist_observed(
                    before,
                    &self.state,
                    &session,
                    session_revision,
                    goal_revision,
                )
            })();
            if let Err(error) = resumed {
                self.state = original;
                return Err(error);
            }
            self.deferred_observation = None;
            let saved = self
                .state
                .status(id)
                .map_err(admission_store_error)?
                .clone();
            if matches!(saved.state, AdmissionState::Decided { .. }) {
                let before = self.state.revision;
                self.state
                    .prepare_destination(id, before, saved.revision)
                    .map_err(admission_store_error)?;
                self.store.persist(before, &self.state)?;
            }
            return Ok(matches!(saved.state, AdmissionState::Classifying));
        }
        if admission.payload.route == DestinationMode::Auto {
            let before = self.state.revision;
            self.state
                .begin_classification(id, before, admission.revision)
                .map_err(admission_store_error)?;
            self.store.persist(before, &self.state)?;
            Ok(true)
        } else {
            self.route(id, None)?;
            Ok(false)
        }
    }

    pub(super) fn submitted(&self, id: &AdmissionId) -> Result<InputSubmitted, StoreError> {
        let wire =
            WireAdmissionId::new(id.as_str()).map_err(|_| StoreError::Corrupt("admission"))?;
        let status = self.input_status(&wire)?;
        let input_hash = self
            .state
            .status(id)
            .map_err(admission_store_error)?
            .payload_hash
            .clone();
        Ok(InputSubmitted {
            manager_revision: status.manager_revision,
            session_revision: status.session_revision,
            admission_id: status.admission_id,
            input_hash,
            route: status.route,
            state: status.state,
            route_reason: status.route_reason,
            destination_session_id: status.destination_session_id,
            run: status.run,
        })
    }

    #[cfg(test)]
    pub(super) fn admit_with_judgement(
        &mut self,
        draft: &Draft,
        session_revision: DecimalU64,
        goal_revision: DecimalU64,
        judgement: RoutingJudgementV1,
    ) -> Result<AdmissionId, StoreError> {
        self.observe(session_revision, goal_revision)?;
        let request = SubmitRequestV1 {
            parent_session: self.current.clone(),
            payload: SubmissionPayloadV1 {
                text: draft.text.clone(),
                attachment_ids: draft
                    .attachment_ids
                    .iter()
                    .map(|id| id.as_str().to_owned())
                    .collect(),
                route: DestinationMode::Auto,
                expected_session_revision: session_revision.get(),
                expected_goal_revision: goal_revision.get(),
            },
            admission_id: None,
        };
        let before = self.state.revision;
        let SubmitOutcomeV1::Accepted {
            admission,
            idempotent,
        } = self
            .state
            .submit(request, before)
            .map_err(admission_store_error)?;
        if !idempotent {
            self.store.persist_submission(
                before,
                &self.state,
                &std::collections::BTreeMap::from([(self.current.clone(), 0)]),
            )?;
        }
        self.route(&admission.id, Some(judgement))?;
        Ok(admission.id)
    }

    pub(super) fn input_appended_and_queue(&mut self, id: &AdmissionId) -> Result<(), StoreError> {
        self.input_appended_and_queue_inner(id, false)
    }

    pub(super) fn recover_input_appended_and_queue(
        &mut self,
        id: &AdmissionId,
    ) -> Result<(), StoreError> {
        self.input_appended_and_queue_inner(id, true)
    }

    fn input_appended_and_queue_inner(
        &mut self,
        id: &AdmissionId,
        recovery: bool,
    ) -> Result<(), StoreError> {
        let admission = self
            .state
            .status(id)
            .map_err(admission_store_error)?
            .clone();
        match admission.state {
            AdmissionState::DestinationReady { ref session_id, .. } => {
                let before = self.state.revision;
                if recovery {
                    self.state
                        .recover_input_appended(id, before, admission.revision, session_id)
                        .map_err(admission_store_error)?;
                } else {
                    self.state
                        .record_input_appended(id, before, admission.revision, session_id)
                        .map_err(admission_store_error)?;
                }
                self.store.persist(before, &self.state)?;
            }
            AdmissionState::InputAppended { .. }
            | AdmissionState::RunQueued { .. }
            | AdmissionState::Running { .. } => {}
            AdmissionState::Terminal { .. } => return Ok(()),
            _ => return Err(StoreError::Corrupt("session manager append state")),
        }
        let admission = self
            .state
            .status(id)
            .map_err(admission_store_error)?
            .clone();
        if matches!(admission.state, AdmissionState::InputAppended { .. }) {
            let before = self.state.revision;
            self.state
                .queue_run(id, before, admission.revision)
                .map_err(admission_store_error)?;
            self.store.persist(before, &self.state)?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn start(&mut self, id: &AdmissionId) -> Result<(), StoreError> {
        if matches!(
            &self.state.status(id).map_err(admission_store_error)?.state,
            AdmissionState::Running { .. }
        ) {
            return Ok(());
        }
        let before = self.state.revision;
        match self
            .state
            .start_next(before)
            .map_err(admission_store_error)?
        {
            NextRunnableV1::Start { admission_id, .. } if &admission_id == id => {
                self.store.persist(before, &self.state)
            }
            NextRunnableV1::Start { .. } | NextRunnableV1::None => Err(StoreError::Busy),
        }
    }

    pub(super) fn next_runnable(&self) -> Result<Option<(AdmissionId, SessionId)>, StoreError> {
        match self.state.next_runnable().map_err(admission_store_error)? {
            NextRunnableV1::Start {
                admission_id,
                session_id,
                ..
            } => Ok(Some((admission_id, session_id))),
            NextRunnableV1::None => Ok(None),
        }
    }

    pub(super) fn start_next(&mut self) -> Result<Option<(AdmissionId, SessionId)>, StoreError> {
        let before = self.state.revision;
        match self
            .state
            .start_next(before)
            .map_err(admission_store_error)?
        {
            NextRunnableV1::Start {
                admission_id,
                session_id,
                ..
            } => {
                self.store.persist(before, &self.state)?;
                Ok(Some((admission_id, session_id)))
            }
            NextRunnableV1::None => Ok(None),
        }
    }

    pub(super) fn terminal(
        &mut self,
        id: &AdmissionId,
        status: TerminalStatus,
    ) -> Result<(), StoreError> {
        let admission = self
            .state
            .status(id)
            .map_err(admission_store_error)?
            .clone();
        if matches!(admission.state, AdmissionState::Terminal { .. }) {
            return Ok(());
        }
        let before = self.state.revision;
        self.state
            .terminal(id, before, admission.revision, status)
            .map_err(admission_store_error)?;
        self.store.persist(before, &self.state)
    }

    #[cfg(test)]
    pub(super) fn persisted_state(&self) -> Result<Option<SessionManagerV1>, StoreError> {
        self.store.load()
    }

    pub(super) fn summary(&self) -> SessionManagerSummaryV2 {
        let sidechats = self.state.sessions.len().saturating_sub(1);
        let sessions = self
            .state
            .session_list()
            .into_iter()
            .filter_map(|session| {
                let (kind, source_session_id) = match &session.init {
                    polaris_core::session_manager::SessionInitV1::Current => {
                        (ManagedSessionKindV2::Current, None)
                    }
                    polaris_core::session_manager::SessionInitV1::EmptySidechat {
                        parent_session,
                        ..
                    } => (
                        ManagedSessionKindV2::Sidechat,
                        WireSessionId::new(parent_session.as_str()).ok(),
                    ),
                };
                let state = if session.active_run.is_some() {
                    ManagedSessionStateV2::Running
                } else if self.state.queue.iter().any(|id| {
                    self.state
                        .admissions
                        .get(id)
                        .and_then(|admission| destination_session(&admission.state))
                        == Some(&session.id)
                }) {
                    ManagedSessionStateV2::Queued
                } else {
                    ManagedSessionStateV2::Idle
                };
                Some(ManagedSessionSummaryV2 {
                    session_id: WireSessionId::new(session.id.as_str()).ok()?,
                    kind,
                    state,
                    source_session_id,
                })
            })
            .collect::<Vec<_>>();
        SessionManagerSummaryV2 {
            manager_revision: DecimalU64::new(self.state.revision),
            session_count: DecimalU64::new(sessions.len() as u64),
            session_total: DecimalU64::new(self.state.sessions.len() as u64),
            session_omitted: sessions.len() != self.state.sessions.len(),
            sessions,
            sidechat: match sidechats {
                0 => SidechatStateV2::None,
                _ if self.state.sessions.len() >= polaris_core::session_manager::MAX_SESSIONS => {
                    SidechatStateV2::Full
                }
                _ => SidechatStateV2::Active,
            },
            queued_admissions: DecimalU64::new(self.state.queue.len() as u64),
            active_runs: DecimalU64::new(
                self.state
                    .session_list()
                    .into_iter()
                    .filter(|session| session.active_run.is_some())
                    .count() as u64,
            ),
        }
    }

    pub(super) fn input_status(
        &self,
        id: &WireAdmissionId,
    ) -> Result<InputStatusResult, StoreError> {
        let id = AdmissionId::new(id.as_str()).map_err(admission_store_error)?;
        let admission = self.state.status(&id).map_err(admission_store_error)?;
        let destination = destination_session(&admission.state)
            .map(|id| WireSessionId::new(id.as_str()).map_err(|_| StoreError::Corrupt("session")))
            .transpose()?;
        let session_revision = destination
            .as_ref()
            .and_then(|wire| {
                self.state
                    .session_list()
                    .into_iter()
                    .find(|session| session.id.as_str() == wire.as_str())
            })
            .map_or(0, |session| session.session_revision);
        let run = matches!(
            admission.state,
            AdmissionState::RunQueued { .. }
                | AdmissionState::Running { .. }
                | AdmissionState::Terminal {
                    run_id: Some(_),
                    ..
                }
        )
        .then(|| super::admission_target(&admission.id));
        Ok(InputStatusResult {
            manager_revision: DecimalU64::new(self.state.revision),
            admission_id: WireAdmissionId::new(admission.id.as_str())
                .map_err(|_| StoreError::Corrupt("admission"))?,
            source_session_id: WireSessionId::new(admission.parent_session.as_str())
                .map_err(|_| StoreError::Corrupt("session"))?,
            state: wire_state(&admission.state),
            session_revision: DecimalU64::new(session_revision),
            route: wire_route(admission.payload.route),
            route_reason: admission.route_reason.as_ref().map(wire_reason),
            destination_session_id: destination,
            run,
        })
    }

    pub(super) fn contains_session(&self, id: &WireSessionId) -> bool {
        self.state
            .sessions
            .keys()
            .any(|session| session.as_str() == id.as_str())
    }

    pub(super) fn cancel(
        &mut self,
        id: &WireAdmissionId,
        expected_manager_revision: DecimalU64,
    ) -> Result<InputStatusResult, StoreError> {
        let internal = AdmissionId::new(id.as_str()).map_err(admission_store_error)?;
        let admission = self
            .state
            .status(&internal)
            .map_err(admission_store_error)?
            .clone();
        let before = expected_manager_revision.get();
        self.state
            .cancel(&internal, before, admission.revision)
            .map_err(admission_store_error)?;
        if self.state.revision != before {
            self.store.persist(before, &self.state)?;
        }
        self.input_status(id)
    }

    pub(super) fn session_list(&self) -> Result<SessionListResult, StoreError> {
        let mut sessions = Vec::with_capacity(self.state.sessions.len());
        for session in self.state.session_list() {
            let (kind, source_session_id) = match &session.init {
                polaris_core::session_manager::SessionInitV1::Current => {
                    (ManagedSessionKindV2::Current, None)
                }
                polaris_core::session_manager::SessionInitV1::EmptySidechat {
                    parent_session,
                    ..
                } => (
                    ManagedSessionKindV2::Sidechat,
                    Some(
                        WireSessionId::new(parent_session.as_str())
                            .map_err(|_| StoreError::Corrupt("session"))?,
                    ),
                ),
            };
            sessions.push(SessionListEntry {
                session_id: WireSessionId::new(session.id.as_str())
                    .map_err(|_| StoreError::Corrupt("session"))?,
                kind,
                session_revision: DecimalU64::new(session.session_revision),
                goal_revision: DecimalU64::new(session.goal_revision),
                source_session_id,
                // The manager run ID is intentionally not a desktop RunTarget;
                // Engine overlays its exact active wire target below.
                active_run: None,
            });
        }
        Ok(SessionListResult {
            manager_revision: DecimalU64::new(self.state.revision),
            session_total: DecimalU64::new(sessions.len() as u64),
            session_omitted: false,
            sessions,
        })
    }

    #[cfg(test)]
    fn observe(
        &mut self,
        session_revision: DecimalU64,
        goal_revision: DecimalU64,
    ) -> Result<(), StoreError> {
        let before = self.state.revision;
        self.state
            .observe_session_revisions(
                &self.current,
                before,
                session_revision.get(),
                goal_revision.get(),
            )
            .map_err(admission_store_error)?;
        if self.state.revision != before {
            self.store.persist_observed(
                before,
                &self.state,
                &self.current,
                session_revision.get(),
                goal_revision.get(),
            )?;
        }
        Ok(())
    }

    #[cfg(test)]
    fn route_current_fallback(&mut self, id: &AdmissionId) -> Result<(), StoreError> {
        self.route(id, None)
    }

    fn route(
        &mut self,
        id: &AdmissionId,
        judgement: Option<RoutingJudgementV1>,
    ) -> Result<(), StoreError> {
        let admission = self
            .state
            .status(id)
            .map_err(admission_store_error)?
            .clone();
        if matches!(admission.state, AdmissionState::Reserved)
            && admission.payload.route == DestinationMode::Auto
        {
            let before = self.state.revision;
            self.state
                .begin_classification(id, before, admission.revision)
                .map_err(admission_store_error)?;
            self.store.persist(before, &self.state)?;
        }
        let admission = self
            .state
            .status(id)
            .map_err(admission_store_error)?
            .clone();
        if matches!(
            admission.state,
            AdmissionState::Reserved | AdmissionState::Classifying
        ) {
            let before = self.state.revision;
            self.state
                .decide(id, before, admission.revision, judgement, false, false)
                .map_err(admission_store_error)?;
            self.store.persist(before, &self.state)?;
        }
        let admission = self
            .state
            .status(id)
            .map_err(admission_store_error)?
            .clone();
        if matches!(admission.state, AdmissionState::Decided { .. }) {
            let before = self.state.revision;
            self.state
                .prepare_destination(id, before, admission.revision)
                .map_err(admission_store_error)?;
            self.store.persist(before, &self.state)?;
        }
        Ok(())
    }
}

fn destination_session(state: &AdmissionState) -> Option<&SessionId> {
    match state {
        AdmissionState::DestinationReady { session_id, .. }
        | AdmissionState::InputAppended { session_id }
        | AdmissionState::RunQueued { session_id, .. }
        | AdmissionState::Running { session_id, .. }
        | AdmissionState::Terminal {
            session_id: Some(session_id),
            ..
        } => Some(session_id),
        _ => None,
    }
}

fn wire_route(route: DestinationMode) -> InputRoute {
    match route {
        DestinationMode::Current => InputRoute::Current,
        DestinationMode::New => InputRoute::New,
        DestinationMode::Auto => InputRoute::Auto,
    }
}

fn wire_state(state: &AdmissionState) -> InputAdmissionState {
    match state {
        AdmissionState::Reserved => InputAdmissionState::Reserved,
        AdmissionState::Classifying => InputAdmissionState::Classifying,
        AdmissionState::Decided { .. } => InputAdmissionState::Decided,
        AdmissionState::DestinationReady { .. } => InputAdmissionState::DestinationReady,
        AdmissionState::InputAppended { .. } => InputAdmissionState::InputAppended,
        AdmissionState::RunQueued { .. } => InputAdmissionState::RunQueued,
        AdmissionState::Running { .. } => InputAdmissionState::Running,
        AdmissionState::Terminal { status, .. } => match status {
            TerminalStatus::Completed => InputAdmissionState::Completed,
            TerminalStatus::Failed => InputAdmissionState::Failed,
            TerminalStatus::Cancelled => InputAdmissionState::Cancelled,
            TerminalStatus::OutcomeUnknown => InputAdmissionState::OutcomeUnknown,
        },
    }
}

fn wire_reason(reason: &polaris_core::session_manager::RouteReason) -> InputRouteReason {
    match reason {
        polaris_core::session_manager::RouteReason::ExplicitCurrent => {
            InputRouteReason::ExplicitCurrent
        }
        polaris_core::session_manager::RouteReason::ExplicitNew => InputRouteReason::ExplicitNew,
        polaris_core::session_manager::RouteReason::SameContext => InputRouteReason::SameContext,
        polaris_core::session_manager::RouteReason::DistinctContext => {
            InputRouteReason::DistinctContext
        }
        polaris_core::session_manager::RouteReason::Ambiguous => InputRouteReason::Ambiguous,
        polaris_core::session_manager::RouteReason::ClassifierUnavailable => {
            InputRouteReason::ClassifierUnavailable
        }
        polaris_core::session_manager::RouteReason::ClassifierInvalid => {
            InputRouteReason::ClassifierInvalid
        }
        polaris_core::session_manager::RouteReason::Cancelled => InputRouteReason::Cancelled,
        polaris_core::session_manager::RouteReason::ApprovalReply => {
            InputRouteReason::ApprovalReply
        }
        polaris_core::session_manager::RouteReason::ContextChanged => {
            InputRouteReason::ContextChanged
        }
        polaris_core::session_manager::RouteReason::CrashRecovery => {
            InputRouteReason::CrashRecovery
        }
    }
}

fn admission_store_error(error: AdmissionError) -> StoreError {
    match error {
        AdmissionError::RevisionConflict | AdmissionError::AdmissionRevisionConflict => {
            StoreError::CasConflict("session manager")
        }
        AdmissionError::SessionBusy | AdmissionError::QueueFull | AdmissionError::SessionLimit => {
            StoreError::Busy
        }
        AdmissionError::UnknownSession | AdmissionError::UnknownAdmission => StoreError::NotFound,
        AdmissionError::AdmissionCollision => StoreError::RequestConflict,
        AdmissionError::Capacity(kind) => StoreError::Capacity(kind),
        AdmissionError::OutcomeUnknown => StoreError::RecoveryRequired,
        _ => StoreError::Corrupt("session manager transition"),
    }
}

fn admission_restore_error(_: AdmissionError) -> StoreError {
    StoreError::Corrupt("session manager state")
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use polaris_core::session_manager::{ContextRelation, RoutingJudgementV1};
    use polaris_desktop_protocol::ids::SessionId as WireSessionId;
    use std::{fs, os::unix::fs::PermissionsExt};

    fn root() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        root
    }

    fn desktop_root(root: &tempfile::TempDir) -> polaris_core::desktop_store::DesktopRoot {
        polaris_core::desktop_store::DesktopRoot::open_owned(root.path()).unwrap()
    }

    fn draft(text: &str) -> Draft {
        Draft {
            draft_revision: DecimalU64::new(1),
            text: text.into(),
            attachment_ids: Vec::new(),
        }
    }

    #[test]
    fn current_run_is_persisted_from_admission_through_terminal() {
        let root = root();
        let session = WireSessionId::new("session-current").unwrap();
        let store_root = desktop_root(&root);
        let mut owner = AdmissionOwner::open(
            &store_root,
            &session,
            DecimalU64::new(0),
            DecimalU64::new(0),
        )
        .unwrap();
        let id = owner
            .admit_current(
                &draft("implement this"),
                DecimalU64::new(0),
                DecimalU64::new(0),
                "request-1",
            )
            .unwrap();
        owner.input_appended_and_queue(&id).unwrap();
        owner.start(&id).unwrap();
        assert_eq!(owner.summary().active_runs.get(), 1);
        owner.terminal(&id, TerminalStatus::Completed).unwrap();
        assert_eq!(owner.summary().active_runs.get(), 0);
        let revision = owner.summary().manager_revision;
        drop(owner);

        let store_root = desktop_root(&root);
        let reopened = AdmissionOwner::open(
            &store_root,
            &session,
            DecimalU64::new(1),
            DecimalU64::new(0),
        )
        .unwrap();
        assert_eq!(reopened.summary().manager_revision, revision);
        assert_eq!(reopened.summary().queued_admissions.get(), 0);
    }

    #[test]
    fn high_confidence_new_route_creates_only_one_empty_sidechat_slot() {
        let root = root();
        let session = WireSessionId::new("session-current").unwrap();
        let store_root = desktop_root(&root);
        let mut owner = AdmissionOwner::open(
            &store_root,
            &session,
            DecimalU64::new(0),
            DecimalU64::new(0),
        )
        .unwrap();
        owner
            .admit_with_judgement(
                &draft("unrelated subject"),
                DecimalU64::new(0),
                DecimalU64::new(0),
                RoutingJudgementV1 {
                    relation: ContextRelation::DistinctContext,
                    probability: Some(0.99),
                    confidence: Some(0.99),
                },
            )
            .unwrap();
        let summary = owner.summary();
        assert_eq!(summary.session_count.get(), 2);
        assert_eq!(summary.sidechat, SidechatStateV2::Full);
        assert_eq!(summary.active_runs.get(), 0);
        // Submission stays queued until it starts or is cancelled.
        assert_eq!(summary.queued_admissions.get(), 1);
    }

    #[test]
    fn an_unavailable_classifier_is_recorded_once_as_current_fallback() {
        let root = root();
        let session = WireSessionId::new("session-current").unwrap();
        let store_root = desktop_root(&root);
        let mut owner = AdmissionOwner::open(
            &store_root,
            &session,
            DecimalU64::new(0),
            DecimalU64::new(0),
        )
        .unwrap();
        let id = owner
            .admit_current(
                &draft("keep current"),
                DecimalU64::new(0),
                DecimalU64::new(0),
                "request-1",
            )
            .unwrap();
        let revision = owner.summary().manager_revision;
        let replay = owner
            .admit_current(
                &draft("keep current"),
                DecimalU64::new(0),
                DecimalU64::new(0),
                "request-1",
            )
            .unwrap();
        assert_eq!(replay, id);
        assert_eq!(owner.summary().manager_revision, revision);
    }
}
