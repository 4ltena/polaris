//! A lazy, offline Clef process owner. Package activation supplies the pinned
//! generation; construction never downloads, starts inference or reads a key.
//! The actor outlives a cancelled model future and owns stop/reap independently.

use polaris_provider::decision::{Decider, Decision, DecisionError, Question};
use polaris_sandbox::{ControlledEnd, ManagedStdio, SandboxMode, SandboxPolicy};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncWriteExt, BufReader},
    sync::{Semaphore, mpsc, oneshot},
};

mod wire;

pub const RUNTIME_REVISION: &str = "620d2225cc436ab0b001e3200e590e3b286ef9ab+polaris-fileptr-v1";

#[derive(Clone)]
pub struct CapacityAdmission {
    pub profile: crate::decision_capacity::ProfileMemory,
    pub policy: crate::decision_capacity::CapacityPolicy,
}

/// Labels associated with the pinned model digest by a trusted acceptance subject.
/// These labels alone never establish that a worker loaded the model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClefModelIdentity {
    pub repository: String,
    pub revision: String,
    pub quantization: String,
}

/// Supplied by the trusted installation owner, not inferred from a project or
/// whichever executable happens to be on PATH. Acceptance of a model/profile
/// and its capacity budget remains the activation owner's responsibility.
#[derive(Clone)]
pub struct ClefConfig {
    pub model_identity: Option<ClefModelIdentity>,
    pub worker: PathBuf,
    pub worker_sha256: String,
    pub model: PathBuf,
    pub model_bytes: u64,
    pub model_sha256: String,
    pub generation: String,
    pub device: ClefDevice,
    pub context_tokens: u32,
    pub request_timeout_ms: u64,
    pub load_timeout_ms: u64,
    /// Existing host-owned private directory for disposable per-run scratch.
    pub private_root: PathBuf,
    /// Normal managed activation supplies this. Low-level synthetic/benchmark
    /// owners may keep their separate, explicit admission contract.
    pub admission: Option<CapacityAdmission>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClefDevice {
    Cpu,
    Metal,
}

impl ClefDevice {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Metal => "metal",
        }
    }
}

pub struct ClefDecider {
    config: ClefConfig,
    actor: Mutex<Option<mpsc::Sender<Job>>>,
    /// No waiting request retains another unbounded copy of the state.
    capacity: Arc<Semaphore>,
}

enum Job {
    Decide {
        state: serde_json::Value,
        questions: BTreeMap<String, Question>,
        reply: oneshot::Sender<polaris_provider::decision::DecisionObservation>,
        context: polaris_provider::decision::DecisionContext,
        capabilities: polaris_provider::decision::DecisionCapabilities,
        _permit: tokio::sync::OwnedSemaphorePermit,
    },
    Shutdown(oneshot::Sender<Result<(), DecisionError>>),
}

impl ClefDecider {
    pub fn new(config: ClefConfig) -> Result<Self, DecisionError> {
        config.validate()?;
        Ok(Self {
            config,
            actor: Mutex::new(None),
            capacity: Arc::new(Semaphore::new(1)),
        })
    }

    fn sender(&self) -> Result<mpsc::Sender<Job>, DecisionError> {
        let mut held = self.actor.lock().map_err(|_| DecisionError::Unavailable)?;
        if let Some(sender) = held.as_ref() {
            return Ok(sender.clone());
        }
        if self.capacity.is_closed() {
            return Err(DecisionError::Unavailable);
        }
        let runtime =
            tokio::runtime::Handle::try_current().map_err(|_| DecisionError::Unavailable)?;
        let (sender, receiver) = mpsc::channel(1);
        runtime.spawn(run(self.config.clone(), receiver));
        *held = Some(sender.clone());
        Ok(sender)
    }

    /// Close admission, finish the current bounded request, then ask the worker
    /// to exit and wait for its process owner. No automatic restart follows.
    pub async fn shutdown(&self) -> Result<(), DecisionError> {
        self.capacity.close();
        let sender = self
            .actor
            .lock()
            .map_err(|_| DecisionError::Unavailable)?
            .take();
        let Some(sender) = sender else {
            return Ok(());
        };
        let (reply, received) = oneshot::channel();
        sender
            .send(Job::Shutdown(reply))
            .await
            .map_err(|_| DecisionError::Unavailable)?;
        received.await.map_err(|_| DecisionError::Unavailable)?
    }
}

#[async_trait::async_trait]
impl Decider for ClefDecider {
    fn name(&self) -> &str {
        "clef"
    }

    fn identity(&self) -> polaris_provider::decision::DecisionIdentity {
        self.config.identity(true)
    }

    fn capabilities(&self) -> polaris_provider::decision::DecisionCapabilities {
        use polaris_provider::decision::{
            DecisionCancellation, DecisionCapabilities, DecisionLocation, DecisionShapes,
            DecisionUsageReporting,
        };
        DecisionCapabilities {
            network: polaris_provider::NetworkBoundary::LocalOnly,
            location: DecisionLocation::LocalProcess,
            protocol: Some("clef-stdio-v1"),
            shapes: Some(DecisionShapes::ALL),
            max_context_tokens: Some(self.config.context_tokens as usize),
            max_questions: Some(64),
            max_options: Some(255),
            max_frame_bytes: Some(wire::MAX_FRAME),
            cancellation: DecisionCancellation::OwnedProcess,
            usage: DecisionUsageReporting::Required,
            ..Default::default()
        }
    }

    async fn decide(
        &self,
        state: &serde_json::Value,
        questions: &BTreeMap<String, Question>,
    ) -> Result<Decision, DecisionError> {
        self.observe_with_context(state, questions, Default::default())
            .await
            .result
    }
    async fn observe(
        &self,
        state: &serde_json::Value,
        questions: &BTreeMap<String, Question>,
    ) -> polaris_provider::decision::DecisionObservation {
        self.observe_with_context(state, questions, Default::default())
            .await
    }
    async fn observe_with_context(
        &self,
        state: &serde_json::Value,
        questions: &BTreeMap<String, Question>,
        context: polaris_provider::decision::DecisionContext,
    ) -> polaris_provider::decision::DecisionObservation {
        let started = std::time::Instant::now();
        let result = async {
            let permit = self
                .capacity
                .clone()
                .try_acquire_owned()
                .map_err(|_| DecisionError::Unavailable)?;
            wire::validate_request(state, questions)?;
            let (reply, received) = oneshot::channel();
            self.sender()?
                .try_send(Job::Decide {
                    state: state.clone(),
                    questions: questions.clone(),
                    reply,
                    context,
                    capabilities: self.capabilities(),
                    _permit: permit,
                })
                .map_err(|_| DecisionError::Unavailable)?;
            received.await.map_err(|_| DecisionError::Unavailable)
        }
        .await;
        result.unwrap_or_else(|error| {
            polaris_provider::decision::DecisionObservation::single(
                self.config.identity(false),
                self.capabilities(),
                Err(error),
                started.elapsed(),
            )
        })
    }
}

impl ClefConfig {
    fn identity(&self, model_confirmed: bool) -> polaris_provider::decision::DecisionIdentity {
        let labels = self.model_identity.as_ref().filter(|_| model_confirmed);
        polaris_provider::decision::DecisionIdentity {
            backend: "clef".into(),
            model: labels.map(|labels| labels.repository.clone()),
            revision: labels.map(|labels| labels.revision.clone()),
            quantization: labels.map(|labels| labels.quantization.clone()),
        }
    }
    fn validate(&self) -> Result<(), DecisionError> {
        if self.model_identity.as_ref().is_some_and(|labels| {
            let text = |value: &str| {
                !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
            };
            !text(&labels.repository)
                || !text(&labels.quantization)
                || labels.revision.len() != 40
                || !labels
                    .revision
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }) {
            return Err(DecisionError::Malformed);
        }
        if !wire::identifier(&self.generation)
            || !wire::digest(&self.worker_sha256)
            || !wire::digest(&self.model_sha256)
            || !(1024..=16384).contains(&self.context_tokens)
            || !(1000..=120000).contains(&self.request_timeout_ms)
            || !(1000..=300000).contains(&self.load_timeout_ms)
            || self.model_bytes == 0
            || self.model_bytes > 64 * 1024 * 1024 * 1024
        {
            return Err(DecisionError::Malformed);
        }
        for path in [&self.worker, &self.model, &self.private_root] {
            if !path.is_absolute()
                || path.canonicalize().ok().as_ref() != Some(path)
                || path
                    .to_str()
                    .is_none_or(|s| s.len() > 4096 || s.chars().any(char::is_control))
            {
                return Err(DecisionError::Malformed);
            }
        }
        use std::os::unix::fs::MetadataExt;
        let root = self
            .private_root
            .symlink_metadata()
            .map_err(|_| DecisionError::Unavailable)?;
        if !root.is_dir() || root.uid() != unsafe { libc::geteuid() } || root.mode() & 0o7077 != 0 {
            return Err(DecisionError::Malformed);
        }
        let model = self
            .model
            .symlink_metadata()
            .map_err(|_| DecisionError::Unavailable)?;
        if !model.is_file() || model.nlink() != 1 || model.len() != self.model_bytes {
            return Err(DecisionError::Malformed);
        }
        Ok(())
    }
}

struct Session {
    process: ManagedStdio,
    stdin: Option<tokio::process::ChildStdin>,
    stdout: BufReader<tokio::process::ChildStdout>,
    _private: tempfile::TempDir,
    owner: String,
    config: ClefConfig,
    request_id: u64,
}

impl Session {
    fn start(config: ClefConfig, authorized: impl FnMut() -> bool) -> Result<Self, DecisionError> {
        use std::{fs, os::unix::fs::PermissionsExt};
        config.validate()?;
        let private = tempfile::Builder::new()
            .prefix("clef-")
            .tempdir_in(&config.private_root)
            .map_err(|_| DecisionError::Unavailable)?;
        let root = private
            .path()
            .canonicalize()
            .map_err(|_| DecisionError::Unavailable)?;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
            .map_err(|_| DecisionError::Unavailable)?;
        let workspace = root.join("runtime");
        let home = root.join("home");
        let tmp = root.join("tmp");
        for dir in [&workspace, &home, &tmp] {
            fs::create_dir(dir).map_err(|_| DecisionError::Unavailable)?;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
                .map_err(|_| DecisionError::Unavailable)?;
        }
        // Execute the hashed private snapshot, never reopen the mutable source
        // pathname after verifying it. Native worker pins the weights by FD.
        let worker = workspace.join("worker");
        snapshot_worker(&config, &worker)?;
        let runtime_roots = vec![PathBuf::from("/usr/lib"), PathBuf::from("/System/Library")];
        #[cfg(test)]
        let runtime_roots = {
            let mut roots = runtime_roots;
            roots.push(PathBuf::from("/bin"));
            roots
        };
        let policy = SandboxPolicy::isolated(SandboxMode::ReadOnly, &workspace, &runtime_roots)
            .and_then(|p| p.with_isolated_sibling_environment(&home, &tmp))
            .and_then(|p| p.with_isolated_readable_files(std::slice::from_ref(&config.model)))
            .and_then(SandboxPolicy::require_process_containment)
            .map_err(|_| DecisionError::Unavailable)?;
        let (process, pipes) = ManagedStdio::spawn_authorized(
            &policy,
            &worker,
            &[],
            &workspace,
            &BTreeMap::new(),
            authorized,
        )
        .map_err(|error| {
            #[cfg(test)]
            eprintln!("synthetic Clef worker launch failed: {error}");
            #[cfg(not(test))]
            let _ = error;
            DecisionError::Unavailable
        })?
        .ok_or(DecisionError::Cancelled)?;
        let stdin = tokio::process::ChildStdin::from_std(pipes.stdin)
            .map_err(|_| DecisionError::Unavailable)?;
        let stdout = tokio::process::ChildStdout::from_std(pipes.stdout)
            .map_err(|_| DecisionError::Unavailable)?;
        let mut entropy = [0; 16];
        getrandom::fill(&mut entropy).map_err(|_| DecisionError::Unavailable)?;
        let owner = entropy.iter().map(|b| format!("{b:02x}")).collect();
        Ok(Self {
            process,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
            _private: private,
            owner,
            config,
            request_id: 0,
        })
    }

    async fn exchange(
        &mut self,
        request: &serde_json::Value,
        timeout: Duration,
    ) -> Result<Vec<u8>, DecisionError> {
        let mut bytes = serde_json::to_vec(request).map_err(|_| DecisionError::TooLarge)?;
        if bytes.len() > wire::MAX_FRAME {
            return Err(DecisionError::TooLarge);
        }
        bytes.push(b'\n');
        let stdin = self.stdin.as_mut().ok_or(DecisionError::Unavailable)?;
        let protocol = async {
            stdin
                .write_all(&bytes)
                .await
                .map_err(|_| DecisionError::Unavailable)?;
            stdin
                .flush()
                .await
                .map_err(|_| DecisionError::Unavailable)?;
            wire::frame(&mut self.stdout).await
        };
        tokio::pin!(protocol);
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                biased;
                _ = &mut deadline => return Err(DecisionError::Unavailable),
                answer = &mut protocol => return answer,
                end = self.process.wait_and_poll() => {
                    if end.is_some() || self.process.has_io_problem() { return Err(DecisionError::Unavailable); }
                }
            }
        }
    }

    async fn ready(&mut self) -> Result<(), DecisionError> {
        let init = serde_json::json!({"protocol":1,"owner":self.owner,"generation":self.config.generation,
            "model":self.config.model,"model_bytes":self.config.model_bytes,"model_sha256":self.config.model_sha256,
            "context_tokens":self.config.context_tokens,"device":self.config.device.as_str(),"request_timeout_ms":self.config.request_timeout_ms});
        let line = self
            .exchange(&init, Duration::from_millis(self.config.load_timeout_ms))
            .await?;
        wire::ready(&line, &self.owner, &self.config)
    }

    fn header(&mut self, op: &str) -> Result<serde_json::Value, DecisionError> {
        self.request_id = self
            .request_id
            .checked_add(1)
            .ok_or(DecisionError::Unavailable)?;
        Ok(
            serde_json::json!({"protocol":1,"owner":self.owner,"generation":self.config.generation,
            "request_id":self.request_id,"op":op}),
        )
    }

    async fn decide(
        &mut self,
        state: serde_json::Value,
        questions: &BTreeMap<String, Question>,
    ) -> Result<Decision, DecisionError> {
        let mut request = self.header("decide")?;
        request["state"] = state;
        request["questions"] =
            serde_json::to_value(questions).map_err(|_| DecisionError::Malformed)?;
        let line = self
            .exchange(
                &request,
                Duration::from_millis(self.config.request_timeout_ms + 1500),
            )
            .await?;
        wire::decision(
            &line,
            &self.owner,
            &self.config.generation,
            self.request_id,
            questions,
        )
    }

    async fn close(&mut self, graceful: bool) -> Result<(), DecisionError> {
        let mut result = Ok(());
        if graceful {
            let request = self.header("shutdown")?;
            result = match self.exchange(&request, Duration::from_secs(2)).await {
                Ok(line) => {
                    wire::closed(&line, &self.owner, &self.config.generation, self.request_id)
                }
                Err(error) => Err(error),
            };
        }
        self.stdin.take();
        if !graceful || result.is_err() {
            self.process.request_stop();
        }
        let deadline = tokio::time::sleep(Duration::from_secs(2));
        tokio::pin!(deadline);
        let mut stopping = !graceful || result.is_err();
        loop {
            let end = tokio::select! {
                _ = &mut deadline, if !stopping => {
                    self.process.request_stop(); stopping = true; result = Err(DecisionError::Unavailable); continue;
                }
                end = self.process.wait_and_poll() => end,
            };
            if let Some(end) = end {
                return if end == ControlledEnd::StopUnconfirmed {
                    Err(DecisionError::Unavailable)
                } else {
                    result
                };
            }
        }
    }
}

fn snapshot_worker(config: &ClefConfig, target: &std::path::Path) -> Result<(), DecisionError> {
    use std::{
        fs::OpenOptions,
        io::{Read, Write},
        os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    };
    let mut input = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&config.worker)
        .map_err(|_| DecisionError::Unavailable)?;
    let before = input.metadata().map_err(|_| DecisionError::Unavailable)?;
    if !before.is_file()
        || before.nlink() != 1
        || before.len() == 0
        || before.len() > 256 * 1024 * 1024
    {
        return Err(DecisionError::Malformed);
    }
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(target)
        .map_err(|_| DecisionError::Unavailable)?;
    let mut hash = Sha256::new();
    let mut bytes = [0; 65536];
    let mut total = 0u64;
    loop {
        let n = input
            .read(&mut bytes)
            .map_err(|_| DecisionError::Unavailable)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > before.len() {
            return Err(DecisionError::Malformed);
        }
        hash.update(&bytes[..n]);
        output
            .write_all(&bytes[..n])
            .map_err(|_| DecisionError::Unavailable)?;
    }
    let after = input.metadata().map_err(|_| DecisionError::Unavailable)?;
    if total != before.len()
        || before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
        || before.ctime() != after.ctime()
        || before.ctime_nsec() != after.ctime_nsec()
        || format!("{:x}", hash.finalize()) != config.worker_sha256
    {
        return Err(DecisionError::Malformed);
    }
    output.flush().map_err(|_| DecisionError::Unavailable)?;
    std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o500))
        .map_err(|_| DecisionError::Unavailable)?;
    Ok(())
}

fn capacity_report(
    config: &ClefConfig,
    context: polaris_provider::decision::DecisionContext,
    observed: crate::decision_capacity::CapacityObservation,
) -> Option<polaris_provider::decision::DecisionCapacity> {
    use crate::decision_capacity::{CapacityAssessment, RunFootprint, UnverifiedCapacity, assess};
    let admission = config.admission.as_ref()?;
    let colocated = context
        .planned_local_model_bytes
        .and_then(|bytes| admission.policy.colocated_bytes.checked_add(bytes));
    let assessment = match (context.planned_local_model_bytes, colocated) {
        (None, _) => CapacityAssessment::Unverified {
            reason: UnverifiedCapacity::ColocatedModels,
        },
        (Some(_), None) => CapacityAssessment::Unverified {
            reason: UnverifiedCapacity::Arithmetic,
        },
        (Some(_), Some(bytes)) if config.device == ClefDevice::Cpu => {
            let mut policy = admission.policy;
            policy.colocated_bytes = bytes;
            // Full validated peak remains a conservative bound even when warm;
            // no unverified resident-RSS subtraction or VRAM conversion is used.
            assess(
                &admission.profile,
                &observed,
                RunFootprint {
                    context_tokens: config.context_tokens,
                    parallel: 1,
                },
                policy,
                std::time::Instant::now(),
            )
        }
        (Some(_), Some(_)) => CapacityAssessment::Unverified {
            reason: UnverifiedCapacity::Target,
        },
    };
    let (status, reason, budget_bytes, required_bytes) = match assessment {
        CapacityAssessment::Fits {
            budget_bytes,
            required_bytes,
            ..
        } => ("fits", None, Some(budget_bytes), Some(required_bytes)),
        CapacityAssessment::Insufficient {
            budget_bytes,
            required_bytes,
        } => (
            "insufficient",
            None,
            Some(budget_bytes),
            Some(required_bytes),
        ),
        CapacityAssessment::Unverified { reason } => {
            ("unverified", Some(format!("{reason:?}")), None, None)
        }
    };
    Some(polaris_provider::decision::DecisionCapacity {
        status,
        reason,
        budget_bytes,
        required_bytes,
        colocated_bytes: colocated,
    })
}

async fn run(config: ClefConfig, mut jobs: mpsc::Receiver<Job>) {
    let mut session: Option<Session> = None;
    loop {
        let job = if let Some(held) = session.as_mut() {
            tokio::select! {
                biased;
                job = jobs.recv() => job,
                end = held.process.wait_and_poll() => {
                    if end.is_some() || held.process.has_io_problem() { break; }
                    continue;
                }
            }
        } else {
            jobs.recv().await
        };
        match job {
            Some(Job::Decide {
                state,
                questions,
                mut reply,
                context,
                capabilities,
                _permit,
            }) => {
                if reply.is_closed() {
                    continue;
                }
                let started_at = std::time::Instant::now();
                let identity = config.identity(false);
                let capacity = capacity_report(
                    &config,
                    context,
                    crate::decision_capacity::observation::cpu_budget(),
                );
                if capacity
                    .as_ref()
                    .is_some_and(|report| report.status != "fits")
                {
                    let mut observation = polaris_provider::decision::DecisionObservation::single(
                        identity,
                        capabilities,
                        Err(DecisionError::Unavailable),
                        started_at.elapsed(),
                    );
                    observation.attempts[0].invoked = false;
                    observation.attempts[0].reason = Some("capacity_refused".into());
                    observation.attempts[0].capacity = capacity;
                    let _ = reply.send(observation);
                    continue;
                }
                let starting = session.is_none();
                if starting {
                    match Session::start(config.clone(), || !reply.is_closed()) {
                        Ok(held) => session = Some(held),
                        Err(error) => {
                            let mut observation =
                                polaris_provider::decision::DecisionObservation::single(
                                    identity,
                                    capabilities,
                                    Err(error),
                                    started_at.elapsed(),
                                );
                            observation.attempts[0].capacity = capacity;
                            let _ = reply.send(observation);
                            break;
                        }
                    }
                }
                let mut model_confirmed = !starting;
                let result = async {
                    // Retain the process in the actor before the first await.
                    if starting {
                        session.as_mut().expect("started session").ready().await?;
                        model_confirmed = true;
                    }
                    session
                        .as_mut()
                        .expect("started session")
                        .decide(state, &questions)
                        .await
                };
                let result = tokio::select! { biased; _ = reply.closed() => Err(DecisionError::Cancelled), result = result => result };
                let failed = result.is_err();
                if failed && let Some(held) = session.as_mut() {
                    let _ = held.close(false).await;
                }
                let mut observation = polaris_provider::decision::DecisionObservation::single(
                    config.identity(model_confirmed),
                    capabilities,
                    result,
                    started_at.elapsed(),
                );
                observation.attempts[0].capacity = capacity;
                let _ = reply.send(observation);
                if failed {
                    return;
                }
            }
            Some(Job::Shutdown(reply)) => {
                let result = if let Some(held) = session.as_mut() {
                    held.close(true).await
                } else {
                    Ok(())
                };
                let _ = reply.send(result);
                return;
            }
            None => break,
        }
    }
    if let Some(held) = session.as_mut() {
        let _ = held.close(false).await;
    }
}

#[cfg(test)]
mod tests;
