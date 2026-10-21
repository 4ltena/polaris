//! 保存・実行の単一所有者。実行入力は信頼済みownerから明示注入する。

mod local_models;
pub use local_models::probe_local_models;
mod projection;
#[cfg(test)]
use polaris_desktop_protocol::orchestration::{ClarificationStateV2, CommonRunnerStateV2};
use projection::{child_control_state, child_summary, orchestration_summary, workflow_status};
#[cfg(target_os = "macos")]
mod mcp_catalog;
#[cfg(all(test, target_os = "macos"))]
use polaris_mcp::test_support as mcp_fixture;
#[cfg(target_os = "macos")]
mod mcp_settings;
#[cfg(target_os = "macos")]
mod mcp_setup;
#[cfg(target_os = "macos")]
pub(crate) use mcp_settings::McpSettings;
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod real;
mod role_bindings;
mod run_details;
mod session_admission;
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod source_owner;
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod source_recovery;
mod source_view;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub use real::{
    TrustedHistoryResources, TrustedMcpResources, TrustedRunCompletion, TrustedRunFactory,
    TrustedRunInputs,
};
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub use source_owner::{SourceApplyRuntime, TrustedSourceFactory};
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub use source_recovery::SourceRecoveryHandle;

use crate::transport::{self, Exit, Outbox, ServiceError, WorkerExit};
use polaris_core::desktop_store::{
    Acceptance, DesktopRoot, InitialState, IntentReceipt, PrototypeRoot, Published, RequestResult,
    StoreError, Writer,
};
use polaris_desktop_protocol::{
    ProtocolVersion,
    event::{Durability, Event, EventBody, TextDelta},
    ids::*,
    request::{DraftUpdate, Request, RequestBody, RunTarget},
    response::*,
    run_state::{Observation, RunState},
    snapshot::{Configuration, Draft, Position, Snapshot},
    validate::{ConnectionState, validate_request},
};
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    time::{Instant, sleep_until, timeout},
};

const TEXT_BYTES: usize = 32 * 1024;
const SCRIPT_CHUNKS: usize = 256;
const REQUEST_RECORDS: usize = 128;
const RUNS: usize = 32;
const SNAPSHOT_TTL: Duration = Duration::from_secs(30);
static EPOCH_SERIAL: AtomicU64 = AtomicU64::new(0);

fn delegation_control_error(message: String) -> ProtocolError {
    let code = if message.contains("revision mismatch")
        || message.contains("conflicting idempotent operation")
    {
        ErrorCode::RevisionConflict
    } else if message.contains("binding mismatch") || message.contains("wrong root") {
        ErrorCode::PermissionDenied
    } else if message.contains("unknown child or attempt") {
        ErrorCode::NotFound
    } else if message.contains("invalid state") || message.contains("invalid control-plane") {
        ErrorCode::InvalidRequest
    } else if message.contains("runtime unavailable") {
        ErrorCode::CapabilityUnavailable
    } else {
        ErrorCode::StorageFailed
    };
    error(code)
}

fn admission_target(id: &polaris_core::session_manager::AdmissionId) -> RunTarget {
    fn derive(prefix: &[u8], id: &str) -> String {
        let mut hash = Sha256::new();
        hash.update(prefix);
        hash.update(id.as_bytes());
        format!("{:x}", hash.finalize())
    }
    RunTarget {
        run_id: RunId::new(format!(
            "run-{}",
            derive(b"polaris-admission-run-v1\0", id.as_str())
        ))
        .expect("derived run ID"),
        attempt_id: AttemptId::new(format!(
            "attempt-{}",
            derive(b"polaris-admission-attempt-v1\0", id.as_str())
        ))
        .expect("derived attempt ID"),
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn workflow_phase(
    phase: polaris_desktop_protocol::request::WorkflowPhase,
) -> polaris_core::workflow::Phase {
    use polaris_desktop_protocol::request::WorkflowPhase as Wire;
    match phase {
        Wire::General => polaris_core::workflow::Phase::General,
        Wire::Brainstorm => polaris_core::workflow::Phase::Brainstorm,
        Wire::Specify => polaris_core::workflow::Phase::Specify,
        Wire::Implement => polaris_core::workflow::Phase::Implement,
        Wire::Review => polaris_core::workflow::Phase::Review,
        Wire::Verify => polaris_core::workflow::Phase::Verify,
        Wire::Deliver => polaris_core::workflow::Phase::Deliver,
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(super) fn validated_workflow(
    config: polaris_core::workflow::WorkflowConfig,
    gates: polaris_core::workflow::WorkflowGatesV1,
    state: polaris_core::workflow::WorkflowStateV1,
    artifacts: polaris_core::workflow::WorkflowArtifactGraphV1,
    skills: &[polaris_skills::Skill],
) -> Result<polaris_core::desktop_store::SavedWorkflow, ProtocolError> {
    let workflow = polaris_core::workflow::SessionWorkflow::restore_with_artifacts(
        config.clone(),
        gates.clone(),
        state,
        artifacts.clone(),
    )
    .map_err(|_| error(ErrorCode::CapabilityUnavailable))?;
    if !config.enabled {
        return Ok(polaris_core::desktop_store::SavedWorkflow::capture(
            &workflow,
        ));
    }
    let turn = workflow
        .begin_turn(skills)
        .map_err(|_| error(ErrorCode::CapabilityUnavailable))?;
    Ok(polaris_core::desktop_store::SavedWorkflow::capture(
        &polaris_core::workflow::SessionWorkflow::restore_with_artifacts(
            config,
            gates,
            turn.state.clone(),
            artifacts,
        )
        .map_err(|_| error(ErrorCode::CapabilityUnavailable))?,
    ))
}

pub fn project_id() -> ProjectId {
    ProjectId::new("fake-project").unwrap()
}
pub fn session_id() -> SessionId {
    SessionId::new("fake-session").unwrap()
}

/// 有限scriptと試験用時計の設定。ネットワーク先やpathは受け取らない。
#[derive(Debug, Clone)]
pub struct Options {
    pub script: Vec<String>,
    pub step_interval: Duration,
    pub output_deadline: Duration,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            script: vec!["こんにちは。".into(), "これはfakeの応答です。".into()],
            step_interval: Duration::from_millis(50),
            output_deadline: Duration::from_secs(5),
        }
    }
}
impl Options {
    fn validate(&self) -> Result<(), ServiceError> {
        if self.script.len() > SCRIPT_CHUNKS
            || self.script.iter().map(String::len).sum::<usize>() > TEXT_BYTES
            || self.step_interval.is_zero()
            || self.step_interval > Duration::from_millis(50)
            || self.output_deadline.is_zero()
            || self.output_deadline > Duration::from_secs(30)
        {
            return Err(ServiceError::Options);
        }
        Ok(())
    }
}

struct Active {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    real: Option<real::RealRun>,
    usage: polaris_provider::UsageMeter,
    target: RunTarget,
    operation: OperationId,
    message: MessageId,
    chunk: usize,
    text: String,
    started: bool,
    execution: crate::execution::RunExecution,
    admission: Option<polaris_core::session_manager::AdmissionId>,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    delegation: Option<DelegationRuntime>,
}
impl Active {
    fn cancel(&mut self) {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if let Some(real) = &mut self.real {
            real.cancel();
        }
        self.execution.cancel();
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
struct DelegationRuntime {
    control: Arc<polaris_core::session::RootDelegationControlV1>,
    projection: Arc<std::sync::RwLock<polaris_core::session::RootDelegationProjectionV1>>,
    updates: std::sync::mpsc::Receiver<()>,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl DelegationRuntime {
    fn projection(&self) -> Option<polaris_core::session::RootDelegationProjectionV1> {
        self.projection.read().ok().map(|value| value.clone())
    }

    fn take_update(&self) -> bool {
        let mut changed = false;
        while self.updates.try_recv().is_ok() {
            changed = true;
        }
        changed
    }

    fn wait_children(
        &self,
        binding: polaris_core::session::RootDelegationHostBindingV1,
        expected_revision: u64,
        after_cursor: Option<u64>,
    ) -> Result<Option<polaris_core::session::ChildWaitSnapshotV1>, String> {
        let control = self.control.clone();
        std::thread::Builder::new()
            .name("polaris-delegation-wait".into())
            .spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|_| "delegation runtime unavailable".to_owned())?
                    .block_on(control.wait_children(&binding, expected_revision, after_cursor))
            })
            .map_err(|_| "delegation runtime unavailable".to_owned())?
            .join()
            .map_err(|_| "delegation runtime unavailable".to_owned())?
    }

    fn request_child_cancel(
        &self,
        binding: polaris_core::session::RootDelegationHostBindingV1,
        expected_revision: u64,
        reference: polaris_core::subagent_control::ChildAttemptRefV1,
    ) -> Result<polaris_core::session::ChildCancelAckV1, String> {
        let control = self.control.clone();
        std::thread::Builder::new()
            .name("polaris-delegation-child-cancel".into())
            .spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|_| "delegation runtime unavailable".to_owned())?
                    .block_on(control.request_child_cancel(&binding, expected_revision, reference))
            })
            .map_err(|_| "delegation runtime unavailable".to_owned())?
            .join()
            .map_err(|_| "delegation runtime unavailable".to_owned())?
    }

    fn request_root_cancel(
        &self,
        binding: polaris_core::session::RootDelegationHostBindingV1,
        expected_revision: u64,
    ) -> Result<polaris_core::session::RootCancelAckV1, String> {
        let control = self.control.clone();
        std::thread::Builder::new()
            .name("polaris-delegation-root-cancel".into())
            .spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|_| "delegation runtime unavailable".to_owned())?
                    .block_on(control.request_root_cancel(&binding, expected_revision))
            })
            .map_err(|_| "delegation runtime unavailable".to_owned())?
            .join()
            .map_err(|_| "delegation runtime unavailable".to_owned())?
    }
}
struct History {
    id: SnapshotId,
    published: Published,
    expires: Instant,
}

struct SessionLane {
    session: SessionId,
    store: Writer,
    // A subscription position belongs to the session it names.  The root
    // swaps lanes for a request, so keeping this on Engine would otherwise
    // let a sidechat subscribe replace the main session's event identity.
    subscription: Option<Position>,
    active: Option<Active>,
    history: Option<History>,
    details: Option<run_details::Details>,
    cleanups: crate::execution::CleanupOwner,
}

struct RoutingJob {
    admission: polaris_core::session_manager::AdmissionId,
    worker: std::thread::JoinHandle<Option<polaris_core::session_manager::RoutingJudgementV1>>,
}

/// Explicit native-owner configuration. No environment/path discovery or auth.
#[derive(Debug, Clone)]
pub struct ServiceConfig {
    pub store_root: PathBuf,
    pub project_id: ProjectId,
    pub session_id: SessionId,
}
/// Durable storage IPC by default; real runs require an explicit trusted owner.
pub struct DesktopService {
    engine: Option<Engine>,
}
impl DesktopService {
    /// The trusted startup owner has already checked this project's source
    /// identity. An unbound production owner cannot accept new input.
    #[cfg(unix)]
    pub(crate) fn with_management_project(
        mut self,
        project: PathBuf,
        identity: (u64, u64),
    ) -> Result<Self, ServiceError> {
        use polaris_core::{
            management_effect::ManagementEffectStoreV1, project_mutation::ProjectMutationOwnerV1,
        };
        let source_path = project;
        let project = std::fs::canonicalize(&source_path).map_err(|_| ServiceError::Options)?;
        if !management_project_matches(&source_path, identity)
            || !management_project_matches(&project, identity)
        {
            return Err(ServiceError::Options);
        }
        let owner = ProjectMutationOwnerV1::for_project(&project)
            .map_err(|_| ServiceError::Store(StoreError::RecoveryRequired))?;
        #[cfg(target_os = "linux")]
        let completed = {
            let apply = polaris_core::apply_owner::ApplyOwnerV1::acquire(&project)
                .map_err(|_| ServiceError::Store(StoreError::RecoveryRequired))?;
            if apply
                .active()
                .map_err(|_| ServiceError::Store(StoreError::RecoveryRequired))?
                .is_some()
            {
                return Err(ServiceError::Store(StoreError::RecoveryRequired));
            }
            let saved = self
                .engine
                .as_ref()
                .ok_or(ServiceError::Worker)?
                .store
                .snapshot()?;
            let mut completed = Vec::new();
            for row in &saved.state.source_applies {
                if row.reserved_result_id.is_some()
                    && row.result.is_some()
                    && apply
                        .d27_result_is_completed(&saved, row)
                        .map_err(|_| ServiceError::Store(StoreError::RecoveryRequired))?
                {
                    completed.push(row.clone());
                }
            }
            completed
        };
        let effects = {
            let mutation = owner
                .try_mutation()
                .map_err(|_| ServiceError::Store(StoreError::RecoveryRequired))?;
            ManagementEffectStoreV1::open(&project, &mutation)
                .map_err(|_| ServiceError::Store(StoreError::RecoveryRequired))?
        };
        if !management_project_matches(&source_path, identity)
            || !management_project_matches(&project, identity)
        {
            return Err(ServiceError::Options);
        }
        let engine = self.engine.as_mut().ok_or(ServiceError::Worker)?;
        engine.management_source_path = Some(source_path);
        engine.management_project = Some(project);
        engine.management_identity = Some(identity);
        engine.management_owner = Some(owner);
        engine.management_effects = Some(effects);
        #[cfg(target_os = "linux")]
        {
            engine.saved_source_completed = completed;
        }

        Ok(self)
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn with_workspace_reader(mut self, reader: crate::WorkspaceViewReader) -> Self {
        if let Some(engine) = self.engine.as_mut() {
            engine.workspace_reader = Some(reader);
        }
        self
    }

    pub fn open(config: ServiceConfig) -> Result<Self, ServiceError> {
        Self::open_storage(
            config,
            Configuration {
                history_mode: Default::default(),
                configuration_revision: DecimalU64::new(0),
                provider: "unconfigured".into(),
                model: String::new(),
                effort: "none".into(),
            },
        )
    }
    fn open_storage(
        config: ServiceConfig,
        configuration: Configuration,
    ) -> Result<Self, ServiceError> {
        Self::open_storage_bound(config, configuration, None)
    }

    #[cfg(target_os = "linux")]
    pub fn open_source_bound(
        config: ServiceConfig,
        source: PathBuf,
        identity: (u64, u64),
    ) -> Result<Self, ServiceError> {
        if !management_project_matches(&source, identity) {
            return Err(ServiceError::Options);
        }
        Self::open_storage_bound(
            config,
            Configuration {
                history_mode: Default::default(),
                configuration_revision: DecimalU64::new(0),
                provider: "unconfigured".into(),
                model: String::new(),
                effort: "none".into(),
            },
            Some((source, identity)),
        )
    }

    fn open_storage_bound(
        config: ServiceConfig,
        configuration: Configuration,
        source: Option<(PathBuf, (u64, u64))>,
    ) -> Result<Self, ServiceError> {
        let root = DesktopRoot::open_owned(&config.store_root)?;
        #[cfg(target_os = "linux")]
        let recovered = if let Some((path, identity)) = &source {
            use polaris_core::{
                apply_intent::ApplyInventoryV1, apply_owner::ApplyOwnerV1,
                project_mutation::open_directory_nofollow, workspace_apply::RecoveryParent,
            };
            if !management_project_matches(path, *identity) {
                return Err(ServiceError::Options);
            }
            let owner = ApplyOwnerV1::acquire(path)
                .map_err(|_| ServiceError::Store(StoreError::RecoveryRequired))?;
            if let Some(intent) = owner
                .active()
                .map_err(|_| ServiceError::Store(StoreError::RecoveryRequired))?
            {
                if intent.descriptor().inventory_id != ApplyInventoryV1::D27 {
                    return Err(ServiceError::Store(StoreError::RecoveryRequired));
                }
                let parent_path = PathBuf::from(
                    &intent
                        .d27_source_binding
                        .as_ref()
                        .ok_or(ServiceError::Store(StoreError::RecoveryRequired))?
                        .manifest
                        .recovery_parent_path,
                );
                let parent_fd = open_directory_nofollow(&parent_path)
                    .map_err(|_| ServiceError::Store(StoreError::RecoveryRequired))?;
                let expected = intent.descriptor().recovery_parent_identity;
                let parent = RecoveryParent::pin(
                    &parent_fd,
                    (expected.device, expected.inode),
                    &parent_path,
                )
                .map_err(|_| ServiceError::Store(StoreError::RecoveryRequired))?;
                Some(
                    owner
                        .recover_d27_session(&root, &config.project_id, &config.session_id, &parent)
                        .map_err(|_| ServiceError::Store(StoreError::RecoveryRequired))?
                        .0,
                )
            } else {
                None
            }
        } else {
            None
        };
        #[cfg(not(target_os = "linux"))]
        let recovered: Option<Writer> = None;
        let store = if let Some(store) = recovered {
            store
        } else {
            root.open_or_create(
                config.project_id,
                config.session_id.clone(),
                InitialState {
                    draft: Draft {
                        draft_revision: DecimalU64::new(0),
                        text: String::new(),
                        attachment_ids: vec![],
                    },
                    configuration,
                    policy_revision: DecimalU64::new(0),
                },
            )?
        };
        let saved = store.snapshot()?;
        let admission = session_admission::AdmissionOwner::open(
            &root,
            &config.session_id,
            saved.marker.session_revision,
            saved
                .state
                .goal
                .as_ref()
                .map_or(DecimalU64::new(0), |goal| goal.revision),
        )?;
        let mut service = Self::from_production_store(store, config.session_id)?;
        service
            .engine
            .as_mut()
            .ok_or(ServiceError::Worker)?
            .admission = Some(admission);
        service.engine.as_mut().ok_or(ServiceError::Worker)?.root = Some(root);
        service
            .engine
            .as_mut()
            .ok_or(ServiceError::Worker)?
            .restore_admission_lanes()?;
        #[cfg(target_os = "linux")]
        if let Some((path, identity)) = source {
            let unresolved = polaris_core::apply_owner::ApplyOwnerV1::acquire(&path)
                .map_err(|_| ServiceError::Store(StoreError::RecoveryRequired))?
                .active()
                .map_err(|_| ServiceError::Store(StoreError::RecoveryRequired))?
                .is_some();
            if unresolved {
                // Keep saved inspection available. Missing management binding
                // makes the existing production input guard refuse new work.
                let engine = service.engine.as_mut().ok_or(ServiceError::Worker)?;
                engine.management_source_path = Some(path.clone());
                engine.management_project = Some(path);
                engine.management_identity = Some(identity);
            } else {
                service = service.with_management_project(path, identity)?;
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = source;
        Ok(service)
    }
    fn from_production_store(store: Writer, session: SessionId) -> Result<Self, ServiceError> {
        let mut engine = Engine::from_store(
            store,
            Options {
                script: vec![],
                ..Options::default()
            },
            session,
        );
        engine.production = true;
        engine.epoch = EngineEpoch::new(engine.epoch.as_str().replacen("fake-", "desktop-", 1))
            .map_err(|_| ServiceError::Options)?;
        Ok(Self {
            engine: Some(engine),
        })
    }
    pub async fn serve<R, W>(&mut self, reader: R, writer: W) -> Result<Exit, ServiceError>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        serve_engine(&mut self.engine, reader, writer).await
    }
}

/// rootの寿命は呼出側が保持できる。reopenは論理engine再起動であり操作を再実行しない。
pub struct FakeService {
    engine: Option<Engine>,
}

impl FakeService {
    pub fn create(root: &PrototypeRoot, options: Options) -> Result<Self, ServiceError> {
        Ok(Self {
            engine: Some(Engine::create(root, options)?),
        })
    }
    pub fn reopen(root: &PrototypeRoot, options: Options) -> Result<Self, ServiceError> {
        Ok(Self {
            engine: Some(Engine::reopen(root, options)?),
        })
    }
    /// 通信reactorと保存所有者を分離する。保存完了を待ちながらEOF/停止を観測する。
    /// blocking保存はabortせず結果を回収する。未知の保存結果にreadyを返さない。
    pub async fn serve<R, W>(&mut self, reader: R, writer: W) -> Result<Exit, ServiceError>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        serve_engine(&mut self.engine, reader, writer).await
    }
}

async fn serve_engine<R, W>(
    slot: &mut Option<Engine>,
    reader: R,
    writer: W,
) -> Result<Exit, ServiceError>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let mut engine = slot.take().ok_or(ServiceError::Options)?;
    if engine.served {
        *slot = Some(engine);
        return Err(ServiceError::Options);
    }
    engine.served = true;
    let deadline = engine.options.output_deadline;
    let step_interval = engine.options.step_interval;
    let (out, mut input, mut workers) = transport::workers(reader, writer, deadline);
    let stopped = engine.disconnected.clone();
    let stop_worker = stopped.clone();
    let (commands, mut incoming) = tokio::sync::mpsc::channel::<Command>(1);
    let (finished, mut completed) = tokio::sync::mpsc::channel(1);
    let owner = tokio::task::spawn_blocking(move || {
        while let Some(command) = incoming.blocking_recv() {
            if stop_worker.load(Ordering::Acquire) {
                break;
            }
            #[cfg(all(test, unix))]
            if engine.pause.as_ref().is_some_and(|pause| {
                matches!(&command,
                    Command::Request(r) if r.request_id.as_str() == pause.request_id)
            }) {
                let pause = engine.pause.take().unwrap();
                let _ = pause.entered.send(());
                let _ = pause.release.blocking_recv();
                if stop_worker.load(Ordering::Acquire) {
                    break;
                }
            }
            let mut ready = false;
            let mut settle_soon = false;
            let result = match command {
                Command::Request(request)
                    if matches!(request.body, RequestBody::LocalModels(..)) =>
                {
                    match engine.start_local_models(&request) {
                        Ok(()) => Ok(()),
                        Err(error) => Response::for_request(&request, Err(error))
                            .map_err(|_| ServiceError::Worker)
                            .and_then(|response| out.send(&response)),
                    }
                }
                Command::Request(request)
                    if matches!(
                        request.body,
                        RequestBody::McpCatalogCheck(..)
                            | RequestBody::McpSetupDetect(..)
                            | RequestBody::McpSetupRegister(..)
                    ) =>
                {
                    #[cfg(target_os = "macos")]
                    let outcome = if matches!(request.body, RequestBody::McpCatalogCheck(..)) {
                        engine
                            .start_mcp_catalog(&request)
                            .map(|result| result.map(SuccessResult::McpCatalogCheck))
                    } else {
                        engine.start_mcp_setup(&request).map(|()| None)
                    };
                    #[cfg(not(target_os = "macos"))]
                    let outcome = Err(error(ErrorCode::CapabilityUnavailable));
                    match outcome {
                        Ok(None) => Ok(()),
                        Ok(Some(result)) => Response::for_request(&request, Ok(result))
                            .map_err(|_| ServiceError::Worker)
                            .and_then(|response| out.send(&response)),
                        Err(error) => Response::for_request(&request, Err(error))
                            .map_err(|_| ServiceError::Worker)
                            .and_then(|response| out.send(&response)),
                    }
                }
                Command::Request(request) => {
                    let mut events = Vec::new();
                    let outcome = engine.handle(&request, &mut events);
                    if outcome.as_ref().is_err_and(|e| {
                        matches!(
                            e.code,
                            ErrorCode::StorageFailed | ErrorCode::RecoveryRequired
                        )
                    }) {
                        engine.failed = true;
                    }
                    ready = matches!(
                        &outcome,
                        Ok(SuccessResult::ShutdownRequest(ShutdownResult {
                            state: ShutdownState::Ready,
                            ..
                        }))
                    );
                    settle_soon = outcome.is_ok()
                        && !ready
                        && matches!(
                            request.body,
                            RequestBody::RunCancel(..)
                                | RequestBody::InputCancel(..)
                                | RequestBody::ShutdownRequest(..)
                        );
                    if stop_worker.load(Ordering::Acquire) {
                        break;
                    }
                    Response::for_request(&request, outcome)
                        .map_err(|_| ServiceError::Worker)
                        .and_then(|response| out.send(&response))
                        .and_then(|()| engine.emit_all(events, &out))
                }
                Command::Step => {
                    if engine
                        .history
                        .as_ref()
                        .is_some_and(|h| Instant::now() >= h.expires)
                    {
                        engine.history = None;
                    }
                    if engine.draining {
                        engine.settle(&out)
                    } else {
                        engine.step(&out)
                    }
                }
            };
            let failed = result.is_err();
            if finished
                .blocking_send((ready, settle_soon, result))
                .is_err()
                || failed
            {
                break;
            }
        }
        engine.draining = true;
        if let Some(active) = &mut engine.active {
            active.cancel();
        }
        // The transport may have dropped its JoinHandle. Production must keep
        // this existing owner alive until join, native cleanup and persistence
        // finish; returning an unfinished Engine would then silently drop it.
        let fake_deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut failure_backoff = step_interval;
        let settled = loop {
            let settled = engine.settle_without_output();
            let failed_without_owned_work = settled.is_err()
                && engine.active.is_none()
                && engine.cleanups.is_empty()
                && !engine.source_unsettled()
                && engine.local_models.is_none()
                && engine.mcp_catalog_idle();
            if engine.ready
                || failed_without_owned_work
                || (!engine.production
                    && (settled.is_err() || std::time::Instant::now() >= fake_deadline))
            {
                break settled;
            }
            let delay = if settled.is_err() {
                // Retain failed persistence in this one owner, without replaying
                // disk writes or calling Writer::recover on an active run. Later
                // iterations only cancel/poll core and the independent cleanup
                // owner. No detached reaper or success/ready is manufactured.
                engine.failed = true;
                failure_backoff = failure_backoff
                    .saturating_mul(2)
                    .min(Duration::from_secs(1));
                failure_backoff
            } else {
                step_interval
            };
            std::thread::sleep(delay);
        };
        engine.subscription = None;
        engine.history = None;
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        engine.close_source_recovery();
        (engine, settled)
    });
    let mut dispatch = Dispatch::new(Instant::now(), step_interval);
    let mut pending = false;
    // reader8件+ここ8件+実行中1件。停止要求を先に処理し、満杯なら明示切断する。
    let mut queued = VecDeque::new();
    // future破棄時はchannelより先にstopを公開し、所有者の次の処理を禁止する。
    let _disconnect = DisconnectGuard(stopped.clone());
    let result = loop {
        tokio::select! {
            biased;
            done = workers.join_next(), if !workers.is_empty() => {
                match done {
                    Some(Ok(WorkerExit::Reader(Ok(())))) => break Ok(Exit::Eof),
                    Some(Ok(WorkerExit::Reader(Err(e)))) => break Err(e),
                    Some(Ok(WorkerExit::Writer(Err(_)))) => break Ok(Exit::OutputClosed),
                    _ => break Err(ServiceError::Worker),
                }
            }
            done = completed.recv(), if pending => {
                pending = false;
                match done {
                    Some((true, _, Ok(()))) => break Ok(Exit::Ready),
                    Some((false, stop, Ok(()))) => { dispatch.settle_soon = stop; },
                    Some((_, _, Err(error))) => break Err(error),
                    None => break Err(ServiceError::Worker),
                }
            }
            request = input.recv() => {
                let Some(request) = request else { continue; };
                if queued.len() == 8 { break Ok(Exit::OutputClosed); }
                if matches!(
                    request.body,
                    RequestBody::RunCancel(..)
                        | RequestBody::InputCancel(..)
                        | RequestBody::SubagentCancel(..)
                        | RequestBody::ShutdownRequest(..)
                ) {
                    queued.push_front(request);
                } else { queued.push_back(request); }
            }
            _ = sleep_until(dispatch.next_step), if !pending => {}

        }
        if !pending {
            let command = dispatch.next(Instant::now(), &mut queued);
            if let Some(command) = command {
                if commands.try_send(command).is_err() {
                    break Err(ServiceError::Worker);
                }
                pending = true;
            }
        }
    };
    stopped.store(true, Ordering::Release);
    drop(commands);
    // 通信不能時はwriterを先に閉じる。保存所有者だけは終端を回収する。
    if !matches!(result, Ok(Exit::Ready)) {
        workers.abort_all();
    }
    drop(completed);
    let (engine, settled) = owner.await.map_err(|_| ServiceError::Worker)?;
    *slot = Some(engine);
    settled?;
    if matches!(result, Ok(Exit::Ready)) {
        let flushed = timeout(deadline, async {
            while let Some(done) = workers.join_next().await {
                match done {
                    Ok(WorkerExit::Reader(_)) => {}
                    Ok(WorkerExit::Writer(result)) => return result,
                    Err(_) => return Err(ServiceError::Worker),
                }
            }
            Err(ServiceError::Worker)
        })
        .await;
        return match flushed {
            Ok(Ok(())) => Ok(Exit::Ready),
            _ => Ok(Exit::OutputClosed),
        };
    }
    match result {
        Err(ServiceError::OutputClosed) => Ok(Exit::OutputClosed),
        other => other,
    }
}

enum Command {
    Request(Request),
    Step,
}

// The owner invokes one command at a time. Keep selection independent of wall
// clock sleeps so tests can supply the exact completion time of a slow Step.
struct Dispatch {
    next_step: Instant,
    interval: Duration,
    settle_soon: bool,
    last_was_step: bool,
}
impl Dispatch {
    fn new(now: Instant, interval: Duration) -> Self {
        Self {
            next_step: now + interval,
            interval,
            settle_soon: false,
            last_was_step: false,
        }
    }

    fn next(&mut self, now: Instant, queued: &mut VecDeque<Request>) -> Option<Command> {
        let control_waiting = queued.front().is_some_and(|request| {
            matches!(
                request.body,
                RequestBody::RunCancel(..)
                    | RequestBody::InputCancel(..)
                    | RequestBody::SubagentCancel(..)
                    | RequestBody::ShutdownRequest(..)
            )
        });
        let command = if !self.settle_soon && control_waiting {
            queued.pop_front().map(Command::Request)
        } else if self.settle_soon
            || (now >= self.next_step && (!self.last_was_step || queued.is_empty()))
        {
            self.settle_soon = false;
            self.next_step = now + self.interval;
            Some(Command::Step)
        } else {
            // A Step can finish after its own next deadline. Give a waiting
            // request one turn without moving that deadline, then run the due
            // Step. Neither normal requests nor Steps can starve the other.
            queued.pop_front().map(Command::Request)
        };
        if let Some(command) = &command {
            self.last_was_step = matches!(command, Command::Step);
        }
        command
    }
}

struct DisconnectGuard(Arc<AtomicBool>);
impl Drop for DisconnectGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

struct Engine {
    #[cfg(target_os = "macos")]
    workspace_reader: Option<crate::WorkspaceViewReader>,
    local_models: Option<local_models::InventoryJob>,
    inventory_generation: u64,
    store: Writer,
    session: SessionId,
    project: ProjectId,
    root: Option<DesktopRoot>,
    other_lane: Option<SessionLane>,
    routing_jobs: Vec<RoutingJob>,
    production: bool,
    #[cfg(unix)]
    management_source_path: Option<PathBuf>,
    #[cfg(unix)]
    management_project: Option<PathBuf>,
    #[cfg(unix)]
    management_identity: Option<(u64, u64)>,
    #[cfg(unix)]
    management_owner: Option<polaris_core::project_mutation::ProjectMutationOwnerV1>,
    #[cfg(unix)]
    management_effects: Option<polaris_core::management_effect::ManagementEffectStoreV1>,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    trusted_runs: Option<real::TrustedRuns>,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    source: Option<source_owner::SourceOwner>,
    #[cfg(target_os = "linux")]
    saved_source_completed: Vec<polaris_core::desktop_store::SavedSourceApply>,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    source_recovery: Option<source_recovery::Ingress>,
    options: Options,
    epoch: EngineEpoch,
    connection: ConnectionState,
    wire_version: Option<ProtocolVersion>,
    client: Option<ClientId>,
    subscription: Option<Position>,
    history: Option<History>,
    details: Option<run_details::Details>,
    serial: u64,
    active: Option<Active>,
    admission: Option<session_admission::AdmissionOwner>,
    draining: bool,
    ready: bool,
    failed: bool,
    served: bool,
    disconnected: Arc<AtomicBool>,
    cleanups: crate::execution::CleanupOwner,
    #[cfg(target_os = "macos")]
    mcp_settings: Option<mcp_settings::McpSettings>,
    #[cfg(target_os = "macos")]
    mcp_catalog: Option<mcp_catalog::CatalogJob>,
    #[cfg(target_os = "macos")]
    mcp_setup: mcp_setup::McpSetupState,
    #[cfg(all(test, unix))]
    pause: Option<tests::Pause>,
}

impl Engine {
    pub fn create(root: &PrototypeRoot, options: Options) -> Result<Self, ServiceError> {
        options.validate()?;
        let store = root.create(
            project_id(),
            session_id(),
            InitialState {
                draft: Draft {
                    draft_revision: DecimalU64::new(0),
                    text: String::new(),
                    attachment_ids: vec![],
                },
                configuration: Configuration {
                    history_mode: Default::default(),
                    configuration_revision: DecimalU64::new(0),
                    provider: "fake".into(),
                    model: "scripted".into(),
                    effort: "none".into(),
                },
                policy_revision: DecimalU64::new(0),
            },
        )?;
        Ok(Self::from_store(store, options, session_id()))
    }
    pub fn reopen(root: &PrototypeRoot, options: Options) -> Result<Self, ServiceError> {
        options.validate()?;
        Ok(Self::from_store(
            root.open(&project_id(), &session_id())?,
            options,
            session_id(),
        ))
    }
    fn from_store(store: Writer, options: Options, session: SessionId) -> Self {
        let serial = EPOCH_SERIAL.fetch_add(1, Ordering::Relaxed);
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let project = store.coordinates().0.clone();
        Self {
            store,
            session,
            project,
            root: None,
            other_lane: None,
            routing_jobs: Vec::new(),
            production: false,
            #[cfg(unix)]
            management_source_path: None,
            #[cfg(unix)]
            management_project: None,
            #[cfg(unix)]
            management_identity: None,
            #[cfg(unix)]
            management_owner: None,
            #[cfg(unix)]
            management_effects: None,
            #[cfg(target_os = "macos")]
            workspace_reader: None,
            local_models: None,
            inventory_generation: 0,
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            trusted_runs: None,
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            source: None,
            #[cfg(target_os = "linux")]
            saved_source_completed: Vec::new(),
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            source_recovery: None,
            options,
            disconnected: Arc::new(AtomicBool::new(false)),
            epoch: EngineEpoch::new(format!("fake-{}-{time}-{serial}", std::process::id()))
                .unwrap(),
            connection: ConnectionState::AwaitingHello,
            wire_version: None,
            client: None,
            subscription: None,
            history: None,
            details: None,
            serial: 0,
            active: None,
            admission: None,
            draining: false,
            ready: false,
            failed: false,
            served: false,
            cleanups: Default::default(),
            #[cfg(target_os = "macos")]
            mcp_settings: None,
            #[cfg(target_os = "macos")]
            mcp_catalog: None,
            #[cfg(target_os = "macos")]
            mcp_setup: Default::default(),
            #[cfg(all(test, unix))]
            pause: None,
        }
    }
    fn source_enabled(&self) -> bool {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            self.source.is_some()
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            false
        }
    }
    fn source_unsettled(&self) -> bool {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            self.source
                .as_ref()
                .is_some_and(|source| source.unsettled())
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            false
        }
    }
    fn poll_source(&mut self, stopping: bool) -> Result<(), ServiceError> {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        self.poll_source_recovery();
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if let Some(source) = &mut self.source {
            let result = source.poll(
                &mut self.store,
                stopping || self.draining || self.disconnected.load(Ordering::Acquire),
                self.failed,
                &self.disconnected,
            );
            if result.is_err() {
                self.failed = true;
            }
            return result;
        }
        let _ = stopping;
        Ok(())
    }
    fn resolve_source(&mut self, request: &Request) -> Result<SuccessResult, ProtocolError> {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if let Some(source) = &mut self.source {
            return source.resolve(&mut self.store, request, &self.draining, &self.disconnected);
        }
        let _ = request;
        Err(error(ErrorCode::CapabilityUnavailable))
    }
    fn real_enabled(&self) -> bool {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            self.production && self.trusted_runs.is_some()
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            false
        }
    }

    fn delegation_projection(&self) -> Option<polaris_core::session::RootDelegationProjectionV1> {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            self.active
                .as_ref()
                .and_then(|active| active.delegation.as_ref())
                .and_then(DelegationRuntime::projection)
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            None
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn delegation_for(
        &self,
        root_run_id: &RunId,
        root_attempt_id: &AttemptId,
    ) -> Result<
        (
            &DelegationRuntime,
            polaris_core::session::RootDelegationHostBindingV1,
        ),
        ProtocolError,
    > {
        let active = self
            .active
            .as_ref()
            .ok_or_else(|| error(ErrorCode::NotFound))?;
        if &active.target.run_id != root_run_id || &active.target.attempt_id != root_attempt_id {
            return Err(error(ErrorCode::PermissionDenied));
        }
        let runtime = active
            .delegation
            .as_ref()
            .ok_or_else(|| error(ErrorCode::CapabilityUnavailable))?;
        let projection = runtime
            .projection()
            .ok_or_else(|| error(ErrorCode::StorageFailed))?;
        if projection.session_id != self.session.as_str()
            || projection.root_run_id.0 != root_run_id.as_str()
            || projection.root_attempt_id.0 != root_attempt_id.as_str()
        {
            return Err(error(ErrorCode::PermissionDenied));
        }
        let binding = polaris_core::session::RootDelegationHostBindingV1 {
            session_id: projection.session_id,
            root_run_id: projection.root_run_id,
            root_attempt_id: projection.root_attempt_id,
            active_task_id: projection.active_task_id,
        };
        Ok((runtime, binding))
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn emit_delegation_update(&mut self, out: &Outbox) -> Result<(), ServiceError> {
        let changed = self
            .active
            .as_ref()
            .and_then(|active| active.delegation.as_ref())
            .is_some_and(DelegationRuntime::take_update);
        if changed {
            let published = self.store.snapshot()?;
            self.emit(
                EventBody::OrchestrationUpdated(orchestration_summary(
                    &published,
                    self.admission.as_ref(),
                    self.delegation_projection().as_ref(),
                )),
                out,
            )?;
        }
        Ok(())
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn record_root_cancel_plan(&self, target: &RunTarget) -> Result<(), ProtocolError> {
        let Some(active) = self
            .active
            .as_ref()
            .filter(|active| active.target == *target)
        else {
            return Ok(());
        };
        let Some(runtime) = active.delegation.as_ref() else {
            return Ok(());
        };
        let projection = runtime
            .projection()
            .ok_or_else(|| error(ErrorCode::StorageFailed))?;
        if projection.session_id != self.session.as_str()
            || projection.root_run_id.0 != target.run_id.as_str()
            || projection.root_attempt_id.0 != target.attempt_id.as_str()
        {
            return Err(error(ErrorCode::PermissionDenied));
        }
        let expected_revision = projection.revision;
        runtime
            .request_root_cancel(
                polaris_core::session::RootDelegationHostBindingV1 {
                    session_id: projection.session_id,
                    root_run_id: projection.root_run_id,
                    root_attempt_id: projection.root_attempt_id,
                    active_task_id: projection.active_task_id,
                },
                expected_revision,
            )
            .map_err(delegation_control_error)?;
        Ok(())
    }
    fn id(&mut self, prefix: &str) -> String {
        self.serial += 1; // 有界な要求数に達する前に接続queueが閉じる。u64はwrapさせない。
        format!("{prefix}-{}-{}", self.epoch.as_str(), self.serial)
    }

    fn swap_lane(&mut self) -> Result<(), ProtocolError> {
        let mut other = self
            .other_lane
            .take()
            .ok_or_else(|| error(ErrorCode::PermissionDenied))?;
        std::mem::swap(&mut self.session, &mut other.session);
        std::mem::swap(&mut self.store, &mut other.store);
        std::mem::swap(&mut self.subscription, &mut other.subscription);
        std::mem::swap(&mut self.active, &mut other.active);
        std::mem::swap(&mut self.history, &mut other.history);
        std::mem::swap(&mut self.details, &mut other.details);
        std::mem::swap(&mut self.cleanups, &mut other.cleanups);
        self.other_lane = Some(other);
        Ok(())
    }

    fn select_lane(&mut self, session: &SessionId) -> Result<(), ProtocolError> {
        if self
            .admission
            .as_ref()
            .is_some_and(|owner| !owner.contains_session(session))
        {
            return Err(error(ErrorCode::PermissionDenied));
        }
        if session == &self.session {
            return Ok(());
        }
        if self
            .other_lane
            .as_ref()
            .is_some_and(|lane| &lane.session == session)
        {
            self.swap_lane()?;
            // A subscription belongs to the lane currently selected by this
            // connection.  The background lane keeps running durably, but its
            // old stream must not publish into the newly selected lane's
            // connection.  A later selection obtains a fresh subscription.
            if let Some(inactive) = &mut self.other_lane {
                inactive.subscription = None;
            }
            Ok(())
        } else {
            Err(error(ErrorCode::PermissionDenied))
        }
    }

    fn restore_admission_lanes(&mut self) -> Result<(), ServiceError> {
        if let Some(sidechat) = self
            .admission
            .as_ref()
            .and_then(session_admission::AdmissionOwner::sidechat_session)
        {
            let sidechat = SessionId::new(sidechat.as_str()).map_err(|_| ServiceError::Worker)?;
            let root = self.root.as_ref().ok_or(ServiceError::Worker)?;
            let current = self.store.snapshot()?;
            // The manager commit deliberately precedes creation of the empty
            // destination. A crash in that window is recovered by creating
            // exactly that manager-owned session, without copying any parent
            // conversation, memory, approval, goal, workflow, or tool state.
            let store = root.open_or_create(
                self.project.clone(),
                sidechat.clone(),
                InitialState {
                    draft: Draft {
                        draft_revision: DecimalU64::new(0),
                        text: String::new(),
                        attachment_ids: Vec::new(),
                    },
                    configuration: current.state.configuration,
                    policy_revision: current.state.policy_revision,
                },
            )?;
            self.other_lane = Some(SessionLane {
                session: sidechat,
                store,
                subscription: None,
                active: None,
                history: None,
                details: None,
                cleanups: Default::default(),
            });
        }
        // A Reserved receipt proves that classification was never dispatched.
        // Reconcile its parent writer, then resume that one pre-dispatch window
        // or choose the persisted context-changed fallback.
        let reserved = self
            .admission
            .as_ref()
            .map(session_admission::AdmissionOwner::recovery_admissions)
            .unwrap_or_default()
            .into_iter()
            .filter(|admission| {
                matches!(
                    admission.state,
                    polaris_core::session_manager::AdmissionState::Reserved
                )
            })
            .collect::<Vec<_>>();
        for admission in reserved {
            let source = self.session.clone();
            let parent = SessionId::new(admission.parent_session.as_str())
                .map_err(|_| ServiceError::Worker)?;
            self.select_lane(&parent)
                .map_err(|_| ServiceError::Worker)?;
            let published = self.store.snapshot()?;
            let goal_revision = published
                .state
                .goal
                .as_ref()
                .map_or(DecimalU64::new(0), |goal| goal.revision);
            self.admission
                .as_mut()
                .ok_or(ServiceError::Worker)?
                .observe_session(
                    &polaris_core::session_manager::SessionId::new(parent.as_str())
                        .map_err(|_| ServiceError::Worker)?,
                    published.marker.session_revision,
                    goal_revision,
                )?;
            let context_unchanged = published.marker.session_revision.get()
                == admission.payload.expected_session_revision
                && goal_revision.get() == admission.payload.expected_goal_revision;
            if context_unchanged {
                let classify = self
                    .admission
                    .as_mut()
                    .ok_or(ServiceError::Worker)?
                    .resume_reserved(&admission.id)?;
                if classify {
                    self.start_routing(admission.id.clone())
                        .map_err(|_| ServiceError::Worker)?;
                }
            } else {
                self.admission
                    .as_mut()
                    .ok_or(ServiceError::Worker)?
                    .context_changed(&admission.id)?;
            }
            self.select_lane(&source)
                .map_err(|_| ServiceError::Worker)?;
        }
        let admissions = self
            .admission
            .as_ref()
            .map(session_admission::AdmissionOwner::recovery_admissions)
            .unwrap_or_default();
        for admission in admissions {
            use polaris_core::session_manager::AdmissionState;
            let (destination, recover_append, running) = match &admission.state {
                AdmissionState::DestinationReady { session_id, .. }
                | AdmissionState::InputAppended { session_id } => {
                    (Some(session_id.clone()), true, false)
                }
                AdmissionState::RunQueued { session_id, .. } => {
                    (Some(session_id.clone()), false, false)
                }
                AdmissionState::Running { session_id, .. } => {
                    (Some(session_id.clone()), false, true)
                }
                _ => (None, false, false),
            };
            let Some(destination) = destination else {
                continue;
            };
            let destination =
                SessionId::new(destination.as_str()).map_err(|_| ServiceError::Worker)?;
            let source = self.session.clone();
            self.select_lane(&destination)
                .map_err(|_| ServiceError::Worker)?;
            let durable = self.store.admitted_run(admission.id.as_str())?;
            self.select_lane(&source)
                .map_err(|_| ServiceError::Worker)?;
            if recover_append {
                if durable.is_some() {
                    self.admission
                        .as_mut()
                        .ok_or(ServiceError::Worker)?
                        .recover_input_appended_and_queue(&admission.id)?;
                } else if matches!(admission.state, AdmissionState::DestinationReady { .. }) {
                    // No external send has happened yet. Finish the owner-local
                    // durable append exactly once from the persisted routing
                    // decision, then queue it normally.
                    self.append_admission(&admission.id)
                        .map_err(|_| ServiceError::Worker)?;
                } else {
                    return Err(StoreError::Corrupt("admission append missing").into());
                }
            } else if durable.is_none() {
                return Err(StoreError::Corrupt("admission run missing").into());
            } else if running {
                self.admission
                    .as_mut()
                    .ok_or(ServiceError::Worker)?
                    .terminal(
                        &admission.id,
                        polaris_core::session_manager::TerminalStatus::OutcomeUnknown,
                    )?;
            }
        }
        Ok(())
    }

    fn handle(
        &mut self,
        request: &Request,
        events: &mut Vec<EventBody>,
    ) -> Result<SuccessResult, ProtocolError> {
        if request.protocol_version == ProtocolVersion::V1 && request.body.requires_v2() {
            return Err(error(ErrorCode::UnsupportedVersion));
        }
        if self
            .wire_version
            .is_some_and(|version| version != request.protocol_version)
        {
            return Err(error(ErrorCode::UnsupportedVersion));
        }
        let gate = validate_request(self.connection, request)
            .map_err(|_| error(ErrorCode::InvalidRequest))?;
        if self
            .client
            .as_ref()
            .is_some_and(|client| client != &request.client_id)
            || matches!(&request.body, RequestBody::RequestStatus(_, p) if p.client_id != request.client_id)
        {
            return Err(error(ErrorCode::PermissionDenied));
        }
        #[cfg(target_os = "linux")]
        if matches!(request.body, RequestBody::WorkflowConfigure(..)) && !self.real_enabled() {
            return Err(error(ErrorCode::CapabilityUnavailable));
        }
        // The root owns at most two exact session lanes. Select the addressed
        // writer before reading its ledger; unrelated sessions still fail
        // without revealing whether a directory exists.
        if let Some(session) = request.body.session_id() {
            self.select_lane(session)?;
        }
        if matches!(request.body, RequestBody::Hello) {
            self.connection = gate.next_state;
            self.wire_version = Some(request.protocol_version);
            self.client = Some(request.client_id.clone());
            let mut capabilities = if self.production && !self.real_enabled() {
                vec![
                    Capability::SessionRead,
                    Capability::HistoryRead,
                    Capability::SourceApplyRead,
                    Capability::DraftUpdate,
                    Capability::SessionConfigure,
                    Capability::LocalModels,
                    Capability::RequestStatus,
                    Capability::Shutdown,
                    Capability::RunDetailsRead,
                ]
            } else {
                let mut capabilities = vec![
                    Capability::SessionRead,
                    Capability::HistoryRead,
                    Capability::SourceApplyRead,
                    Capability::DraftUpdate,
                    Capability::SessionConfigure,
                    Capability::RunStart,
                    Capability::RunCancel,
                    Capability::ApprovalResolve,
                    Capability::RequestStatus,
                    Capability::Shutdown,
                    Capability::RunDetailsRead,
                ];
                #[cfg(target_os = "macos")]
                if self.workspace_reader.is_some() {
                    capabilities.extend([Capability::WorkspaceRead, Capability::AttachmentRead]);
                }
                if self.source_enabled() {
                    capabilities.push(Capability::SourceApplyResolve);
                }
                if self.real_enabled() {
                    capabilities.push(Capability::WorkflowConfigure);
                    capabilities.push(Capability::LocalModels);
                    capabilities.push(Capability::RoleConfigure);
                }
                if self.production {
                    capabilities.retain(|c| *c != Capability::SessionConfigure);
                }
                #[cfg(target_os = "macos")]
                if self.mcp_settings.is_some() {
                    capabilities.push(Capability::McpSettings);
                }
                capabilities
            };
            if request.protocol_version == ProtocolVersion::V2 && self.admission.is_some() {
                capabilities.extend([Capability::SessionManage, Capability::InputCancel]);
            }
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            if request.protocol_version == ProtocolVersion::V2 && self.real_enabled() {
                capabilities.push(Capability::SubagentControl);
            }
            return Ok(SuccessResult::Hello(Hello {
                protocol_version: request.protocol_version,
                engine_epoch: self.epoch.clone(),
                capabilities,
                limits: Limits::default(),
            }));
        }
        if self.production
            && ((self.real_enabled() && matches!(request.body, RequestBody::SessionConfigure(..)))
                || (!self.real_enabled()
                    && matches!(
                        request.body,
                        RequestBody::RunStart(..)
                            | RequestBody::RunCancel(..)
                            | RequestBody::ApprovalResolve(..)
                    )))
        {
            return Err(error(ErrorCode::CapabilityUnavailable));
        }
        #[cfg(target_os = "macos")]
        if self.mcp_catalog.is_some() && matches!(request.body, RequestBody::RunStart(..)) {
            return Err(error(ErrorCode::SessionBusy));
        }
        if matches!(request.body, RequestBody::SourceApplyResolve(..)) && !self.source_enabled() {
            return Err(error(ErrorCode::CapabilityUnavailable));
        }
        if self.failed {
            return Err(error(ErrorCode::StorageFailed));
        }
        if self.draining
            && matches!(
                request.body,
                RequestBody::RunStart(..)
                    | RequestBody::DraftUpdate(..)
                    | RequestBody::SessionConfigure(..)
                    | RequestBody::WorkflowConfigure(..)
                    | RequestBody::GoalUpdate(..)
                    | RequestBody::InstructionSend(..)
            )
        {
            return Err(error(ErrorCode::SessionBusy));
        }
        match &request.body {
            RequestBody::Hello => unreachable!(),
            RequestBody::WorkspaceRead(_, params) => {
                #[cfg(target_os = "macos")]
                {
                    if self.draining || self.active.is_some() || params.selected_path.len() > 4096 {
                        return Err(error(ErrorCode::SessionBusy));
                    }
                    let reader = self
                        .workspace_reader
                        .as_ref()
                        .ok_or_else(|| error(ErrorCode::CapabilityUnavailable))?;
                    Ok(SuccessResult::WorkspaceRead(reader.read(
                        if params.selected_path.is_empty() {
                            None
                        } else {
                            Some(&params.selected_path)
                        },
                    )))
                }
                #[cfg(not(target_os = "macos"))]
                {
                    let _ = params;
                    Err(error(ErrorCode::CapabilityUnavailable))
                }
            }
            RequestBody::AttachmentRead(_, params) => {
                #[cfg(target_os = "macos")]
                {
                    if self.draining || self.active.is_some() || params.path.len() > 4096 {
                        return Err(error(ErrorCode::SessionBusy));
                    }
                    self.workspace_reader
                        .as_ref()
                        .ok_or_else(|| error(ErrorCode::CapabilityUnavailable))?;
                    Ok(SuccessResult::AttachmentRead(
                        crate::WorkspaceViewReader::read_attachment(std::path::Path::new(
                            &params.path,
                        )),
                    ))
                }
                #[cfg(not(target_os = "macos"))]
                {
                    let _ = params;
                    Err(error(ErrorCode::CapabilityUnavailable))
                }
            }
            RequestBody::LocalModels(..) => Err(error(ErrorCode::CapabilityUnavailable)),
            RequestBody::SessionRolesConfigure(..) => self.configure_role_bindings(request, events),
            RequestBody::SessionOpen(_) => {
                Ok(SuccessResult::SessionOpen(Box::new(self.snapshot(false)?)))
            }
            RequestBody::SessionList => self.session_list(),
            // Selection is also the atomic subscription hand-off for this
            // connection. The returned position is the only stream identity
            // valid after the old lane subscription was invalidated.
            RequestBody::SessionSelect(_) => {
                Ok(SuccessResult::SessionSelect(Box::new(self.snapshot(true)?)))
            }
            RequestBody::InputStatus(_, params) => self.input_status(&params.admission_id),
            RequestBody::InputSubmit(_, params) => self.input_submit(request, params, events),
            RequestBody::InputCancel(_, params) => self.input_cancel(params, events),
            RequestBody::SubagentWait(_, params) => self.subagent_wait(params),
            RequestBody::SubagentCancel(_, params) => self.subagent_cancel(params, events),
            RequestBody::SessionSnapshot(_) => Ok(SuccessResult::SessionSnapshot(Box::new(
                self.snapshot(false)?,
            ))),
            RequestBody::SessionSubscribe(_, params) => {
                if let Some(resume) = &params.resume
                    && (resume.engine_epoch != self.epoch
                        || self.subscription.as_ref().is_none_or(|p| {
                            p.subscription_id != resume.subscription_id
                                || resume.event_seq > p.event_seq
                        }))
                {
                    return Err(error(ErrorCode::RevisionConflict));
                }
                Ok(SuccessResult::SessionSubscribe(Box::new(
                    self.snapshot(true)?,
                )))
            }
            RequestBody::HistoryPage(_, params) => {
                Ok(SuccessResult::HistoryPage(self.history_page(params)?))
            }
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            RequestBody::WorkflowConfigure(_, params) => self.configure_workflow(params, events),
            #[cfg(not(any(target_os = "macos", target_os = "linux")))]
            RequestBody::WorkflowConfigure(..) => Err(error(ErrorCode::CapabilityUnavailable)),
            RequestBody::SourceApplyList(_, params) => self.source_list(params),
            RequestBody::SourceApplyPage(_, params) => self.source_page(params),
            RequestBody::ApprovalResolve(..) => self.resolve_approval(request, events),
            RequestBody::SourceApplyResolve(..) => self.resolve_source(request),
            RequestBody::RequestStatus(session, params) => {
                let accepted = self
                    .store
                    .request_status(session, &params.client_id, &params.request_id)
                    .map_err(store_error)?;
                Ok(SuccessResult::RequestStatus(status(accepted)))
            }
            RequestBody::ShutdownRequest(params) => {
                if params.engine_epoch != self.epoch {
                    return Err(error(ErrorCode::RevisionConflict));
                }
                self.draining = true;
                self.cleanups.step();
                self.ready &= self.active.is_none()
                    && self.cleanups.is_empty()
                    && !self.source_unsettled()
                    && self.local_models.is_none()
                    && self.mcp_catalog_idle();
                if let Some(active) = &mut self.active {
                    active.cancel();
                }
                Ok(SuccessResult::ShutdownRequest(ShutdownResult {
                    engine_epoch: self.epoch.clone(),
                    state: if self.ready {
                        ShutdownState::Ready
                    } else {
                        ShutdownState::Draining
                    },
                }))
            }
            RequestBody::McpCatalogCheck(..)
            | RequestBody::McpSetupDetect(..)
            | RequestBody::McpSetupRegister(..) => Err(error(ErrorCode::InvalidRequest)),
            RequestBody::RunDetails(_, params) => {
                Ok(SuccessResult::RunDetails(self.run_details(params)?))
            }
            RequestBody::BrowseList(..) | RequestBody::BrowseRead(..) => {
                Err(error(ErrorCode::CapabilityUnavailable))
            }
            RequestBody::McpCatalogCancel(_, target) => {
                #[cfg(target_os = "macos")]
                {
                    if self.mcp_settings.is_none() {
                        return Err(error(ErrorCode::CapabilityUnavailable));
                    }
                    Ok(SuccessResult::McpCatalogCancel(
                        self.cancel_mcp_catalog(target),
                    ))
                }
                #[cfg(not(target_os = "macos"))]
                {
                    let _ = target;
                    Err(error(ErrorCode::CapabilityUnavailable))
                }
            }
            RequestBody::McpRegistryRead(_)
            | RequestBody::McpRegistrySave(..)
            | RequestBody::McpSecretSet(..)
            | RequestBody::McpSecretDelete(..) => {
                #[cfg(target_os = "macos")]
                {
                    self.mcp_settings_request(request)
                }
                #[cfg(not(target_os = "macos"))]
                {
                    Err(error(ErrorCode::CapabilityUnavailable))
                }
            }
            _ => self.mutate(request, events),
        }
    }

    fn session_list(&self) -> Result<SuccessResult, ProtocolError> {
        let owner = self
            .admission
            .as_ref()
            .ok_or_else(|| error(ErrorCode::CapabilityUnavailable))?;
        let mut result = owner.session_list().map_err(store_error)?;
        if let Some(active) = &self.active
            && let Some(entry) = result
                .sessions
                .iter_mut()
                .find(|entry| entry.session_id == self.session)
        {
            entry.active_run = Some(active.target.clone());
        }
        if let Some(lane) = &self.other_lane
            && let Some(active) = &lane.active
            && let Some(entry) = result
                .sessions
                .iter_mut()
                .find(|entry| entry.session_id == lane.session)
        {
            entry.active_run = Some(active.target.clone());
        }
        Ok(SuccessResult::SessionList(result))
    }

    fn input_status(&self, admission: &AdmissionId) -> Result<SuccessResult, ProtocolError> {
        let owner = self
            .admission
            .as_ref()
            .ok_or_else(|| error(ErrorCode::CapabilityUnavailable))?;
        let mut result = owner.input_status(admission).map_err(store_error)?;
        self.overlay_admission_status(&mut result)?;
        Ok(SuccessResult::InputStatus(result))
    }

    fn subagent_wait(
        &self,
        params: &polaris_desktop_protocol::request::SubagentWait,
    ) -> Result<SuccessResult, ProtocolError> {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            let (runtime, binding) =
                self.delegation_for(&params.root_run_id, &params.root_attempt_id)?;
            let projected_root_run_id = binding.root_run_id.0.clone();
            let projected_root_attempt_id = binding.root_attempt_id.0.clone();
            let waited = runtime
                .wait_children(
                    binding,
                    params.expected_revision.get(),
                    params.after_cursor.map(DecimalU64::get),
                )
                .map_err(delegation_control_error)?;
            let (revision, cursor, state, child_total, children) = match waited {
                Some(snapshot) => {
                    let child_total = snapshot.children.len();
                    let children = snapshot
                        .children
                        .iter()
                        .take(32)
                        .filter_map(|child| {
                            child_summary(
                                &projected_root_run_id,
                                &projected_root_attempt_id,
                                snapshot.revision,
                                child,
                            )
                        })
                        .collect::<Vec<_>>();
                    (
                        snapshot.revision,
                        snapshot.cursor,
                        SubagentWaitStateV2::Updated,
                        child_total,
                        children,
                    )
                }
                None => {
                    let child_total = runtime
                        .projection()
                        .ok_or_else(|| error(ErrorCode::StorageFailed))?
                        .children
                        .len();
                    (
                        params.expected_revision.get(),
                        params.after_cursor.map_or(0, DecimalU64::get),
                        SubagentWaitStateV2::Unchanged,
                        child_total,
                        Vec::new(),
                    )
                }
            };
            Ok(SuccessResult::SubagentWait(SubagentWaitResult {
                root_run_id: params.root_run_id.clone(),
                root_attempt_id: params.root_attempt_id.clone(),
                revision: DecimalU64::new(revision),
                cursor: DecimalU64::new(cursor),
                state,
                child_total: DecimalU64::new(child_total as u64),
                child_omitted: children.len() != child_total,
                children,
            }))
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = params;
            Err(error(ErrorCode::CapabilityUnavailable))
        }
    }

    fn subagent_cancel(
        &mut self,
        params: &polaris_desktop_protocol::request::SubagentCancel,
        events: &mut Vec<EventBody>,
    ) -> Result<SuccessResult, ProtocolError> {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            let (runtime, binding) =
                self.delegation_for(&params.root_run_id, &params.root_attempt_id)?;
            let reference = polaris_core::subagent_control::ChildAttemptRefV1 {
                logical_child_id: polaris_core::subagent_control::LogicalChildId::new(
                    params.logical_child_id.as_str(),
                )
                .map_err(|_| error(ErrorCode::InvalidRequest))?,
                attempt_id: polaris_core::subagent_control::ChildAttemptId::new(
                    params.child_attempt_id.as_str(),
                )
                .map_err(|_| error(ErrorCode::InvalidRequest))?,
            };
            let ack = runtime
                .request_child_cancel(binding, params.expected_revision.get(), reference)
                .map_err(delegation_control_error)?;
            let (disposition, state, cursor) = match ack.result {
                polaris_core::subagent_control::CancelResultV1::Requested { cursor } => (
                    SubagentCancelDispositionV2::Requested,
                    polaris_desktop_protocol::orchestration::ChildControlStateV2::Cancelling,
                    cursor,
                ),
                polaris_core::subagent_control::CancelResultV1::AlreadyRequested { cursor } => (
                    SubagentCancelDispositionV2::AlreadyRequested,
                    polaris_desktop_protocol::orchestration::ChildControlStateV2::Cancelling,
                    cursor,
                ),
                polaris_core::subagent_control::CancelResultV1::AlreadyTerminal {
                    state,
                    cursor,
                } => (
                    SubagentCancelDispositionV2::AlreadyTerminal,
                    child_control_state(&state),
                    cursor,
                ),
            };
            let runtime_effect = match ack.runtime_effect {
                polaris_core::session::RuntimeCancelEffectV1::Unavailable => {
                    SubagentRuntimeEffectV2::Unavailable
                }
                polaris_core::session::RuntimeCancelEffectV1::Requested => {
                    SubagentRuntimeEffectV2::Requested
                }
            };
            let published = self.store.snapshot().map_err(store_error)?;
            events.push(EventBody::OrchestrationUpdated(orchestration_summary(
                &published,
                self.admission.as_ref(),
                self.delegation_projection().as_ref(),
            )));
            Ok(SuccessResult::SubagentCancel(SubagentCancelResult {
                root_run_id: params.root_run_id.clone(),
                root_attempt_id: params.root_attempt_id.clone(),
                logical_child_id: params.logical_child_id.clone(),
                child_attempt_id: params.child_attempt_id.clone(),
                revision: DecimalU64::new(ack.revision),
                cursor: DecimalU64::new(cursor),
                disposition,
                state,
                runtime_effect,
            }))
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = (params, events);
            Err(error(ErrorCode::CapabilityUnavailable))
        }
    }

    fn overlay_admission_status(
        &self,
        result: &mut InputStatusResult,
    ) -> Result<(), ProtocolError> {
        let internal =
            polaris_core::session_manager::AdmissionId::new(result.admission_id.as_str())
                .map_err(|_| error(ErrorCode::StorageFailed))?;
        self.overlay_admission_run(&internal, &mut result.run);
        let Some(destination) = result.destination_session_id.as_ref() else {
            return Ok(());
        };
        if destination == &self.session {
            result.session_revision = self
                .store
                .snapshot()
                .map_err(store_error)?
                .marker
                .session_revision;
        } else if let Some(lane) = self
            .other_lane
            .as_ref()
            .filter(|lane| &lane.session == destination)
        {
            result.session_revision = lane
                .store
                .snapshot()
                .map_err(store_error)?
                .marker
                .session_revision;
        }
        Ok(())
    }

    fn input_cancel(
        &mut self,
        params: &polaris_desktop_protocol::request::InputCancel,
        events: &mut Vec<EventBody>,
    ) -> Result<SuccessResult, ProtocolError> {
        let current = self.session.clone();
        let before = self
            .admission
            .as_ref()
            .ok_or_else(|| error(ErrorCode::CapabilityUnavailable))?
            .input_status(&params.admission_id)
            .map_err(store_error)?;
        if before.source_session_id != current
            && before.destination_session_id.as_ref() != Some(&current)
        {
            return Err(error(ErrorCode::PermissionDenied));
        }
        let internal =
            polaris_core::session_manager::AdmissionId::new(params.admission_id.as_str())
                .map_err(|_| error(ErrorCode::InvalidRequest))?;
        let mut result = self
            .admission
            .as_mut()
            .ok_or_else(|| error(ErrorCode::CapabilityUnavailable))?
            .cancel(&params.admission_id, params.expected_manager_revision)
            .map_err(store_error)?;
        if let Some(active) = self
            .active
            .as_mut()
            .filter(|active| active.admission.as_ref() == Some(&internal))
        {
            active.cancel();
        }
        if let Some(active) = self
            .other_lane
            .as_mut()
            .and_then(|lane| lane.active.as_mut())
            .filter(|active| active.admission.as_ref() == Some(&internal))
        {
            active.cancel();
        }
        if result.destination_session_id.is_none() {
            result.destination_session_id = before.destination_session_id.clone();
        }
        self.overlay_admission_status(&mut result)?;

        // Cancelling an unappended explicit-new admission can remove the
        // manager's unused sidechat slot. Keep the remaining managed lane
        // selected and stop exposing the detached writer by session ID.
        if self
            .admission
            .as_ref()
            .is_some_and(|owner| !owner.contains_session(&self.session))
        {
            self.swap_lane()?;
        }
        if self.other_lane.as_ref().is_some_and(|lane| {
            self.admission
                .as_ref()
                .is_some_and(|owner| !owner.contains_session(&lane.session))
                && lane.active.is_none()
        }) {
            self.other_lane = None;
        }
        let published = self.store.snapshot().map_err(store_error)?;
        events.push(EventBody::OrchestrationUpdated(orchestration_summary(
            &published,
            self.admission.as_ref(),
            self.delegation_projection().as_ref(),
        )));
        Ok(SuccessResult::InputCancel(result))
    }

    fn input_submit(
        &mut self,
        request: &Request,
        params: &polaris_desktop_protocol::request::InputSubmit,
        events: &mut Vec<EventBody>,
    ) -> Result<SuccessResult, ProtocolError> {
        let source = request
            .body
            .session_id()
            .cloned()
            .ok_or_else(|| error(ErrorCode::InvalidRequest))?;
        #[cfg(unix)]
        let _project_activity = self.input_management_guard()?;
        self.select_lane(&source)?;
        let (
            expected_manager_revision,
            expected_session_revision,
            expected_goal_revision,
            route,
            attachment_ids,
            admission_id,
            text,
        ) = match params {
            polaris_desktop_protocol::request::InputSubmit::Text {
                expected_manager_revision,
                expected_session_revision,
                expected_goal_revision,
                route,
                attachment_ids,
                admission_id,
                text,
            } => (
                *expected_manager_revision,
                *expected_session_revision,
                *expected_goal_revision,
                *route,
                attachment_ids.as_slice(),
                admission_id.as_ref(),
                text.as_str(),
            ),
            polaris_desktop_protocol::request::InputSubmit::Reference { .. } => {
                // The reference resolver has no owner API yet. Never turn a
                // missing immutable result into an empty or duplicate input.
                return Err(error(ErrorCode::ReferenceUnavailable));
            }
        };
        let source_snapshot = self.store.snapshot().map_err(store_error)?;
        let source_goal_revision = source_snapshot
            .state
            .goal
            .as_ref()
            .map_or(DecimalU64::new(0), |goal| goal.revision);
        let mut receipt_counts = std::collections::BTreeMap::new();
        receipt_counts.insert(
            polaris_core::session_manager::SessionId::new(source.as_str())
                .map_err(|_| error(ErrorCode::StorageFailed))?,
            source_snapshot
                .state
                .runs
                .iter()
                .filter(|run| run.admission.is_some())
                .count(),
        );
        if let Some(other) = self.other_lane.as_ref() {
            let saved = other.store.snapshot().map_err(store_error)?;
            receipt_counts.insert(
                polaris_core::session_manager::SessionId::new(other.session.as_str())
                    .map_err(|_| error(ErrorCode::StorageFailed))?,
                saved
                    .state
                    .runs
                    .iter()
                    .filter(|run| run.admission.is_some())
                    .count(),
            );
        }
        let (id, needs_classification) = self
            .admission
            .as_mut()
            .ok_or_else(|| error(ErrorCode::CapabilityUnavailable))?
            .submit_text(
                &source,
                expected_manager_revision,
                expected_session_revision,
                expected_goal_revision,
                source_snapshot.marker.session_revision,
                source_goal_revision,
                route,
                attachment_ids,
                admission_id,
                text,
                &receipt_counts,
            )
            .map_err(store_error)?;
        if needs_classification {
            self.start_routing(id.clone())?;
        }
        let ready = self
            .admission
            .as_ref()
            .ok_or_else(|| error(ErrorCode::CapabilityUnavailable))?
            .admission(&id)
            .map_err(store_error)?;
        if matches!(
            ready.state,
            polaris_core::session_manager::AdmissionState::DestinationReady { .. }
        ) {
            self.append_admission(&id)?;
        }
        self.pump_admissions()?;
        let mut submitted = self
            .admission
            .as_ref()
            .ok_or_else(|| error(ErrorCode::CapabilityUnavailable))?
            .submitted(&id)
            .map_err(store_error)?;
        self.overlay_admission_run(&id, &mut submitted.run);
        let published = self.store.snapshot().map_err(store_error)?;
        events.push(EventBody::OrchestrationUpdated(orchestration_summary(
            &published,
            self.admission.as_ref(),
            self.delegation_projection().as_ref(),
        )));
        Ok(SuccessResult::InputSubmit(submitted))
    }

    #[cfg(unix)]
    fn input_management_guard(
        &self,
    ) -> Result<Option<polaris_core::project_mutation::ProjectActivityLeaseV1>, ProtocolError> {
        use polaris_core::project_mutation::ProjectMutationError;

        if !self.production {
            return Ok(None);
        }
        let project = self
            .management_project
            .as_ref()
            .ok_or_else(|| error(ErrorCode::RecoveryRequired))?;
        let source_path = self
            .management_source_path
            .as_ref()
            .ok_or_else(|| error(ErrorCode::RecoveryRequired))?;
        let identity = self
            .management_identity
            .ok_or_else(|| error(ErrorCode::RecoveryRequired))?;
        let owner = self
            .management_owner
            .as_ref()
            .ok_or_else(|| error(ErrorCode::RecoveryRequired))?;
        let effects = self
            .management_effects
            .as_ref()
            .ok_or_else(|| error(ErrorCode::RecoveryRequired))?;
        if !management_project_matches(source_path, identity)
            || !management_project_matches(project, identity)
        {
            return Err(error(ErrorCode::RecoveryRequired));
        }
        let activity = owner.try_activity().map_err(|failure| match failure {
            ProjectMutationError::Busy => error(ErrorCode::SessionBusy),
            _ => error(ErrorCode::RecoveryRequired),
        })?;
        if !management_project_matches(source_path, identity)
            || !management_project_matches(project, identity)
        {
            return Err(error(ErrorCode::RecoveryRequired));
        }
        if polaris_core::sync::composite::C10CompositeBlockerV1::inspect_with_activity(
            project,
            effects.state_dir(),
            &activity,
        )
        .map_err(|_| error(ErrorCode::RecoveryRequired))?
        .is_some()
        {
            return Err(error(ErrorCode::RecoveryRequired));
        }
        Ok(Some(activity))
    }

    fn start_routing(
        &mut self,
        admission: polaris_core::session_manager::AdmissionId,
    ) -> Result<(), ProtocolError> {
        if self
            .routing_jobs
            .iter()
            .any(|job| job.admission == admission)
        {
            return Ok(());
        }
        let published = self.store.snapshot().map_err(store_error)?;
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let decider = self
            .trusted_runs
            .as_ref()
            .and_then(|runs| runs.factory.outside_run_decider(&published));
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let decider: Option<Arc<dyn polaris_provider::decision::Decider>> = None;
        let Some(decider) = decider else {
            self.admission
                .as_mut()
                .ok_or_else(|| error(ErrorCode::CapabilityUnavailable))?
                .finish_classification(&admission, None)
                .map_err(store_error)?;
            return Ok(());
        };
        let context = self.routing_context(&published)?;
        let current_input = self
            .admission
            .as_ref()
            .ok_or_else(|| error(ErrorCode::CapabilityUnavailable))?
            .admission(&admission)
            .map_err(store_error)?
            .payload
            .text;
        let worker = std::thread::Builder::new()
            .name("polaris-input-route".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .ok()?;
                runtime.block_on(polaris_core::session_manager::classify_context(
                    decider.as_ref(),
                    &context,
                    &current_input,
                ))
            });
        let worker = match worker {
            Ok(worker) => worker,
            Err(_) => {
                self.admission
                    .as_mut()
                    .ok_or_else(|| error(ErrorCode::CapabilityUnavailable))?
                    .finish_classification(&admission, None)
                    .map_err(store_error)?;
                return Ok(());
            }
        };
        self.routing_jobs.push(RoutingJob { admission, worker });
        Ok(())
    }

    fn routing_context(
        &self,
        published: &polaris_core::desktop_store::Published,
    ) -> Result<polaris_core::session_manager::RoutingContextV1, ProtocolError> {
        let goal = published
            .state
            .goal
            .as_ref()
            .filter(|goal| goal.active)
            .map_or_else(|| "none".to_owned(), |goal| goal.text.clone());
        let starts: Vec<_> = published
            .raw
            .iter()
            .filter(|event| event.starts_turn && event.message.role == polaris_provider::Role::User)
            .collect();
        let inherited = usize::try_from(published.state.inherited_turn_count.get())
            .map_err(|_| error(ErrorCode::StorageFailed))?;
        let mut turns = Vec::new();
        for (index, event) in starts.into_iter().enumerate().filter(|(index, _)| {
            *index < inherited
                || published
                    .state
                    .runs
                    .get(index.saturating_sub(inherited))
                    .is_some_and(|record| record.run.state.is_terminal())
        }) {
            let mut text = event.message.content.clone();
            if text.len() > polaris_core::session_manager::MAX_TURN_BYTES {
                let mut end = polaris_core::session_manager::MAX_TURN_BYTES;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
            }
            turns.push(
                polaris_core::session_manager::CompletedUserTurnV1::new(
                    format!("turn-{index}-{}", event.turn_id),
                    text,
                )
                .map_err(|_| error(ErrorCode::StorageFailed))?,
            );
        }
        polaris_core::session_manager::RoutingContextV1::new(goal, turns)
            .map_err(|_| error(ErrorCode::StorageFailed))
    }

    fn poll_routing(&mut self, events: &mut Vec<EventBody>) -> Result<(), ProtocolError> {
        let mut index = 0;
        while index < self.routing_jobs.len() {
            if !self.routing_jobs[index].worker.is_finished() {
                index += 1;
                continue;
            }
            let job = self.routing_jobs.swap_remove(index);
            let judgement = job.worker.join().ok().flatten();
            self.admission
                .as_mut()
                .ok_or_else(|| error(ErrorCode::CapabilityUnavailable))?
                .finish_classification(&job.admission, judgement)
                .map_err(store_error)?;
            self.append_admission(&job.admission)?;
            self.pump_admissions()?;
            let published = self.store.snapshot().map_err(store_error)?;
            events.push(EventBody::OrchestrationUpdated(orchestration_summary(
                &published,
                self.admission.as_ref(),
                self.delegation_projection().as_ref(),
            )));
        }
        Ok(())
    }

    fn ensure_admission_lane(
        &mut self,
        admission: &polaris_core::session_manager::AdmissionV1,
    ) -> Result<SessionId, ProtocolError> {
        let destination = match &admission.state {
            polaris_core::session_manager::AdmissionState::DestinationReady {
                session_id, ..
            } => {
                SessionId::new(session_id.as_str()).map_err(|_| error(ErrorCode::StorageFailed))?
            }
            _ => return Err(error(ErrorCode::RevisionConflict)),
        };
        if destination == self.session
            || self
                .other_lane
                .as_ref()
                .is_some_and(|lane| lane.session == destination)
        {
            return Ok(destination);
        }
        if self.other_lane.is_some() {
            return Err(error(ErrorCode::SessionBusy));
        }
        let root = self
            .root
            .as_ref()
            .ok_or_else(|| error(ErrorCode::CapabilityUnavailable))?;
        let current = self.store.snapshot().map_err(store_error)?;
        let store = root
            .create(
                self.project.clone(),
                destination.clone(),
                InitialState {
                    draft: Draft {
                        draft_revision: DecimalU64::new(0),
                        text: String::new(),
                        attachment_ids: Vec::new(),
                    },
                    configuration: current.state.configuration,
                    policy_revision: current.state.policy_revision,
                },
            )
            .map_err(store_error)?;
        self.other_lane = Some(SessionLane {
            session: destination.clone(),
            store,
            subscription: None,
            active: None,
            history: None,
            details: None,
            cleanups: Default::default(),
        });
        Ok(destination)
    }

    fn append_admission(
        &mut self,
        id: &polaris_core::session_manager::AdmissionId,
    ) -> Result<(), ProtocolError> {
        let mut admission = self
            .admission
            .as_ref()
            .ok_or_else(|| error(ErrorCode::CapabilityUnavailable))?
            .admission(id)
            .map_err(store_error)?;
        let source = self.session.clone();
        let parent = SessionId::new(admission.parent_session.as_str())
            .map_err(|_| error(ErrorCode::StorageFailed))?;
        self.select_lane(&parent)?;
        let parent_snapshot = self.store.snapshot().map_err(store_error)?;
        let parent_goal_revision = parent_snapshot
            .state
            .goal
            .as_ref()
            .map_or(0, |goal| goal.revision.get());
        if parent_snapshot.marker.session_revision.get()
            != admission.payload.expected_session_revision
            || parent_goal_revision != admission.payload.expected_goal_revision
        {
            self.admission
                .as_mut()
                .ok_or_else(|| error(ErrorCode::CapabilityUnavailable))?
                .context_changed(id)
                .map_err(store_error)?;
            admission = self
                .admission
                .as_ref()
                .ok_or_else(|| error(ErrorCode::CapabilityUnavailable))?
                .admission(id)
                .map_err(store_error)?;
        }
        let destination = self.ensure_admission_lane(&admission)?;
        self.select_lane(&destination)?;
        let target = admission_target(id);
        let existing = self.store.admitted_run(id.as_str()).map_err(store_error)?;
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let account = if existing.is_none() {
            self.trusted_runs
                .as_ref()
                .map(|runs| runs.factory.bind_account())
                .transpose()
                .map_err(|_| error(ErrorCode::CapabilityUnavailable))?
                .flatten()
        } else {
            None
        };
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let account = None;
        let appended = self
            .store
            .append_admitted_run(&admission, target, account)
            .map_err(store_error)?;
        if appended.newly_appended {
            self.admission
                .as_mut()
                .ok_or_else(|| error(ErrorCode::CapabilityUnavailable))?
                .input_appended_and_queue(id)
                .map_err(store_error)?;
        } else {
            // The writer's exact admission binding proves that a prior append
            // committed even if its manager acknowledgement was lost.
            self.admission
                .as_mut()
                .ok_or_else(|| error(ErrorCode::CapabilityUnavailable))?
                .recover_input_appended_and_queue(id)
                .map_err(store_error)?;
        }
        // A sidechat appends its run in a different writer, so that writer
        // cannot consume the source draft itself.  Once the destination run
        // and admission transition are both durable, consume only the exact
        // source draft that was admitted.  A later edit remains untouched.
        self.select_lane(&parent)?;
        self.consume_unchanged_admission_source_draft(&admission)?;
        self.select_lane(&source)?;
        Ok(())
    }

    /// Consume the source-lane copy only after a destination append is durable.
    /// The deterministic request identity makes crash recovery idempotent, and
    /// the exact text/attachment comparison preserves a newer source draft.
    fn consume_unchanged_admission_source_draft(
        &mut self,
        admission: &polaris_core::session_manager::AdmissionV1,
    ) -> Result<(), ProtocolError> {
        let snapshot = self.store.snapshot().map_err(store_error)?;
        let attachments = admission
            .payload
            .attachment_ids
            .iter()
            .map(|id| AttachmentId::new(id.as_str()).map_err(|_| error(ErrorCode::StorageFailed)))
            .collect::<Result<Vec<_>, _>>()?;
        if snapshot.state.draft.text != admission.payload.text
            || snapshot.state.draft.attachment_ids != attachments
        {
            return Ok(());
        }
        let digest = Sha256::digest(
            [
                b"polaris-admission-source-consume-v1\0".as_slice(),
                admission.id.as_str().as_bytes(),
            ]
            .concat(),
        );
        let request = Request {
            protocol_version: ProtocolVersion,
            client_id: ClientId::new("admission-owner").expect("fixed client ID"),
            request_id: RequestId::new(format!("admission-consume-{digest:x}"))
                .expect("derived request ID"),
            body: RequestBody::DraftUpdate(
                self.session.clone(),
                DraftUpdate {
                    expected_draft_revision: snapshot.state.draft.draft_revision,
                    text: String::new(),
                    attachment_ids: Vec::new(),
                },
            ),
        };
        self.store
            .apply_bound(&request, None, None)
            .map_err(store_error)?;
        Ok(())
    }

    fn overlay_admission_run(
        &self,
        id: &polaris_core::session_manager::AdmissionId,
        target: &mut Option<RunTarget>,
    ) {
        let matches = |active: &Active| active.admission.as_ref().is_some_and(|saved| saved == id);
        if let Some(active) = self.active.as_ref().filter(|active| matches(active)) {
            *target = Some(active.target.clone());
        } else if let Some(active) = self
            .other_lane
            .as_ref()
            .and_then(|lane| lane.active.as_ref())
            .filter(|active| matches(active))
        {
            *target = Some(active.target.clone());
        }
    }

    fn pump_admissions(&mut self) -> Result<(), ProtocolError> {
        loop {
            let next = self
                .admission
                .as_ref()
                .ok_or_else(|| error(ErrorCode::CapabilityUnavailable))?
                .next_runnable()
                .map_err(store_error)?;
            let Some((id, destination)) = next else {
                return Ok(());
            };
            let source = self.session.clone();
            let destination = SessionId::new(destination.as_str())
                .map_err(|_| error(ErrorCode::StorageFailed))?;
            self.select_lane(&destination)?;
            if self.active.is_some() {
                self.select_lane(&source)?;
                return Ok(());
            }
            let run = self
                .store
                .admitted_run(id.as_str())
                .map_err(store_error)?
                .ok_or_else(|| error(ErrorCode::StorageFailed))?;
            self.admission
                .as_mut()
                .ok_or_else(|| error(ErrorCode::CapabilityUnavailable))?
                .start_next()
                .map_err(store_error)?
                .ok_or_else(|| error(ErrorCode::RevisionConflict))?;
            let target = RunTarget {
                run_id: run.run.run_id,
                attempt_id: run.run.attempt_id,
            };
            self.active = Some(Active {
                #[cfg(any(target_os = "macos", target_os = "linux"))]
                real: self.real_enabled().then(real::RealRun::queued),
                usage: polaris_provider::UsageMeter::default(),
                execution: crate::execution::RunExecution::new(&target, self.disconnected.clone()),
                target,
                operation: OperationId::new(self.id("operation"))
                    .map_err(|_| error(ErrorCode::StorageFailed))?,
                message: message_id(
                    self.store.snapshot().map_err(store_error)?.raw.len() as u64 + 1,
                ),
                chunk: 0,
                text: String::new(),
                started: false,
                admission: Some(id),
                #[cfg(any(target_os = "macos", target_os = "linux"))]
                delegation: None,
            });
            self.select_lane(&source)?;
        }
    }

    fn resolve_approval(
        &mut self,
        request: &Request,
        events: &mut Vec<EventBody>,
    ) -> Result<SuccessResult, ProtocolError> {
        let RequestBody::ApprovalResolve(_, params) = &request.body else {
            return Err(error(ErrorCode::InvalidRequest));
        };
        let existing = self
            .store
            .request_status(&self.session, &request.client_id, &request.request_id)
            .map_err(store_error)?;
        if existing.is_none() {
            if self.draining {
                return Err(error(ErrorCode::SessionBusy));
            }
            if self
                .store
                .snapshot()
                .map_err(store_error)?
                .state
                .requests
                .len()
                >= REQUEST_RECORDS - 1
            {
                return Err(error(ErrorCode::CapabilityUnavailable));
            }
        }
        let accepted = self
            .store
            .resolve_approval(request, crate::execution::now_ms().map_err(store_error)?)
            .map_err(store_error)?;
        let RequestResult::ApprovalResolved { approval_id } = accepted.record.result else {
            return Err(error(ErrorCode::InvalidRequest));
        };
        let response = ApprovalResolved {
            approval_id: approval_id.clone(),
            state: Resolved::Resolved,
            decision: params.decision,
        };
        // 再送ACKは保存結果だけ。実行窓口へ一回限りの許可を再発行しない。
        if existing.is_none() {
            if let Some(active) = &mut self.active
                && active.target.run_id == params.run_id
                && active.target.attempt_id == params.attempt_id
            {
                active
                    .execution
                    .approval_resolved(&approval_id, params.decision);
            }
            events.push(EventBody::ApprovalResolved(response.clone()));
        }
        Ok(SuccessResult::ApprovalResolve(response))
    }

    /// GUI configuration has only three selections. The profile text, skill
    /// IDs and paths remain the startup owner's validated catalog.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn configure_workflow(
        &mut self,
        params: &polaris_desktop_protocol::request::WorkflowConfigure,
        events: &mut Vec<EventBody>,
    ) -> Result<SuccessResult, ProtocolError> {
        if self.active.is_some()
            || !self.cleanups.is_empty()
            || self.source_unsettled()
            || self
                .store
                .snapshot()
                .map_err(store_error)?
                .state
                .runs
                .iter()
                .any(|run| !run.run.state.is_terminal())
        {
            return Err(error(ErrorCode::SessionBusy));
        }
        let factory = self
            .trusted_runs
            .as_ref()
            .ok_or_else(|| error(ErrorCode::CapabilityUnavailable))?
            .factory
            .clone();
        let saved = self.store.snapshot().map_err(store_error)?;
        let artifacts = saved.state.workflow_artifacts.clone();
        let (mut config, gates, state) = match saved.state.workflow {
            Some(saved) => (saved.config, saved.gates, saved.state),
            None => {
                let config = factory
                    .workflow_config()
                    .ok_or_else(|| error(ErrorCode::CapabilityUnavailable))?
                    .clone();
                (
                    config.clone(),
                    polaris_core::workflow::WorkflowGatesV1::default(),
                    polaris_core::workflow::WorkflowStateV1 {
                        phase: config.initial_phase,
                        ..Default::default()
                    },
                )
            }
        };
        config.enabled = params.enabled;
        config.mode = match params.mode {
            polaris_desktop_protocol::request::WorkflowMode::Manual => {
                polaris_core::workflow::WorkflowMode::Manual
            }
            polaris_desktop_protocol::request::WorkflowMode::Adaptive => {
                polaris_core::workflow::WorkflowMode::Adaptive
            }
        };
        let phase = workflow_phase(params.initial_phase);
        config.initial_phase = phase;
        let workflow = {
            let candidate = polaris_core::workflow::SessionWorkflow::restore_with_artifacts(
                config.clone(),
                gates.clone(),
                state,
                artifacts.clone(),
            )
            .map_err(|_| error(ErrorCode::CapabilityUnavailable))?;
            candidate
                .request_phase(phase)
                .map_err(|_| error(ErrorCode::CapabilityUnavailable))?;
            validated_workflow(
                config,
                gates,
                candidate.state(),
                artifacts,
                factory.workflow_skills(),
            )?
        };
        self.store
            .configure_workflow(params.expected_session_revision, Some(workflow))
            .map_err(store_error)?;
        let saved = self.store.snapshot().map_err(store_error)?;
        events.push(EventBody::OrchestrationUpdated(orchestration_summary(
            &saved,
            self.admission.as_ref(),
            self.delegation_projection().as_ref(),
        )));
        Ok(SuccessResult::WorkflowConfigure(WorkflowConfigured {
            session_revision: saved.marker.session_revision,
            workflow: saved.state.workflow.as_ref().map(workflow_status),
        }))
    }

    /// New/legacy desktop sessions must have a durable configuration before a
    /// RunStart is admitted. Failure leaves the request unaccepted.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn initialize_workflow(&mut self, expected: DecimalU64) -> Result<(), ProtocolError> {
        let factory = self
            .trusted_runs
            .as_ref()
            .ok_or_else(|| error(ErrorCode::CapabilityUnavailable))?
            .factory
            .clone();
        let config = factory
            .workflow_config()
            .ok_or_else(|| error(ErrorCode::CapabilityUnavailable))?
            .clone();
        let artifacts = self
            .store
            .snapshot()
            .map_err(store_error)?
            .state
            .workflow_artifacts;
        let workflow = validated_workflow(
            config.clone(),
            polaris_core::workflow::WorkflowGatesV1::default(),
            polaris_core::workflow::WorkflowStateV1 {
                phase: config.initial_phase,
                ..Default::default()
            },
            artifacts,
            factory.workflow_skills(),
        )?;
        self.store
            .configure_workflow(expected, Some(workflow))
            .map_err(store_error)
    }

    fn mutate(
        &mut self,
        request: &Request,
        events: &mut Vec<EventBody>,
    ) -> Result<SuccessResult, ProtocolError> {
        let mut before = self.store.snapshot().map_err(store_error)?;
        let existing = self
            .store
            .request_status(&self.session, &request.client_id, &request.request_id)
            .map_err(store_error)?;
        if existing.is_none() {
            // 永続台帳も無制限に増やさない。上限時にも既存照会・再送と取消を残す。
            let active_cancel = matches!(&request.body, RequestBody::RunCancel(_, target)
                if before.state.runs.iter().any(|r| r.run.run_id == target.run_id
                    && r.run.attempt_id == target.attempt_id && !r.run.state.is_terminal()));
            let reserve = usize::from(!active_cancel);
            if before.state.requests.len() >= REQUEST_RECORDS - reserve {
                return Err(error(ErrorCode::CapabilityUnavailable));
            }
            match &request.body {
                #[cfg(any(target_os = "macos", target_os = "linux"))]
                RequestBody::RunStart(..)
                    if self.real_enabled() && before.state.workflow.is_none() =>
                {
                    self.initialize_workflow(before.marker.session_revision)?;
                    before = self.store.snapshot().map_err(store_error)?;
                }
                RequestBody::DraftUpdate(_, p)
                    if p.text.len() > TEXT_BYTES || !p.attachment_ids.is_empty() =>
                {
                    return Err(error(ErrorCode::CapabilityUnavailable));
                }
                RequestBody::SessionConfigure(_, p) if self.production => {
                    if self.active.is_some()
                        || !self.cleanups.is_empty()
                        || self.source_unsettled()
                        || before
                            .state
                            .runs
                            .iter()
                            .any(|run| !run.run.state.is_terminal())
                    {
                        return Err(error(ErrorCode::SessionBusy));
                    }
                    polaris_core::desktop_store::validate_owner_configuration(
                        &p.provider,
                        &p.model,
                        &p.effort,
                    )
                    .map_err(|_| error(ErrorCode::CapabilityUnavailable))?;
                }
                RequestBody::SessionConfigure(_, p)
                    if p.provider != "fake"
                        || !matches!(p.model.as_str(), "scripted" | "scripted-alt")
                        || p.effort != "none" =>
                {
                    return Err(error(ErrorCode::CapabilityUnavailable));
                }
                RequestBody::RunStart(..) if before.state.runs.len() >= RUNS => {
                    return Err(error(ErrorCode::CapabilityUnavailable));
                }
                _ => {}
            }
        }
        // queued取消の保存終端は次run受付で上書きせず、先に配信queueへ引き渡す。
        if !self.production
            && matches!(request.body, RequestBody::RunStart(..))
            && let Some(active) = &self.active
            && let Some(record) = before
                .state
                .runs
                .iter()
                .find(|r| r.run.run_id == active.target.run_id)
            && record.run.state.is_terminal()
        {
            events.push(EventBody::RunState(record.run.clone()));
            self.active = None;
        }
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if self.real_enabled()
            && existing.is_none()
            && matches!(request.body, RequestBody::RunStart(..))
            && !real::history_ready(&before)
        {
            return Err(error(ErrorCode::CapabilityUnavailable));
        }
        if existing.is_none()
            && matches!(request.body, RequestBody::RunStart(..))
            && self.active.is_some()
        {
            return Err(error(ErrorCode::SessionBusy));
        }
        let target = matches!(request.body, RequestBody::RunStart(..)).then(|| RunTarget {
            run_id: RunId::new(self.id("run")).unwrap(),
            attempt_id: AttemptId::new(self.id("attempt")).unwrap(),
        });
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if existing.is_none()
            && let RequestBody::RunCancel(_, target) = &request.body
        {
            // Child cancellation intent must be durable before the root's
            // run.cancel receipt can make the root cancellation externally
            // observable.
            self.record_root_cancel_plan(target)?;
        }
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if existing.is_none()
            && let (Some(source), Some(target)) = (&mut self.source, &target)
        {
            source.reserve(target.clone())?;
        }
        // The binding is decided before the acceptance writes it, so a run is
        // never saved without the registration its sends must use. A re-sent
        // request keeps the binding its acceptance already saved.
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let account = match (&target, existing.is_none(), self.trusted_runs.as_ref()) {
            (Some(_), true, Some(runs)) => match runs.factory.bind_account() {
                Ok(account) => account,
                Err(_) => {
                    // Nothing was accepted, so the placeholder goes back too.
                    #[cfg(any(target_os = "macos", target_os = "linux"))]
                    if let (Some(source), Some(target)) = (&mut self.source, &target) {
                        source.release_unused(target);
                    }
                    return Err(error(ErrorCode::CapabilityUnavailable));
                }
            },
            _ => None,
        };
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let account = None;
        // Legacy `run.start` remains an explicit current-session operation.
        // It is not auto-routed and therefore never consumes an admission ID.
        let admission_id = None;
        let accepted = match self.store.apply_bound(request, target.clone(), account) {
            Ok(accepted) => accepted,
            Err(e) => {
                // A definitely rejected start releases its placeholder. Ambiguous
                // storage retains it; no new run can silently replace ownership.
                #[cfg(any(target_os = "macos", target_os = "linux"))]
                if existing.is_none()
                    && self.store.snapshot().is_ok()
                    && let (Some(source), Some(target)) = (&mut self.source, &target)
                {
                    source.release_unused(target);
                }
                return Err(store_error(e));
            }
        };
        let revision = accepted.record.accepted_revision;
        let fresh = existing.is_none();
        match accepted.record.result {
            RequestResult::ApprovalResolved { .. } | RequestResult::SourceApplyResolved { .. } => {
                Err(error(ErrorCode::InvalidRequest))
            }
            RequestResult::DraftUpdated { draft_revision } => {
                if fresh {
                    events.push(EventBody::DraftUpdated(
                        self.store.snapshot().map_err(store_error)?.state.draft,
                    ));
                }
                Ok(SuccessResult::DraftUpdate(DraftUpdated {
                    session_revision: revision,
                    draft_revision,
                }))
            }
            RequestResult::Configured { configuration, .. } => {
                if fresh {
                    events.push(EventBody::ConfigurationUpdated(configuration.clone()));
                }
                Ok(SuccessResult::SessionConfigure(Configured {
                    session_revision: revision,
                    configuration,
                }))
            }
            RequestResult::GoalUpdated { goal_revision } => Ok(SuccessResult::GoalUpdate(
                polaris_desktop_protocol::response::GoalUpdated {
                    session_revision: revision,
                    goal_revision,
                },
            )),
            RequestResult::InstructionReceived { instruction_id } => {
                Ok(SuccessResult::InstructionSend(
                    polaris_desktop_protocol::response::InstructionReceived {
                        session_revision: revision,
                        instruction_id,
                    },
                ))
            }
            RequestResult::RunAccepted { run_id, attempt_id } => {
                if fresh {
                    let run = accepted
                        .run
                        .ok_or_else(|| error(ErrorCode::StorageFailed))?;
                    events.push(EventBody::RunState(run.run));
                    events.push(EventBody::DraftUpdated(
                        self.store.snapshot().map_err(store_error)?.state.draft,
                    ));
                    self.active = Some(Active {
                        #[cfg(any(target_os = "macos", target_os = "linux"))]
                        real: self.real_enabled().then(real::RealRun::queued),
                        usage: polaris_provider::UsageMeter::default(),
                        execution: crate::execution::RunExecution::new(
                            &RunTarget {
                                run_id: run_id.clone(),
                                attempt_id: attempt_id.clone(),
                            },
                            self.disconnected.clone(),
                        ),
                        target: RunTarget {
                            run_id: run_id.clone(),
                            attempt_id: attempt_id.clone(),
                        },
                        operation: OperationId::new(self.id("operation")).unwrap(),
                        message: message_id(before.raw.len() as u64 + 2),
                        chunk: 0,
                        text: String::new(),
                        started: false,
                        admission: admission_id,
                        #[cfg(any(target_os = "macos", target_os = "linux"))]
                        delegation: None,
                    });
                    if self.admission.is_some() {
                        let published = self.store.snapshot().map_err(store_error)?;
                        events.push(EventBody::OrchestrationUpdated(orchestration_summary(
                            &published,
                            self.admission.as_ref(),
                            self.delegation_projection().as_ref(),
                        )));
                    }
                }
                Ok(SuccessResult::RunStart(RunAccepted {
                    session_revision: revision,
                    run_id,
                    attempt_id,
                    state: Queued::Queued,
                }))
            }
            RequestResult::CancelRequested { run_id, attempt_id } => {
                #[cfg(any(target_os = "macos", target_os = "linux"))]
                if let Some(source) = &mut self.source {
                    source.cancel_target(&RunTarget {
                        run_id: run_id.clone(),
                        attempt_id: attempt_id.clone(),
                    });
                }
                if let Some(active) = &mut self.active
                    && active.target.run_id == run_id
                    && active.target.attempt_id == attempt_id
                {
                    active.cancel();
                }
                // ACKだけを先に返し、取消終端は次の所有者stepで別eventとして通知する。
                Ok(SuccessResult::RunCancel(CancelAccepted {
                    status: CancelStatus::CancelRequested,
                }))
            }
        }
    }

    fn decision_status(&self) -> Option<Vec<String>> {
        if self.wire_version != Some(ProtocolVersion::V2) {
            return None;
        }
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            self.trusted_runs
                .as_ref()?
                .factory
                .decision_status()
                .map(|lines| {
                    lines
                        .into_iter()
                        .take(32)
                        .map(|line| line.chars().take(2048).collect())
                        .collect()
                })
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        None
    }

    fn snapshot(&mut self, subscribe: bool) -> Result<Snapshot, ProtocolError> {
        let published = self.store.snapshot().map_err(store_error)?;
        let id = SnapshotId::new(self.id("snapshot")).unwrap();
        let position = if subscribe || self.subscription.is_none() {
            Position {
                engine_epoch: self.epoch.clone(),
                subscription_id: SubscriptionId::new(self.id("subscription")).unwrap(),
                event_seq: DecimalU64::new(0),
            }
        } else {
            self.subscription.clone().unwrap()
        };
        let state = &published.state;
        let snapshot = Snapshot {
            snapshot_id: id.clone(),
            history_start_cursor: cursor(&id, 0),
            session_id: self.session.clone(),
            summary: if self.production {
                "保存済みの会話".into()
            } else {
                "一時領域のfake会話".into()
            },
            session_revision: published.marker.session_revision,
            content_revision: published.marker.content_revision,
            plan_revision: DecimalU64::new(0),
            policy_revision: state.policy_revision,
            position: position.clone(),
            draft: state.draft.clone(),
            configuration: state.configuration.clone(),
            workflow: state.workflow.as_ref().map(workflow_status),
            orchestration: (self.wire_version == Some(ProtocolVersion::V2)).then(|| {
                orchestration_summary(
                    &published,
                    self.admission.as_ref(),
                    self.delegation_projection().as_ref(),
                )
            }),
            memory: state.runs.last().and_then(|r| r.memory.clone()),
            decision_status: self.decision_status(),
            role_bindings: role_bindings::to_wire(state.role_bindings.as_ref()),
            role_catalog: self.role_catalog_wire(),
            tasks: state.tasks.clone(),
            runs: state.runs.iter().map(|r| r.run.clone()).collect(),
            children: state.children.iter().map(|c| c.child.clone()).collect(),
            child_attempt_count: DecimalU64::new(state.children.len() as u64),
            unresolved_approvals: state.unresolved_approvals.clone(),
        };
        // 購読は常に最大1件。置換snapshotより前のeventは同じwriter内で先に配信される。
        if subscribe {
            self.subscription = Some(position);
        }
        self.history = Some(History {
            id,
            published,
            expires: Instant::now() + SNAPSHOT_TTL,
        });
        Ok(snapshot)
    }

    fn history_page(
        &self,
        params: &polaris_desktop_protocol::request::HistoryPage,
    ) -> Result<HistoryPageResult, ProtocolError> {
        let history = self
            .history
            .as_ref()
            .filter(|h| h.id == params.snapshot_id && Instant::now() < h.expires)
            .ok_or_else(|| error(ErrorCode::RevisionConflict))?;
        let prefix = format!("{}:", history.id.as_str());
        let offset = params
            .cursor
            .as_str()
            .strip_prefix(&prefix)
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|i| {
                *i <= history.published.raw.len() && cursor(&history.id, *i) == params.cursor
            })
            .ok_or_else(|| error(ErrorCode::RevisionConflict))?;
        // 最大4件。保存側は各本文のJSON表現を128KiB以下に制限する。
        // ID等の固定上限を加えても1MiB frame未満で、本文を切り捨てない。
        let end =
            (offset + usize::from(params.limit.get()).min(4)).min(history.published.raw.len());
        let messages = history.published.raw[offset..end]
            .iter()
            .map(|raw| HistoryMessage {
                message_id: message_id(raw.sequence),
                role: match raw.message.role {
                    polaris_provider::Role::User => MessageRole::User,
                    polaris_provider::Role::Assistant => MessageRole::Assistant,
                    polaris_provider::Role::Tool => MessageRole::Tool,
                },
                text: raw.message.content.clone(),
                saved_byte_offset: DecimalU64::new(raw.message.content.len() as u64),
            })
            .collect();
        Ok(HistoryPageResult {
            snapshot_id: history.id.clone(),
            session_revision: history.published.marker.session_revision,
            messages,
            next_cursor: (end < history.published.raw.len()).then(|| cursor(&history.id, end)),
        })
    }

    fn emit_all(&mut self, events: Vec<EventBody>, out: &Outbox) -> Result<(), ServiceError> {
        for body in events {
            self.emit(body, out)?;
        }
        Ok(())
    }
    fn emit(&mut self, body: EventBody, out: &Outbox) -> Result<(), ServiceError> {
        if self.wire_version == Some(ProtocolVersion::V1)
            && matches!(body, EventBody::OrchestrationUpdated(_))
        {
            return Ok(());
        }
        if let Some(position) = &mut self.subscription {
            position.event_seq = position
                .event_seq
                .checked_add(1)
                .map_err(|_| ServiceError::Worker)?;
            let event = Event {
                protocol_version: self.wire_version.unwrap_or(ProtocolVersion),
                engine_epoch: self.epoch.clone(),
                subscription_id: position.subscription_id.clone(),
                event_seq: position.event_seq,
                session_id: self.session.clone(),
                session_revision: self.store.snapshot()?.marker.session_revision,
                body,
            };
            // seq付与後に送れなければ接続を閉じる。欠落を黙って間引かない。
            out.send(&event)?;
        }
        Ok(())
    }
    // Storage failure forbids readiness but must not prevent core thread join.
    fn cancel_and_poll_core(&mut self) {
        if let Some(active) = &mut self.active {
            active.cancel();
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            if let Some(real) = &mut active.real
                && real.poll(&active.target, None).is_err()
            {
                self.failed = true;
            }
        }
    }
    fn mcp_catalog_idle(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            self.mcp_catalog.is_none() && self.mcp_setup.job.is_none()
        }
        #[cfg(not(target_os = "macos"))]
        {
            true
        }
    }
    fn step(&mut self, out: &Outbox) -> Result<(), ServiceError> {
        let mut routing_events = Vec::new();
        self.poll_routing(&mut routing_events)
            .map_err(|_| ServiceError::Worker)?;
        self.emit_all(routing_events, out)?;
        self.step_lane(out)?;
        if self.other_lane.is_some() {
            self.swap_lane().map_err(|_| ServiceError::Worker)?;
            let result = self.step_lane(out);
            self.swap_lane().map_err(|_| ServiceError::Worker)?;
            result?;
        }
        if self.admission.is_some() {
            self.pump_admissions().map_err(|_| ServiceError::Worker)?;
        }
        Ok(())
    }

    fn step_lane(&mut self, out: &Outbox) -> Result<(), ServiceError> {
        self.poll_local_models(Some(out), false)?;
        #[cfg(target_os = "macos")]
        self.poll_mcp_catalog(Some(out), false)?;
        #[cfg(target_os = "macos")]
        self.poll_mcp_setup(Some(out), false)?;
        self.poll_source(false)?;
        self.cleanups.step();
        self.checkpoint_usage()?;
        if self.failed {
            self.cancel_and_poll_core();
            return Err(StoreError::RecoveryRequired.into());
        }
        let approval_events = if let Some(active) = &mut self.active {
            active.execution.step(&mut self.store, &active.target)?;
            active.execution.take_approval_events()
        } else {
            Vec::new()
        };
        self.emit_all(approval_events, out)?;
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.real.is_some())
        {
            self.emit_delegation_update(out)?;
            let result = self.step_real(Some(out), false);
            self.emit_delegation_update(out)?;
            return result;
        }
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.execution.awaiting_approval())
        {
            return Ok(());
        }
        let Some(active) = &self.active else {
            return Ok(());
        };
        let state = self
            .store
            .snapshot()?
            .state
            .runs
            .into_iter()
            .find(|r| r.run.run_id == active.target.run_id)
            .ok_or(StoreError::NotFound)?
            .run;
        if state.state == RunState::Cancelling || state.state.is_terminal() {
            return self.complete(Observation::Cancelled, Some(out));
        }
        if !active.started {
            if self
                .store
                .record_intent(&active.target, active.operation.clone())?
                != IntentReceipt::NewlyPublished
            {
                return Err(StoreError::RunConflict.into());
            }
            self.active.as_mut().unwrap().started = true;
            let mut run = state;
            run.state = RunState::Running;
            return self.emit(EventBody::RunState(run), out);
        }
        let active = self.active.as_mut().unwrap();
        if let Some(text) = self.options.script.get(active.chunk) {
            let delta = TextDelta {
                message_id: active.message.clone(),
                byte_offset: DecimalU64::new(active.text.len() as u64),
                text: text.clone(),
                durability: Durability::Tentative,
            };
            active.text.push_str(text);
            active.chunk += 1;
            self.emit(EventBody::MessageDelta(delta), out)
        } else {
            self.complete(Observation::Succeeded, Some(out))
        }
    }

    fn checkpoint_usage(&mut self) -> Result<(), ServiceError> {
        if let Some(active) = &self.active {
            let report = active.usage.snapshot();
            // 未実行のfixtureを、使用量ゼロの実測と表示しない。
            if report.reported_responses > 0
                || report.missing_responses > 0
                || report.failed_requests > 0
            {
                self.store.record_usage(&active.target, report)?;
            }
        }
        Ok(())
    }

    fn complete(&mut self, outcome: Observation, out: Option<&Outbox>) -> Result<(), ServiceError> {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.real.is_some())
        {
            return self.step_real(out, outcome == Observation::Cancelled);
        }
        self.checkpoint_usage()?;
        let (approval_events, settled) = if let Some(active) = &mut self.active {
            if outcome == Observation::Cancelled {
                active.cancel();
            }
            active.execution.step(&mut self.store, &active.target)?;
            self.cleanups.step();
            (
                active.execution.take_approval_events(),
                active.execution.quiescent() && self.cleanups.is_empty(),
            )
        } else {
            (Vec::new(), true)
        };
        if let Some(out) = out {
            self.emit_all(approval_events, out)?;
        }
        if !settled {
            return Ok(());
        }
        let Some(active) = self.active.as_ref() else {
            return Ok(());
        };
        let outcome = active.execution.observation(outcome);
        let mut saved_text = active.text.clone();
        active.execution.append_results(&mut saved_text);
        let published = self.store.snapshot()?;
        let record = published
            .state
            .runs
            .iter()
            .find(|r| r.run.run_id == active.target.run_id)
            .ok_or(StoreError::NotFound)?;
        if !record.run.state.is_terminal() {
            let result = ResultId::new(format!("result-{}", active.target.run_id.as_str()))
                .map_err(|_| ServiceError::Worker)?;
            if active.started {
                self.store.finish_with_text(
                    &active.target,
                    &active.operation,
                    outcome,
                    result,
                    saved_text.clone(),
                )?;
            } else {
                self.store
                    .finish(&active.target, Observation::Cancelled, result)?;
            }
        }
        self.active.as_mut().unwrap().text = saved_text;
        self.active.as_mut().unwrap().execution.saved = true;
        let admission_terminal = self.active.as_ref().and_then(|active| {
            active.admission.clone().map(|id| {
                let status = if !active.started || outcome == Observation::Cancelled {
                    polaris_core::session_manager::TerminalStatus::Cancelled
                } else if outcome == Observation::Succeeded {
                    polaris_core::session_manager::TerminalStatus::Completed
                } else if outcome == Observation::OutcomeUnknown {
                    polaris_core::session_manager::TerminalStatus::OutcomeUnknown
                } else {
                    polaris_core::session_manager::TerminalStatus::Failed
                };
                (id, status)
            })
        });
        if let (Some(owner), Some((admission, status))) =
            (self.admission.as_mut(), admission_terminal)
        {
            owner.terminal(&admission, status)?;
            if let Some(out) = out {
                let published = self.store.snapshot()?;
                self.emit(
                    EventBody::OrchestrationUpdated(orchestration_summary(
                        &published,
                        self.admission.as_ref(),
                        self.delegation_projection().as_ref(),
                    )),
                    out,
                )?;
            }
        }
        let active = self.active.take().unwrap();
        let run = self
            .store
            .snapshot()?
            .state
            .runs
            .into_iter()
            .find(|r| r.run.run_id == active.target.run_id)
            .ok_or(StoreError::NotFound)?
            .run;
        if let Some(out) = out {
            if active.started {
                self.emit(
                    EventBody::MessageDelta(TextDelta {
                        message_id: active.message,
                        byte_offset: DecimalU64::new(0),
                        text: active.text,
                        durability: Durability::Saved,
                    }),
                    out,
                )?;
            }
            self.emit(EventBody::RunState(run), out)?;
        }
        Ok(())
    }
    fn settle(&mut self, out: &Outbox) -> Result<(), ServiceError> {
        let mut routing_events = Vec::new();
        self.poll_routing(&mut routing_events)
            .map_err(|_| ServiceError::Worker)?;
        self.emit_all(routing_events, out)?;
        self.settle_lane(Some(out))?;
        let mut all_ready = self.ready;
        if self.other_lane.is_some() {
            self.swap_lane().map_err(|_| ServiceError::Worker)?;
            let result = self.settle_lane(Some(out));
            self.swap_lane().map_err(|_| ServiceError::Worker)?;
            result?;
            all_ready &= self.ready;
        }
        self.ready = all_ready && self.routing_jobs.is_empty();
        Ok(())
    }

    fn settle_lane(&mut self, out: Option<&Outbox>) -> Result<(), ServiceError> {
        self.ready = false;
        self.poll_local_models(out, true)?;
        #[cfg(target_os = "macos")]
        self.poll_mcp_catalog(out, true)?;
        #[cfg(target_os = "macos")]
        self.poll_mcp_setup(out, true)?;
        let source_result = self.poll_source(true);
        self.cleanups.step();
        self.cancel_and_poll_core();
        source_result?;
        if self.failed {
            return Err(StoreError::RecoveryRequired.into());
        }
        self.complete(Observation::Cancelled, out)?;
        self.store.snapshot()?;
        self.ready = self.active.is_none()
            && self.cleanups.is_empty()
            && !self.source_unsettled()
            && self.local_models.is_none()
            && self.mcp_catalog_idle();
        Ok(())
    }
    fn settle_without_output(&mut self) -> Result<(), ServiceError> {
        let mut ignored = Vec::new();
        self.poll_routing(&mut ignored)
            .map_err(|_| ServiceError::Worker)?;
        self.settle_lane(None)?;
        let mut all_ready = self.ready;
        if self.other_lane.is_some() {
            self.swap_lane().map_err(|_| ServiceError::Worker)?;
            let result = self.settle_lane(None);
            self.swap_lane().map_err(|_| ServiceError::Worker)?;
            result?;
            all_ready &= self.ready;
        }
        self.ready = all_ready && self.routing_jobs.is_empty();
        Ok(())
    }
}

fn message_id(sequence: u64) -> MessageId {
    MessageId::new(format!("message-{sequence}")).unwrap()
}
fn cursor(id: &SnapshotId, offset: usize) -> HistoryCursor {
    HistoryCursor::new(format!("{}:{offset}", id.as_str())).unwrap()
}
#[cfg(unix)]
fn management_project_matches(project: &std::path::Path, identity: (u64, u64)) -> bool {
    use std::os::unix::fs::MetadataExt;

    std::fs::symlink_metadata(project)
        .is_ok_and(|metadata| metadata.is_dir() && (metadata.dev(), metadata.ino()) == identity)
}
fn error(code: ErrorCode) -> ProtocolError {
    ProtocolError {
        code,
        message: match code {
            ErrorCode::PermissionDenied => "対象の会話にアクセスできません",
            ErrorCode::RevisionConflict => "期待版または要求内容が一致しません",
            ErrorCode::SessionBusy => "実行中または終了処理中です",
            ErrorCode::CapabilityUnavailable => "このserviceでは利用できません",
            ErrorCode::ReferenceUnavailable => "参照入力はこのserviceでは利用できません",
            ErrorCode::StorageFailed | ErrorCode::RecoveryRequired => "保存状態を確認できません",
            ErrorCode::Capacity => "保存容量が不足しています。新しい入力は受け付けていません",
            ErrorCode::NotFound => "対象がありません",
            _ => "要求を受け付けられません",
        }
        .into(),
    }
}
fn store_error(error_value: StoreError) -> ProtocolError {
    error(match error_value {
        StoreError::Busy => ErrorCode::SessionBusy,
        StoreError::CasConflict(_) | StoreError::RequestConflict | StoreError::RunConflict => {
            ErrorCode::RevisionConflict
        }
        StoreError::NotFound | StoreError::Deleted => ErrorCode::NotFound,
        StoreError::TargetMismatch => ErrorCode::PermissionDenied,
        StoreError::UnsupportedMethod => ErrorCode::CapabilityUnavailable,
        StoreError::RecoveryRequired => ErrorCode::RecoveryRequired,
        StoreError::Capacity(_) => ErrorCode::Capacity,
        _ => ErrorCode::StorageFailed,
    })
}
fn status(accepted: Option<Acceptance>) -> RequestStatusResult {
    let Some(accepted) = accepted else {
        return RequestStatusResult::NotFound {};
    };
    let revision = accepted.record.accepted_revision;
    if let Some(run) = accepted.run {
        if run.run.state == RunState::OutcomeUnknown {
            return RequestStatusResult::OutcomeUnknown {
                session_revision: revision,
                run_id: run.run.run_id,
                attempt_id: run.run.attempt_id,
            };
        }
        if let Some(result_id) = run.result_id {
            return RequestStatusResult::Completed {
                session_revision: revision,
                result_id,
            };
        }
        return RequestStatusResult::Accepted {
            session_revision: revision,
            run: Some(RunTarget {
                run_id: run.run.run_id,
                attempt_id: run.run.attempt_id,
            }),
        };
    }
    RequestStatusResult::Completed {
        session_revision: revision,
        result_id: ResultId::new(format!("request-{}", accepted.record.request_hash)).unwrap(),
    }
}

#[cfg(all(test, unix))]
#[path = "tests.rs"]
mod tests;
