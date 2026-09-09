//! Desktopの待機窓口。worker・結果・回収handleの所有はserviceに置く。
use polaris_desktop_protocol::ids::RunId;
pub use polaris_sandbox::{
    ControlledEnd, ControlledOutcome, PendingCleanup, SandboxMode, SandboxPolicy,
    run_confined_controlled, run_confined_controlled_authorized, take_pending_cleanups,
};
use std::{
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
};
use tokio::sync::{mpsc, oneshot};

pub const CAPACITY: usize = 32;
pub const MAX_REQUEST_BYTES: usize = 256 * 1024;

pub struct ExecutionCommand {
    pub policy: SandboxPolicy,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub stdin: Option<String>,
}

impl ExecutionCommand {
    /// 承認の補足表示。実行値から生成し、programとargsの表示を置き換えない。
    pub fn input_description(&self) -> Option<String> {
        let input = self.stdin.as_ref()?;
        if self.args == ["--confined-apply"] {
            match serde_json::from_str::<polaris_sandbox::Mutation>(input) {
                Ok(polaris_sandbox::Mutation::Write { path, content }) => {
                    return Some(format!("書き込み先: {path:?}（{} bytes）", content.len()));
                }
                Ok(polaris_sandbox::Mutation::Edit { path, old, new }) => {
                    return Some(format!(
                        "編集先: {path:?}（{} → {} bytes）",
                        old.len(),
                        new.len()
                    ));
                }
                Err(_) => {}
            }
        }
        Some(format!("標準入力: {} bytes", input.len()))
    }

    /// 表示用文字列ではなく、実際に起動する値すべてを承認へ束縛する。
    pub fn binding_hash(&self) -> Result<String, String> {
        let roots: Vec<&[u8]> = self
            .policy
            .writable_roots()
            .iter()
            .map(|root| root.as_os_str().as_encoded_bytes())
            .collect();
        let payload = serde_json::to_vec(&(
            self.policy.mode(),
            roots,
            self.policy.isolated_boundary().map(|boundary| {
                (
                    boundary.workspace.as_os_str().as_encoded_bytes(),
                    boundary
                        .readable_roots
                        .iter()
                        .map(|root| root.as_os_str().as_encoded_bytes())
                        .collect::<Vec<_>>(),
                    boundary.environment.as_ref().map(|environment| {
                        (
                            "isolated-env-v1",
                            environment.home.as_os_str().as_encoded_bytes(),
                            environment.tmpdir.as_os_str().as_encoded_bytes(),
                        )
                    }),
                )
            }),
            self.program.as_os_str().as_encoded_bytes(),
            &self.args,
            &self.stdin,
        ))
        .map_err(|error| error.to_string())?;
        Ok(crate::conversation_state::content_hash(&payload))
    }
}

pub use polaris_tools::isolated_read::Request as ConfinedReadRequest;

struct ReadGrant {
    policy: SandboxPolicy,
    helper: PathBuf,
    binding: String,
    identity: std::fs::Metadata,
}
impl ReadGrant {
    fn command(&self, stdin: Option<String>) -> ExecutionCommand {
        ExecutionCommand {
            policy: self.policy.clone(),
            program: self.helper.clone(),
            args: vec!["--confined-read".into()],
            stdin,
        }
    }
    fn validate(&self) -> Result<(), String> {
        let invalid = || "invalid trusted read helper boundary".to_owned();
        if self.policy.mode() != SandboxMode::ReadOnly || !self.policy.writable_roots().is_empty() {
            return Err(invalid());
        }
        self.policy
            .restrict(SandboxMode::ReadOnly, &[])
            .map_err(|_| invalid())?;
        let boundary = self.policy.isolated_boundary().ok_or_else(invalid)?;
        let env = boundary.environment.as_ref().ok_or_else(invalid)?;
        if std::fs::canonicalize(&self.helper).map_err(|_| invalid())? != self.helper
            || !std::fs::metadata(&self.helper)
                .map_err(|_| invalid())?
                .is_file()
            || self.helper.starts_with(&boundary.workspace)
            || self.helper.starts_with(&env.home)
            || self.helper.starts_with(&env.tmpdir)
            || !boundary
                .readable_roots
                .iter()
                .any(|root| root != &boundary.workspace && self.helper.starts_with(root))
        {
            return Err(invalid());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let current = std::fs::metadata(&self.helper).map_err(|_| invalid())?;
            if current.mode() & 0o111 == 0
                || (
                    current.dev(),
                    current.ino(),
                    current.len(),
                    current.mtime(),
                    current.mtime_nsec(),
                    current.ctime(),
                    current.ctime_nsec(),
                ) != (
                    self.identity.dev(),
                    self.identity.ino(),
                    self.identity.len(),
                    self.identity.mtime(),
                    self.identity.mtime_nsec(),
                    self.identity.ctime(),
                    self.identity.ctime_nsec(),
                )
            {
                return Err(invalid());
            }
        }
        Ok(())
    }
}
struct AuthorizedRead {
    run_id: RunId,
    grant: Arc<ReadGrant>,
    binding: String,
    expires: std::time::Instant,
}

/// Only a trusted owner constructs this grant; model commands cannot attach it.
struct MutationGrant {
    helper: Arc<ReadGrant>,
    scope: SandboxPolicy,
}
impl MutationGrant {
    fn policy_for(&self, requested: &SandboxPolicy) -> Result<SandboxPolicy, String> {
        self.helper.validate()?;
        if requested.mode() == SandboxMode::ReadOnly || requested.isolated_boundary().is_none() {
            return Err("copy mutation requires a writable isolated policy".into());
        }
        let actual = requested
            .restrict(SandboxMode::WorkspaceWrite, requested.writable_roots())
            .map_err(|_| "invalid child mutation policy")?;
        let expected = self
            .scope
            .restrict(SandboxMode::WorkspaceWrite, requested.writable_roots())
            .map_err(|_| "mutation exceeds confirmed copy scope")?;
        if actual != expected {
            return Err("mutation policy differs from confirmed boundary".into());
        }
        Ok(actual)
    }
    fn validate_target(
        &self,
        policy: &SandboxPolicy,
        mutation: &polaris_sandbox::Mutation,
    ) -> Result<(), String> {
        let path = match mutation {
            polaris_sandbox::Mutation::Write { path, .. }
            | polaris_sandbox::Mutation::Edit { path, .. } => path,
        };
        let boundary = policy.isolated_boundary().ok_or("missing copy boundary")?;
        if !path.is_absolute()
            || path.components().any(|c| {
                !matches!(
                    c,
                    std::path::Component::RootDir | std::path::Component::Normal(_)
                )
            })
            || !path.starts_with(&boundary.workspace)
            || boundary
                .environment
                .as_ref()
                .is_some_and(|env| path.starts_with(&env.home) || path.starts_with(&env.tmpdir))
            || !matches!(
                polaris_tools::predicate::predict(policy, path),
                polaris_tools::predicate::Verdict::Allowed
            )
        {
            return Err("mutation target is outside ordinary confirmed copy scope".into());
        }
        // Never authorize a symlink path. OS confinement still enforces the
        // narrowed write boundary against changes after this metadata check.
        let mut ancestor = Some(path.as_path());
        while let Some(path) = ancestor {
            match std::fs::symlink_metadata(path) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err("mutation path contains a symlink".into());
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err("mutation path unavailable".into()),
            }
            if path == boundary.workspace {
                break;
            }
            ancestor = path.parent();
        }
        Ok(())
    }
}
struct AuthorizedMutation {
    run_id: RunId,
    grant: Arc<MutationGrant>,
    policy: SandboxPolicy,
    binding: String,
    expires: std::time::Instant,
}

/// Trusted folder ceiling. This does not grant source or host operations.
#[derive(Clone)]
pub enum ConfirmedExecutionCapability {
    FixedHelpersOnly,
    ConfinedCode { scope: SandboxPolicy },
}
struct AuthorizedExecution {
    run_id: RunId,
    grant: Arc<MutationGrant>,
    policy: SandboxPolicy,
    binding: String,
    expires: std::time::Instant,
}

#[derive(Clone)]
pub struct ExecutionPort {
    run_id: RunId,
    tx: mpsc::Sender<ExecutionRequest>,
    read_grant: Option<Arc<ReadGrant>>,
    mutation_grant: Option<Arc<MutationGrant>>,
    execution_ceiling: bool,
    execution_grant: Option<Arc<MutationGrant>>,
}
impl ExecutionPort {
    pub fn channel(run_id: RunId) -> (Self, mpsc::Receiver<ExecutionRequest>) {
        let (tx, rx) = mpsc::channel(CAPACITY);
        (
            Self {
                run_id,
                tx,
                read_grant: None,
                mutation_grant: None,
                execution_ceiling: false,
                execution_grant: None,
            },
            rx,
        )
    }
    /// 信頼済みrun owner専用。通常channelにはこの権限を付けない。
    pub fn channel_with_read_helper(
        run_id: RunId,
        trusted_helper: PathBuf,
        readonly_isolated_policy: SandboxPolicy,
    ) -> Result<(Self, mpsc::Receiver<ExecutionRequest>), String> {
        let identity =
            std::fs::metadata(&trusted_helper).map_err(|_| "trusted read helper unavailable")?;
        let mut grant = ReadGrant {
            policy: readonly_isolated_policy,
            helper: trusted_helper,
            binding: String::new(),
            identity,
        };
        grant.validate()?;
        grant.binding = grant.command(None).binding_hash()?;
        let (mut port, rx) = Self::channel(run_id);
        port.read_grant = Some(Arc::new(grant));
        Ok((port, rx))
    }
    /// Trusted owner entry: an explicit copy scope enables automatic Write/Edit
    /// only. None preserves the read-only grant and ordinary approval behavior.
    pub fn channel_with_read_helper_and_mutations(
        run_id: RunId,
        trusted_helper: PathBuf,
        readonly_isolated_policy: SandboxPolicy,
        mutation_scope: Option<SandboxPolicy>,
    ) -> Result<(Self, mpsc::Receiver<ExecutionRequest>), String> {
        let (mut port, receiver) =
            Self::channel_with_read_helper(run_id, trusted_helper, readonly_isolated_policy)?;
        if let Some(scope) = mutation_scope {
            if scope.mode() == SandboxMode::ReadOnly || scope.writable_roots().is_empty() {
                return Err("read-only policy cannot grant copy mutation".into());
            }
            let helper = port.read_grant.as_ref().unwrap().clone();
            let boundary = helper.policy.isolated_boundary().unwrap();
            if scope
                .writable_roots()
                .iter()
                .any(|root| !root.starts_with(&boundary.workspace))
                || scope
                    .restrict(SandboxMode::ReadOnly, &[])
                    .map_err(|_| "invalid mutation scope")?
                    != helper.policy
            {
                return Err("mutation grant must share the trusted copy boundary".into());
            }
            let scope = scope
                .restrict(SandboxMode::WorkspaceWrite, scope.writable_roots())
                .map_err(|_| "invalid mutation scope")?;
            port.mutation_grant = Some(Arc::new(MutationGrant { helper, scope }));
        }
        Ok((port, receiver))
    }
    /// Explicit opt-in; generic submit cannot acquire this capability.
    pub(crate) fn configure_execution_capability(
        &mut self,
        capability: Option<ConfirmedExecutionCapability>,
    ) -> Result<(), String> {
        let Some(capability) = capability else {
            return Ok(());
        };
        let helper = self
            .read_grant
            .as_ref()
            .ok_or("missing trusted helper")?
            .clone();
        helper.validate()?;
        let grant = match capability {
            ConfirmedExecutionCapability::FixedHelpersOnly => None,
            ConfirmedExecutionCapability::ConfinedCode { scope } => {
                let scope = scope
                    .restrict(SandboxMode::WorkspaceWrite, scope.writable_roots())
                    .map_err(|_| "invalid confined execution scope")?;
                let boundary = helper.policy.isolated_boundary().unwrap();
                if scope
                    .restrict(SandboxMode::ReadOnly, &[])
                    .map_err(|_| "invalid execution boundary")?
                    != helper.policy
                    || scope
                        .writable_roots()
                        .iter()
                        .any(|root| !root.starts_with(&boundary.workspace))
                {
                    return Err("execution scope differs from trusted copy".into());
                }
                Some(Arc::new(MutationGrant { helper, scope }))
            }
        };
        self.execution_ceiling = true;
        self.execution_grant = grant;
        Ok(())
    }
    pub fn has_execution_ceiling(&self) -> bool {
        self.execution_ceiling
    }

    /// Confirmed Build permission applies only to this fixed isolated shell route.
    pub fn submit_confined_execution(
        &self,
        requested_policy: &SandboxPolicy,
        helper: &Path,
        shell_command: &str,
    ) -> Result<ExecutionWait, String> {
        let grant = self
            .execution_grant
            .as_ref()
            .ok_or("folder permission forbids code execution")?;
        if helper != grant.helper.helper {
            return Err("execution helper mismatch".into());
        }
        let policy = grant.policy_for(requested_policy)?;
        let command = ExecutionCommand {
            policy: policy.clone(),
            program: "/bin/sh".into(),
            args: vec!["-c".into(), shell_command.into()],
            stdin: None,
        };
        let stamp = AuthorizedExecution {
            run_id: self.run_id.clone(),
            grant: grant.clone(),
            policy,
            binding: command.binding_hash()?,
            expires: std::time::Instant::now() + std::time::Duration::from_secs(5),
        };
        self.enqueue(command, None, None, Some(stamp))
    }
    pub(crate) fn permits_request(&self, request: &ExecutionRequest, run: &RunId) -> bool {
        if !self.execution_ceiling {
            return true;
        }
        if run != &self.run_id {
            return false;
        }
        request.authorized_read.as_ref().is_some_and(|stamp| {
            self.read_grant
                .as_ref()
                .is_some_and(|grant| Arc::ptr_eq(grant, &stamp.grant))
                && request.command.policy == stamp.grant.policy
                && request.is_authorized_read_for(run)
        }) || request.authorized_mutation.as_ref().is_some_and(|stamp| {
            self.mutation_grant
                .as_ref()
                .is_some_and(|grant| Arc::ptr_eq(grant, &stamp.grant))
                && request.is_authorized_mutation_for(run)
        }) || request.authorized_execution.as_ref().is_some_and(|stamp| {
            self.execution_grant
                .as_ref()
                .is_some_and(|grant| Arc::ptr_eq(grant, &stamp.grant))
                && request.is_authorized_execution_for(run)
        })
    }
    pub fn has_mutation_grant(&self) -> bool {
        self.mutation_grant.is_some()
    }
    /// Build a fixed-helper request. A present but invalid grant never falls
    /// back to interactive approval or an arbitrary executable.
    pub fn submit_confined_mutation(
        &self,
        requested_policy: &SandboxPolicy,
        helper: &Path,
        mutation: &polaris_sandbox::Mutation,
    ) -> Result<ExecutionWait, String> {
        let grant = self
            .mutation_grant
            .as_ref()
            .ok_or("copy mutation grant unavailable")?;
        if helper != grant.helper.helper {
            return Err("mutation helper mismatch".into());
        }
        let policy = grant.policy_for(requested_policy)?;
        let mut mutation = mutation.clone();
        let path = match &mut mutation {
            polaris_sandbox::Mutation::Write { path, .. }
            | polaris_sandbox::Mutation::Edit { path, .. } => path,
        };
        if path.is_relative() {
            *path = policy.isolated_boundary().unwrap().workspace.join(&*path);
        }
        grant.validate_target(&policy, &mutation)?;
        let payload = serde_json::to_string(&mutation).map_err(|_| "invalid copy mutation")?;
        let command = ExecutionCommand {
            policy: policy.clone(),
            program: grant.helper.helper.clone(),
            args: vec!["--confined-apply".into()],
            stdin: Some(payload),
        };
        let stamp = AuthorizedMutation {
            run_id: self.run_id.clone(),
            grant: grant.clone(),
            policy,
            binding: command.binding_hash()?,
            expires: std::time::Instant::now() + std::time::Duration::from_secs(5),
        };
        self.enqueue(command, None, Some(stamp), None)
    }
    pub fn has_read_helper(&self) -> bool {
        self.read_grant.is_some()
    }
    pub fn read_helper_matches(&self, helper: &Path, policy: &SandboxPolicy) -> bool {
        self.read_grant.as_ref().is_some_and(|grant| {
            let command = ExecutionCommand {
                policy: policy.clone(),
                program: helper.into(),
                args: vec!["--confined-read".into()],
                stdin: None,
            };
            command
                .binding_hash()
                .is_ok_and(|hash| hash == grant.binding)
                && grant.validate().is_ok()
        })
    }
    pub fn submit_confined_read(
        &self,
        request: &ConfinedReadRequest,
    ) -> Result<ExecutionWait, String> {
        let grant = self
            .read_grant
            .as_ref()
            .ok_or("trusted read helper not configured")?;
        grant.validate()?;
        if let ConfinedReadRequest::Lines { budget, .. } = request
            && !(1024..=polaris_tools::isolated_read::OUTPUT_BUDGET).contains(budget)
        {
            return Err("invalid isolated read budget".into());
        }
        let payload =
            serde_json::to_string(request).map_err(|_| "invalid isolated read request")?;
        if payload.len() > polaris_tools::isolated_read::REQUEST_LIMIT {
            return Err("isolated read request too large".into());
        }
        let command = grant.command(Some(payload));
        let stamp = AuthorizedRead {
            run_id: self.run_id.clone(),
            grant: grant.clone(),
            binding: command.binding_hash()?,
            expires: std::time::Instant::now() + std::time::Duration::from_secs(5),
        };
        self.enqueue(command, Some(stamp), None, None)
    }
    pub fn run_id(&self) -> &RunId {
        &self.run_id
    }
    /// 即時enqueueだけを行う。登録・intent保存・起動はserviceの直列化点で行う。
    pub fn submit(&self, command: ExecutionCommand) -> Result<ExecutionWait, String> {
        self.enqueue(command, None, None, None)
    }
    fn enqueue(
        &self,
        mut command: ExecutionCommand,
        authorized_read: Option<AuthorizedRead>,
        authorized_mutation: Option<AuthorizedMutation>,
        authorized_execution: Option<AuthorizedExecution>,
    ) -> Result<ExecutionWait, String> {
        if self.execution_ceiling
            && authorized_read.is_none()
            && authorized_mutation.is_none()
            && authorized_execution.is_none()
        {
            return Err("generic execution exceeds confirmed folder ceiling".into());
        }
        let bytes = command
            .program
            .as_os_str()
            .len()
            .saturating_add(command.args.iter().map(String::len).sum::<usize>())
            .saturating_add(command.stdin.as_ref().map_or(0, String::len))
            .saturating_add(
                command
                    .policy
                    .writable_roots()
                    .iter()
                    .map(|root| root.as_os_str().len())
                    .sum::<usize>(),
            )
            .saturating_add(command.policy.isolated_boundary().map_or(0, |boundary| {
                let roots = boundary
                    .readable_roots
                    .iter()
                    .fold(boundary.workspace.as_os_str().len(), |bytes, root| {
                        bytes.saturating_add(root.as_os_str().len())
                    });
                roots.saturating_add(boundary.environment.as_ref().map_or(0, |environment| {
                    environment
                        .home
                        .as_os_str()
                        .len()
                        .saturating_add(environment.tmpdir.as_os_str().len())
                }))
            }));
        if bytes > MAX_REQUEST_BYTES || command.args.len() > 256 {
            return Err("desktop execution request exceeds limit".into());
        }
        command.args.iter_mut().for_each(String::shrink_to_fit);
        command.args.shrink_to_fit();
        if let Some(stdin) = &mut command.stdin {
            stdin.shrink_to_fit();
        }
        command.program.shrink_to_fit();
        let cancelled = Arc::new(AtomicBool::new(false));
        let (reply, rx) = oneshot::channel();
        self.tx
            .try_send(ExecutionRequest {
                command,
                authorized_read,
                authorized_mutation,
                authorized_execution,
                cancelled: cancelled.clone(),
                reply,
            })
            .map_err(|_| "desktop execution owner unavailable or full".to_owned())?;
        Ok(ExecutionWait { rx, cancelled })
    }
}
pub struct ExecutionRequest {
    authorized_read: Option<AuthorizedRead>,
    authorized_mutation: Option<AuthorizedMutation>,
    authorized_execution: Option<AuthorizedExecution>,
    pub command: ExecutionCommand,
    pub cancelled: Arc<AtomicBool>,
    pub reply: oneshot::Sender<Arc<ExecutionResult>>,
}

impl ExecutionRequest {
    pub(crate) fn has_execution_authorization(&self) -> bool {
        self.authorized_execution.is_some()
    }
    pub fn is_authorized_execution_for(&self, run: &RunId) -> bool {
        self.authorized_execution.as_ref().is_some_and(|stamp| {
            &stamp.run_id == run
                && std::time::Instant::now() < stamp.expires
                && self.command.policy == stamp.policy
                && stamp
                    .grant
                    .policy_for(&self.command.policy)
                    .is_ok_and(|policy| policy == stamp.policy)
                && self.command.program == Path::new("/bin/sh")
                && self.command.args.len() == 2
                && self.command.args[0] == "-c"
                && self.command.stdin.is_none()
                && self
                    .command
                    .binding_hash()
                    .is_ok_and(|hash| hash == stamp.binding)
        })
    }
    pub(crate) fn has_mutation_authorization(&self) -> bool {
        self.authorized_mutation.is_some()
    }
    pub fn is_authorized_mutation_for(&self, run_id: &RunId) -> bool {
        self.authorized_mutation.as_ref().is_some_and(|stamp| {
            &stamp.run_id == run_id
                && std::time::Instant::now() < stamp.expires
                && self.command.policy == stamp.policy
                && stamp
                    .grant
                    .policy_for(&self.command.policy)
                    .is_ok_and(|policy| policy == stamp.policy)
                && self.command.program == stamp.grant.helper.helper
                && self.command.args == ["--confined-apply"]
                && self
                    .command
                    .binding_hash()
                    .is_ok_and(|hash| hash == stamp.binding)
                && self.command.stdin.as_ref().is_some_and(|payload| {
                    serde_json::from_str::<polaris_sandbox::Mutation>(payload).is_ok_and(
                        |mutation| {
                            stamp
                                .grant
                                .validate_target(&self.command.policy, &mutation)
                                .is_ok()
                        },
                    )
                })
        })
    }
    pub(crate) fn is_authorized_automatic_for(&self, run_id: &RunId) -> bool {
        self.is_authorized_read_for(run_id)
            || self.is_authorized_mutation_for(run_id)
            || self.is_authorized_execution_for(run_id)
    }
    /// private印・run・全実行値を照合する。引数だけでは認可しない。
    pub fn is_authorized_read_for(&self, run_id: &RunId) -> bool {
        self.authorized_read.as_ref().is_some_and(|stamp| {
            &stamp.run_id == run_id
                && std::time::Instant::now() < stamp.expires
                && stamp.grant.validate().is_ok()
                && self
                    .command
                    .binding_hash()
                    .is_ok_and(|hash| hash == stamp.binding)
        })
    }
}

/// PendingCleanupは待機側へ移さない。本文・EOF・切捨ては別々に保持する。
#[derive(Debug)]
pub struct ExecutionResult {
    pub end: ControlledEnd,
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub stdout_eof: bool,
    pub stderr_eof: bool,
    pub problem: Option<String>,
}
impl ExecutionResult {
    pub fn not_started(problem: String) -> Self {
        Self {
            end: ControlledEnd::CancelledBeforeSpawn,
            stdout_eof: true,
            stderr_eof: true,
            ..Self::failed(problem)
        }
    }
    pub fn from_outcome(mut out: ControlledOutcome) -> (Self, Option<PendingCleanup>) {
        let pending = out.pending.take();
        (
            Self {
                end: out.end,
                status: out.status.and_then(|s| s.code()),
                stdout: out.stdout,
                stderr: out.stderr,
                stdout_truncated: out.stdout_truncated,
                stderr_truncated: out.stderr_truncated,
                stdout_eof: out.stdout_eof,
                stderr_eof: out.stderr_eof,
                problem: out.problem,
            },
            pending,
        )
    }
    pub fn failed(problem: String) -> Self {
        Self {
            end: ControlledEnd::StopUnconfirmed,
            status: None,
            stdout: String::new(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            stdout_eof: false,
            stderr_eof: false,
            problem: Some(problem),
        }
    }
    pub fn text(&self) -> String {
        format!(
            "{}{}\n[execution end={:?} status={:?} stdout_truncated={} stderr_truncated={} stdout_eof={} stderr_eof={} problem={}]",
            self.stdout,
            self.stderr,
            self.end,
            self.status,
            self.stdout_truncated,
            self.stderr_truncated,
            self.stdout_eof,
            self.stderr_eof,
            self.problem.as_deref().unwrap_or("none")
        )
    }
}
pub struct ExecutionWait {
    rx: oneshot::Receiver<Arc<ExecutionResult>>,
    cancelled: Arc<AtomicBool>,
}
impl Future for ExecutionWait {
    type Output = Result<Arc<ExecutionResult>, String>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.rx)
            .poll(cx)
            .map(|r| r.map_err(|_| "desktop execution owner disconnected".into()))
    }
}
impl Drop for ExecutionWait {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn mutation_fixture() -> (
        tempfile::TempDir,
        PathBuf,
        SandboxPolicy,
        RunId,
        ExecutionPort,
        mpsc::Receiver<ExecutionRequest>,
    ) {
        let (dir, helper, readonly) = read_fixture();
        let boundary = readonly.isolated_boundary().unwrap();
        let scope = SandboxPolicy::isolated(
            SandboxMode::WorkspaceWrite,
            &boundary.workspace,
            &[helper.parent().unwrap().into()],
        )
        .unwrap()
        .with_isolated_environment(
            &boundary.environment.as_ref().unwrap().home,
            &boundary.environment.as_ref().unwrap().tmpdir,
        )
        .unwrap();
        let run = RunId::new("mutation-run").unwrap();
        let (port, receiver) = ExecutionPort::channel_with_read_helper_and_mutations(
            run.clone(),
            helper.clone(),
            readonly,
            Some(scope.clone()),
        )
        .unwrap();
        (dir, helper, scope, run, port, receiver)
    }

    #[test]
    fn execution_ceiling_preserves_read_and_rejects_generic_requests() {
        for build in [false, true] {
            let (_dir, helper, scope, run, mut port, mut receiver) = mutation_fixture();
            port.configure_execution_capability(Some(if build {
                ConfirmedExecutionCapability::ConfinedCode {
                    scope: scope.clone(),
                }
            } else {
                ConfirmedExecutionCapability::FixedHelpersOnly
            }))
            .unwrap();
            assert!(port.submit(command()).is_err());
            let _read = port
                .submit_confined_read(&ConfinedReadRequest::DiffBefore { path: "x".into() })
                .unwrap();
            let request = receiver.try_recv().unwrap();
            assert!(port.permits_request(&request, &run));
            let submitted = port.submit_confined_execution(&scope, &helper, "printf fixture");
            assert_eq!(submitted.is_ok(), build);
            if build {
                let request = receiver.try_recv().unwrap();
                assert!(port.permits_request(&request, &run));
                assert!(request.is_authorized_automatic_for(&run));
            }
        }
    }
    #[test]
    fn execution_grant_binds_payload_runtime_scope_helper_run_and_expiry() {
        let (dir, helper, scope, run, mut port, mut receiver) = mutation_fixture();
        let selected = dir
            .path()
            .canonicalize()
            .unwrap()
            .join("workspace/selected");
        std::fs::create_dir(&selected).unwrap();
        let child = scope
            .restrict(SandboxMode::WorkspaceWrite, &[selected])
            .unwrap();
        port.configure_execution_capability(Some(ConfirmedExecutionCapability::ConfinedCode {
            scope: scope.clone(),
        }))
        .unwrap();
        for change in 0..6 {
            let _wait = port
                .submit_confined_execution(&child, &helper, "printf fixture")
                .unwrap();
            let mut request = receiver.try_recv().unwrap();
            assert!(port.permits_request(&request, &run));
            match change {
                0 => request.command.args[1].push_str(" changed"),
                1 => {
                    request.command.policy = child
                        .clone()
                        .with_isolated_runtime_bins(&[helper.parent().unwrap().into()])
                        .unwrap()
                }
                2 => request.command.policy = scope.clone(),
                3 => request.command.program = "/bin/echo".into(),
                4 => {
                    request.authorized_execution.as_mut().unwrap().run_id =
                        RunId::new("different").unwrap()
                }
                _ => {
                    request.authorized_execution.as_mut().unwrap().expires =
                        std::time::Instant::now()
                }
            }
            assert!(!port.permits_request(&request, &run));
            assert!(!request.is_authorized_automatic_for(&run));
        }
        assert!(
            port.submit_confined_execution(&scope, &helper.with_extension("other"), "true")
                .is_err()
        );
        std::fs::write(&helper, "changed helper identity").unwrap();
        assert!(
            port.submit_confined_execution(&scope, &helper, "true")
                .is_err()
        );
    }
    #[test]
    fn execution_grant_normalizes_full_access_and_rejects_expansion() {
        let (_dir, helper, scope, run, mut port, mut receiver) = mutation_fixture();
        let boundary = scope.isolated_boundary().unwrap();
        let env = boundary.environment.as_ref().unwrap();
        let full = SandboxPolicy::isolated(
            SandboxMode::FullAccess,
            &boundary.workspace,
            &[helper.parent().unwrap().into()],
        )
        .unwrap()
        .with_isolated_environment(&env.home, &env.tmpdir)
        .unwrap();
        port.configure_execution_capability(Some(ConfirmedExecutionCapability::ConfinedCode {
            scope: full.clone(),
        }))
        .unwrap();
        let _wait = port
            .submit_confined_execution(&full, &helper, "true")
            .unwrap();
        let request = receiver.try_recv().unwrap();
        assert_eq!(request.command.policy.mode(), SandboxMode::WorkspaceWrite);
        assert!(port.permits_request(&request, &run));
        let readonly = scope.restrict(SandboxMode::ReadOnly, &[]).unwrap();
        assert!(
            port.submit_confined_execution(&readonly, &helper, "true")
                .is_err()
        );
        let changed = scope
            .with_isolated_runtime_bins(&[helper.parent().unwrap().into()])
            .unwrap();
        assert!(
            port.submit_confined_execution(&changed, &helper, "true")
                .is_err()
        );
    }

    #[test]
    fn mutation_grant_binds_payload_run_helper_policy_and_expiry() {
        let (_dir, helper, scope, run, port, mut receiver) = mutation_fixture();
        for variant in 0..7 {
            let mutation = polaris_sandbox::Mutation::Write {
                path: "file".into(),
                content: "ordinary".into(),
            };
            let _waiting = port
                .submit_confined_mutation(&scope, &helper, &mutation)
                .unwrap();
            let mut request = receiver.try_recv().unwrap();
            assert!(request.is_authorized_mutation_for(&run));
            assert!(!request.is_authorized_mutation_for(&RunId::new("other").unwrap()));
            assert!(!request.is_authorized_read_for(&run));
            match variant {
                0 => request.command.stdin = Some("{}".into()),
                1 => request.command.program = "/bin/sh".into(),
                2 => request.command.args = vec!["-c".into(), "true".into()],
                3 => {
                    request.command.policy =
                        SandboxPolicy::new(SandboxMode::FullAccess, &[]).unwrap()
                }
                4 => {
                    request.authorized_mutation.as_mut().unwrap().expires =
                        std::time::Instant::now()
                }
                5 => {
                    // Existing binding_hash omits runtime bins: full structural
                    // policy comparison must independently reject this change.
                    let before = request.command.binding_hash().unwrap();
                    request.command.policy = scope
                        .clone()
                        .with_isolated_runtime_bins(&[helper.parent().unwrap().into()])
                        .unwrap();
                    assert_eq!(before, request.command.binding_hash().unwrap());
                }
                _ => {
                    let stamp = request.authorized_mutation.as_ref().unwrap();
                    assert_eq!(stamp.policy, scope);
                    std::fs::write(&helper, "replacement helper").unwrap();
                }
            }
            assert!(
                !request.is_authorized_mutation_for(&run),
                "variant {variant}"
            );
        }
    }

    #[test]
    fn mutation_grant_enforces_child_scope_and_rejects_sensitive_aliases() {
        let (dir, helper, scope, run, port, mut receiver) = mutation_fixture();
        let workspace = &scope.isolated_boundary().unwrap().workspace;
        let selected = workspace.join("selected");
        std::fs::create_dir(&selected).unwrap();
        let child = scope
            .restrict(SandboxMode::WorkspaceWrite, std::slice::from_ref(&selected))
            .unwrap();
        let mutation = polaris_sandbox::Mutation::Edit {
            path: selected.join("file"),
            old: "a".into(),
            new: "b".into(),
        };
        let _waiting = port
            .submit_confined_mutation(&child, &helper, &mutation)
            .unwrap();
        let request = receiver.try_recv().unwrap();
        assert!(request.is_authorized_mutation_for(&run));
        assert_eq!(request.command.policy, child);
        for path in [
            workspace.join("sibling"),
            selected.join(".env"),
            selected.join("../escape"),
            dir.path().join("source"),
            workspace.join("home/file"),
        ] {
            let mutation = polaris_sandbox::Mutation::Write {
                path,
                content: "x".into(),
            };
            assert!(
                port.submit_confined_mutation(&child, &helper, &mutation)
                    .is_err()
            );
        }
        std::os::unix::fs::symlink(workspace, selected.join("alias")).unwrap();
        let alias = polaris_sandbox::Mutation::Write {
            path: selected.join("alias/file"),
            content: "x".into(),
        };
        assert!(
            port.submit_confined_mutation(&child, &helper, &alias)
                .is_err()
        );
        let readonly = scope.restrict(SandboxMode::ReadOnly, &[]).unwrap();
        assert!(
            port.submit_confined_mutation(&readonly, &helper, &mutation)
                .is_err()
        );
        assert!(
            ExecutionPort::channel_with_read_helper_and_mutations(
                run.clone(),
                helper.clone(),
                readonly.clone(),
                Some(readonly.clone())
            )
            .is_err()
        );
        let (plain, mut ordinary) =
            ExecutionPort::channel_with_read_helper(run, helper.clone(), readonly).unwrap();
        assert!(!plain.has_mutation_grant());
        assert!(
            plain
                .submit_confined_mutation(&scope, &helper, &mutation)
                .is_err()
        );
        let _waiting = plain.submit(request.command).unwrap();
        assert!(!ordinary.try_recv().unwrap().has_mutation_authorization());
    }

    #[test]
    fn mutation_grant_rejects_scope_expansion_and_plain_submit() {
        let (_dir, helper, scope, run, _port, _receiver) = mutation_fixture();
        let workspace = &scope.isolated_boundary().unwrap().workspace;
        let selected = workspace.join("selected");
        std::fs::create_dir(&selected).unwrap();
        let confirmed = scope
            .restrict(SandboxMode::WorkspaceWrite, std::slice::from_ref(&selected))
            .unwrap();
        let (port, mut receiver) = ExecutionPort::channel_with_read_helper_and_mutations(
            run.clone(),
            helper.clone(),
            scope.restrict(SandboxMode::ReadOnly, &[]).unwrap(),
            Some(confirmed.clone()),
        )
        .unwrap();
        let mutation = polaris_sandbox::Mutation::Write {
            path: selected.join("file"),
            content: "x".into(),
        };
        // Even a target inside the confirmed folder cannot carry a wider policy.
        assert!(
            port.submit_confined_mutation(&scope, &helper, &mutation)
                .is_err()
        );
        let _waiting = port
            .submit_confined_mutation(&confirmed, &helper, &mutation)
            .unwrap();
        let request = receiver.try_recv().unwrap();
        assert!(request.is_authorized_mutation_for(&run));
        let _plain = port.submit(request.command).unwrap();
        let plain = receiver.try_recv().unwrap();
        assert!(!plain.has_mutation_authorization());
        assert!(!plain.is_authorized_automatic_for(&run));
    }

    fn command() -> ExecutionCommand {
        ExecutionCommand {
            policy: SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[]).unwrap(),
            program: "/missing".into(),
            args: vec![],
            stdin: None,
        }
    }
    fn read_fixture() -> (tempfile::TempDir, PathBuf, SandboxPolicy) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        for name in ["workspace", "workspace/home", "workspace/tmp", "runtime"] {
            std::fs::create_dir(base.join(name)).unwrap();
        }
        let helper = base.join("runtime/helper");
        std::fs::write(&helper, "dummy helper, never executed").unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let policy = SandboxPolicy::isolated(
            SandboxMode::ReadOnly,
            &base.join("workspace"),
            &[base.join("runtime")],
        )
        .unwrap()
        .with_isolated_environment(&base.join("workspace/home"), &base.join("workspace/tmp"))
        .unwrap();
        (dir, helper, policy)
    }
    #[test]
    fn trusted_read_grant_is_private_and_binds_all_execution_values() {
        let (_dir, helper, policy) = read_fixture();
        let run = RunId::new("read-run").unwrap();
        let (port, mut rx) =
            ExecutionPort::channel_with_read_helper(run.clone(), helper.clone(), policy.clone())
                .unwrap();
        let input = ConfinedReadRequest::Lines {
            path: "dummy".into(),
            offset: 2,
            limit: 3,
            budget: 1024,
        };
        for mutation in 0..7 {
            let _wait = port.submit_confined_read(&input).unwrap();
            let mut request = rx.try_recv().unwrap();
            assert!(request.is_authorized_read_for(&run));
            assert!(!request.is_authorized_read_for(&RunId::new("other").unwrap()));
            match mutation {
                0 => request.command.args.push("--confined-apply".into()),
                1 => request.command.stdin = Some("{}".into()),
                2 => request.command.program = "/bin/sh".into(),
                3 => {
                    request.command.policy =
                        SandboxPolicy::new(SandboxMode::FullAccess, &[]).unwrap()
                }
                4 => {
                    request.command.policy = SandboxPolicy::isolated(
                        SandboxMode::ReadOnly,
                        &policy.isolated_boundary().unwrap().workspace,
                        &[],
                    )
                    .unwrap()
                }
                5 => {
                    let env = policy
                        .isolated_boundary()
                        .unwrap()
                        .environment
                        .as_ref()
                        .unwrap();
                    request.command.policy = policy
                        .clone()
                        .with_isolated_environment(&env.tmpdir, &env.home)
                        .unwrap();
                }
                _ => request.authorized_read.as_mut().unwrap().expires = std::time::Instant::now(),
            }
            assert!(!request.is_authorized_read_for(&run));
        }
        let command = port
            .read_grant
            .as_ref()
            .unwrap()
            .command(Some(serde_json::to_string(&input).unwrap()));
        let _wait = port.submit(command).unwrap();
        assert!(!rx.try_recv().unwrap().is_authorized_read_for(&run));
        let (plain, _) = ExecutionPort::channel(run);
        assert!(plain.submit_confined_read(&input).is_err());
        assert!(
            port.submit_confined_read(&ConfinedReadRequest::Lines {
                path: "dummy".into(),
                offset: 0,
                limit: 1,
                budget: 32769
            })
            .is_err()
        );
        let replacement = helper.with_file_name("replacement");
        std::fs::copy(&helper, &replacement).unwrap();
        std::fs::rename(replacement, &helper).unwrap();
        assert!(port.submit_confined_read(&input).is_err());
    }
    #[test]
    fn trusted_read_grant_rejects_noncanonical_or_unconfined_helper() {
        let (_dir, helper, policy) = read_fixture();
        let run = RunId::new("read-run").unwrap();
        assert!(
            ExecutionPort::channel_with_read_helper(
                run.clone(),
                helper.clone(),
                SandboxPolicy::new(SandboxMode::ReadOnly, &[]).unwrap()
            )
            .is_err()
        );
        let workspace = &policy.isolated_boundary().unwrap().workspace;
        assert!(
            ExecutionPort::channel_with_read_helper(
                run.clone(),
                helper.clone(),
                SandboxPolicy::isolated(
                    SandboxMode::FullAccess,
                    workspace,
                    &[helper.parent().unwrap().into()]
                )
                .unwrap()
                .with_isolated_environment(&workspace.join("home"), &workspace.join("tmp"))
                .unwrap()
            )
            .is_err()
        );
        let inside = workspace.join("helper");
        std::fs::copy(&helper, &inside).unwrap();
        assert!(
            ExecutionPort::channel_with_read_helper(run.clone(), inside, policy.clone()).is_err()
        );
        let alias = helper.with_file_name("alias");
        std::os::unix::fs::symlink(&helper, &alias).unwrap();
        assert!(ExecutionPort::channel_with_read_helper(run, alias, policy).is_err());
    }
    #[test]
    fn approval_hash_binds_arguments_input_program_and_policy() {
        let baseline = command().binding_hash().unwrap();
        let mut changed = command();
        changed.args.push("different".into());
        assert_ne!(baseline, changed.binding_hash().unwrap());
        let mut changed = command();
        changed.stdin = Some("different".into());
        assert_ne!(baseline, changed.binding_hash().unwrap());
        let mut changed = command();
        changed.program = "/another-program".into();
        assert_ne!(baseline, changed.binding_hash().unwrap());
        let mut changed = command();
        changed.policy =
            SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[std::env::temp_dir()]).unwrap();
        assert_ne!(baseline, changed.binding_hash().unwrap());
        assert_eq!(baseline, command().binding_hash().unwrap());
    }
    #[test]
    fn unpolled_wait_drop_cancels_queued_request_only() {
        let (port, mut owner) = ExecutionPort::channel(RunId::new("run").unwrap());
        let wait = port.submit(command()).unwrap();
        drop(wait);
        let request = owner.try_recv().unwrap();
        assert!(request.cancelled.load(Ordering::Acquire));
        assert!(request.reply.is_closed());
    }
    #[test]
    fn approval_hash_binds_isolated_read_boundary_and_cwd() {
        let workspace = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let mut request = command();
        request.policy =
            SandboxPolicy::isolated(SandboxMode::ReadOnly, workspace.path(), &[]).unwrap();
        let original = request.binding_hash().unwrap();
        request.policy = SandboxPolicy::isolated(
            SandboxMode::ReadOnly,
            workspace.path(),
            &[runtime.path().into()],
        )
        .unwrap();
        assert_ne!(original, request.binding_hash().unwrap());
        request.policy = SandboxPolicy::isolated(SandboxMode::ReadOnly, other.path(), &[]).unwrap();
        assert_ne!(original, request.binding_hash().unwrap());
        assert_ne!(original, command().binding_hash().unwrap());
    }
    #[test]
    fn queue_and_payload_are_bounded_and_run_binding_is_checked() {
        let (port, _owner) = ExecutionPort::channel(RunId::new("run").unwrap());
        let waits: Vec<_> = (0..CAPACITY)
            .map(|_| port.submit(command()).unwrap())
            .collect();
        assert!(port.submit(command()).is_err());
        let mut oversized = command();
        oversized.stdin = Some("x".repeat(MAX_REQUEST_BYTES + 1));
        assert!(port.submit(oversized).is_err());
        let (sink, _rx, _) =
            crate::desktop_events::DesktopEventSink::channel(RunId::new("other-run").unwrap());
        assert!(sink.with_execution(port).is_err());
        drop(waits);
    }
}
