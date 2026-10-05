//! Linux terminal adapter for the existing source-bound native owner.
//! The service owns completion receipts, copies and dispatch authority.
use super::{RunArgs, approver::KeyReader, render};
use polaris_core::{
    apply_owner::ApplyOwnerV1,
    audit::AuditLog,
    desktop_store::{BootstrapTier, Published, SourceApplyIdentity},
    isolated_run::PreparedWorkspace,
    isolated_workspace::Limits,
    project_mutation::open_directory_nofollow,
};
use polaris_desktop_protocol::{
    codec,
    event::Event,
    ids::*,
    request::*,
    response::{Response, SuccessResult},
    snapshot::{ApprovalDecision, Configuration, Snapshot},
    source_apply::SourceApplySummary,
};
use polaris_desktop_service::{
    ConfirmedSourceGrant, CurrentSourcePolicy, DesktopService, ProductionSourceFactory,
    ServiceConfig, ServiceError, SourceApplyClock, SourceApplyRuntime, TrustedRunFactory,
    TrustedRunInputs,
};
use std::{
    cell::RefCell,
    fs,
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

const SESSION: &str = "tui-source-apply-v1";
fn limits() -> Limits {
    Limits {
        max_entries: 40_000,
        max_files: 20_000,
        max_file_bytes: 16 * 1024 * 1024,
        max_total_bytes: 256 * 1024 * 1024,
        max_depth: 32,
    }
}
struct Clock;
impl SourceApplyClock for Clock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u64::MAX as u128) as u64
    }
}

/// Tighten only the pinned, user-owned project state directory. Do not follow
/// aliases, change an ancestor, or relax the intent owner's private-mode check.
pub(super) fn prepare_state(project: &Path, state: &Path) -> Result<(), String> {
    let expected = polaris_core::project::state_dir(project).map_err(|e| e.to_string())?;
    if fs::canonicalize(&expected).map_err(|e| e.to_string())? != state {
        return Err("projectと保存場所が一致しません".into());
    }
    let fd = open_directory_nofollow(state).map_err(|e| e.to_string())?;
    let m = fd.metadata().map_err(|e| e.to_string())?;
    if m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o7000 != 0 {
        return Err("保存場所の所有者またはmodeが不正です".into());
    }
    fd.set_permissions(fs::Permissions::from_mode(0o700))
        .map_err(|e| e.to_string())?;
    fd.sync_all().map_err(|e| e.to_string())?;
    let fresh = open_directory_nofollow(state)
        .and_then(|f| f.metadata())
        .map_err(|e| e.to_string())?;
    if (fresh.dev(), fresh.ino()) != (m.dev(), m.ino()) || fresh.mode() & 0o7777 != 0o700 {
        return Err("保存場所が変更されました".into());
    }
    Ok(())
}

struct Runs {
    source: PathBuf,
    helper: PathBuf,
    provider: Arc<dyn polaris_provider::Provider>,
    configuration: Configuration,
    always_on: polaris_core::prompt::AlwaysOn,
    audit: Arc<tokio::sync::Mutex<AuditLog>>,
    max_turns: u32,
    skills: Vec<polaris_skills::Skill>,
    agents: Vec<polaris_skills::AgentType>,
    concurrency: usize,
    write_concurrency: usize,
    workflow: polaris_core::workflow::WorkflowConfig,
    preparation: polaris_core::input_preparation::InputPreparationMode,
    decider: Option<Arc<dyn polaris_provider::decision::Decider>>,
}
impl TrustedRunFactory for Runs {
    fn workflow_config(&self) -> Option<&polaris_core::workflow::WorkflowConfig> {
        Some(&self.workflow)
    }
    fn workflow_skills(&self) -> &[polaris_skills::Skill] {
        &self.skills
    }
    fn role_catalog(&self) -> &[polaris_skills::AgentType] {
        &self.agents
    }
    fn input_preparation(&self) -> polaris_core::input_preparation::InputPreparationMode {
        self.preparation
    }
    fn decider(&self) -> Option<Arc<dyn polaris_provider::decision::Decider>> {
        self.decider.clone()
    }
    fn prepare(
        &self,
        _target: &RunTarget,
        published: &Published,
    ) -> Result<TrustedRunInputs, ServiceError> {
        if published.state.configuration != self.configuration
            || self
                .provider
                .model()
                .is_some_and(|model| model != self.configuration.model)
        {
            return Err(ServiceError::Options);
        }
        let prepared = Arc::new(
            PreparedWorkspace::prepare(
                &self.source,
                &self.helper,
                polaris_sandbox::SandboxMode::WorkspaceWrite,
                &[],
                limits(),
            )
            .map_err(|_| ServiceError::Options)?,
        );
        let mutation_policy = Some(prepared.policy().clone());
        Ok(TrustedRunInputs {
            prepared,
            mutation_policy,
            execution_capability: Some(
                polaris_core::desktop_execution::ConfirmedExecutionCapability::FixedHelpersOnly,
            ),
            provider: self.provider.clone(),
            provider_pool: self.provider.clone(),
            always_on: self.always_on.clone(),
            audit: self.audit.clone(),
            max_turns: self.max_turns,
            skills: self.skills.clone(),
            agent_types: self.agents.clone(),
            spawn_concurrency: self.concurrency,
            spawn_write_concurrency: self.write_concurrency,
            tool_memory: None,
            history: None,
            mcp: None,
            external_ceiling: polaris_core::mcp::operation::ExternalCeiling::Denied,
        })
    }
}

pub(super) struct Frontend {
    io: DuplexStream,
    service: tokio::task::JoinHandle<Result<polaris_desktop_service::Exit, ServiceError>>,
    session: SessionId,
    client: ClientId,
    hello: bool,
    usable: bool,
    sequence: u64,
    writable: bool,
}
impl Frontend {
    pub(super) fn open(
        args: &RunArgs<'_>,
        audit: Arc<tokio::sync::Mutex<AuditLog>>,
    ) -> Result<Self, String> {
        let source = fs::canonicalize(polaris_core::project::resolve_root(&args.cwd))
            .map_err(|e| e.to_string())?;
        let state = fs::canonicalize(&args.state_dir).map_err(|e| e.to_string())?;
        prepare_state(&source, &state)?;
        match fs::DirBuilder::new().mode(0o700).create(state.join("sync")) {
            Ok(()) => open_directory_nofollow(&state)
                .and_then(|f| f.sync_all())
                .map_err(|e| e.to_string())?,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.to_string()),
        }
        let owner = ApplyOwnerV1::acquire(&source).map_err(|e| e.to_string())?;
        let parent = owner
            .c12_recovery_parent(owner.active().map_err(|e| e.to_string())?.is_none())
            .map_err(|e| e.to_string())?;
        let base = open_directory_nofollow(parent.provenance()).map_err(|e| e.to_string())?;
        parent.validate_identity().map_err(|e| format!("{e:?}"))?;
        let meta = fs::metadata(&source).map_err(|e| e.to_string())?;
        let configuration = Configuration {
            history_mode: Default::default(),
            configuration_revision: DecimalU64::new(0),
            provider: args.provider_name.clone(),
            model: args.model_name.clone(),
            effort: args
                .initial_effort_name
                .clone()
                .unwrap_or_else(|| "none".into()),
        };
        let writable = args.sandbox.mode() != polaris_sandbox::SandboxMode::ReadOnly;
        let grant = Arc::new(ConfirmedSourceGrant {
            policy: CurrentSourcePolicy {
                source_path: source.clone(),
                source_identity: SourceApplyIdentity {
                    device: DecimalU64::new(meta.dev()),
                    inode: DecimalU64::new(meta.ino()),
                },
                policy_revision: DecimalU64::new(0),
                read_allowed: true,
                write_allowed: writable,
            },
            tier: if writable {
                BootstrapTier::ReadCreate
            } else {
                BootstrapTier::ReadOnly
            },
        });
        let factory = ProductionSourceFactory::new(
            grant,
            base,
            parent.identity(),
            parent.provenance().to_owned(),
        )
        .map_err(|e| e.to_string())?;
        drop(owner);
        let store = state.join("tui-source-v3");
        match fs::DirBuilder::new().mode(0o700).create(&store) {
            Ok(()) => open_directory_nofollow(&state)
                .and_then(|f| f.sync_all())
                .map_err(|e| e.to_string())?,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.to_string()),
        }
        let session = SessionId::new(SESSION).map_err(|e| e.to_string())?;
        let runs = Arc::new(Runs {
            source: source.clone(),
            helper: args.helper.clone(),
            provider: args.provider.clone(),
            configuration: configuration.clone(),
            always_on: args.always_on.clone(),
            audit,
            max_turns: args.max_turns,
            skills: args.skills.to_vec(),
            agents: args.agent_types.to_vec(),
            concurrency: args.spawn_concurrency,
            write_concurrency: args.spawn_write_concurrency,
            workflow: args.workflow_config.clone(),
            preparation: args.input_preparation,
            decider: args.decider.clone(),
        });
        let mut owner = DesktopService::open_source_bound_with_trusted_runs(
            ServiceConfig {
                store_root: store,
                project_id: ProjectId::new(polaris_core::project::project_id(&source))
                    .map_err(|e| e.to_string())?,
                session_id: session.clone(),
            },
            source,
            (meta.dev(), meta.ino()),
            configuration,
            runs,
            SourceApplyRuntime {
                factory: Arc::new(factory),
                clock: Arc::new(Clock),
                limits: limits(),
                approval_ttl_ms: 15 * 60 * 1000,
            },
        )
        .map_err(|e| e.to_string())?;
        let (io, peer) = tokio::io::duplex(2 * codec::MAX_FRAME_BYTES);
        let (read, write) = tokio::io::split(peer);
        let service = tokio::spawn(async move { owner.serve(read, write).await });
        Ok(Self {
            io,
            service,
            session,
            client: ClientId::new(format!(
                "tui-{}",
                polaris_core::session_store::new_session_id().map_err(|e| e.to_string())?
            ))
            .map_err(|e| e.to_string())?,
            hello: false,
            usable: true,
            sequence: 0,
            writable,
        })
    }
    async fn request(&mut self, body: RequestBody) -> Result<SuccessResult, String> {
        if !self.usable {
            return Err("IPC応答は結果不明です。この接続では再操作せず、再起動して保存済み状態を確認してください".into());
        }
        if self.service.is_finished() {
            return Err("D27 ownerは停止しています。再起動してください".into());
        }
        self.sequence = self.sequence.checked_add(1).ok_or("要求IDの上限です")?;
        let request = Request {
            protocol_version: Default::default(),
            client_id: self.client.clone(),
            request_id: RequestId::new(format!("tui-{}", self.sequence))
                .map_err(|e| e.to_string())?,
            body,
        };
        let frame = codec::encode(&request).map_err(|e| e.to_string())?;
        let transport = tokio::time::timeout(Duration::from_secs(10), async {
            self.io.write_all(&frame).await.map_err(|e| e.to_string())?;
            loop {
                let mut header = [0; 4];
                self.io
                    .read_exact(&mut header)
                    .await
                    .map_err(|e| e.to_string())?;
                let size = u32::from_be_bytes(header) as usize;
                if size > codec::MAX_FRAME_BYTES {
                    return Err("IPC frameが上限を超えました".into());
                }
                let mut bytes = vec![0; size];
                self.io
                    .read_exact(&mut bytes)
                    .await
                    .map_err(|e| e.to_string())?;
                if let Ok(response) = codec::from_json::<Response>(&bytes) {
                    response
                        .validate_for(&request)
                        .map_err(|_| "IPC応答の相関が一致しません")?;
                    return Ok(response.outcome);
                }
                let event: Event = codec::from_json(&bytes).map_err(|e| e.to_string())?;
                if event.session_id != self.session {
                    return Err("IPC eventのsessionが一致しません".into());
                }
            }
        })
        .await;
        match transport {
            Ok(Ok(outcome)) => outcome.map_err(|e| format!("{:?}: {}", e.code, e.message)),
            failed => {
                self.usable = false;
                let reason: String = match failed {
                    Ok(Err(reason)) => reason,
                    _ => "応答期限を超えました".into(),
                };
                Err(format!(
                    "IPCは結果不明です（{reason}）。この接続を失効しました。再起動して確認してください"
                ))
            }
        }
    }
    async fn snapshot(&mut self) -> Result<Box<Snapshot>, String> {
        if !self.hello {
            let SuccessResult::Hello(_) = self.request(RequestBody::Hello).await? else {
                return Err("Hello応答が不正です".into());
            };
            self.hello = true;
        }
        match self
            .request(RequestBody::SessionSnapshot(self.session.clone()))
            .await?
        {
            SuccessResult::SessionSnapshot(s) => Ok(s),
            _ => Err("snapshot応答が不正です".into()),
        }
    }
    pub(super) async fn shutdown(self) -> Result<(), String> {
        let Self { io, service, .. } = self;
        drop(io);
        tokio::time::timeout(Duration::from_secs(120), service)
            .await
            .map_err(|_| "D27 ownerの停止を確認できません。保存済み証拠を再起動で確認してください")?
            .map_err(|_| "D27 ownerのjoinに失敗しました")?
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    async fn list(&mut self) -> Result<(DecimalU64, Vec<SourceApplySummary>), String> {
        let revision = self.snapshot().await?.session_revision;
        let mut offset = DecimalU64::new(0);
        let mut items = Vec::new();
        loop {
            match self
                .request(RequestBody::SourceApplyList(
                    self.session.clone(),
                    SourceApplyList {
                        expected_session_revision: revision,
                        offset,
                        limit: PageLimit::new(32).unwrap(),
                    },
                ))
                .await?
            {
                SuccessResult::SourceApplyList(page) => {
                    items.extend(page.items);
                    match page.next_offset {
                        Some(next) if next.get() > offset.get() => offset = next,
                        None => return Ok((revision, items)),
                        _ => return Err("候補pageが進みません".into()),
                    }
                }
                _ => return Err("候補一覧の応答が不正です".into()),
            }
        }
    }
    async fn preview(
        &mut self,
        id: &str,
    ) -> Result<(DecimalU64, SourceApplySummary, Vec<String>), String> {
        let (revision, items) = self.list().await?;
        let item = items
            .into_iter()
            .find(|i| i.approval_id.as_str() == id)
            .ok_or("表示された正確な承認IDを指定してください")?;
        let mut offset = DecimalU64::new(0);
        let mut lines = vec![
            format!("D27 source: {}", item.source_path),
            format!("recovery: {}", item.recovery_parent_path),
            format!("approval: {}", item.approval_id.as_str()),
            format!(
                "revision: {} / policy: {}",
                revision.get(),
                item.policy_revision.get()
            ),
            format!("hash: {}", item.payload_hash),
        ];
        loop {
            match self
                .request(RequestBody::SourceApplyPage(
                    self.session.clone(),
                    SourceApplyPage {
                        approval_id: item.approval_id.clone(),
                        payload_hash: item.payload_hash.clone(),
                        expected_session_revision: revision,
                        offset,
                        limit: PageLimit::new(32).unwrap(),
                    },
                ))
                .await?
            {
                SuccessResult::SourceApplyPage(page) => {
                    lines.extend(
                        page.entries.iter().map(|e| {
                            format!("{}: {:?} → {:?}", e.relative_path, e.before, e.after)
                        }),
                    );
                    match page.next_offset {
                        Some(next) if next.get() > offset.get() => offset = next,
                        None => return Ok((revision, item, lines)),
                        _ => return Err("変更pageが進みません".into()),
                    }
                }
                _ => return Err("変更一覧の応答が不正です".into()),
            }
        }
    }
    async fn run<B: ratatui::backend::Backend, R: KeyReader>(
        &mut self,
        task: &str,
        terminal: &RefCell<ratatui::Terminal<B>>,
        reader: &mut R,
    ) -> Result<String, String> {
        if !self.writable {
            return Err("read-onlyでは隔離コピーの変更を開始できません".into());
        }
        if task.trim().is_empty() {
            return Err("使い方: /apply run <隔離コピーで実行する指示>".into());
        }
        if !confirm(
            terminal,
            reader,
            &[
                "隔離コピーで実行します。元projectへはまだ適用しません。".into(),
                "コピー内のread/writeのみ。build・外部MCPは許可しません。".into(),
                task.into(),
            ],
        ) {
            return Ok("隔離runを取り消しました".into());
        }
        let snapshot = self.snapshot().await?;
        if snapshot.runs.iter().any(|r| !r.state.is_terminal()) {
            return Err("既存の隔離runが実行中です".into());
        }
        let updated = self
            .request(RequestBody::DraftUpdate(
                self.session.clone(),
                DraftUpdate {
                    expected_draft_revision: snapshot.draft.draft_revision,
                    text: task.into(),
                    attachment_ids: vec![],
                },
            ))
            .await?;
        let SuccessResult::DraftUpdate(updated) = updated else {
            return Err("下書き応答が不正です".into());
        };
        let started = self
            .request(RequestBody::RunStart(
                self.session.clone(),
                RunStart {
                    expected_draft_revision: updated.draft_revision,
                    expected_configuration_revision: snapshot.configuration.configuration_revision,
                    expected_policy_revision: snapshot.policy_revision,
                    mcp_prompt: None,
                },
            ))
            .await?;
        let SuccessResult::RunStart(started) = started else {
            return Err("run開始応答が不正です".into());
        };
        let target = RunTarget {
            run_id: started.run_id,
            attempt_id: started.attempt_id,
        };
        let mut cancelled = false;
        loop {
            let snapshot = self.snapshot().await?;
            let run = snapshot
                .runs
                .iter()
                .find(|r| r.run_id == target.run_id && r.attempt_id == target.attempt_id)
                .ok_or("runの座標が見つかりません")?;
            if run.state.is_terminal() {
                return Ok(format!(
                    "隔離run: {:?}。変更候補は /apply list で確認できます",
                    run.state
                ));
            }
            draw(
                terminal,
                &[
                    format!("隔離run {}: {:?}", target.run_id.as_str(), run.state),
                    "Escで取消。元projectは未適用です".into(),
                ],
            )?;
            for approval in snapshot.unresolved_approvals {
                let allow = confirm(
                    terminal,
                    reader,
                    &[
                        approval.display.title,
                        approval.display.description,
                        format!("scope: {}", approval.scope),
                        format!("hash: {}", approval.payload_hash),
                    ],
                );
                self.request(RequestBody::ApprovalResolve(
                    self.session.clone(),
                    ApprovalResolve {
                        approval_id: approval.approval_id,
                        run_id: approval.run_id,
                        attempt_id: approval.attempt_id,
                        policy_revision: approval.policy_revision,
                        decision: if allow {
                            ApprovalDecision::Allow
                        } else {
                            ApprovalDecision::Deny
                        },
                    },
                ))
                .await?;
            }
            if !cancelled
                && (super::linux_pending_exit_code().is_some()
                    || ratatui::crossterm::event::poll(Duration::ZERO)
                        .map_err(|e| e.to_string())?
                        && matches!(
                            reader.read_key(),
                            Ok(ratatui::crossterm::event::KeyCode::Esc)
                        ))
            {
                self.request(RequestBody::RunCancel(self.session.clone(), target.clone()))
                    .await?;
                cancelled = true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    pub(super) async fn action<B: ratatui::backend::Backend, R: KeyReader>(
        &mut self,
        requested: &str,
        terminal: &RefCell<ratatui::Terminal<B>>,
        reader: &mut R,
    ) -> Result<String, String> {
        let words: Vec<_> = requested.split_whitespace().collect();
        match words.as_slice() {
            [] | ["list"] | ["status"] => {
                let (_, items) = self.list().await?;
                if items.is_empty() {
                    return Ok("D27候補なし。/apply run <指示> で隔離実行します".into());
                }
                Ok(items
                    .iter()
                    .map(|i| {
                        format!(
                            "{}: {} entries / decision {:?} / result {:?}",
                            i.approval_id.as_str(),
                            i.entry_count.get(),
                            i.decision,
                            i.result
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
            ["run", ..] => {
                self.run(
                    requested.strip_prefix("run").unwrap().trim_start(),
                    terminal,
                    reader,
                )
                .await
            }
            ["view", id] => Ok(self.preview(id).await?.2.join("\n")),
            ["resolve", id, decision] if *decision == "allow" || *decision == "deny" => {
                if *decision == "allow" && !self.writable {
                    return Err("sourceへの書込み許可がありません".into());
                }
                let (revision, item, mut lines) = self.preview(id).await?;
                lines.push("IrreversibleLocal。yで表示した要求だけを確定、Escで取消".into());
                if !confirm(terminal, reader, &lines) {
                    return Ok("D27承認を取り消しました".into());
                }
                self.request(RequestBody::SourceApplyResolve(
                    self.session.clone(),
                    SourceApplyResolve {
                        run_id: item.run_id,
                        attempt_id: item.attempt_id,
                        approval_id: item.approval_id,
                        payload_hash: item.payload_hash,
                        expected_session_revision: revision,
                        expected_policy_revision: item.policy_revision,
                        decision: if *decision == "allow" {
                            ApprovalDecision::Allow
                        } else {
                            ApprovalDecision::Deny
                        },
                    },
                ))
                .await?;
                Ok("承認応答を受理しました。適用成功は /apply list の保存済み結果で確認してください".into())
            }
            _ => {
                Err("使い方: /apply run <指示> | list | view <ID> | resolve <ID> allow|deny".into())
            }
        }
    }
}

fn draw<B: ratatui::backend::Backend>(
    terminal: &RefCell<ratatui::Terminal<B>>,
    lines: &[String],
) -> Result<(), String> {
    use ratatui::{
        text::Line,
        widgets::{Block, Paragraph, Wrap},
    };
    terminal
        .borrow_mut()
        .draw(|f| {
            let text = lines
                .iter()
                .map(|s| Line::from(render::sanitize(s)))
                .collect::<Vec<_>>();
            f.render_widget(
                Paragraph::new(text)
                    .block(Block::bordered().title("D27 — 隔離実行と元projectへの適用"))
                    .wrap(Wrap { trim: false }),
                f.area(),
            );
        })
        .map(|_| ())
        .map_err(|e| e.to_string())
}
fn confirm<B: ratatui::backend::Backend, R: KeyReader>(
    terminal: &RefCell<ratatui::Terminal<B>>,
    reader: &mut R,
    lines: &[String],
) -> bool {
    let mut lines = lines.to_vec();
    lines.push("y: 確定 / Esc: 取消 / ↑↓: 全項目を確認".into());
    let mut offset = 0;
    loop {
        if draw(terminal, &lines[offset..]).is_err() {
            return false;
        }
        match reader.read_key() {
            Ok(ratatui::crossterm::event::KeyCode::Char('y')) => return true,
            Ok(ratatui::crossterm::event::KeyCode::Esc) | Err(_) => return false,
            Ok(ratatui::crossterm::event::KeyCode::Down) => {
                offset = (offset + 1).min(lines.len().saturating_sub(1))
            }
            Ok(ratatui::crossterm::event::KeyCode::Up) => offset = offset.saturating_sub(1),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyCode;
    use std::{collections::VecDeque, sync::Mutex};

    struct Keys(VecDeque<KeyCode>);
    impl KeyReader for Keys {
        fn read_key(&mut self) -> std::io::Result<KeyCode> {
            self.0
                .pop_front()
                .ok_or_else(|| std::io::Error::other("fixture has no more keys"))
        }
    }
    /// A synthetic provider mutates only its prepared copy during the live
    /// worker. It never edits after completion or manufactures a receipt.
    struct Provider {
        copy: Mutex<Option<PathBuf>>,
    }
    #[async_trait::async_trait]
    impl polaris_provider::Provider for Provider {
        async fn complete(
            &self,
            _: polaris_provider::CompletionRequest,
        ) -> Result<polaris_provider::CompletionResponse, polaris_provider::ProviderError> {
            let copy = self.copy.lock().unwrap().clone().unwrap();
            fs::write(copy.join("ordinary"), "after").unwrap();
            Ok(polaris_provider::CompletionResponse {
                text: "isolated fixture finished".into(),
                ..Default::default()
            })
        }
    }
    struct Factory {
        runs: Runs,
        provider: Arc<Provider>,
    }

    #[tokio::test]
    async fn m2d_d27_lost_response_closes_further_dispatch() {
        for failure in ["wrong-id", "eof", "timeout"] {
            let (io, mut peer) = tokio::io::duplex(2 * codec::MAX_FRAME_BYTES);
            let service = tokio::spawn(async move {
                let size = peer.read_u32().await.unwrap() as usize;
                let mut bytes = vec![0; size];
                peer.read_exact(&mut bytes).await.unwrap();
                let request: Request = codec::from_json(&bytes).unwrap();
                assert!(matches!(request.body, RequestBody::SourceApplyResolve(..)));
                match failure {
                    "wrong-id" => {
                        let response=Response {protocol_version:request.protocol_version,client_id:request.client_id,
                            request_id:RequestId::new("unrelated-old-response").unwrap(),
                            outcome:Err(polaris_desktop_protocol::response::ProtocolError {code:polaris_desktop_protocol::response::ErrorCode::CapabilityUnavailable,message:"fixture".into()})};
                        peer.write_all(&codec::encode(&response).unwrap())
                            .await
                            .unwrap();
                    }
                    "eof" => peer.shutdown().await.unwrap(),
                    "timeout" => {}
                    _ => unreachable!(),
                }
                // Any second request would arrive before checking its response.
                // The frontend must refuse it locally, then close on shutdown.
                let mut next = [0; 1];
                assert_eq!(
                    peer.read(&mut next).await.unwrap(),
                    0,
                    "a second operation was sent after unknown transport outcome"
                );
                Ok(polaris_desktop_service::Exit::Eof)
            });
            let mut owner = Frontend {
                io,
                service,
                session: SessionId::new(SESSION).unwrap(),
                client: ClientId::new("lost-response-fixture").unwrap(),
                hello: true,
                usable: true,
                sequence: 0,
                writable: true,
            };
            let answer = SourceApplyResolve {
                run_id: RunId::new("run").unwrap(),
                attempt_id: AttemptId::new("attempt").unwrap(),
                approval_id: ApprovalId::new("approval").unwrap(),
                payload_hash: "a".repeat(64),
                expected_session_revision: DecimalU64::new(1),
                expected_policy_revision: DecimalU64::new(0),
                decision: ApprovalDecision::Allow,
            };
            assert!(
                owner
                    .request(RequestBody::SourceApplyResolve(
                        owner.session.clone(),
                        answer.clone()
                    ))
                    .await
                    .is_err()
            );
            assert!(!owner.usable);
            assert!(
                owner
                    .request(RequestBody::SourceApplyResolve(
                        owner.session.clone(),
                        answer
                    ))
                    .await
                    .is_err()
            );
            assert!(
                owner
                    .request(RequestBody::RunStart(
                        owner.session.clone(),
                        RunStart {
                            expected_draft_revision: DecimalU64::new(0),
                            expected_configuration_revision: DecimalU64::new(0),
                            expected_policy_revision: DecimalU64::new(0),
                            mcp_prompt: None
                        }
                    ))
                    .await
                    .is_err()
            );
            owner.shutdown().await.unwrap();
        }
    }
    impl TrustedRunFactory for Factory {
        fn workflow_config(&self) -> Option<&polaris_core::workflow::WorkflowConfig> {
            self.runs.workflow_config()
        }
        fn prepare(
            &self,
            target: &RunTarget,
            published: &Published,
        ) -> Result<TrustedRunInputs, ServiceError> {
            let inputs = self.runs.prepare(target, published)?;
            *self.provider.copy.lock().unwrap() =
                Some(inputs.prepared.snapshot().path().to_owned());
            Ok(inputs)
        }
    }

    #[test]
    fn m2d_d27_live_copy_confirmation_and_source_bound_restart() {
        const CHILD: &str = "POLARIS_TUI_M2D_D27_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let fixture = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "source_apply::tests::m2d_d27_live_copy_confirmation_and_source_bound_restart",
                    "--nocapture",
                ])
                .env(CHILD, fixture.path())
                .env("POLARIS_DATA_DIR", fixture.path().join("data"))
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("m2d-d27-receipt: verified"));
            return;
        }
        let root = PathBuf::from(std::env::var_os(CHILD).unwrap());
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let source = root.join("project");
                fs::create_dir(&source).unwrap();
                fs::create_dir(source.join(".polaris")).unwrap();
                fs::write(source.join("ordinary"), "before").unwrap();
                let state = polaris_core::project::state_dir(&source).unwrap();
                fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
                prepare_state(&source, &state).unwrap();
                assert_eq!(fs::metadata(&state).unwrap().mode() & 0o777, 0o700);
                fs::DirBuilder::new()
                    .mode(0o700)
                    .create(state.join("sync"))
                    .unwrap();
                let lease = ApplyOwnerV1::acquire(&source).unwrap();
                let parent = lease.c12_recovery_parent(true).unwrap();
                let meta = fs::metadata(&source).unwrap();
                let grant = Arc::new(ConfirmedSourceGrant {
                    policy: CurrentSourcePolicy {
                        source_path: source.clone(),
                        source_identity: SourceApplyIdentity {
                            device: DecimalU64::new(meta.dev()),
                            inode: DecimalU64::new(meta.ino()),
                        },
                        policy_revision: DecimalU64::new(0),
                        read_allowed: true,
                        write_allowed: true,
                    },
                    tier: BootstrapTier::ReadCreate,
                });
                let source_factory = ProductionSourceFactory::new(
                    grant,
                    open_directory_nofollow(parent.provenance()).unwrap(),
                    parent.identity(),
                    parent.provenance().to_owned(),
                )
                .unwrap();
                drop(lease);
                let configuration = Configuration {
                    history_mode: Default::default(),
                    configuration_revision: DecimalU64::new(0),
                    provider: "fixture".into(),
                    model: "fixture".into(),
                    effort: "none".into(),
                };
                let helper = root.join("unused-helper");
                fs::write(&helper, "never executed by this synthetic provider").unwrap();
                fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
                let provider = Arc::new(Provider {
                    copy: Mutex::new(None),
                });
                let factory = Arc::new(Factory {
                    provider: provider.clone(),
                    runs: Runs {
                        source: source.clone(),
                        helper,
                        provider,
                        configuration: configuration.clone(),
                        always_on: polaris_core::prompt::assemble_always_on("", "", &[]),
                        audit: Arc::new(tokio::sync::Mutex::new(
                            AuditLog::open(&root.join("audit")).unwrap(),
                        )),
                        max_turns: 2,
                        skills: vec![],
                        agents: vec![],
                        concurrency: 1,
                        write_concurrency: 1,
                        workflow: Default::default(),
                        preparation: polaris_core::input_preparation::InputPreparationMode::Off,
                        decider: None,
                    },
                });
                let store = state.join("tui-source-v3");
                fs::DirBuilder::new().mode(0o700).create(&store).unwrap();
                let session = SessionId::new(SESSION).unwrap();
                let config = ServiceConfig {
                    store_root: store,
                    project_id: ProjectId::new(polaris_core::project::project_id(&source)).unwrap(),
                    session_id: session.clone(),
                };
                let mut service = DesktopService::open_source_bound_with_trusted_runs(
                    config.clone(),
                    source.clone(),
                    (meta.dev(), meta.ino()),
                    configuration,
                    factory,
                    SourceApplyRuntime {
                        factory: Arc::new(source_factory),
                        clock: Arc::new(Clock),
                        limits: limits(),
                        approval_ttl_ms: 60_000,
                    },
                )
                .unwrap();
                let (io, peer) = tokio::io::duplex(2 * codec::MAX_FRAME_BYTES);
                let (read, write) = tokio::io::split(peer);
                let handle = tokio::spawn(async move { service.serve(read, write).await });
                let mut owner = Frontend {
                    io,
                    service: handle,
                    session,
                    client: ClientId::new("fixture-first-process").unwrap(),
                    hello: false,
                    usable: true,
                    sequence: 0,
                    writable: true,
                };
                let before = owner.snapshot().await.unwrap();
                let SuccessResult::DraftUpdate(draft) = owner
                    .request(RequestBody::DraftUpdate(
                        owner.session.clone(),
                        DraftUpdate {
                            expected_draft_revision: before.draft.draft_revision,
                            text: "fixture input".into(),
                            attachment_ids: vec![],
                        },
                    ))
                    .await
                    .unwrap()
                else {
                    panic!("draft")
                };
                owner
                    .request(RequestBody::RunStart(
                        owner.session.clone(),
                        RunStart {
                            expected_draft_revision: draft.draft_revision,
                            expected_configuration_revision: before
                                .configuration
                                .configuration_revision,
                            expected_policy_revision: before.policy_revision,
                            mcp_prompt: None,
                        },
                    ))
                    .await
                    .unwrap();
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                let item = loop {
                    let snapshot = owner.snapshot().await.unwrap();
                    if snapshot.runs.iter().any(|run| !run.state.is_terminal()) {
                        assert!(std::time::Instant::now() < deadline, "run did not join");
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        continue;
                    }
                    let (_, items) = owner.list().await.unwrap();
                    if let Some(item) = items.into_iter().next() {
                        break item;
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "no live completion candidate"
                    );
                    tokio::time::sleep(Duration::from_millis(10)).await;
                };
                assert_eq!(
                    fs::read_to_string(source.join("ordinary")).unwrap(),
                    "before"
                );
                let terminal = RefCell::new(
                    ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 25)).unwrap(),
                );
                let mut cancel = Keys(VecDeque::from([KeyCode::Esc]));
                assert!(
                    owner
                        .action(
                            &format!("resolve {} allow", item.approval_id.as_str()),
                            &terminal,
                            &mut cancel
                        )
                        .await
                        .unwrap()
                        .contains("取り消し")
                );
                assert_eq!(
                    fs::read_to_string(source.join("ordinary")).unwrap(),
                    "before"
                );
                let (revision, item, preview) =
                    owner.preview(item.approval_id.as_str()).await.unwrap();
                assert!(preview.iter().any(|s| s.contains("ordinary")));
                let answer = SourceApplyResolve {
                    run_id: item.run_id.clone(),
                    attempt_id: item.attempt_id.clone(),
                    approval_id: item.approval_id.clone(),
                    payload_hash: item.payload_hash.clone(),
                    expected_session_revision: revision,
                    expected_policy_revision: item.policy_revision,
                    decision: ApprovalDecision::Allow,
                };
                let mut stale = answer.clone();
                stale.expected_session_revision = revision.checked_add(1).unwrap();
                assert!(
                    owner
                        .request(RequestBody::SourceApplyResolve(
                            owner.session.clone(),
                            stale
                        ))
                        .await
                        .is_err()
                );
                assert_eq!(
                    fs::read_to_string(source.join("ordinary")).unwrap(),
                    "before"
                );
                let mut yes = Keys(VecDeque::from([KeyCode::Char('y')]));
                owner
                    .action(
                        &format!("resolve {} allow", item.approval_id.as_str()),
                        &terminal,
                        &mut yes,
                    )
                    .await
                    .unwrap();
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                loop {
                    let (_, items) = owner.list().await.unwrap();
                    if items[0].result_saved {
                        break;
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "source result not saved"
                    );
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                assert_eq!(
                    fs::read_to_string(source.join("ordinary")).unwrap(),
                    "after"
                );
                fs::write(source.join("ordinary"), "later external edit").unwrap();
                let _ = owner
                    .request(RequestBody::SourceApplyResolve(
                        owner.session.clone(),
                        answer,
                    ))
                    .await;
                assert_eq!(
                    fs::read_to_string(source.join("ordinary")).unwrap(),
                    "later external edit"
                );
                owner.shutdown().await.unwrap();
                let mut service = DesktopService::open_source_bound(
                    config,
                    source.clone(),
                    (meta.dev(), meta.ino()),
                )
                .unwrap();
                let (io, peer) = tokio::io::duplex(2 * codec::MAX_FRAME_BYTES);
                let (read, write) = tokio::io::split(peer);
                let handle = tokio::spawn(async move { service.serve(read, write).await });
                let mut reopened = Frontend {
                    io,
                    service: handle,
                    session: SessionId::new(SESSION).unwrap(),
                    client: ClientId::new("fixture-second-process").unwrap(),
                    hello: false,
                    usable: true,
                    sequence: 0,
                    writable: true,
                };
                let (_, items) = reopened.list().await.unwrap();
                assert!(items[0].result_saved);
                assert_eq!(
                    fs::read_to_string(source.join("ordinary")).unwrap(),
                    "later external edit"
                );
                reopened.shutdown().await.unwrap();
                println!("m2d-d27-receipt: verified");
            });
    }
}
