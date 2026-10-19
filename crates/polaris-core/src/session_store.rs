//! Durable session attachment, generation checks, and tombstone-serialized writes.

use crate::{
    conversation_memory::published_view,
    conversation_state::{
        AdmissionReceiptV1, ConversationSnapshot, ConversationStateV2, ConversationStore,
        HistoryMode, RewindBoundaryV1, RewindEffectStatusV1, RewindEligibilityV1,
    },
    workflow::{
        ActiveEvidenceArtifactsV1, ConvergenceInputV1, ObservedEvidenceSubjectV1,
        build_owner_convergence, validate_owner_convergence,
    },
    workflow::{
        ArtifactRefV1, ArtifactStatusV1, ChangeContractV1, Phase, SessionWorkflow, WorkflowConfig,
        WorkflowMode,
    },
    workflow_artifact_store::WorkflowArtifactStore,
};
use polaris_memory::MemoryStore;
use polaris_provider::{Message, Role};
use std::{
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// Clones used by the UI's running turn share a cursor. A separately resumed
/// writer gets its own cursor and cannot silently overwrite a newer generation.
#[derive(Clone)]
pub struct PersistedSession {
    pub store: Arc<ConversationStore>,
    pub database: PathBuf,
    pub data_root: PathBuf,
    generation: Arc<Mutex<u64>>,
}

pub fn new_session_id() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
    bytes[6] = (bytes[6] & 15) | 64;
    bytes[8] = (bytes[8] & 63) | 128;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

impl PersistedSession {
    /// Private host roots for adjacent owner stores. Conversational tools
    /// never receive these paths; they are only passed into typed owners.
    pub fn verification_store_root(&self) -> PathBuf {
        self.data_root.clone()
    }
    #[cfg(any(unix, all(test, windows)))]
    pub fn create(
        data_root: &Path,
        database: &Path,
        project_id: &str,
        mode: HistoryMode,
        workflow: Option<&SessionWorkflow>,
    ) -> io::Result<Self> {
        Self::create_with_id(
            data_root,
            database,
            project_id,
            &new_session_id()?,
            mode,
            workflow,
        )
    }

    #[cfg(not(any(unix, all(test, windows))))]
    pub fn create(
        _data_root: &Path,
        _database: &Path,
        _project_id: &str,
        _mode: HistoryMode,
        _workflow: Option<&SessionWorkflow>,
    ) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "このplatformではセッション保存の保護を確認できません",
        ))
    }

    /// Create an empty durable conversation named by a host-owned admission.
    /// Ordinary callers should use [`Self::create`]; accepting an explicit ID
    /// lets the session-manager ledger and raw store share one recoverable
    /// sidechat identity.
    #[cfg(any(unix, all(test, windows)))]
    pub fn create_with_id(
        data_root: &Path,
        database: &Path,
        project_id: &str,
        session_id: &str,
        mode: HistoryMode,
        workflow: Option<&SessionWorkflow>,
    ) -> io::Result<Self> {
        let state = Self::new_state(project_id, session_id, mode, workflow)?;
        #[cfg(all(test, windows))]
        let _creation_guard = windows_creation_guard(data_root, &state)?;
        let mut memory = open_session_memory(database, true)?;
        // A structured session is saved only after its index migration reads back.
        if mode == HistoryMode::Structured10 {
            memory
                .enable_structured_memory()
                .map_err(io::Error::other)?;
            if memory
                .structured_memory_schema_version()
                .map_err(io::Error::other)?
                != Some(polaris_memory::structured::STRUCTURED_MEMORY_SCHEMA_VERSION)
            {
                return Err(io::Error::other(
                    "structured memory migration did not read back",
                ));
            }
        }
        drop(memory);
        let store = ConversationStore::create(data_root, state)?;
        Ok(Self {
            store: Arc::new(store),
            database: database.into(),
            data_root: data_root.into(),
            generation: Arc::new(Mutex::new(0)),
        })
    }

    #[cfg(not(any(unix, all(test, windows))))]
    pub fn create_with_id(
        _data_root: &Path,
        _database: &Path,
        _project_id: &str,
        _session_id: &str,
        _mode: HistoryMode,
        _workflow: Option<&SessionWorkflow>,
    ) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "このplatformではセッション保存の保護を確認できません",
        ))
    }

    /// Pure bootstrap state used by the switch owner's pre-start descriptor.
    pub(crate) fn new_state(
        project_id: &str,
        session_id: &str,
        mode: HistoryMode,
        workflow: Option<&SessionWorkflow>,
    ) -> io::Result<ConversationStateV2> {
        #[cfg(all(test, windows))]
        if workflow.is_some() {
            windows_refuse_owner()?;
        }
        let mut state = ConversationStateV2::new(project_id.into(), session_id.into())?;
        state.history_mode = mode;
        if let Some(workflow) = workflow {
            state.workflow_config = Some(
                crate::conversation_state::SavedWorkflowConfigV1::capture(&workflow.config),
            );
            state.workflow = workflow.state();
            state.gates = workflow.gates.clone();
            state.workflow_artifacts = workflow.artifact_graph().clone();
        }
        Ok(state)
    }

    pub fn open(data_root: &Path, database: &Path, project_id: &str, id: &str) -> io::Result<Self> {
        let store = ConversationStore::open(data_root, project_id, id)?;
        let initial = store.lock()?.snapshot()?;
        let generation = initial.state.generation;
        let this = Self {
            store: Arc::new(store),
            database: database.into(),
            data_root: data_root.into(),
            generation: Arc::new(Mutex::new(generation)),
        };
        this.snapshot()?;
        Ok(this)
    }

    fn cursor(&self) -> io::Result<std::sync::MutexGuard<'_, u64>> {
        self.generation
            .lock()
            .map_err(|_| io::Error::other("保存カーソルを取得できません"))
    }

    pub fn snapshot(&self) -> io::Result<ConversationSnapshot> {
        let cursor = self.cursor()?;
        let lock = self.store.lock()?;
        let snapshot = lock.snapshot()?;
        if snapshot.state.generation != *cursor {
            return Err(io::Error::other(
                "別の書き込みでセッションが更新されました。再開し直してください",
            ));
        }
        let mut memory = open_session_memory(&self.database, false)?;
        memory
            .with_conversation_marker(&published_view(&snapshot.state)?, || Ok(()))
            .map_err(io::Error::other)?;
        Ok(snapshot)
    }

    /// Accept only the exact snapshot produced by our async history operation.
    /// No locks are retained across the caller's model/embedding await.
    pub fn accept_prepared(&self, expected: &ConversationSnapshot) -> io::Result<()> {
        self.accept_owned_snapshot(expected, false)
    }

    pub fn accept_publication(&self, expected: &ConversationSnapshot) -> io::Result<()> {
        self.accept_owned_snapshot(expected, true)
    }

    fn accept_owned_snapshot(
        &self,
        expected: &ConversationSnapshot,
        published: bool,
    ) -> io::Result<()> {
        let mut cursor = self.cursor()?;
        let owned = if published {
            cursor
                .checked_add(1)
                .ok_or_else(|| io::Error::other("保存世代が上限です"))?
        } else {
            *cursor
        };
        if expected.state.generation != owned {
            return Err(io::Error::other("要求準備中に別の書き込みが発生しました"));
        }
        let lock = self.store.lock()?;
        lock.check_snapshot(expected)?;
        let mut memory = open_session_memory(&self.database, false)?;
        memory
            .with_conversation_marker(&published_view(&expected.state)?, || Ok(()))
            .map_err(io::Error::other)?;
        *cursor = owned;
        Ok(())
    }

    pub fn append(
        &self,
        message: Message,
        starts_turn: bool,
        workflow: Option<&SessionWorkflow>,
    ) -> io::Result<()> {
        self.update(&[(message, starts_turn)], workflow, None)
            .map(|_| ())
    }

    /// Return the identity produced by the same locked append/publication.
    pub(crate) fn append_input(
        &self,
        message: Message,
        workflow: Option<&SessionWorkflow>,
    ) -> io::Result<Option<crate::request_input::SourceBinding>> {
        self.update(&[(message, true)], workflow, None)
    }

    /// Publish a real input and its host-selected context together.
    pub(crate) fn append_input_with_supplement(
        &self,
        message: Message,
        supplement: Message,
        workflow: Option<&SessionWorkflow>,
    ) -> io::Result<Option<crate::request_input::SourceBinding>> {
        self.update(&[(message, true), (supplement, false)], workflow, None)
    }

    /// Append one accepted user input and publish its admission receipt in
    /// the same conversation marker generation. Exact retries return the
    /// original input binding without writing another raw event.
    pub(crate) fn append_admitted_input(
        &self,
        message: Message,
        workflow: Option<&SessionWorkflow>,
        admission_id: &crate::session_manager::AdmissionId,
        payload_hash: &str,
    ) -> io::Result<Option<crate::request_input::SourceBinding>> {
        self.update(
            &[(message, true)],
            workflow,
            Some((admission_id.as_str(), payload_hash)),
        )
    }

    /// The receipt identifies the first event. Both events are published in
    /// one marker generation, so recovery cannot see an input without its
    /// selected prompt result.
    pub(crate) fn append_admitted_input_with_supplement(
        &self,
        message: Message,
        supplement: Message,
        workflow: Option<&SessionWorkflow>,
        admission_id: &crate::session_manager::AdmissionId,
        payload_hash: &str,
    ) -> io::Result<Option<crate::request_input::SourceBinding>> {
        self.update(
            &[(message, true), (supplement, false)],
            workflow,
            Some((admission_id.as_str(), payload_hash)),
        )
    }

    /// Publishes a completed assistant/tool suffix and its workflow checkpoint
    /// under one writer lock and one state marker. Batch messages are never a
    /// new real user turn.
    pub fn append_batch(
        &self,
        messages: &[Message],
        workflow: Option<&SessionWorkflow>,
    ) -> io::Result<()> {
        let events: Vec<_> = messages
            .iter()
            .cloned()
            .map(|message| (message, false))
            .collect();
        self.update(&events, workflow, None).map(|_| ())
    }

    pub fn checkpoint(&self, workflow: Option<&SessionWorkflow>) -> io::Result<()> {
        self.update(&[], workflow, None).map(|_| ())
    }

    /// Publish exactly one workflow-only marker update.  Management owners use
    /// this instead of `checkpoint`: it compares the caller's complete
    /// preimage while holding the session lock, rejects every non-workflow
    /// field change, and never appends raw history.
    pub fn publish_workflow_change(
        &self,
        expected_generation: u64,
        before_state_hash: &str,
        candidate: ConversationStateV2,
    ) -> io::Result<ConversationSnapshot> {
        #[cfg(all(test, windows))]
        windows_refuse_owner()?;
        let mut cursor = self.cursor()?;
        let mut lock = self.store.lock()?;
        let before = lock.snapshot()?;
        if before.state.generation != *cursor || before.state.generation != expected_generation {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "workflow変更の保存世代が変わりました",
            ));
        }
        if crate::conversation_state::content_hash(
            &serde_json::to_vec(&before.state).map_err(io::Error::other)?,
        ) != before_state_hash
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "workflow変更の保存前stateが変わりました",
            ));
        }
        if before.state.raw_commit_offset != before.raw_offset
            || before.state.raw_commit_hash != before.raw_hash
        {
            return Err(io::Error::other("workflow変更前に未公開の原文があります"));
        }
        let expected_next = expected_generation
            .checked_add(1)
            .ok_or_else(|| io::Error::other("保存世代が上限です"))?;
        if candidate.generation != expected_generation
            || candidate.raw_commit_offset != before.raw_offset
            || candidate.raw_commit_hash != before.raw_hash
            || !workflow_only_change(&before.state, &candidate)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "workflow変更候補が専用保存契約に一致しません",
            ));
        }
        candidate.validate()?;
        let mut memory = open_session_memory(&self.database, false)?;
        memory
            .with_conversation_marker(&published_view(&before.state)?, || {
                lock.publish(&before, candidate)?;
                Ok(())
            })
            .map_err(io::Error::other)?;
        *cursor = expected_next;
        let published = lock.snapshot()?;
        if published.state.generation != expected_next
            || published.raw_offset != before.raw_offset
            || published.raw_hash != before.raw_hash
        {
            return Err(io::Error::other("workflow変更の公開結果が一致しません"));
        }
        memory
            .with_conversation_marker(&published_view(&published.state)?, || Ok(()))
            .map_err(io::Error::other)?;
        Ok(published)
    }

    /// Publish a complete preimage CAS for settings, independent draft and
    /// goal only. The mode exception delegates to proven-empty initialization;
    /// ordinary `LockedConversation::publish` never migrates a live session.
    pub fn publish_session_change(
        &self,
        expected_generation: u64,
        before_state_hash: &str,
        candidate: ConversationStateV2,
    ) -> io::Result<ConversationSnapshot> {
        #[cfg(all(test, windows))]
        crate::conversation_state::windows_state_admission(&candidate)?;
        let mut cursor = self.cursor()?;
        let mut lock = self.store.lock()?;
        let before = lock.snapshot()?;
        if before.state.generation != *cursor
            || before.state.generation != expected_generation
            || crate::conversation_state::content_hash(
                &serde_json::to_vec(&before.state).map_err(io::Error::other)?,
            ) != before_state_hash
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "セッション設定の保存前stateが変わりました",
            ));
        }
        if before.state.raw_commit_offset != before.raw_offset
            || before.state.raw_commit_hash != before.raw_hash
        {
            return Err(io::Error::other("セッション設定前に未公開の原文があります"));
        }
        let changes_mode = candidate.history_mode != before.state.history_mode;
        let mut same_mode = candidate.clone();
        same_mode.history_mode = before.state.history_mode;
        if !crate::session_settings::session_only_change(&before.state, &same_mode) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "セッション設定候補が専用保存契約に一致しません",
            ));
        }
        candidate.validate()?;
        if changes_mode {
            lock.validate_history_initialization(&before)?;
        }
        let expected_next = expected_generation
            .checked_add(1)
            .ok_or_else(|| io::Error::other("保存世代が上限です"))?;
        let mut memory = open_session_memory(&self.database, false)?;
        if changes_mode && candidate.history_mode == HistoryMode::Structured10 {
            memory
                .enable_structured_memory()
                .map_err(io::Error::other)?;
            if memory
                .structured_memory_schema_version()
                .map_err(io::Error::other)?
                != Some(polaris_memory::structured::STRUCTURED_MEMORY_SCHEMA_VERSION)
            {
                return Err(io::Error::other(
                    "structured memory migration did not read back",
                ));
            }
        }
        memory
            .with_conversation_marker(&published_view(&before.state)?, || {
                if changes_mode {
                    lock.initialize_history_mode(&before, candidate)?;
                } else {
                    lock.publish(&before, candidate)?;
                }
                Ok(())
            })
            .map_err(io::Error::other)?;
        *cursor = expected_next;
        let published = lock.snapshot()?;
        if published.state.generation != expected_next
            || published.raw_offset != before.raw_offset
            || published.raw_hash != before.raw_hash
        {
            return Err(io::Error::other("セッション設定の公開結果が一致しません"));
        }
        memory
            .with_conversation_marker(&published_view(&published.state)?, || Ok(()))
            .map_err(io::Error::other)?;
        Ok(published)
    }

    /// Persist only validated, reference-only verification checkpoint state.
    /// Normal persistence preserves `running`; only the restore path changes a
    /// crashed run to `outcome_unknown`.
    pub fn publish_verification_checkpoint(
        &self,
        expected_generation: u64,
        checkpoint: crate::verification::checkpoint::SavedVerificationCheckpointV1,
    ) -> io::Result<()> {
        checkpoint.validate().map_err(io::Error::other)?;
        self.update_workflow_artifacts(expected_generation, move |before, _store| {
            if checkpoint.workflow_revision != before.workflow.revision {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "verification checkpoint workflow revisionが変わりました",
                ));
            }
            if let Some(previous) = &before.verification_checkpoint {
                if !previous.same_preparation_basis(&checkpoint) {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "verification checkpointのCAS basisが変わりました",
                    ));
                }
                if previous.stage != checkpoint.stage
                    && !previous.stage.can_transition_to(checkpoint.stage)
                {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "verification checkpointのCAS stageが一致しません",
                    ));
                }
            } else if checkpoint.stage
                != crate::verification::VerificationCheckpointStageV1::Prepared
            {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "verification checkpointはpreparedから保存します",
                ));
            }
            let mut next = before.clone();
            next.verification_checkpoint = Some(checkpoint);
            Ok((next, ()))
        })
    }

    /// Stores a draft or ready contract body while holding the session writer
    /// lock, then advances only the graph marker.  A stale caller leaves at
    /// most an unreachable immutable body, never a dangling graph reference.
    pub fn publish_change_contract(
        &self,
        expected_generation: u64,
        contract: ChangeContractV1,
    ) -> io::Result<ArtifactRefV1> {
        if !matches!(
            contract.header.status,
            ArtifactStatusV1::Draft | ArtifactStatusV1::Ready
        ) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "change contractの承認はtrusted ownerだけが行えます",
            ));
        }
        let reference = contract.reference();
        self.update_workflow_artifacts(expected_generation, move |before, store| {
            let mut graph = before.workflow_artifacts.clone();
            let inserted = graph
                .insert_change_contract(&contract)
                .map_err(io::Error::other)?;
            store.put_change_contract(&contract)?;
            let mut next = before.clone();
            if inserted {
                next.workflow_artifacts = graph;
            }
            Ok((next, reference))
        })
    }

    /// Trusted approval always creates a new immutable revision.  The approved
    /// body is durable before its graph entry and active workflow reference are
    /// committed in the same session marker publication.
    pub fn approve_change_contract_trusted(
        &self,
        expected_generation: u64,
        reference: ArtifactRefV1,
    ) -> io::Result<SessionWorkflow> {
        self.update_workflow_artifacts(expected_generation, move |before, store| {
            let draft = store.load_change_contract(&before.workflow_artifacts, &reference)?;
            if !matches!(
                draft.header.status,
                ArtifactStatusV1::Draft | ArtifactStatusV1::Ready
            ) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "承認できるchange contract revisionではありません",
                ));
            }
            let approved = draft
                .revised(ArtifactStatusV1::Approved)
                .map_err(io::Error::other)?;
            let approved_reference = approved.reference();
            let mut graph = before.workflow_artifacts.clone();
            graph
                .insert_change_contract(&approved)
                .map_err(io::Error::other)?;
            store.put_change_contract(&approved)?;

            let config = before
                .workflow_config
                .as_ref()
                .map(|saved| saved.config.clone())
                .unwrap_or_else(WorkflowConfig::default);
            let mut gates = before.gates.clone();
            gates.phase_gate_schema_version =
                Some(crate::workflow::WORKFLOW_ARTIFACT_SCHEMA_VERSION);
            let restored = SessionWorkflow::restore_with_artifacts(
                config,
                gates,
                before.workflow.clone(),
                graph.clone(),
            )
            .map_err(io::Error::other)?;
            let workflow = restored
                .with_approved_change_contract(graph.clone(), approved_reference)
                .map_err(io::Error::other)?;
            let mut next = before.clone();
            next.workflow = workflow.state();
            next.gates = workflow.gates.clone();
            next.workflow_artifacts = graph;
            Ok((next, workflow))
        })
    }

    /// One host-owned operation for the ordinary agent path. The model may
    /// propose fields, but only this owner verifies routine eligibility,
    /// creates the approved immutable revision and advances the active ref.
    pub fn publish_and_approve_routine_contract(
        &self,
        expected_workflow_revision: u64,
        contract: ChangeContractV1,
    ) -> io::Result<SessionWorkflow> {
        if contract.header.status != ArtifactStatusV1::Draft || !contract.eligibility.is_routine() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "routine条件を満たすdraftだけを自動承認できます",
            ));
        }
        let generation = *self.cursor()?;
        self.update_workflow_artifacts(generation, move |before, store| {
            if before.workflow.revision != expected_workflow_revision {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "workflow revisionが変わりました",
                ));
            }
            let mut graph = before.workflow_artifacts.clone();
            graph
                .insert_change_contract(&contract)
                .map_err(io::Error::other)?;
            store.put_change_contract(&contract)?;
            let approved = contract
                .revised(ArtifactStatusV1::Approved)
                .map_err(io::Error::other)?;
            let approved_reference = approved.reference();
            graph
                .insert_change_contract(&approved)
                .map_err(io::Error::other)?;
            store.put_change_contract(&approved)?;
            let config = before
                .workflow_config
                .as_ref()
                .map(|saved| saved.config.clone())
                .unwrap_or_default();
            let mut gates = before.gates.clone();
            gates.phase_gate_schema_version =
                Some(crate::workflow::WORKFLOW_ARTIFACT_SCHEMA_VERSION);
            let restored = SessionWorkflow::restore_with_artifacts(
                config,
                gates,
                before.workflow.clone(),
                graph.clone(),
            )
            .map_err(io::Error::other)?;
            let workflow = restored
                .with_approved_change_contract(graph.clone(), approved_reference)
                .map_err(io::Error::other)?;
            let mut next = before.clone();
            next.workflow = workflow.state();
            next.gates = workflow.gates.clone();
            next.workflow_artifacts = graph;
            Ok((next, workflow))
        })
    }

    /// Returns only an immutable body named by the session's current graph.
    pub fn load_change_contract(&self, reference: &ArtifactRefV1) -> io::Result<ChangeContractV1> {
        let snapshot = self.snapshot()?;
        let store = WorkflowArtifactStore::open(&self.data_root, &snapshot.state.project_id)?;
        store.load_change_contract(&snapshot.state.workflow_artifacts, reference)
    }

    pub fn load_project_principles(
        &self,
        reference: &ArtifactRefV1,
    ) -> io::Result<crate::workflow::ProjectPrinciplesV1> {
        let snapshot = self.snapshot()?;
        let store = WorkflowArtifactStore::open(&self.data_root, &snapshot.state.project_id)?;
        store.load_project_principles(&snapshot.state.workflow_artifacts, reference)
    }

    pub fn load_idea_decision(
        &self,
        reference: &ArtifactRefV1,
    ) -> io::Result<crate::workflow::IdeaDecisionV1> {
        let snapshot = self.snapshot()?;
        let store = WorkflowArtifactStore::open(&self.data_root, &snapshot.state.project_id)?;
        store.load_idea_decision(&snapshot.state.workflow_artifacts, reference)
    }

    pub fn load_feature_spec(
        &self,
        reference: &ArtifactRefV1,
    ) -> io::Result<crate::workflow::FeatureSpecV1> {
        let snapshot = self.snapshot()?;
        let store = WorkflowArtifactStore::open(&self.data_root, &snapshot.state.project_id)?;
        store.load_feature_spec(&snapshot.state.workflow_artifacts, reference)
    }

    pub fn load_implementation_plan(
        &self,
        reference: &ArtifactRefV1,
    ) -> io::Result<crate::workflow::ImplementationPlanV1> {
        let snapshot = self.snapshot()?;
        let store = WorkflowArtifactStore::open(&self.data_root, &snapshot.state.project_id)?;
        store.load_implementation_plan(&snapshot.state.workflow_artifacts, reference)
    }

    pub fn load_task_graph(
        &self,
        reference: &ArtifactRefV1,
    ) -> io::Result<crate::workflow::TaskGraphV1> {
        let snapshot = self.snapshot()?;
        let store = WorkflowArtifactStore::open(&self.data_root, &snapshot.state.project_id)?;
        store.load_task_graph(&snapshot.state.workflow_artifacts, reference)
    }

    /// Persist convergence only after this session owner has rebuilt its
    /// binding and required criteria from the exact currently active immutable
    /// artifacts. Callers provide only a sealed verification-owner bundle;
    /// they cannot select a gate binding or criterion list.
    pub fn publish_convergence_trusted(
        &self,
        expected_generation: u64,
        expected_workflow_revision: u64,
        bundle: crate::verification::owner::VerifiedConvergenceBundleV1,
    ) -> io::Result<SessionWorkflow> {
        self.update_workflow_artifacts(expected_generation, move |before, store| {
            if before.workflow.revision != expected_workflow_revision {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "workflow revisionが変わりました",
                ));
            }
            let convergence = build_owner_convergence_for_state(before, store, &bundle)?;
            let config = before
                .workflow_config
                .as_ref()
                .map(|saved| saved.config.clone())
                .unwrap_or_else(WorkflowConfig::default);
            let restored = SessionWorkflow::restore_with_artifacts(
                config,
                before.gates.clone(),
                before.workflow.clone(),
                before.workflow_artifacts.clone(),
            )
            .map_err(io::Error::other)?;
            let workflow = restored
                .with_convergence_trusted(convergence)
                .map_err(io::Error::other)?;
            let mut next = before.clone();
            next.workflow = workflow.state();
            next.gates = workflow.gates.clone();
            Ok((next, workflow))
        })
    }

    /// Publish a sealed convergence bundle and its terminal verification
    /// checkpoint through one marker commit.  The exact active artifacts and
    /// observed binding are rechecked while holding the session writer lock;
    /// no old-revision checkpoint can be appended afterwards.
    pub fn publish_verification_convergence(
        &self,
        expected_generation: u64,
        checkpoint: crate::verification::SavedVerificationCheckpointV1,
        bundle: crate::verification::owner::VerifiedConvergenceBundleV1,
    ) -> io::Result<(
        SessionWorkflow,
        crate::verification::SavedVerificationCheckpointV1,
    )> {
        checkpoint.validate().map_err(io::Error::other)?;
        self.update_workflow_artifacts(expected_generation, move |before, store| {
            let Some(previous) = &before.verification_checkpoint else {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "verification checkpointがありません",
                ));
            };
            if previous != &checkpoint
                || checkpoint.stage != crate::verification::VerificationCheckpointStageV1::Terminal
                || checkpoint.workflow_revision != before.workflow.revision
            {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "verification checkpointのCASが一致しません",
                ));
            }
            let convergence = build_owner_convergence_for_state(before, store, &bundle)?;
            let config = before
                .workflow_config
                .as_ref()
                .map(|saved| saved.config.clone())
                .unwrap_or_else(WorkflowConfig::default);
            let restored = SessionWorkflow::restore_with_artifacts(
                config,
                before.gates.clone(),
                before.workflow.clone(),
                before.workflow_artifacts.clone(),
            )
            .map_err(io::Error::other)?;
            let workflow = restored
                .with_convergence_trusted(convergence)
                .map_err(io::Error::other)?;
            let mut published = checkpoint.clone();
            published.workflow_revision = workflow.state().revision;
            published
                .transition(crate::verification::VerificationCheckpointStageV1::Published)
                .map_err(io::Error::other)?;
            let mut next = before.clone();
            next.workflow = workflow.state();
            next.gates = workflow.gates.clone();
            next.verification_checkpoint = Some(published.clone());
            Ok((next, (workflow, published)))
        })
    }

    /// Add one host-authored review to an already published verification and
    /// advance the real Review -> Verify gate in the same session marker CAS.
    /// The caller cannot replace the checkpoint body: this method starts from
    /// the exact published checkpoint currently held by the durable session.
    pub fn publish_review_convergence(
        &self,
        expected_generation: u64,
        expected_workflow_revision: u64,
        checkpoint_id: String,
        review_ref: crate::verification::store::VerificationRecordRefV1,
        bundle: crate::verification::owner::VerifiedConvergenceBundleV1,
    ) -> io::Result<(
        SessionWorkflow,
        crate::verification::SavedVerificationCheckpointV1,
    )> {
        review_ref.validate()?;
        self.update_workflow_artifacts(expected_generation, move |before, store| {
            if before.workflow.revision != expected_workflow_revision {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "review workflow revisionが変わりました",
                ));
            }
            if !matches!(before.workflow.phase, Phase::Implement | Phase::Review) {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "review publicationはImplementまたはReview phaseだけで実行できます",
                ));
            }
            let previous = before.verification_checkpoint.as_ref().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "verification checkpointがありません",
                )
            })?;
            previous.validate().map_err(io::Error::other)?;
            if previous.checkpoint_id != checkpoint_id
                || previous.stage != crate::verification::VerificationCheckpointStageV1::Published
                || !previous.review_refs.is_empty()
            {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "published verification checkpointのCASが一致しません",
                ));
            }

            let convergence = build_owner_convergence_for_state(before, store, &bundle)?;
            let config = before
                .workflow_config
                .as_ref()
                .map(|saved| saved.config.clone())
                .unwrap_or_else(WorkflowConfig::default);
            let restored = SessionWorkflow::restore_with_artifacts(
                config,
                before.gates.clone(),
                before.workflow.clone(),
                before.workflow_artifacts.clone(),
            )
            .map_err(io::Error::other)?;
            let workflow = restored
                .with_convergence_trusted(convergence)
                .map_err(io::Error::other)?;
            if workflow.state().phase == Phase::Implement {
                workflow
                    .request_phase(Phase::Review)
                    .map_err(io::Error::other)?;
            }
            workflow
                .request_phase(Phase::Verify)
                .map_err(io::Error::other)?;

            let mut published = previous.clone();
            published.review_refs.push(review_ref);
            published.workflow_revision = workflow.state().revision;
            published.validate().map_err(io::Error::other)?;
            let mut next = before.clone();
            next.workflow = workflow.state();
            next.gates = workflow.gates.clone();
            next.verification_checkpoint = Some(published.clone());
            Ok((next, (workflow, published)))
        })
    }

    fn update_workflow_artifacts<T>(
        &self,
        expected_generation: u64,
        update: impl FnOnce(
            &ConversationStateV2,
            &WorkflowArtifactStore,
        ) -> io::Result<(ConversationStateV2, T)>,
    ) -> io::Result<T> {
        let mut cursor = self.cursor()?;
        let mut lock = self.store.lock()?;
        let before = lock.snapshot()?;
        if before.state.generation != *cursor || before.state.generation != expected_generation {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "workflow artifactの保存世代が変わりました。再開し直してください",
            ));
        }
        let store = WorkflowArtifactStore::open(&self.data_root, &before.state.project_id)?;
        let (next, output) = update(&before.state, &store)?;
        if next == before.state {
            return Ok(output);
        }
        let mut memory = open_session_memory(&self.database, false)?;
        memory
            .with_conversation_marker(&published_view(&before.state)?, || {
                lock.publish(&before, next)?;
                Ok(())
            })
            .map_err(io::Error::other)?;
        *cursor = before
            .state
            .generation
            .checked_add(1)
            .ok_or_else(|| io::Error::other("保存世代が上限です"))?;
        Ok(output)
    }

    fn update(
        &self,
        events: &[(Message, bool)],
        workflow: Option<&SessionWorkflow>,
        admission: Option<(&str, &str)>,
    ) -> io::Result<Option<crate::request_input::SourceBinding>> {
        #[cfg(all(test, windows))]
        if workflow.is_some() {
            windows_refuse_owner()?;
        }
        let mut cursor = self.cursor()?;
        let mut lock = self.store.lock()?;
        let before = lock.snapshot()?;
        if before.state.generation != *cursor {
            return Err(io::Error::other(
                "保存世代が変わりました。原文を保持して停止しました",
            ));
        }
        if let Some((admission_id, payload_hash)) = admission {
            crate::session_manager::AdmissionId::new(admission_id.to_owned())
                .map_err(io::Error::other)?;
            if payload_hash.len() != 64
                || !payload_hash.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(io::Error::other("admission payload hashが不正です"));
            }
            if !matches!(events.len(), 1 | 2)
                || !events[0].1
                || events[0].0.role != Role::User
                || events.get(1).is_some_and(|(message, starts_turn)| {
                    *starts_turn
                        || message.role != Role::User
                        || message.tool_call_id.is_some()
                        || !message.tool_calls.is_empty()
                        || !message.reasoning.is_empty()
                        || !message.hosted_web_search.is_empty()
                        || !message.url_citations.is_empty()
                })
            {
                return Err(io::Error::other(
                    "admission inputは単一のuser turnが必要です",
                ));
            }
            if let Some(receipt) = before.state.admission_receipts.get(admission_id) {
                if receipt.payload_hash != payload_hash {
                    return Err(io::Error::other(
                        "admission receiptが既存入力と衝突しました",
                    ));
                }
                let index = before
                    .events
                    .iter()
                    .position(|event| event.sequence == receipt.sequence)
                    .ok_or_else(|| io::Error::other("admission receiptの原文がありません"))?;
                let event = &before.events[index];
                if !event.starts_turn
                    || event.turn_id != receipt.turn_id
                    || serde_json::to_vec(&event.message).map_err(io::Error::other)?
                        != serde_json::to_vec(&events[0].0).map_err(io::Error::other)?
                {
                    return Err(io::Error::other(
                        "admission inputの原文が既存入力と不一致です",
                    ));
                }
                if let Some((supplement, _)) = events.get(1) {
                    let saved = before.events.get(index + 1).ok_or_else(|| {
                        io::Error::other("admission prompt結果の原文がありません")
                    })?;
                    if saved.starts_turn
                        || saved.turn_id != event.turn_id
                        || serde_json::to_vec(&saved.message).map_err(io::Error::other)?
                            != serde_json::to_vec(supplement).map_err(io::Error::other)?
                    {
                        return Err(io::Error::other(
                            "admission prompt結果が既存入力と不一致です",
                        ));
                    }
                }
                return Ok(crate::request_input::SourceBinding::from_event(
                    &before.state.session_id,
                    event,
                ));
            }
            if before.state.admission_receipts.len()
                >= crate::session_manager::MAX_ADMISSION_RECEIPTS
            {
                return Err(io::Error::other(
                    "admission receipt容量不足です。原文は保存していません",
                ));
            }
        }
        if let Some(candidate) = workflow {
            if before.state.workflow_artifacts != *candidate.artifact_graph()
                || before.state.gates.phase_gate_schema_version
                    != candidate.gates.phase_gate_schema_version
                || before.state.gates.convergence != candidate.gates.convergence
                || before.state.workflow.active_change_contract
                    != candidate.state().active_change_contract
            {
                return Err(io::Error::other(
                    "workflow artifactまたはphase gateを通常checkpointでは変更できません",
                ));
            }
            if before.state.workflow.phase != candidate.state().phase {
                let artifact_store =
                    WorkflowArtifactStore::open(&self.data_root, &before.state.project_id)?;
                if let Some(reference) = &before.state.workflow.active_change_contract {
                    artifact_store
                        .load_change_contract(&before.state.workflow_artifacts, reference)?;
                }
                if let Some(reference) = &before.state.workflow.active_feature {
                    artifact_store
                        .load_feature_spec(&before.state.workflow_artifacts, reference)?;
                }
                if let Some(reference) = &before.state.workflow.active_task {
                    let task_graph = artifact_store
                        .load_task_graph(&before.state.workflow_artifacts, &reference.task_graph)?;
                    reference
                        .validate_against(&task_graph)
                        .map_err(io::Error::other)?;
                }
                if let Some(convergence) = &before.state.gates.convergence {
                    validate_owner_convergence_for_state(
                        &before.state,
                        &artifact_store,
                        convergence,
                    )?;
                }
                crate::workflow::evaluate_phase_transition(
                    &before.state.workflow,
                    candidate.state().phase,
                    &before.state.gates,
                    &before.state.workflow_artifacts,
                )
                .map_err(io::Error::other)?;
            }
        }
        if admission.is_none()
            && events.is_empty()
            && before.state.raw_commit_hash == before.raw_hash
            && before.state.raw_commit_offset == before.raw_offset
            && workflow.is_none_or(|workflow| {
                before.state.workflow == workflow.state()
                    && before.state.gates == workflow.gates
                    && before.state.workflow_artifacts == *workflow.artifact_graph()
                    && before
                        .state
                        .workflow_config
                        .as_ref()
                        .is_some_and(|config| config.config == workflow.config)
            })
        {
            let mut memory = open_session_memory(&self.database, false)?;
            return memory
                .with_conversation_marker(&published_view(&before.state)?, || Ok(()))
                .map(|_| None)
                .map_err(io::Error::other);
        }
        let mut memory = open_session_memory(&self.database, false)?;
        // The old writer's forget is serialized with both raw append and marker.
        let mut source = None;
        memory
            .with_conversation_marker(&published_view(&before.state)?, || {
                crate::conversation_memory::reject_unpublished_structured_attempt(
                    &self.store,
                    &before,
                )?;
                let mut snapshot = before.clone();
                for (message, starts_turn) in events {
                    snapshot = lock.append(message.clone(), *starts_turn)?;
                }
                let mut next = snapshot.state.clone();
                if let Some(workflow) = workflow {
                    next.workflow_config = Some(
                        crate::conversation_state::SavedWorkflowConfigV1::capture(&workflow.config),
                    );
                    next.workflow = workflow.state();
                    next.gates = workflow.gates.clone();
                    next.workflow_artifacts = workflow.artifact_graph().clone();
                }
                if let Some((admission_id, payload_hash)) = admission {
                    let event = snapshot
                        .events
                        .get(before.events.len())
                        .ok_or_else(|| io::Error::other("admission inputを保存できません"))?;
                    next.admission_receipts.insert(
                        admission_id.to_owned(),
                        AdmissionReceiptV1 {
                            payload_hash: payload_hash.to_owned(),
                            sequence: event.sequence,
                            turn_id: event.turn_id,
                        },
                    );
                }
                lock.publish(&snapshot, next)?;
                source = events
                    .iter()
                    .position(|(_, starts_turn)| *starts_turn)
                    .and_then(|index| snapshot.events.get(before.events.len() + index))
                    .and_then(|event| {
                        crate::request_input::SourceBinding::from_event(
                            &snapshot.state.session_id,
                            event,
                        )
                    });
                Ok(())
            })
            .map_err(io::Error::other)?;
        *cursor = before.state.generation + 1;
        Ok(source)
    }

    pub fn clear(&self) -> io::Result<()> {
        let mut cursor = self.cursor()?;
        let mut lock = self.store.lock()?;
        let before = lock.snapshot()?;
        if before.state.generation != *cursor {
            return Err(io::Error::other("保存世代が変わりました"));
        }
        let mut memory = open_session_memory(&self.database, false)?;
        memory
            .with_conversation_marker(&published_view(&before.state)?, || {
                lock.clear_epoch()?;
                Ok(())
            })
            .map_err(io::Error::other)?;
        *cursor = before.state.generation + 1;
        Ok(())
    }

    pub fn fork(&self, artifact_hash: &str) -> io::Result<Self> {
        self.fork_with_id(&new_session_id()?, artifact_hash)
    }

    /// The management owner fixes the child identity before recording its
    /// effect, so a restart can inspect a single target after an interruption.
    pub fn fork_with_id(&self, child_id: &str, artifact_hash: &str) -> io::Result<Self> {
        let cursor = self.cursor()?;
        let lock = self.store.lock()?;
        let snapshot = lock.snapshot()?;
        if snapshot.state.generation != *cursor {
            return Err(io::Error::other("保存世代が変わりました"));
        }
        let mut memory = open_session_memory(&self.database, false)?;
        let mut child = None;
        memory
            .with_conversation_marker(&published_view(&snapshot.state)?, || {
                child = Some(lock.fork(&self.data_root, child_id, artifact_hash)?);
                Ok(())
            })
            .map_err(io::Error::other)?;
        let store = child.ok_or_else(|| io::Error::other("分岐を保存できません"))?;
        let generation = store.lock()?.snapshot()?.state.generation;
        Ok(Self {
            store: Arc::new(store),
            database: self.database.clone(),
            data_root: self.data_root.clone(),
            generation: Arc::new(Mutex::new(generation)),
        })
    }

    /// Record the durable pre-user rewind boundary. The caller supplies only
    /// opaque workspace and tool-memory identities; their owners retain all
    /// filesystem and object-store access.
    pub fn begin_rewind_boundary(
        &self,
        workspace_pre_manifest_id: String,
        visible_memory_raw_object_ids: Vec<String>,
    ) -> io::Result<RewindBoundaryV1> {
        let cursor = self.cursor()?;
        let mut lock = self.store.lock()?;
        let snapshot = lock.snapshot()?;
        if snapshot.state.generation != *cursor {
            return Err(io::Error::other("保存世代が変わりました"));
        }
        lock.begin_rewind_boundary(workspace_pre_manifest_id, visible_memory_raw_object_ids)
    }

    /// Mark the selected input's boundary complete after its turn and owned
    /// writers have stopped. Ineligible boundaries remain durable diagnostics
    /// but are intentionally absent from `list_rewind_boundaries`.
    pub fn finish_rewind_boundary(
        &self,
        boundary_id: &str,
        workspace_post_manifest_id: String,
        eligibility: RewindEligibilityV1,
        effect: RewindEffectStatusV1,
    ) -> io::Result<RewindBoundaryV1> {
        let cursor = self.cursor()?;
        let mut lock = self.store.lock()?;
        let snapshot = lock.snapshot()?;
        if snapshot.state.generation != *cursor {
            return Err(io::Error::other("保存世代が変わりました"));
        }
        lock.finish_rewind_boundary(boundary_id, workspace_post_manifest_id, eligibility, effect)
    }

    /// Permanently marks an interrupted or failed turn unavailable without a
    /// post-manifest, so the next input is not blocked by a stale boundary.
    pub fn abandon_rewind_boundary(
        &self,
        boundary_id: &str,
        reason: String,
    ) -> io::Result<RewindBoundaryV1> {
        let cursor = self.cursor()?;
        let mut lock = self.store.lock()?;
        let snapshot = lock.snapshot()?;
        if snapshot.state.generation != *cursor {
            return Err(io::Error::other("保存世代が変わりました"));
        }
        lock.abandon_rewind_boundary(boundary_id, reason)
    }

    /// Recovery path for startup/resume after an interrupted process. It does
    /// not read or alter the raw marker, so a malformed crash tail cannot keep
    /// the next user admission blocked behind an unfinished boundary.
    pub fn abandon_unfinished_rewind_boundaries(
        &self,
        reason: String,
    ) -> io::Result<Vec<RewindBoundaryV1>> {
        let mut lock = self.store.lock()?;
        lock.abandon_unfinished_rewind_boundaries(reason)
    }

    pub fn list_rewind_boundaries(&self) -> io::Result<Vec<RewindBoundaryV1>> {
        let cursor = self.cursor()?;
        let lock = self.store.lock()?;
        let snapshot = lock.snapshot()?;
        if snapshot.state.generation != *cursor {
            return Err(io::Error::other("保存世代が変わりました"));
        }
        lock.list_rewind_boundaries()
    }

    /// Prepare a distinct, non-active full-prefix branch. Workspace restore,
    /// project-derived context reload, and activation remain caller-owned.
    #[cfg(unix)]
    pub fn prepare_rewind_branch(
        &self,
        workspace: &crate::rewind_workspace::WorkspaceOwner,
        boundary_id: &str,
    ) -> io::Result<Self> {
        // The project lease precedes the nonblocking source writer lock.
        let lease = workspace.try_mutation().map_err(io::Error::other)?;
        let cursor = self.cursor()?;
        let lock = self.store.lock()?;
        let snapshot = lock.snapshot()?;
        if snapshot.state.generation != *cursor {
            return Err(io::Error::other("保存世代が変わりました"));
        }
        let plan = lock.plan_rewind_branch(boundary_id, &new_session_id()?)?;
        let (_, pre) = plan.workspace_manifests();
        let boundaries = lock.list_rewind_boundaries()?;
        let post = boundaries
            .last()
            .and_then(|boundary| boundary.completion.as_ref())
            .ok_or_else(|| io::Error::other("現在の巻き戻し境界が未完了です"))?
            .workspace_post_manifest_id
            .parse()
            .map_err(io::Error::other)?;
        let pre = pre.parse().map_err(io::Error::other)?;
        let preview = workspace
            .preview_with_management_with_lease(&lease, &post, &pre)
            .map_err(io::Error::other)?;
        if !preview.restorable {
            return Err(io::Error::other(
                "管理操作または保存状態が巻き戻しを許可しません",
            ));
        }
        crate::rewind_candidate_capacity::preflight_rewind_candidate_capacity(
            workspace,
            &lease,
            &self.data_root,
            &snapshot.state.project_id,
            plan.estimated_file_bytes(),
        )?;
        let mut memory = open_session_memory(&self.database, false)?;
        let mut child = None;
        memory
            .with_conversation_marker(&published_view(&snapshot.state)?, || {
                child = Some(lock.create_rewind_branch_from_plan(&self.data_root, plan)?);
                Ok(())
            })
            .map_err(io::Error::other)?;
        let store = child.ok_or_else(|| io::Error::other("rewind分岐を保存できません"))?;
        let generation = store.lock()?.snapshot()?.state.generation;
        Ok(Self {
            store: Arc::new(store),
            database: self.database.clone(),
            data_root: self.data_root.clone(),
            generation: Arc::new(Mutex::new(generation)),
        })
    }

    /// Import into a fresh UUID; the caller's legacy JSONL remains untouched.
    pub fn import(
        data_root: &Path,
        database: &Path,
        project_id: &str,
        mode: HistoryMode,
        workflow: Option<&SessionWorkflow>,
        messages: &[Message],
    ) -> io::Result<Self> {
        #[cfg(all(test, windows))]
        windows_refuse_owner()?;
        let this = Self::create(data_root, database, project_id, mode, workflow)?;
        let events: Vec<_> = messages
            .iter()
            .cloned()
            .map(|message| {
                let starts_turn = message.role == Role::User;
                (message, starts_turn)
            })
            .collect();
        this.update(&events, workflow, None)?;
        Ok(this)
    }

    pub fn restore_workflow(&self, config: WorkflowConfig) -> io::Result<SessionWorkflow> {
        let (workflow, needs_checkpoint) = self.prepare_workflow(config)?;
        if needs_checkpoint {
            // Evidence is bound to observed source/diff hashes. A restart has
            // no current observation, so remove it through the same durable
            // owner before exposing the restored workflow.
            if self.snapshot()?.state.gates.convergence.is_some() {
                self.clear_convergence_on_resume(&workflow)?;
            } else {
                self.checkpoint(Some(&workflow))?;
            }
        }
        Ok(workflow)
    }

    fn clear_convergence_on_resume(&self, workflow: &SessionWorkflow) -> io::Result<()> {
        let expected_generation = self.snapshot()?.state.generation;
        let workflow = workflow.clone();
        self.update_workflow_artifacts(expected_generation, move |before, _| {
            if before.gates.convergence.is_none() {
                return Ok((before.clone(), ()));
            }
            let mut next = before.clone();
            next.workflow_config = Some(crate::conversation_state::SavedWorkflowConfigV1::capture(
                &workflow.config,
            ));
            next.workflow = workflow.state();
            next.gates = workflow.gates.clone();
            next.workflow_artifacts = workflow.artifact_graph().clone();
            Ok((next, ()))
        })
    }

    /// Restore without writing so callers can validate explicit overrides first.
    /// The flag requests a checkpoint for a missing saved configuration.
    pub fn prepare_workflow(
        &self,
        mut config: WorkflowConfig,
    ) -> io::Result<(SessionWorkflow, bool)> {
        #[cfg(all(test, windows))]
        windows_refuse_owner()?;
        let state = self.snapshot()?.state;
        if let Some(reference) = &state.workflow.active_change_contract {
            let store = WorkflowArtifactStore::open(&self.data_root, &state.project_id)?;
            store.load_change_contract(&state.workflow_artifacts, reference)?;
        }
        let mut gates = state.gates;
        // Resume has not verified that the working files still match evidence.
        // A new explicit verification can replace this conservative stale flag.
        if let Some(verification) = &mut gates.verification {
            verification.stale = true;
        }
        let invalidated_convergence = gates.convergence.take().is_some();
        let needs_checkpoint = state.workflow_config.is_none() || invalidated_convergence;
        let config = match state.workflow_config {
            Some(saved) => saved.config,
            None => {
                config.mode = WorkflowMode::Manual;
                config
            }
        };
        let workflow = SessionWorkflow::restore_with_artifacts(
            config,
            gates,
            state.workflow,
            state.workflow_artifacts,
        )
        .map_err(io::Error::other)?;
        Ok((workflow, needs_checkpoint))
    }
}

fn build_owner_convergence_for_state(
    state: &ConversationStateV2,
    store: &WorkflowArtifactStore,
    bundle: &crate::verification::owner::VerifiedConvergenceBundleV1,
) -> io::Result<ConvergenceInputV1> {
    let observed = ObservedEvidenceSubjectV1 {
        subject_hash: bundle.binding().subject_hash.clone(),
        source_hash: bundle.binding().source_hash.clone(),
        diff_hash: bundle.binding().diff_hash.clone(),
    };
    let graph = &state.workflow_artifacts;
    if let Some(reference) = &state.workflow.active_change_contract {
        let contract = store.load_change_contract(graph, reference)?;
        return build_owner_convergence(
            ActiveEvidenceArtifactsV1::RoutineContract {
                artifact_graph: graph,
                active_contract: reference,
                contract: &contract,
            },
            &observed,
            bundle.implementation_receipts().to_vec(),
            bundle.reviews().to_vec(),
            bundle.evidence().to_vec(),
        )
        .and_then(|convergence| verify_bundle_binding(bundle, convergence))
        .map_err(io::Error::other);
    }
    let (feature_reference, active_task) = match (
        state.workflow.active_feature.as_ref(),
        state.workflow.active_task.as_ref(),
    ) {
        (Some(feature), Some(task)) => (feature, task),
        _ => {
            return Err(io::Error::other(
                "convergenceにはcurrent active contractまたはtaskが必要です",
            ));
        }
    };
    let feature = store.load_feature_spec(graph, feature_reference)?;
    let task_graph = store.load_task_graph(graph, &active_task.task_graph)?;
    build_owner_convergence(
        ActiveEvidenceArtifactsV1::PlannedTask {
            artifact_graph: graph,
            active_feature: feature_reference,
            active_task,
            feature: &feature,
            task_graph: &task_graph,
        },
        &observed,
        bundle.implementation_receipts().to_vec(),
        bundle.reviews().to_vec(),
        bundle.evidence().to_vec(),
    )
    .and_then(|convergence| verify_bundle_binding(bundle, convergence))
    .map_err(io::Error::other)
}

fn verify_bundle_binding(
    bundle: &crate::verification::owner::VerifiedConvergenceBundleV1,
    convergence: ConvergenceInputV1,
) -> Result<ConvergenceInputV1, String> {
    if convergence.binding != *bundle.binding() {
        return Err(
            "verification bundle binding is not current for active workflow artifacts".into(),
        );
    }
    Ok(convergence)
}

fn workflow_only_change(before: &ConversationStateV2, candidate: &ConversationStateV2) -> bool {
    let mut expected = before.clone();
    expected.generation = candidate.generation;
    expected.workflow_config = candidate.workflow_config.clone();
    expected.workflow = candidate.workflow.clone();
    expected.gates = candidate.gates.clone();
    expected.workflow_artifacts = candidate.workflow_artifacts.clone();
    expected == *candidate
}

fn validate_owner_convergence_for_state(
    state: &ConversationStateV2,
    store: &WorkflowArtifactStore,
    convergence: &ConvergenceInputV1,
) -> io::Result<()> {
    let observed = ObservedEvidenceSubjectV1 {
        subject_hash: convergence.binding.subject_hash.clone(),
        source_hash: convergence.binding.source_hash.clone(),
        diff_hash: convergence.binding.diff_hash.clone(),
    };
    let graph = &state.workflow_artifacts;
    if let Some(reference) = &state.workflow.active_change_contract {
        let contract = store.load_change_contract(graph, reference)?;
        validate_owner_convergence(
            ActiveEvidenceArtifactsV1::RoutineContract {
                artifact_graph: graph,
                active_contract: reference,
                contract: &contract,
            },
            &observed,
            convergence.clone(),
        )
        .map_err(io::Error::other)?;
        return Ok(());
    }
    let (feature_reference, active_task) = match (
        state.workflow.active_feature.as_ref(),
        state.workflow.active_task.as_ref(),
    ) {
        (Some(feature), Some(task)) => (feature, task),
        _ => return Err(io::Error::other("convergenceにactive artifactがありません")),
    };
    let feature = store.load_feature_spec(graph, feature_reference)?;
    let task_graph = store.load_task_graph(graph, &active_task.task_graph)?;
    validate_owner_convergence(
        ActiveEvidenceArtifactsV1::PlannedTask {
            artifact_graph: graph,
            active_feature: feature_reference,
            active_task,
            feature: &feature,
            task_graph: &task_graph,
        },
        &observed,
        convergence.clone(),
    )
    .map_err(io::Error::other)?;
    Ok(())
}

fn open_session_memory(database: &Path, _create: bool) -> io::Result<MemoryStore> {
    #[cfg(all(test, windows))]
    let memory = MemoryStore::open_windows_protected(database, _create);
    #[cfg(not(all(test, windows)))]
    let memory = MemoryStore::open(database);
    memory.map_err(io::Error::other)
}

#[cfg(all(test, windows))]
fn windows_refuse_owner() -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "このWindows試験ではprivate artifact/workflow保存を利用できません",
    ))
}

#[cfg(all(test, windows))]
fn windows_creation_guard(
    data_root: &Path,
    state: &ConversationStateV2,
) -> io::Result<(
    polaris_auth::protection::WindowsFamilyDirectory,
    Option<polaris_auth::protection::WindowsFamilyDirectory>,
)> {
    use polaris_auth::{AuthError, protection::WindowsFamilyDirectory};
    use std::ffi::OsStr;
    let unavailable = || {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "このplatformではセッション保存の保護を確認できません",
        )
    };
    state.validate()?;
    crate::conversation_state::windows_state_admission(state)?;
    let root = WindowsFamilyDirectory::open_existing(data_root).map_err(|_| unavailable())?;
    // Keep the qualified ancestors alive across schema creation and publication.
    let sessions = match root.child(OsStr::new("sessions-v2"), false) {
        Ok(sessions) => {
            match sessions.child(OsStr::new(&state.session_id), false) {
                Err(AuthError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {}
                _ => return Err(unavailable()),
            }
            Some(sessions)
        }
        Err(AuthError::Io(error)) if error.kind() == io::ErrorKind::NotFound => None,
        Err(_) => return Err(unavailable()),
    };
    Ok((root, sessions))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::session::Session;
    use crate::workflow::RoutineEligibilityV1;

    fn workflow(config: WorkflowConfig) -> SessionWorkflow {
        SessionWorkflow::new(config)
    }

    fn create(root: &Path) -> PersistedSession {
        PersistedSession::create(
            root,
            &root.join("memory.sqlite3"),
            "project",
            HistoryMode::Legacy,
            None,
        )
        .unwrap()
    }

    fn change_contract(id: &str) -> ChangeContractV1 {
        ChangeContractV1::draft(
            id,
            RoutineEligibilityV1::local_component("polaris-core"),
            "a routine workflow change",
            "the previous behavior did not retain a contract",
            "the approved immutable contract is active",
            vec!["focused persistence test passes".into()],
        )
        .unwrap()
    }

    #[test]
    fn m27_review_publication_is_one_cas_and_crosses_the_real_review_gate() {
        use crate::verification::{
            CriterionRunV1, EvidenceKindV1, RunnerProvenanceV1, SavedVerificationCheckpointV1,
            VerificationCheckpointStageV1,
            owner::VerificationOwnerV1,
            store::{VerificationBindingV1, VerificationStore},
        };
        use crate::workflow::{
            EVIDENCE_SCHEMA_VERSION, EvidenceBindingV1, EvidenceProvenanceV1, EvidenceRecordV1,
            EvidenceResultV1, EvidenceSourceKindV1, ReviewRecordV1, VerificationCriterionV1,
        };

        let dir = tempfile::tempdir().unwrap();
        let initial = SessionWorkflow::new(WorkflowConfig {
            enabled: true,
            initial_phase: Phase::Specify,
            ..WorkflowConfig::default()
        });
        let saved = PersistedSession::create(
            dir.path(),
            &dir.path().join("memory.sqlite3"),
            "review-cas",
            HistoryMode::Legacy,
            Some(&initial),
        )
        .unwrap();
        let criterion =
            VerificationCriterionV1::stable("The focused behavior passes.", true).unwrap();
        let criterion_id = criterion.id.clone();
        let contract = ChangeContractV1::draft_typed(
            "change-review-cas",
            RoutineEligibilityV1::local_component("polaris-core"),
            "fix the focused behavior",
            "the focused behavior fails",
            "the focused behavior passes",
            vec![criterion],
        )
        .unwrap();
        let implemented = saved
            .publish_and_approve_routine_contract(initial.state().revision, contract)
            .unwrap();
        implemented.request_phase(Phase::Implement).unwrap();
        saved.checkpoint(Some(&implemented)).unwrap();
        let snapshot = saved.snapshot().unwrap();
        let active = snapshot
            .state
            .workflow
            .active_change_contract
            .clone()
            .unwrap();
        let hash = |byte: char| std::iter::repeat_n(byte, 64).collect::<String>();
        let binding = EvidenceBindingV1 {
            task_id: active.id.clone(),
            task_definition_hash: active.content_hash.clone(),
            subject_hash: hash('a'),
            source_hash: hash('b'),
            diff_hash: hash('c'),
            artifact_graph_basis_hash: snapshot.state.workflow_artifacts.basis_hash().unwrap(),
        };
        let verification_binding = VerificationBindingV1 {
            project_graph_hash: hash('1'),
            analyzer_hash: hash('2'),
            tool_hash: hash('3'),
            source_hash: binding.source_hash.clone(),
            diff_hash: binding.diff_hash.clone(),
            config_hash: hash('4'),
            analysis_request_hash: hash('5'),
        };
        let evidence = EvidenceRecordV1 {
            schema_version: EVIDENCE_SCHEMA_VERSION,
            record_id: "evidence-review-cas".into(),
            binding: binding.clone(),
            criterion_id: criterion_id.clone(),
            result: EvidenceResultV1::Passed,
            provenance: EvidenceProvenanceV1 {
                source_kind: EvidenceSourceKindV1::TestExecution,
                source_ref: "test:review-cas".into(),
                observed_at_unix_secs: 1,
            },
        };
        let store =
            VerificationStore::open(&saved.verification_store_root(), "review-cas").unwrap();
        let evidence_ref = store
            .put_final_evidence_record(&verification_binding, &evidence)
            .unwrap();
        let catalog = crate::verification::StaticCapabilityCatalogV1::default();
        let owner = VerificationOwnerV1::new(&store, &catalog);
        let terminal_bundle = owner
            .bundle_from_final_evidence_records(&binding, std::slice::from_ref(&evidence), 2)
            .unwrap();
        let mut checkpoint = SavedVerificationCheckpointV1::new(
            "checkpoint-review-cas".into(),
            "idempotency-review-cas".into(),
            snapshot.state.workflow.revision,
            vec![active],
            binding.clone(),
        )
        .unwrap();
        checkpoint.criterion_runs = vec![CriterionRunV1 {
            criterion_id,
            evidence_kind: EvidenceKindV1::ExistingTest,
            runner_id: "runner-review-cas".into(),
            provenance: Some(RunnerProvenanceV1 {
                plan_hash: hash('6'),
                program_hash: hash('7'),
                report_hash: hash('8'),
                cleanup_complete: true,
                execution_available: true,
            }),
        }];
        checkpoint.evidence_refs = vec![evidence_ref];
        saved
            .publish_verification_checkpoint(snapshot.state.generation, checkpoint.clone())
            .unwrap();
        checkpoint
            .transition(VerificationCheckpointStageV1::Running)
            .unwrap();
        let generation = saved.snapshot().unwrap().state.generation;
        saved
            .publish_verification_checkpoint(generation, checkpoint.clone())
            .unwrap();
        checkpoint
            .transition(VerificationCheckpointStageV1::Terminal)
            .unwrap();
        let generation = saved.snapshot().unwrap().state.generation;
        saved
            .publish_verification_checkpoint(generation, checkpoint.clone())
            .unwrap();
        let generation = saved.snapshot().unwrap().state.generation;
        let (workflow, published) = saved
            .publish_verification_convergence(generation, checkpoint, terminal_bundle)
            .unwrap();

        let review = ReviewRecordV1 {
            schema_version: EVIDENCE_SCHEMA_VERSION,
            record_id: "review-cas".into(),
            binding: binding.clone(),
            findings: Vec::new(),
        };
        let review_ref = store
            .put_review_record(&verification_binding, &binding, &review)
            .unwrap();
        let bundle = || {
            let bundle = owner
                .bundle_from_final_evidence_records(&binding, std::slice::from_ref(&evidence), 3)
                .unwrap();
            owner
                .add_review_reference(bundle, &verification_binding, &review_ref)
                .unwrap()
        };
        let generation = saved.snapshot().unwrap().state.generation;
        assert!(
            saved
                .publish_review_convergence(
                    generation,
                    workflow.state().revision + 1,
                    published.checkpoint_id.clone(),
                    review_ref.clone(),
                    bundle(),
                )
                .is_err()
        );
        assert!(
            saved
                .publish_review_convergence(
                    generation,
                    workflow.state().revision,
                    "tampered-checkpoint".into(),
                    review_ref.clone(),
                    bundle(),
                )
                .is_err()
        );
        let mut stale_binding = binding.clone();
        stale_binding.task_definition_hash = hash('9');
        let mut stale_evidence = evidence.clone();
        stale_evidence.binding = stale_binding.clone();
        let stale_bundle = owner
            .bundle_from_final_evidence_records(&stale_binding, &[stale_evidence], 3)
            .unwrap();
        assert!(
            saved
                .publish_review_convergence(
                    generation,
                    workflow.state().revision,
                    published.checkpoint_id.clone(),
                    review_ref.clone(),
                    stale_bundle,
                )
                .is_err()
        );
        let (verified, reviewed) = saved
            .publish_review_convergence(
                generation,
                workflow.state().revision,
                published.checkpoint_id.clone(),
                review_ref.clone(),
                bundle(),
            )
            .unwrap();
        assert_eq!(verified.state().phase, Phase::Verify);
        assert_eq!(reviewed.review_refs, std::slice::from_ref(&review_ref));
        let snapshot = saved.snapshot().unwrap();
        assert_eq!(snapshot.state.workflow.phase, Phase::Verify);
        assert_eq!(snapshot.state.gates.convergence.unwrap().reviews, [review]);
        assert!(
            saved
                .publish_review_convergence(
                    snapshot.state.generation,
                    verified.state().revision,
                    published.checkpoint_id,
                    review_ref.clone(),
                    bundle(),
                )
                .is_err()
        );
    }

    #[test]
    fn resumed_partial_tool_batch_is_closed_without_reexecution() {
        let dir = tempfile::tempdir().unwrap();
        let saved = create(dir.path());
        let mut session = crate::session::Session::new();
        session.attach(saved.clone()).unwrap();
        session.push_user("work");
        session.push_assistant_tool_calls(
            "",
            vec![
                polaris_provider::ToolCall {
                    id: "a".into(),
                    name: "write".into(),
                    arguments: serde_json::json!({}),
                },
                polaris_provider::ToolCall {
                    id: "b".into(),
                    name: "write".into(),
                    arguments: serde_json::json!({}),
                },
            ],
            Vec::new(),
        );
        session.push_tool_result("a", "completed");
        session.messages.truncate(1);
        session.recover_interrupted_tools().unwrap();
        assert_eq!(session.messages.len(), 4);
        assert_eq!(session.messages[1].tool_calls.len(), 2);
        let mut resumed = crate::session::Session::new();
        resumed.attach(saved.clone()).unwrap();
        resumed.push_user("continue");
        resumed.check_persistence().unwrap();
        let raw = saved.snapshot().unwrap();
        assert_eq!(raw.events.len(), 5);
        assert_eq!(raw.events[3].message.tool_call_id.as_deref(), Some("b"));
        assert!(raw.events[3].message.content.contains("結果は不明"));
        assert_eq!(raw.events[4].message.content, "continue");
        assert_eq!(raw.events[2].message.content, "completed");
    }

    #[test]
    fn separately_resumed_writer_cannot_append_after_another_writer() {
        let dir = tempfile::tempdir().unwrap();
        let first = create(dir.path());
        let id = first.snapshot().unwrap().state.session_id;
        let second = PersistedSession::open(dir.path(), &first.database, "project", &id).unwrap();
        first.append(Message::user("first"), true, None).unwrap();
        assert!(second.append(Message::user("stale"), true, None).is_err());
        assert_eq!(first.snapshot().unwrap().events.len(), 1);
    }

    #[test]
    fn every_history_mode_can_be_created_and_resumed_explicitly() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("memory.sqlite3");
        for mode in [
            HistoryMode::Legacy,
            HistoryMode::Strict10,
            HistoryMode::Structured10,
        ] {
            let saved =
                PersistedSession::create(dir.path(), &database, "project", mode, None).unwrap();
            let id = saved.snapshot().unwrap().state.session_id;
            let reopened = PersistedSession::open(dir.path(), &database, "project", &id).unwrap();
            assert_eq!(reopened.snapshot().unwrap().state.history_mode, mode);
        }
        let broken = tempfile::tempdir().unwrap();
        let database = broken.path().join("memory.sqlite3");
        MemoryStore::open(&database).unwrap();
        rusqlite::Connection::open(&database)
            .unwrap()
            .execute_batch(
                "CREATE TABLE memory_schema_meta (component TEXT PRIMARY KEY NOT NULL, schema_version INTEGER NOT NULL);
                 INSERT INTO memory_schema_meta VALUES ('structured_memory', 99);",
            )
            .unwrap();
        assert!(
            PersistedSession::create(
                broken.path(),
                &database,
                "project",
                HistoryMode::Structured10,
                None
            )
            .is_err()
        );
        assert!(!broken.path().join("sessions-v2").exists(), "nothing saved");
    }

    #[test]
    fn forgotten_session_rejects_raw_append_and_resume() {
        let dir = tempfile::tempdir().unwrap();
        let saved = create(dir.path());
        let id = saved.snapshot().unwrap().state.session_id;
        let raw = saved.store.directory().join("raw.jsonl");
        let mut session = Session::new();
        session.attach(saved.clone()).unwrap();
        session.push_user("retained");
        let original = std::fs::read(&raw).unwrap();
        MemoryStore::open(&saved.database)
            .unwrap()
            .delete_session("project", &id)
            .unwrap();
        session.push_assistant("must not persist", Vec::new());
        assert!(session.check_persistence().is_err());
        assert_eq!(session.messages.len(), 1);
        assert_eq!(std::fs::read(&raw).unwrap(), original);
        assert!(PersistedSession::open(dir.path(), &saved.database, "project", &id).is_err());
    }

    #[test]
    fn unchanged_checkpoint_does_not_invalidate_pending_generation() {
        let dir = tempfile::tempdir().unwrap();
        let saved = create(dir.path());
        saved.append(Message::user("turn"), true, None).unwrap();
        let expected = saved.snapshot().unwrap();
        saved.checkpoint(None).unwrap();
        assert_eq!(saved.snapshot().unwrap().state, expected.state);
    }

    #[test]
    fn saved_workflow_config_is_authoritative_for_resume_and_fork() {
        let dir = tempfile::tempdir().unwrap();
        let saved_config = WorkflowConfig {
            enabled: false,
            mode: WorkflowMode::Manual,
            ..Default::default()
        };
        let saved_workflow = workflow(saved_config.clone());
        let saved = PersistedSession::create(
            dir.path(),
            &dir.path().join("memory.sqlite3"),
            "project",
            HistoryMode::Legacy,
            Some(&saved_workflow),
        )
        .unwrap();
        let mut changed_config = saved_config.clone();
        changed_config.enabled = true;
        changed_config.mode = WorkflowMode::Adaptive;
        changed_config.initial_phase = crate::workflow::Phase::Review;

        let resumed = saved.restore_workflow(changed_config.clone()).unwrap();
        assert_eq!(resumed.config, saved_config);
        assert!(!resumed.config.enabled);

        let child = saved.fork(&"a".repeat(64)).unwrap();
        let forked = child.restore_workflow(changed_config).unwrap();
        assert_eq!(forked.config, saved_config);
    }

    #[test]
    fn legacy_workflow_config_defaults_to_manual_and_is_checkpointed_once() {
        let dir = tempfile::tempdir().unwrap();
        let saved = create(dir.path());
        let current = WorkflowConfig {
            enabled: false,
            mode: WorkflowMode::Adaptive,
            initial_phase: crate::workflow::Phase::Implement,
            ..Default::default()
        };

        let restored = saved.restore_workflow(current.clone()).unwrap();
        assert_eq!(restored.config.mode, WorkflowMode::Manual);
        assert!(!restored.config.enabled);
        let first = saved.snapshot().unwrap();
        assert_eq!(first.state.generation, 1);
        assert_eq!(
            first.state.workflow_config.unwrap().config.mode,
            WorkflowMode::Manual
        );

        let again = saved.restore_workflow(current).unwrap();
        assert_eq!(again.config.mode, WorkflowMode::Manual);
        assert_eq!(saved.snapshot().unwrap().state.generation, 1);
    }

    #[test]
    fn unknown_saved_workflow_config_schema_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let saved = create(dir.path());
        let path = saved.store.directory().join("state.json");
        let mut state: ConversationStateV2 =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        state.workflow_config = Some(crate::conversation_state::SavedWorkflowConfigV1 {
            schema_version: 99,
            config: WorkflowConfig::default(),
        });
        std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
        assert!(saved.restore_workflow(WorkflowConfig::default()).is_err());
    }

    #[test]
    fn change_contract_resumes_and_trusted_approval_creates_a_revision_chain() {
        let dir = tempfile::tempdir().unwrap();
        let saved = create(dir.path());
        let draft = change_contract("routine-contract");
        let draft_reference = saved.publish_change_contract(0, draft.clone()).unwrap();
        assert_eq!(saved.snapshot().unwrap().state.generation, 1);
        assert_eq!(saved.load_change_contract(&draft_reference).unwrap(), draft);

        let approved = saved
            .approve_change_contract_trusted(1, draft_reference.clone())
            .unwrap();
        let approved_reference = approved.state().active_change_contract.unwrap();
        assert_eq!(approved_reference.revision, 2);
        let approved_body = saved.load_change_contract(&approved_reference).unwrap();
        assert_eq!(approved_body.header.supersedes, Some(draft_reference));
        assert_eq!(approved_body.header.status, ArtifactStatusV1::Approved);

        let id = saved.snapshot().unwrap().state.session_id;
        let reopened = PersistedSession::open(
            dir.path(),
            &dir.path().join("memory.sqlite3"),
            "project",
            &id,
        )
        .unwrap();
        let restored = reopened
            .restore_workflow(WorkflowConfig::default())
            .unwrap();
        assert_eq!(
            restored.state().active_change_contract,
            Some(approved_reference)
        );
    }

    #[test]
    fn restart_rejects_an_active_contract_when_its_immutable_body_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let saved = create(dir.path());
        let draft_reference = saved
            .publish_change_contract(0, change_contract("missing-body-contract"))
            .unwrap();
        let approved = saved
            .approve_change_contract_trusted(1, draft_reference)
            .unwrap();
        let approved_reference = approved.state().active_change_contract.unwrap();
        let id = saved.snapshot().unwrap().state.session_id;
        let object = dir
            .path()
            .join("workflow-artifacts-v1")
            .join(crate::conversation_state::content_hash(b"project"))
            .join(format!("{}.json", approved_reference.content_hash));
        std::fs::remove_file(object).unwrap();

        let reopened = PersistedSession::open(
            dir.path(),
            &dir.path().join("memory.sqlite3"),
            "project",
            &id,
        )
        .unwrap();
        assert!(
            reopened
                .restore_workflow(WorkflowConfig::default())
                .is_err()
        );
    }

    #[test]
    fn change_contract_rejects_stale_generation_without_a_graph_marker() {
        let dir = tempfile::tempdir().unwrap();
        let saved = create(dir.path());
        let id = saved.snapshot().unwrap().state.session_id;
        let competing = PersistedSession::open(
            dir.path(),
            &dir.path().join("memory.sqlite3"),
            "project",
            &id,
        )
        .unwrap();
        saved
            .publish_change_contract(0, change_contract("first-contract"))
            .unwrap();
        assert!(
            competing
                .publish_change_contract(0, change_contract("stale-contract"))
                .is_err()
        );
        assert_eq!(
            saved
                .snapshot()
                .unwrap()
                .state
                .workflow_artifacts
                .artifacts
                .len(),
            1
        );
    }

    #[test]
    fn fork_shares_immutable_bodies_but_not_its_artifact_graph() {
        let dir = tempfile::tempdir().unwrap();
        let saved = create(dir.path());
        let parent_reference = saved
            .publish_change_contract(0, change_contract("parent-contract"))
            .unwrap();
        let child = saved.fork(&"a".repeat(64)).unwrap();
        assert_eq!(
            child
                .load_change_contract(&parent_reference)
                .unwrap()
                .reference(),
            parent_reference
        );

        let child_generation = child.snapshot().unwrap().state.generation;
        child
            .publish_change_contract(child_generation, change_contract("child-contract"))
            .unwrap();
        assert_eq!(
            saved
                .snapshot()
                .unwrap()
                .state
                .workflow_artifacts
                .artifacts
                .len(),
            1
        );
        assert_eq!(
            child
                .snapshot()
                .unwrap()
                .state
                .workflow_artifacts
                .artifacts
                .len(),
            2
        );
    }

    #[test]
    fn append_batch_publishes_suffix_and_workflow_once() {
        let dir = tempfile::tempdir().unwrap();
        let config = WorkflowConfig::default();
        let workflow = workflow(config);
        let saved = PersistedSession::create(
            dir.path(),
            &dir.path().join("memory.sqlite3"),
            "project",
            HistoryMode::Legacy,
            Some(&workflow),
        )
        .unwrap();
        saved
            .append(Message::user("turn"), true, Some(&workflow))
            .unwrap();
        let before = saved.snapshot().unwrap().state.generation;
        saved
            .append_batch(
                &[Message::assistant("answer"), Message::assistant("detail")],
                Some(&workflow),
            )
            .unwrap();
        let after = saved.snapshot().unwrap();
        assert_eq!(after.state.generation, before + 1);
        assert_eq!(after.events.len(), 3);
        assert!(after.events[1..].iter().all(|event| !event.starts_turn));
    }

    #[test]
    fn clear_and_fork_preserve_raw_but_isolate_future_history() {
        let dir = tempfile::tempdir().unwrap();
        let saved = create(dir.path());
        saved.append(Message::user("first"), true, None).unwrap();
        saved
            .append(Message::assistant("answer"), false, None)
            .unwrap();
        let child = saved.fork(&"a".repeat(64)).unwrap();
        saved.clear().unwrap();
        saved
            .append(Message::user("second epoch"), true, None)
            .unwrap();
        let snapshot = saved.snapshot().unwrap();
        assert_eq!(snapshot.events.len(), 3);
        assert_eq!(snapshot.recent_messages().len(), 1);
        assert_eq!(child.snapshot().unwrap().events.len(), 2);
        assert_ne!(
            snapshot.state.session_id,
            child.snapshot().unwrap().state.session_id
        );
    }

    #[test]
    fn legacy_import_does_not_mutate_its_source() {
        let dir = tempfile::tempdir().unwrap();
        let messages = vec![Message::user("legacy"), Message::assistant("answer")];
        let path = dir.path().join("old.jsonl");
        let source = serde_json::to_vec(&messages).unwrap();
        std::fs::write(&path, &source).unwrap();
        let saved = PersistedSession::import(
            dir.path(),
            &dir.path().join("memory.sqlite3"),
            "project",
            HistoryMode::Legacy,
            None,
            &messages,
        )
        .unwrap();
        assert_eq!(saved.snapshot().unwrap().events.len(), 2);
        assert_eq!(std::fs::read(&path).unwrap(), source);
    }

    #[test]
    fn admitted_input_and_receipt_commit_once_under_the_host_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("memory.sqlite3");
        let persistence = PersistedSession::create_with_id(
            dir.path(),
            &database,
            "project",
            "00112233-4455-4677-8899-aabbccddeeff",
            HistoryMode::Legacy,
            None,
        )
        .unwrap();
        let mut session = Session::new();
        session.attach(persistence.clone()).unwrap();
        let admission = crate::session_manager::AdmissionId::new("adm-test").unwrap();
        let payload_hash = "a".repeat(64);

        assert!(
            session
                .push_admitted_user("isolated task", &admission, &payload_hash)
                .unwrap()
        );
        assert!(
            !session
                .push_admitted_user("isolated task", &admission, &payload_hash)
                .unwrap()
        );
        assert_eq!(session.messages.len(), 1);
        let snapshot = persistence.snapshot().unwrap();
        assert_eq!(snapshot.events.len(), 1);
        assert_eq!(
            snapshot.state.session_id,
            "00112233-4455-4677-8899-aabbccddeeff"
        );
        assert_eq!(
            snapshot
                .state
                .admission_receipts
                .get(admission.as_str())
                .unwrap()
                .payload_hash,
            payload_hash
        );
    }

    #[test]
    fn admitted_prompt_pair_has_one_receipt_and_replay_checks_the_supplement() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("memory.sqlite3");
        let persistence = PersistedSession::create_with_id(
            dir.path(),
            &database,
            "project",
            "00112233-4455-4677-8899-aabbccddeeff",
            HistoryMode::Legacy,
            None,
        )
        .unwrap();
        let mut session = Session::new();
        session.attach(persistence.clone()).unwrap();
        let admission = crate::session_manager::AdmissionId::new("adm-prompt").unwrap();
        let payload_hash = "b".repeat(64);
        let supplement = Message::user("selected prompt result");
        assert!(
            session
                .push_admitted_user_with_supplement(
                    "isolated task",
                    supplement.clone(),
                    &admission,
                    &payload_hash,
                )
                .unwrap()
        );
        let saved = persistence.snapshot().unwrap();
        assert_eq!(saved.state.generation, 1);
        assert_eq!(saved.events.len(), 2);
        assert!(saved.events[0].starts_turn);
        assert!(!saved.events[1].starts_turn);
        assert_eq!(saved.events[0].turn_id, saved.events[1].turn_id);
        assert_eq!(
            saved.state.admission_receipts[admission.as_str()].sequence,
            saved.events[0].sequence
        );
        assert!(
            !session
                .push_admitted_user_with_supplement(
                    "isolated task",
                    supplement,
                    &admission,
                    &payload_hash,
                )
                .unwrap()
        );
        assert!(
            session
                .push_admitted_user_with_supplement(
                    "isolated task",
                    Message::user("different result"),
                    &admission,
                    &payload_hash,
                )
                .is_err()
        );
        assert!(
            session
                .push_admitted_user_with_supplement(
                    "different input",
                    Message::user("selected prompt result"),
                    &admission,
                    &payload_hash,
                )
                .is_err()
        );
        assert_eq!(persistence.snapshot().unwrap().events.len(), 2);
    }
}

#[cfg(all(test, windows))]
mod windows_tests;
