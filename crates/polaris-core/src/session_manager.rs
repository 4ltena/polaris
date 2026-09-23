//! Durable, side-effect-free admission and multi-session scheduling state.
//!
//! This module deliberately does not own an engine, a writer, or a provider.
//! An owner persists each returned state transition before it appends input or
//! starts a run.  Keeping that boundary here makes a crash recoverable without
//! re-classifying, re-appending, or re-sending a user turn.

use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_SESSIONS: usize = 2;
pub const MAX_QUEUED_RUNS: usize = 32;
pub const MAX_COMPLETED_TURNS: usize = 10;
pub const MAX_INPUT_BYTES: usize = 32 * 1024;
pub const MAX_GOAL_BYTES: usize = 4 * 1024;
pub const MAX_TURN_BYTES: usize = 4 * 1024;
pub const MAX_ATTACHMENTS: usize = 16;
const MAX_ID_BYTES: usize = 128;
const HASH_BYTES: usize = 64;

#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    pub fn new(value: impl Into<String>) -> Result<Self, AdmissionError> {
        let value = value.into();
        validate_id("session_id", &value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct AdmissionId(String);

impl AdmissionId {
    /// Stable for the same parent session and canonical admission payload.
    pub fn derive(parent: &SessionId, payload_hash: &str) -> Self {
        let mut hash = Sha256::new();
        hash.update(b"polaris-admission-v1\0");
        hash.update(parent.as_str().as_bytes());
        hash.update(b"\0");
        hash.update(payload_hash.as_bytes());
        Self(format!("adm-{:x}", hash.finalize()))
    }

    pub fn new(value: impl Into<String>) -> Result<Self, AdmissionError> {
        let value = value.into();
        validate_id("admission_id", &value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct RunId(String);

impl RunId {
    fn derive(admission: &AdmissionId) -> Self {
        let mut hash = Sha256::new();
        hash.update(b"polaris-admission-run-v1\0");
        hash.update(admission.as_str().as_bytes());
        Self(format!("run-{:x}", hash.finalize()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DestinationMode {
    Current,
    New,
    Auto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AdmissionDestination {
    Current,
    New,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum RouteReason {
    ExplicitCurrent,
    ExplicitNew,
    SameContext,
    DistinctContext,
    Ambiguous,
    ClassifierUnavailable,
    ClassifierInvalid,
    Cancelled,
    ApprovalReply,
    ContextChanged,
    CrashRecovery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ContextRelation {
    SameContext,
    DistinctContext,
    Unclear,
}

/// The only classifier result that can create a sidechat is a complete,
/// high-confidence `distinct_context` answer.  The caller records transport
/// failures as `None`, never as a guessed result.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingJudgementV1 {
    pub relation: ContextRelation,
    pub probability: Option<f64>,
    pub confidence: Option<f64>,
}

impl RoutingJudgementV1 {
    pub fn destination(&self) -> Result<Option<AdmissionDestination>, AdmissionError> {
        validate_probability("probability", self.probability)?;
        validate_probability("confidence", self.confidence)?;
        match self.relation {
            ContextRelation::DistinctContext
                if self.probability.is_some_and(|value| value >= 0.90)
                    && self.confidence.is_some_and(|value| value >= 0.90) =>
            {
                Ok(Some(AdmissionDestination::New))
            }
            ContextRelation::DistinctContext | ContextRelation::Unclear => Ok(None),
            ContextRelation::SameContext
                if self.probability.is_some() && self.confidence.is_some() =>
            {
                Ok(Some(AdmissionDestination::Current))
            }
            ContextRelation::SameContext => Ok(None),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletedUserTurnV1 {
    pub turn_id: String,
    pub text: String,
}

impl CompletedUserTurnV1 {
    pub fn new(
        turn_id: impl Into<String>,
        text: impl Into<String>,
    ) -> Result<Self, AdmissionError> {
        let value = Self {
            turn_id: turn_id.into(),
            text: text.into(),
        };
        validate_id("turn_id", &value.turn_id)?;
        validate_text("completed turn", &value.text, MAX_TURN_BYTES)?;
        Ok(value)
    }
}

/// Bounded input to Jev.  It is a projection only and is never a replacement
/// for persisted raw conversation history.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingContextV1 {
    pub goal: String,
    pub completed_user_turns: Vec<CompletedUserTurnV1>,
}

impl RoutingContextV1 {
    pub fn new(
        goal: impl Into<String>,
        completed_user_turns: impl IntoIterator<Item = CompletedUserTurnV1>,
    ) -> Result<Self, AdmissionError> {
        let goal = goal.into();
        validate_text("goal", &goal, MAX_GOAL_BYTES)?;
        let mut completed_user_turns: Vec<_> = completed_user_turns.into_iter().collect();
        if completed_user_turns.len() > MAX_COMPLETED_TURNS {
            let skip = completed_user_turns.len() - MAX_COMPLETED_TURNS;
            completed_user_turns.drain(..skip);
        }
        for turn in &completed_user_turns {
            validate_id("turn_id", &turn.turn_id)?;
            validate_text("completed turn", &turn.text, MAX_TURN_BYTES)?;
        }
        Ok(Self {
            goal,
            completed_user_turns,
        })
    }

    pub fn validate(&self) -> Result<(), AdmissionError> {
        if self.completed_user_turns.len() > MAX_COMPLETED_TURNS {
            return Err(AdmissionError::Limit("completed user turns"));
        }
        validate_text("goal", &self.goal, MAX_GOAL_BYTES)?;
        for turn in &self.completed_user_turns {
            validate_id("turn_id", &turn.turn_id)?;
            validate_text("completed turn", &turn.text, MAX_TURN_BYTES)?;
        }
        Ok(())
    }
}

/// Classify one admission against the same bounded context on every host.
///
/// Transport failures, incomplete typed answers, and invalid choices return
/// `None`; [`SessionManagerV1::decide`] owns the deterministic current-session
/// fallback. Keeping the typed question here prevents Desktop and TUI from
/// silently drifting to different sidechat semantics.
pub async fn classify_context(
    decider: &dyn polaris_provider::decision::Decider,
    context: &RoutingContextV1,
    current_input: &str,
) -> Option<RoutingJudgementV1> {
    use polaris_provider::decision::Question;

    context.validate().ok()?;
    validate_text("input", current_input, MAX_INPUT_BYTES).ok()?;
    let state = serde_json::json!({
        "routing_context": context,
        "current_input": current_input,
    });
    let questions = BTreeMap::from([(
        "context_relation".to_owned(),
        Question::Choice {
            instructions: "Classify whether this input continues the current conversation context. Return exactly one typed relation.".into(),
            criteria: BTreeMap::from([
                ("same_context".into(), "It continues or refines the current goal and recent completed user turns.".into()),
                ("distinct_context".into(), "It is a separate task that should start with empty isolated context.".into()),
                ("unclear".into(), "The bounded context is insufficient to decide safely.".into()),
            ]),
        },
    )]);
    let decision = decider.decide(&state, &questions).await.ok()?;
    let answer = decision.answers.get("context_relation")?;
    let choice = answer.choice.as_deref()?;
    let relation = match choice {
        "same_context" => ContextRelation::SameContext,
        "distinct_context" => ContextRelation::DistinctContext,
        "unclear" => ContextRelation::Unclear,
        _ => return None,
    };
    Some(RoutingJudgementV1 {
        relation,
        probability: answer.probabilities.get(choice).copied(),
        confidence: answer.confidence,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmissionPayloadV1 {
    pub text: String,
    #[serde(default)]
    pub attachment_ids: Vec<String>,
    pub route: DestinationMode,
    pub expected_session_revision: u64,
    pub expected_goal_revision: u64,
}

impl SubmissionPayloadV1 {
    pub fn payload_hash(&self) -> Result<String, AdmissionError> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(|_| AdmissionError::InvalidPayload)?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
    }

    pub fn validate(&self) -> Result<(), AdmissionError> {
        validate_text("input", &self.text, MAX_INPUT_BYTES)?;
        if self.attachment_ids.len() > MAX_ATTACHMENTS {
            return Err(AdmissionError::Limit("attachments"));
        }
        for attachment in &self.attachment_ids {
            validate_id("attachment_id", attachment)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitRequestV1 {
    pub parent_session: SessionId,
    pub payload: SubmissionPayloadV1,
    /// A client may persist this deterministic value before retrying.  If
    /// absent, `submit` derives it from parent plus payload hash.
    #[serde(default)]
    pub admission_id: Option<AdmissionId>,
}

impl SubmitRequestV1 {
    pub fn derived_admission_id(&self) -> Result<AdmissionId, AdmissionError> {
        if let Some(provided) = &self.admission_id {
            // `AdmissionId` validated its bounded wire-safe shape when it was
            // constructed.  A client may retain this ID before sending, then
            // recover an ambiguous response through `input.status`.  `submit`
            // still binds it to the exact parent and canonical payload hash.
            return Ok(provided.clone());
        }
        Ok(AdmissionId::derive(
            &self.parent_session,
            &self.payload.payload_hash()?,
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum TerminalStatus {
    Completed,
    Failed,
    Cancelled,
    OutcomeUnknown,
}

/// Forward-only receipt state.  No variant allows an already appended input
/// to return to a state which could append it again.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "state", deny_unknown_fields)]
pub enum AdmissionState {
    Reserved,
    Classifying,
    Decided {
        destination: AdmissionDestination,
        reason: RouteReason,
    },
    DestinationReady {
        session_id: SessionId,
        reason: RouteReason,
    },
    InputAppended {
        session_id: SessionId,
    },
    RunQueued {
        session_id: SessionId,
        run_id: RunId,
    },
    Running {
        session_id: SessionId,
        run_id: RunId,
    },
    Terminal {
        session_id: Option<SessionId>,
        run_id: Option<RunId>,
        status: TerminalStatus,
    },
}

impl AdmissionState {
    fn session_id(&self) -> Option<&SessionId> {
        match self {
            Self::DestinationReady { session_id, .. }
            | Self::InputAppended { session_id }
            | Self::RunQueued { session_id, .. }
            | Self::Running { session_id, .. }
            | Self::Terminal {
                session_id: Some(session_id),
                ..
            } => Some(session_id),
            _ => None,
        }
    }

    fn run_id(&self) -> Option<&RunId> {
        match self {
            Self::RunQueued { run_id, .. }
            | Self::Running { run_id, .. }
            | Self::Terminal {
                run_id: Some(run_id),
                ..
            } => Some(run_id),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionV1 {
    pub id: AdmissionId,
    pub revision: u64,
    pub payload_hash: String,
    pub parent_session: SessionId,
    pub payload: SubmissionPayloadV1,
    #[serde(default)]
    pub route_reason: Option<RouteReason>,
    pub state: AdmissionState,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum SessionInitV1 {
    Current,
    EmptySidechat {
        created_from_admission_id: AdmissionId,
        parent_session: SessionId,
        /// These explicit false values make a copied parent state invalid.
        inherit_history: bool,
        inherit_memory: bool,
        inherit_approval: bool,
        inherit_tool_state: bool,
    },
}

impl SessionInitV1 {
    fn empty_sidechat(admission: AdmissionId, parent: SessionId) -> Self {
        Self::EmptySidechat {
            created_from_admission_id: admission,
            parent_session: parent,
            inherit_history: false,
            inherit_memory: false,
            inherit_approval: false,
            inherit_tool_state: false,
        }
    }

    fn validate(&self) -> Result<(), AdmissionError> {
        if let Self::EmptySidechat {
            created_from_admission_id,
            parent_session,
            inherit_history,
            inherit_memory,
            inherit_approval,
            inherit_tool_state,
            ..
        } = self
        {
            validate_id("admission_id", created_from_admission_id.as_str())?;
            validate_id("session_id", parent_session.as_str())?;
            if *inherit_history || *inherit_memory || *inherit_approval || *inherit_tool_state {
                return Err(AdmissionError::InvalidRestore(
                    "sidechat inherits parent state",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedSessionV1 {
    pub id: SessionId,
    pub init: SessionInitV1,
    /// Owner-observed revision of the durable session contents.  Admissions
    /// bind their routing snapshot to this value before any classifier call
    /// or input append is allowed.
    #[serde(default)]
    pub session_revision: u64,
    /// Owner-observed revision of the current goal.  Goal changes do not
    /// necessarily change the raw session revision, so they are tracked
    /// independently.
    #[serde(default)]
    pub goal_revision: u64,
    pub active_run: Option<RunId>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionManagerV1 {
    pub version: u8,
    pub revision: u64,
    pub sessions: BTreeMap<SessionId, ManagedSessionV1>,
    pub admissions: BTreeMap<AdmissionId, AdmissionV1>,
    /// FIFO non-running admission IDs. The ID is inserted at durable submit,
    /// so a slow Auto classification cannot be overtaken by a later explicit
    /// route. The head becomes runnable only after reaching `RunQueued`.
    pub queue: Vec<AdmissionId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitOutcomeV1 {
    Accepted {
        admission: AdmissionV1,
        idempotent: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NextRunnableV1 {
    Start {
        admission_id: AdmissionId,
        session_id: SessionId,
        run_id: RunId,
    },
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelOutcomeV1 {
    Cancelled {
        admission_id: AdmissionId,
        run_id: Option<RunId>,
    },
    AlreadyTerminal {
        admission_id: AdmissionId,
    },
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum AdmissionError {
    #[error("{0} is invalid")]
    Invalid(&'static str),
    #[error("{0} exceeds its limit")]
    Limit(&'static str),
    #[error("invalid admission payload")]
    InvalidPayload,
    #[error("session does not exist")]
    UnknownSession,
    #[error("admission does not exist")]
    UnknownAdmission,
    #[error("admission id conflicts with another payload")]
    AdmissionCollision,
    #[error("manager revision conflict")]
    RevisionConflict,
    #[error("admission revision conflict")]
    AdmissionRevisionConflict,
    #[error("transition is not allowed from the current admission state")]
    InvalidTransition,
    #[error("sidechat session limit reached")]
    SessionLimit,
    #[error("run queue is full")]
    QueueFull,
    #[error("session already has an active run")]
    SessionBusy,
    #[error("submission session revision is stale")]
    SessionRevisionConflict,
    #[error("submission goal revision is stale")]
    GoalRevisionConflict,
    #[error("invalid restored manager: {0}")]
    InvalidRestore(&'static str),
}

impl SessionManagerV1 {
    pub fn new(current_session: SessionId) -> Self {
        let session = ManagedSessionV1 {
            id: current_session.clone(),
            init: SessionInitV1::Current,
            session_revision: 0,
            goal_revision: 0,
            active_run: None,
        };
        Self {
            version: 1,
            revision: 0,
            sessions: BTreeMap::from([(current_session, session)]),
            admissions: BTreeMap::new(),
            queue: Vec::new(),
        }
    }

    pub fn submit(
        &mut self,
        request: SubmitRequestV1,
        expected_manager_revision: u64,
    ) -> Result<SubmitOutcomeV1, AdmissionError> {
        request.payload.validate()?;
        if !self.sessions.contains_key(&request.parent_session) {
            return Err(AdmissionError::UnknownSession);
        }
        let payload_hash = request.payload.payload_hash()?;
        let id = request.derived_admission_id()?;
        if let Some(existing) = self.admissions.get(&id) {
            if existing.payload_hash == payload_hash
                && existing.parent_session == request.parent_session
            {
                return Ok(SubmitOutcomeV1::Accepted {
                    admission: existing.clone(),
                    idempotent: true,
                });
            }
            return Err(AdmissionError::AdmissionCollision);
        }
        self.ensure_revision(expected_manager_revision)?;
        let waiting = self
            .admissions
            .values()
            .filter(|admission| {
                !matches!(
                    admission.state,
                    AdmissionState::Running { .. } | AdmissionState::Terminal { .. }
                )
            })
            .count();
        if waiting >= MAX_QUEUED_RUNS {
            return Err(AdmissionError::QueueFull);
        }
        let admission = AdmissionV1 {
            id: id.clone(),
            revision: 0,
            payload_hash,
            parent_session: request.parent_session,
            payload: request.payload,
            route_reason: None,
            state: AdmissionState::Reserved,
        };
        self.admissions.insert(id.clone(), admission.clone());
        self.queue.push(id);
        self.bump();
        Ok(SubmitOutcomeV1::Accepted {
            admission,
            idempotent: false,
        })
    }

    /// Publishes the durable revisions observed by the owner.  This is the
    /// only way external session/goal changes enter the admission model, so
    /// subsequent classification and append guards cannot silently use stale
    /// submission preconditions.
    pub fn observe_session_revisions(
        &mut self,
        id: &SessionId,
        expected_manager_revision: u64,
        session_revision: u64,
        goal_revision: u64,
    ) -> Result<(), AdmissionError> {
        self.ensure_revision(expected_manager_revision)?;
        let session = self
            .sessions
            .get_mut(id)
            .ok_or(AdmissionError::UnknownSession)?;
        if session_revision < session.session_revision || goal_revision < session.goal_revision {
            return Err(AdmissionError::InvalidRestore("session revision regressed"));
        }
        if session.session_revision == session_revision && session.goal_revision == goal_revision {
            return Ok(());
        }
        session.session_revision = session_revision;
        session.goal_revision = goal_revision;
        self.bump();
        Ok(())
    }

    pub fn begin_classification(
        &mut self,
        id: &AdmissionId,
        expected_manager_revision: u64,
        expected_admission_revision: u64,
    ) -> Result<AdmissionV1, AdmissionError> {
        self.ensure_revision(expected_manager_revision)?;
        self.ensure_submission_context(id)?;
        let admission = self.admission_mut(id, expected_admission_revision)?;
        match &admission.state {
            AdmissionState::Reserved => admission.state = AdmissionState::Classifying,
            AdmissionState::Classifying => return Ok(admission.clone()),
            _ => return Err(AdmissionError::InvalidTransition),
        }
        admission.revision += 1;
        let result = admission.clone();
        self.bump();
        Ok(result)
    }

    /// Records a classifier result or a deterministic fallback.  It never
    /// calls a model and it never reuses a result after a restart.
    pub fn decide(
        &mut self,
        id: &AdmissionId,
        expected_manager_revision: u64,
        expected_admission_revision: u64,
        judgement: Option<RoutingJudgementV1>,
        approval_reply: bool,
        cancelled: bool,
    ) -> Result<AdmissionV1, AdmissionError> {
        self.ensure_revision(expected_manager_revision)?;
        self.ensure_submission_context(id)?;
        let admission = self.admission_mut(id, expected_admission_revision)?;
        let (destination, reason) = match admission.payload.route {
            DestinationMode::Current => {
                (AdmissionDestination::Current, RouteReason::ExplicitCurrent)
            }
            DestinationMode::New => (AdmissionDestination::New, RouteReason::ExplicitNew),
            DestinationMode::Auto if approval_reply => {
                (AdmissionDestination::Current, RouteReason::ApprovalReply)
            }
            DestinationMode::Auto if cancelled => {
                (AdmissionDestination::Current, RouteReason::Cancelled)
            }
            DestinationMode::Auto => match judgement {
                Some(value) => match value.destination() {
                    Ok(Some(AdmissionDestination::New)) => {
                        (AdmissionDestination::New, RouteReason::DistinctContext)
                    }
                    Ok(Some(AdmissionDestination::Current)) => {
                        (AdmissionDestination::Current, RouteReason::SameContext)
                    }
                    Ok(None) => (AdmissionDestination::Current, RouteReason::Ambiguous),
                    Err(_) => (
                        AdmissionDestination::Current,
                        RouteReason::ClassifierInvalid,
                    ),
                },
                None => (
                    AdmissionDestination::Current,
                    RouteReason::ClassifierUnavailable,
                ),
            },
        };
        match &admission.state {
            AdmissionState::Reserved if admission.payload.route != DestinationMode::Auto => {}
            AdmissionState::Classifying => {}
            AdmissionState::Decided {
                destination: old,
                reason: old_reason,
            } if *old == destination && *old_reason == reason => return Ok(admission.clone()),
            _ => return Err(AdmissionError::InvalidTransition),
        }
        admission.state = AdmissionState::Decided {
            destination,
            reason,
        };
        admission.route_reason = Some(reason);
        admission.revision += 1;
        let result = admission.clone();
        self.bump();
        Ok(result)
    }

    /// A persisted `Classifying` state means the process might have sent Jev
    /// just before a crash.  Recovery always chooses the current session and
    /// never sends that classification again.
    pub fn recover_classifying(&mut self) -> Result<usize, AdmissionError> {
        self.validate()?;
        let mut changed = 0;
        for admission in self.admissions.values_mut() {
            if admission.state == AdmissionState::Classifying {
                admission.state = AdmissionState::Decided {
                    destination: AdmissionDestination::Current,
                    reason: RouteReason::CrashRecovery,
                };
                admission.route_reason = Some(RouteReason::CrashRecovery);
                admission.revision += 1;
                changed += 1;
            }
        }
        if changed != 0 {
            self.bump();
        }
        Ok(changed)
    }

    /// Publishes the required no-retry fallback when the parent session CAS
    /// changed while Jev was in flight.  The owner performs that CAS; this
    /// model records its outcome without attempting another classification.
    pub fn decide_context_changed(
        &mut self,
        id: &AdmissionId,
        expected_manager_revision: u64,
        expected_admission_revision: u64,
    ) -> Result<AdmissionV1, AdmissionError> {
        self.ensure_revision(expected_manager_revision)?;
        let state = self
            .admissions
            .get(id)
            .ok_or(AdmissionError::UnknownAdmission)
            .and_then(|admission| {
                if admission.revision == expected_admission_revision {
                    Ok(admission.state.clone())
                } else {
                    Err(AdmissionError::AdmissionRevisionConflict)
                }
            })?;
        match &state {
            AdmissionState::Reserved
            | AdmissionState::Classifying
            | AdmissionState::Decided { .. } => {}
            AdmissionState::DestinationReady { .. } => {}
            _ => return Err(AdmissionError::InvalidTransition),
        }
        if matches!(
            state,
            AdmissionState::Decided {
                destination: AdmissionDestination::Current,
                reason: RouteReason::ContextChanged,
            }
        ) {
            return Ok(self.status(id)?.clone());
        }
        let abandoned_sidechat = match &state {
            AdmissionState::DestinationReady { session_id, .. } => Some(session_id.clone()),
            _ => None,
        };
        let admission = self.admission_mut(id, expected_admission_revision)?;
        admission.state = AdmissionState::Decided {
            destination: AdmissionDestination::Current,
            reason: RouteReason::ContextChanged,
        };
        admission.route_reason = Some(RouteReason::ContextChanged);
        admission.revision += 1;
        let result = admission.clone();
        if let Some(session_id) = abandoned_sidechat {
            self.remove_unused_sidechat(id, &session_id);
        }
        self.bump();
        Ok(result)
    }

    pub fn prepare_destination(
        &mut self,
        id: &AdmissionId,
        expected_manager_revision: u64,
        expected_admission_revision: u64,
    ) -> Result<AdmissionV1, AdmissionError> {
        self.ensure_revision(expected_manager_revision)?;
        let (parent, destination, reason) = {
            let admission = self
                .admissions
                .get(id)
                .ok_or(AdmissionError::UnknownAdmission)?;
            if admission.revision != expected_admission_revision {
                return Err(AdmissionError::AdmissionRevisionConflict);
            }
            match &admission.state {
                AdmissionState::Decided {
                    destination,
                    reason,
                } => (admission.parent_session.clone(), *destination, *reason),
                AdmissionState::DestinationReady { .. } => return Ok(admission.clone()),
                _ => return Err(AdmissionError::InvalidTransition),
            }
        };
        let target = match destination {
            AdmissionDestination::Current => parent,
            AdmissionDestination::New => {
                if self.sessions.len() >= MAX_SESSIONS {
                    return Err(AdmissionError::SessionLimit);
                }
                let session_id = sidechat_session_id(id);
                if let Some(existing) = self.sessions.get(&session_id) {
                    match &existing.init {
                        SessionInitV1::EmptySidechat {
                            created_from_admission_id,
                            parent_session,
                            ..
                        } if created_from_admission_id == id && parent_session == &parent => {}
                        _ => return Err(AdmissionError::InvalidRestore("sidechat id collision")),
                    }
                } else {
                    self.sessions.insert(
                        session_id.clone(),
                        ManagedSessionV1 {
                            id: session_id.clone(),
                            init: SessionInitV1::empty_sidechat(id.clone(), parent),
                            session_revision: 0,
                            goal_revision: 0,
                            active_run: None,
                        },
                    );
                }
                session_id
            }
        };
        let admission = self.admission_mut(id, expected_admission_revision)?;
        admission.state = AdmissionState::DestinationReady {
            session_id: target,
            reason,
        };
        admission.revision += 1;
        let result = admission.clone();
        self.bump();
        Ok(result)
    }

    /// The owner calls this only after it atomically persists the raw input
    /// and admission index in the destination session.
    pub fn record_input_appended(
        &mut self,
        id: &AdmissionId,
        expected_manager_revision: u64,
        expected_admission_revision: u64,
        session_id: &SessionId,
    ) -> Result<AdmissionV1, AdmissionError> {
        self.ensure_revision(expected_manager_revision)?;
        let state = self
            .admissions
            .get(id)
            .ok_or(AdmissionError::UnknownAdmission)
            .and_then(|admission| {
                if admission.revision == expected_admission_revision {
                    Ok(admission.state.clone())
                } else {
                    Err(AdmissionError::AdmissionRevisionConflict)
                }
            })?;
        match &state {
            AdmissionState::InputAppended {
                session_id: destination,
            }
            | AdmissionState::RunQueued {
                session_id: destination,
                ..
            }
            | AdmissionState::Running {
                session_id: destination,
                ..
            }
            | AdmissionState::Terminal {
                session_id: Some(destination),
                ..
            } if destination == session_id => return Ok(self.status(id)?.clone()),
            AdmissionState::DestinationReady {
                session_id: destination,
                ..
            } if destination == session_id => {}
            _ => return Err(AdmissionError::InvalidTransition),
        }
        let allow_context_changed = matches!(
            state,
            AdmissionState::DestinationReady {
                reason: RouteReason::ContextChanged,
                ..
            }
        );
        if !allow_context_changed {
            self.ensure_submission_context(id)?;
        }
        let admission = self.admission_mut(id, expected_admission_revision)?;
        admission.state = AdmissionState::InputAppended {
            session_id: session_id.clone(),
        };
        admission.revision += 1;
        let result = admission.clone();
        let destination = self
            .sessions
            .get_mut(session_id)
            .ok_or(AdmissionError::InvalidRestore("append destination missing"))?;
        destination.session_revision = destination
            .session_revision
            .checked_add(1)
            .expect("session revision overflow");
        self.bump();
        Ok(result)
    }

    /// Recovery-only counterpart to `record_input_appended`.  The root owner
    /// may call this only after the destination Writer proves the exact
    /// admission ID, payload hash and destination were committed together.
    /// That proof supersedes the pre-append revision guard because the append
    /// itself is the revision change being recovered.
    pub fn recover_input_appended(
        &mut self,
        id: &AdmissionId,
        expected_manager_revision: u64,
        expected_admission_revision: u64,
        session_id: &SessionId,
    ) -> Result<AdmissionV1, AdmissionError> {
        self.ensure_revision(expected_manager_revision)?;
        let admission = self.admission_mut(id, expected_admission_revision)?;
        match &admission.state {
            AdmissionState::DestinationReady {
                session_id: destination,
                ..
            } if destination == session_id => {}
            AdmissionState::InputAppended {
                session_id: destination,
            }
            | AdmissionState::RunQueued {
                session_id: destination,
                ..
            }
            | AdmissionState::Running {
                session_id: destination,
                ..
            } if destination == session_id => return Ok(admission.clone()),
            _ => return Err(AdmissionError::InvalidTransition),
        }
        admission.state = AdmissionState::InputAppended {
            session_id: session_id.clone(),
        };
        admission.revision += 1;
        let result = admission.clone();
        let destination = self
            .sessions
            .get_mut(session_id)
            .ok_or(AdmissionError::InvalidRestore("append destination missing"))?;
        destination.session_revision = destination
            .session_revision
            .max(result.payload.expected_session_revision.saturating_add(1));
        self.bump();
        Ok(result)
    }

    pub fn queue_run(
        &mut self,
        id: &AdmissionId,
        expected_manager_revision: u64,
        expected_admission_revision: u64,
    ) -> Result<AdmissionV1, AdmissionError> {
        self.ensure_revision(expected_manager_revision)?;
        let state = self
            .admissions
            .get(id)
            .ok_or(AdmissionError::UnknownAdmission)
            .and_then(|admission| {
                if admission.revision == expected_admission_revision {
                    Ok(admission.state.clone())
                } else {
                    Err(AdmissionError::AdmissionRevisionConflict)
                }
            })?;
        let session_id = match state {
            AdmissionState::InputAppended { session_id } => session_id,
            AdmissionState::RunQueued { .. } | AdmissionState::Running { .. } => {
                return Ok(self.status(id)?.clone());
            }
            _ => return Err(AdmissionError::InvalidTransition),
        };
        let admission = self.admission_mut(id, expected_admission_revision)?;
        let run_id = RunId::derive(id);
        admission.state = AdmissionState::RunQueued { session_id, run_id };
        admission.revision += 1;
        let result = admission.clone();
        self.bump();
        Ok(result)
    }

    pub fn next_runnable(&self) -> Result<NextRunnableV1, AdmissionError> {
        let Some(id) = self.queue.first() else {
            return Ok(NextRunnableV1::None);
        };
        let admission = self
            .admissions
            .get(id)
            .ok_or(AdmissionError::InvalidRestore("queue admission missing"))?;
        let AdmissionState::RunQueued { session_id, run_id } = &admission.state else {
            return Ok(NextRunnableV1::None);
        };
        let session = self
            .sessions
            .get(session_id)
            .ok_or(AdmissionError::InvalidRestore("queued session missing"))?;
        if session.active_run.is_some() || self.active_run_count() >= MAX_SESSIONS {
            return Ok(NextRunnableV1::None);
        }
        Ok(NextRunnableV1::Start {
            admission_id: id.clone(),
            session_id: session_id.clone(),
            run_id: run_id.clone(),
        })
    }

    /// Commits the reservation returned from `next_runnable`.  The expected
    /// revisions make a stale scheduler observation harmless.
    pub fn start_next(
        &mut self,
        expected_manager_revision: u64,
    ) -> Result<NextRunnableV1, AdmissionError> {
        self.ensure_revision(expected_manager_revision)?;
        let candidate = self.next_runnable()?;
        let NextRunnableV1::Start {
            admission_id,
            session_id,
            run_id,
        } = candidate
        else {
            return Ok(NextRunnableV1::None);
        };
        let popped = self.queue.remove(0);
        debug_assert_eq!(popped, admission_id);
        self.sessions
            .get_mut(&session_id)
            .ok_or(AdmissionError::InvalidRestore("queued session missing"))?
            .active_run = Some(run_id.clone());
        let admission = self
            .admissions
            .get_mut(&admission_id)
            .ok_or(AdmissionError::UnknownAdmission)?;
        admission.state = AdmissionState::Running {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
        };
        admission.revision += 1;
        self.bump();
        Ok(NextRunnableV1::Start {
            admission_id,
            session_id,
            run_id,
        })
    }

    pub fn terminal(
        &mut self,
        id: &AdmissionId,
        expected_manager_revision: u64,
        expected_admission_revision: u64,
        status: TerminalStatus,
    ) -> Result<AdmissionV1, AdmissionError> {
        self.ensure_revision(expected_manager_revision)?;
        let (session_id, run_id) = {
            let admission = self
                .admissions
                .get(id)
                .ok_or(AdmissionError::UnknownAdmission)?;
            if admission.revision != expected_admission_revision {
                return Err(AdmissionError::AdmissionRevisionConflict);
            }
            match &admission.state {
                AdmissionState::Running { session_id, run_id } => {
                    (Some(session_id.clone()), Some(run_id.clone()))
                }
                AdmissionState::Terminal { .. } => return Ok(admission.clone()),
                _ => return Err(AdmissionError::InvalidTransition),
            }
        };
        if let (Some(session_id), Some(run_id)) = (&session_id, &run_id) {
            let session = self
                .sessions
                .get_mut(session_id)
                .ok_or(AdmissionError::InvalidRestore("running session missing"))?;
            if session.active_run.as_ref() != Some(run_id) {
                return Err(AdmissionError::InvalidRestore(
                    "running session run mismatch",
                ));
            }
            session.active_run = None;
        }
        let admission = self.admission_mut(id, expected_admission_revision)?;
        admission.state = AdmissionState::Terminal {
            session_id,
            run_id,
            status,
        };
        admission.revision += 1;
        let result = admission.clone();
        self.bump();
        Ok(result)
    }

    /// Cancellation consumes the admission.  A running run ID is returned so
    /// the owner can forward exactly one cancellation to that engine; it is
    /// never requeued automatically after recovery.
    pub fn cancel(
        &mut self,
        id: &AdmissionId,
        expected_manager_revision: u64,
        expected_admission_revision: u64,
    ) -> Result<CancelOutcomeV1, AdmissionError> {
        self.ensure_revision(expected_manager_revision)?;
        let state = {
            let admission = self
                .admissions
                .get(id)
                .ok_or(AdmissionError::UnknownAdmission)?;
            if admission.revision != expected_admission_revision {
                return Err(AdmissionError::AdmissionRevisionConflict);
            }
            admission.state.clone()
        };
        if matches!(state, AdmissionState::Terminal { .. }) {
            return Ok(CancelOutcomeV1::AlreadyTerminal {
                admission_id: id.clone(),
            });
        }
        self.queue.retain(|queued| queued != id);
        let session_id = state.session_id().cloned();
        let run_id = state.run_id().cloned();
        let prepared_sidechat = match &state {
            AdmissionState::DestinationReady { session_id, .. } => Some(session_id.clone()),
            _ => None,
        };
        if let (Some(session_id), Some(run_id)) = (&session_id, &run_id)
            && let Some(session) = self.sessions.get_mut(session_id)
            && session.active_run.as_ref() == Some(run_id)
        {
            session.active_run = None;
        }
        let discard_prepared_sidechat = prepared_sidechat
            .as_ref()
            .is_some_and(|sidechat| self.sidechat_is_unused_for(id, sidechat));
        let terminal_session_id = if discard_prepared_sidechat {
            None
        } else {
            session_id.clone()
        };
        let admission = self.admission_mut(id, expected_admission_revision)?;
        admission.state = AdmissionState::Terminal {
            session_id: terminal_session_id,
            run_id: run_id.clone(),
            status: TerminalStatus::Cancelled,
        };
        admission.revision += 1;
        if discard_prepared_sidechat && let Some(sidechat) = prepared_sidechat {
            self.remove_unused_sidechat(id, &sidechat);
        }
        self.bump();
        Ok(CancelOutcomeV1::Cancelled {
            admission_id: id.clone(),
            run_id,
        })
    }

    pub fn status(&self, id: &AdmissionId) -> Result<&AdmissionV1, AdmissionError> {
        self.admissions
            .get(id)
            .ok_or(AdmissionError::UnknownAdmission)
    }

    pub fn session_list(&self) -> Vec<&ManagedSessionV1> {
        self.sessions.values().collect()
    }

    /// Validation for data loaded from a durable store.  It does not mutate
    /// state; callers run `recover_classifying` only after this succeeds.
    pub fn validate(&self) -> Result<(), AdmissionError> {
        if self.version != 1 || self.sessions.is_empty() || self.sessions.len() > MAX_SESSIONS {
            return Err(AdmissionError::InvalidRestore("manager shape"));
        }
        if self.queue.len() > MAX_QUEUED_RUNS {
            return Err(AdmissionError::InvalidRestore("queue limit"));
        }
        let mut queue_ids = BTreeSet::new();
        for id in &self.queue {
            if !queue_ids.insert(id) {
                return Err(AdmissionError::InvalidRestore("duplicate queue entry"));
            }
            let admission = self
                .admissions
                .get(id)
                .ok_or(AdmissionError::InvalidRestore("queue admission missing"))?;
            if matches!(
                &admission.state,
                AdmissionState::Running { .. } | AdmissionState::Terminal { .. }
            ) {
                return Err(AdmissionError::InvalidRestore(
                    "queue contains started admission",
                ));
            }
        }
        let mut active = BTreeSet::new();
        for (id, session) in &self.sessions {
            if id != &session.id {
                return Err(AdmissionError::InvalidRestore("session key mismatch"));
            }
            validate_id("session_id", id.as_str())?;
            session.init.validate()?;
            if let Some(run) = &session.active_run
                && !active.insert(run.clone())
            {
                return Err(AdmissionError::InvalidRestore("duplicate active run"));
            }
        }
        if active.len() > MAX_SESSIONS {
            return Err(AdmissionError::InvalidRestore("active run limit"));
        }
        for (id, admission) in &self.admissions {
            if id != &admission.id
                || validate_id("admission_id", id.as_str()).is_err()
                || !valid_hash(&admission.payload_hash)
            {
                return Err(AdmissionError::InvalidRestore("admission identity"));
            }
            admission.payload.validate()?;
            if admission.payload.payload_hash()? != admission.payload_hash {
                return Err(AdmissionError::InvalidRestore("admission payload hash"));
            }
            if !self.sessions.contains_key(&admission.parent_session) {
                return Err(AdmissionError::InvalidRestore("admission parent missing"));
            }
            if let Some(session_id) = admission.state.session_id()
                && !self.sessions.contains_key(session_id)
            {
                return Err(AdmissionError::InvalidRestore(
                    "admission destination missing",
                ));
            }
            if let AdmissionState::Decided { reason, .. }
            | AdmissionState::DestinationReady { reason, .. } = &admission.state
                && admission.route_reason.as_ref() != Some(reason)
            {
                return Err(AdmissionError::InvalidRestore("route reason mismatch"));
            }
            match &admission.state {
                AdmissionState::Reserved
                | AdmissionState::Classifying
                | AdmissionState::Decided { .. }
                | AdmissionState::DestinationReady { .. }
                | AdmissionState::InputAppended { .. }
                | AdmissionState::RunQueued { .. }
                    if !queue_ids.contains(id) =>
                {
                    return Err(AdmissionError::InvalidRestore("waiting admission absent"));
                }
                AdmissionState::Running { session_id, run_id } => {
                    let session = self
                        .sessions
                        .get(session_id)
                        .ok_or(AdmissionError::InvalidRestore("running session missing"))?;
                    if session.active_run.as_ref() != Some(run_id) {
                        return Err(AdmissionError::InvalidRestore(
                            "running session run mismatch",
                        ));
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn ensure_revision(&self, expected: u64) -> Result<(), AdmissionError> {
        if self.revision == expected {
            Ok(())
        } else {
            Err(AdmissionError::RevisionConflict)
        }
    }

    fn admission_mut(
        &mut self,
        id: &AdmissionId,
        expected: u64,
    ) -> Result<&mut AdmissionV1, AdmissionError> {
        let admission = self
            .admissions
            .get_mut(id)
            .ok_or(AdmissionError::UnknownAdmission)?;
        if admission.revision != expected {
            return Err(AdmissionError::AdmissionRevisionConflict);
        }
        Ok(admission)
    }

    fn ensure_submission_context(&self, id: &AdmissionId) -> Result<(), AdmissionError> {
        let admission = self
            .admissions
            .get(id)
            .ok_or(AdmissionError::UnknownAdmission)?;
        let session = self
            .sessions
            .get(&admission.parent_session)
            .ok_or(AdmissionError::UnknownSession)?;
        if session.session_revision != admission.payload.expected_session_revision {
            return Err(AdmissionError::SessionRevisionConflict);
        }
        if session.goal_revision != admission.payload.expected_goal_revision {
            return Err(AdmissionError::GoalRevisionConflict);
        }
        Ok(())
    }

    /// A sidechat may be removed only before its input is appended and only
    /// when no other receipt still references it.  This keeps cancellation of
    /// a prepared `New` route from leaking empty sessions while preserving any
    /// sidechat that might already contain durable user data.
    fn sidechat_is_unused_for(&self, admission_id: &AdmissionId, session_id: &SessionId) -> bool {
        self.sessions.get(session_id).is_some_and(|session| {
            matches!(
                &session.init,
                SessionInitV1::EmptySidechat {
                    created_from_admission_id,
                    ..
                } if created_from_admission_id == admission_id
            ) && session.active_run.is_none()
        }) && self.admissions.iter().all(|(other_id, admission)| {
            other_id == admission_id || admission.state.session_id() != Some(session_id)
        })
    }

    fn remove_unused_sidechat(&mut self, admission_id: &AdmissionId, session_id: &SessionId) {
        if self.sidechat_is_unused_for(admission_id, session_id) {
            self.sessions.remove(session_id);
        }
    }

    fn active_run_count(&self) -> usize {
        self.sessions
            .values()
            .filter(|session| session.active_run.is_some())
            .count()
    }

    fn bump(&mut self) {
        self.revision = self
            .revision
            .checked_add(1)
            .expect("manager revision overflow");
    }
}

fn sidechat_session_id(admission: &AdmissionId) -> SessionId {
    // v2 conversation storage uses UUID-shaped IDs. Deriving all 128 bits
    // from the admission keeps retry/recovery deterministic while making the
    // sidechat directly openable by every existing resume path.
    let mut digest = Sha256::new();
    digest.update(b"polaris-sidechat-session-v1\0");
    digest.update(admission.as_str().as_bytes());
    let mut bytes: [u8; 16] = digest.finalize()[..16]
        .try_into()
        .expect("sha256 prefix is exactly 16 bytes");
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    SessionId(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    ))
}

fn validate_text(kind: &'static str, value: &str, maximum: usize) -> Result<(), AdmissionError> {
    if value.is_empty() || value.len() > maximum || value.contains('\0') {
        return Err(if value.len() > maximum {
            AdmissionError::Limit(kind)
        } else {
            AdmissionError::Invalid(kind)
        });
    }
    Ok(())
}

fn validate_id(kind: &'static str, value: &str) -> Result<(), AdmissionError> {
    if value.is_empty()
        || value.len() > MAX_ID_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(AdmissionError::Invalid(kind));
    }
    Ok(())
}

fn valid_hash(value: &str) -> bool {
    value.len() == HASH_BYTES && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validate_probability(kind: &'static str, value: Option<f64>) -> Result<(), AdmissionError> {
    if value.is_some_and(|number| !number.is_finite() || !(0.0..=1.0).contains(&number)) {
        return Err(AdmissionError::Invalid(kind));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_provider::decision::{Answer, Decision, DecisionError, Question};

    struct RoutingAnswer(&'static str, f64, f64);

    #[async_trait::async_trait]
    impl polaris_provider::decision::Decider for RoutingAnswer {
        async fn decide(
            &self,
            state: &serde_json::Value,
            questions: &BTreeMap<String, Question>,
        ) -> Result<Decision, DecisionError> {
            assert_eq!(
                state["routing_context"]["completed_user_turns"]
                    .as_array()
                    .unwrap()
                    .len(),
                10
            );
            assert_eq!(questions.len(), 1);
            Ok(Decision {
                answers: BTreeMap::from([(
                    "context_relation".into(),
                    Answer {
                        choice: Some(self.0.into()),
                        probabilities: BTreeMap::from([(self.0.into(), self.1)]),
                        confidence: Some(self.2),
                        ..Answer::default()
                    },
                )]),
                input_tokens: Some(10),
                output_tokens: Some(1),
            })
        }
    }

    fn current() -> SessionId {
        SessionId::new("current").unwrap()
    }

    #[tokio::test]
    async fn shared_classifier_keeps_the_bounded_context_and_typed_scores() {
        let turns = (0..12)
            .map(|index| CompletedUserTurnV1::new(format!("turn-{index}"), format!("text-{index}")))
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let context = RoutingContextV1::new("goal", turns).unwrap();
        let judgement = classify_context(
            &RoutingAnswer("distinct_context", 0.96, 0.94),
            &context,
            "an unrelated request",
        )
        .await
        .unwrap();
        assert_eq!(judgement.relation, ContextRelation::DistinctContext);
        assert_eq!(judgement.probability, Some(0.96));
        assert_eq!(judgement.confidence, Some(0.94));
        assert_eq!(
            judgement.destination().unwrap(),
            Some(AdmissionDestination::New)
        );
    }
    fn request(route: DestinationMode, text: &str) -> SubmitRequestV1 {
        SubmitRequestV1 {
            parent_session: current(),
            payload: SubmissionPayloadV1 {
                text: text.into(),
                attachment_ids: vec![],
                route,
                expected_session_revision: 0,
                expected_goal_revision: 0,
            },
            admission_id: None,
        }
    }
    fn submit(manager: &mut SessionManagerV1, route: DestinationMode, text: &str) -> AdmissionV1 {
        let mut request = request(route, text);
        let session = manager.sessions.get(&request.parent_session).unwrap();
        request.payload.expected_session_revision = session.session_revision;
        request.payload.expected_goal_revision = session.goal_revision;
        let revision = manager.revision;
        let SubmitOutcomeV1::Accepted { admission, .. } =
            manager.submit(request, revision).unwrap();
        admission
    }
    fn decide_auto(
        manager: &mut SessionManagerV1,
        admission: AdmissionV1,
        judgement: Option<RoutingJudgementV1>,
    ) -> AdmissionV1 {
        let classified = manager
            .begin_classification(&admission.id, manager.revision, admission.revision)
            .unwrap();
        manager
            .decide(
                &classified.id,
                manager.revision,
                classified.revision,
                judgement,
                false,
                false,
            )
            .unwrap()
    }

    #[test]
    fn topic_split_requires_both_thresholds() {
        let mut manager = SessionManagerV1::new(current());
        let admission = submit(&mut manager, DestinationMode::Auto, "plan a trip");
        let decided = decide_auto(
            &mut manager,
            admission,
            Some(RoutingJudgementV1 {
                relation: ContextRelation::DistinctContext,
                probability: Some(0.90),
                confidence: Some(0.90),
            }),
        );
        assert!(matches!(
            decided.state,
            AdmissionState::Decided {
                destination: AdmissionDestination::New,
                ..
            }
        ));
        let prepared = manager
            .prepare_destination(&decided.id, manager.revision, decided.revision)
            .unwrap();
        assert!(matches!(
            prepared.state,
            AdmissionState::DestinationReady { ref session_id, .. }
                if session_id != &current() && session_id.as_str().len() == 36
        ));
        let sidechat = prepared.state.session_id().unwrap();
        assert!(matches!(
            manager.sessions.get(sidechat).unwrap().init,
            SessionInitV1::EmptySidechat {
                inherit_history: false,
                inherit_memory: false,
                inherit_approval: false,
                inherit_tool_state: false,
                ..
            }
        ));
    }

    #[test]
    fn ambiguous_and_approval_reply_stay_current() {
        let mut manager = SessionManagerV1::new(current());
        let admission = submit(&mut manager, DestinationMode::Auto, "maybe new topic");
        let decided = decide_auto(
            &mut manager,
            admission,
            Some(RoutingJudgementV1 {
                relation: ContextRelation::DistinctContext,
                probability: Some(0.99),
                confidence: Some(0.89),
            }),
        );
        assert!(matches!(
            decided.state,
            AdmissionState::Decided {
                destination: AdmissionDestination::Current,
                reason: RouteReason::Ambiguous
            }
        ));
        let admission = submit(&mut manager, DestinationMode::Auto, "approved");
        let classified = manager
            .begin_classification(&admission.id, manager.revision, admission.revision)
            .unwrap();
        let decided = manager
            .decide(
                &classified.id,
                manager.revision,
                classified.revision,
                None,
                true,
                false,
            )
            .unwrap();
        assert!(matches!(
            decided.state,
            AdmissionState::Decided {
                destination: AdmissionDestination::Current,
                reason: RouteReason::ApprovalReply
            }
        ));
    }

    #[test]
    fn same_submit_is_idempotent_and_different_payload_collides() {
        let mut manager = SessionManagerV1::new(current());
        let mut original = request(DestinationMode::Current, "same");
        original.admission_id = Some(AdmissionId::new("adm-client-1").unwrap());
        let revision = manager.revision;
        let SubmitOutcomeV1::Accepted {
            admission,
            idempotent,
        } = manager.submit(original.clone(), revision).unwrap();
        assert!(!idempotent);
        let SubmitOutcomeV1::Accepted { idempotent, .. } =
            manager.submit(original, manager.revision).unwrap();
        assert!(idempotent);
        let mut conflict = request(DestinationMode::Current, "different");
        conflict.admission_id = Some(admission.id);
        assert_eq!(
            manager.submit(conflict, manager.revision),
            Err(AdmissionError::AdmissionCollision)
        );
    }

    #[test]
    fn client_admission_id_is_retained_for_ambiguous_response_recovery() {
        let mut manager = SessionManagerV1::new(current());
        let mut original = request(DestinationMode::Current, "recover by status");
        let client_id = AdmissionId::new("adm-018f-client-held").unwrap();
        original.admission_id = Some(client_id.clone());
        let SubmitOutcomeV1::Accepted { admission, .. } =
            manager.submit(original.clone(), manager.revision).unwrap();
        assert_eq!(admission.id, client_id);
        let SubmitOutcomeV1::Accepted {
            admission: replay,
            idempotent,
        } = manager.submit(original, manager.revision).unwrap();
        assert!(idempotent);
        assert_eq!(replay.id, client_id);
    }

    #[test]
    fn thirty_third_waiting_admission_is_rejected_before_ack() {
        let mut manager = SessionManagerV1::new(current());
        for index in 0..MAX_QUEUED_RUNS {
            let admission = submit(
                &mut manager,
                DestinationMode::Current,
                &format!("waiting-{index}"),
            );
            assert!(matches!(admission.state, AdmissionState::Reserved));
        }
        let revision = manager.revision;
        let request = request(DestinationMode::Current, "overflow");
        assert_eq!(
            manager.submit(request, revision),
            Err(AdmissionError::QueueFull)
        );
        assert_eq!(manager.revision, revision);
        assert_eq!(manager.admissions.len(), MAX_QUEUED_RUNS);
    }

    #[test]
    fn crash_classification_falls_back_without_resend() {
        let mut manager = SessionManagerV1::new(current());
        let admission = submit(&mut manager, DestinationMode::Auto, "new work");
        let classified = manager
            .begin_classification(&admission.id, manager.revision, admission.revision)
            .unwrap();
        assert_eq!(manager.recover_classifying().unwrap(), 1);
        let recovered = manager.status(&classified.id).unwrap();
        assert!(matches!(
            recovered.state,
            AdmissionState::Decided {
                destination: AdmissionDestination::Current,
                reason: RouteReason::CrashRecovery
            }
        ));
    }

    #[test]
    fn max_two_sessions_max_queue_and_one_active_run() {
        let mut capacity = SessionManagerV1::new(current());
        let first = submit(&mut capacity, DestinationMode::New, "one");
        let first = capacity
            .decide(
                &first.id,
                capacity.revision,
                first.revision,
                None,
                false,
                false,
            )
            .unwrap();
        let _first = capacity
            .prepare_destination(&first.id, capacity.revision, first.revision)
            .unwrap();
        let second = submit(&mut capacity, DestinationMode::New, "two");
        let second = capacity
            .decide(
                &second.id,
                capacity.revision,
                second.revision,
                None,
                false,
                false,
            )
            .unwrap();
        assert_eq!(
            capacity.prepare_destination(&second.id, capacity.revision, second.revision),
            Err(AdmissionError::SessionLimit)
        );

        let mut manager = SessionManagerV1::new(current());
        let queued = queue_current(&mut manager, "current");
        let started = manager.start_next(manager.revision).unwrap();
        assert!(matches!(started, NextRunnableV1::Start { .. }));
        let queued_again = queue_current(&mut manager, "current again");
        assert_eq!(manager.next_runnable().unwrap(), NextRunnableV1::None);
        assert!(matches!(queued.state, AdmissionState::RunQueued { .. }));
        assert!(matches!(
            queued_again.state,
            AdmissionState::RunQueued { .. }
        ));

        let mut filled = SessionManagerV1::new(current());
        for index in 0..MAX_QUEUED_RUNS {
            queue_current(&mut filled, &format!("queued {index}"));
        }
        let revision = filled.revision;
        assert_eq!(
            filled.submit(request(DestinationMode::Current, "over capacity"), revision,),
            Err(AdmissionError::QueueFull)
        );
    }

    #[test]
    fn fifo_and_duplicate_append_or_run_are_rejected_or_idempotent() {
        let mut manager = SessionManagerV1::new(current());
        let mut ids = Vec::new();
        for text in ["one", "two"] {
            let admission = submit(&mut manager, DestinationMode::Current, text);
            let admission = manager
                .decide(
                    &admission.id,
                    manager.revision,
                    admission.revision,
                    None,
                    false,
                    false,
                )
                .unwrap();
            let admission = manager
                .prepare_destination(&admission.id, manager.revision, admission.revision)
                .unwrap();
            let target = admission.state.session_id().unwrap().clone();
            let appended = manager
                .record_input_appended(&admission.id, manager.revision, admission.revision, &target)
                .unwrap();
            let duplicate = manager
                .record_input_appended(&appended.id, manager.revision, appended.revision, &target)
                .unwrap();
            assert_eq!(duplicate.revision, appended.revision);
            let queued = manager
                .queue_run(&appended.id, manager.revision, appended.revision)
                .unwrap();
            let duplicate = manager
                .queue_run(&queued.id, manager.revision, queued.revision)
                .unwrap();
            assert_eq!(duplicate.revision, queued.revision);
            ids.push(queued.id);
        }
        assert_eq!(manager.queue, ids);
        let NextRunnableV1::Start { admission_id, .. } =
            manager.start_next(manager.revision).unwrap()
        else {
            panic!("expected first")
        };
        assert_eq!(admission_id, ids[0]);
    }

    fn queue_current(manager: &mut SessionManagerV1, text: &str) -> AdmissionV1 {
        let admission = submit(manager, DestinationMode::Current, text);
        let admission = manager
            .decide(
                &admission.id,
                manager.revision,
                admission.revision,
                None,
                false,
                false,
            )
            .unwrap();
        let admission = manager
            .prepare_destination(&admission.id, manager.revision, admission.revision)
            .unwrap();
        let session_id = admission.state.session_id().unwrap().clone();
        let admission = manager
            .record_input_appended(
                &admission.id,
                manager.revision,
                admission.revision,
                &session_id,
            )
            .unwrap();
        manager
            .queue_run(&admission.id, manager.revision, admission.revision)
            .unwrap()
    }

    #[test]
    fn routing_context_is_last_ten_turns() {
        let turns =
            (0..12).map(|index| CompletedUserTurnV1::new(format!("turn-{index}"), "x").unwrap());
        let context = RoutingContextV1::new("goal", turns).unwrap();
        assert_eq!(context.completed_user_turns.len(), 10);
        assert_eq!(context.completed_user_turns[0].turn_id, "turn-2");
    }

    #[test]
    fn restore_rejects_a_sidechat_that_copies_parent_state() {
        let mut manager = SessionManagerV1::new(current());
        let sidechat = SessionId::new("sidechat-bad").unwrap();
        manager.sessions.insert(
            sidechat.clone(),
            ManagedSessionV1 {
                id: sidechat,
                init: SessionInitV1::EmptySidechat {
                    created_from_admission_id: AdmissionId::new("adm-source").unwrap(),
                    parent_session: current(),
                    inherit_history: true,
                    inherit_memory: false,
                    inherit_approval: false,
                    inherit_tool_state: false,
                },
                session_revision: 0,
                goal_revision: 0,
                active_run: None,
            },
        );
        assert_eq!(
            manager.validate(),
            Err(AdmissionError::InvalidRestore(
                "sidechat inherits parent state"
            ))
        );
    }

    #[test]
    fn queued_run_replay_succeeds_when_the_queue_is_full() {
        let mut manager = SessionManagerV1::new(current());
        for index in 0..MAX_QUEUED_RUNS {
            queue_current(&mut manager, &format!("queued {index}"));
        }
        let queued_id = manager.queue[0].clone();
        let queued = manager.status(&queued_id).unwrap().clone();
        assert!(matches!(queued.state, AdmissionState::RunQueued { .. }));

        let replay = manager
            .queue_run(&queued.id, manager.revision, queued.revision)
            .unwrap();
        assert_eq!(replay, queued);
        assert_eq!(manager.queue.len(), MAX_QUEUED_RUNS);
        assert_eq!(manager.queue[0], queued_id);
    }

    #[test]
    fn cancelling_prepared_new_route_removes_only_its_empty_sidechat() {
        let mut manager = SessionManagerV1::new(current());
        let admission = submit(&mut manager, DestinationMode::New, "separate work");
        let decided = manager
            .decide(
                &admission.id,
                manager.revision,
                admission.revision,
                None,
                false,
                false,
            )
            .unwrap();
        let prepared = manager
            .prepare_destination(&decided.id, manager.revision, decided.revision)
            .unwrap();
        let sidechat = prepared.state.session_id().unwrap().clone();
        assert!(manager.sessions.contains_key(&sidechat));

        let cancelled = manager
            .cancel(&prepared.id, manager.revision, prepared.revision)
            .unwrap();
        assert!(matches!(
            cancelled,
            CancelOutcomeV1::Cancelled { run_id: None, .. }
        ));
        assert!(!manager.sessions.contains_key(&sidechat));
        assert!(matches!(
            manager.status(&prepared.id).unwrap().state,
            AdmissionState::Terminal {
                session_id: None,
                status: TerminalStatus::Cancelled,
                ..
            }
        ));
        manager.validate().unwrap();
    }

    #[test]
    fn cancelling_after_sidechat_append_preserves_the_referenced_session() {
        let mut manager = SessionManagerV1::new(current());
        let admission = submit(&mut manager, DestinationMode::New, "keep this input");
        let decided = manager
            .decide(
                &admission.id,
                manager.revision,
                admission.revision,
                None,
                false,
                false,
            )
            .unwrap();
        let prepared = manager
            .prepare_destination(&decided.id, manager.revision, decided.revision)
            .unwrap();
        let sidechat = prepared.state.session_id().unwrap().clone();
        let appended = manager
            .record_input_appended(&prepared.id, manager.revision, prepared.revision, &sidechat)
            .unwrap();

        manager
            .cancel(&appended.id, manager.revision, appended.revision)
            .unwrap();
        assert!(manager.sessions.contains_key(&sidechat));
        assert!(matches!(
            manager.status(&appended.id).unwrap().state,
            AdmissionState::Terminal {
                session_id: Some(ref session_id),
                status: TerminalStatus::Cancelled,
                ..
            } if session_id == &sidechat
        ));
        manager.validate().unwrap();
    }

    #[test]
    fn stale_submission_revisions_block_classification_and_append() {
        let mut manager = SessionManagerV1::new(current());
        let admission = submit(&mut manager, DestinationMode::Auto, "analyse this");
        manager
            .observe_session_revisions(&current(), manager.revision, 1, 0)
            .unwrap();
        assert_eq!(
            manager.begin_classification(&admission.id, manager.revision, admission.revision),
            Err(AdmissionError::SessionRevisionConflict)
        );
        assert!(matches!(
            manager.status(&admission.id).unwrap().state,
            AdmissionState::Reserved
        ));

        let decided = manager
            .decide_context_changed(&admission.id, manager.revision, admission.revision)
            .unwrap();
        let prepared = manager
            .prepare_destination(&decided.id, manager.revision, decided.revision)
            .unwrap();
        let destination = prepared.state.session_id().unwrap().clone();
        // The explicit context-changed route is the one no-retry fallback;
        // it may append to the current session even though the original
        // classifier snapshot is stale.
        manager
            .record_input_appended(
                &prepared.id,
                manager.revision,
                prepared.revision,
                &destination,
            )
            .unwrap();

        let fresh = submit(&mut manager, DestinationMode::Current, "next");
        let decided = manager
            .decide(
                &fresh.id,
                manager.revision,
                fresh.revision,
                None,
                false,
                false,
            )
            .unwrap();
        let prepared = manager
            .prepare_destination(&decided.id, manager.revision, decided.revision)
            .unwrap();
        manager
            .observe_session_revisions(&current(), manager.revision, 3, 1)
            .unwrap();
        let destination = prepared.state.session_id().unwrap().clone();
        assert_eq!(
            manager.record_input_appended(
                &prepared.id,
                manager.revision,
                prepared.revision,
                &destination,
            ),
            Err(AdmissionError::SessionRevisionConflict)
        );
        // A goal-only change is distinguished from a raw session change.
        let fresh = submit(&mut manager, DestinationMode::Auto, "another");
        manager
            .observe_session_revisions(&current(), manager.revision, 3, 2)
            .unwrap();
        assert_eq!(
            manager.begin_classification(&fresh.id, manager.revision, fresh.revision),
            Err(AdmissionError::GoalRevisionConflict)
        );
    }
}
