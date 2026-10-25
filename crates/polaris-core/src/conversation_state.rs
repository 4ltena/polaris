//! Versioned session storage, separate from the legacy Message-only JSONL reader.
//!
//! A lock is held only for local I/O. Summary and embedding work use the returned
//! immutable snapshot and must reacquire/compare its generation before publishing.
//! SQLite tombstones are owned by the memory layer: its transaction must enclose
//! `publish` when publishing indexed summaries. This module does not authorize reads.

use crate::workflow::{
    WORKFLOW_SCHEMA_VERSION, WorkflowArtifactGraphV1, WorkflowConfig, WorkflowGatesV1,
    WorkflowStateV1,
};
use polaris_provider::{Message, Role};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub const SCHEMA_VERSION: u32 = 2;
pub const REWIND_BOUNDARY_SCHEMA_VERSION: u32 = 1;
pub const REAL_TURN_WINDOW: usize = 10;
pub const SAVED_WORKFLOW_CONFIG_SCHEMA_VERSION: u32 = 1;
const MAX_ADMISSION_RECEIPTS: usize = crate::session_manager::MAX_ADMISSION_RECEIPTS;

/// Proof that one admission was committed with one raw user event. The
/// session-manager ledger owns routing and scheduling; this marker-owned
/// receipt closes the crash window between its decision and the raw append.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionReceiptV1 {
    pub payload_hash: String,
    pub sequence: u64,
    pub turn_id: u64,
}

pub(crate) mod structured_attempt;

pub use polaris_desktop_protocol::snapshot::HistoryMode;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AncestorRange {
    pub session_id: String,
    pub epoch: u64,
    pub generation: u64,
    pub visible_summary_ids: Vec<String>,
    /// Exact v3 publication IDs granted at fork time. Missing legacy markers
    /// grant no structured-memory rows.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub visible_structured_publication_ids: Vec<String>,
}

/// Extracted source prefix committed with a v3 publication. It is marker state,
/// not a source of raw text and is intentionally absent until M3-2 publishes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StructuredCoverage {
    pub epoch: u64,
    pub last_source_sequence: u64,
    pub raw_offset: u64,
    pub prefix_hash: String,
}

/// Durable pre-user boundary. Workspace IDs are deliberately opaque: only the
/// workspace owner can resolve them into captured files or transactions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RewindBoundaryV1 {
    pub schema_version: u32,
    pub boundary_id: String,
    pub project_id: String,
    pub session_id: String,
    pub epoch: u64,
    pub source_generation: u64,
    pub target_turn_id: u64,
    pub created_at_unix_ms: u64,
    /// Set only after the target user event is durable. The picker needs no
    /// raw-file scan to render this bounded private preview after restart.
    pub user_message_preview: Option<String>,
    pub raw_offset: u64,
    pub raw_hash: String,
    pub workspace_pre_manifest_id: String,
    /// Opaque raw tool-memory object IDs visible before this input. The memory
    /// owner applies this allowlist when the prepared branch becomes active.
    pub visible_memory_raw_object_ids: Vec<String>,
    pub source_state: ConversationStateV2,
    pub completion: Option<RewindBoundaryCompletionV1>,
    /// A durable diagnostic for a turn that could not reach post-capture.
    pub abandoned_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RewindBoundaryCompletionV1 {
    pub workspace_post_manifest_id: String,
    pub eligibility: RewindEligibilityV1,
    pub effect: RewindEffectStatusV1,
    pub completed_generation: u64,
    pub completed_raw_offset: u64,
    pub completed_raw_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum RewindEligibilityV1 {
    Eligible,
    Ineligible { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "effect", rename_all = "snake_case", deny_unknown_fields)]
pub enum RewindEffectStatusV1 {
    NoExternalMutation,
    ExternalMutation { description: String },
    Unknown { description: String },
}

/// Metadata retained with a prepared branch so the tool-memory owner can use
/// the exact source visibility set after a process restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RewindBranchMetadataV1 {
    pub schema_version: u32,
    pub boundary_id: String,
    pub origin_session_id: String,
    pub origin_generation: u64,
    pub workspace_pre_manifest_id: String,
    pub workspace_post_manifest_id: String,
    /// The branch head immediately after importing the selected raw prefix.
    /// A later raw head requires its own completed rewind boundary before it
    /// can be matched to a workspace manifest.
    pub branch_initial_raw_offset: u64,
    pub branch_initial_raw_hash: String,
    pub visible_memory_raw_object_ids: Vec<String>,
}

/// Immutable candidate bytes prepared while the source writer lock is held.
/// The workspace owner uses the byte estimate for its fail-closed capacity
/// preflight, then asks the same lock to materialize this exact plan.
#[derive(Clone)]
pub(crate) struct RewindCandidatePlan {
    source_session_id: String,
    source_generation: u64,
    source_raw_offset: u64,
    source_raw_hash: String,
    expected: ConversationSnapshot,
    metadata: RewindBranchMetadataV1,
    state_bytes: u64,
    metadata_bytes: u64,
}

impl RewindCandidatePlan {
    pub(crate) fn workspace_manifests(&self) -> (&str, &str) {
        (
            &self.metadata.workspace_post_manifest_id,
            &self.metadata.workspace_pre_manifest_id,
        )
    }

    pub(crate) fn estimated_file_bytes(&self) -> u64 {
        self.expected
            .raw_offset
            .saturating_add(self.state_bytes)
            .saturating_add(self.metadata_bytes)
    }
}

/// Binds a resumed pre-rewind conversation head to the exact workspace tree
/// selected by the user. Later completed turn boundaries supersede it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceHeadBindingV1 {
    pub schema_version: u32,
    pub project_id: String,
    pub session_id: String,
    pub generation: u64,
    pub raw_commit_offset: u64,
    pub raw_commit_hash: String,
    pub workspace_manifest_id: String,
}

/// The whole workflow configuration is durable session state.  The wrapper
/// version lets a newer writer reject unknown configurations rather than
/// silently dropping fields while publishing a later state marker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedWorkflowConfigV1 {
    pub schema_version: u32,
    pub config: WorkflowConfig,
}

impl SavedWorkflowConfigV1 {
    pub fn capture(config: &WorkflowConfig) -> Self {
        Self {
            schema_version: SAVED_WORKFLOW_CONFIG_SCHEMA_VERSION,
            config: config.clone(),
        }
    }

    fn validate(&self) -> io::Result<()> {
        if self.schema_version != SAVED_WORKFLOW_CONFIG_SCHEMA_VERSION {
            return Err(invalid("Unsupported workflow configuration storage format"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationStateV2 {
    pub schema_version: u32,
    pub project_id: String,
    pub session_id: String,
    pub epoch: u64,
    pub generation: u64,
    pub raw_commit_offset: u64,
    pub raw_commit_hash: String,
    #[serde(default)]
    pub history_mode: HistoryMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_settings: Option<crate::session_settings::SavedSessionSettingsV1>,
    #[serde(
        default,
        skip_serializing_if = "crate::session_settings::SavedSessionDraftV1::is_initial"
    )]
    pub draft: crate::session_settings::SavedSessionDraftV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<crate::desktop_store::SavedGoal>,
    pub recent_turn_ids: Vec<u64>,
    pub visible_summary_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub visible_structured_publication_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_coverage: Option<StructuredCoverage>,
    /// Missing only in sessions written before workflow configuration became
    /// durable. Those are initialized explicitly by `restore_workflow`.
    #[serde(default)]
    pub workflow_config: Option<SavedWorkflowConfigV1>,
    pub workflow: WorkflowStateV1,
    pub gates: WorkflowGatesV1,
    /// M27 metadata only. Immutable artifact bodies live in the adjacent
    /// project-scoped object store. Legacy sessions deserialize an empty graph.
    #[serde(default)]
    pub workflow_artifacts: WorkflowArtifactGraphV1,
    /// Reference-only verification state. Older markers have no checkpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_checkpoint:
        Option<crate::verification::checkpoint::SavedVerificationCheckpointV1>,
    /// Admission IDs are retained for the life of the conversation so a
    /// retry after a lost response cannot append the same accepted input.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub admission_receipts: BTreeMap<String, AdmissionReceiptV1>,
    pub ancestors: Vec<AncestorRange>,
}

impl ConversationStateV2 {
    pub fn new(project_id: String, session_id: String) -> io::Result<Self> {
        let state = Self {
            schema_version: SCHEMA_VERSION,
            project_id,
            session_id,
            epoch: 0,
            generation: 0,
            raw_commit_offset: 0,
            raw_commit_hash: content_hash(&[]),
            history_mode: HistoryMode::Legacy,
            session_settings: None,
            draft: crate::session_settings::SavedSessionDraftV1::default(),
            goal: None,
            recent_turn_ids: Vec::new(),
            visible_summary_ids: Vec::new(),
            visible_structured_publication_ids: Vec::new(),
            structured_coverage: None,
            workflow_config: None,
            workflow: WorkflowStateV1::default(),
            gates: WorkflowGatesV1::default(),
            workflow_artifacts: WorkflowArtifactGraphV1::default(),
            verification_checkpoint: None,
            admission_receipts: BTreeMap::new(),
            ancestors: Vec::new(),
        };
        state.validate()?;
        Ok(state)
    }
    pub fn validate(&self) -> io::Result<()> {
        if let Some(settings) = &self.session_settings {
            settings.validate()?;
        }
        self.draft.validate()?;
        if let Some(goal) = &self.goal {
            goal.validate().map_err(io::Error::other)?;
        }
        if self.schema_version != SCHEMA_VERSION
            || self.workflow.schema_version != WORKFLOW_SCHEMA_VERSION
            || self.gates.schema_version != WORKFLOW_SCHEMA_VERSION
            || self
                .workflow_config
                .as_ref()
                .is_some_and(|config| config.validate().is_err())
        {
            return Err(invalid("Unsupported session storage format"));
        }
        self.workflow_artifacts
            .validate()
            .map_err(|_| invalid("Invalid workflow artifact graph"))?;
        if let Some(checkpoint) = &self.verification_checkpoint {
            checkpoint
                .validate()
                .map_err(|_| invalid("Invalid verification checkpoint"))?;
        }
        if self.admission_receipts.len() > MAX_ADMISSION_RECEIPTS
            || self.admission_receipts.iter().any(|(id, receipt)| {
                crate::session_manager::AdmissionId::new(id.clone()).is_err()
                    || !valid_hash(&receipt.payload_hash)
                    || receipt.sequence == 0
                    || receipt.turn_id == 0
            })
        {
            return Err(invalid("Invalid admission receipt"));
        }
        if let Some(reference) = &self.workflow.active_change_contract {
            self.workflow_artifacts
                .approved_routine_contract(reference)
                .map_err(|_| invalid("Invalid active change contract"))?;
        }
        if self.project_id.is_empty()
            || !valid_session_id(&self.session_id)
            || !valid_hash(&self.raw_commit_hash)
            || self.recent_turn_ids.len() > REAL_TURN_WINDOW
            || self.recent_turn_ids.windows(2).any(|ids| ids[0] >= ids[1])
            || self.visible_summary_ids.iter().any(|id| id.is_empty())
            || self
                .visible_summary_ids
                .iter()
                .collect::<BTreeSet<_>>()
                .len()
                != self.visible_summary_ids.len()
            || self
                .visible_structured_publication_ids
                .iter()
                .any(|id| id.is_empty())
            || self
                .visible_structured_publication_ids
                .iter()
                .collect::<BTreeSet<_>>()
                .len()
                != self.visible_structured_publication_ids.len()
            || self.structured_coverage.as_ref().is_some_and(|coverage| {
                coverage.epoch != self.epoch
                    || coverage.last_source_sequence == 0
                    || !valid_hash(&coverage.prefix_hash)
            })
            || (self.history_mode != HistoryMode::Structured10
                && (!self.visible_structured_publication_ids.is_empty()
                    || self.structured_coverage.is_some()
                    || self
                        .ancestors
                        .iter()
                        .any(|ancestor| !ancestor.visible_structured_publication_ids.is_empty())))
            || self.ancestors.iter().any(|ancestor| {
                !valid_session_id(&ancestor.session_id)
                    || ancestor.session_id == self.session_id
                    || ancestor
                        .visible_structured_publication_ids
                        .iter()
                        .any(|id| id.is_empty())
                    || ancestor
                        .visible_structured_publication_ids
                        .iter()
                        .collect::<BTreeSet<_>>()
                        .len()
                        != ancestor.visible_structured_publication_ids.len()
            })
        {
            return Err(invalid("Invalid session state identifiers or range"));
        }
        Ok(())
    }
}

/// `starts_turn` is set only for actual user input, not few-shot or retrieved data.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawEventV2 {
    pub schema_version: u32,
    pub sequence: u64,
    pub epoch: u64,
    pub turn_id: u64,
    pub starts_turn: bool,
    pub message: Message,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationSnapshot {
    pub state: ConversationStateV2,
    pub raw_hash: String,
    pub raw_offset: u64,
    pub events: Vec<RawEventV2>,
}
impl ConversationSnapshot {
    pub fn active_turns(&self) -> Vec<u64> {
        self.events
            .iter()
            .filter(|event| event.epoch == self.state.epoch && event.starts_turn)
            .map(|event| event.turn_id)
            .collect()
    }
    pub fn expired_turns(&self) -> Vec<u64> {
        let mut turns = self.active_turns();
        turns.truncate(turns.len().saturating_sub(REAL_TURN_WINDOW));
        turns
    }
    pub fn recent_messages(&self) -> Vec<Message> {
        self.recent_request(None).0
    }
    pub(crate) fn recent_request(
        &self,
        source: Option<&crate::request_input::SourceBinding>,
    ) -> (Vec<Message>, Option<crate::request_input::BoundInput>) {
        let turns = self.active_turns();
        let first = turns.get(turns.len().saturating_sub(REAL_TURN_WINDOW));
        crate::request_input::project_events(
            &self.state.session_id,
            self.events.iter().filter(|event| {
                event.epoch == self.state.epoch
                    && first.is_some_and(|first| event.turn_id >= *first)
            }),
            source,
        )
    }
    /// Expired turns must not split a function call/result group.
    pub fn validate_complete_turn(&self, turn_id: u64) -> io::Result<()> {
        validate_complete_turn_events(
            self.events
                .iter()
                .filter(|event| event.epoch == self.state.epoch && event.turn_id == turn_id),
        )
    }

    fn validate_admission_receipts(&self) -> io::Result<()> {
        for receipt in self.state.admission_receipts.values() {
            let event = self
                .events
                .iter()
                .find(|event| event.sequence == receipt.sequence)
                .ok_or_else(|| invalid("Missing admission receipt source text"))?;
            if !event.starts_turn || event.turn_id != receipt.turn_id {
                return Err(invalid("Invalid admission receipt source boundary"));
            }
        }
        Ok(())
    }
}

pub(crate) fn validate_complete_turn_events<'a>(
    events: impl Iterator<Item = &'a RawEventV2>,
) -> io::Result<()> {
    let mut events = events.peekable();
    if !events.peek().is_some_and(|event| event.starts_turn) {
        return Err(invalid("Missing original turn"));
    }
    validate_complete_messages(events.map(|event| &event.message))
}

/// Structural round-trip validation only; Message-only history carries no
/// starts-turn or prompt authorization provenance.
pub(crate) fn validate_complete_messages<'a>(
    messages: impl Iterator<Item = &'a Message>,
) -> io::Result<()> {
    let mut pending = BTreeSet::new();
    let mut seen = BTreeSet::new();
    for msg in messages {
        if !pending.is_empty() && msg.role != Role::Tool {
            return Err(invalid("Tool result is incomplete"));
        }
        for call in &msg.tool_calls {
            if msg.role != Role::Assistant || call.id.is_empty() || !seen.insert(call.id.clone()) {
                return Err(invalid("Invalid tool call ID"));
            }
            pending.insert(call.id.clone());
        }
        if msg.role == Role::Tool {
            if !msg
                .tool_call_id
                .as_ref()
                .is_some_and(|id| pending.remove(id))
            {
                return Err(invalid("Unmatched tool result"));
            }
        } else if msg.tool_call_id.is_some() {
            return Err(invalid("Result ID on a non-tool message"));
        }
    }
    if !pending.is_empty() {
        return Err(invalid("Tool result is incomplete"));
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct ConversationStore {
    directory: PathBuf,
    project_id: String,
    session_id: String,
    #[cfg(all(test, windows))]
    windows: Option<WindowsConversationStore>,
}

impl ConversationStore {
    #[cfg(all(test, windows))]
    pub fn create(data_root: &Path, state: ConversationStateV2) -> io::Result<Self> {
        state.validate()?;
        windows_state_admission(&state)?;
        if state.generation != 0
            || state.raw_commit_offset != 0
            || state.raw_commit_hash != content_hash(&[])
            || !state.recent_turn_ids.is_empty()
            || !state.visible_summary_ids.is_empty()
            || !state.visible_structured_publication_ids.is_empty()
            || state.structured_coverage.is_some()
        {
            return Err(invalid("Invalid new session state"));
        }
        let windows = WindowsConversationStore::create(data_root, &state.session_id)?;
        windows.atomic_write("state.json", &serde_json::to_vec(&state)?)?;
        windows.directory.sync_all().map_err(windows_error)?;
        Ok(Self {
            directory: data_root.join("sessions-v2").join(&state.session_id),
            project_id: state.project_id,
            session_id: state.session_id,
            windows: Some(windows),
        })
    }

    #[cfg(all(test, windows))]
    pub(crate) fn open_with_reader(
        data_root: &Path,
        project_id: &str,
        session_id: &str,
        read: &mut impl FnMut(&Path) -> io::Result<Vec<u8>>,
    ) -> io::Result<Self> {
        if !valid_session_id(session_id) {
            return Err(invalid("Invalid session ID"));
        }
        let store = Self {
            directory: data_root.join("sessions-v2").join(session_id),
            project_id: project_id.into(),
            session_id: session_id.into(),
            windows: Some(WindowsConversationStore::open(data_root, session_id)?),
        };
        store.lock()?.snapshot_with_reader(read)?;
        Ok(store)
    }

    #[cfg(all(test, windows))]
    fn windows(&self) -> io::Result<&WindowsConversationStore> {
        self.windows.as_ref().ok_or_else(windows_unsupported)
    }

    #[cfg(all(test, windows))]
    pub fn lock(&self) -> io::Result<LockedConversation<'_>> {
        let file = self
            .windows()?
            .directory
            .open_file(std::ffi::OsStr::new("writer.lock"), true, false)
            .map_err(windows_error)?;
        file.try_lock().map_err(io::Error::other)?;
        Ok(LockedConversation {
            store: self,
            _lock: file,
        })
    }

    #[cfg(unix)]
    pub fn create(data_root: &Path, state: ConversationStateV2) -> io::Result<Self> {
        state.validate()?;
        if state.generation != 0
            || state.raw_commit_offset != 0
            || state.raw_commit_hash != content_hash(&[])
            || !state.recent_turn_ids.is_empty()
            || !state.visible_summary_ids.is_empty()
            || !state.visible_structured_publication_ids.is_empty()
            || state.structured_coverage.is_some()
        {
            return Err(invalid("Invalid new session state"));
        }
        let root = data_root.join("sessions-v2");
        fs::create_dir_all(&root)?;
        reject_symlink(&root)?;
        let directory = root.join(&state.session_id);
        // Never overwrite an existing session, even after an interrupted creation.
        private_directory(&directory)?;
        private_directory(&directory.join("snapshots"))?;
        let raw = private_file(&directory.join("raw.jsonl"), true)?;
        raw.sync_all()?;
        let lock = private_file(&directory.join("writer.lock"), true)?;
        lock.sync_all()?;
        atomic_write(&directory.join("state.json"), &serde_json::to_vec(&state)?)?;
        File::open(&root)?.sync_all()?;
        Ok(Self {
            directory,
            project_id: state.project_id,
            session_id: state.session_id,
        })
    }
    #[cfg(all(not(unix), not(all(test, windows))))]
    pub fn create(_data_root: &Path, _state: ConversationStateV2) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Cannot verify conversation storage protections on this platform",
        ))
    }

    pub fn open(data_root: &Path, project_id: &str, session_id: &str) -> io::Result<Self> {
        Self::open_with_reader(data_root, project_id, session_id, &mut |path| {
            fs::read(path)
        })
    }
    #[cfg(unix)]
    pub(crate) fn open_with_reader(
        data_root: &Path,
        project_id: &str,
        session_id: &str,
        read: &mut impl FnMut(&Path) -> io::Result<Vec<u8>>,
    ) -> io::Result<Self> {
        if !valid_session_id(session_id) {
            return Err(invalid("Invalid session ID"));
        }
        let directory = data_root.join("sessions-v2").join(session_id);
        reject_symlink(&data_root.join("sessions-v2"))?;
        reject_symlink(&directory)?;
        let store = Self {
            directory,
            project_id: project_id.into(),
            session_id: session_id.into(),
        };
        store.lock()?.snapshot_with_reader(read)?;
        Ok(store)
    }

    #[cfg(all(not(unix), not(all(test, windows))))]
    pub(crate) fn open_with_reader(
        _data_root: &Path,
        _project_id: &str,
        _session_id: &str,
        _read: &mut impl FnMut(&Path) -> io::Result<Vec<u8>>,
    ) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Cannot verify conversation storage protections on this platform",
        ))
    }
    pub fn directory(&self) -> &Path {
        &self.directory
    }
    pub fn rewind_branch_metadata(&self) -> io::Result<Option<RewindBranchMetadataV1>> {
        self.rewind_branch_metadata_with_reader(&mut |path| fs::read(path))
    }
    #[cfg(not(all(test, windows)))]
    pub(crate) fn rewind_branch_metadata_with_reader(
        &self,
        read: &mut impl FnMut(&Path) -> io::Result<Vec<u8>>,
    ) -> io::Result<Option<RewindBranchMetadataV1>> {
        let path = self.directory.join("rewind-branch-v1.json");
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(invalid("Symlinks within sessions are not allowed"));
            }
            Ok(metadata) if !metadata.file_type().is_file() => {
                return Err(invalid("Rewind branch metadata is not a regular file"));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        }
        let metadata: RewindBranchMetadataV1 = serde_json::from_slice(&read(&path)?)?;
        validate_rewind_branch_metadata(&metadata)?;
        Ok(Some(metadata))
    }
    #[cfg(all(test, windows))]
    pub(crate) fn rewind_branch_metadata_with_reader(
        &self,
        _read: &mut impl FnMut(&Path) -> io::Result<Vec<u8>>,
    ) -> io::Result<Option<RewindBranchMetadataV1>> {
        self.windows()?.require_absent("rewind-branch-v1.json")?;
        Ok(None)
    }

    #[cfg(all(test, windows))]
    pub fn workspace_head_binding(&self) -> io::Result<Option<WorkspaceHeadBindingV1>> {
        self.windows()?
            .require_absent("workspace-head-binding-v1.json")?;
        Ok(None)
    }

    #[cfg(not(all(test, windows)))]
    pub fn workspace_head_binding(&self) -> io::Result<Option<WorkspaceHeadBindingV1>> {
        let path = self.directory.join("workspace-head-binding-v1.json");
        if !path.exists() {
            return Ok(None);
        }
        reject_symlink(&path)?;
        let binding: WorkspaceHeadBindingV1 = serde_json::from_slice(&fs::read(path)?)?;
        validate_workspace_head_binding(&binding)?;
        if binding.project_id != self.project_id || binding.session_id != self.session_id {
            return Err(invalid(
                "Workspace file binding does not match the conversation",
            ));
        }
        Ok(Some(binding))
    }

    pub fn bind_workspace_head(
        &self,
        expected: &ConversationSnapshot,
        manifest_id: &str,
    ) -> io::Result<()> {
        self.write_workspace_head_binding(expected, manifest_id, false)
    }

    /// The workspace owner calls this only while its first-legacy-binding
    /// reservation is held. A failed attempt may have written a sidecar before
    /// the tree changed, so a retry for the same committed conversation head
    /// must be able to replace that uncommitted sidecar.
    #[cfg(unix)]
    pub fn repair_pending_workspace_head_binding(
        &self,
        expected: &ConversationSnapshot,
        manifest_id: &str,
        permit: crate::rewind_workspace::PendingLegacyBindingPermit<'_>,
    ) -> io::Result<()> {
        if permit.session_id() != self.session_id {
            return Err(invalid(
                "Legacy conversation binding reservation does not match the conversation ID",
            ));
        }
        self.write_workspace_head_binding(expected, manifest_id, true)
    }

    fn write_workspace_head_binding(
        &self,
        expected: &ConversationSnapshot,
        manifest_id: &str,
        pending_repair: bool,
    ) -> io::Result<()> {
        validate_opaque_id(manifest_id, "workspace manifest ID")?;
        let lock = self.lock()?;
        let current = lock.snapshot()?;
        if current.state.project_id != expected.state.project_id
            || current.state.session_id != expected.state.session_id
            || current.state.generation != expected.state.generation
            || current.raw_offset != expected.raw_offset
            || current.raw_hash != expected.raw_hash
            || current.raw_offset != current.state.raw_commit_offset
            || current.raw_hash != current.state.raw_commit_hash
        {
            return Err(invalid(
                "Cannot bind workspace files because the conversation changed",
            ));
        }
        if let Some(turn) = current.active_turns().last() {
            current.validate_complete_turn(*turn)?;
        }
        let binding = WorkspaceHeadBindingV1 {
            schema_version: 1,
            project_id: self.project_id.clone(),
            session_id: self.session_id.clone(),
            generation: current.state.generation,
            raw_commit_offset: current.raw_offset,
            raw_commit_hash: current.raw_hash,
            workspace_manifest_id: manifest_id.into(),
        };
        if let Some(existing) = self.workspace_head_binding()? {
            if existing == binding {
                return Ok(());
            }
            if !pending_repair
                || existing.project_id != binding.project_id
                || existing.session_id != binding.session_id
                || existing.generation != binding.generation
                || existing.raw_commit_offset != binding.raw_commit_offset
                || existing.raw_commit_hash != binding.raw_commit_hash
            {
                return Err(invalid(
                    "The conversation is already bound to different workspace files",
                ));
            }
        }
        atomic_write(
            &self.directory.join("workspace-head-binding-v1.json"),
            &serde_json::to_vec(&binding)?,
        )
    }
    /// Reads the immutable raw snapshot named by an indexed conversation source.
    /// The caller supplies only a generation and hash already verified by the
    /// memory index; this method never accepts an arbitrary filesystem path.
    pub fn load_snapshot(
        &self,
        generation: u64,
        raw_hash: &str,
    ) -> io::Result<ConversationSnapshot> {
        if !valid_hash(raw_hash) {
            return Err(invalid("Invalid source snapshot hash"));
        }
        let path = self
            .directory
            .join("snapshots")
            .join(format!("{generation}-{raw_hash}.json"));
        #[cfg(not(all(test, windows)))]
        let bytes = {
            reject_symlink(&path)?;
            fs::read(&path)?
        };
        #[cfg(all(test, windows))]
        let bytes = self
            .windows()?
            .read_snapshot(path.file_name().ok_or_else(windows_unsupported)?)?;
        decode_saved_snapshot(
            &bytes,
            &self.project_id,
            &self.session_id,
            generation,
            raw_hash,
        )
    }
    /// The v2 root inferred from this session's fixed directory layout.
    pub fn data_root(&self) -> io::Result<&Path> {
        self.directory
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| invalid("Invalid session storage location"))
    }
    #[cfg(unix)]
    pub fn lock(&self) -> io::Result<LockedConversation<'_>> {
        reject_symlink(&self.directory.join("writer.lock"))?;
        let file = private_file(&self.directory.join("writer.lock"), false)?;
        file.try_lock().map_err(io::Error::other)?;
        Ok(LockedConversation {
            store: self,
            _lock: file,
        })
    }

    #[cfg(all(not(unix), not(all(test, windows))))]
    pub fn lock(&self) -> io::Result<LockedConversation<'_>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Cannot verify conversation storage protections on this platform",
        ))
    }
}

/// Dropping the guard explicitly releases the OS lock, including cancellation.
/// Do not retain this across provider calls.
pub struct LockedConversation<'a> {
    store: &'a ConversationStore,
    #[cfg(not(all(test, windows)))]
    _lock: File,
    #[cfg(all(test, windows))]
    _lock: polaris_auth::protection::WindowsFamilyFile,
}
impl Drop for LockedConversation<'_> {
    fn drop(&mut self) {
        // Closing this descriptor alone is insufficient while a concurrently
        // forked process retains the same open-file description before exec.
        // An explicit unlock ends our critical section without waiting for it.
        let _ = self._lock.unlock();
    }
}
impl LockedConversation<'_> {
    /// Creates a durable boundary before admitting one user input. The caller
    /// must finish or invalidate it before accepting the following user turn.
    pub fn begin_rewind_boundary(
        &mut self,
        workspace_pre_manifest_id: String,
        visible_memory_raw_object_ids: Vec<String>,
    ) -> io::Result<RewindBoundaryV1> {
        validate_opaque_id(&workspace_pre_manifest_id, "workspace pre manifest ID")?;
        validate_memory_raw_object_ids(&visible_memory_raw_object_ids)?;
        let snapshot = self.snapshot()?;
        if snapshot.raw_offset != snapshot.state.raw_commit_offset
            || snapshot.raw_hash != snapshot.state.raw_commit_hash
        {
            return Err(invalid(
                "The rewind boundary source marker is not finalized",
            ));
        }
        if let Some(turn) = snapshot.active_turns().last() {
            snapshot.validate_complete_turn(*turn)?;
        }
        let mut boundaries = self.load_rewind_boundaries()?;
        if boundaries
            .iter()
            .any(|boundary| boundary.completion.is_none() && boundary.abandoned_reason.is_none())
        {
            return Err(invalid("An incomplete rewind boundary exists"));
        }
        let target_turn_id = snapshot.events.last().map_or(Ok(1), |event| {
            event
                .turn_id
                .checked_add(1)
                .ok_or_else(|| invalid("Turn number limit reached"))
        })?;
        let boundary = RewindBoundaryV1 {
            schema_version: REWIND_BOUNDARY_SCHEMA_VERSION,
            boundary_id: new_rewind_boundary_id()?,
            project_id: snapshot.state.project_id.clone(),
            session_id: snapshot.state.session_id.clone(),
            epoch: snapshot.state.epoch,
            source_generation: snapshot.state.generation,
            target_turn_id,
            created_at_unix_ms: rewind_boundary_timestamp()?,
            user_message_preview: None,
            raw_offset: snapshot.raw_offset,
            raw_hash: snapshot.raw_hash.clone(),
            workspace_pre_manifest_id,
            visible_memory_raw_object_ids,
            source_state: snapshot.state,
            completion: None,
            abandoned_reason: None,
        };
        validate_rewind_boundary(&boundary)?;
        boundaries.push(boundary.clone());
        self.save_rewind_boundaries(&boundaries)?;
        Ok(boundary)
    }

    /// Finishes the boundary only when its user turn is the latest completed
    /// turn. This prevents a delayed post-capture from claiming a later state.
    pub fn finish_rewind_boundary(
        &mut self,
        boundary_id: &str,
        workspace_post_manifest_id: String,
        eligibility: RewindEligibilityV1,
        effect: RewindEffectStatusV1,
    ) -> io::Result<RewindBoundaryV1> {
        validate_opaque_id(boundary_id, "rewind boundary ID")?;
        validate_opaque_id(&workspace_post_manifest_id, "workspace post manifest ID")?;
        validate_rewind_completion_input(&eligibility, &effect)?;
        let snapshot = self.snapshot()?;
        if snapshot.raw_offset != snapshot.state.raw_commit_offset
            || snapshot.raw_hash != snapshot.state.raw_commit_hash
        {
            return Err(invalid(
                "The rewind boundary post-source marker is not finalized",
            ));
        }
        let mut boundaries = self.load_rewind_boundaries()?;
        let boundary = boundaries
            .iter_mut()
            .find(|boundary| boundary.boundary_id == boundary_id)
            .ok_or_else(|| invalid("No rewind boundary"))?;
        validate_rewind_boundary(boundary)?;
        if boundary.completion.is_some() {
            return Err(invalid("The rewind boundary is already complete"));
        }
        if boundary.abandoned_reason.is_some() {
            return Err(invalid("The rewind boundary was discarded"));
        }
        if boundary.project_id != snapshot.state.project_id
            || boundary.session_id != snapshot.state.session_id
            || boundary.epoch != snapshot.state.epoch
            || snapshot.active_turns().last() != Some(&boundary.target_turn_id)
        {
            return Err(invalid("The rewind boundary turn is not the current end"));
        }
        snapshot.validate_complete_turn(boundary.target_turn_id)?;
        let user = snapshot
            .events
            .iter()
            .find(|event| {
                event.epoch == boundary.epoch
                    && event.turn_id == boundary.target_turn_id
                    && event.starts_turn
            })
            .ok_or_else(|| invalid("Missing rewind boundary user input"))?;
        if user.message.role != Role::User {
            return Err(invalid("Invalid rewind boundary user input"));
        }
        boundary.user_message_preview = Some(rewind_message_preview(&user.message.content));
        boundary.completion = Some(RewindBoundaryCompletionV1 {
            workspace_post_manifest_id,
            eligibility,
            effect,
            completed_generation: snapshot.state.generation,
            completed_raw_offset: snapshot.raw_offset,
            completed_raw_hash: snapshot.raw_hash,
        });
        validate_rewind_boundary(boundary)?;
        let completed = boundary.clone();
        self.save_rewind_boundaries(&boundaries)?;
        Ok(completed)
    }

    /// Permanently marks an interrupted or failed turn unavailable. This keeps
    /// its diagnostic without preventing the following user admission.
    pub fn abandon_rewind_boundary(
        &mut self,
        boundary_id: &str,
        reason: String,
    ) -> io::Result<RewindBoundaryV1> {
        validate_opaque_id(boundary_id, "rewind boundary ID")?;
        validate_rewind_reason(&reason)?;
        let mut boundaries = self.load_rewind_boundaries()?;
        let boundary = boundaries
            .iter_mut()
            .find(|boundary| boundary.boundary_id == boundary_id)
            .ok_or_else(|| invalid("No rewind boundary"))?;
        validate_rewind_boundary(boundary)?;
        if boundary.completion.is_some() || boundary.abandoned_reason.is_some() {
            return Err(invalid("The rewind boundary is already finalized"));
        }
        boundary.abandoned_reason = Some(reason);
        validate_rewind_boundary(boundary)?;
        let abandoned = boundary.clone();
        self.save_rewind_boundaries(&boundaries)?;
        Ok(abandoned)
    }

    /// Startup recovery marks every still-pending boundary unavailable without
    /// touching completed records or requiring a readable raw snapshot.
    pub fn abandon_unfinished_rewind_boundaries(
        &mut self,
        reason: String,
    ) -> io::Result<Vec<RewindBoundaryV1>> {
        validate_rewind_reason(&reason)?;
        let mut boundaries = self.load_rewind_boundaries()?;
        let mut abandoned = Vec::new();
        for boundary in &mut boundaries {
            if boundary.completion.is_none() && boundary.abandoned_reason.is_none() {
                boundary.abandoned_reason = Some(reason.clone());
                validate_rewind_boundary(boundary)?;
                abandoned.push(boundary.clone());
            }
        }
        if !abandoned.is_empty() {
            self.save_rewind_boundaries(&boundaries)?;
        }
        Ok(abandoned)
    }

    pub fn list_rewind_boundaries(&self) -> io::Result<Vec<RewindBoundaryV1>> {
        let snapshot = self.snapshot()?;
        let boundaries = self.load_rewind_boundaries()?;
        let latest_is_current_safe = boundaries.last().is_some_and(|boundary| {
            boundary.completion.as_ref().is_some_and(|completion| {
                matches!(completion.eligibility, RewindEligibilityV1::Eligible)
                    && matches!(completion.effect, RewindEffectStatusV1::NoExternalMutation)
                    && completion.completed_raw_offset == snapshot.raw_offset
                    && completion.completed_raw_hash == snapshot.raw_hash
            })
        });
        if !latest_is_current_safe {
            return Ok(Vec::new());
        }
        Ok(boundaries
            .iter()
            .enumerate()
            .filter_map(|(index, boundary)| {
                let complete_safe_chain = boundaries[index..].iter().all(|later| {
                    matches!(
                        later.completion.as_ref(),
                        Some(RewindBoundaryCompletionV1 {
                            eligibility: RewindEligibilityV1::Eligible,
                            effect: RewindEffectStatusV1::NoExternalMutation,
                            ..
                        })
                    )
                });
                let selectable = boundary.project_id == snapshot.state.project_id
                    && boundary.session_id == snapshot.state.session_id
                    && complete_safe_chain;
                selectable.then(|| boundary.clone())
            })
            .collect())
    }

    /// Builds a full raw-prefix branch from an eligible durable boundary. It
    /// deliberately differs from `fork`, whose ten-turn behavior is retained.
    #[cfg(test)]
    pub fn prepare_rewind_branch(
        &self,
        data_root: &Path,
        boundary_id: &str,
        new_id: &str,
    ) -> io::Result<ConversationStore> {
        if !valid_session_id(new_id) {
            return Err(invalid("Invalid fork session ID"));
        }
        let plan = self.plan_rewind_branch(boundary_id, new_id)?;
        self.create_rewind_branch_from_plan(data_root, plan)
    }

    pub(crate) fn plan_rewind_branch(
        &self,
        boundary_id: &str,
        new_id: &str,
    ) -> io::Result<RewindCandidatePlan> {
        if !valid_session_id(new_id) {
            return Err(invalid("Invalid fork session ID"));
        }
        let current = self.snapshot()?;
        let boundary = self
            .list_rewind_boundaries()?
            .into_iter()
            .find(|boundary| boundary.boundary_id == boundary_id)
            .ok_or_else(|| invalid("No selectable rewind boundaries"))?;
        validate_rewind_boundary_for_branch(&boundary, &current)?;
        let (expected, metadata) = rewind_branch_candidate_plan(&current, &boundary, new_id)?;
        let state_bytes = u64::try_from(serde_json::to_vec(&expected.state)?.len())
            .map_err(|_| invalid("Rewind candidate state limit reached"))?;
        let metadata_bytes = u64::try_from(serde_json::to_vec(&metadata)?.len())
            .map_err(|_| invalid("Rewind candidate metadata limit reached"))?;
        Ok(RewindCandidatePlan {
            source_session_id: current.state.session_id.clone(),
            source_generation: current.state.generation,
            source_raw_offset: current.raw_offset,
            source_raw_hash: current.raw_hash.clone(),
            expected,
            metadata,
            state_bytes,
            metadata_bytes,
        })
    }

    pub(crate) fn create_rewind_branch_from_plan(
        &self,
        data_root: &Path,
        plan: RewindCandidatePlan,
    ) -> io::Result<ConversationStore> {
        let current = self.snapshot()?;
        if current.state.session_id != plan.source_session_id
            || current.state.generation != plan.source_generation
            || current.raw_offset != plan.source_raw_offset
            || current.raw_hash != plan.source_raw_hash
        {
            return Err(invalid(
                "The rewind candidate source changed after planning",
            ));
        }
        ConversationStore::create_rewind_candidate(data_root, plan.expected, &plan.metadata)
    }
}

impl ConversationStore {
    /// Verify the durable evidence which makes a prepared rewind child a
    /// candidate for activation.  This deliberately does not authorize the
    /// child: the workspace transaction owner performs that separate check.
    pub fn validate_rewind_branch_candidate(
        source: &ConversationStore,
        child: &ConversationStore,
    ) -> io::Result<RewindBranchMetadataV1> {
        Self::validate_rewind_branch_candidate_with_reader(source, child, &mut |path| {
            fs::read(path)
        })
    }
    pub(crate) fn validate_rewind_branch_candidate_with_reader(
        source: &ConversationStore,
        child: &ConversationStore,
        read: &mut impl FnMut(&Path) -> io::Result<Vec<u8>>,
    ) -> io::Result<RewindBranchMetadataV1> {
        let source_lock = source.lock()?;
        let source_snapshot = source_lock.snapshot_with_reader(read)?;
        let metadata = child
            .rewind_branch_metadata_with_reader(read)?
            .ok_or_else(|| invalid("Missing rewind candidate metadata"))?;
        if metadata.origin_session_id != source_snapshot.state.session_id {
            return Err(invalid("Rewind candidate source mismatch"));
        }
        let boundaries = source_lock.load_rewind_boundaries_with_reader(read)?;
        let matches: Vec<_> = boundaries
            .iter()
            .filter(|boundary| boundary.boundary_id == metadata.boundary_id)
            .collect();
        let [boundary] = matches.as_slice() else {
            return Err(invalid(
                "The rewind candidate source boundary is not unique",
            ));
        };
        validate_rewind_boundary_for_candidate(boundary, &source_snapshot)?;
        let (expected, expected_metadata) =
            rewind_branch_candidate_plan(&source_snapshot, boundary, &child.session_id)?;
        if metadata != expected_metadata {
            return Err(invalid(
                "Rewind candidate metadata does not match source evidence",
            ));
        }

        let child_lock = child.lock()?;
        let child_snapshot = child_lock.snapshot_with_reader(read)?;
        if child_snapshot.state.project_id != expected.state.project_id
            || child_snapshot.state.session_id != expected.state.session_id
            || child_snapshot.state.epoch != expected.state.epoch
            || child_snapshot.state.history_mode != expected.state.history_mode
            || child_snapshot.state.ancestors != expected.state.ancestors
        {
            return Err(invalid(
                "Rewind candidate conversation state does not match fork evidence",
            ));
        }
        let raw_path = child.directory.join("raw.jsonl");
        reject_symlink(&raw_path)?;
        let raw = read(&raw_path)?;
        let initial_len = usize::try_from(expected.raw_offset)
            .map_err(|_| invalid("Invalid rewind candidate source range"))?;
        if raw.len() < initial_len
            || content_hash(&raw[..initial_len]) != expected.raw_hash
            || child_snapshot.raw_offset < expected.raw_offset
        {
            return Err(invalid("Rewind candidate initial source prefix mismatch"));
        }
        Ok(metadata)
    }

    #[cfg(unix)]
    fn create_rewind_candidate(
        data_root: &Path,
        expected: ConversationSnapshot,
        metadata: &RewindBranchMetadataV1,
    ) -> io::Result<Self> {
        expected.state.validate()?;
        validate_rewind_branch_metadata(metadata)?;
        if expected.state.generation != 1
            || expected.raw_offset != expected.state.raw_commit_offset
            || expected.raw_hash != expected.state.raw_commit_hash
            || metadata.branch_initial_raw_offset != expected.raw_offset
            || metadata.branch_initial_raw_hash != expected.raw_hash
        {
            return Err(invalid("Invalid rewind candidate initial state"));
        }
        let root = data_root.join("sessions-v2");
        fs::create_dir_all(&root)?;
        reject_symlink(&root)?;
        let directory = root.join(&expected.state.session_id);
        // Metadata is the admission marker.  It must be durable before any
        // valid state can make this non-empty child appear resumable.
        private_directory(&directory)?;
        write_rewind_branch_metadata_create_new(&directory, metadata)?;
        fail_point("rewind_candidate_after_metadata")?;
        private_directory(&directory.join("snapshots"))?;
        let mut raw = private_file(&directory.join("raw.jsonl"), true)?;
        for event in &expected.events {
            serde_json::to_writer(&mut raw, event)?;
            raw.write_all(b"\n")?;
        }
        raw.sync_all()?;
        let lock = private_file(&directory.join("writer.lock"), true)?;
        lock.sync_all()?;
        atomic_write(
            &directory.join("state.json"),
            &serde_json::to_vec(&expected.state)?,
        )?;
        File::open(&directory)?.sync_all()?;
        File::open(&root)?.sync_all()?;
        let child = Self {
            directory,
            project_id: expected.state.project_id.clone(),
            session_id: expected.state.session_id.clone(),
        };
        child.lock()?.check_snapshot(&expected)?;
        Ok(child)
    }

    #[cfg(not(unix))]
    fn create_rewind_candidate(
        _data_root: &Path,
        _expected: ConversationSnapshot,
        _metadata: &RewindBranchMetadataV1,
    ) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Cannot verify conversation storage protections on this platform",
        ))
    }
}

impl LockedConversation<'_> {
    fn load_rewind_boundaries(&self) -> io::Result<Vec<RewindBoundaryV1>> {
        self.load_rewind_boundaries_with_reader(&mut |path| fs::read(path))
    }
    #[cfg(not(all(test, windows)))]
    fn load_rewind_boundaries_with_reader(
        &self,
        read: &mut impl FnMut(&Path) -> io::Result<Vec<u8>>,
    ) -> io::Result<Vec<RewindBoundaryV1>> {
        let path = self.store.directory.join("rewind-boundaries-v1.json");
        if !path.exists() {
            return Ok(Vec::new());
        }
        reject_symlink(&path)?;
        decode_rewind_boundaries(&read(&path)?)
    }

    #[cfg(all(test, windows))]
    fn load_rewind_boundaries_with_reader(
        &self,
        _read: &mut impl FnMut(&Path) -> io::Result<Vec<u8>>,
    ) -> io::Result<Vec<RewindBoundaryV1>> {
        self.store
            .windows()?
            .require_absent("rewind-boundaries-v1.json")?;
        Ok(Vec::new())
    }

    fn save_rewind_boundaries(&self, boundaries: &[RewindBoundaryV1]) -> io::Result<()> {
        for boundary in boundaries {
            validate_rewind_boundary(boundary)?;
        }
        atomic_write(
            &self.store.directory.join("rewind-boundaries-v1.json"),
            &serde_json::to_vec(boundaries)?,
        )
    }

    /// Copies only the active real-turn window into a fresh namespace. Ancestor
    /// reads remain bounded at the parent's committed generation; the memory
    /// layer must still check its latest tombstone on every such lookup.
    pub fn fork(
        &self,
        data_root: &Path,
        new_id: &str,
        artifact_hash: &str,
    ) -> io::Result<ConversationStore> {
        let snapshot = self.snapshot()?;
        let (next, selected) = fork_plan(&snapshot, new_id, artifact_hash)?;
        let child = ConversationStore::create(data_root, next)?;
        let mut lock = child.lock()?;
        for (message, starts_turn) in selected {
            lock.append(message, starts_turn)?;
        }
        let copied = lock.snapshot()?;
        lock.publish(&copied, copied.state.clone())?;
        drop(lock);
        Ok(child)
    }

    pub fn snapshot(&self) -> io::Result<ConversationSnapshot> {
        self.snapshot_with_reader(&mut |path| fs::read(path))
    }
    pub(crate) fn snapshot_with_reader(
        &self,
        read: &mut impl FnMut(&Path) -> io::Result<Vec<u8>>,
    ) -> io::Result<ConversationSnapshot> {
        let state_path = self.store.directory.join("state.json");
        #[cfg(not(all(test, windows)))]
        let state_bytes = {
            reject_symlink(&state_path)?;
            read(&state_path)?
        };
        #[cfg(all(test, windows))]
        let state_bytes = {
            let _ = (read, &state_path);
            self.store.windows()?.read("state.json")?
        };
        let state: ConversationStateV2 = serde_json::from_slice(&state_bytes)?;
        state.validate()?;
        #[cfg(all(test, windows))]
        windows_state_admission(&state)?;
        if state.project_id != self.store.project_id || state.session_id != self.store.session_id {
            return Err(invalid("Session ownership mismatch"));
        }
        let raw_path = self.store.directory.join("raw.jsonl");
        #[cfg(not(all(test, windows)))]
        let bytes = {
            reject_symlink(&raw_path)?;
            read(&raw_path)?
        };
        #[cfg(all(test, windows))]
        let bytes = {
            let _ = raw_path;
            self.store.windows()?.read("raw.jsonl")?
        };
        decode_raw_snapshot(state, bytes)
    }
    pub fn append(
        &mut self,
        message: Message,
        starts_turn: bool,
    ) -> io::Result<ConversationSnapshot> {
        let snapshot = self.snapshot()?;
        let prior = snapshot.events.last();
        let sequence = prior.map_or(Ok(1), |event| {
            event
                .sequence
                .checked_add(1)
                .ok_or_else(|| invalid("Event number limit reached"))
        })?;
        let turn_id = if starts_turn {
            prior.map_or(Ok(1), |event| {
                event
                    .turn_id
                    .checked_add(1)
                    .ok_or_else(|| invalid("Turn number limit reached"))
            })?
        } else {
            prior
                .filter(|event| event.epoch == snapshot.state.epoch)
                .ok_or_else(|| invalid("Response precedes user input"))?
                .turn_id
        };
        let event = RawEventV2 {
            schema_version: SCHEMA_VERSION,
            sequence,
            epoch: snapshot.state.epoch,
            turn_id,
            starts_turn,
            message,
        };
        validate_event(&event, prior)?;
        if starts_turn
            && let Some(previous) = prior.filter(|event| event.epoch == snapshot.state.epoch)
        {
            snapshot.validate_complete_turn(previous.turn_id)?;
        }
        let mut line = serde_json::to_vec(&event)?;
        line.push(b'\n');
        let path = self.store.directory.join("raw.jsonl");
        #[cfg(not(all(test, windows)))]
        let mut file = {
            reject_symlink(&path)?;
            OpenOptions::new().append(true).open(path)?
        };
        #[cfg(all(test, windows))]
        let mut file = {
            let _ = path;
            self.store.windows()?.append_file()?
        };
        file.write_all(&line)?;
        fail_point("raw-sync")?;
        file.sync_all()?;
        self.snapshot()
    }
    pub fn save_snapshot(&self, expected: &ConversationSnapshot) -> io::Result<PathBuf> {
        self.check_snapshot(expected)?;
        fail_point("snapshot")?;
        let directory = self.store.directory.join("snapshots");
        reject_symlink(&directory)?;
        let path = directory.join(format!(
            "{}-{}.json",
            expected.state.generation, expected.raw_hash
        ));
        let body = serde_json::to_vec(expected)?;
        #[cfg(all(test, windows))]
        self.store
            .windows()?
            .save_snapshot(path.file_name().ok_or_else(windows_unsupported)?, &body)?;
        #[cfg(not(all(test, windows)))]
        if path.exists() {
            reject_symlink(&path)?;
            if fs::read(&path)? != body {
                return Err(invalid("Different content for the same snapshot ID"));
            }
        } else {
            atomic_write(&path, &body)?;
        }
        Ok(path)
    }
    pub fn check_snapshot(&self, expected: &ConversationSnapshot) -> io::Result<()> {
        let actual = self.snapshot()?;
        if actual.state != expected.state
            || actual.raw_hash != expected.raw_hash
            || actual.raw_offset != expected.raw_offset
            || serde_json::to_vec(&actual.events)? != serde_json::to_vec(&expected.events)?
        {
            return Err(invalid("The session changed during summarization"));
        }
        Ok(())
    }
    /// The caller must hold the memory transaction which checks forgotten-session
    /// tombstones and verifies all visible summary IDs before invoking this method.
    /// All workflow and gate evidence is atomically published with the raw marker.
    pub fn publish(
        &mut self,
        expected: &ConversationSnapshot,
        next: ConversationStateV2,
    ) -> io::Result<()> {
        self.check_snapshot(expected)?;
        if next.project_id != expected.state.project_id
            || next.session_id != expected.state.session_id
            || next.epoch != expected.state.epoch
            || next.generation != expected.state.generation
            || next.history_mode != expected.state.history_mode
        {
            return Err(invalid("Publication generation or ownership mismatch"));
        }
        self.publish_state_marker(expected, next)
    }

    fn publish_state_marker(
        &self,
        expected: &ConversationSnapshot,
        mut next: ConversationStateV2,
    ) -> io::Result<()> {
        #[cfg(all(test, windows))]
        windows_state_admission(&next)?;
        next.generation = next
            .generation
            .checked_add(1)
            .ok_or_else(|| invalid("Storage generation limit reached"))?;
        next.raw_commit_offset = expected.raw_offset;
        next.raw_commit_hash = expected.raw_hash.clone();
        let turns = expected.active_turns();
        next.recent_turn_ids = turns.into_iter().rev().take(REAL_TURN_WINDOW).collect();
        next.recent_turn_ids.reverse();
        if let Some(coverage) = &next.structured_coverage {
            #[cfg(not(all(test, windows)))]
            let bytes = fs::read(self.store.directory.join("raw.jsonl"))?;
            #[cfg(all(test, windows))]
            let bytes = self.store.windows()?.read("raw.jsonl")?;
            validate_coverage_boundary(&bytes, expected, expected.raw_offset, coverage)?;
        }
        next.validate()?;
        // A metadata-only publication leaves the exact raw head and its
        // workspace manifest unchanged. Carry only a binding of the expected
        // committed head; stale bindings and newly appended raw stay stale.
        let carried_binding = self
            .store
            .workspace_head_binding()?
            .filter(|binding| {
                binding.generation == expected.state.generation
                    && binding.raw_commit_offset == expected.state.raw_commit_offset
                    && binding.raw_commit_hash == expected.state.raw_commit_hash
                    && binding.raw_commit_offset == expected.raw_offset
                    && binding.raw_commit_hash == expected.raw_hash
            })
            .map(|mut binding| {
                binding.generation = next.generation;
                binding
            });
        fail_point("marker")?;
        #[cfg(not(all(test, windows)))]
        atomic_write(
            &self.store.directory.join("state.json"),
            &serde_json::to_vec(&next)?,
        )?;
        #[cfg(all(test, windows))]
        self.store
            .windows()?
            .atomic_write("state.json", &serde_json::to_vec(&next)?)?;
        if let Some(binding) = carried_binding {
            atomic_write(
                &self.store.directory.join("workspace-head-binding-v1.json"),
                &serde_json::to_vec(&binding)?,
            )?;
            if self.store.workspace_head_binding()?.as_ref() != Some(&binding) {
                return Err(invalid(
                    "Cannot verify workspace file association after publication",
                ));
            }
        }
        Ok(())
    }

    /// A mode can be initialized only before this conversation has any raw
    /// history, ancestry or rewind boundary. Ordinary publication keeps its
    /// mode invariant, including after clear starts a new empty epoch.
    pub(crate) fn initialize_history_mode(
        &mut self,
        expected: &ConversationSnapshot,
        next: ConversationStateV2,
    ) -> io::Result<()> {
        self.validate_history_initialization(expected)?;
        let mut same_mode = next.clone();
        same_mode.history_mode = expected.state.history_mode;
        if !crate::session_settings::session_only_change(&expected.state, &same_mode) {
            return Err(invalid("Invalid history mode initialization candidate"));
        }
        self.publish_state_marker(expected, next)
    }

    pub(crate) fn validate_history_initialization(
        &self,
        expected: &ConversationSnapshot,
    ) -> io::Result<()> {
        self.check_snapshot(expected)?;
        crate::session_settings::validate_empty_history(expected)?;
        if !self.load_rewind_boundaries()?.is_empty() {
            return Err(invalid(
                "Cannot change the history mode of a conversation with rewind boundaries",
            ));
        }
        Ok(())
    }
    /// Clear starts an empty epoch; raw history is retained for explicit recovery.
    pub fn clear_epoch(&mut self) -> io::Result<()> {
        let snapshot = self.snapshot()?;
        let mut next = snapshot.state.clone();
        next.epoch = next
            .epoch
            .checked_add(1)
            .ok_or_else(|| invalid("Epoch limit reached"))?;
        next.generation = next
            .generation
            .checked_add(1)
            .ok_or_else(|| invalid("Storage generation limit reached"))?;
        next.raw_commit_hash = snapshot.raw_hash;
        next.raw_commit_offset = snapshot.raw_offset;
        next.recent_turn_ids.clear();
        next.visible_summary_ids.clear();
        next.visible_structured_publication_ids.clear();
        next.structured_coverage = None;
        next.ancestors.clear();
        #[cfg(not(all(test, windows)))]
        {
            atomic_write(
                &self.store.directory.join("state.json"),
                &serde_json::to_vec(&next)?,
            )
        }
        #[cfg(all(test, windows))]
        {
            self.store
                .windows()?
                .atomic_write("state.json", &serde_json::to_vec(&next)?)
        }
    }
}

pub(crate) fn validate_coverage_boundary(
    bytes: &[u8],
    expected: &ConversationSnapshot,
    committed_offset: u64,
    coverage: &StructuredCoverage,
) -> io::Result<()> {
    if coverage.epoch != expected.state.epoch
        || committed_offset > expected.raw_offset
        || coverage.raw_offset > committed_offset
    {
        return Err(invalid(
            "Structured memory coverage does not match the saved source boundary",
        ));
    }
    if bytes.len() as u64 != expected.raw_offset || content_hash(bytes) != expected.raw_hash {
        return Err(invalid("Structured memory coverage source mismatch"));
    }
    let covered = usize::try_from(coverage.raw_offset)
        .map_err(|_| invalid("Invalid structured memory coverage range"))?;
    if content_hash(&bytes[..covered]) != coverage.prefix_hash {
        return Err(invalid("Structured memory coverage hash mismatch"));
    }
    let mut offset = 0usize;
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        offset = offset
            .checked_add(line.len())
            .ok_or_else(|| invalid("Source offset limit reached"))?;
        let event: RawEventV2 = serde_json::from_slice(line)?;
        if event.epoch == coverage.epoch
            && event.sequence == coverage.last_source_sequence
            && offset as u64 == coverage.raw_offset
        {
            return Ok(());
        }
    }
    Err(invalid("Structured memory coverage source mismatch"))
}

pub(crate) fn validate_event(event: &RawEventV2, previous: Option<&RawEventV2>) -> io::Result<()> {
    if event.schema_version != SCHEMA_VERSION
        || event.sequence != previous.map_or(1, |event| event.sequence.saturating_add(1))
        || event.turn_id == 0
        || (event.starts_turn && event.message.role != Role::User)
        || previous.is_none() && !event.starts_turn
    {
        return Err(invalid("Invalid source event format or order"));
    }
    if let Some(previous) = previous
        && (event.epoch < previous.epoch
            || (event.epoch != previous.epoch && !event.starts_turn)
            || (event.starts_turn && event.turn_id != previous.turn_id.saturating_add(1))
            || (!event.starts_turn && event.turn_id != previous.turn_id))
    {
        return Err(invalid("Invalid original turn boundary"));
    }
    Ok(())
}
pub(crate) fn valid_session_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, byte)| {
            if [8, 13, 18, 23].contains(&i) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}
fn validate_opaque_id(id: &str, name: &str) -> io::Result<()> {
    if id.is_empty() || id.len() > 256 || id.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(invalid(&format!("Invalid {name}")));
    }
    Ok(())
}
fn validate_memory_raw_object_ids(ids: &[String]) -> io::Result<()> {
    if ids.iter().any(|id| !valid_hash(id))
        || ids.iter().collect::<BTreeSet<_>>().len() != ids.len()
    {
        return Err(invalid("Invalid tool-memory raw object allowlist"));
    }
    Ok(())
}
fn validate_rewind_completion_input(
    eligibility: &RewindEligibilityV1,
    effect: &RewindEffectStatusV1,
) -> io::Result<()> {
    let detail = match effect {
        RewindEffectStatusV1::NoExternalMutation => None,
        RewindEffectStatusV1::ExternalMutation { description }
        | RewindEffectStatusV1::Unknown { description } => Some(description),
    };
    if detail.is_some_and(|value| value.is_empty() || value.len() > 1024)
        || matches!(eligibility, RewindEligibilityV1::Ineligible { reason } if reason.is_empty() || reason.len() > 1024)
        || (matches!(eligibility, RewindEligibilityV1::Eligible) && detail.is_some())
    {
        return Err(invalid(
            "Invalid rewind boundary eligibility or effect state",
        ));
    }
    Ok(())
}
fn validate_rewind_reason(reason: &str) -> io::Result<()> {
    if reason.is_empty()
        || reason.len() > 1024
        || reason.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(invalid("Invalid rewind boundary reason"));
    }
    Ok(())
}
fn validate_rewind_boundary(boundary: &RewindBoundaryV1) -> io::Result<()> {
    if boundary.schema_version != REWIND_BOUNDARY_SCHEMA_VERSION
        || !valid_session_id(&boundary.session_id)
        || boundary.project_id.is_empty()
        || boundary.epoch != boundary.source_state.epoch
        || boundary.source_generation != boundary.source_state.generation
        || boundary.session_id != boundary.source_state.session_id
        || boundary.project_id != boundary.source_state.project_id
        || boundary.raw_offset != boundary.source_state.raw_commit_offset
        || boundary.raw_hash != boundary.source_state.raw_commit_hash
        || !valid_hash(&boundary.raw_hash)
        || boundary.target_turn_id == 0
        || boundary.created_at_unix_ms == 0
        || boundary
            .user_message_preview
            .as_ref()
            .is_some_and(|preview| preview.is_empty() || preview.chars().count() > 512)
        || (boundary.completion.is_some() && boundary.user_message_preview.is_none())
        || (boundary.completion.is_some() && boundary.abandoned_reason.is_some())
    {
        return Err(invalid("Invalid saved rewind boundary content"));
    }
    validate_opaque_id(&boundary.boundary_id, "rewind boundary ID")?;
    validate_opaque_id(
        &boundary.workspace_pre_manifest_id,
        "workspace pre manifest ID",
    )?;
    validate_memory_raw_object_ids(&boundary.visible_memory_raw_object_ids)?;
    boundary.source_state.validate()?;
    if let Some(completion) = &boundary.completion {
        validate_opaque_id(
            &completion.workspace_post_manifest_id,
            "workspace post manifest ID",
        )?;
        validate_rewind_completion_input(&completion.eligibility, &completion.effect)?;
        if completion.completed_generation < boundary.source_generation
            || completion.completed_raw_offset < boundary.raw_offset
            || !valid_hash(&completion.completed_raw_hash)
        {
            return Err(invalid("Invalid rewind boundary completion marker"));
        }
    }
    if let Some(reason) = &boundary.abandoned_reason {
        validate_rewind_reason(reason)?;
    }
    Ok(())
}
fn validate_rewind_boundary_for_branch(
    boundary: &RewindBoundaryV1,
    current: &ConversationSnapshot,
) -> io::Result<()> {
    validate_rewind_boundary(boundary)?;
    let completion = boundary
        .completion
        .as_ref()
        .ok_or_else(|| invalid("The rewind boundary is incomplete"))?;
    if !matches!(completion.eligibility, RewindEligibilityV1::Eligible)
        || boundary.project_id != current.state.project_id
        || boundary.session_id != current.state.session_id
        || boundary.epoch != current.state.epoch
        || boundary.raw_offset > current.raw_offset
    {
        return Err(invalid(
            "The rewind boundary does not apply to the current session",
        ));
    }
    Ok(())
}

/// Decoded observations only; these functions grant no filesystem or migration authority.
pub(crate) fn decode_saved_snapshot(
    bytes: &[u8],
    project_id: &str,
    session_id: &str,
    generation: u64,
    raw_hash: &str,
) -> io::Result<ConversationSnapshot> {
    if !valid_hash(raw_hash) {
        return Err(invalid("Invalid source snapshot hash"));
    }
    let snapshot: ConversationSnapshot = serde_json::from_slice(bytes)?;
    if snapshot.state.project_id != project_id
        || snapshot.state.session_id != session_id
        || snapshot.state.generation != generation
        || snapshot.raw_hash != raw_hash
        || snapshot.raw_offset > u64::try_from(usize::MAX).unwrap_or(u64::MAX)
    {
        return Err(invalid(
            "Source snapshot ownership, generation, or hash mismatch",
        ));
    }
    let mut raw = Vec::new();
    for event in &snapshot.events {
        serde_json::to_writer(&mut raw, event)?;
        raw.push(b'\n');
    }
    if raw.len() as u64 != snapshot.raw_offset || content_hash(&raw) != snapshot.raw_hash {
        return Err(invalid("Source snapshot body hash or length mismatch"));
    }
    snapshot.state.validate()?;
    for (index, event) in snapshot.events.iter().enumerate() {
        validate_event(
            event,
            index.checked_sub(1).and_then(|i| snapshot.events.get(i)),
        )?;
        if event.epoch > snapshot.state.epoch {
            return Err(invalid("Invalid source snapshot epoch"));
        }
    }
    if let Some(coverage) = &snapshot.state.structured_coverage {
        validate_coverage_boundary(&raw, &snapshot, snapshot.state.raw_commit_offset, coverage)?;
    }
    Ok(snapshot)
}
pub(crate) fn decode_raw_snapshot(
    state: ConversationStateV2,
    bytes: Vec<u8>,
) -> io::Result<ConversationSnapshot> {
    state.validate()?;
    let committed =
        usize::try_from(state.raw_commit_offset).map_err(|_| invalid("Invalid source range"))?;
    if committed > bytes.len()
        || content_hash(&bytes[..committed]) != state.raw_commit_hash
        || (committed > 0 && bytes[committed - 1] != b'\n')
    {
        return Err(invalid("Saved source hash mismatch"));
    }
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        return Err(invalid(
            "The source tail is incomplete. Stopped with the original text retained",
        ));
    }
    let mut events = Vec::<RawEventV2>::new();
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let event: RawEventV2 = serde_json::from_slice(line)?;
        validate_event(&event, events.last())?;
        if event.epoch > state.epoch {
            return Err(invalid("Source text exists for an unpublished epoch"));
        }
        events.push(event);
    }
    let snapshot = ConversationSnapshot {
        state,
        raw_hash: content_hash(&bytes),
        raw_offset: bytes.len() as u64,
        events,
    };
    snapshot.validate_admission_receipts()?;
    if let Some(coverage) = &snapshot.state.structured_coverage {
        validate_coverage_boundary(
            &bytes,
            &snapshot,
            snapshot.state.raw_commit_offset,
            coverage,
        )?;
    }
    Ok(snapshot)
}
/// Migration also checks receipts and the committed prefix of historical snapshots.
#[cfg(test)]
pub(crate) fn validate_migration_snapshot(snapshot: &ConversationSnapshot) -> io::Result<()> {
    snapshot.validate_admission_receipts()?;
    let mut raw = Vec::new();
    for event in &snapshot.events {
        serde_json::to_writer(&mut raw, event)?;
        raw.push(b'\n');
    }
    let committed = usize::try_from(snapshot.state.raw_commit_offset)
        .map_err(|_| invalid("Invalid source range"))?;
    if committed > raw.len()
        || content_hash(&raw[..committed]) != snapshot.state.raw_commit_hash
        || (committed > 0 && raw[committed - 1] != b"\n"[0])
    {
        return Err(invalid("Saved source hash mismatch"));
    }
    Ok(())
}

pub(crate) fn decode_rewind_boundaries(bytes: &[u8]) -> io::Result<Vec<RewindBoundaryV1>> {
    let boundaries: Vec<RewindBoundaryV1> = serde_json::from_slice(bytes)?;
    let mut ids = BTreeSet::new();
    let mut previous: Option<&RewindBoundaryV1> = None;
    for boundary in &boundaries {
        validate_rewind_boundary(boundary)?;
        if !ids.insert(&boundary.boundary_id) {
            return Err(invalid("Duplicate rewind boundary IDs"));
        }
        if let Some(previous) = previous
            && previous.project_id == boundary.project_id
            && previous.session_id == boundary.session_id
            && previous.source_generation > boundary.source_generation
        {
            return Err(invalid("Invalid rewind boundary order"));
        }
        previous = Some(boundary);
    }
    Ok(boundaries)
}

pub(crate) fn validate_rewind_branch_metadata(metadata: &RewindBranchMetadataV1) -> io::Result<()> {
    if metadata.schema_version != REWIND_BOUNDARY_SCHEMA_VERSION
        || !valid_session_id(&metadata.origin_session_id)
        || !valid_hash(&metadata.branch_initial_raw_hash)
        || (metadata.branch_initial_raw_offset == 0
            && metadata.branch_initial_raw_hash != content_hash(&[]))
    {
        return Err(invalid("Invalid rewind branch metadata"));
    }
    validate_opaque_id(&metadata.boundary_id, "rewind boundary ID")?;
    validate_opaque_id(
        &metadata.workspace_pre_manifest_id,
        "workspace pre manifest ID",
    )?;
    validate_opaque_id(
        &metadata.workspace_post_manifest_id,
        "workspace post manifest ID",
    )?;
    validate_memory_raw_object_ids(&metadata.visible_memory_raw_object_ids)
}
pub(crate) fn validate_workspace_head_binding(binding: &WorkspaceHeadBindingV1) -> io::Result<()> {
    if binding.schema_version != 1
        || !valid_session_id(&binding.session_id)
        || binding.project_id.is_empty()
        || !valid_hash(&binding.raw_commit_hash)
        || (binding.raw_commit_offset == 0 && binding.raw_commit_hash != content_hash(&[]))
    {
        return Err(invalid("Invalid workspace file binding record"));
    }
    validate_opaque_id(&binding.workspace_manifest_id, "workspace manifest ID")
}
fn write_rewind_branch_metadata_create_new(
    path: &Path,
    metadata: &RewindBranchMetadataV1,
) -> io::Result<()> {
    validate_rewind_branch_metadata(metadata)?;
    let metadata_path = path.join("rewind-branch-v1.json");
    let mut file = private_file(&metadata_path, true)?;
    file.write_all(&serde_json::to_vec(metadata)?)?;
    file.sync_all()?;
    File::open(path)?.sync_all()
}

fn rewind_branch_candidate_plan(
    source: &ConversationSnapshot,
    boundary: &RewindBoundaryV1,
    child_id: &str,
) -> io::Result<(ConversationSnapshot, RewindBranchMetadataV1)> {
    validate_rewind_boundary_for_candidate(boundary, source)?;
    let selected = raw_prefix_events(&source.events, boundary.raw_offset, &boundary.raw_hash)?;
    let completion = boundary
        .completion
        .as_ref()
        .ok_or_else(|| invalid("The rewind boundary is incomplete"))?;
    let mut state = ConversationStateV2::new(boundary.project_id.clone(), child_id.into())?;
    state.history_mode = boundary.source_state.history_mode;
    state.session_settings = boundary.source_state.session_settings.clone();
    state.goal = boundary.source_state.goal.clone();
    state.workflow_config = boundary.source_state.workflow_config.clone();
    state.workflow = boundary.source_state.workflow.clone();
    state.gates = boundary.source_state.gates.clone();
    state.workflow_artifacts = boundary.source_state.workflow_artifacts.clone();
    state.verification_checkpoint = boundary.source_state.verification_checkpoint.clone();
    state.admission_receipts = boundary.source_state.admission_receipts.clone();
    state.ancestors = boundary.source_state.ancestors.clone();
    state.ancestors.push(AncestorRange {
        session_id: boundary.session_id.clone(),
        epoch: boundary.epoch,
        generation: boundary.source_generation,
        visible_summary_ids: boundary.source_state.visible_summary_ids.clone(),
        visible_structured_publication_ids: boundary
            .source_state
            .visible_structured_publication_ids
            .clone(),
    });
    let expected = expected_rewind_snapshot(state, selected)?;
    let metadata = RewindBranchMetadataV1 {
        schema_version: REWIND_BOUNDARY_SCHEMA_VERSION,
        boundary_id: boundary.boundary_id.clone(),
        origin_session_id: boundary.session_id.clone(),
        origin_generation: boundary.source_generation,
        workspace_pre_manifest_id: boundary.workspace_pre_manifest_id.clone(),
        workspace_post_manifest_id: completion.workspace_post_manifest_id.clone(),
        branch_initial_raw_offset: expected.raw_offset,
        branch_initial_raw_hash: expected.raw_hash.clone(),
        visible_memory_raw_object_ids: boundary.visible_memory_raw_object_ids.clone(),
    };
    validate_rewind_branch_metadata(&metadata)?;
    Ok((expected, metadata))
}

/// Candidate provenance remains readable after the source starts a later
/// epoch. Selection is stricter (`validate_rewind_boundary_for_branch`), but
/// reopening an already-created child binds to its immutable source prefix.
fn validate_rewind_boundary_for_candidate(
    boundary: &RewindBoundaryV1,
    source: &ConversationSnapshot,
) -> io::Result<()> {
    validate_rewind_boundary(boundary)?;
    let completion = boundary
        .completion
        .as_ref()
        .ok_or_else(|| invalid("The rewind boundary is incomplete"))?;
    if !matches!(completion.eligibility, RewindEligibilityV1::Eligible)
        || !matches!(completion.effect, RewindEffectStatusV1::NoExternalMutation)
        || boundary.project_id != source.state.project_id
        || boundary.session_id != source.state.session_id
        || boundary.source_generation != boundary.source_state.generation
        || boundary.raw_offset > source.raw_offset
    {
        return Err(invalid("Invalid rewind candidate source boundary"));
    }
    Ok(())
}

fn expected_rewind_snapshot(
    mut state: ConversationStateV2,
    selected: Vec<RawEventV2>,
) -> io::Result<ConversationSnapshot> {
    let mut events = Vec::with_capacity(selected.len());
    let mut raw = Vec::new();
    let mut turn_id = 0_u64;
    for (index, source_event) in selected.into_iter().enumerate() {
        if source_event.starts_turn {
            turn_id = turn_id
                .checked_add(1)
                .ok_or_else(|| invalid("Fork turn number limit reached"))?;
        }
        let event = RawEventV2 {
            schema_version: SCHEMA_VERSION,
            sequence: u64::try_from(index + 1)
                .map_err(|_| invalid("Fork event number limit reached"))?,
            epoch: 0,
            turn_id,
            starts_turn: source_event.starts_turn,
            message: source_event.message,
        };
        validate_event(&event, events.last())?;
        serde_json::to_writer(&mut raw, &event)?;
        raw.push(b'\n');
        events.push(event);
    }
    state.generation = 1;
    state.raw_commit_offset =
        u64::try_from(raw.len()).map_err(|_| invalid("Fork source length limit reached"))?;
    state.raw_commit_hash = content_hash(&raw);
    state.recent_turn_ids = events
        .iter()
        .filter(|event| event.starts_turn)
        .map(|event| event.turn_id)
        .rev()
        .take(REAL_TURN_WINDOW)
        .collect();
    state.recent_turn_ids.reverse();
    state.validate()?;
    Ok(ConversationSnapshot {
        raw_hash: state.raw_commit_hash.clone(),
        raw_offset: state.raw_commit_offset,
        state,
        events,
    })
}
fn raw_prefix_events(
    events: &[RawEventV2],
    expected_offset: u64,
    expected_hash: &str,
) -> io::Result<Vec<RawEventV2>> {
    if expected_offset == 0 {
        if expected_hash == content_hash(&[]) {
            return Ok(Vec::new());
        }
        return Err(invalid("Rewind source prefix hash mismatch"));
    }
    let mut raw = Vec::new();
    let mut prefix = Vec::new();
    for event in events {
        serde_json::to_writer(&mut raw, event)?;
        raw.push(b'\n');
        if raw.len() as u64 > expected_offset {
            return Err(invalid(
                "The rewind source prefix is not on an event boundary",
            ));
        }
        prefix.push(event.clone());
        if raw.len() as u64 == expected_offset {
            break;
        }
    }
    if raw.len() as u64 != expected_offset || content_hash(&raw) != expected_hash {
        return Err(invalid("Rewind source prefix hash mismatch"));
    }
    Ok(prefix)
}
fn new_rewind_boundary_id() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
    Ok(format!(
        "rewind-{}",
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
}
fn rewind_boundary_timestamp() -> io::Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| invalid("Cannot retrieve the rewind boundary timestamp"))
        .and_then(|duration| {
            u64::try_from(duration.as_millis())
                .map_err(|_| invalid("Rewind boundary timestamp limit reached"))
        })
}
fn rewind_message_preview(content: &str) -> String {
    let preview: String = content.chars().take(512).collect();
    if preview.is_empty() {
        "(empty user message)".into()
    } else {
        preview
    }
}
fn valid_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}
fn fork_plan(
    source: &ConversationSnapshot,
    child_id: &str,
    artifact_hash: &str,
) -> io::Result<(ConversationStateV2, Vec<(Message, bool)>)> {
    if source.raw_hash != source.state.raw_commit_hash
        || source.raw_offset != source.state.raw_commit_offset
    {
        return Err(invalid("The fork source marker is not finalized"));
    }
    let turns = source.active_turns();
    let first = turns.get(turns.len().saturating_sub(REAL_TURN_WINDOW));
    for turn in turns.iter().rev().take(REAL_TURN_WINDOW) {
        source.validate_complete_turn(*turn)?;
    }
    let mut initial = ConversationStateV2::new(source.state.project_id.clone(), child_id.into())?;
    initial.history_mode = source.state.history_mode;
    initial.session_settings = source.state.session_settings.clone();
    initial.workflow_config = source.state.workflow_config.clone();
    initial.workflow = source.state.workflow.clone();
    initial.gates = source.state.gates.for_fork(artifact_hash);
    initial.workflow_artifacts = source.state.workflow_artifacts.clone();
    initial.ancestors = source.state.ancestors.clone();
    initial.ancestors.push(AncestorRange {
        session_id: source.state.session_id.clone(),
        epoch: source.state.epoch,
        generation: source.state.generation,
        visible_summary_ids: source.state.visible_summary_ids.clone(),
        visible_structured_publication_ids: source.state.visible_structured_publication_ids.clone(),
    });
    let selected = source
        .events
        .iter()
        .filter(|event| {
            event.epoch == source.state.epoch && first.is_some_and(|first| event.turn_id >= *first)
        })
        .map(|event| (event.message.clone(), event.starts_turn))
        .collect();
    Ok((initial, selected))
}

/// Exact post-publication state and raw hash expected from the fork writer.
/// This is computed before its management effect starts and checked on reopen.
pub(crate) fn expected_fork_snapshot(
    source: &ConversationSnapshot,
    child_id: &str,
    artifact_hash: &str,
) -> io::Result<ConversationSnapshot> {
    let (mut state, selected) = fork_plan(source, child_id, artifact_hash)?;
    let mut events = Vec::with_capacity(selected.len());
    let mut raw = Vec::new();
    let mut turn_id = 0_u64;
    for (index, (message, starts_turn)) in selected.into_iter().enumerate() {
        if starts_turn {
            turn_id = turn_id
                .checked_add(1)
                .ok_or_else(|| invalid("Fork turn number limit reached"))?;
        }
        let event = RawEventV2 {
            schema_version: SCHEMA_VERSION,
            sequence: u64::try_from(index + 1)
                .map_err(|_| invalid("Fork event number limit reached"))?,
            epoch: 0,
            turn_id,
            starts_turn,
            message,
        };
        validate_event(&event, events.last())?;
        raw.extend_from_slice(&serde_json::to_vec(&event)?);
        raw.push(b'\n');
        events.push(event);
    }
    let raw_hash = content_hash(&raw);
    state.generation = 1;
    state.raw_commit_offset =
        u64::try_from(raw.len()).map_err(|_| invalid("Fork source length limit reached"))?;
    state.raw_commit_hash = raw_hash.clone();
    state.recent_turn_ids = events
        .iter()
        .filter(|event| event.starts_turn)
        .map(|event| event.turn_id)
        .collect();
    state.validate()?;
    Ok(ConversationSnapshot {
        state,
        raw_hash,
        raw_offset: raw.len() as u64,
        events,
    })
}

pub fn content_hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
#[cfg(test)]
thread_local! { pub(crate) static FAIL_POINT: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) }; }
fn fail_point(_point: &str) -> io::Result<()> {
    #[cfg(test)]
    if FAIL_POINT.with(|point| point.get() == Some(_point)) {
        return Err(io::Error::other("injected persistence failure"));
    }
    Ok(())
}
fn reject_symlink(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(invalid("Symlinks within sessions are not allowed"));
    }
    Ok(())
}
#[cfg(unix)]
fn private_directory(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

#[cfg(not(unix))]
fn private_directory(_path: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Cannot verify conversation storage protections on this platform",
    ))
}
#[cfg(not(all(test, windows)))]
fn private_file(path: &Path, create: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(create);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}
#[cfg(all(test, windows))]
fn private_file(_path: &Path, _create: bool) -> io::Result<File> {
    Err(windows_unsupported())
}
fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .ok_or_else(|| invalid("Invalid storage location"))?;
    let temporary = parent.join(format!(
        ".pending-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = private_file(&temporary, true)?;
    // On failure leave the private temporary file for recovery, never remove raw.
    file.write_all(bytes)?;
    fail_point("atomic_file_sync")?;
    file.sync_all()?;
    fail_point("atomic_rename")?;
    fs::rename(&temporary, path)?;
    fail_point("atomic_directory_sync")?;
    File::open(parent)?.sync_all()
}

#[cfg(all(test, windows))]
fn windows_unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "This Windows storage path lacks protection support",
    )
}
#[cfg(all(test, windows))]
fn windows_error(error: polaris_auth::AuthError) -> io::Error {
    match error {
        polaris_auth::AuthError::Io(error) => error,
        _ => windows_unsupported(),
    }
}
#[cfg(all(test, windows))]
pub(crate) fn windows_state_admission(state: &ConversationStateV2) -> io::Result<()> {
    // Structured receipts and workflow/workspace owners retain their refusal.
    if state.history_mode == HistoryMode::Structured10
        || state.workflow_config.is_some()
        || state.structured_coverage.is_some()
        || !state.visible_structured_publication_ids.is_empty()
    {
        return Err(windows_unsupported());
    }
    Ok(())
}
#[cfg(all(test, windows))]
#[derive(Clone, Debug)]
struct WindowsConversationStore {
    directory: std::sync::Arc<polaris_auth::protection::WindowsFamilyDirectory>,
    snapshots: std::sync::Arc<polaris_auth::protection::WindowsFamilyDirectory>,
}
#[cfg(all(test, windows))]
impl WindowsConversationStore {
    fn root(path: &Path) -> io::Result<polaris_auth::protection::WindowsFamilyDirectory> {
        polaris_auth::protection::WindowsFamilyDirectory::open_existing(path)
            .map_err(|_| windows_unsupported())
    }
    fn child(
        root: &polaris_auth::protection::WindowsFamilyDirectory,
        name: &str,
    ) -> io::Result<polaris_auth::protection::WindowsFamilyDirectory> {
        let name = std::ffi::OsStr::new(name);
        match root.child(name, false) {
            Ok(value) => Ok(value),
            Err(polaris_auth::AuthError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                match root.child(name, true) {
                    Ok(value) => Ok(value),
                    Err(polaris_auth::AuthError::Io(error))
                        if error.kind() == io::ErrorKind::AlreadyExists =>
                    {
                        root.child(name, false).map_err(windows_error)
                    }
                    Err(error) => Err(windows_error(error)),
                }
            }
            Err(error) => Err(windows_error(error)),
        }
    }
    fn create(data_root: &Path, id: &str) -> io::Result<Self> {
        let root = Self::root(data_root)?;
        let sessions = Self::child(&root, "sessions-v2")?;
        root.sync_all().map_err(windows_error)?;
        let directory = sessions
            .child(std::ffi::OsStr::new(id), true)
            .map_err(windows_error)?;
        let snapshots = directory
            .child(std::ffi::OsStr::new("snapshots"), true)
            .map_err(windows_error)?;
        for name in ["raw.jsonl", "writer.lock"] {
            directory
                .open_file(std::ffi::OsStr::new(name), true, true)
                .map_err(windows_error)?
                .sync_all()?;
        }
        snapshots.sync_all().map_err(windows_error)?;
        directory.sync_all().map_err(windows_error)?;
        sessions.sync_all().map_err(windows_error)?;
        Ok(Self {
            directory: std::sync::Arc::new(directory),
            snapshots: std::sync::Arc::new(snapshots),
        })
    }
    fn open(data_root: &Path, id: &str) -> io::Result<Self> {
        let root = Self::root(data_root)?;
        let sessions = root
            .child(std::ffi::OsStr::new("sessions-v2"), false)
            .map_err(windows_error)?;
        let directory = sessions
            .child(std::ffi::OsStr::new(id), false)
            .map_err(windows_error)?;
        let snapshots = directory
            .child(std::ffi::OsStr::new("snapshots"), false)
            .map_err(windows_error)?;
        Ok(Self {
            directory: std::sync::Arc::new(directory),
            snapshots: std::sync::Arc::new(snapshots),
        })
    }
    fn read(&self, name: &str) -> io::Result<Vec<u8>> {
        Self::read_at(&self.directory, std::ffi::OsStr::new(name))
    }
    fn read_at(
        dir: &polaris_auth::protection::WindowsFamilyDirectory,
        name: &std::ffi::OsStr,
    ) -> io::Result<Vec<u8>> {
        let mut file = dir.open_file(name, false, false).map_err(windows_error)?;
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut bytes)?;
        Ok(bytes)
    }
    fn read_snapshot(&self, name: &std::ffi::OsStr) -> io::Result<Vec<u8>> {
        Self::read_at(&self.snapshots, name)
    }
    fn require_absent(&self, name: &str) -> io::Result<()> {
        match self
            .directory
            .open_file(std::ffi::OsStr::new(name), false, false)
        {
            Err(polaris_auth::AuthError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                Ok(())
            }
            _ => Err(windows_unsupported()),
        }
    }
    fn append_file(&self) -> io::Result<polaris_auth::protection::WindowsFamilyFile> {
        let mut file = self
            .directory
            .open_file(std::ffi::OsStr::new("raw.jsonl"), true, false)
            .map_err(windows_error)?;
        std::io::Seek::seek(&mut file, std::io::SeekFrom::End(0))?;
        Ok(file)
    }
    fn temporary() -> std::ffi::OsString {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        format!(
            ".pending-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )
        .into()
    }
    fn atomic_write(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        let temporary = Self::temporary();
        let mut file = self
            .directory
            .open_file(&temporary, true, true)
            .map_err(windows_error)?;
        file.write_all(bytes)?;
        fail_point("atomic_file_sync")?;
        file.sync_all()?;
        drop(file);
        fail_point("atomic_rename")?;
        self.directory
            .replace(&temporary, std::ffi::OsStr::new(name))
            .map_err(windows_error)?;
        fail_point("atomic_directory_sync")?;
        self.directory.sync_all().map_err(windows_error)
    }
    fn save_snapshot(&self, name: &std::ffi::OsStr, body: &[u8]) -> io::Result<()> {
        match Self::read_at(&self.snapshots, name) {
            Ok(existing) if existing == body => return Ok(()),
            Ok(_) => return Err(invalid("Different content for the same snapshot ID")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let temporary = Self::temporary();
        let mut file = self
            .snapshots
            .open_file(&temporary, true, true)
            .map_err(windows_error)?;
        file.write_all(body)?;
        fail_point("atomic_file_sync")?;
        file.sync_all()?;
        drop(file);
        fail_point("atomic_rename")?;
        if !self
            .snapshots
            .link_no_replace(&temporary, name)
            .map_err(windows_error)?
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "The snapshot publication destination already exists",
            ));
        }
        self.snapshots
            .unlink_file(&temporary)
            .map_err(windows_error)?;
        fail_point("atomic_directory_sync")?;
        self.snapshots.sync_all().map_err(windows_error)?;
        if Self::read_at(&self.snapshots, name)? != body {
            return Err(invalid("Snapshot content differs after publication"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const ID: &str = "00000000-0000-4000-8000-000000000001";
    #[cfg(unix)]
    fn store(root: &Path) -> ConversationStore {
        store_with_mode(root, HistoryMode::Legacy)
    }
    #[cfg(unix)]
    fn store_with_mode(root: &Path, history_mode: HistoryMode) -> ConversationStore {
        let mut state = ConversationStateV2::new("project".into(), ID.into()).unwrap();
        state.history_mode = history_mode;
        ConversationStore::create(root, state).unwrap()
    }
    #[cfg(unix)]
    #[test]
    fn workspace_head_binding_is_durable_and_refuses_a_different_tree() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let snapshot = store.lock().unwrap().snapshot().unwrap();
        let first = "a".repeat(32);
        store.bind_workspace_head(&snapshot, &first).unwrap();
        assert_eq!(
            store
                .workspace_head_binding()
                .unwrap()
                .unwrap()
                .workspace_manifest_id,
            first
        );
        let reopened = ConversationStore::open(dir.path(), "project", ID).unwrap();
        assert_eq!(
            reopened
                .workspace_head_binding()
                .unwrap()
                .unwrap()
                .raw_commit_hash,
            snapshot.raw_hash
        );
        assert!(store.bind_workspace_head(&snapshot, &first).is_ok());
        assert!(
            store
                .bind_workspace_head(&snapshot, &"b".repeat(32))
                .is_err()
        );
        let mut lock = store.lock().unwrap();
        let before = lock.snapshot().unwrap();
        lock.publish(&before, before.state.clone()).unwrap();
        let after = lock.snapshot().unwrap();
        let carried = store.workspace_head_binding().unwrap().unwrap();
        assert_eq!(carried.generation, after.state.generation);
        assert_eq!(carried.workspace_manifest_id, first);
        assert_eq!(carried.raw_commit_hash, snapshot.raw_hash);
        let appended = lock.append(Message::user("new raw head"), true).unwrap();
        lock.publish(&appended, appended.state.clone()).unwrap();
        assert_eq!(store.workspace_head_binding().unwrap().unwrap(), carried);
        assert_ne!(
            carried.generation,
            lock.snapshot().unwrap().state.generation
        );
    }
    #[cfg(unix)]
    #[test]
    fn roundtrip_publishes_gates_and_detects_stale_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let old = {
            let mut lock = store.lock().unwrap();
            let snapshot = lock.append(Message::user("question"), true).unwrap();
            lock.save_snapshot(&snapshot).unwrap();
            let mut next = snapshot.state.clone();
            next.workflow.phase = crate::workflow::Phase::Implement;
            next.gates.approved_spec_hash = Some("spec".into());
            lock.publish(&snapshot, next).unwrap();
            snapshot
        };
        let resumed = ConversationStore::open(dir.path(), "project", ID).unwrap();
        let mut lock = resumed.lock().unwrap();
        let snapshot = lock.snapshot().unwrap();
        assert_eq!(snapshot.state.generation, 1);
        assert_eq!(
            snapshot.state.gates.approved_spec_hash.as_deref(),
            Some("spec")
        );
        assert_eq!(
            snapshot.state.workflow.phase,
            crate::workflow::Phase::Implement
        );
        assert!(lock.publish(&old, old.state.clone()).is_err());
        assert!(!dir.path().join("sessions").exists());
    }
    #[cfg(unix)]
    #[test]
    fn structured_marker_is_opt_in_and_fork_freezes_exact_v3_publication_ids() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with_mode(dir.path(), HistoryMode::Structured10);
        let mut lock = store.lock().unwrap();
        let snapshot = lock.append(Message::user("question"), true).unwrap();
        let mut next = snapshot.state.clone();
        next.visible_structured_publication_ids = vec!["publication-v3".into()];
        next.structured_coverage = Some(StructuredCoverage {
            epoch: 0,
            last_source_sequence: 1,
            raw_offset: snapshot.raw_offset,
            prefix_hash: snapshot.raw_hash.clone(),
        });
        lock.publish(&snapshot, next).unwrap();
        let child = lock
            .fork(
                dir.path(),
                "00000000-0000-4000-8000-000000000002",
                "artifact",
            )
            .unwrap();
        let child_state = child.lock().unwrap().snapshot().unwrap().state;
        assert!(child_state.visible_structured_publication_ids.is_empty());
        assert_eq!(
            child_state
                .ancestors
                .last()
                .unwrap()
                .visible_structured_publication_ids,
            ["publication-v3"]
        );
        assert!(child_state.structured_coverage.is_none());
    }
    #[cfg(unix)]
    #[test]
    fn coverage_requires_a_committed_raw_boundary_and_mode_never_changes() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with_mode(dir.path(), HistoryMode::Structured10);
        let raw_path = store.directory().join("raw.jsonl");
        let mut lock = store.lock().unwrap();
        let snapshot = lock.append(Message::user("first"), true).unwrap();
        let mut next = snapshot.state.clone();
        next.visible_structured_publication_ids = vec!["publication-v3".into()];
        next.structured_coverage = Some(StructuredCoverage {
            epoch: snapshot.state.epoch,
            last_source_sequence: snapshot.events.last().unwrap().sequence,
            raw_offset: snapshot.raw_offset,
            prefix_hash: snapshot.raw_hash.clone(),
        });
        lock.publish(&snapshot, next).unwrap();
        let expected = lock.append(Message::user("second"), true).unwrap();
        let raw_before = fs::read(&raw_path).unwrap();
        let state_before = expected.state.clone();
        for change in [0, 1, 2, 3] {
            let mut invalid = expected.state.clone();
            invalid.visible_structured_publication_ids = vec!["publication-v3".into()];
            invalid.structured_coverage = Some(StructuredCoverage {
                epoch: expected.state.epoch,
                last_source_sequence: expected.events.last().unwrap().sequence,
                raw_offset: expected.raw_offset,
                prefix_hash: expected.raw_hash.clone(),
            });
            match change {
                0 => invalid.structured_coverage.as_mut().unwrap().raw_offset -= 1,
                1 => {
                    invalid.structured_coverage.as_mut().unwrap().prefix_hash =
                        content_hash(b"forged")
                }
                2 => {
                    invalid
                        .structured_coverage
                        .as_mut()
                        .unwrap()
                        .last_source_sequence += 1
                }
                3 => invalid.history_mode = HistoryMode::Strict10,
                _ => unreachable!(),
            }
            assert!(lock.publish(&expected, invalid).is_err(), "tamper {change}");
            assert_eq!(fs::read(&raw_path).unwrap(), raw_before);
            assert_eq!(lock.snapshot().unwrap().state, state_before);
        }
        drop(lock);
        let state_path = store.directory().join("state.json");
        let mut persisted: ConversationStateV2 =
            serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
        let coverage = persisted.structured_coverage.as_mut().unwrap();
        coverage.last_source_sequence = expected.events.last().unwrap().sequence;
        coverage.raw_offset = expected.raw_offset;
        coverage.prefix_hash = expected.raw_hash.clone();
        fs::write(&state_path, serde_json::to_vec(&persisted).unwrap()).unwrap();
        assert!(store.lock().unwrap().snapshot().is_err());
        assert_eq!(fs::read(&raw_path).unwrap(), raw_before);
    }
    #[test]
    fn legacy_and_strict_markers_reject_structured_metadata() {
        for mode in [HistoryMode::Legacy, HistoryMode::Strict10] {
            let mut state = ConversationStateV2::new("project".into(), ID.into()).unwrap();
            state.history_mode = mode;
            assert!(state.validate().is_ok());
            state
                .visible_structured_publication_ids
                .push("publication-v3".into());
            assert!(state.validate().is_err());
            state.visible_structured_publication_ids.clear();
            state.ancestors.push(AncestorRange {
                session_id: "00000000-0000-4000-8000-000000000002".into(),
                epoch: 0,
                generation: 0,
                visible_summary_ids: vec![],
                visible_structured_publication_ids: vec!["publication-v3".into()],
            });
            assert!(state.validate().is_err());
        }
    }
    #[cfg(unix)]
    #[test]
    fn new_structured_sessions_cannot_pregrant_publications_or_coverage() {
        let dir = tempfile::tempdir().unwrap();
        let mut valid = ConversationStateV2::new("project".into(), ID.into()).unwrap();
        valid.history_mode = HistoryMode::Structured10;
        assert!(ConversationStore::create(dir.path(), valid).is_ok());
        for prefilled in [true, false] {
            let id = if prefilled {
                "00000000-0000-4000-8000-000000000003"
            } else {
                "00000000-0000-4000-8000-000000000004"
            };
            let mut state = ConversationStateV2::new("project".into(), id.into()).unwrap();
            state.history_mode = HistoryMode::Structured10;
            if prefilled {
                state
                    .visible_structured_publication_ids
                    .push("publication-v3".into());
            } else {
                state.structured_coverage = Some(StructuredCoverage {
                    epoch: 0,
                    last_source_sequence: 1,
                    raw_offset: 0,
                    prefix_hash: content_hash(&[]),
                });
            }
            assert!(ConversationStore::create(dir.path(), state).is_err());
            assert!(!dir.path().join("sessions-v2").join(id).exists());
        }
    }
    #[cfg(unix)]
    #[test]
    fn structured_clear_removes_v3_grants_and_coverage_but_retains_raw() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with_mode(dir.path(), HistoryMode::Structured10);
        let mut lock = store.lock().unwrap();
        let snapshot = lock.append(Message::user("retain raw"), true).unwrap();
        let mut next = snapshot.state.clone();
        next.visible_structured_publication_ids = vec!["current-publication".into()];
        next.structured_coverage = Some(StructuredCoverage {
            epoch: 0,
            last_source_sequence: 1,
            raw_offset: snapshot.raw_offset,
            prefix_hash: snapshot.raw_hash.clone(),
        });
        next.ancestors.push(AncestorRange {
            session_id: "00000000-0000-4000-8000-000000000002".into(),
            epoch: 0,
            generation: 1,
            visible_summary_ids: vec![],
            visible_structured_publication_ids: vec!["ancestor-publication".into()],
        });
        lock.publish(&snapshot, next).unwrap();
        let raw = fs::read(store.directory().join("raw.jsonl")).unwrap();
        lock.clear_epoch().unwrap();
        let cleared = lock.snapshot().unwrap();
        assert!(cleared.state.visible_structured_publication_ids.is_empty());
        assert!(cleared.state.structured_coverage.is_none());
        assert!(cleared.state.ancestors.is_empty());
        assert_eq!(fs::read(store.directory().join("raw.jsonl")).unwrap(), raw);
    }
    #[cfg(unix)]
    #[test]
    fn ten_real_turns_exclude_examples_and_retain_raw() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let mut lock = store.lock().unwrap();
        for turn in 1..=12 {
            lock.append(Message::user(format!("turn {turn}")), true)
                .unwrap();
            lock.append(Message::user("example"), false).unwrap();
            lock.append(Message::assistant("answer"), false).unwrap();
        }
        let snapshot = lock.snapshot().unwrap();
        assert_eq!(snapshot.expired_turns(), vec![1, 2]);
        assert_eq!(snapshot.recent_messages().len(), 30);
        assert_eq!(snapshot.recent_messages()[0].content, "turn 3");
        assert_eq!(snapshot.events.len(), 36);
        lock.clear_epoch().unwrap();
        let cleared = lock.snapshot().unwrap();
        assert!(cleared.recent_messages().is_empty());
        assert_eq!(cleared.events.len(), 36);
        let new = lock.append(Message::user("new epoch"), true).unwrap();
        assert_eq!(new.active_turns(), vec![13]);
    }
    #[cfg(unix)]
    #[test]
    fn raw_corruption_is_never_repaired_by_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let mut lock = store.lock().unwrap();
        let snapshot = lock.append(Message::user("original"), true).unwrap();
        lock.publish(&snapshot, snapshot.state.clone()).unwrap();
        let path = store.directory().join("raw.jsonl");
        let bytes = fs::read(&path).unwrap();
        let changed = String::from_utf8(bytes)
            .unwrap()
            .replace("original", "modified");
        fs::write(&path, &changed).unwrap();
        assert!(lock.snapshot().is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), changed);
    }
    #[cfg(unix)]
    #[test]
    fn unpublished_raw_is_recoverable_but_incomplete_tail_stops() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        {
            store
                .lock()
                .unwrap()
                .append(Message::user("recover me"), true)
                .unwrap();
        }
        let snapshot = store.lock().unwrap().snapshot().unwrap();
        assert_eq!(snapshot.state.raw_commit_offset, 0);
        assert_eq!(snapshot.events[0].message.content, "recover me");
        let path = store.directory().join("raw.jsonl");
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{partial")
            .unwrap();
        let before = fs::read(&path).unwrap();
        assert!(store.lock().unwrap().snapshot().is_err());
        assert_eq!(fs::read(path).unwrap(), before);
    }
    #[cfg(unix)]
    #[test]
    fn pending_tool_calls_cannot_cross_real_user_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let mut lock = store.lock().unwrap();
        lock.append(Message::user("inspect"), true).unwrap();
        lock.append(
            Message::assistant_with_tool_calls(
                "",
                vec![polaris_provider::ToolCall {
                    id: "call".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({}),
                }],
            ),
            false,
        )
        .unwrap();
        assert!(lock.append(Message::user("next"), true).is_err());
        lock.append(Message::tool_result("call", "result"), false)
            .unwrap();
        lock.append(Message::user("next"), true).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn wrong_scope_version_and_path_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        assert!(ConversationStore::open(dir.path(), "other", ID).is_err());
        assert!(ConversationStore::open(dir.path(), "project", "../escape").is_err());
        assert!(
            ConversationStore::create(
                dir.path(),
                ConversationStateV2::new("project".into(), ID.into()).unwrap()
            )
            .is_err()
        );
        let path = store.directory().join("state.json");
        let mut state: ConversationStateV2 =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        state.gates.schema_version = 9;
        fs::write(path, serde_json::to_vec(&state).unwrap()).unwrap();
        assert!(store.lock().unwrap().snapshot().is_err());
    }
    #[cfg(unix)]
    #[test]
    fn lock_is_exclusive_and_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let lock = store.lock().unwrap();
        assert!(store.lock().is_err());
        drop(lock);
        assert!(store.lock().is_ok());
    }
    #[cfg(unix)]
    #[test]
    fn drop_unlocks_even_if_an_inherited_description_remains_open() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let lock = store.lock().unwrap();
        // dup models the shared open-file description temporarily inherited
        // by a concurrently forked child before exec closes CLOEXEC handles.
        let inherited = lock._lock.try_clone().unwrap();
        assert!(store.lock().is_err());
        drop(lock);
        let next_writer = store.lock().expect("owner released the critical section");
        drop(inherited);
        assert!(
            store.lock().is_err(),
            "a retained handle cannot release the next writer"
        );
        drop(next_writer);
        assert!(store.lock().is_ok());
    }
    #[cfg(unix)]
    #[test]
    fn failed_sync_snapshot_and_marker_never_publish_or_erase_raw() {
        for point in ["raw-sync", "snapshot", "marker"] {
            let dir = tempfile::tempdir().unwrap();
            let store = store(dir.path());
            let mut lock = store.lock().unwrap();
            FAIL_POINT.with(|failure| failure.set(Some(point)));
            let result = lock
                .append(Message::user("retain original"), true)
                .and_then(|snapshot| {
                    lock.save_snapshot(&snapshot)?;
                    lock.publish(&snapshot, snapshot.state.clone())
                });
            FAIL_POINT.with(|failure| failure.set(None));
            assert!(result.is_err(), "{point} must stop publication");
            drop(lock);
            let snapshot = store.lock().unwrap().snapshot().unwrap();
            assert_eq!(snapshot.state.generation, 0);
            assert_eq!(snapshot.state.raw_commit_offset, 0);
            assert_eq!(snapshot.events[0].message.content, "retain original");
        }
    }
    #[cfg(unix)]
    #[test]
    fn a_forged_snapshot_body_cannot_be_saved_under_a_valid_raw_hash() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let mut lock = store.lock().unwrap();
        let mut snapshot = lock.append(Message::user("original"), true).unwrap();
        snapshot.events[0].message.content = "replacement".into();
        assert!(lock.save_snapshot(&snapshot).is_err());
        assert!(lock.publish(&snapshot, snapshot.state.clone()).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn fork_copies_only_ten_turns_and_freezes_parent_generation_without_grants() {
        use crate::workflow::{ExternalGrant, VerificationEvidence};
        let dir = tempfile::tempdir().unwrap();
        let parent = store_with_mode(dir.path(), HistoryMode::Strict10);
        let mut lock = parent.lock().unwrap();
        for turn in 1..=12 {
            lock.append(Message::user(format!("turn {turn}")), true)
                .unwrap();
        }
        let snapshot = lock.snapshot().unwrap();
        let mut next = snapshot.state.clone();
        next.gates.verification = Some(VerificationEvidence {
            artifact_hash: "old".into(),
            passed: true,
            stale: false,
        });
        next.gates.external_grants.push(ExternalGrant {
            operation: "push".into(),
            target: "main".into(),
            artifact_hash: "old".into(),
        });
        lock.publish(&snapshot, next).unwrap();
        let expected = expected_fork_snapshot(
            &lock.snapshot().unwrap(),
            "00000000-0000-4000-8000-000000000002",
            "changed",
        )
        .unwrap();
        let child = lock
            .fork(
                dir.path(),
                "00000000-0000-4000-8000-000000000002",
                "changed",
            )
            .unwrap();
        let fork = child.lock().unwrap().snapshot().unwrap();
        assert_eq!(fork.state, expected.state);
        assert_eq!(fork.raw_hash, expected.raw_hash);
        assert_eq!(fork.raw_offset, expected.raw_offset);
        assert_eq!(
            serde_json::to_vec(&fork.events).unwrap(),
            serde_json::to_vec(&expected.events).unwrap()
        );
        assert_eq!(fork.events.len(), 10);
        assert_eq!(fork.state.history_mode, HistoryMode::Strict10);
        assert_eq!(fork.events[0].message.content, "turn 3");
        assert!(fork.state.gates.external_grants.is_empty());
        assert!(fork.state.gates.verification.unwrap().stale);
        assert_eq!(fork.state.ancestors[0].generation, 1);
        lock.append(Message::user("parent future"), true).unwrap();
        assert_eq!(child.lock().unwrap().snapshot().unwrap().events.len(), 10);
        assert_eq!(lock.snapshot().unwrap().events.len(), 13);
    }
    #[cfg(unix)]
    #[test]
    fn rewind_branch_uses_the_saved_full_prefix_and_visibility_after_ten_turns() {
        let dir = tempfile::tempdir().unwrap();
        let parent = store_with_mode(dir.path(), HistoryMode::Structured10);
        let mut lock = parent.lock().unwrap();
        for turn in 1..=11 {
            lock.append(Message::user(format!("turn {turn}")), true)
                .unwrap();
            lock.append(Message::assistant(format!("answer {turn}")), false)
                .unwrap();
            let snapshot = lock.snapshot().unwrap();
            let mut next = snapshot.state.clone();
            if turn == 11 {
                next.visible_structured_publication_ids = vec!["publication-before".into()];
                next.structured_coverage = Some(StructuredCoverage {
                    epoch: 0,
                    last_source_sequence: snapshot.events.last().unwrap().sequence,
                    raw_offset: snapshot.raw_offset,
                    prefix_hash: snapshot.raw_hash.clone(),
                });
            }
            lock.publish(&snapshot, next).unwrap();
        }
        let boundary = lock
            .begin_rewind_boundary("a".repeat(32), vec!["b".repeat(64)])
            .unwrap();
        lock.append(Message::user("turn 12"), true).unwrap();
        lock.append(Message::assistant("answer 12"), false).unwrap();
        let snapshot = lock.snapshot().unwrap();
        lock.publish(&snapshot, snapshot.state.clone()).unwrap();
        lock.finish_rewind_boundary(
            &boundary.boundary_id,
            "c".repeat(32),
            RewindEligibilityV1::Eligible,
            RewindEffectStatusV1::NoExternalMutation,
        )
        .unwrap();

        let child = lock
            .prepare_rewind_branch(
                dir.path(),
                &boundary.boundary_id,
                "00000000-0000-4000-8000-000000000002",
            )
            .unwrap();
        let child_snapshot = child.lock().unwrap().snapshot().unwrap();
        assert_eq!(child_snapshot.active_turns(), (1..=11).collect::<Vec<_>>());
        assert_eq!(child_snapshot.events.len(), 22);
        assert_eq!(
            child_snapshot.state.recent_turn_ids,
            (2..=11).collect::<Vec<_>>()
        );
        assert_eq!(
            child_snapshot.events.last().unwrap().message.content,
            "answer 11"
        );
        assert!(
            child_snapshot
                .state
                .visible_structured_publication_ids
                .is_empty()
        );
        assert_eq!(
            child_snapshot
                .state
                .ancestors
                .last()
                .unwrap()
                .visible_structured_publication_ids,
            ["publication-before"]
        );
        let metadata = child.rewind_branch_metadata().unwrap().unwrap();
        assert_eq!(metadata.visible_memory_raw_object_ids, ["b".repeat(64)]);
        assert_eq!(
            metadata.branch_initial_raw_offset,
            child_snapshot.raw_offset
        );
        assert_eq!(metadata.branch_initial_raw_hash, child_snapshot.raw_hash);
        let reopened = ConversationStore::open(
            dir.path(),
            "project",
            "00000000-0000-4000-8000-000000000002",
        )
        .unwrap();
        let reopened_metadata = reopened.rewind_branch_metadata().unwrap().unwrap();
        assert_eq!(reopened_metadata.workspace_post_manifest_id, "c".repeat(32));
        assert_eq!(
            reopened_metadata.branch_initial_raw_offset,
            child_snapshot.raw_offset
        );
        assert_eq!(
            reopened_metadata.branch_initial_raw_hash,
            child_snapshot.raw_hash
        );
    }
    #[cfg(unix)]
    #[test]
    fn strict10_rewind_keeps_older_raw_turns_without_the_later_turn() {
        let dir = tempfile::tempdir().unwrap();
        let parent = store_with_mode(dir.path(), HistoryMode::Strict10);
        let mut lock = parent.lock().unwrap();
        for turn in 1..=11 {
            lock.append(Message::user(format!("turn {turn}")), true)
                .unwrap();
            lock.append(Message::assistant(format!("answer {turn}")), false)
                .unwrap();
            let snapshot = lock.snapshot().unwrap();
            lock.publish(&snapshot, snapshot.state.clone()).unwrap();
        }
        let boundary = lock
            .begin_rewind_boundary("a".repeat(32), Vec::new())
            .unwrap();
        lock.append(Message::user("turn 12"), true).unwrap();
        lock.append(Message::assistant("answer 12"), false).unwrap();
        let snapshot = lock.snapshot().unwrap();
        lock.publish(&snapshot, snapshot.state.clone()).unwrap();
        lock.finish_rewind_boundary(
            &boundary.boundary_id,
            "c".repeat(32),
            RewindEligibilityV1::Eligible,
            RewindEffectStatusV1::NoExternalMutation,
        )
        .unwrap();
        let child = lock
            .prepare_rewind_branch(
                dir.path(),
                &boundary.boundary_id,
                "00000000-0000-4000-8000-000000000002",
            )
            .unwrap();
        let restored = child.lock().unwrap().snapshot().unwrap();
        assert_eq!(restored.state.history_mode, HistoryMode::Strict10);
        assert_eq!(restored.active_turns(), (1..=11).collect::<Vec<_>>());
        assert_eq!(restored.events.len(), 22);
        assert_eq!(restored.state.recent_turn_ids, (2..=11).collect::<Vec<_>>());
        assert_eq!(restored.events[0].message.content, "turn 1");
        assert_eq!(restored.events.last().unwrap().message.content, "answer 11");
    }
    #[cfg(unix)]
    #[test]
    fn rewind_branch_before_the_first_turn_keeps_an_empty_initial_raw_head() {
        let dir = tempfile::tempdir().unwrap();
        let parent = store(dir.path());
        let mut lock = parent.lock().unwrap();
        let boundary = lock
            .begin_rewind_boundary("a".repeat(32), Vec::new())
            .unwrap();
        lock.append(Message::user("first"), true).unwrap();
        lock.append(Message::assistant("done"), false).unwrap();
        let snapshot = lock.snapshot().unwrap();
        lock.publish(&snapshot, snapshot.state.clone()).unwrap();
        lock.finish_rewind_boundary(
            &boundary.boundary_id,
            "b".repeat(32),
            RewindEligibilityV1::Eligible,
            RewindEffectStatusV1::NoExternalMutation,
        )
        .unwrap();

        let child = lock
            .prepare_rewind_branch(
                dir.path(),
                &boundary.boundary_id,
                "00000000-0000-4000-8000-000000000002",
            )
            .unwrap();
        let child_snapshot = child.lock().unwrap().snapshot().unwrap();
        assert!(child_snapshot.events.is_empty());
        let metadata = child.rewind_branch_metadata().unwrap().unwrap();
        assert_eq!(metadata.branch_initial_raw_offset, 0);
        assert_eq!(metadata.branch_initial_raw_hash, content_hash(&[]));
    }

    #[cfg(unix)]
    #[test]
    fn rewind_candidate_keeps_metadata_when_creation_stops_before_state() {
        let dir = tempfile::tempdir().unwrap();
        let parent = store(dir.path());
        let mut lock = parent.lock().unwrap();
        let boundary = lock
            .begin_rewind_boundary("a".repeat(32), vec!["b".repeat(64)])
            .unwrap();
        lock.append(Message::user("captured"), true).unwrap();
        lock.append(Message::assistant("done"), false).unwrap();
        let snapshot = lock.snapshot().unwrap();
        lock.publish(&snapshot, snapshot.state.clone()).unwrap();
        lock.finish_rewind_boundary(
            &boundary.boundary_id,
            "c".repeat(32),
            RewindEligibilityV1::Eligible,
            RewindEffectStatusV1::NoExternalMutation,
        )
        .unwrap();

        FAIL_POINT.with(|point| point.set(Some("rewind_candidate_after_metadata")));
        let result = lock.prepare_rewind_branch(
            dir.path(),
            &boundary.boundary_id,
            "00000000-0000-4000-8000-000000000002",
        );
        FAIL_POINT.with(|point| point.set(None));
        assert!(result.is_err());
        let child = dir
            .path()
            .join("sessions-v2/00000000-0000-4000-8000-000000000002");
        assert!(child.join("rewind-branch-v1.json").is_file());
        assert!(!child.join("state.json").exists());
        assert!(
            ConversationStore::open(
                dir.path(),
                "project",
                "00000000-0000-4000-8000-000000000002",
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn rewind_metadata_rejects_a_dangling_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let conversation = store(dir.path());
        std::os::unix::fs::symlink(
            "missing-rewind-branch-v1.json",
            conversation.directory().join("rewind-branch-v1.json"),
        )
        .unwrap();
        assert!(conversation.rewind_branch_metadata().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rewind_candidate_rewrites_epoch_and_accepts_a_later_child_turn() {
        let dir = tempfile::tempdir().unwrap();
        let parent = store(dir.path());
        let mut lock = parent.lock().unwrap();
        lock.append(Message::user("old epoch"), true).unwrap();
        lock.append(Message::assistant("old answer"), false)
            .unwrap();
        let snapshot = lock.snapshot().unwrap();
        lock.publish(&snapshot, snapshot.state.clone()).unwrap();
        lock.clear_epoch().unwrap();
        let boundary = lock
            .begin_rewind_boundary("a".repeat(32), Vec::new())
            .unwrap();
        lock.append(Message::user("new epoch"), true).unwrap();
        lock.append(Message::assistant("new answer"), false)
            .unwrap();
        let snapshot = lock.snapshot().unwrap();
        lock.publish(&snapshot, snapshot.state.clone()).unwrap();
        lock.finish_rewind_boundary(
            &boundary.boundary_id,
            "b".repeat(32),
            RewindEligibilityV1::Eligible,
            RewindEffectStatusV1::NoExternalMutation,
        )
        .unwrap();
        let child = lock
            .prepare_rewind_branch(
                dir.path(),
                &boundary.boundary_id,
                "00000000-0000-4000-8000-000000000002",
            )
            .unwrap();
        let child_snapshot = child.lock().unwrap().snapshot().unwrap();
        assert!(child_snapshot.events.iter().all(|event| event.epoch == 0));
        assert_eq!(child_snapshot.events[0].turn_id, 1);
        assert_eq!(child_snapshot.events[0].message.content, "old epoch");
        drop(lock);
        ConversationStore::validate_rewind_branch_candidate(&parent, &child).unwrap();

        let mut source_lock = parent.lock().unwrap();
        source_lock.clear_epoch().unwrap();
        drop(source_lock);
        ConversationStore::validate_rewind_branch_candidate(&parent, &child).unwrap();

        let mut child_lock = child.lock().unwrap();
        child_lock
            .append(Message::user("later child turn"), true)
            .unwrap();
        let child_snapshot = child_lock.snapshot().unwrap();
        child_lock
            .publish(&child_snapshot, child_snapshot.state.clone())
            .unwrap();
        drop(child_lock);
        ConversationStore::validate_rewind_branch_candidate(&parent, &child).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn rewind_listing_rejects_incomplete_and_later_irreversible_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let parent = store(dir.path());
        let mut lock = parent.lock().unwrap();
        let first = lock
            .begin_rewind_boundary("a".repeat(32), Vec::new())
            .unwrap();
        lock.append(Message::user("first"), true).unwrap();
        lock.append(Message::assistant("done"), false).unwrap();
        let snapshot = lock.snapshot().unwrap();
        lock.publish(&snapshot, snapshot.state.clone()).unwrap();
        lock.finish_rewind_boundary(
            &first.boundary_id,
            "b".repeat(32),
            RewindEligibilityV1::Eligible,
            RewindEffectStatusV1::NoExternalMutation,
        )
        .unwrap();
        assert_eq!(lock.list_rewind_boundaries().unwrap().len(), 1);

        let second = lock
            .begin_rewind_boundary("c".repeat(32), Vec::new())
            .unwrap();
        assert!(lock.list_rewind_boundaries().unwrap().is_empty());
        lock.append(Message::user("second"), true).unwrap();
        lock.append(Message::assistant("done"), false).unwrap();
        let snapshot = lock.snapshot().unwrap();
        lock.publish(&snapshot, snapshot.state.clone()).unwrap();
        lock.finish_rewind_boundary(
            &second.boundary_id,
            "d".repeat(32),
            RewindEligibilityV1::Ineligible {
                reason: "external write".into(),
            },
            RewindEffectStatusV1::ExternalMutation {
                description: "remote side effect".into(),
            },
        )
        .unwrap();
        assert!(lock.list_rewind_boundaries().unwrap().is_empty());

        let interrupted = lock
            .begin_rewind_boundary("e".repeat(32), Vec::new())
            .unwrap();
        lock.append(Message::user("interrupted"), true).unwrap();
        let snapshot = lock.snapshot().unwrap();
        lock.publish(&snapshot, snapshot.state.clone()).unwrap();
        lock.abandon_rewind_boundary(&interrupted.boundary_id, "interrupted turn".into())
            .unwrap();
        let next = lock
            .begin_rewind_boundary("f".repeat(32), Vec::new())
            .unwrap();
        assert!(next.completion.is_none());
    }
    #[cfg(unix)]
    #[test]
    fn rewind_listing_requires_the_latest_completed_boundary_to_match_raw_head() {
        let dir = tempfile::tempdir().unwrap();
        let parent = store(dir.path());
        let mut lock = parent.lock().unwrap();
        let boundary = lock
            .begin_rewind_boundary("a".repeat(32), Vec::new())
            .unwrap();
        lock.append(Message::user("captured"), true).unwrap();
        lock.append(Message::assistant("done"), false).unwrap();
        let snapshot = lock.snapshot().unwrap();
        lock.publish(&snapshot, snapshot.state.clone()).unwrap();
        lock.finish_rewind_boundary(
            &boundary.boundary_id,
            "b".repeat(32),
            RewindEligibilityV1::Eligible,
            RewindEffectStatusV1::NoExternalMutation,
        )
        .unwrap();
        assert_eq!(lock.list_rewind_boundaries().unwrap().len(), 1);

        lock.append(Message::user("unrecorded raw tail"), true)
            .unwrap();
        assert!(lock.list_rewind_boundaries().unwrap().is_empty());
    }
    #[cfg(unix)]
    #[test]
    fn rewind_listing_rejects_a_later_abandoned_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let parent = store(dir.path());
        let mut lock = parent.lock().unwrap();
        let first = lock
            .begin_rewind_boundary("a".repeat(32), Vec::new())
            .unwrap();
        lock.append(Message::user("first"), true).unwrap();
        lock.append(Message::assistant("done"), false).unwrap();
        let snapshot = lock.snapshot().unwrap();
        lock.publish(&snapshot, snapshot.state.clone()).unwrap();
        lock.finish_rewind_boundary(
            &first.boundary_id,
            "b".repeat(32),
            RewindEligibilityV1::Eligible,
            RewindEffectStatusV1::NoExternalMutation,
        )
        .unwrap();
        assert_eq!(lock.list_rewind_boundaries().unwrap().len(), 1);

        let later = lock
            .begin_rewind_boundary("c".repeat(32), Vec::new())
            .unwrap();
        lock.append(Message::user("interrupted"), true).unwrap();
        let snapshot = lock.snapshot().unwrap();
        lock.publish(&snapshot, snapshot.state.clone()).unwrap();
        lock.abandon_rewind_boundary(&later.boundary_id, "interrupted turn".into())
            .unwrap();
        assert!(lock.list_rewind_boundaries().unwrap().is_empty());
    }
    #[cfg(unix)]
    #[test]
    fn startup_recovery_abandons_only_pending_rewind_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let parent = store(dir.path());
        let mut lock = parent.lock().unwrap();
        let complete = lock
            .begin_rewind_boundary("a".repeat(32), Vec::new())
            .unwrap();
        lock.append(Message::user("complete"), true).unwrap();
        lock.append(Message::assistant("done"), false).unwrap();
        let snapshot = lock.snapshot().unwrap();
        lock.publish(&snapshot, snapshot.state.clone()).unwrap();
        lock.finish_rewind_boundary(
            &complete.boundary_id,
            "b".repeat(32),
            RewindEligibilityV1::Eligible,
            RewindEffectStatusV1::NoExternalMutation,
        )
        .unwrap();
        let pending = lock
            .begin_rewind_boundary("c".repeat(32), Vec::new())
            .unwrap();

        let abandoned = lock
            .abandon_unfinished_rewind_boundaries("process interrupted".into())
            .unwrap();
        assert_eq!(abandoned.len(), 1);
        assert_eq!(abandoned[0].boundary_id, pending.boundary_id);
        assert_eq!(
            abandoned[0].abandoned_reason.as_deref(),
            Some("process interrupted")
        );
        assert!(
            lock.abandon_unfinished_rewind_boundaries("process interrupted".into())
                .unwrap()
                .is_empty()
        );
        let next = lock
            .begin_rewind_boundary("d".repeat(32), Vec::new())
            .unwrap();
        assert!(next.completion.is_none());
    }
}

#[cfg(all(test, windows))]
mod windows_store_tests;
