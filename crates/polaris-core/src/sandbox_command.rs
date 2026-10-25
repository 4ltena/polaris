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
    #[error("Sandbox management effect: {0}")]
    Effect(#[from] ManagementEffectStoreError),
    #[error("Sandbox storage: {0}")]
    Io(#[from] io::Error),
    #[error("sandbox: {0}")]
    Sandbox(#[from] SandboxError),
    #[error("This sandbox operation has already started or finalized. It will not be rerun")]
    ExistingOperation,
}

pub struct SandboxCommandPreviewV1 {
    argv: Vec<String>,
    policy: SandboxPolicy,
    before: ConversationSnapshot,
    executable_hash: String,
    request_hash: ManagementEffectHashV1,
    #[cfg(target_os = "macos")]
    scratch: Option<CommandScratch>,
}

#[cfg(target_os = "macos")]
struct CommandScratch {
    directory: Option<tempfile::TempDir>,
    pins: Vec<(PathBuf, fs::File)>,
    preserve: std::cell::Cell<bool>,
}

#[cfg(target_os = "macos")]
impl CommandScratch {
    fn prepare(
        policy: SandboxPolicy,
        data_root: &Path,
    ) -> Result<(SandboxPolicy, Self), SandboxCommandError> {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
        let boundary = policy
            .isolated_boundary()
            .ok_or_else(|| invalid("isolated policy is missing"))?;
        let parent = std::env::temp_dir().canonicalize()?;
        let data_root = data_root.canonicalize()?;
        if parent.starts_with(&boundary.workspace)
            || parent.starts_with(&data_root)
            || boundary
                .readable_roots
                .iter()
                .any(|root| parent.starts_with(root))
            || polaris_tools::path_policy::is_denied(&parent)
        {
            return Err(invalid("private scratch parent overlaps command authority").into());
        }
        let directory = polaris_auth::protection::with_protected_paths(|registered| {
            let secrets =
                crate::isolated_workspace::RegisteredSecrets::from_auth_registry(registered)
                    .map_err(|_| invalid("scratch protection is unavailable"))?;
            let metadata = parent.symlink_metadata()?;
            if !metadata.is_dir()
                || secrets.denies(&parent, &metadata)
                || registered
                    .paths()
                    .iter()
                    .any(|path| path.parent() == Some(parent.as_path()))
            {
                return Err(invalid("scratch parent is protected"));
            }
            tempfile::Builder::new()
                .prefix("polaris-command-")
                .permissions(fs::Permissions::from_mode(0o700))
                .tempdir_in(&parent)
        })
        .map_err(|_| invalid("scratch protection is unavailable"))??;
        let container = directory.path().canonicalize()?;
        if container.starts_with(&data_root) || data_root.starts_with(&container) {
            return Err(invalid("private scratch overlaps session storage").into());
        }
        let home = container.join("home");
        let tmp = container.join("tmp");
        for path in [&home, &tmp] {
            fs::DirBuilder::new().mode(0o700).create(path)?;
        }
        let policy = policy.with_isolated_private_environment(&container, &home, &tmp)?;
        let pins = [container, home, tmp]
            .into_iter()
            .map(|path| {
                let file = fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(&path)?;
                Ok((path, file))
            })
            .collect::<io::Result<Vec<_>>>()?;
        let scratch = Self {
            directory: Some(directory),
            pins,
            preserve: std::cell::Cell::new(false),
        };
        scratch.revalidate()?;
        Ok((policy, scratch))
    }

    fn revalidate(&self) -> io::Result<()> {
        use std::os::unix::fs::MetadataExt;
        for (path, pinned) in &self.pins {
            let named = path.symlink_metadata()?;
            let held = pinned.metadata()?;
            if !named.is_dir()
                || path.canonicalize()? != *path
                || (named.dev(), named.ino()) != (held.dev(), held.ino())
                || named.uid() != unsafe { libc::geteuid() }
                || named.mode() & 0o7777 != 0o700
            {
                return Err(invalid("private command scratch changed"));
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
impl Drop for CommandScratch {
    fn drop(&mut self) {
        // Never remove a changed namespace or scratch still used by an unconfirmed child.
        if (self.preserve.get() || self.revalidate().is_err())
            && let Some(directory) = self.directory.take()
        {
            let _ = directory.keep();
        }
    }
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
            return Err(invalid("Specify command as a JSON array beginning with an absolute program path, within 64 arguments and 8 KiB").into());
        }
        let project = project.canonicalize()?;
        let before = session.snapshot()?;
        if before.state.project_id
            != project
                .to_str()
                .ok_or_else(|| invalid("Project path is not UTF-8"))?
            || before.raw_offset != before.state.raw_commit_offset
            || before.raw_hash != before.state.raw_commit_hash
        {
            return Err(invalid(
                "Sandbox and saved conversation project or source marker mismatch",
            )
            .into());
        }
        let program = Path::new(&argv[0]).canonicalize()?;
        argv[0] = program
            .to_str()
            .ok_or_else(|| invalid("Program path is not UTF-8"))?
            .to_owned();
        let policy = contained_policy(startup, &project)?;
        if !allowed_program(&policy, &project, &program) {
            return Err(invalid("The program is outside the approved project/runtime").into());
        }
        let executable_hash = executable_hash(&program)?;
        #[cfg(target_os = "macos")]
        let (policy, scratch) = if policy
            .isolated_boundary()
            .is_some_and(|boundary| boundary.environment.is_none())
        {
            let (policy, scratch) = CommandScratch::prepare(policy, &session.data_root)?;
            (policy, Some(scratch))
        } else {
            (policy, None)
        };
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
            #[cfg(target_os = "macos")]
            scratch,
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
        #[cfg(target_os = "macos")]
        if let Some(scratch) = &prepared.preview.scratch {
            scratch.preserve.set(true);
        }
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
            #[cfg(target_os = "macos")]
            if let Some(scratch) = &prepared.preview.scratch {
                scratch.preserve.set(false);
            }
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
        #[cfg(target_os = "macos")]
        if let Some(scratch) = &preview.scratch {
            scratch.revalidate()?;
        }
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
                    .ok_or_else(|| invalid("Invalid project path"))?
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
            return Err(
                invalid("The confirmed sandbox project, conversation, or program changed").into(),
            );
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
        return Err(invalid("The program must be a regular file within 32 MiB"));
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_EXECUTABLE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_EXECUTABLE_BYTES {
        return Err(invalid("The program changed during confirmation"));
    }
    Ok(content_hash(&bytes))
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(all(test, target_os = "macos"))]
mod scratch_tests {
    use super::*;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};

    fn prepared() -> (tempfile::TempDir, SandboxPolicy, CommandScratch) {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let data = root.path().join("data");
        fs::create_dir(&project).unwrap();
        fs::create_dir(&data).unwrap();
        let policy = SandboxPolicy::isolated(SandboxMode::ReadOnly, &project, &[]).unwrap();
        let (policy, scratch) = CommandScratch::prepare(policy, &data).unwrap();
        (root, policy, scratch)
    }

    fn remove_empty_scratch(container: &Path) {
        fs::remove_dir(container.join("home")).unwrap();
        fs::remove_dir(container.join("tmp")).unwrap();
        fs::remove_dir(container).unwrap();
    }

    #[test]
    fn private_scratch_is_owned_through_preview_and_cleaned_after_confirmed_stop() {
        let (_root, policy, scratch) = prepared();
        let environment = policy
            .isolated_boundary()
            .unwrap()
            .environment
            .as_ref()
            .unwrap();
        let container = environment.home.parent().unwrap().to_owned();
        for path in [&container, &environment.home, &environment.tmpdir] {
            assert_eq!(fs::metadata(path).unwrap().mode() & 0o7777, 0o700);
        }
        scratch.revalidate().unwrap();
        drop(scratch);
        assert!(!container.exists());
    }

    #[test]
    fn failed_owner_dispatch_retains_scratch_and_unknown_receipt() {
        use crate::{
            conversation_state::HistoryMode, management_effect::ManagementEffectStateV1,
            rewind_workspace::WorkspaceOwner,
        };
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let data = root.path().join("data");
        let state = root.path().join("state");
        for path in [&project, &data, &state] {
            fs::DirBuilder::new().mode(0o700).create(path).unwrap();
        }
        let project = project.canonicalize().unwrap();
        let program = PathBuf::from("/usr/bin/printf").canonicalize().unwrap();
        let session = PersistedSession::create(
            &data,
            &state.join("memory.sqlite3"),
            project.to_str().unwrap(),
            HistoryMode::Legacy,
            None,
        )
        .unwrap();
        let workspace = WorkspaceOwner::open(
            &project,
            &state.join("rewind-workspace"),
            Default::default(),
        )
        .unwrap();
        let lease = workspace.try_mutation().unwrap();
        let effects = ManagementEffectStoreV1::open_at_state_dir(&project, &state, &lease).unwrap();
        let owner = SandboxCommandOwnerV1 {
            effects: &effects,
            lease: &lease,
            project: &project,
        };
        let startup = SandboxPolicy::new(SandboxMode::ReadOnly, &[]).unwrap();
        let preview = SandboxCommandOwnerV1::preview(
            &session,
            &project,
            &startup,
            vec![program.to_str().unwrap().into()],
        )
        .unwrap();
        let container = preview
            .policy()
            .isolated_boundary()
            .unwrap()
            .environment
            .as_ref()
            .unwrap()
            .home
            .parent()
            .unwrap()
            .to_owned();
        let prepared = owner.prepare(&session, preview).unwrap();
        let home = container.join("home");
        let retained_home = container.join("retained-home");
        let mut faulted = false;
        assert!(
            owner
                .execute(&session, &prepared, || {
                    if !faulted {
                        fs::rename(&home, &retained_home).unwrap();
                        faulted = true;
                    }
                    false
                })
                .is_err()
        );
        assert!(faulted);
        // Restore the pinned inode so retention depends on the dispatch error,
        // rather than Drop's separate namespace-change protection.
        fs::rename(&retained_home, &home).unwrap();
        prepared
            .preview
            .scratch
            .as_ref()
            .unwrap()
            .revalidate()
            .unwrap();
        assert!(matches!(
            effects.effects().unwrap()[0].state,
            ManagementEffectStateV1::OutcomeUnknown { .. }
        ));
        assert!(owner.execute(&session, &prepared, || false).is_err());
        assert_eq!(effects.effects().unwrap().len(), 1);
        drop(prepared);
        assert!(container.join("home").is_dir());
        assert!(container.join("tmp").is_dir());
        remove_empty_scratch(&container);
    }

    #[test]
    fn replaced_scratch_namespace_is_refused_and_not_deleted() {
        let (_root, policy, scratch) = prepared();
        let environment = policy
            .isolated_boundary()
            .unwrap()
            .environment
            .as_ref()
            .unwrap();
        let container = environment.home.parent().unwrap().to_owned();
        let retained = container.join("retained-home");
        fs::rename(&environment.home, &retained).unwrap();
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&environment.home)
            .unwrap();
        fs::write(environment.home.join("replacement"), b"must remain").unwrap();
        assert!(scratch.revalidate().is_err());
        drop(scratch);
        assert_eq!(
            fs::read(environment.home.join("replacement")).unwrap(),
            b"must remain"
        );
        fs::remove_file(environment.home.join("replacement")).unwrap();
        fs::remove_dir(retained).unwrap();
        remove_empty_scratch(&container);
    }
}
