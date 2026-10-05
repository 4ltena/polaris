//! P1値を共用する保存marker、付随状態、要求結果と明示的な保存エラー。

use crate::conversation_state::RawEventV2;
use polaris_desktop_protocol::{
    ids::*,
    snapshot::{ApprovalDecision, Configuration, Draft, PendingApproval, Run, Task},
};
use serde::{Deserialize, Serialize};

pub type StoreResult<T> = Result<T, StoreError>;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("保存I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("保存形式: {0}")]
    Json(#[from] serde_json::Error),
    #[error("不正な公開状態: {0}")]
    Corrupt(&'static str),
    #[error("関連要約の処理上限: {0}")]
    RelatedSummaryBudget(polaris_memory::conversation::RelatedSummaryBudget),
    #[error("未対応の保存版: {0}")]
    UnsupportedVersion(u32),
    #[error("このOSの原子的置換・directory sync・私用領域は未対応")]
    UnsupportedPlatform,
    #[error("別writerが所有しています")]
    Busy,
    #[error("公開状態の再照合が必要です")]
    RecoveryRequired,
    #[error("削除済みsessionです")]
    Deleted,
    #[error("対象session/projectが一致しません")]
    TargetMismatch,
    #[error("未対応の変更methodです")]
    UnsupportedMethod,
    #[error("期待版が一致しません: {0}")]
    CasConflict(&'static str),
    #[error("要求IDが異なる内容に再利用されました")]
    RequestConflict,
    #[error("run/attempt/operation/結果が競合しています")]
    RunConflict,
    #[error("対象が存在しません")]
    NotFound,
    #[error("値の範囲超過")]
    Overflow,
    #[error("SessionManager容量不足: {0}")]
    Capacity(&'static str),
}

impl From<polaris_desktop_protocol::ids::ValueError> for StoreError {
    fn from(_: polaris_desktop_protocol::ids::ValueError) -> Self {
        Self::Overflow
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Marker {
    pub schema_version: u32,
    pub session_id: SessionId,
    pub project_id: ProjectId,
    pub epoch: DecimalU64,
    pub session_revision: DecimalU64,
    pub content_revision: DecimalU64,
    pub raw_offset: DecimalU64,
    pub raw_hash: String,
    pub sidecar_offset: DecimalU64,
    pub sidecar_hash: String,
    pub deleted: bool,
}

#[derive(Debug, Clone)]
pub struct InitialState {
    pub draft: Draft,
    pub configuration: Configuration,
    pub policy_revision: DecimalU64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Operation {
    pub operation_id: OperationId,
    pub result_id: Option<ResultId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<super::SavedMcpOperation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<super::SavedMcpPrompt>,
    /// One step of a bundled setting command, with what it was asked to do
    /// and how it ended. Absent for every other kind of operation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipe: Option<super::SavedRecipeStep>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunRecord {
    pub run: Run,
    /// Durable identity of an `input.submit` admission.  The destination is
    /// repeated even though it equals the containing marker so a recovered
    /// root owner can prove which session accepted the payload without
    /// re-appending it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission: Option<SavedAdmissionBinding>,
    pub configuration: Configuration,
    /// Accepted choices and past observations only; never runtime authorization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_bindings: Option<super::SavedRoleBindings>,
    /// What each declared weight is worth. Absent means no weight is
    /// priced and every delegation keeps the parent's model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier_bindings: Option<super::SavedTierBindings>,
    /// 受理時に固定した登録。無い記録は複数認証導入前のもので、単一認証を使う。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<super::SavedAccount>,
    pub policy_revision: DecimalU64,
    /// 送信時の添付IDも原文と同じ世代に保持する。実ファイル解決は行わない。
    pub input: Draft,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_prompt: Option<polaris_desktop_protocol::request::McpPromptChoice>,
    pub operations: Vec<Operation>,
    pub result_id: Option<ResultId>,
    /// None means no usage observation has been published, not zero consumption.
    #[serde(default)]
    pub usage: Option<polaris_provider::UsageReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<polaris_desktop_protocol::snapshot::MemoryStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_resources: Option<super::MemoryResources>,
    /// Frozen at acceptance, replaced atomically with the terminal checkpoint.
    #[serde(default)]
    pub workflow: Option<super::SavedWorkflow>,
    /// Some received output could not be retained; never report a complete transcript.
    #[serde(default)]
    pub history_gap: Option<HistoryGap>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedAdmissionBinding {
    pub admission_id: String,
    pub payload_hash: String,
    pub destination_session_id: SessionId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmittedRun {
    pub run: RunRecord,
    pub newly_appended: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryGap {
    OutputRejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RequestResult {
    SourceApplyResolved {
        approval_id: ApprovalId,
    },
    ApprovalResolved {
        approval_id: ApprovalId,
    },
    DraftUpdated {
        draft_revision: DecimalU64,
    },
    Configured {
        configuration_revision: DecimalU64,
        configuration: Configuration,
    },
    CancelRequested {
        run_id: RunId,
        attempt_id: AttemptId,
    },
    RunAccepted {
        run_id: RunId,
        attempt_id: AttemptId,
    },
    GoalUpdated {
        goal_revision: DecimalU64,
    },
    /// Received and durable. Applying it is a separate transition.
    InstructionReceived {
        instruction_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestRecord {
    pub client_id: ClientId,
    pub request_id: RequestId,
    pub request_hash: String,
    pub accepted_revision: DecimalU64,
    pub result: RequestResult,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sidecar {
    /// 完全な継承原文のturn数。子の実行台帳には含めず、作成後は変更しない。
    pub inherited_turn_count: DecimalU64,
    pub conversation_memory: Option<super::SavedMemory>,
    /// Controller-only evidence; never an RPC/model execution capability.
    pub source_applies: Vec<super::SavedSourceApply>,
    pub children: Vec<super::SavedChild>,
    pub session_revision: DecimalU64,
    pub draft: Draft,
    pub configuration: Configuration,
    /// Accepted choices and past observations only; never runtime authorization.
    pub role_bindings: Option<super::SavedRoleBindings>,
    /// What each declared weight is worth. Absent means no weight is
    /// priced and every delegation keeps the parent's model.
    pub tier_bindings: Option<super::SavedTierBindings>,
    /// M6-3: what the session is working toward. It never occupies the input
    /// and never authorizes anything by itself.
    pub goal: Option<super::SavedGoal>,
    /// Instructions that arrived while a run was going: durable when received,
    /// marked separately when a run applied them.
    pub instructions: Vec<super::SavedInstruction>,
    pub policy_revision: DecimalU64,
    pub tasks: Vec<Task>,
    pub runs: Vec<RunRecord>,
    pub requests: Vec<RequestRecord>,
    pub unresolved_approvals: Vec<PendingApproval>,
    pub approval_records: Vec<ApprovalRecord>,
    pub workflow: Option<super::SavedWorkflow>,
    /// Metadata only; immutable bodies remain in the root-owned artifact CAS.
    pub workflow_artifacts: crate::workflow::WorkflowArtifactGraphV1,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SidecarWireV1 {
    #[serde(default)]
    inherited_turn_count: DecimalU64,
    #[serde(default)]
    conversation_memory: Option<super::SavedMemory>,
    #[serde(default)]
    source_applies: Vec<WireObject<SourceApplyWireV1>>,
    #[serde(default)]
    children: Vec<super::SavedChild>,
    session_revision: DecimalU64,
    draft: Draft,
    configuration: Configuration,
    #[serde(default)]
    role_bindings: Option<super::SavedRoleBindings>,
    #[serde(default)]
    tier_bindings: Option<super::SavedTierBindings>,
    #[serde(default)]
    goal: Option<super::SavedGoal>,
    #[serde(default)]
    instructions: Vec<super::SavedInstruction>,
    policy_revision: DecimalU64,
    tasks: Vec<Task>,
    runs: Vec<RunRecord>,
    requests: Vec<RequestRecord>,
    unresolved_approvals: Vec<PendingApproval>,
    #[serde(default)]
    approval_records: Vec<ApprovalRecord>,
    #[serde(default)]
    workflow: Option<super::SavedWorkflow>,
    #[serde(default)]
    workflow_artifacts: crate::workflow::WorkflowArtifactGraphV1,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SidecarWireV2 {
    desktop_sidecar_schema_version: SidecarSchemaV2,
    inherited_turn_count: DecimalU64,
    #[serde(deserialize_with = "required_nullable")]
    conversation_memory: Option<super::SavedMemory>,
    source_applies: Vec<WireObject<SourceApplyWireV2>>,
    children: Vec<super::SavedChild>,
    session_revision: DecimalU64,
    draft: Draft,
    configuration: Configuration,
    #[serde(deserialize_with = "required_nullable")]
    role_bindings: Option<super::SavedRoleBindings>,
    #[serde(deserialize_with = "required_nullable")]
    tier_bindings: Option<super::SavedTierBindings>,
    #[serde(deserialize_with = "required_nullable")]
    goal: Option<super::SavedGoal>,
    instructions: Vec<super::SavedInstruction>,
    policy_revision: DecimalU64,
    tasks: Vec<Task>,
    runs: Vec<RunRecord>,
    requests: Vec<RequestRecord>,
    unresolved_approvals: Vec<PendingApproval>,
    approval_records: Vec<ApprovalRecord>,
    #[serde(deserialize_with = "required_nullable")]
    workflow: Option<super::SavedWorkflow>,
    workflow_artifacts: crate::workflow::WorkflowArtifactGraphV1,
}

#[derive(Deserialize)]
struct SidecarSchemaV2(u8);
impl SidecarSchemaV2 {
    fn validate(self) -> std::result::Result<(), &'static str> {
        if self.0 == 2 {
            Ok(())
        } else {
            Err("unsupported desktop sidecar version")
        }
    }
}

fn required_nullable<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error> {
    Option::<T>::deserialize(deserializer)
}

/// These wire records are JSON objects; struct tuple/array decoding is not
/// part of either version. MapAccess retains duplicate fields for validation.
struct WireObject<T>(T);
impl<'de, T: Deserialize<'de>> Deserialize<'de> for WireObject<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ObjectVisitor<T>(std::marker::PhantomData<T>);
        impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for ObjectVisitor<T> {
            type Value = WireObject<T>;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("JSON object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                map: A,
            ) -> Result<Self::Value, A::Error> {
                T::deserialize(serde::de::value::MapAccessDeserializer::new(map)).map(WireObject)
            }
        }
        deserializer.deserialize_map(ObjectVisitor(std::marker::PhantomData))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceApplyWireV1 {
    candidate: super::SourceApplyCandidate,
    decision: Option<ApprovalDecision>,
    invalidated: bool,
    intent_revision: Option<DecimalU64>,
    result: Option<super::SourceApplyResult>,
}
impl From<SourceApplyWireV1> for super::SavedSourceApply {
    fn from(value: SourceApplyWireV1) -> Self {
        Self {
            candidate: value.candidate,
            decision: value.decision,
            invalidated: value.invalidated,
            intent_revision: value.intent_revision,
            reserved_result_id: None,
            result: value.result,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceApplyWireV2 {
    candidate: super::SourceApplyCandidate,
    #[serde(deserialize_with = "required_nullable")]
    decision: Option<ApprovalDecision>,
    invalidated: bool,
    #[serde(deserialize_with = "required_nullable")]
    intent_revision: Option<DecimalU64>,
    #[serde(deserialize_with = "required_nullable")]
    reserved_result_id: Option<ResultId>,
    #[serde(deserialize_with = "required_nullable")]
    result: Option<super::SourceApplyResult>,
}
impl From<SourceApplyWireV2> for super::SavedSourceApply {
    fn from(value: SourceApplyWireV2) -> Self {
        Self {
            candidate: value.candidate,
            decision: value.decision,
            invalidated: value.invalidated,
            intent_revision: value.intent_revision,
            reserved_result_id: value.reserved_result_id,
            result: value.result,
        }
    }
}

// Buffering serde Content preserves duplicate map entries, unlike Value.
#[derive(Deserialize)]
#[serde(untagged)]
enum SidecarWire {
    V2(SidecarWireV2),
    V1(SidecarWireV1),
}
impl SidecarWire {
    fn into_state(self) -> std::result::Result<(Sidecar, u8), &'static str> {
        Ok(match self {
            Self::V1(value) => (
                Sidecar {
                    inherited_turn_count: value.inherited_turn_count,
                    conversation_memory: value.conversation_memory,
                    source_applies: value
                        .source_applies
                        .into_iter()
                        .map(|record| record.0.into())
                        .collect(),
                    children: value.children,
                    session_revision: value.session_revision,
                    draft: value.draft,
                    configuration: value.configuration,
                    role_bindings: value.role_bindings,
                    tier_bindings: value.tier_bindings,
                    goal: value.goal,
                    instructions: value.instructions,
                    policy_revision: value.policy_revision,
                    tasks: value.tasks,
                    runs: value.runs,
                    requests: value.requests,
                    unresolved_approvals: value.unresolved_approvals,
                    approval_records: value.approval_records,
                    workflow: value.workflow,
                    workflow_artifacts: value.workflow_artifacts,
                },
                1,
            ),
            Self::V2(value) => {
                value.desktop_sidecar_schema_version.validate()?;
                (
                    Sidecar {
                        inherited_turn_count: value.inherited_turn_count,
                        conversation_memory: value.conversation_memory,
                        source_applies: value
                            .source_applies
                            .into_iter()
                            .map(|record| record.0.into())
                            .collect(),
                        children: value.children,
                        session_revision: value.session_revision,
                        draft: value.draft,
                        configuration: value.configuration,
                        role_bindings: value.role_bindings,
                        tier_bindings: value.tier_bindings,
                        goal: value.goal,
                        instructions: value.instructions,
                        policy_revision: value.policy_revision,
                        tasks: value.tasks,
                        runs: value.runs,
                        requests: value.requests,
                        unresolved_approvals: value.unresolved_approvals,
                        approval_records: value.approval_records,
                        workflow: value.workflow,
                        workflow_artifacts: value.workflow_artifacts,
                    },
                    2,
                )
            }
        })
    }
}

impl Serialize for Sidecar {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut value = serializer.serialize_struct("SidecarWireV2", 20)?;
        value.serialize_field("desktop_sidecar_schema_version", &2u8)?;
        value.serialize_field("inherited_turn_count", &self.inherited_turn_count)?;
        value.serialize_field("conversation_memory", &self.conversation_memory)?;
        value.serialize_field("source_applies", &self.source_applies)?;
        value.serialize_field("children", &self.children)?;
        value.serialize_field("session_revision", &self.session_revision)?;
        value.serialize_field("draft", &self.draft)?;
        value.serialize_field("configuration", &self.configuration)?;
        value.serialize_field("role_bindings", &self.role_bindings)?;
        value.serialize_field("tier_bindings", &self.tier_bindings)?;
        value.serialize_field("goal", &self.goal)?;
        value.serialize_field("instructions", &self.instructions)?;
        value.serialize_field("policy_revision", &self.policy_revision)?;
        value.serialize_field("tasks", &self.tasks)?;
        value.serialize_field("runs", &self.runs)?;
        value.serialize_field("requests", &self.requests)?;
        value.serialize_field("unresolved_approvals", &self.unresolved_approvals)?;
        value.serialize_field("approval_records", &self.approval_records)?;
        value.serialize_field("workflow", &self.workflow)?;
        value.serialize_field("workflow_artifacts", &self.workflow_artifacts)?;
        value.end()
    }
}
impl<'de> Deserialize<'de> for Sidecar {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        WireObject::<SidecarWire>::deserialize(deserializer)?
            .0
            .into_state()
            .map(|(state, _)| state)
            .map_err(serde::de::Error::custom)
    }
}
pub(super) fn decode_sidecar_wire(bytes: &[u8]) -> StoreResult<(Sidecar, u8)> {
    serde_json::from_slice::<WireObject<SidecarWire>>(bytes)?
        .0
        .into_state()
        .map_err(StoreError::Corrupt)
}

#[derive(Debug, Clone)]
pub struct Published {
    pub marker: Marker,
    pub state: Sidecar,
    pub raw: Vec<RawEventV2>,
}

impl Published {
    pub(super) fn raw_run_index(&self, run_index: usize) -> StoreResult<usize> {
        usize::try_from(self.state.inherited_turn_count.get())
            .map_err(|_| StoreError::Overflow)?
            .checked_add(run_index)
            .ok_or(StoreError::Overflow)
    }
}

/// 既存受理でも最新run状態を返し、不存在とoutcome_unknownを区別する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Acceptance {
    pub record: RequestRecord,
    pub run: Option<RunRecord>,
}

/// AlreadyRecordedは実行許可ではない。上位層は同じ操作を再dispatchしない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentReceipt {
    NewlyPublished,
    AlreadyRecorded,
}

/// IDと束縛を失効後も保持する。回答の履歴と実行権を分離する。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRecord {
    pub pending: PendingApproval,
    pub decision: Option<ApprovalDecision>,
    pub consumed: bool,
    pub invalidated: bool,
}
