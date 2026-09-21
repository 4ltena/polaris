//! TempDirの所有、writerのOS lock、対象別CASと実行意図・終端の公開をまとめる。

use super::{disk, model::*, source_apply::*};
use crate::conversation_state::{RawEventV2, content_hash};
use polaris_desktop_protocol::{
    ids::*,
    request::{Request, RequestBody, RunTarget},
    run_state::{Control, Observation, RunInput, RunState},
    snapshot::{ApprovalDecision, Draft, PendingApproval, Run},
};
use polaris_provider::Message;
use std::{
    fs::{self, File},
    io::Write,
    sync::Arc,
};
use tempfile::TempDir;

pub(super) mod details;
mod history;
pub(super) mod mcp;
pub(super) mod prompt;
pub(super) mod recipe;
pub use details::{RunDetailEntry, RunDetails};
pub use history::OwnerRequestHistory;

/// 任意pathや既存TempDirを受け取る入口を持たない、試作領域の所有者。
#[derive(Debug, Clone)]
pub struct PrototypeRoot {
    backing: StoreRoot,
}

/// Durable owner of an existing private directory. Never deletes it on drop.
#[derive(Debug, Clone)]
pub struct DesktopRoot {
    backing: StoreRoot,
}
#[derive(Debug, Clone)]
struct StoreRoot {
    directory: Arc<disk::Directory>,
    // The prototype alone owns deletion; Writers retain this owner until released.
    _temporary: Option<Arc<TempDir>>,
}
impl PrototypeRoot {
    pub fn new() -> StoreResult<Self> {
        disk::supported()?;
        let mut builder = tempfile::Builder::new();
        builder.prefix("polaris-desktop-v3-");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(fs::Permissions::from_mode(0o700));
        }
        let temporary = Arc::new(builder.tempdir()?);
        let directory = disk::Directory::open_owned(&fs::canonicalize(temporary.path())?)?;
        Ok(Self {
            backing: StoreRoot {
                directory: Arc::new(directory),
                _temporary: Some(temporary),
            },
        })
    }
    pub fn create(
        &self,
        project: ProjectId,
        session: SessionId,
        initial: InitialState,
    ) -> StoreResult<Writer> {
        self.backing.create(project, session, initial)
    }
    pub fn open(&self, project: &ProjectId, session: &SessionId) -> StoreResult<Writer> {
        self.backing.open(project, session)
    }
}
impl DesktopRoot {
    /// Verify and identify the retained private directory without reopening it.
    /// Used to compare native publication coordinates before consuming a Writer.
    pub fn identity(&self) -> StoreResult<(u64, u64)> {
        self.backing.directory.identity()
    }

    pub fn open_owned(path: &std::path::Path) -> StoreResult<Self> {
        Ok(Self {
            backing: StoreRoot {
                directory: Arc::new(disk::Directory::open_owned(path)?),
                _temporary: None,
            },
        })
    }
    pub fn create(
        &self,
        project: ProjectId,
        session: SessionId,
        initial: InitialState,
    ) -> StoreResult<Writer> {
        self.backing.create(project, session, initial)
    }
    pub fn open(&self, project: &ProjectId, session: &SessionId) -> StoreResult<Writer> {
        self.backing.open(project, session)
    }
    /// Only an absent session directory permits initialization, never a missing
    /// marker inside an existing/partially-created session directory.
    pub fn open_or_create(
        &self,
        project: ProjectId,
        session: SessionId,
        initial: InitialState,
    ) -> StoreResult<Writer> {
        match self
            .backing
            .directory
            .child(&content_hash(session.as_str().as_bytes()), false)
        {
            Ok(_) => self.open(&project, &session),
            Err(StoreError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                self.create(project, session, initial)
            }
            Err(e) => Err(e),
        }
    }
}
impl StoreRoot {
    pub fn create(
        &self,
        project: ProjectId,
        session: SessionId,
        initial: InitialState,
    ) -> StoreResult<Writer> {
        let state = Sidecar {
            inherited_turn_count: DecimalU64::new(0),
            conversation_memory: None,
            session_revision: DecimalU64::new(0),
            draft: initial.draft,
            configuration: initial.configuration,
            role_bindings: None,
            tier_bindings: None,
            goal: None,
            instructions: Vec::new(),
            policy_revision: initial.policy_revision,
            tasks: Vec::new(),
            children: Vec::new(),
            runs: Vec::new(),
            requests: Vec::new(),
            source_applies: Vec::new(),
            unresolved_approvals: Vec::new(),
            approval_records: Vec::new(),
            workflow: None,
        };
        self.create_seeded(project, session, state, Vec::new())
    }

    fn create_seeded(
        &self,
        project: ProjectId,
        session: SessionId,
        state: Sidecar,
        raw: Vec<RawEventV2>,
    ) -> StoreResult<Writer> {
        let raw_bytes = raw
            .iter()
            .map(disk::line)
            .collect::<StoreResult<Vec<_>>>()?
            .concat();
        let side = disk::line(&state)?;
        let marker = Marker {
            schema_version: 3,
            session_id: session,
            project_id: project,
            epoch: DecimalU64::new(1),
            session_revision: DecimalU64::new(0),
            content_revision: DecimalU64::new(0),
            raw_offset: DecimalU64::new(raw_bytes.len() as u64),
            raw_hash: content_hash(&raw_bytes),
            sidecar_offset: DecimalU64::new(side.len() as u64),
            sidecar_hash: content_hash(&side),
            deleted: false,
        };
        let published = Published { marker, state, raw };
        validate_ledger(&published)?;
        self.directory.verify()?;
        let directory = Arc::new(self.directory.child(
            &content_hash(published.marker.session_id.as_str().as_bytes()),
            true,
        )?);
        let guard = directory.open("writer.lock", true, true)?;
        acquire(&guard)?;
        let mut file = directory.open(disk::RAW, true, true)?;
        file.write_all(&raw_bytes)?;
        file.sync_all()?;
        let mut file = directory.open(disk::SIDECAR, true, true)?;
        file.write_all(&side)?;
        file.sync_all()?;
        disk::publish_marker(&directory, &published.marker)?;
        self.directory.sync()?;
        Ok(Writer {
            root: self.clone(),
            directory,
            guard,
            published,
            recovery_required: false,
        })
    }

    /// 再openは同期確認後に非終端runを復旧世代として公開する。自動実行しない。
    pub fn open(&self, project: &ProjectId, session: &SessionId) -> StoreResult<Writer> {
        self.directory.verify()?;
        let directory = Arc::new(
            self.directory
                .child(&content_hash(session.as_str().as_bytes()), false)?,
        );
        let guard = directory.open("writer.lock", true, false)?;
        acquire(&guard)?;
        let published = reconcile(&directory, project, session)?;
        self.directory.sync()?;
        if published.marker.deleted {
            return Err(StoreError::Deleted);
        }
        let mut writer = Writer {
            root: self.clone(),
            directory,
            guard,
            published,
            recovery_required: false,
        };
        let mut next = writer.published.clone();
        let mut changed = !next.state.unresolved_approvals.is_empty();
        changed |= invalidate_approvals(&mut next.state, |_| true);
        changed |= super::source_apply::invalidate_unconsumed(&mut next.state);
        for record in &mut next.state.runs {
            if !record.run.state.is_terminal() {
                let unresolved = record.operations.iter().any(mcp::unresolved);
                record.run.state = if unresolved {
                    RunState::OutcomeUnknown
                } else {
                    RunState::Interrupted
                };
                if let Some(memory) = &mut record.memory {
                    memory.phase = polaris_desktop_protocol::snapshot::MemoryPhase::OutcomeUnknown;
                    memory.detail =
                        "前回の実行が中断されました。未確定の要約は自動再送しません".into();
                }
                changed = true;
            }
        }
        for child in &mut next.state.children {
            if !child.child.state.is_terminal() {
                child.child.state = RunState::OutcomeUnknown;
                changed = true;
            }
        }
        if changed {
            writer.commit(next, false)?;
        }
        Ok(writer)
    }
}

fn acquire(file: &File) -> StoreResult<()> {
    match file.try_lock() {
        Ok(()) => Ok(()),
        Err(std::fs::TryLockError::WouldBlock) => Err(StoreError::Busy),
        Err(std::fs::TryLockError::Error(e)) => Err(StoreError::Io(e)),
    }
}

fn reconcile(
    dir: &disk::Directory,
    project: &ProjectId,
    session: &SessionId,
) -> StoreResult<Published> {
    let published = disk::load(dir, project, session)?;
    validate_ledger(&published)?;
    disk::sync_published(dir)?;
    if disk::read_marker(dir)? != published.marker {
        return Err(StoreError::RecoveryRequired);
    }
    Ok(published)
}

fn validate_ledger(p: &Published) -> StoreResult<()> {
    if p.state.inherited_turn_count.get() > 10
        || p.raw.iter().filter(|event| event.starts_turn).count() as u64
            != p.state
                .inherited_turn_count
                .get()
                .checked_add(p.state.runs.len() as u64)
                .ok_or(StoreError::Overflow)?
    {
        return Err(StoreError::Corrupt("原文turnと実行台帳の対応"));
    }
    super::memory::validate(p)?;
    prompt::validate(p)?;
    mcp::validate(p)?;
    super::children::validate(&p.state)?;
    super::source_apply::validate(&p.state)?;
    use std::collections::BTreeSet;
    let mut keys = BTreeSet::new();
    let mut runs = BTreeSet::new();
    let mut attempts = BTreeSet::new();
    let mut operations = BTreeSet::new();
    let mut active = 0;
    for record in &p.state.runs {
        if !runs.insert(&record.run.run_id) || !attempts.insert(&record.run.attempt_id) {
            return Err(StoreError::Corrupt("run/attemptの重複"));
        }
        active += usize::from(!record.run.state.is_terminal());
        for op in &record.operations {
            if !operations.insert(&op.operation_id) {
                return Err(StoreError::Corrupt("operationの重複"));
            }
        }
    }
    if active > 1 {
        return Err(StoreError::Corrupt("複数の非終端親run"));
    }
    for request in &p.state.requests {
        if let RequestResult::SourceApplyResolved { approval_id } = &request.result
            && !p
                .state
                .source_applies
                .iter()
                .any(|r| &r.candidate.approval_id == approval_id && r.decision.is_some())
        {
            return Err(StoreError::Corrupt("source answer reference"));
        }
        if !keys.insert((&request.client_id, &request.request_id))
            || request.accepted_revision > p.marker.session_revision
            || request.request_hash.len() != 64
            || !request.request_hash.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(StoreError::Corrupt("要求台帳のキー・世代・hash"));
        }
        if let RequestResult::RunAccepted { run_id, attempt_id }
        | RequestResult::CancelRequested { run_id, attempt_id } = &request.result
            && !p
                .state
                .runs
                .iter()
                .any(|r| &r.run.run_id == run_id && &r.run.attempt_id == attempt_id)
        {
            return Err(StoreError::Corrupt("要求台帳のrun参照"));
        }
    }
    let mut approval_ids = BTreeSet::new();
    let mut approval_operations = BTreeSet::new();
    for r in &p.state.approval_records {
        let a = &r.pending;
        if !approval_ids.insert(&a.approval_id)
            || !approval_operations.insert(&a.operation_id)
            || !valid_hash(&a.payload_hash)
            || a.operation.is_empty()
            || !p
                .state
                .runs
                .iter()
                .any(|run| run.run.run_id == a.run_id && run.run.attempt_id == a.attempt_id)
            || (r.consumed && r.decision != Some(ApprovalDecision::Allow))
            || (r.consumed
                && !p.state.runs.iter().any(|run| {
                    run.run.run_id == a.run_id
                        && run.run.attempt_id == a.attempt_id
                        && run
                            .operations
                            .iter()
                            .any(|op| op.operation_id == a.operation_id)
                }))
        {
            return Err(StoreError::Corrupt("承認台帳のID・対象・hash・消費"));
        }
        let pending = p
            .state
            .unresolved_approvals
            .iter()
            .filter(|p| p.approval_id == a.approval_id)
            .collect::<Vec<_>>();
        if !r.invalidated && r.decision.is_none() {
            if pending.len() != 1 || pending[0] != a {
                return Err(StoreError::Corrupt("承認pendingの不一致"));
            }
        } else if !pending.is_empty() {
            return Err(StoreError::Corrupt("回答済み承認のpending"));
        }
    }
    for request in &p.state.requests {
        if let RequestResult::ApprovalResolved { approval_id } = &request.result
            && !p
                .state
                .approval_records
                .iter()
                .any(|r| &r.pending.approval_id == approval_id && r.decision.is_some())
        {
            return Err(StoreError::Corrupt("回答の承認参照"));
        }
    }
    Ok(())
}

/// File lockはこのhandleの生存中保持する。Cloneは実装しない。
#[derive(Debug)]
pub struct Writer {
    root: StoreRoot,
    directory: Arc<disk::Directory>,
    guard: File,
    published: Published,
    recovery_required: bool,
}

/// Retains the current writer/root and, for an ancestor, a read-only shared lock.
pub(super) struct MemoryDirectory<'a> {
    pub(super) directory: Arc<disk::Directory>,
    read_guard: Option<File>,
    root: StoreRoot,
    owner: Option<&'a Writer>,
}
impl MemoryDirectory<'_> {
    pub(super) fn verify(&self) -> StoreResult<()> {
        if let Some(owner) = self.owner {
            owner.ready()?;
        }
        self.root.directory.verify()?;
        self.directory.verify()?;
        if let Some(guard) = &self.read_guard {
            disk::same(guard, &self.directory.open("writer.lock", false, false)?)?;
        }
        Ok(())
    }

    /// Move a parent's shared guard across the child's mutable marker commit.
    /// A current-session reader must keep borrowing its exclusive Writer.
    pub(super) fn into_parent(mut self) -> StoreResult<MemoryDirectory<'static>> {
        self.verify()?;
        let guard = self.read_guard.take().ok_or(StoreError::RunConflict)?;
        Ok(MemoryDirectory {
            directory: Arc::clone(&self.directory),
            read_guard: Some(guard),
            root: self.root.clone(),
            owner: None,
        })
    }
}
impl Drop for MemoryDirectory<'_> {
    fn drop(&mut self) {
        if let Some(guard) = &self.read_guard {
            let _ = guard.unlock();
        }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        let _ = self.guard.unlock();
    }
}

impl Writer {
    pub(super) fn memory_directory(&self, session: &SessionId) -> StoreResult<MemoryDirectory<'_>> {
        self.ready()?;
        let (directory, read_guard) = if session == &self.published.marker.session_id {
            (Arc::clone(&self.directory), None)
        } else {
            let directory = Arc::new(
                self.root
                    .directory
                    .child(&content_hash(session.as_str().as_bytes()), false)?,
            );
            let guard = directory.open("writer.lock", false, false)?;
            match guard.try_lock_shared() {
                Ok(()) => {}
                Err(std::fs::TryLockError::WouldBlock) => return Err(StoreError::Busy),
                Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
            }
            (directory, Some(guard))
        };
        let owner = MemoryDirectory {
            directory,
            read_guard,
            root: self.root.clone(),
            owner: Some(self),
        };
        owner.verify()?;
        Ok(owner)
    }

    /// 新規子へ完全な直近原文と固定記憶参照だけを渡す。モデル・RPCは呼ばない。
    pub fn fork_memory(&self, session: SessionId, artifact_hash: &str) -> StoreResult<Writer> {
        self.ready()?;
        let parent = disk::load(&self.directory, self.coordinates().0, self.coordinates().1)?;
        validate_ledger(&parent)?;
        if parent.marker != self.published.marker {
            return Err(StoreError::RecoveryRequired);
        }
        let (state, raw) = super::memory::fork_seed(&parent, &session, artifact_hash)?;
        self.root
            .create_seeded(parent.marker.project_id.clone(), session, state, raw)
    }

    /// Identity of the retained store root, not merely the session coordinates.
    /// Trusted launch uses this to reject a Writer from another directory.
    pub fn root_identity(&self) -> StoreResult<(u64, u64)> {
        self.root.directory.identity()
    }

    /// Immutable owner coordinates, available even when publication needs
    /// recovery. These identify the store; they do not authorize operations.
    pub fn coordinates(&self) -> (&ProjectId, &SessionId) {
        (
            &self.published.marker.project_id,
            &self.published.marker.session_id,
        )
    }
    #[cfg(all(test, unix))]
    pub(super) fn test_directory(&self) -> std::path::PathBuf {
        self.directory.path().to_owned()
    }

    #[cfg(all(test, unix))]
    pub(super) fn test_publish_approval(
        &mut self,
        approval: polaris_desktop_protocol::snapshot::PendingApproval,
    ) {
        let mut next = self.published.clone();
        next.state.unresolved_approvals.push(approval);
        self.commit(next, false).unwrap();
    }

    fn ready(&self) -> StoreResult<()> {
        self.root.directory.verify()?;
        self.directory.verify()?;
        disk::same(
            &self.guard,
            &self.directory.open("writer.lock", false, false)?,
        )?;
        if self.recovery_required {
            return Err(StoreError::RecoveryRequired);
        }
        if self.published.marker.deleted {
            return Err(StoreError::Deleted);
        }
        Ok(())
    }

    pub fn snapshot(&self) -> StoreResult<Published> {
        self.ready()?;
        Ok(self.published.clone())
    }

    /// 同handleの曖昧な公開だけを解決する。操作の再dispatchを許可するAPIではない。
    pub fn recover(&mut self) -> StoreResult<()> {
        self.recovery_required = true;
        self.root.directory.verify()?;
        self.directory.verify()?;
        disk::same(
            &self.guard,
            &self.directory.open("writer.lock", false, false)?,
        )?;
        let current = &self.published.marker;
        let published = reconcile(&self.directory, &current.project_id, &current.session_id)?;
        self.root.directory.sync()?;
        self.published = published;
        self.recovery_required = false;
        self.ready()
    }

    /// Trusted next-run metadata update, including while a run is active.
    /// Accepted runs retain their snapshot; no endpoint or provider is resolved.
    pub fn configure_role_bindings(
        &mut self,
        expected_configuration_revision: DecimalU64,
        role_bindings: Option<super::SavedRoleBindings>,
        tier_bindings: Option<super::SavedTierBindings>,
        catalog: &[polaris_skills::AgentType],
    ) -> StoreResult<()> {
        self.ready()?;
        cas(
            expected_configuration_revision,
            self.published.state.configuration.configuration_revision,
            "configuration",
        )?;
        if let Some(saved) = &role_bindings {
            saved.validate_catalog(catalog)?;
        }
        if let Some(saved) = &tier_bindings {
            saved.validate()?;
        }
        let mut next = self.published.clone();
        next.state.configuration.configuration_revision = next
            .state
            .configuration
            .configuration_revision
            .checked_add(1)?;
        next.state.role_bindings = role_bindings;
        next.state.tier_bindings = tier_bindings;
        self.commit(next, false)
    }

    /// Trusted controller setting; cannot alter a run already accepted.
    pub fn configure_workflow(
        &mut self,
        expected_session_revision: DecimalU64,
        workflow: Option<super::SavedWorkflow>,
    ) -> StoreResult<()> {
        self.ready()?;
        cas(
            expected_session_revision,
            self.published.marker.session_revision,
            "session",
        )?;
        if self
            .published
            .state
            .runs
            .iter()
            .any(|run| !run.run.state.is_terminal())
        {
            return Err(StoreError::Busy);
        }
        if let Some(saved) = &workflow {
            saved.validate()?;
        }
        let mut next = self.published.clone();
        next.state.workflow = workflow;
        self.commit(next, false)
    }

    pub(super) fn commit(&mut self, mut next: Published, content_changed: bool) -> StoreResult<()> {
        self.ready()?;
        super::memory::validate_transition(&self.published.state, &next.state)?;
        mcp::validate_transition(&self.published.state, &next.state)?;
        prompt::validate_transition(&self.published.state, &next.state)?;
        next.marker.session_revision = self.published.marker.session_revision.checked_add(1)?;
        next.state.session_revision = next.marker.session_revision;
        if content_changed {
            next.marker.content_revision = self.published.marker.content_revision.checked_add(1)?;
        }
        validate_ledger(&next)?;
        super::source_apply::validate_transition(&self.published.state, &next.state)?;
        // 事前失敗も含め停止し、未公開tailを次の変更に混ぜない。
        self.recovery_required = true;
        next.marker = disk::commit(&self.directory, &self.published, &next)?;
        self.root.directory.verify()?;
        self.directory.verify()?;
        self.published = next;
        self.recovery_required = false;
        Ok(())
    }

    pub(super) fn memory_raw_prefix(&self, offset: DecimalU64, hash: &str) -> StoreResult<Vec<u8>> {
        self.ready()?;
        if offset > self.published.marker.raw_offset {
            return Err(StoreError::Corrupt("記憶の原文範囲"));
        }
        disk::prefix(&self.directory, disk::RAW, offset, hash)
    }

    fn acceptance(&self, record: &RequestRecord) -> Acceptance {
        let run = match &record.result {
            RequestResult::RunAccepted { run_id, attempt_id }
            | RequestResult::CancelRequested { run_id, attempt_id } => self
                .published
                .state
                .runs
                .iter()
                .find(|r| &r.run.run_id == run_id && &r.run.attempt_id == attempt_id)
                .cloned(),
            _ => None,
        };
        Acceptance {
            record: record.clone(),
            run,
        }
    }

    pub fn request_status(
        &self,
        session: &SessionId,
        client: &ClientId,
        request: &RequestId,
    ) -> StoreResult<Option<Acceptance>> {
        self.ready()?;
        if session != &self.published.marker.session_id {
            return Err(StoreError::TargetMismatch);
        }
        Ok(self
            .published
            .state
            .requests
            .iter()
            .find(|r| &r.client_id == client && &r.request_id == request)
            .map(|r| self.acceptance(r)))
    }

    /// P1の型付き要求を決定的に直列化し、dedupをCASより先に照合する。
    /// run ID/attemptは新規RunStartでだけ必要。同じ要求の再送では無視する。
    pub fn apply(
        &mut self,
        request: &Request,
        target: Option<RunTarget>,
    ) -> StoreResult<Acceptance> {
        self.apply_bound(request, target, None)
    }

    /// The same acceptance, with the registration the run is bound to. The
    /// binding is written by the acceptance itself, never by a later save.
    pub fn apply_bound(
        &mut self,
        request: &Request,
        target: Option<RunTarget>,
        account: Option<super::SavedAccount>,
    ) -> StoreResult<Acceptance> {
        if let Some(account) = &account {
            account.validate()?;
        }
        self.ready()?;
        if request.body.session_id() != Some(&self.published.marker.session_id) {
            return Err(StoreError::TargetMismatch);
        }
        if !matches!(
            request.body,
            RequestBody::DraftUpdate(..)
                | RequestBody::SessionConfigure(..)
                | RequestBody::GoalUpdate(..)
                | RequestBody::InstructionSend(..)
                | RequestBody::RunStart(..)
                | RequestBody::RunCancel(..)
        ) {
            return Err(StoreError::UnsupportedMethod);
        }
        let hash = content_hash(&serde_json::to_vec(request)?);
        if let Some(existing) = self.request_status(
            &self.published.marker.session_id,
            &request.client_id,
            &request.request_id,
        )? {
            if existing.record.request_hash != hash {
                return Err(StoreError::RequestConflict);
            }
            return Ok(existing);
        }
        let mut next = self.published.clone();
        let state = &mut next.state;
        let content_changed = matches!(request.body, RequestBody::RunStart(..));
        let result =
            match &request.body {
                RequestBody::DraftUpdate(_, params) => {
                    if target.is_some() {
                        return Err(StoreError::RunConflict);
                    }
                    cas(
                        params.expected_draft_revision,
                        state.draft.draft_revision,
                        "draft",
                    )?;
                    state.draft = Draft {
                        draft_revision: state.draft.draft_revision.checked_add(1)?,
                        text: params.text.clone(),
                        attachment_ids: params.attachment_ids.clone(),
                    };
                    RequestResult::DraftUpdated {
                        draft_revision: state.draft.draft_revision,
                    }
                }
                RequestBody::SessionConfigure(_, params) => {
                    if target.is_some() {
                        return Err(StoreError::RunConflict);
                    }
                    cas(
                        params.expected_configuration_revision,
                        state.configuration.configuration_revision,
                        "configuration",
                    )?;
                    state.configuration.configuration_revision =
                        state.configuration.configuration_revision.checked_add(1)?;
                    state.configuration.provider = params.provider.clone();
                    state.configuration.model = params.model.clone();
                    state.configuration.effort = params.effort.clone();
                    state.configuration.history_mode = params.history_mode;
                    RequestResult::Configured {
                        configuration_revision: state.configuration.configuration_revision,
                        configuration: state.configuration.clone(),
                    }
                }
                RequestBody::GoalUpdate(_, params) => {
                    if target.is_some() {
                        return Err(StoreError::RunConflict);
                    }
                    use polaris_desktop_protocol::request::GoalOperation;
                    let previous = state.goal.clone();
                    let goal = match (&params.operation, previous) {
                        (GoalOperation::Set { text }, previous) => {
                            if text.trim().is_empty() || text.len() > super::goal::MAX_GOAL_BYTES {
                                return Err(StoreError::Corrupt("goal"));
                            }
                            super::SavedGoal {
                                text: text.clone(),
                                revision: DecimalU64::new(
                                    previous
                                        .as_ref()
                                        .map_or(0, |goal| goal.revision.get())
                                        .checked_add(1)
                                        .ok_or(StoreError::Overflow)?,
                                ),
                                active: true,
                                // A new goal never clears an explicit stop.
                                stopped: previous.is_some_and(|goal| goal.stopped),
                                automatic_used: DecimalU64::new(0),
                            }
                        }
                        (_, None) => return Err(StoreError::NotFound),
                        (operation, Some(previous)) => super::SavedGoal {
                            revision: previous.revision.checked_add(1)?,
                            active: !matches!(operation, GoalOperation::Clear) && previous.active,
                            stopped: match operation {
                                GoalOperation::Stop => true,
                                GoalOperation::Resume => false,
                                _ => previous.stopped,
                            },
                            ..previous
                        },
                    };
                    goal.validate()?;
                    let goal_revision = goal.revision;
                    state.goal = Some(goal);
                    RequestResult::GoalUpdated { goal_revision }
                }
                RequestBody::InstructionSend(_, params) => {
                    if target.is_some() {
                        return Err(StoreError::RunConflict);
                    }
                    if state.instructions.len() >= super::goal::MAX_INSTRUCTIONS {
                        // Applied instructions are already in a run's history,
                        // so the oldest of them makes room. A queue of
                        // instructions nobody took is refused instead.
                        let applied = state
                            .instructions
                            .iter()
                            .position(|held| held.applied_to.is_some())
                            .ok_or(StoreError::Overflow)?;
                        state.instructions.remove(applied);
                    }
                    // The request identity is the instruction identity, so a
                    // resend can never become a second instruction.
                    let instruction = super::SavedInstruction {
                        instruction_id: format!("i-{}", request.request_id.as_str()),
                        text: params.text.clone(),
                        received_revision: state.session_revision.checked_add(1)?,
                        applied_to: None,
                    };
                    instruction.validate()?;
                    if state
                        .instructions
                        .iter()
                        .any(|held| held.instruction_id == instruction.instruction_id)
                    {
                        return Err(StoreError::RequestConflict);
                    }
                    let instruction_id = instruction.instruction_id.clone();
                    state.instructions.push(instruction);
                    RequestResult::InstructionReceived { instruction_id }
                }
                RequestBody::RunStart(_, params) => {
                    if let Some(choice) = &params.mcp_prompt {
                        prompt::validate_choice(choice)?;
                    }
                    if state.runs.iter().any(|r| !r.run.state.is_terminal()) {
                        return Err(StoreError::Busy);
                    }
                    cas(
                        params.expected_draft_revision,
                        state.draft.draft_revision,
                        "draft",
                    )?;
                    cas(
                        params.expected_configuration_revision,
                        state.configuration.configuration_revision,
                        "configuration",
                    )?;
                    cas(
                        params.expected_policy_revision,
                        state.policy_revision,
                        "policy",
                    )?;
                    let target = target.ok_or(StoreError::RunConflict)?;
                    if state.runs.iter().any(|r| {
                        r.run.run_id == target.run_id || r.run.attempt_id == target.attempt_id
                    }) {
                        return Err(StoreError::RunConflict);
                    }
                    let prior = next.raw.last();
                    next.raw.push(RawEventV2 {
                        schema_version: 2,
                        sequence: prior.map_or(Ok(1), |p| {
                            p.sequence.checked_add(1).ok_or(StoreError::Overflow)
                        })?,
                        epoch: next.marker.epoch.get(),
                        turn_id: prior.map_or(Ok(1), |p| {
                            p.turn_id.checked_add(1).ok_or(StoreError::Overflow)
                        })?,
                        starts_turn: true,
                        message: Message::user(state.draft.text.clone()),
                    });
                    state.runs.push(RunRecord {
                        run: Run {
                            run_id: target.run_id.clone(),
                            attempt_id: target.attempt_id.clone(),
                            state: RunState::Queued,
                            task_ids: Vec::new(),
                        },
                        configuration: state.configuration.clone(),
                        role_bindings: state.role_bindings.clone(),
                        tier_bindings: state.tier_bindings.clone(),
                        account,
                        policy_revision: state.policy_revision,
                        input: state.draft.clone(),
                        mcp_prompt: params.mcp_prompt.clone(),
                        operations: Vec::new(),
                        result_id: None,
                        usage: None,
                        memory: None,
                        memory_resources: None,
                        workflow: state.workflow.clone(),
                        history_gap: None,
                    });
                    state.draft = Draft {
                        draft_revision: state.draft.draft_revision.checked_add(1)?,
                        text: String::new(),
                        attachment_ids: Vec::new(),
                    };
                    RequestResult::RunAccepted {
                        run_id: target.run_id,
                        attempt_id: target.attempt_id,
                    }
                }
                RequestBody::RunCancel(_, cancel) => {
                    if target.is_some() {
                        return Err(StoreError::RunConflict);
                    }
                    let record = state
                        .runs
                        .iter_mut()
                        .find(|r| {
                            r.run.run_id == cancel.run_id && r.run.attempt_id == cancel.attempt_id
                        })
                        .ok_or(StoreError::NotFound)?;
                    let before = record.run.state;
                    record.run.state = before
                        .transition(RunInput::Control(Control::Cancel))
                        .map_err(|_| StoreError::RunConflict)?
                        .state();
                    // 未開始の取消には操作結果がなく、台帳と終端を同時公開できる。
                    if before == RunState::Queued {
                        record.result_id = Some(ResultId::new(format!("cancel-{hash}"))?);
                    }
                    invalidate_approvals(state, |a| {
                        a.run_id == cancel.run_id && a.attempt_id == cancel.attempt_id
                    });
                    RequestResult::CancelRequested {
                        run_id: cancel.run_id.clone(),
                        attempt_id: cancel.attempt_id.clone(),
                    }
                }
                _ => return Err(StoreError::UnsupportedMethod),
            };
        let record = RequestRecord {
            client_id: request.client_id.clone(),
            request_id: request.request_id.clone(),
            request_hash: hash,
            accepted_revision: next.marker.session_revision.checked_add(1)?,
            result,
        };
        state.requests.push(record.clone());
        self.commit(next, content_changed)?;
        Ok(self.acceptance(&record))
    }

    /// NewlyPublishedだけが新しい開始意図の耐久保存を示す。実際の認可は上位層の責務。
    pub fn record_intent(
        &mut self,
        target: &RunTarget,
        operation: OperationId,
    ) -> StoreResult<IntentReceipt> {
        let (next, receipt) = self.prepare_unapproved_intent(target, operation)?;
        if receipt == IntentReceipt::NewlyPublished {
            self.commit(next, false)?;
        }
        Ok(receipt)
    }

    fn prepare_unapproved_intent(
        &self,
        target: &RunTarget,
        operation: OperationId,
    ) -> StoreResult<(Published, IntentReceipt)> {
        self.ready()?;
        if self
            .published
            .state
            .approval_records
            .iter()
            .any(|r| r.pending.operation_id == operation)
        {
            return Err(StoreError::RunConflict);
        }
        let index = self.run_index(target)?;
        let record = &self.published.state.runs[index];
        if record
            .operations
            .iter()
            .any(|o| o.operation_id == operation)
        {
            return Ok((self.published.clone(), IntentReceipt::AlreadyRecorded));
        }
        if !record.run.state.permits_start()
            || self
                .published
                .state
                .runs
                .iter()
                .any(|r| r.operations.iter().any(|o| o.operation_id == operation))
        {
            return Err(StoreError::RunConflict);
        }
        let mut next = self.published.clone();
        let record = &mut next.state.runs[index];
        if record.run.state != RunState::Running {
            record.run.state = record
                .run
                .state
                .transition(RunInput::Control(Control::Start))
                .map_err(|_| StoreError::RunConflict)?
                .state();
        }
        record.operations.push(Operation {
            operation_id: operation,
            result_id: None,
            mcp: None,
            prompt: None,
            recipe: None,
        });
        Ok((next, IntentReceipt::NewlyPublished))
    }

    pub fn record_operation_result(
        &mut self,
        target: &RunTarget,
        operation: &OperationId,
        result: ResultId,
    ) -> StoreResult<()> {
        self.ready()?;
        let index = self.run_index(target)?;
        let record = &self.published.state.runs[index];
        let op = record
            .operations
            .iter()
            .position(|o| &o.operation_id == operation)
            .ok_or(StoreError::NotFound)?;
        if let Some(existing) = &record.operations[op].result_id {
            return if existing == &result {
                Ok(())
            } else {
                Err(StoreError::RunConflict)
            };
        }
        if record.run.state.is_terminal() {
            return Err(StoreError::RunConflict);
        }
        let mut next = self.published.clone();
        next.state.runs[index].operations[op].result_id = Some(result);
        self.commit(next, false)
    }

    /// Publish the owner's cumulative meter, including failure or cancellation.
    /// Repeated checkpoints do not add counts; older observations cannot erase them.
    pub fn record_usage(
        &mut self,
        target: &RunTarget,
        report: polaris_provider::UsageReport,
    ) -> StoreResult<()> {
        self.ready()?;
        let index = self.run_index(target)?;
        if let Some(prior) = self.published.state.runs[index].usage {
            if prior == report {
                return Ok(());
            }
            if report.reported_responses < prior.reported_responses
                || report.missing_responses < prior.missing_responses
                || report.failed_requests < prior.failed_requests
                || report.usage.input_tokens < prior.usage.input_tokens
                || report.usage.output_tokens < prior.usage.output_tokens
                || report.usage.total_tokens < prior.usage.total_tokens
                || report.usage.cached_tokens < prior.usage.cached_tokens
            {
                return Err(StoreError::RunConflict);
            }
        }
        let mut next = self.published.clone();
        next.state.runs[index].usage = Some(report);
        self.commit(next, false)
    }

    /// Publish a bounded diagnostic before terminal publication. If this fails,
    /// the owner must retain recovery-required state and cannot announce ready.
    /// Sets or replaces the goal. A stop stays until an explicit resume, so a
    /// new goal does not silently restart anything.
    pub fn set_goal(&mut self, text: String) -> StoreResult<u64> {
        self.ready()?;
        if text.trim().is_empty() || text.len() > super::goal::MAX_GOAL_BYTES {
            return Err(StoreError::Corrupt("goal"));
        }
        let mut next = self.published.clone();
        let revision = next
            .state
            .goal
            .as_ref()
            .map_or(0, |goal| goal.revision.get());
        let goal = super::SavedGoal {
            text,
            revision: DecimalU64::new(revision.checked_add(1).ok_or(StoreError::Overflow)?),
            active: true,
            stopped: next.state.goal.as_ref().is_some_and(|goal| goal.stopped),
            automatic_used: DecimalU64::new(0),
        };
        goal.validate()?;
        let revision = goal.revision.get();
        next.state.goal = Some(goal);
        self.commit(next, false)?;
        Ok(revision)
    }

    /// Clears, stops or resumes the goal. Clearing keeps the revision so an
    /// older run cannot be treated as still pursuing it.
    pub fn update_goal(&mut self, active: bool, stopped: bool) -> StoreResult<u64> {
        self.ready()?;
        let mut next = self.published.clone();
        let goal = next.state.goal.as_mut().ok_or(StoreError::NotFound)?;
        goal.revision = goal.revision.checked_add(1)?;
        goal.active = active && !goal.text.trim().is_empty();
        goal.stopped = stopped;
        let revision = goal.revision.get();
        let goal = goal.clone();
        goal.validate()?;
        self.commit(next, false)?;
        Ok(revision)
    }

    /// Spends one automatic continuation. Refused once the budget is gone,
    /// so a restart cannot hand out a second one.
    pub fn record_automatic_resume(&mut self) -> StoreResult<u64> {
        self.ready()?;
        let mut next = self.published.clone();
        let goal = next.state.goal.as_mut().ok_or(StoreError::NotFound)?;
        if goal.automatic_used.get() >= crate::resume::MAX_AUTOMATIC {
            return Err(StoreError::RunConflict);
        }
        goal.automatic_used = goal.automatic_used.checked_add(1)?;
        let used = goal.automatic_used.get();
        self.commit(next, false)?;
        Ok(used)
    }

    /// Durable on receipt. Applying it is a separate, later transition.
    pub fn receive_instruction(&mut self, id: String, text: String) -> StoreResult<()> {
        self.ready()?;
        let mut next = self.published.clone();
        if next.state.instructions.len() >= super::goal::MAX_INSTRUCTIONS {
            let applied = next
                .state
                .instructions
                .iter()
                .position(|held| held.applied_to.is_some())
                .ok_or(StoreError::Overflow)?;
            next.state.instructions.remove(applied);
        }
        if next
            .state
            .instructions
            .iter()
            .any(|instruction| instruction.instruction_id == id)
        {
            // The same instruction is never stored twice, so a resend cannot
            // be applied twice either.
            return Ok(());
        }
        let instruction = super::SavedInstruction {
            instruction_id: id,
            text,
            received_revision: next.state.session_revision.checked_add(1)?,
            applied_to: None,
        };
        instruction.validate()?;
        next.state.instructions.push(instruction);
        self.commit(next, false)
    }

    /// Marks exactly the instructions a run took in, at its apply boundary.
    pub fn apply_instructions(&mut self, target: &RunTarget, ids: &[String]) -> StoreResult<()> {
        self.ready()?;
        self.run_index(target)?;
        let mut next = self.published.clone();
        for id in ids {
            let instruction = next
                .state
                .instructions
                .iter_mut()
                .find(|instruction| &instruction.instruction_id == id)
                .ok_or(StoreError::NotFound)?;
            // The same attempt never applies one twice; a later attempt may
            // take over one an abandoned attempt had taken.
            if instruction
                .applied_to
                .as_ref()
                .is_some_and(|applied| applied.attempt_id == target.attempt_id)
            {
                return Err(StoreError::RunConflict);
            }
            instruction.applied_to = Some(super::AppliedInstruction {
                run_id: target.run_id.clone(),
                attempt_id: target.attempt_id.clone(),
            });
        }
        self.commit(next, false)
    }

    pub fn record_history_gap(&mut self, target: &RunTarget, gap: HistoryGap) -> StoreResult<()> {
        self.ready()?;
        let index = self.run_index(target)?;
        let record = &self.published.state.runs[index];
        if record.history_gap == Some(gap) {
            return Ok(());
        }
        if record.run.state.is_terminal() {
            return Err(StoreError::RunConflict);
        }
        let mut next = self.published.clone();
        next.state.runs[index].history_gap = Some(gap);
        self.commit(next, false)
    }

    /// 全開始意図の結果参照を確認し、終端と結果参照を一緒に公開する。
    pub fn finish(
        &mut self,
        target: &RunTarget,
        outcome: Observation,
        result: ResultId,
    ) -> StoreResult<()> {
        self.ready()?;
        let index = self.run_index(target)?;
        let record = &self.published.state.runs[index];
        if record.history_gap.is_some() && outcome == Observation::Succeeded {
            return Err(StoreError::RunConflict);
        }
        if record.run.state.is_terminal() {
            return if record.run.state == outcome.state()
                && record.result_id.as_ref() == Some(&result)
            {
                Ok(())
            } else {
                Err(StoreError::RunConflict)
            };
        }
        if record.operations.iter().any(|o| o.result_id.is_none())
            && outcome != Observation::OutcomeUnknown
        {
            return Err(StoreError::RunConflict);
        }
        let state = record
            .run
            .state
            .transition(RunInput::Observed(outcome))
            .map_err(|_| StoreError::RunConflict)?
            .state();
        let mut next = self.published.clone();
        super::children::seal(&mut next.state, target, outcome)?;
        next.state.runs[index].run.state = state;
        next.state.runs[index].result_id = Some(result);
        next.state
            .unresolved_approvals
            .retain(|a| a.run_id != target.run_id || a.attempt_id != target.attempt_id);
        invalidate_approvals(&mut next.state, |a| {
            a.run_id == target.run_id && a.attempt_id == target.attempt_id
        });
        self.commit(next, false)
    }

    /// Persist a child start before exposing its event. Replays never authorize spawning.
    pub fn record_child_start(
        &mut self,
        target: &RunTarget,
        mut child: polaris_desktop_protocol::snapshot::Child,
        agent_type: String,
        task: String,
    ) -> StoreResult<IntentReceipt> {
        self.ready()?;
        let index = self.run_index(target)?;
        if child.state != RunState::Running {
            return Err(StoreError::RunConflict);
        }
        if let Some(existing) = self
            .published
            .state
            .children
            .iter()
            .find(|c| c.child.run_id == child.run_id)
        {
            if child
                .display_name
                .as_ref()
                .is_some_and(|name| existing.child.display_name.as_ref() != Some(name))
            {
                return Err(StoreError::RunConflict);
            }
            child.display_name = existing.child.display_name.clone();
            let saved = super::SavedChild {
                root: target.clone(),
                child,
                agent_type,
                task,
            };
            let mut replay = existing.clone();
            replay.child.state = RunState::Running;
            return if replay == saved {
                Ok(IntentReceipt::AlreadyRecorded)
            } else {
                Err(StoreError::RunConflict)
            };
        }
        if child.display_name.is_some() {
            return Err(StoreError::RunConflict);
        }
        let root_state = self.published.state.runs[index].run.state;
        if root_state == RunState::Queued || root_state.is_terminal() {
            return Err(StoreError::RunConflict);
        }
        let used_names = self
            .published
            .state
            .children
            .iter()
            .filter(|saved| saved.root == *target || !saved.child.state.is_terminal())
            .filter_map(|saved| saved.child.display_name.as_deref());
        let display_name =
            super::star_names::first_available(used_names).ok_or(StoreError::RunConflict)?;
        child.display_name = Some(display_name.to_owned());
        let saved = super::SavedChild {
            root: target.clone(),
            child,
            agent_type,
            task,
        };
        let mut next = self.published.clone();
        next.state.children.push(saved);
        super::children::validate(&next.state).map_err(|_| StoreError::RunConflict)?;
        self.commit(next, false)?;
        Ok(IntentReceipt::NewlyPublished)
    }

    /// Call after observing the child's terminal, or after joining when its result is unknown.
    /// A receipt is returned only after durable publication; task acceptance is unchanged.
    pub fn record_child_finish(
        &mut self,
        target: &RunTarget,
        identity: &crate::desktop_events::ChildIdentity,
        outcome: Observation,
    ) -> StoreResult<IntentReceipt> {
        self.ready()?;
        let root = self.run_index(target)?;
        let index = self
            .published
            .state
            .children
            .iter()
            .position(|c| {
                c.root == *target
                    && c.child.run_id == identity.run_id
                    && c.child.attempt_id == identity.attempt_id
                    && c.child.parent_run_id == identity.parent_run_id
            })
            .ok_or(StoreError::RunConflict)?;
        let state = self.published.state.children[index].child.state;
        if state.is_terminal() {
            return if state == outcome.state() {
                Ok(IntentReceipt::AlreadyRecorded)
            } else {
                Err(StoreError::RunConflict)
            };
        }
        if self.published.state.runs[root].run.state.is_terminal() {
            return Err(StoreError::RunConflict);
        }
        let mut next = self.published.clone();
        next.state.children[index].child.state = outcome.state();
        self.commit(next, false)?;
        Ok(IntentReceipt::NewlyPublished)
    }

    /// 所有者が観測した単一操作の結果・assistant原文・終端を同じmarkerで公開する。
    /// fake以外の操作を一括で成功扱いにせず、指定外の未解決意図は拒否する。
    pub fn finish_with_text(
        &mut self,
        target: &RunTarget,
        operation: &OperationId,
        outcome: Observation,
        result: ResultId,
        text: String,
    ) -> StoreResult<()> {
        self.finish_with_messages(
            target,
            operation,
            outcome,
            result,
            vec![Message::assistant(text)],
        )
    }

    /// Publish the observed assistant/tool suffix with its terminal marker.
    /// The accepted user message is already durable; callers must not submit it again.
    /// Incomplete tool calls may survive an interrupted turn, but never a success.
    pub fn finish_with_messages(
        &mut self,
        target: &RunTarget,
        operation: &OperationId,
        outcome: Observation,
        result: ResultId,
        messages: Vec<Message>,
    ) -> StoreResult<()> {
        self.ready()?;
        let index = self.run_index(target)?;
        let workflow = self.published.state.runs[index].workflow.clone();
        self.finish_with_messages_and_workflow(
            target, operation, outcome, result, messages, workflow,
        )
    }

    /// Atomically acknowledge one workflow transition and its complete raw
    /// suffix. The worker always supplies the complete suffix; this owner
    /// appends only the tail beyond its durable cursor.
    pub fn checkpoint_workflow(
        &mut self,
        target: &RunTarget,
        full_turn_suffix: Vec<Message>,
        previous: super::SavedWorkflow,
        candidate: super::SavedWorkflow,
        tool_call_id: Option<&str>,
    ) -> StoreResult<()> {
        self.ready()?;
        previous.validate()?;
        candidate.validate()?;
        validate_turn_suffix(&full_turn_suffix, Observation::Cancelled)?;
        if let Some(id) = tool_call_id
            && !full_turn_suffix
                .iter()
                .any(|message| message.tool_call_id.as_deref() == Some(id))
        {
            return Err(StoreError::RunConflict);
        }
        if previous.config != candidate.config
            || !same_or_staled_gates(&previous.gates, &candidate.gates)
            || (candidate.state.revision == previous.state.revision
                && candidate.state != previous.state)
            || (candidate.state.revision != previous.state.revision
                && candidate.state.revision
                    != previous
                        .state
                        .revision
                        .checked_add(1)
                        .ok_or(StoreError::Overflow)?)
        {
            return Err(StoreError::RunConflict);
        }
        let index = self.run_index(target)?;
        let record = &self.published.state.runs[index];
        if record.run.state.is_terminal() {
            return if record.workflow.as_ref() == Some(&candidate)
                && same_messages(
                    &saved_turn_suffix(&self.published, self.published.raw_run_index(index)?)?,
                    &full_turn_suffix,
                )? {
                Ok(())
            } else {
                Err(StoreError::RunConflict)
            };
        }
        if record.workflow.as_ref() == Some(&candidate)
            && same_messages(
                &saved_turn_suffix(&self.published, self.published.raw_run_index(index)?)?,
                &full_turn_suffix,
            )?
        {
            return Ok(());
        }
        let saved = record.workflow.as_ref().ok_or(StoreError::RunConflict)?;
        if !checkpoint_base_matches(saved, &previous) {
            return Err(StoreError::RunConflict);
        }
        let mut next = self.published.clone();
        let tail = turn_suffix_tail(&next, next.raw_run_index(index)?, &full_turn_suffix)?;
        let raw_index = next.raw_run_index(index)?;
        append_turn_suffix(&mut next.raw, raw_index, tail)?;
        next.state.runs[index].workflow = Some(candidate.clone());
        next.state.workflow = Some(candidate);
        self.commit(next, true)
    }

    /// Reconcile the durable marker after an uncertain ACK, without dispatching
    /// or appending anything. The owner handle binds project/session/epoch;
    /// target plus the exact saved workflow and full suffix bind the checkpoint.
    pub fn confirm_workflow_checkpoint(
        &mut self,
        target: &RunTarget,
        full_turn_suffix: &[Message],
        candidate: &super::SavedWorkflow,
    ) -> StoreResult<()> {
        self.recover()?;
        let index = self.run_index(target)?;
        if self.published.state.runs[index].workflow.as_ref() == Some(candidate)
            && same_messages(
                &saved_turn_suffix(&self.published, self.published.raw_run_index(index)?)?,
                full_turn_suffix,
            )?
        {
            Ok(())
        } else {
            Err(StoreError::RunConflict)
        }
    }

    /// Publish raw output and workflow evidence under the same durable marker.
    /// Replay compares the checkpoint belonging to this run, not a newer turn.
    pub fn finish_with_messages_and_workflow(
        &mut self,
        target: &RunTarget,
        operation: &OperationId,
        outcome: Observation,
        result: ResultId,
        messages: Vec<Message>,
        workflow: Option<super::SavedWorkflow>,
    ) -> StoreResult<()> {
        self.ready()?;
        if let Some(saved) = &workflow {
            saved.validate()?;
        }
        validate_turn_suffix(&messages, outcome)?;
        let index = self.run_index(target)?;
        let record = &self.published.state.runs[index];
        if record.history_gap.is_some() && outcome == Observation::Succeeded {
            return Err(StoreError::RunConflict);
        }
        if record.run.state.is_terminal() {
            if !record
                .operations
                .iter()
                .any(|o| &o.operation_id == operation && o.result_id.as_ref() == Some(&result))
            {
                return Err(StoreError::RunConflict);
            }
            let same_body = same_messages(
                &saved_turn_suffix(&self.published, self.published.raw_run_index(index)?)?,
                &messages,
            )?;
            return if record.run.state == outcome.state()
                && record.result_id.as_ref() == Some(&result)
                && same_body
                && record.workflow == workflow
            {
                Ok(())
            } else {
                Err(StoreError::RunConflict)
            };
        }
        let op = record
            .operations
            .iter()
            .position(|o| &o.operation_id == operation)
            .ok_or(StoreError::NotFound)?;
        if (record
            .operations
            .iter()
            .enumerate()
            .any(|(i, o)| i != op && o.result_id.is_none())
            && outcome != Observation::OutcomeUnknown)
            || record.operations[op]
                .result_id
                .as_ref()
                .is_some_and(|r| r != &result)
        {
            return Err(StoreError::RunConflict);
        }
        let state = record
            .run
            .state
            .transition(RunInput::Observed(outcome))
            .map_err(|_| StoreError::RunConflict)?
            .state();
        let mut next = self.published.clone();
        super::children::seal(&mut next.state, target, outcome)?;
        let tail = turn_suffix_tail(&next, next.raw_run_index(index)?, &messages)?;
        let raw_index = next.raw_run_index(index)?;
        append_turn_suffix(&mut next.raw, raw_index, tail)?;
        let record = &mut next.state.runs[index];
        record.operations[op].result_id = Some(result.clone());
        record.run.state = state;
        record.result_id = Some(result);
        record.workflow = workflow.clone();
        next.state.workflow = workflow;
        next.state
            .unresolved_approvals
            .retain(|a| a.run_id != target.run_id || a.attempt_id != target.attempt_id);
        invalidate_approvals(&mut next.state, |a| {
            a.run_id == target.run_id && a.attempt_id == target.attempt_id
        });
        self.commit(next, true)
    }

    /// pendingの耐久保存が完了した場合だけ通知可能な値を返す。
    pub fn publish_approval(
        &mut self,
        approval: PendingApproval,
        now_ms: u64,
    ) -> StoreResult<PendingApproval> {
        self.ready()?;
        let target = RunTarget {
            run_id: approval.run_id.clone(),
            attempt_id: approval.attempt_id.clone(),
        };
        self.check_approval(&approval, &target, now_ms)?;
        if !valid_hash(&approval.payload_hash)
            || approval.operation.is_empty()
            || self.published.state.approval_records.iter().any(|r| {
                r.pending.approval_id == approval.approval_id
                    || r.pending.operation_id == approval.operation_id
            })
            || self.published.state.unresolved_approvals.iter().any(|a| {
                a.approval_id == approval.approval_id || a.operation_id == approval.operation_id
            })
            || self.published.state.runs.iter().any(|r| {
                r.operations
                    .iter()
                    .any(|o| o.operation_id == approval.operation_id)
            })
        {
            return Err(StoreError::RunConflict);
        }
        let mut next = self.published.clone();
        next.state.unresolved_approvals.push(approval.clone());
        next.state.approval_records.push(ApprovalRecord {
            pending: approval.clone(),
            decision: None,
            consumed: false,
            invalidated: false,
        });
        self.commit(next, false)?;
        Ok(approval)
    }

    fn check_approval(
        &self,
        a: &PendingApproval,
        target: &RunTarget,
        now_ms: u64,
    ) -> StoreResult<()> {
        let index = self.run_index(target)?;
        if a.run_id != target.run_id
            || a.attempt_id != target.attempt_id
            || !self.published.state.runs[index].run.state.permits_start()
            || now_ms >= a.expires_at_unix_ms.get()
        {
            return Err(StoreError::RunConflict);
        }
        cas(
            self.published.state.runs[index].policy_revision,
            self.published.state.policy_revision,
            "run policy",
        )?;
        cas(
            a.policy_revision,
            self.published.state.policy_revision,
            "policy",
        )
    }

    /// now_msはservice所有者の時計。IPC要求から取得してはならない。
    pub fn resolve_approval(&mut self, request: &Request, now_ms: u64) -> StoreResult<Acceptance> {
        self.ready()?;
        let RequestBody::ApprovalResolve(session, params) = &request.body else {
            return Err(StoreError::UnsupportedMethod);
        };
        if session != &self.published.marker.session_id {
            return Err(StoreError::TargetMismatch);
        }
        let hash = content_hash(&serde_json::to_vec(request)?);
        if let Some(existing) =
            self.request_status(session, &request.client_id, &request.request_id)?
        {
            if existing.record.request_hash != hash {
                return Err(StoreError::RequestConflict);
            }
            return Ok(existing);
        }
        let index = self
            .published
            .state
            .approval_records
            .iter()
            .position(|r| r.pending.approval_id == params.approval_id)
            .ok_or(StoreError::NotFound)?;
        let ledger = &self.published.state.approval_records[index];
        let a = &ledger.pending;
        if ledger.invalidated
            || ledger.decision.is_some()
            || !self.published.state.unresolved_approvals.contains(a)
            || !a.display.choices.contains(&params.decision)
        {
            return Err(StoreError::RunConflict);
        }
        cas(params.policy_revision, a.policy_revision, "approval policy")?;
        self.check_approval(
            a,
            &RunTarget {
                run_id: params.run_id.clone(),
                attempt_id: params.attempt_id.clone(),
            },
            now_ms,
        )?;
        let mut next = self.published.clone();
        next.state.approval_records[index].decision = Some(params.decision);
        next.state
            .unresolved_approvals
            .retain(|a| a.approval_id != params.approval_id);
        let record = RequestRecord {
            client_id: request.client_id.clone(),
            request_id: request.request_id.clone(),
            request_hash: hash,
            accepted_revision: next.marker.session_revision.checked_add(1)?,
            result: RequestResult::ApprovalResolved {
                approval_id: params.approval_id.clone(),
            },
        };
        next.state.requests.push(record.clone());
        self.commit(next, false)?;
        Ok(self.acceptance(&record))
    }

    /// 許可消費とintentを同じmarkerで公開する。slot登録・draining照合は所有者が直列化する。
    #[allow(clippy::too_many_arguments)]
    pub fn consume_approval_intent(
        &mut self,
        target: &RunTarget,
        approval: &ApprovalId,
        operation: &OperationId,
        operation_kind: &str,
        scope: &str,
        payload_hash: &str,
        now_ms: u64,
    ) -> StoreResult<IntentReceipt> {
        let (next, receipt) = self.prepare_approval_intent(
            target,
            approval,
            operation,
            operation_kind,
            scope,
            payload_hash,
            now_ms,
        )?;
        if receipt == IntentReceipt::NewlyPublished {
            self.commit(next, false)?;
        }
        Ok(receipt)
    }

    /// 許可消費とintentを同じmarkerで公開する。slot登録・draining照合は所有者が直列化する。
    #[allow(clippy::too_many_arguments)]
    fn prepare_approval_intent(
        &self,
        target: &RunTarget,
        approval: &ApprovalId,
        operation: &OperationId,
        operation_kind: &str,
        scope: &str,
        payload_hash: &str,
        now_ms: u64,
    ) -> StoreResult<(Published, IntentReceipt)> {
        self.ready()?;
        let index = self
            .published
            .state
            .approval_records
            .iter()
            .position(|r| &r.pending.approval_id == approval)
            .ok_or(StoreError::NotFound)?;
        let ledger = &self.published.state.approval_records[index];
        let a = &ledger.pending;
        self.check_approval(a, target, now_ms)?;
        if ledger.invalidated
            || ledger.decision != Some(ApprovalDecision::Allow)
            || &a.operation_id != operation
            || a.operation != operation_kind
            || a.scope != scope
            || a.payload_hash != payload_hash
        {
            return Err(StoreError::RunConflict);
        }
        if ledger.consumed {
            return Ok((self.published.clone(), IntentReceipt::AlreadyRecorded));
        }
        if self
            .published
            .state
            .runs
            .iter()
            .any(|r| r.operations.iter().any(|o| &o.operation_id == operation))
        {
            return Err(StoreError::RunConflict);
        }
        let run_index = self.run_index(target)?;
        let mut next = self.published.clone();
        next.state.approval_records[index].consumed = true;
        let run = &mut next.state.runs[run_index];
        if run.run.state != RunState::Running {
            run.run.state = run
                .run
                .state
                .transition(RunInput::Control(Control::Start))
                .map_err(|_| StoreError::RunConflict)?
                .state();
        }
        run.operations.push(Operation {
            operation_id: operation.clone(),
            result_id: None,
            mcp: None,
            prompt: None,
            recipe: None,
        });
        Ok((next, IntentReceipt::NewlyPublished))
    }

    pub fn invalidate_approval(&mut self, approval: &ApprovalId) -> StoreResult<()> {
        self.ready()?;
        if !self
            .published
            .state
            .approval_records
            .iter()
            .any(|r| &r.pending.approval_id == approval)
        {
            return Err(StoreError::NotFound);
        }
        let mut next = self.published.clone();
        if invalidate_approvals(&mut next.state, |a| &a.approval_id == approval) {
            self.commit(next, false)?;
        }
        Ok(())
    }

    pub fn expire_approvals(&mut self, now_ms: u64) -> StoreResult<Vec<ApprovalId>> {
        self.ready()?;
        let ids = self
            .published
            .state
            .approval_records
            .iter()
            .filter(|r| !r.invalidated && now_ms >= r.pending.expires_at_unix_ms.get())
            .map(|r| r.pending.approval_id.clone())
            .collect();
        let mut next = self.published.clone();
        if invalidate_approvals(&mut next.state, |a| now_ms >= a.expires_at_unix_ms.get()) {
            self.commit(next, false)?;
        }
        Ok(ids)
    }

    /// Register only after the controller has joined workers, completed cleanup,
    /// synced a private recovery parent and validated its pinned directory FD.
    pub fn publish_source_apply(
        &mut self,
        target: &RunTarget,
        candidate: SourceApplyCandidate,
        guard: SourceApplyGuard,
        proof: SourceApplyIdentityProof,
    ) -> StoreResult<()> {
        self.check_source_apply_guard(target, guard)?;
        if candidate.run_id != target.run_id || candidate.attempt_id != target.attempt_id {
            return Err(StoreError::TargetMismatch);
        }
        candidate.payload.check_proof(proof)?;
        if candidate.policy_revision != guard.expected_policy_revision
            || guard.now_ms >= candidate.expires_at_unix_ms.get()
            || candidate.payload.payload_hash()? != candidate.payload_hash
        {
            return Err(StoreError::RunConflict);
        }
        if let Some(old) = self.published.state.source_applies.iter().find(|r| {
            r.candidate.approval_id == candidate.approval_id
                || r.candidate.operation_id == candidate.operation_id
        }) {
            return if old.candidate == candidate {
                Ok(())
            } else {
                Err(StoreError::RunConflict)
            };
        }
        let mut next = self.published.clone();
        next.state.source_applies.push(SavedSourceApply {
            candidate,
            decision: None,
            invalidated: false,
            intent_revision: None,
            result: None,
        });
        self.commit(next, false)
    }

    /// Persist a source decision and its request receipt in one publication.
    /// Replayed receipts are evidence only and never grant dispatch authority.
    pub fn resolve_source_apply_request(
        &mut self,
        request: &Request,
        now_ms: u64,
    ) -> StoreResult<Acceptance> {
        self.ready()?;
        let RequestBody::SourceApplyResolve(session, params) = &request.body else {
            return Err(StoreError::UnsupportedMethod);
        };
        if session != &self.published.marker.session_id {
            return Err(StoreError::TargetMismatch);
        }
        let hash = content_hash(&serde_json::to_vec(request)?);
        if let Some(existing) =
            self.request_status(session, &request.client_id, &request.request_id)?
        {
            if existing.record.request_hash != hash {
                return Err(StoreError::RequestConflict);
            }
            return Ok(existing);
        }
        let target = RunTarget {
            run_id: params.run_id.clone(),
            attempt_id: params.attempt_id.clone(),
        };
        let guard = SourceApplyGuard {
            expected_session_revision: params.expected_session_revision,
            expected_policy_revision: params.expected_policy_revision,
            now_ms,
        };
        self.check_source_apply_guard(&target, guard)?;
        let index = self.source_apply_index(&target, &params.approval_id)?;
        let record = &self.published.state.source_applies[index];
        Self::check_source_apply_permission(record, guard)?;
        if record.candidate.payload_hash != params.payload_hash || record.decision.is_some() {
            return Err(StoreError::RunConflict);
        }
        let mut next = self.published.clone();
        next.state.source_applies[index].decision = Some(params.decision);
        let record = RequestRecord {
            client_id: request.client_id.clone(),
            request_id: request.request_id.clone(),
            request_hash: hash,
            accepted_revision: next.marker.session_revision.checked_add(1)?,
            result: RequestResult::SourceApplyResolved {
                approval_id: params.approval_id.clone(),
            },
        };
        next.state.requests.push(record.clone());
        self.commit(next, false)?;
        Ok(self.acceptance(&record))
    }

    pub fn resolve_source_apply(
        &mut self,
        target: &RunTarget,
        approval: &ApprovalId,
        decision: ApprovalDecision,
        guard: SourceApplyGuard,
    ) -> StoreResult<()> {
        self.check_source_apply_guard(target, guard)?;
        let index = self.source_apply_index(target, approval)?;
        let record = &self.published.state.source_applies[index];
        Self::check_source_apply_permission(record, guard)?;
        if let Some(old) = record.decision {
            return if old == decision {
                Ok(())
            } else {
                Err(StoreError::RunConflict)
            };
        }
        let mut next = self.published.clone();
        next.state.source_applies[index].decision = Some(decision);
        self.commit(next, false)
    }

    /// Durable intent is the permission linearization point. The controller must
    /// serialize this call with cancel/policy changes and validate pinned FDs.
    /// The proof is only compared to stored identities, not OS-verified here.
    /// Only NewlyPublished may be dispatched; errors/recovery/replays never may.
    pub fn consume_source_apply_intent(
        &mut self,
        target: &RunTarget,
        approval: &ApprovalId,
        payload_hash: &str,
        guard: SourceApplyGuard,
        proof: SourceApplyIdentityProof,
    ) -> StoreResult<IntentReceipt> {
        self.check_source_apply_guard(target, guard)?;
        let index = self.source_apply_index(target, approval)?;
        let record = &self.published.state.source_applies[index];
        record.candidate.payload.check_proof(proof)?;
        if record.candidate.payload_hash != payload_hash {
            return Err(StoreError::RunConflict);
        }
        if record.intent_revision.is_some() {
            return Ok(IntentReceipt::AlreadyRecorded);
        }
        Self::check_source_apply_permission(record, guard)?;
        if record.decision != Some(ApprovalDecision::Allow) {
            return Err(StoreError::RunConflict);
        }
        let mut next = self.published.clone();
        next.state.source_applies[index].intent_revision =
            Some(next.marker.session_revision.checked_add(1)?);
        self.commit(next, false)?;
        Ok(IntentReceipt::NewlyPublished)
    }

    /// Expiry or later revocation cannot suppress evidence of an in-flight apply.
    pub fn record_source_apply_result(
        &mut self,
        target: &RunTarget,
        operation: &OperationId,
        expected_session_revision: DecimalU64,
        result: SourceApplyResult,
    ) -> StoreResult<()> {
        self.check_source_apply_target(target, expected_session_revision)?;
        let index = self
            .published
            .state
            .source_applies
            .iter()
            .position(|r| {
                &r.candidate.operation_id == operation
                    && r.candidate.run_id == target.run_id
                    && r.candidate.attempt_id == target.attempt_id
            })
            .ok_or(StoreError::NotFound)?;
        let record = &self.published.state.source_applies[index];
        if record.intent_revision.is_none() {
            return Err(StoreError::RunConflict);
        }
        result.validate()?;
        if let Some(old) = &record.result {
            return if old == &result {
                Ok(())
            } else {
                Err(StoreError::RunConflict)
            };
        }
        let mut next = self.published.clone();
        next.state.source_applies[index].result = Some(result);
        self.commit(next, false)
    }

    /// Invalidates future permission only; never erases or cancels an intent.
    pub fn invalidate_source_apply(
        &mut self,
        target: &RunTarget,
        approval: &ApprovalId,
        expected_session_revision: DecimalU64,
    ) -> StoreResult<()> {
        self.check_source_apply_target(target, expected_session_revision)?;
        let index = self.source_apply_index(target, approval)?;
        let record = &self.published.state.source_applies[index];
        if record.invalidated || record.intent_revision.is_some() {
            return Ok(());
        }
        let mut next = self.published.clone();
        next.state.source_applies[index].invalidated = true;
        self.commit(next, false)
    }

    fn check_source_apply_target(
        &self,
        target: &RunTarget,
        expected: DecimalU64,
    ) -> StoreResult<()> {
        self.ready()?;
        cas(
            expected,
            self.published.marker.session_revision,
            "source apply session",
        )?;
        let run = &self.published.state.runs[self.run_index(target)?];
        if !run.run.state.is_terminal() || run.result_id.is_none() {
            return Err(StoreError::RunConflict);
        }
        Ok(())
    }

    fn check_source_apply_guard(
        &self,
        target: &RunTarget,
        guard: SourceApplyGuard,
    ) -> StoreResult<()> {
        self.check_source_apply_target(target, guard.expected_session_revision)?;
        cas(
            guard.expected_policy_revision,
            self.published.state.policy_revision,
            "source apply policy",
        )
    }

    fn source_apply_index(&self, target: &RunTarget, approval: &ApprovalId) -> StoreResult<usize> {
        self.published
            .state
            .source_applies
            .iter()
            .position(|r| {
                &r.candidate.approval_id == approval
                    && r.candidate.run_id == target.run_id
                    && r.candidate.attempt_id == target.attempt_id
            })
            .ok_or(StoreError::NotFound)
    }

    fn check_source_apply_permission(
        record: &SavedSourceApply,
        guard: SourceApplyGuard,
    ) -> StoreResult<()> {
        if record.invalidated
            || guard.now_ms >= record.candidate.expires_at_unix_ms.get()
            || record.candidate.policy_revision != guard.expected_policy_revision
        {
            return Err(StoreError::RunConflict);
        }
        Ok(())
    }

    pub fn set_policy_revision(&mut self, revision: DecimalU64) -> StoreResult<()> {
        self.ready()?;
        if revision < self.published.state.policy_revision {
            return Err(StoreError::CasConflict("policy"));
        }
        if revision == self.published.state.policy_revision {
            return Ok(());
        }
        let mut next = self.published.clone();
        next.state.policy_revision = revision;
        super::source_apply::invalidate_unconsumed(&mut next.state);
        invalidate_approvals(&mut next.state, |_| true);
        self.commit(next, false)
    }

    fn run_index(&self, target: &RunTarget) -> StoreResult<usize> {
        self.published
            .state
            .runs
            .iter()
            .position(|r| r.run.run_id == target.run_id && r.run.attempt_id == target.attempt_id)
            .ok_or(StoreError::NotFound)
    }

    /// 停止済みsessionのtombstoneを同じ公開点へ保存する。log・台帳は消去しない。
    pub fn tombstone(&mut self) -> StoreResult<()> {
        if !self.recovery_required && self.published.marker.deleted {
            return Ok(());
        }
        self.ready()?;
        if self
            .published
            .state
            .runs
            .iter()
            .any(|r| !r.run.state.is_terminal())
        {
            return Err(StoreError::Busy);
        }
        let mut next = self.published.clone();
        if next
            .state
            .source_applies
            .iter()
            .any(|r| r.intent_revision.is_some() && r.result.is_none())
        {
            return Err(StoreError::Busy);
        }
        super::source_apply::invalidate_unconsumed(&mut next.state);
        next.marker.deleted = true;
        next.state.unresolved_approvals.clear();
        self.commit(next, false)
    }
}

fn cas(expected: DecimalU64, actual: DecimalU64, name: &'static str) -> StoreResult<()> {
    if expected == actual {
        Ok(())
    } else {
        Err(StoreError::CasConflict(name))
    }
}

fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn same_message(left: &Message, right: &Message) -> StoreResult<bool> {
    Ok(serde_json::to_vec(left)? == serde_json::to_vec(right)?)
}

fn same_messages(left: &[Message], right: &[Message]) -> StoreResult<bool> {
    if left.len() != right.len() {
        return Ok(false);
    }
    left.iter()
        .zip(right)
        .try_fold(true, |same, (left, right)| {
            Ok(same && same_message(left, right)?)
        })
}

fn saved_turn_suffix(p: &Published, index: usize) -> StoreResult<Vec<Message>> {
    let run_index = index
        .checked_sub(
            usize::try_from(p.state.inherited_turn_count.get())
                .map_err(|_| StoreError::Overflow)?,
        )
        .ok_or(StoreError::RunConflict)?;
    let supplemental = prompt::attachment(p, run_index)?.map(|event| event.sequence);
    let input = p
        .raw
        .iter()
        .filter(|event| event.starts_turn)
        .nth(index)
        .ok_or(StoreError::RunConflict)?;
    Ok(p.raw
        .iter()
        .filter(|event| {
            event.turn_id == input.turn_id
                && !event.starts_turn
                && Some(event.sequence) != supplemental
        })
        .map(|event| event.message.clone())
        .collect())
}

fn turn_suffix_tail(p: &Published, index: usize, full: &[Message]) -> StoreResult<Vec<Message>> {
    let saved = saved_turn_suffix(p, index)?;
    if saved.len() > full.len()
        || !saved
            .iter()
            .zip(full)
            .try_fold(true, |same, (saved, full)| {
                Ok::<bool, StoreError>(same && same_message(saved, full)?)
            })?
    {
        return Err(StoreError::RunConflict);
    }
    Ok(full[saved.len()..].to_vec())
}

fn append_turn_suffix(
    raw: &mut Vec<RawEventV2>,
    index: usize,
    tail: Vec<Message>,
) -> StoreResult<()> {
    if tail.is_empty() {
        return Ok(());
    }
    // Only the current accepted turn may acquire a new suffix.
    if raw.iter().filter(|event| event.starts_turn).count() != index + 1 {
        return Err(StoreError::RunConflict);
    }
    let input = raw
        .iter()
        .filter(|event| event.starts_turn)
        .nth(index)
        .ok_or(StoreError::RunConflict)?;
    let turn_id = input.turn_id;
    let mut sequence = raw.last().ok_or(StoreError::RunConflict)?.sequence;
    let epoch = raw.last().ok_or(StoreError::RunConflict)?.epoch;
    for message in tail {
        sequence = sequence.checked_add(1).ok_or(StoreError::Overflow)?;
        raw.push(RawEventV2 {
            schema_version: 2,
            sequence,
            epoch,
            turn_id,
            starts_turn: false,
            message,
        });
    }
    Ok(())
}

fn same_or_staled_gates(
    previous: &crate::workflow::WorkflowGatesV1,
    candidate: &crate::workflow::WorkflowGatesV1,
) -> bool {
    if previous.schema_version != candidate.schema_version
        || previous.approved_spec_hash != candidate.approved_spec_hash
        || previous.external_grants != candidate.external_grants
    {
        return false;
    }
    match (&previous.verification, &candidate.verification) {
        (None, None) => true,
        (Some(previous), Some(candidate)) => {
            previous.artifact_hash == candidate.artifact_hash
                && previous.passed == candidate.passed
                && (!previous.stale || candidate.stale)
        }
        _ => false,
    }
}

fn checkpoint_base_matches(saved: &super::SavedWorkflow, previous: &super::SavedWorkflow) -> bool {
    saved.config == previous.config
        && saved.state.phase == previous.state.phase
        && saved.state.revision == previous.state.revision
        && same_or_staled_gates(&saved.gates, &previous.gates)
        && (saved.state == previous.state
            // `begin_turn` fixes the first request's manifest before provider
            // admission. A legacy durable snapshot has no manifest yet.
            || (saved.state.skill_manifest_hash.is_empty()
                && saved.state.skills.is_empty()
                && !previous.state.skill_manifest_hash.is_empty()))
}

pub(crate) fn validate_turn_suffix(messages: &[Message], outcome: Observation) -> StoreResult<()> {
    use polaris_provider::Role;
    use std::collections::BTreeSet;
    // Count serialized bytes without making a second unbounded copy.
    struct Budget(usize);
    impl Write for Budget {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.checked_sub(bytes.len()).ok_or_else(|| {
                std::io::Error::other("desktop turn exceeds storage transaction limit")
            })?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    if messages.len() > 256 {
        return Err(StoreError::Overflow);
    }
    serde_json::to_writer(Budget(4 * 1024 * 1024), messages).map_err(|_| StoreError::Overflow)?;
    let mut seen = BTreeSet::new();
    let mut pending = BTreeSet::new();
    for message in messages {
        // history.page returns up to four complete text values in a 1 MiB frame.
        // Bound escaped JSON, not UTF-8 alone; never silently truncate raw text.
        serde_json::to_writer(Budget(128 * 1024), &message.content)
            .map_err(|_| StoreError::Overflow)?;
        match message.role {
            Role::User => return Err(StoreError::RunConflict),
            Role::Assistant => {
                if message.tool_call_id.is_some() || !pending.is_empty() {
                    return Err(StoreError::RunConflict);
                }
                for call in &message.tool_calls {
                    if call.id.is_empty() || !seen.insert(call.id.as_str()) {
                        return Err(StoreError::RunConflict);
                    }
                    pending.insert(call.id.as_str());
                }
            }
            Role::Tool => {
                if !message.tool_calls.is_empty()
                    || !message.reasoning.is_empty()
                    || !message.hosted_web_search.is_empty()
                    || !message.url_citations.is_empty()
                    || !message
                        .tool_call_id
                        .as_deref()
                        .is_some_and(|id| pending.remove(id))
                {
                    return Err(StoreError::RunConflict);
                }
            }
        }
    }
    if outcome == Observation::Succeeded
        && (!pending.is_empty() || !messages.last().is_some_and(|m| m.role == Role::Assistant))
    {
        return Err(StoreError::RunConflict);
    }
    Ok(())
}

fn invalidate_approvals(state: &mut Sidecar, predicate: impl Fn(&PendingApproval) -> bool) -> bool {
    let before = state.unresolved_approvals.len();
    state.unresolved_approvals.retain(|a| !predicate(a));
    let mut changed = before != state.unresolved_approvals.len();
    for record in &mut state.approval_records {
        if !record.invalidated && predicate(&record.pending) {
            record.invalidated = true;
            changed = true;
        }
    }
    changed
}
