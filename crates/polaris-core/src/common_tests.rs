//! A small, bounded black-box test schema and its confined runner.
//!
//! The caller supplies the execution boundary. A plan cannot select a shell,
//! a cwd, a writable root, an environment, or a timeout.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use polaris_sandbox::{
    ControlledEnd, PendingCleanup, SandboxError, SandboxPolicy, run_confined_controlled_in_dir,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const SCHEMA_VERSION: u8 = 1;
pub const MAX_PLAN_BYTES: usize = 64 * 1024;
const MAX_CASES: usize = 32;
const MAX_ARGV: usize = 64;
const MAX_TEXT_BYTES: usize = 16 * 1024;
const MAX_PATHS: usize = 32;
const MAX_OUTPUT_RECORD_BYTES: usize = 8 * 1024;
const MAX_OBSERVED_FILE_BYTES: u64 = 1024 * 1024;
const MAX_EXPECTED_NODES: usize = 256;
const MAX_EXPECTED_DEPTH: usize = 16;
const MAX_PROGRAM_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PointerExpectation {
    pub pointer: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "case", deny_unknown_fields)]
pub enum Case {
    ExitsWith {
        status: i32,
    },
    RejectsInput {
        /// Replacement arguments after the plan's program path.
        argv: Vec<String>,
        stdin: Option<String>,
        /// Files that must remain absent before and after the rejected call.
        #[serde(default)]
        observed_paths: Vec<String>,
    },
    Says {
        stream: Stream,
        contains: Vec<String>,
        absent: Vec<String>,
    },
    Creates {
        paths: Vec<String>,
    },
    StructuredOutput {
        pointers: Vec<PointerExpectation>,
    },
    RepeatsCleanly {
        #[serde(default)]
        observed_paths: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stream {
    Stdout,
    Stderr,
}

/// Strict v1 input. Unknown fields are rejected by serde rather than ignored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub version: u8,
    pub argv: Vec<String>,
    #[serde(default)]
    pub stdin: Option<String>,
    pub cases: Vec<Case>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    UnsupportedVersion(u8),
    EmptyArgv,
    ShellString(String),
    NoCases,
    TooMany { field: &'static str, limit: usize },
    EmptyContract(&'static str),
    OversizedText(&'static str),
    OversizedPlan,
    OversizedExpectedValue,
    InvalidPointer(String),
    EscapesWriteRoot(String),
}

impl Plan {
    pub fn check(&self) -> Result<(), PlanError> {
        if self.version != SCHEMA_VERSION {
            return Err(PlanError::UnsupportedVersion(self.version));
        }
        if serde_json::to_vec(self)
            .map_err(|_| PlanError::OversizedPlan)?
            .len()
            > MAX_PLAN_BYTES
        {
            return Err(PlanError::OversizedPlan);
        }
        check_argv(&self.argv, "argv")?;
        check_text(self.stdin.as_deref(), "stdin")?;
        if self.cases.is_empty() {
            return Err(PlanError::NoCases);
        }
        if self.cases.len() > MAX_CASES {
            return Err(PlanError::TooMany {
                field: "cases",
                limit: MAX_CASES,
            });
        }
        for case in &self.cases {
            match case {
                Case::RejectsInput {
                    argv,
                    stdin,
                    observed_paths,
                } => {
                    if argv.is_empty() && stdin.is_none() {
                        return Err(PlanError::EmptyContract("rejects_input"));
                    }
                    if argv.len() > MAX_ARGV {
                        return Err(PlanError::TooMany {
                            field: "rejects_input.argv",
                            limit: MAX_ARGV,
                        });
                    }
                    for arg in argv {
                        check_text(Some(arg), "rejects_input.argv")?;
                    }
                    check_text(stdin.as_deref(), "rejects_input.stdin")?;
                    check_paths(observed_paths, "rejects_input.observed_paths")?;
                }
                Case::Says {
                    contains, absent, ..
                } => {
                    if contains.is_empty() && absent.is_empty() {
                        return Err(PlanError::EmptyContract("says"));
                    }
                    if contains.len() + absent.len() > MAX_PATHS {
                        return Err(PlanError::TooMany {
                            field: "says",
                            limit: MAX_PATHS,
                        });
                    }
                    for text in contains.iter().chain(absent) {
                        check_text(Some(text), "says")?;
                    }
                }
                Case::Creates { paths } => check_paths(paths, "creates.paths")?,
                Case::StructuredOutput { pointers } => {
                    if pointers.is_empty() {
                        return Err(PlanError::EmptyContract("structured_output"));
                    }
                    if pointers.len() > MAX_PATHS {
                        return Err(PlanError::TooMany {
                            field: "structured_output.pointers",
                            limit: MAX_PATHS,
                        });
                    }
                    for item in pointers {
                        if !item.pointer.is_empty() && !item.pointer.starts_with('/') {
                            return Err(PlanError::InvalidPointer(item.pointer.clone()));
                        }
                        check_text(Some(&item.pointer), "structured_output.pointer")?;
                        if let Some(expected) = &item.expected {
                            check_expected_value(expected)?;
                        }
                    }
                }
                Case::RepeatsCleanly { observed_paths } => {
                    check_paths(observed_paths, "repeats_cleanly.observed_paths")?
                }
                Case::ExitsWith { .. } => {}
            }
        }
        Ok(())
    }

    pub fn case_names(&self) -> Vec<&'static str> {
        self.cases
            .iter()
            .map(|case| match case {
                Case::ExitsWith { .. } => "exits_with",
                Case::RejectsInput { .. } => "rejects_input",
                Case::Says { .. } => "says",
                Case::Creates { .. } => "creates",
                Case::StructuredOutput { .. } => "structured_output",
                Case::RepeatsCleanly { .. } => "repeats_cleanly",
            })
            .collect()
    }
}

fn check_expected_value(value: &Value) -> Result<(), PlanError> {
    if serde_json::to_vec(value)
        .map_err(|_| PlanError::OversizedExpectedValue)?
        .len()
        > MAX_TEXT_BYTES
    {
        return Err(PlanError::OversizedExpectedValue);
    }
    let mut todo = vec![(value, 1_usize)];
    let mut nodes = 0_usize;
    while let Some((value, depth)) = todo.pop() {
        nodes += 1;
        if nodes > MAX_EXPECTED_NODES || depth > MAX_EXPECTED_DEPTH {
            return Err(PlanError::OversizedExpectedValue);
        }
        match value {
            Value::Array(items) => todo.extend(items.iter().map(|item| (item, depth + 1))),
            Value::Object(items) => todo.extend(items.values().map(|item| (item, depth + 1))),
            _ => {}
        }
    }
    Ok(())
}

fn check_argv(argv: &[String], field: &'static str) -> Result<(), PlanError> {
    let Some(program) = argv.first() else {
        return Err(PlanError::EmptyArgv);
    };
    if argv.len() > MAX_ARGV {
        return Err(PlanError::TooMany {
            field,
            limit: MAX_ARGV,
        });
    }
    if argv.len() == 1 && program.split_whitespace().count() > 1 {
        return Err(PlanError::ShellString(program.clone()));
    }
    for arg in argv {
        check_text(Some(arg), field)?;
    }
    Ok(())
}
fn check_text(text: Option<&str>, field: &'static str) -> Result<(), PlanError> {
    if text.is_some_and(|text| text.len() > MAX_TEXT_BYTES) {
        Err(PlanError::OversizedText(field))
    } else {
        Ok(())
    }
}
fn check_paths(paths: &[String], field: &'static str) -> Result<(), PlanError> {
    if paths.is_empty() {
        return Err(PlanError::EmptyContract(field));
    }
    if paths.len() > MAX_PATHS {
        return Err(PlanError::TooMany {
            field,
            limit: MAX_PATHS,
        });
    }
    for path in paths {
        check_text(Some(path), field)?;
        let path = Path::new(path);
        if path.is_absolute() || path.components().any(|part| part.as_os_str() == "..") {
            return Err(PlanError::EscapesWriteRoot(path.display().to_string()));
        }
    }
    Ok(())
}

/// Execution boundaries remain with the caller; this type creates no files
/// and does not derive a less-restrictive sandbox policy.
pub struct RunConfig<'a> {
    pub policy: &'a SandboxPolicy,
    pub cwd: &'a Path,
    pub write_root: &'a Path,
    pub timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "result")]
pub enum RunReport {
    Passed {
        identity: RunIdentityV1,
        cases: Vec<CaseReport>,
        fallback_required: bool,
    },
    Failed {
        identity: Option<RunIdentityV1>,
        cases: Vec<CaseReport>,
        fallback_required: bool,
    },
    Unavailable {
        identity: Option<RunIdentityV1>,
        reason: String,
        fallback_required: bool,
    },
}

/// Hashes established inside the runner after plan validation and program
/// snapshotting. They never contain command output, environment values, or
/// private paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunIdentityV1 {
    pub plan_hash: String,
    pub program_hash: String,
}

/// The serializable public report is deliberately separate from native
/// ownership.  A caller must retain this outcome until cleanup completes.
#[must_use = "poll the owned scratch cleanup before discarding a pending run"]
pub struct RunOutcome {
    pub report: RunReport,
    pub identity: Option<RunIdentityV1>,
    cleanup: Option<OwnedScratchCleanup>,
}

static ABANDONED_RUN_OWNERS: Mutex<Vec<OwnedScratchCleanup>> = Mutex::new(Vec::new());

impl RunOutcome {
    fn clean(report: RunReport) -> Self {
        let identity = report.identity().cloned();
        Self {
            report,
            identity,
            cleanup: None,
        }
    }

    fn pending(report: RunReport, cleanup: OwnedScratchCleanup) -> Self {
        let identity = report.identity().cloned();
        Self {
            report,
            identity,
            cleanup: Some(cleanup),
        }
    }

    pub fn has_pending_cleanup(&self) -> bool {
        self.cleanup.is_some()
    }

    pub fn cleanup_mut(&mut self) -> Option<&mut OwnedScratchCleanup> {
        self.cleanup.as_mut()
    }

    pub fn take_cleanup(&mut self) -> Option<OwnedScratchCleanup> {
        self.cleanup.take()
    }

    /// Transfer the complete owner to the in-process supervisor registry.
    /// This is deliberately explicit so callers never silently drop only the
    /// native child handle while Scratch is still reachable.
    pub fn park_cleanup(&mut self) -> bool {
        let Some(cleanup) = self.cleanup.take() else {
            return false;
        };
        ABANDONED_RUN_OWNERS
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(cleanup);
        true
    }
}

impl Drop for RunOutcome {
    fn drop(&mut self) {
        if self.cleanup.is_some() {
            // Library callers can abandon a result during unwind. Keep the
            // compound owner together: moving only PendingCleanup would allow
            // Scratch's Drop to delete directories still reachable by a child.
            let _ = self.park_cleanup();
        }
    }
}

/// Transfer only compound common-runner owners. This intentionally does not
/// inspect the sandbox-wide registry, which may contain unrelated services.
pub fn take_abandoned_run_owners() -> Vec<OwnedScratchCleanup> {
    std::mem::take(
        &mut *ABANDONED_RUN_OWNERS
            .lock()
            .unwrap_or_else(|error| error.into_inner()),
    )
}

impl RunReport {
    pub fn identity(&self) -> Option<&RunIdentityV1> {
        match self {
            Self::Passed { identity, .. } => Some(identity),
            Self::Failed { identity, .. } | Self::Unavailable { identity, .. } => identity.as_ref(),
        }
    }

    /// Deterministic evidence provenance excluding stdout and stderr. It binds
    /// only the trusted identity and bounded outcome shape.
    pub fn bounded_evidence_hash(&self) -> String {
        #[derive(Serialize)]
        struct EvidenceCase<'a> {
            case: &'a str,
            passed: bool,
            status: Option<i32>,
            output_truncated: bool,
            timed_out: bool,
            observed_paths: &'a [ObservedPath],
        }
        #[derive(Serialize)]
        #[serde(rename_all = "snake_case", tag = "result")]
        enum Shape<'a> {
            Passed {
                identity: &'a RunIdentityV1,
                cases: Vec<EvidenceCase<'a>>,
            },
            Failed {
                identity: Option<&'a RunIdentityV1>,
                cases: Vec<EvidenceCase<'a>>,
            },
            Unavailable {
                identity: Option<&'a RunIdentityV1>,
                reason: &'a str,
            },
        }
        fn cases<'a>(records: &'a [CaseReport]) -> Vec<EvidenceCase<'a>> {
            records
                .iter()
                .map(|record| EvidenceCase {
                    case: record.case,
                    passed: record.passed,
                    status: record.status,
                    output_truncated: record.output_truncated,
                    timed_out: record.timed_out,
                    observed_paths: &record.observed_paths,
                })
                .collect()
        }
        let shape = match self {
            Self::Passed {
                identity,
                cases: records,
                ..
            } => Shape::Passed {
                identity,
                cases: cases(records),
            },
            Self::Failed {
                identity,
                cases: records,
                ..
            } => Shape::Failed {
                identity: identity.as_ref(),
                cases: cases(records),
            },
            Self::Unavailable {
                identity, reason, ..
            } => Shape::Unavailable {
                identity: identity.as_ref(),
                reason,
            },
        };
        format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&shape).expect("bounded common-case report serializes")
            )
        )
    }
}
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CaseReport {
    pub case: &'static str,
    pub passed: bool,
    pub detail: String,
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub output_truncated: bool,
    pub timed_out: bool,
    pub observed_paths: Vec<ObservedPath>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ObservedPath {
    pub path: String,
    pub exists: bool,
    pub state: ObservedPathState,
    pub fingerprint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedPathState {
    Absent,
    File,
    Unsafe,
    Unreadable,
}
struct Invocation {
    status: Option<i32>,
    stdout: String,
    stderr: String,
    truncated: bool,
    timed_out: bool,
    clean: bool,
    detail: String,
    pending: Option<PendingCleanup>,
}

impl Invocation {
    fn timed_out() -> Self {
        Self {
            status: None,
            stdout: String::new(),
            stderr: String::new(),
            truncated: false,
            timed_out: true,
            clean: false,
            detail: "plan deadline expired before spawn".into(),
            pending: None,
        }
    }
}

struct Scratch {
    root: PathBuf,
    workspace: PathBuf,
    home: PathBuf,
    tmpdir: PathBuf,
    preserve: bool,
}

impl Scratch {
    fn new() -> Result<Self, String> {
        let root = tempfile::Builder::new()
            .prefix("polaris-common-tests-")
            .tempdir()
            .map_err(|error| format!("private scratch is unavailable: {error}"))?
            .keep()
            .canonicalize()
            .map_err(|error| format!("private scratch is unavailable: {error}"))?;
        // The case program sees only this empty, runner-owned workspace.  It
        // is deliberately a sibling of the private HOME/TMP container: the
        // sandbox policy makes the former readable and the latter private.
        let workspace = match (|| {
            tempfile::Builder::new()
                .prefix("polaris-common-tests-workspace-")
                .tempdir()
                .map_err(|error| format!("private workspace is unavailable: {error}"))?
                .keep()
                .canonicalize()
                .map_err(|error| format!("private workspace is unavailable: {error}"))
        })() {
            Ok(workspace) => workspace,
            Err(error) => {
                let _ = remove_private_scratch(&root);
                return Err(error);
            }
        };
        let home = root.join("home");
        let tmpdir = root.join("tmp");
        if let Err(error) = std::fs::create_dir(&home).and_then(|_| std::fs::create_dir(&tmpdir)) {
            let _ = remove_private_scratch(&root);
            let _ = remove_private_workspace(&workspace);
            return Err(format!("private scratch is unavailable: {error}"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in [&root, &workspace, &home, &tmpdir] {
                if let Err(error) =
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
                {
                    let _ = remove_private_scratch(&root);
                    let _ = remove_private_workspace(&workspace);
                    return Err(format!("private scratch is unavailable: {error}"));
                }
            }
        }
        Ok(Self {
            root,
            workspace,
            home,
            tmpdir,
            preserve: false,
        })
    }
    fn cleanup(&mut self) {
        if !self.preserve {
            let _ = remove_private_scratch(&self.root);
            let _ = remove_private_workspace(&self.workspace);
        }
    }

    fn snapshot_program(&self, source: &Path) -> Result<ProgramIdentity, String> {
        let metadata = source
            .metadata()
            .map_err(|error| format!("program is unreadable: {error}"))?;
        if !metadata.is_file() || metadata.len() > MAX_PROGRAM_BYTES {
            return Err("program is not an admitted regular executable".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o111 == 0 {
                return Err("program is not executable".into());
            }
        }
        let destination = self.workspace.join("program");
        std::fs::copy(source, &destination)
            .map_err(|error| format!("program snapshot is unavailable: {error}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o500))
                .map_err(|error| format!("program snapshot is unavailable: {error}"))?;
        }
        program_identity(&destination)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// Non-serializable, non-cloneable ownership of the exact unreaped child and
/// the two private directories it can still access.  It never consults or
/// drains the process-global registry, so unrelated run owners stay intact.
#[must_use = "poll the exact child and remove private scratch before returning"]
pub struct OwnedScratchCleanup {
    pending: PendingCleanup,
    root: Option<PathBuf>,
    workspace: Option<PathBuf>,
    child_reaped: bool,
}

impl OwnedScratchCleanup {
    pub fn child_id(&self) -> Option<u32> {
        self.pending.child_id()
    }

    /// Returns true only after this direct child is reaped, its original
    /// process group is absent, and both validated private directories are
    /// removed.  A false result retains the same owner for a later poll.
    pub fn try_stop_reap_and_cleanup(&mut self) -> Result<bool, String> {
        if !self.child_reaped {
            match self.pending.try_stop_and_reap() {
                // `PendingCleanup` consumes its native owner once the direct
                // child and its process group have both disappeared. Its
                // public poll result is `None` both while it is pending and
                // after that consumed state, so distinguish them through the
                // remaining exact child identity before retaining scratch.
                Ok(None) if self.pending.child_id().is_some() => return Ok(false),
                Ok(None) => self.child_reaped = true,
                Err(error) => return Err(format!("child cleanup failed: {error}")),
                Ok(Some(_)) => self.child_reaped = true,
            }
        }
        if let Some(root) = self.root.as_deref() {
            remove_private_scratch(root)?;
            self.root = None;
        }
        if let Some(workspace) = self.workspace.as_deref() {
            remove_private_workspace(workspace)?;
            self.workspace = None;
        }
        Ok(true)
    }
}

fn remove_private_scratch(path: &Path) -> Result<(), String> {
    remove_private_tempdir(path, "polaris-common-tests-")
}

fn remove_private_workspace(path: &Path) -> Result<(), String> {
    remove_private_tempdir(path, "polaris-common-tests-workspace-")
}

fn remove_private_tempdir(path: &Path, expected_prefix: &str) -> Result<(), String> {
    let temp = std::env::temp_dir()
        .canonicalize()
        .map_err(|error| format!("temp root is unavailable: {error}"))?;
    let metadata = path
        .symlink_metadata()
        .map_err(|error| format!("scratch is unavailable: {error}"))?;
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("scratch is unavailable: {error}"))?;
    let name = canonical
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || !canonical.starts_with(&temp)
        || !name.starts_with(expected_prefix)
    {
        return Err("scratch path failed private-container validation".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err("scratch path is not private".into());
        }
    }
    std::fs::remove_dir_all(&canonical).map_err(|error| format!("scratch cleanup failed: {error}"))
}

fn unavailable(reason: String) -> RunOutcome {
    RunOutcome::clean(RunReport::Unavailable {
        identity: None,
        reason,
        fallback_required: true,
    })
}

fn deadline_report() -> CaseReport {
    CaseReport {
        case: "plan_deadline",
        passed: false,
        detail: "the plan deadline expired; no later invocation was spawned".into(),
        status: None,
        stdout: String::new(),
        stderr: String::new(),
        output_truncated: false,
        timed_out: true,
        observed_paths: Vec::new(),
    }
}

fn confined_policy(
    config: &RunConfig<'_>,
    source_root: &Path,
    program: &ProgramIdentity,
    scratch: &Scratch,
) -> Result<SandboxPolicy, String> {
    if config.policy.mode() == polaris_sandbox::SandboxMode::FullAccess
        || !config.policy.contains(source_root)
    {
        return Err("caller policy cannot be narrowed to a confined write root".into());
    }
    if !network_is_enforced(config.policy) {
        return Err("the caller policy cannot enforce the runner's no-network boundary".into());
    }
    let runtime_roots = runtime_roots()?;
    if let Some(boundary) = config.policy.isolated_boundary()
        && runtime_roots.iter().any(|root| {
            !boundary
                .readable_roots
                .iter()
                .any(|allowed| root.starts_with(allowed))
        })
    {
        return Err("caller policy does not permit the required system runtime roots".into());
    }
    if !program.path.starts_with(source_root)
        && !runtime_roots
            .iter()
            .any(|root| program.path.starts_with(root))
    {
        return Err(
            "program must be inside the source root or an approved system executable root".into(),
        );
    }
    let mut header = [0_u8; 2];
    if std::io::Read::read_exact(
        &mut std::fs::File::open(&program.path)
            .map_err(|error| format!("program is unreadable: {error}"))?,
        &mut header,
    )
    .is_ok()
        && header == *b"#!"
    {
        return Err("script and interpreter entrypoints are not accepted".into());
    }
    let policy = SandboxPolicy::isolated_registered(
        polaris_sandbox::SandboxMode::WorkspaceWrite,
        &scratch.workspace,
        &runtime_roots,
    )
    .map_err(|error| format!("isolated sandbox is unavailable: {error}"))?
    .with_isolated_private_environment(&scratch.root, &scratch.home, &scratch.tmpdir)
    .map_err(|error| format!("private scratch is unavailable: {error}"))?
    .require_process_containment()
    .map_err(|error| format!("process containment is unavailable: {error}"))?;
    Ok(policy)
}

fn runtime_roots() -> Result<Vec<PathBuf>, String> {
    ["/usr/bin", "/bin", "/usr/lib", "/System/Library"]
        .into_iter()
        .filter(|path| Path::new(path).is_dir())
        .map(PathBuf::from)
        .map(|path| {
            path.canonicalize()
                .map_err(|error| format!("runtime root is unavailable: {error}"))
        })
        .collect()
}

/// Execute a checked plan. An unavailable sandbox is reported as unavailable;
/// this function never retries with an unconfined process.
pub fn run(plan: &Plan, config: RunConfig<'_>) -> RunOutcome {
    if let Err(error) = plan.check() {
        return RunOutcome::clean(RunReport::Failed {
            identity: None,
            cases: vec![invalid_report(error)],
            fallback_required: false,
        });
    }
    let plan_hash = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(plan).expect("checked plan serializes"),)
    );
    let (_cwd, write_root) = match boundary(&config) {
        Ok(boundary) => boundary,
        Err(reason) => {
            return RunOutcome::clean(RunReport::Unavailable {
                identity: None,
                reason,
                fallback_required: true,
            });
        }
    };
    let source_program = match program_identity(Path::new(&plan.argv[0])) {
        Ok(program) => program,
        Err(reason) => {
            return RunOutcome::clean(RunReport::Unavailable {
                identity: None,
                reason,
                fallback_required: true,
            });
        }
    };
    if !admitted_program_source(&source_program.path, &write_root) {
        return unavailable(
            "program source is outside the admitted roots or has a sensitive path".into(),
        );
    }
    let scratch = match Scratch::new() {
        Ok(scratch) => scratch,
        Err(reason) => return unavailable(reason),
    };
    let policy = match confined_policy(&config, &write_root, &source_program, &scratch) {
        Ok(policy) => policy,
        Err(reason) => return unavailable(reason),
    };
    let program = match scratch.snapshot_program(&source_program.path) {
        Ok(program) => program,
        Err(reason) => return unavailable(reason),
    };
    let identity = match run_identity(plan_hash, &program.path) {
        Ok(identity) => identity,
        Err(reason) => return unavailable(reason),
    };
    let deadline = Instant::now() + config.timeout;
    let mut reports = Vec::new();
    for case in &plan.cases {
        if let Case::RejectsInput {
            argv,
            stdin,
            observed_paths,
        } = case
        {
            let before = observe_paths(&scratch.workspace, observed_paths);
            let invocation = match invoke(
                &program,
                argv,
                stdin.as_deref(),
                &policy,
                &scratch.workspace,
                deadline,
            ) {
                Ok(value) => value,
                Err(reason) => {
                    return RunOutcome::clean(RunReport::Unavailable {
                        identity: Some(identity),
                        reason,
                        fallback_required: true,
                    });
                }
            };
            let after = observe_paths(&scratch.workspace, observed_paths);
            let clean = invocation.clean
                && invocation.status.is_some_and(|status| status != 0)
                && before
                    .iter()
                    .chain(&after)
                    .all(|path| path.state == ObservedPathState::Absent);
            let detail = if clean {
                "input rejected".into()
            } else {
                format!(
                    "{}; rejected input must exit non-zero and leave observed paths absent",
                    invocation.detail
                )
            };
            reports.push(report("rejects_input", clean, detail, &invocation, after));
            if let Some(pending) = invocation.pending {
                return failed_with_cleanup(reports, scratch, identity, pending);
            }
        }
    }
    let base = match invoke(
        &program,
        &plan.argv[1..],
        plan.stdin.as_deref(),
        &policy,
        &scratch.workspace,
        deadline,
    ) {
        Ok(value) => value,
        Err(reason) => {
            return RunOutcome::clean(RunReport::Unavailable {
                identity: Some(identity),
                reason,
                fallback_required: true,
            });
        }
    };
    if base.pending.is_some() {
        reports.push(report(
            "execution",
            false,
            "base invocation has pending cleanup; no later case or repeat was spawned".into(),
            &base,
            Vec::new(),
        ));
        let pending = match base.pending {
            Some(pending) => pending,
            None => unreachable!("pending cleanup was checked above"),
        };
        return failed_with_cleanup(reports, scratch, identity, pending);
    }
    for case in &plan.cases {
        let item = match case {
            Case::RejectsInput { .. } => continue,
            Case::ExitsWith { status } => report(
                "exits_with",
                base.clean && base.status == Some(*status),
                format!("expected status {status}; {}", base.detail),
                &base,
                Vec::new(),
            ),
            Case::Says {
                stream,
                contains,
                absent,
            } => {
                let output = match stream {
                    Stream::Stdout => &base.stdout,
                    Stream::Stderr => &base.stderr,
                };
                let passed = base.clean
                    && contains.iter().all(|needle| output.contains(needle))
                    && absent.iter().all(|needle| !output.contains(needle));
                report("says", passed, base.detail.clone(), &base, Vec::new())
            }
            Case::Creates { paths } => {
                let paths = observe_paths(&scratch.workspace, paths);
                report(
                    "creates",
                    base.clean && paths.iter().all(|path| path.exists),
                    base.detail.clone(),
                    &base,
                    paths,
                )
            }
            Case::StructuredOutput { pointers } => {
                let parsed = serde_json::from_str::<Value>(&base.stdout);
                let passed = base.clean
                    && parsed.as_ref().is_ok_and(|json| {
                        pointers.iter().all(|item| {
                            json.pointer(&item.pointer).is_some_and(|actual| {
                                item.expected
                                    .as_ref()
                                    .is_none_or(|expected| actual == expected)
                            })
                        })
                    });
                report(
                    "structured_output",
                    passed,
                    base.detail.clone(),
                    &base,
                    Vec::new(),
                )
            }
            Case::RepeatsCleanly { observed_paths } => {
                let first_paths = observe_paths(&scratch.workspace, observed_paths);
                let second = match invoke(
                    &program,
                    &plan.argv[1..],
                    plan.stdin.as_deref(),
                    &policy,
                    &scratch.workspace,
                    deadline,
                ) {
                    Ok(value) => value,
                    Err(reason) => {
                        return RunOutcome::clean(RunReport::Unavailable {
                            identity: Some(identity),
                            reason,
                            fallback_required: true,
                        });
                    }
                };
                let paths = observe_paths(&scratch.workspace, observed_paths);
                let passed = base.clean
                    && second.clean
                    && base.status == second.status
                    && paths.iter().all(|path| path.exists)
                    && first_paths == paths;
                let item = report(
                    "repeats_cleanly",
                    passed,
                    second.detail.clone(),
                    &second,
                    paths,
                );
                if let Some(pending) = second.pending {
                    reports.push(item);
                    return failed_with_cleanup(reports, scratch, identity, pending);
                }
                item
            }
        };
        reports.push(item);
    }
    if reports.iter().any(|case| case.timed_out) {
        reports.push(deadline_report());
        return RunOutcome::clean(RunReport::Failed {
            identity: Some(identity),
            cases: reports,
            fallback_required: false,
        });
    }
    if reports.iter().all(|case| case.passed) {
        RunOutcome::clean(RunReport::Passed {
            identity,
            cases: reports,
            fallback_required: false,
        })
    } else {
        RunOutcome::clean(RunReport::Failed {
            identity: Some(identity),
            cases: reports,
            fallback_required: false,
        })
    }
}

fn failed_with_cleanup(
    cases: Vec<CaseReport>,
    mut scratch: Scratch,
    identity: RunIdentityV1,
    pending: PendingCleanup,
) -> RunOutcome {
    scratch.preserve = true;
    let cleanup = OwnedScratchCleanup {
        pending,
        root: Some(scratch.root.clone()),
        workspace: Some(scratch.workspace.clone()),
        child_reaped: false,
    };
    RunOutcome::pending(
        RunReport::Failed {
            identity: Some(identity),
            cases,
            fallback_required: false,
        },
        cleanup,
    )
}

fn boundary(config: &RunConfig<'_>) -> Result<(PathBuf, PathBuf), String> {
    if config.timeout.is_zero() {
        return Err("timeout must be non-zero".into());
    }
    let cwd = config
        .cwd
        .canonicalize()
        .map_err(|error| format!("cwd is unavailable: {error}"))?;
    let write_root = config
        .write_root
        .canonicalize()
        .map_err(|error| format!("write root is unavailable: {error}"))?;
    if !cwd.starts_with(&write_root) {
        return Err("cwd must be inside the caller-owned write root".into());
    }
    if !config.policy.contains(&write_root) {
        return Err("policy does not enforce writes to the supplied write root".into());
    }
    Ok((cwd, write_root))
}
fn invoke(
    program: &ProgramIdentity,
    args: &[String],
    stdin: Option<&str>,
    policy: &SandboxPolicy,
    cwd: &Path,
    deadline: Instant,
) -> Result<Invocation, String> {
    if Instant::now() >= deadline {
        return Ok(Invocation::timed_out());
    }
    if program_identity(&program.path)? != *program {
        return Err("program identity changed before launch".into());
    }
    let outcome =
        run_confined_controlled_in_dir(policy, &program.path, args, stdin, Some(cwd), || {
            Instant::now() >= deadline
        })
        .map_err(|error| match error {
            SandboxError::NotEnforced(reason) => reason,
            SandboxError::UnsupportedPlatform => {
                "this platform has no confined controlled runner".into()
            }
            SandboxError::Denied { path, policy } => format!("sandbox denied {path} ({policy})"),
            SandboxError::Io(error) => format!("launch failed: {error}"),
        })?;
    let timed_out = outcome.end == ControlledEnd::Cancelled && Instant::now() >= deadline;
    Ok(classify_outcome(outcome, timed_out))
}

fn classify_outcome(
    mut outcome: polaris_sandbox::ControlledOutcome,
    timed_out: bool,
) -> Invocation {
    // Keep this exact native owner in the common-runner outcome.  Do not drop
    // it into the process-global registry during the normal CLI path.
    let pending = outcome.pending.take();
    let clean = outcome.end == ControlledEnd::Exited
        && outcome.status.is_some()
        && outcome.problem.is_none()
        && !outcome.stdout_truncated
        && !outcome.stderr_truncated
        && outcome.stdout_eof
        && outcome.stderr_eof
        && pending.is_none();
    let detail = if pending.is_some() {
        "cleanup is pending".into()
    } else if timed_out {
        "timed out".into()
    } else if let Some(problem) = outcome.problem {
        problem
    } else if !clean {
        format!("controlled execution ended as {:?}", outcome.end)
    } else {
        "completed".into()
    };
    Invocation {
        status: outcome.status.map(|status| status.code().unwrap_or(-1)),
        stdout: truncate_record(outcome.stdout),
        stderr: truncate_record(outcome.stderr),
        truncated: outcome.stdout_truncated
            || outcome.stderr_truncated
            || !outcome.stdout_eof
            || !outcome.stderr_eof,
        timed_out,
        clean,
        detail,
        pending,
    }
}
fn truncate_record(mut text: String) -> String {
    if text.len() > MAX_OUTPUT_RECORD_BYTES {
        let mut end = MAX_OUTPUT_RECORD_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}
fn observe_paths(write_root: &Path, paths: &[String]) -> Vec<ObservedPath> {
    paths
        .iter()
        .map(|path| {
            let candidate = write_root.join(path);
            let (state, fingerprint) = match candidate.symlink_metadata() {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    (ObservedPathState::Absent, None)
                }
                Err(_) => (ObservedPathState::Unreadable, None),
                Ok(metadata)
                    if !metadata.is_file()
                        || metadata.file_type().is_symlink()
                        || metadata.len() > MAX_OBSERVED_FILE_BYTES =>
                {
                    (ObservedPathState::Unsafe, None)
                }
                Ok(_) => match candidate.canonicalize() {
                    Err(_) => (ObservedPathState::Unreadable, None),
                    Ok(actual) if !actual.starts_with(write_root) => {
                        (ObservedPathState::Unsafe, None)
                    }
                    Ok(_) => match std::fs::read(&candidate) {
                        Ok(bytes) => (
                            ObservedPathState::File,
                            Some(format!("{:x}", Sha256::digest(bytes))),
                        ),
                        Err(_) => (ObservedPathState::Unreadable, None),
                    },
                },
            };
            let exists = state == ObservedPathState::File;
            ObservedPath {
                path: path.clone(),
                exists,
                state,
                fingerprint,
            }
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn network_is_enforced(policy: &SandboxPolicy) -> bool {
    policy.mode() != polaris_sandbox::SandboxMode::FullAccess
        && !policy
            .isolated_boundary()
            .is_some_and(|boundary| boundary.loopback_network)
}
#[cfg(not(target_os = "macos"))]
fn network_is_enforced(_policy: &SandboxPolicy) -> bool {
    false
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProgramIdentity {
    path: PathBuf,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

fn program_identity(program: &Path) -> Result<ProgramIdentity, String> {
    let path = program
        .canonicalize()
        .map_err(|error| format!("program is unavailable: {error}"))?;
    let metadata = path
        .metadata()
        .map_err(|error| format!("program is unreadable: {error}"))?;
    if !metadata.is_file() {
        return Err("program is not a regular file".into());
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if ["sh", "bash", "zsh", "dash", "ksh", "fish", "env"].contains(&name) {
        return Err("shell and interpreter entrypoints are not accepted".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(ProgramIdentity {
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(not(unix))]
    Ok(ProgramIdentity { path })
}

fn run_identity(plan_hash: String, program: &Path) -> Result<RunIdentityV1, String> {
    let bytes = std::fs::read(program)
        .map_err(|error| format!("program snapshot is unreadable: {error}"))?;
    if bytes.len() as u64 > MAX_PROGRAM_BYTES {
        return Err("program snapshot exceeds the runner size limit".into());
    }
    Ok(RunIdentityV1 {
        plan_hash,
        program_hash: format!("{:x}", Sha256::digest(bytes)),
    })
}

/// The runner may copy one executable from the caller's project, but never
/// admits hidden state, credentials, or an arbitrary home-directory binary as
/// that source.  The confined child receives only the resulting copy.
fn admitted_program_source(program: &Path, source_root: &Path) -> bool {
    if program.starts_with(source_root) {
        return program
            .strip_prefix(source_root)
            .ok()
            .into_iter()
            .flat_map(Path::components)
            .filter_map(|component| component.as_os_str().to_str())
            .all(|component| {
                let lowered = component.to_ascii_lowercase();
                !matches!(
                    lowered.as_str(),
                    ".git" | ".polaris" | ".env" | "credentials"
                ) && !lowered.starts_with('.')
                    && !lowered.contains("secret")
                    && !lowered.contains("token")
                    && !lowered.ends_with(".key")
            });
    }
    runtime_roots()
        .ok()
        .is_some_and(|roots| roots.iter().any(|root| program.starts_with(root)))
}
fn report(
    case: &'static str,
    passed: bool,
    detail: String,
    invocation: &Invocation,
    observed_paths: Vec<ObservedPath>,
) -> CaseReport {
    CaseReport {
        case,
        passed,
        detail,
        status: invocation.status,
        stdout: invocation.stdout.clone(),
        stderr: invocation.stderr.clone(),
        output_truncated: invocation.truncated,
        timed_out: invocation.timed_out,
        observed_paths,
    }
}
fn invalid_report(error: PlanError) -> CaseReport {
    CaseReport {
        case: "plan",
        passed: false,
        detail: format!("invalid plan: {error:?}"),
        status: None,
        stdout: String::new(),
        stderr: String::new(),
        output_truncated: false,
        timed_out: false,
        observed_paths: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_sandbox::SandboxMode;
    fn plan(cases: Vec<Case>) -> Plan {
        Plan {
            version: SCHEMA_VERSION,
            argv: vec!["fixture".into()],
            stdin: None,
            cases,
        }
    }
    #[test]
    fn strict_v1_schema_rejects_unknown_versions_and_unbounded_contracts() {
        assert!(matches!(
            Plan {
                version: 2,
                ..plan(vec![Case::ExitsWith { status: 0 }])
            }
            .check(),
            Err(PlanError::UnsupportedVersion(2))
        ));
        assert!(matches!(
            plan(vec![Case::RepeatsCleanly {
                observed_paths: vec![]
            }])
            .check(),
            Err(PlanError::EmptyContract(_))
        ));
        let json =
            r#"{"version":1,"argv":["x"],"cases":[{"case":"exits_with","status":0,"extra":true}]}"#;
        assert!(serde_json::from_str::<Plan>(json).is_err());
        let mut expected = Value::Null;
        for _ in 0..=MAX_EXPECTED_DEPTH {
            expected = Value::Array(vec![expected]);
        }
        assert!(matches!(
            plan(vec![Case::StructuredOutput {
                pointers: vec![PointerExpectation {
                    pointer: "/value".into(),
                    expected: Some(expected),
                }]
            }])
            .check(),
            Err(PlanError::OversizedExpectedValue)
        ));
    }

    #[test]
    fn legacy_v1_observation_fields_decode_then_fail_closed_when_absent() {
        let rejects = r#"{"version":1,"argv":["x"],"cases":[{"case":"rejects_input","argv":["bad"],"stdin":null}]}"#;
        let repeats = r#"{"version":1,"argv":["x"],"cases":[{"case":"repeats_cleanly"}]}"#;
        let rejects: Plan = serde_json::from_str(rejects).expect("legacy rejects_input decodes");
        let repeats: Plan = serde_json::from_str(repeats).expect("legacy repeats_cleanly decodes");
        assert!(matches!(
            rejects.check(),
            Err(PlanError::EmptyContract("rejects_input.observed_paths"))
        ));
        assert!(matches!(
            repeats.check(),
            Err(PlanError::EmptyContract("repeats_cleanly.observed_paths"))
        ));
    }

    #[test]
    fn serialized_plan_limit_accepts_the_largest_constructed_fixture_only() {
        let mut plan = plan(vec![Case::Says {
            stream: Stream::Stdout,
            contains: (0..5).map(|_| "x".repeat(12_000)).collect(),
            absent: Vec::new(),
        }]);
        let mut slot = 0;
        loop {
            let full = match &plan.cases[0] {
                Case::Says { contains, .. } => contains[slot].len() == MAX_TEXT_BYTES,
                _ => unreachable!(),
            };
            if full {
                break;
            }
            let contains_len = match &mut plan.cases[0] {
                Case::Says { contains, .. } => {
                    contains[slot].push('x');
                    contains.len()
                }
                _ => unreachable!(),
            };
            if serde_json::to_vec(&plan).unwrap().len() > MAX_PLAN_BYTES {
                let Case::Says { contains, .. } = &mut plan.cases[0] else {
                    unreachable!()
                };
                contains[slot].pop();
                break;
            }
            slot = (slot + 1) % contains_len;
        }
        assert!(serde_json::to_vec(&plan).unwrap().len() <= MAX_PLAN_BYTES);
        assert!(plan.check().is_ok());
        let mut oversized = plan.clone();
        let Case::Says { contains, .. } = &mut oversized.cases[0] else {
            unreachable!()
        };
        contains[0].push('x');
        assert!(matches!(oversized.check(), Err(PlanError::OversizedPlan)));
    }

    #[test]
    fn truncated_or_timed_out_controlled_output_never_counts_as_clean() {
        let outcome = |end, stdout_truncated| polaris_sandbox::ControlledOutcome {
            end,
            status: None,
            stdout: String::new(),
            stderr: String::new(),
            stdout_truncated,
            stderr_truncated: false,
            stdout_eof: true,
            stderr_eof: true,
            problem: None,
            pending: None,
        };
        let truncated = classify_outcome(outcome(ControlledEnd::Exited, true), false);
        assert!(!truncated.clean && truncated.truncated);
        let timed_out = classify_outcome(outcome(ControlledEnd::Cancelled, false), true);
        assert!(!timed_out.clean && timed_out.timed_out);
    }

    #[test]
    fn evidence_hash_is_stable_and_excludes_command_output() {
        let identity = RunIdentityV1 {
            plan_hash: "a".repeat(64),
            program_hash: "b".repeat(64),
        };
        let report = |stdout: &str| RunReport::Failed {
            identity: Some(identity.clone()),
            cases: vec![CaseReport {
                case: "says",
                passed: false,
                detail: "bounded failure".into(),
                status: Some(1),
                stdout: stdout.into(),
                stderr: "also excluded".into(),
                output_truncated: false,
                timed_out: false,
                observed_paths: Vec::new(),
            }],
            fallback_required: false,
        };
        assert_eq!(
            report("secret-a").bounded_evidence_hash(),
            report("secret-b").bounded_evidence_hash()
        );
        let encoded = serde_json::to_string(&report("secret-a")).unwrap();
        assert!(encoded.contains("plan_hash") && encoded.contains("program_hash"));
    }

    #[test]
    fn observed_paths_do_not_treat_symlinks_or_oversize_files_as_absent() {
        let root = tempfile::tempdir().unwrap();
        let oversized = root.path().join("oversized");
        std::fs::write(&oversized, vec![0_u8; MAX_OBSERVED_FILE_BYTES as usize + 1]).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&oversized, root.path().join("link")).unwrap();
        let paths = observe_paths(
            root.path(),
            &["oversized".into(), "link".into(), "missing".into()],
        );
        assert_eq!(paths[0].state, ObservedPathState::Unsafe);
        assert_eq!(paths[1].state, ObservedPathState::Unsafe);
        assert_eq!(paths[2].state, ObservedPathState::Absent);
    }

    #[test]
    fn program_identity_rejects_replacement_and_full_access_network() {
        let root = tempfile::tempdir().unwrap();
        let program = root.path().join("program");
        std::fs::write(&program, "first").unwrap();
        let first = program_identity(&program).unwrap();
        let replacement = root.path().join("replacement");
        std::fs::write(&replacement, "second").unwrap();
        std::fs::rename(&replacement, &program).unwrap();
        assert_ne!(program_identity(&program).unwrap(), first);
        let full_access = SandboxPolicy::new(SandboxMode::FullAccess, &[]).unwrap();
        assert!(!network_is_enforced(&full_access));
        let shell = root.path().join("sh");
        std::fs::write(&shell, "binary-looking fixture").unwrap();
        assert!(program_identity(&shell).is_err());
    }

    #[test]
    fn program_snapshot_refuses_project_secret_paths() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let admitted = target.join("runner");
        std::fs::write(&admitted, "not executed").unwrap();
        let secret = root.path().join(".polaris");
        std::fs::create_dir(&secret).unwrap();
        let secret_program = secret.join("token-helper");
        std::fs::write(&secret_program, "not executed").unwrap();
        assert!(admitted_program_source(&admitted, root.path()));
        assert!(!admitted_program_source(&secret_program, root.path()));
    }

    #[test]
    fn admitted_program_is_copied_into_the_runner_workspace() {
        let scratch = Scratch::new().expect("private runner workspace");
        let source = program_identity(Path::new("/usr/bin/true")).expect("system executable");
        let snapshot = scratch
            .snapshot_program(&source.path)
            .expect("program snapshot");
        assert!(snapshot.path.starts_with(&scratch.workspace));
        assert_ne!(snapshot.path, source.path);
    }

    #[test]
    fn runner_executes_or_reports_a_required_sandbox_fallback() {
        let root = tempfile::tempdir().unwrap();
        let plan = Plan {
            version: SCHEMA_VERSION,
            argv: vec!["/usr/bin/true".into()],
            stdin: None,
            cases: vec![Case::ExitsWith { status: 0 }],
        };
        let policy =
            SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.path().to_path_buf()]).unwrap();
        let result = run(
            &plan,
            RunConfig {
                policy: &policy,
                cwd: root.path(),
                write_root: root.path(),
                timeout: Duration::from_secs(5),
            },
        );
        match &result.report {
            RunReport::Passed { .. } => {}
            RunReport::Unavailable {
                fallback_required: true,
                ..
            } => {}
            other => panic!("runner did not produce a safe result: {other:?}"),
        }
    }
}
