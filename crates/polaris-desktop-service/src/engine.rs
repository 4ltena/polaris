//! 保存・実行の単一所有者。実行入力は信頼済みownerから明示注入する。

mod local_models;
#[cfg(target_os = "macos")]
mod real;
#[cfg(target_os = "macos")]
mod source_owner;
#[cfg(target_os = "macos")]
mod source_recovery;
mod source_view;
#[cfg(target_os = "macos")]
pub use real::{TrustedRunCompletion, TrustedRunFactory, TrustedRunInputs};
#[cfg(target_os = "macos")]
pub use source_owner::{SourceApplyRuntime, TrustedSourceFactory};
#[cfg(target_os = "macos")]
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
    request::{Request, RequestBody, RunTarget},
    response::*,
    run_state::{Observation, RunState},
    snapshot::{Configuration, Draft, Position, Snapshot},
    validate::{ConnectionState, validate_request},
};
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
    #[cfg(target_os = "macos")]
    real: Option<real::RealRun>,
    usage: polaris_provider::UsageMeter,
    target: RunTarget,
    operation: OperationId,
    message: MessageId,
    chunk: usize,
    text: String,
    started: bool,
    execution: crate::execution::RunExecution,
}
impl Active {
    fn cancel(&mut self) {
        #[cfg(target_os = "macos")]
        if let Some(real) = &mut self.real {
            real.cancel();
        }
        self.execution.cancel();
    }
}
struct History {
    id: SnapshotId,
    published: Published,
    expires: Instant,
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
    pub fn open(config: ServiceConfig) -> Result<Self, ServiceError> {
        Self::open_storage(
            config,
            Configuration {
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
        let root = DesktopRoot::open_owned(&config.store_root)?;
        let store = root.open_or_create(
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
        )?;
        Self::from_production_store(store, config.session_id)
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
                            RequestBody::RunCancel(..) | RequestBody::ShutdownRequest(..)
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
                && engine.local_models.is_none();
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
        #[cfg(target_os = "macos")]
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
                if matches!(request.body, RequestBody::RunCancel(..) | RequestBody::ShutdownRequest(..)) {
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
                RequestBody::RunCancel(..) | RequestBody::ShutdownRequest(..)
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
    local_models: Option<local_models::InventoryJob>,
    inventory_generation: u64,
    store: Writer,
    session: SessionId,
    production: bool,
    #[cfg(target_os = "macos")]
    trusted_runs: Option<real::TrustedRuns>,
    #[cfg(target_os = "macos")]
    source: Option<source_owner::SourceOwner>,
    #[cfg(target_os = "macos")]
    source_recovery: Option<source_recovery::Ingress>,
    options: Options,
    epoch: EngineEpoch,
    connection: ConnectionState,
    client: Option<ClientId>,
    subscription: Option<Position>,
    history: Option<History>,
    serial: u64,
    active: Option<Active>,
    draining: bool,
    ready: bool,
    failed: bool,
    served: bool,
    disconnected: Arc<AtomicBool>,
    cleanups: crate::execution::CleanupOwner,
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
        Self {
            store,
            session,
            production: false,
            local_models: None,
            inventory_generation: 0,
            #[cfg(target_os = "macos")]
            trusted_runs: None,
            #[cfg(target_os = "macos")]
            source: None,
            #[cfg(target_os = "macos")]
            source_recovery: None,
            options,
            disconnected: Arc::new(AtomicBool::new(false)),
            epoch: EngineEpoch::new(format!("fake-{}-{time}-{serial}", std::process::id()))
                .unwrap(),
            connection: ConnectionState::AwaitingHello,
            client: None,
            subscription: None,
            history: None,
            serial: 0,
            active: None,
            draining: false,
            ready: false,
            failed: false,
            served: false,
            cleanups: Default::default(),
            #[cfg(all(test, unix))]
            pause: None,
        }
    }
    fn source_enabled(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            self.source.is_some()
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    }
    fn source_unsettled(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            self.source
                .as_ref()
                .is_some_and(|source| source.unsettled())
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    }
    fn poll_source(&mut self, stopping: bool) -> Result<(), ServiceError> {
        #[cfg(target_os = "macos")]
        self.poll_source_recovery();
        #[cfg(target_os = "macos")]
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
        #[cfg(target_os = "macos")]
        if let Some(source) = &mut self.source {
            return source.resolve(&mut self.store, request, &self.draining, &self.disconnected);
        }
        let _ = request;
        Err(error(ErrorCode::CapabilityUnavailable))
    }
    fn real_enabled(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            self.production && self.trusted_runs.is_some()
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    }
    fn id(&mut self, prefix: &str) -> String {
        self.serial += 1; // 有界な要求数に達する前に接続queueが閉じる。u64はwrapさせない。
        format!("{prefix}-{}-{}", self.epoch.as_str(), self.serial)
    }

    fn handle(
        &mut self,
        request: &Request,
        events: &mut Vec<EventBody>,
    ) -> Result<SuccessResult, ProtocolError> {
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
        // 固定のproject/session所属を台帳やsnapshotを読む前に照合する。
        if request
            .body
            .session_id()
            .is_some_and(|id| id != &self.session)
        {
            return Err(error(ErrorCode::PermissionDenied));
        }
        if matches!(request.body, RequestBody::Hello) {
            self.connection = gate.next_state;
            self.client = Some(request.client_id.clone());
            return Ok(SuccessResult::Hello(Hello {
                protocol_version: ProtocolVersion,
                engine_epoch: self.epoch.clone(),
                capabilities: if self.production && !self.real_enabled() {
                    vec![
                        Capability::SessionRead,
                        Capability::HistoryRead,
                        Capability::SourceApplyRead,
                        Capability::DraftUpdate,
                        Capability::SessionConfigure,
                        Capability::LocalModels,
                        Capability::RequestStatus,
                        Capability::Shutdown,
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
                    ];
                    if self.source_enabled() {
                        capabilities.push(Capability::SourceApplyResolve);
                    }
                    if self.production {
                        capabilities.retain(|c| *c != Capability::SessionConfigure);
                    }
                    capabilities
                },
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
            )
        {
            return Err(error(ErrorCode::SessionBusy));
        }
        match &request.body {
            RequestBody::Hello => unreachable!(),
            RequestBody::LocalModels(..) => Err(error(ErrorCode::CapabilityUnavailable)),
            RequestBody::SessionOpen(_) => {
                Ok(SuccessResult::SessionOpen(Box::new(self.snapshot(false)?)))
            }
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
                    && self.local_models.is_none();
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
            _ => self.mutate(request, events),
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

    fn mutate(
        &mut self,
        request: &Request,
        events: &mut Vec<EventBody>,
    ) -> Result<SuccessResult, ProtocolError> {
        let before = self.store.snapshot().map_err(store_error)?;
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
        #[cfg(target_os = "macos")]
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
        #[cfg(target_os = "macos")]
        if existing.is_none()
            && let (Some(source), Some(target)) = (&mut self.source, &target)
        {
            source.reserve(target.clone())?;
        }
        let accepted = match self.store.apply(request, target.clone()) {
            Ok(accepted) => accepted,
            Err(e) => {
                // A definitely rejected start releases its placeholder. Ambiguous
                // storage retains it; no new run can silently replace ownership.
                #[cfg(target_os = "macos")]
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
                        #[cfg(target_os = "macos")]
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
                    });
                }
                Ok(SuccessResult::RunStart(RunAccepted {
                    session_revision: revision,
                    run_id,
                    attempt_id,
                    state: Queued::Queued,
                }))
            }
            RequestResult::CancelRequested { run_id, attempt_id } => {
                #[cfg(target_os = "macos")]
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
        if let Some(position) = &mut self.subscription {
            position.event_seq = position
                .event_seq
                .checked_add(1)
                .map_err(|_| ServiceError::Worker)?;
            let event = Event {
                protocol_version: ProtocolVersion,
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
            #[cfg(target_os = "macos")]
            if let Some(real) = &mut active.real
                && real.poll(&active.target, None).is_err()
            {
                self.failed = true;
            }
        }
    }
    fn step(&mut self, out: &Outbox) -> Result<(), ServiceError> {
        self.poll_local_models(Some(out), false)?;
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
        #[cfg(target_os = "macos")]
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.real.is_some())
        {
            return self.step_real(Some(out), false);
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
        #[cfg(target_os = "macos")]
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
        self.ready = false;
        self.poll_local_models(Some(out), true)?;
        let source_result = self.poll_source(true);
        self.cleanups.step();
        self.cancel_and_poll_core();
        source_result?;
        if self.failed {
            return Err(StoreError::RecoveryRequired.into());
        }
        self.complete(Observation::Cancelled, Some(out))?;
        self.store.snapshot()?;
        self.ready = self.active.is_none()
            && self.cleanups.is_empty()
            && !self.source_unsettled()
            && self.local_models.is_none();
        Ok(())
    }
    fn settle_without_output(&mut self) -> Result<(), ServiceError> {
        self.ready = false;
        self.poll_local_models(None, true)?;
        let source_result = self.poll_source(true);
        self.cleanups.step();
        self.cancel_and_poll_core();
        source_result?;
        if self.failed {
            return Err(StoreError::RecoveryRequired.into());
        }
        self.complete(Observation::Cancelled, None)?;
        self.store.snapshot()?;
        self.ready = self.active.is_none()
            && self.cleanups.is_empty()
            && !self.source_unsettled()
            && self.local_models.is_none();
        Ok(())
    }
}

fn message_id(sequence: u64) -> MessageId {
    MessageId::new(format!("message-{sequence}")).unwrap()
}
fn cursor(id: &SnapshotId, offset: usize) -> HistoryCursor {
    HistoryCursor::new(format!("{}:{offset}", id.as_str())).unwrap()
}
fn error(code: ErrorCode) -> ProtocolError {
    ProtocolError {
        code,
        message: match code {
            ErrorCode::PermissionDenied => "対象の会話にアクセスできません",
            ErrorCode::RevisionConflict => "期待版または要求内容が一致しません",
            ErrorCode::SessionBusy => "実行中または終了処理中です",
            ErrorCode::CapabilityUnavailable => "このserviceでは利用できません",
            ErrorCode::StorageFailed | ErrorCode::RecoveryRequired => "保存状態を確認できません",
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
