//! One explicit command under a narrowed startup ceiling and project lease.
//! A started or uncertain operation is never dispatched again.

use crate::{
    conversation_state::{ConversationSnapshot, content_hash},
    management_effect::{
        ManagementDenialReasonV1, ManagementEffectHashV1, ManagementEffectScopeV1,
        ManagementEffectStoreError, ManagementEffectStoreV1, ManagementInventoryIdV1,
        ManagementOperationIdV1, ManagementPermissionScopeV1, ManagementRewindDispositionV1,
        ManagementUnknownReasonV1, PrepareManagementEffectOutcomeV1, PrepareManagementEffectV1,
        PreparedManagementEffectV1,
    },
    project_mutation::ProjectMutationLeaseV1,
    session_store::PersistedSession,
};
use polaris_sandbox::{ControlledEnd, ControlledOutcome, SandboxError, SandboxMode, SandboxPolicy};
use std::{
    fs,
    io::{self, Read},
    path::Path,
};

#[cfg(not(target_os = "linux"))]
use std::path::PathBuf;

const MAX_COMMAND_BYTES: usize = 8 * 1024;
const MAX_EXECUTABLE_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum SandboxCommandError {
    #[error("sandbox管理効果: {0}")]
    Effect(#[from] ManagementEffectStoreError),
    #[error("sandbox保存: {0}")]
    Io(#[from] io::Error),
    #[error("sandbox: {0}")]
    Sandbox(#[from] SandboxError),
    #[error("このsandbox操作は既に開始または確定しています。再実行しません")]
    ExistingOperation,
}

pub struct SandboxCommandPreviewV1 {
    argv: Vec<String>,
    policy: SandboxPolicy,
    before: ConversationSnapshot,
    executable_hash: String,
    request_hash: ManagementEffectHashV1,
}

impl SandboxCommandPreviewV1 {
    pub fn argv(&self) -> &[String] {
        &self.argv
    }
    pub fn policy(&self) -> &SandboxPolicy {
        &self.policy
    }
}

pub struct PreparedSandboxCommandV1 {
    effect: PreparedManagementEffectV1,
    preview: SandboxCommandPreviewV1,
}

impl PreparedSandboxCommandV1 {
    pub fn operation_id(&self) -> ManagementOperationIdV1 {
        self.effect.operation_id()
    }
}

pub struct SandboxCommandOwnerV1<'a> {
    pub effects: &'a ManagementEffectStoreV1,
    pub lease: &'a ProjectMutationLeaseV1,
    pub project: &'a Path,
}

impl SandboxCommandOwnerV1<'_> {
    pub fn preview(
        session: &PersistedSession,
        project: &Path,
        startup: &SandboxPolicy,
        mut argv: Vec<String>,
    ) -> Result<SandboxCommandPreviewV1, SandboxCommandError> {
        if argv.is_empty()
            || argv.len() > 64
            || argv.iter().map(String::len).sum::<usize>() > MAX_COMMAND_BYTES
            || argv.iter().any(|arg| arg.contains('\0'))
            || !Path::new(&argv[0]).is_absolute()
        {
            return Err(invalid("commandは絶対pathのprogramを先頭にした64引数・8KiB以内のJSON配列で指定してください").into());
        }
        let project = project.canonicalize()?;
        let before = session.snapshot()?;
        if before.state.project_id
            != project
                .to_str()
                .ok_or_else(|| invalid("project pathがUTF-8ではありません"))?
            || before.raw_offset != before.state.raw_commit_offset
            || before.raw_hash != before.state.raw_commit_hash
        {
            return Err(invalid("sandboxと保存会話のprojectまたは原文markerが一致しません").into());
        }
        let program = Path::new(&argv[0]).canonicalize()?;
        argv[0] = program
            .to_str()
            .ok_or_else(|| invalid("program pathがUTF-8ではありません"))?
            .to_owned();
        let policy = contained_policy(startup, &project)?;
        if !allowed_program(&policy, &project, &program) {
            return Err(invalid("programが承認されたproject/runtimeの外にあります").into());
        }
        let executable_hash = executable_hash(&program)?;
        let request_hash = ManagementEffectHashV1::digest(
            &serde_json::to_vec(&(
                &argv,
                &before.state,
                &executable_hash,
                format!("{policy:?}"),
            ))
            .map_err(io::Error::other)?,
        );
        Ok(SandboxCommandPreviewV1 {
            argv,
            policy,
            before,
            executable_hash,
            request_hash,
        })
    }

    pub fn prepare(
        &self,
        session: &PersistedSession,
        preview: SandboxCommandPreviewV1,
    ) -> Result<PreparedSandboxCommandV1, SandboxCommandError> {
        self.check(session, &preview)?;
        let request = PrepareManagementEffectV1 {
            operation_id: ManagementOperationIdV1::random()
                .map_err(|error| io::Error::other(error.to_string()))?,
            inventory: ManagementInventoryIdV1::parse("C38").expect("fixed sandbox inventory"),
            scope: ManagementEffectScopeV1 {
                project: self.effects.project_scope_hash(),
                session: Some(ManagementEffectHashV1::digest(
                    preview.before.state.session_id.as_bytes(),
                )),
                generation: preview.before.state.generation,
                revision: 0,
                permission: if prepared_system_scope(&preview.policy) {
                    ManagementPermissionScopeV1::System
                } else {
                    ManagementPermissionScopeV1::Project
                },
                account_binding: None,
            },
            request_hash: preview.request_hash,
            rewind: ManagementRewindDispositionV1::RuntimeUnknown,
        };
        match self
            .effects
            .prepare(self.lease, self.effects.cursor()?, request)?
        {
            PrepareManagementEffectOutcomeV1::Prepared(effect) => {
                Ok(PreparedSandboxCommandV1 { effect, preview })
            }
            PrepareManagementEffectOutcomeV1::Existing(_) => {
                Err(SandboxCommandError::ExistingOperation)
            }
        }
    }

    pub fn deny(&self, prepared: &PreparedSandboxCommandV1) -> Result<(), SandboxCommandError> {
        self.effects.deny(
            self.lease,
            &prepared.effect,
            ManagementDenialReasonV1::UserDeclined,
        )?;
        Ok(())
    }

    pub fn execute(
        &self,
        session: &PersistedSession,
        prepared: &PreparedSandboxCommandV1,
        cancelled: impl FnMut() -> bool,
    ) -> Result<ControlledOutcome, SandboxCommandError> {
        if let Err(error) = self.check(session, &prepared.preview) {
            self.effects.deny(
                self.lease,
                &prepared.effect,
                ManagementDenialReasonV1::StaleRequest,
            )?;
            return Err(error);
        }
        let started = self.effects.start(self.lease, &prepared.effect)?;
        let result = polaris_sandbox::run_confined_controlled_in_dir(
            &prepared.preview.policy,
            Path::new(&prepared.preview.argv[0]),
            &prepared.preview.argv[1..],
            None,
            Some(self.project),
            cancelled,
        );
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(error) => {
                let _ = self.effects.mark_outcome_unknown(
                    self.lease,
                    &started,
                    ManagementUnknownReasonV1::InterruptedAfterStart,
                );
                return Err(error.into());
            }
        };
        if outcome.end == ControlledEnd::StopUnconfirmed
            || outcome.pending.is_some()
            || !outcome.stdout_eof
            || !outcome.stderr_eof
            || outcome.problem.is_some()
        {
            self.effects.mark_outcome_unknown(
                self.lease,
                &started,
                ManagementUnknownReasonV1::DetachedEffect,
            )?;
        } else {
            let evidence = ManagementEffectHashV1::digest(
                &serde_json::to_vec(&(
                    prepared.effect.operation_id().as_hex(),
                    format!("{:?}", outcome.end),
                    outcome.status.as_ref().and_then(|status| status.code()),
                    &outcome.stdout,
                    &outcome.stderr,
                    outcome.stdout_truncated,
                    outcome.stderr_truncated,
                ))
                .map_err(io::Error::other)?,
            );
            if let Err(error) = self
                .effects
                .complete_started(self.lease, &started, evidence)
            {
                let _ = self.effects.mark_outcome_unknown(
                    self.lease,
                    &started,
                    ManagementUnknownReasonV1::ReceiptPersistenceFailed,
                );
                return Err(error.into());
            }
        }
        Ok(outcome)
    }

    fn check(
        &self,
        session: &PersistedSession,
        preview: &SandboxCommandPreviewV1,
    ) -> Result<(), SandboxCommandError> {
        let current = session.snapshot()?;
        let project = self.project.canonicalize()?;
        if current.state != preview.before.state
            || current.raw_hash != preview.before.raw_hash
            || current.raw_offset != preview.before.raw_offset
            || ManagementEffectHashV1::digest(project.as_os_str().as_encoded_bytes())
                != self.effects.project_scope_hash()
            || current.state.project_id
                != project
                    .to_str()
                    .ok_or_else(|| invalid("project pathが不正です"))?
            || preview
                .policy
                .isolated_boundary()
                .is_some_and(|boundary| boundary.workspace != project)
            || !allowed_program(&preview.policy, &project, Path::new(&preview.argv[0]))
            || session.store.directory().canonicalize()?
                != session
                    .data_root
                    .join("sessions-v2")
                    .join(&current.state.session_id)
                    .canonicalize()?
            || executable_hash(Path::new(&preview.argv[0]))? != preview.executable_hash
        {
            return Err(invalid("確認したsandboxのproject・会話・programが変わりました").into());
        }
        Ok(())
    }
}

fn contained_policy(
    startup: &SandboxPolicy,
    project: &Path,
) -> Result<SandboxPolicy, SandboxError> {
    let mode = if startup.mode() == SandboxMode::ReadOnly {
        SandboxMode::ReadOnly
    } else {
        SandboxMode::WorkspaceWrite
    };
    if mode == SandboxMode::WorkspaceWrite && !startup.contains(project) {
        return Err(SandboxError::NotEnforced(
            "project exceeds startup write ceiling".into(),
        ));
    }
    #[cfg(target_os = "linux")]
    {
        // Match the existing CLI Linux Landlock policy; never discard a stronger isolated ceiling.
        if startup.isolated_boundary().is_some() {
            return Err(SandboxError::NotEnforced(
                "Linux does not implement the requested isolated read/network boundary".into(),
            ));
        }
        let roots = if mode == SandboxMode::ReadOnly {
            vec![]
        } else {
            vec![project.to_path_buf()]
        };
        return startup.restrict(mode, &roots);
    }
    #[cfg(not(target_os = "linux"))]
    {
        if let Some(boundary) = startup.isolated_boundary() {
            if boundary.workspace != project {
                return Err(SandboxError::NotEnforced(
                    "startup workspace differs from selected project".into(),
                ));
            }
            let roots = if mode == SandboxMode::ReadOnly {
                vec![]
            } else {
                vec![project.to_path_buf()]
            };
            return startup
                .restrict(mode, &roots)?
                .require_process_containment();
        }
        let runtime: Vec<PathBuf> = ["/usr", "/bin", "/lib", "/lib64"]
            .into_iter()
            .map(PathBuf::from)
            .filter(|path| path.is_dir())
            .collect();
        SandboxPolicy::isolated(mode, project, &runtime)?.require_process_containment()
    }
}

fn prepared_system_scope(policy: &SandboxPolicy) -> bool {
    policy.isolated_boundary().is_none()
}
fn allowed_program(policy: &SandboxPolicy, project: &Path, program: &Path) -> bool {
    if let Some(boundary) = policy.isolated_boundary() {
        return boundary
            .readable_roots
            .iter()
            .any(|root| program.starts_with(root));
    }
    program.starts_with(project)
        || ["/usr", "/bin", "/lib", "/lib64"]
            .iter()
            .any(|root| program.starts_with(root))
}

fn executable_hash(program: &Path) -> io::Result<String> {
    let mut file = fs::File::open(program)?;
    if !file.metadata()?.is_file() || file.metadata()?.len() > MAX_EXECUTABLE_BYTES {
        return Err(invalid("programは32MiB以内の通常fileである必要があります"));
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_EXECUTABLE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_EXECUTABLE_BYTES {
        return Err(invalid("programが確認中に変わりました"));
    }
    Ok(content_hash(&bytes))
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
