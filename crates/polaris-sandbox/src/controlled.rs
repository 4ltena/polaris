//! Bounded native process ownership, not a secret-file or inherited-FD sandbox.
//! Call on an execution owner registered independently of the agent Future.
//! The caller must not install another reaper for these children. Process
//! groups do not contain descendants that deliberately escape with setsid.

use crate::{SandboxError, SandboxPolicy};
use std::path::Path;
use std::process::ExitStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlledEnd {
    Exited,
    CancelledBeforeSpawn,
    Cancelled,
    StopUnconfirmed,
}

#[derive(Debug)]
#[must_use = "retain pending cleanup in the run owner before reporting shutdown readiness"]
pub struct ControlledOutcome {
    pub end: ControlledEnd,
    pub status: Option<ExitStatus>,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub stdout_eof: bool,
    pub stderr_eof: bool,
    pub problem: Option<String>,
    pub pending: Option<PendingCleanup>,
}

/// `cancelled` must be fast and nonblocking. Cancellation is latched. A panic
/// unwinds into the cleanup registry, not into an unowned Child. No guarantees
/// are possible for process abort or a callback that never returns.
pub fn run_confined_controlled(
    policy: &SandboxPolicy,
    program: &Path,
    args: &[String],
    stdin: Option<&str>,
    cancelled: impl FnMut() -> bool,
) -> Result<ControlledOutcome, SandboxError> {
    run_confined_controlled_authorized(policy, program, args, stdin, cancelled, || true)
}

/// `authorized` must be fast and nonblocking. It is checked once immediately
/// before native spawn, after command preparation. Expiry after that check does
/// not cancel a running child; `cancelled` remains the ongoing stop signal.
pub fn run_confined_controlled_authorized(
    policy: &SandboxPolicy,
    program: &Path,
    args: &[String],
    stdin: Option<&str>,
    cancelled: impl FnMut() -> bool,
    authorized: impl FnMut() -> bool,
) -> Result<ControlledOutcome, SandboxError> {
    // v1 has no cancellation/ownership protocol: reject before connecting or
    // transmitting a command, never fall back from an opted-in broker.
    if crate::broker::from_environment()?.is_some() {
        return Err(SandboxError::NotEnforced(
            "broker v1 does not support controlled execution".into(),
        ));
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        native::run_authorized(
            policy,
            program,
            args,
            stdin,
            cancelled,
            authorized,
            native::Timing::default(),
        )
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (policy, program, args, stdin, cancelled, authorized);
        Err(SandboxError::UnsupportedPlatform)
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub use native::{PendingCleanup, take_pending_cleanups};

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
#[derive(Debug)]
pub struct PendingCleanup;
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
impl PendingCleanup {
    pub fn child_id(&self) -> Option<u32> {
        None
    }
    pub fn try_reap(&mut self) -> std::io::Result<Option<ExitStatus>> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "controlled execution is unsupported",
        ))
    }
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn take_pending_cleanups() -> Vec<PendingCleanup> {
    Vec::new()
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod native {
    use super::*;
    use std::io::{self, Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Stdio};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    const LIMIT: usize = 256 * 1024;
    const QUANTUM: usize = 64 * 1024;
    const TICK: Duration = Duration::from_millis(20);
    pub(super) struct Timing {
        term: Duration,
        kill: Duration,
        drain: Duration,
        defer_reap: bool,
    }
    impl Default for Timing {
        fn default() -> Self {
            Self {
                term: Duration::from_millis(500),
                kill: Duration::from_secs(2),
                drain: Duration::from_millis(200),
                defer_reap: false,
            }
        }
    }

    static PENDING: Mutex<Vec<Owned>> = Mutex::new(Vec::new());
    const MAX_OWNERS: usize = 32;
    static RESERVED: AtomicUsize = AtomicUsize::new(0);
    #[derive(Debug)]
    struct Reservation;
    impl Reservation {
        fn acquire() -> Result<Self, SandboxError> {
            RESERVED.fetch_update(Ordering::AcqRel, Ordering::Acquire,
                |n| (n < MAX_OWNERS).then_some(n + 1))
                .map(|_| Self)
                .map_err(|_| SandboxError::NotEnforced("controlled execution owner capacity is exhausted; reap pending children first".into()))
        }
    }
    impl Drop for Reservation {
        fn drop(&mut self) {
            RESERVED.fetch_sub(1, Ordering::AcqRel);
        }
    }
    #[derive(Debug)]
    struct Owned {
        _reservation: Reservation,
        child: Child,
        pgid: libc::pid_t,
        status: Option<ExitStatus>,
        may_signal: bool,
    }

    /// Non-cloneable ownership of an unreaped child or unconfirmed group.
    /// Drop parks it in `take_pending_cleanups`; it never blocks on wait.
    #[derive(Debug)]
    #[must_use = "poll or transfer this handle; dropped handles remain in the cleanup registry"]
    pub struct PendingCleanup {
        owned: Option<Owned>,
    }

    impl PendingCleanup {
        pub fn child_id(&self) -> Option<u32> {
            self.owned.as_ref().map(|owned| owned.child.id())
        }
        /// Reaps only the owned direct child; never signals a recycled PGID.
        /// Some means the child was reaped AND group absence was observed.
        /// Escaped descendants are outside this confirmation. Stop polling once
        /// Some is returned; the handle is then empty.
        pub fn try_reap(&mut self) -> io::Result<Option<ExitStatus>> {
            let Some(owned) = self.owned.as_mut() else {
                return Ok(None);
            };
            owned.may_signal = false;
            if owned.status.is_none() {
                owned.status = owned.child.try_wait()?;
            }
            if owned.status.is_some() && group_absent(owned.pgid)? {
                let status = owned.status;
                self.owned = None;
                return Ok(status);
            }
            Ok(None)
        }
    }
    impl Drop for PendingCleanup {
        fn drop(&mut self) {
            if let Some(mut owned) = self.owned.take() {
                // Before reap, the child pins this PID. After final signal or
                // reap, never send another group signal based on its number.
                if owned.may_signal && owned.status.is_none() {
                    let _ = signal(owned.pgid, libc::SIGKILL);
                }
                owned.may_signal = false;
                PENDING
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(owned);
            }
        }
    }
    /// Drain this registry into the service's independent run owner. A nonempty
    /// registry prevents shutdown.ready. Dropping a returned handle re-parks it.
    pub fn take_pending_cleanups() -> Vec<PendingCleanup> {
        std::mem::take(&mut *PENDING.lock().unwrap_or_else(|e| e.into_inner()))
            .into_iter()
            .map(|owned| PendingCleanup { owned: Some(owned) })
            .collect()
    }

    fn signal(pgid: libc::pid_t, sig: libc::c_int) -> io::Result<()> {
        assert!(pgid > 1);
        // SAFETY: negative, validated PID selects only our spawned group.
        if unsafe { libc::kill(-pgid, sig) } == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(e)
        }
    }
    fn group_absent(pgid: libc::pid_t) -> io::Result<bool> {
        // Observation only, never a post-reap signal with side effects.
        if unsafe { libc::kill(-pgid, 0) } == 0 {
            return Ok(false);
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::ESRCH) => Ok(true),
            Some(libc::EPERM) => Ok(false),
            _ => Err(e),
        }
    }
    fn exited_without_reap(owned: &mut Owned) -> io::Result<bool> {
        // SAFETY: initialized OS-provided layout; WNOWAIT preserves PID ownership.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                owned.child.id() as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if rc == 0 {
            return Ok(unsafe { info.si_pid() } != 0);
        }
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::ECHILD) {
            owned.may_signal = false;
        }
        if e.kind() == io::ErrorKind::Interrupted {
            Ok(false)
        } else {
            Err(e)
        }
    }
    fn nonblocking(fd: &impl AsRawFd) -> io::Result<()> {
        // SAFETY: owned, open pipe FD; preserve all existing status flags.
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        if flags < 0
            || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[derive(Default)]
    struct Capture {
        bytes: Vec<u8>,
        truncated: bool,
        eof: bool,
    }
    impl Capture {
        fn text(&mut self) -> String {
            let mut text = String::from_utf8_lossy(&self.bytes).into_owned();
            if text.len() > LIMIT {
                let mut end = LIMIT;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
                // Do not retain the replacement-expanded allocation either.
                text.shrink_to_fit();
                self.truncated = true;
            }
            text
        }
        fn drain(&mut self, stream: &mut Option<impl Read>) -> io::Result<()> {
            let Some(reader) = stream.as_mut() else {
                return Ok(());
            };
            let mut buf = [0; 8192];
            let mut count = 0;
            while count < QUANTUM {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        self.eof = true;
                        *stream = None;
                        break;
                    }
                    Ok(n) => {
                        count += n;
                        let keep = n.min(LIMIT - self.bytes.len());
                        self.bytes.extend_from_slice(&buf[..keep]);
                        self.truncated |= keep != n;
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => break,
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        }
    }

    #[cfg(test)]
    fn run(
        policy: &SandboxPolicy,
        program: &Path,
        args: &[String],
        stdin: Option<&str>,
        cancelled: impl FnMut() -> bool,
        timing: Timing,
    ) -> Result<ControlledOutcome, SandboxError> {
        run_authorized(policy, program, args, stdin, cancelled, || true, timing)
    }

    pub(super) fn run_authorized(
        policy: &SandboxPolicy,
        program: &Path,
        args: &[String],
        stdin: Option<&str>,
        mut cancelled: impl FnMut() -> bool,
        mut authorized: impl FnMut() -> bool,
        timing: Timing,
    ) -> Result<ControlledOutcome, SandboxError> {
        let empty = || ControlledOutcome {
            end: ControlledEnd::CancelledBeforeSpawn,
            status: None,
            stdout: String::new(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            stdout_eof: true,
            stderr_eof: true,
            problem: None,
            pending: None,
        };
        if cancelled() {
            return Ok(empty());
        }
        // Includes active children AND returned/parked cleanup handles. Moving
        // a handle never releases capacity; only completed cleanup does.
        let reservation = Reservation::acquire()?;
        let mut command = crate::confine::build_command(policy, program, args)?;
        command
            .process_group(0)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if cancelled() {
            return Ok(empty());
        }
        if !authorized() {
            return Ok(empty());
        }
        let child = command
            .spawn()
            .map_err(crate::confine::classify_spawn_error)?;
        let pgid = libc::pid_t::try_from(child.id()).expect("OS child PID fits pid_t");
        let mut owner = PendingCleanup {
            owned: Some(Owned {
                _reservation: reservation,
                child,
                pgid,
                status: None,
                may_signal: true,
            }),
        };
        let owned = owner.owned.as_mut().expect("owner");
        let mut input = owned.child.stdin.take();
        let mut output = owned.child.stdout.take();
        let mut error = owned.child.stderr.take();
        let mut problem = None;
        let mut stop_problem = None;
        for fd in [
            input.as_ref().map(AsRawFd::as_raw_fd),
            output.as_ref().map(AsRawFd::as_raw_fd),
            error.as_ref().map(AsRawFd::as_raw_fd),
        ]
        .into_iter()
        .flatten()
        {
            // BorrowedFd does not take ownership of the pipe.
            let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
            if let Err(e) = nonblocking(&fd) {
                problem = Some(e.to_string());
            }
        }
        if problem.is_some() {
            input = None;
            output = None;
            error = None;
        }
        let mut out = Capture::default();
        let mut err = Capture::default();
        let mut offset: usize = 0;
        let payload = stdin.unwrap_or("").as_bytes();
        let mut was_cancelled = false;
        let mut exited = false;
        let mut term_at = None;
        let mut kill_at = None;
        let mut drain_until = None;
        loop {
            let now = Instant::now();
            let owned = owner.owned.as_mut().expect("owner");
            if !exited && owned.status.is_none() {
                match exited_without_reap(owned) {
                    Ok(true) => {
                        exited = true;
                        drain_until = Some(now + timing.drain);
                    }
                    Ok(false) => {}
                    Err(e) => {
                        problem.get_or_insert(e.to_string());
                    }
                }
            }
            if !exited && !was_cancelled {
                was_cancelled = cancelled();
            }
            if (exited || was_cancelled || problem.is_some()) && term_at.is_none() {
                input = None;
                if owned.may_signal
                    && let Err(e) = signal(pgid, libc::SIGTERM)
                {
                    stop_problem.get_or_insert(e.to_string());
                }
                term_at = Some(now);
            }
            if term_at.is_some_and(|t| now.duration_since(t) >= timing.term) && kill_at.is_none() {
                if owned.may_signal
                    && let Err(e) = signal(pgid, libc::SIGKILL)
                {
                    stop_problem.get_or_insert(e.to_string());
                }
                owned.may_signal = false;
                kill_at = Some(now);
                drain_until.get_or_insert(now + timing.drain);
            }
            if let Err(e) = out.drain(&mut output) {
                problem.get_or_insert(e.to_string());
                output = None;
            }
            if let Err(e) = err.drain(&mut error) {
                problem.get_or_insert(e.to_string());
                error = None;
            }
            if let Some(writer) = input.as_mut() {
                let end = payload.len().min(offset.saturating_add(QUANTUM));
                match writer.write(&payload[offset..end]) {
                    Ok(n) => {
                        offset += n;
                        if offset == payload.len() {
                            input = None;
                        }
                    }
                    Err(e)
                        if e.kind() == io::ErrorKind::WouldBlock
                            || e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
                        input = None;
                    }
                    Err(e) => {
                        problem.get_or_insert(e.to_string());
                        input = None;
                    }
                }
            }
            if drain_until.is_some_and(|until| now >= until) {
                output = None;
                error = None;
            }
            if let Some(killed) = kill_at {
                if !timing.defer_reap && owned.status.is_none() {
                    match owned.child.try_wait() {
                        Ok(status) => owned.status = status,
                        Err(e) => {
                            problem.get_or_insert(e.to_string());
                        }
                    }
                }
                let gone = match group_absent(pgid) {
                    Ok(gone) => gone,
                    Err(e) => {
                        problem.get_or_insert(e.to_string());
                        false
                    }
                };
                if owned.status.is_some() && gone && output.is_none() && error.is_none() {
                    break;
                }
                if now.duration_since(killed) >= timing.kill {
                    break;
                }
            }
            // Small OS pipes can yield only 512 bytes before WouldBlock.
            // Wake when the peer makes progress instead of charging a full
            // idle tick per pipeful. Per-pass I/O and cancellation stay bounded.
            let mut fds = [
                libc::pollfd {
                    fd: input.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                    events: libc::POLLOUT,
                    revents: 0,
                },
                libc::pollfd {
                    fd: output.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: error.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // SAFETY: the fixed array is valid and each nonnegative fd stays
            // owned through poll. Negative fds are ignored, including all-idle
            // cleanup waits. Timeout still bounds cancellation/cleanup latency.
            let waited = unsafe {
                libc::poll(
                    fds.as_mut_ptr(),
                    fds.len() as libc::nfds_t,
                    TICK.as_millis() as libc::c_int,
                )
            };
            let wait_error = if waited < 0 {
                let error = io::Error::last_os_error();
                (error.kind() != io::ErrorKind::Interrupted).then_some(error)
            } else if fds.iter().any(|fd| fd.revents & libc::POLLNVAL != 0) {
                Some(io::Error::other("controlled pipe became invalid"))
            } else {
                None
            };
            if let Some(wait_error) = wait_error {
                problem.get_or_insert(wait_error.to_string());
                input = None;
                output = None;
                // Close bad descriptors before the next bounded cleanup wait.
                error = None;
            }
        }
        let owned = owner.owned.as_ref().expect("owner");
        let status = owned.status;
        let confirmed = status.is_some() && group_absent(pgid).unwrap_or(false);
        // macOS may reject signalling an already-exited group leader. A
        // subsequently reaped child and absent group establish stop without
        // treating that race as an execution I/O failure.
        if !confirmed && problem.is_none() {
            problem = stop_problem;
        }
        let stdout = out.text();
        let stderr = err.text();
        let pending = if confirmed {
            owner.owned = None;
            None
        } else {
            Some(owner)
        };
        if confirmed
            && let Some(status) = status
            && let Some(detail) = crate::confine::classify_apply_failure(&status, &stdout, &stderr)
        {
            return Err(SandboxError::NotEnforced(detail));
        }
        Ok(ControlledOutcome {
            end: if !confirmed {
                ControlledEnd::StopUnconfirmed
            } else if was_cancelled {
                ControlledEnd::Cancelled
            } else {
                ControlledEnd::Exited
            },
            status,
            stdout,
            stderr,
            stdout_truncated: out.truncated,
            stderr_truncated: err.truncated,
            stdout_eof: out.eof,
            stderr_eof: err.eof,
            problem,
            pending,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::SandboxMode;
        use std::io::BufRead;
        static REGISTRY_TEST: Mutex<()> = Mutex::new(());

        // Invoked as a real child of the controlled launcher, never in-process.
        #[test]
        #[ignore = "fixture entered explicitly by controlled tests"]
        fn dummy_child() {
            let mut input = io::BufReader::new(io::stdin());
            let mut mode = String::new();
            input.read_line(&mut mode).unwrap();
            let mode = mode.trim_end();
            if mode == "echo" {
                let mut body = String::new();
                input.read_to_string(&mut body).unwrap();
                println!("ECHO:{body}");
                eprintln!("ERROR-MARKER");
            } else if let Some(path) = mode.strip_prefix("authorized-marker|") {
                std::fs::write(path, "payload-started").unwrap();
                println!("ECHO:authorized-payload");
            } else if mode == "fail" {
                std::process::exit(7);
            } else if mode == "invalid-utf8" {
                io::stdout().write_all(&vec![0xff; LIMIT]).unwrap();
                io::stderr().write_all(&vec![0xff; LIMIT]).unwrap();
            } else if mode == "flood" {
                io::stdout().write_all(&vec![b'a'; LIMIT * 2]).unwrap();
                io::stderr().write_all(&vec![b'b'; LIMIT * 2]).unwrap();
            } else if let Some(ready) = mode.strip_prefix("ignore|") {
                unsafe {
                    libc::signal(libc::SIGTERM, libc::SIG_IGN);
                }
                std::fs::write(ready, std::process::id().to_string()).unwrap();
                loop {
                    std::thread::sleep(Duration::from_millis(20));
                }
            } else if let Some(ready) = mode.strip_prefix("term|") {
                std::fs::write(ready, std::process::id().to_string()).unwrap();
                loop {
                    std::thread::sleep(Duration::from_millis(20));
                }
            } else if let Some(ready) = mode.strip_prefix("escape|") {
                // This fixture retains the pipe in a new session. The test
                // owns its PID separately and explicitly cleans it up.
                let pid = unsafe { libc::fork() };
                assert!(pid >= 0);
                if pid == 0 {
                    unsafe {
                        libc::setsid();
                        libc::sleep(4);
                        libc::_exit(0);
                    }
                }
                std::fs::write(ready, pid.to_string()).unwrap();
            } else {
                panic!("unknown fixture mode");
            }
        }

        fn policy() -> SandboxPolicy {
            SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[std::env::temp_dir()]).unwrap()
        }
        fn dummy_args() -> Vec<String> {
            [
                "--exact",
                "controlled::native::tests::dummy_child",
                "--ignored",
                "--nocapture",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect()
        }
        fn launch(mode: &str, cancel: impl FnMut() -> bool, timing: Timing) -> ControlledOutcome {
            run(
                &policy(),
                &std::env::current_exe().unwrap(),
                &dummy_args(),
                Some(mode),
                cancel,
                timing,
            )
            .unwrap()
        }
        fn assert_reaped(status: Option<ExitStatus>, pid: u32) {
            assert!(status.is_some());
            let mut raw = 0;
            assert_eq!(
                unsafe { libc::waitpid(pid as libc::pid_t, &mut raw, libc::WNOHANG) },
                -1
            );
            assert_eq!(
                io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
        }

        #[test]
        fn pre_cancel_never_spawns_even_a_missing_program() {
            let out = run(
                &policy(),
                Path::new("/missing-controlled-dummy"),
                &[],
                None,
                || true,
                Timing::default(),
            )
            .unwrap();
            assert_eq!(out.end, ControlledEnd::CancelledBeforeSpawn);
            assert!(out.status.is_none() && out.pending.is_none());
        }

        #[test]
        fn reservation_bound_is_enforced_before_spawn() {
            const MARKER: &str = "CONTROLLED_CAPACITY_FIXTURE";
            if std::env::var_os(MARKER).is_none() {
                let out = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "controlled::native::tests::reservation_bound_is_enforced_before_spawn",
                        "--nocapture",
                    ])
                    .env_clear()
                    .env(MARKER, "1")
                    .output()
                    .unwrap();
                assert!(
                    out.status.success(),
                    "{}",
                    String::from_utf8_lossy(&out.stderr)
                );
                return;
            }
            let mut reservations: Vec<_> = (0..MAX_OWNERS)
                .map(|_| Reservation::acquire().unwrap())
                .collect();
            assert!(Reservation::acquire().is_err());
            let p = SandboxPolicy::new(SandboxMode::ReadOnly, &[]).unwrap();
            let error = run(
                &p,
                Path::new("/missing-capacity-fixture"),
                &[],
                None,
                || false,
                Timing::default(),
            )
            .unwrap_err();
            assert!(matches!(error, SandboxError::NotEnforced(_)));
            reservations.pop();
            let restored = Reservation::acquire().unwrap();
            drop((restored, reservations));
            assert_eq!(RESERVED.load(Ordering::Acquire), 0);
        }

        #[test]
        fn broker_v1_is_rejected_without_connecting() {
            const MARKER: &str = "CONTROLLED_BROKER_FIXTURE";
            if std::env::var_os(MARKER).is_some() {
                let error = run_confined_controlled(
                    &policy(),
                    Path::new("/missing-broker-fixture"),
                    &[],
                    None,
                    || false,
                )
                .unwrap_err();
                assert!(
                    matches!(error, SandboxError::NotEnforced(ref text) if text.contains("broker v1"))
                );
                let error = run_confined_controlled_authorized(
                    &policy(),
                    Path::new("/missing-broker-fixture"),
                    &[],
                    None,
                    || false,
                    || panic!("broker must be rejected before native authorization"),
                )
                .unwrap_err();
                assert!(
                    matches!(error, SandboxError::NotEnforced(ref text) if text.contains("broker v1"))
                );
                return;
            }
            let home = tempfile::Builder::new()
                .prefix("cb")
                .tempdir_in("/tmp")
                .unwrap();
            let home = home.path().canonicalize().unwrap();
            let dir = home
                .join(".local/share/codex-benchmarks")
                .join("a".repeat(32));
            std::fs::create_dir_all(&dir).unwrap();
            let socket = dir.join("broker.sock");
            let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
            listener.set_nonblocking(true).unwrap();
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "controlled::native::tests::broker_v1_is_rejected_without_connecting",
                    "--nocapture",
                ])
                .env_clear()
                .env(MARKER, "1")
                .env("HOME", &home)
                .env("POLARIS_SANDBOX_BROKER", &socket)
                .env("POLARIS_SANDBOX_BROKER_TOKEN", "b".repeat(64))
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
        }

        #[test]
        fn authorization_is_checked_once_immediately_before_spawn() {
            use std::cell::Cell;
            for allow in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let marker = dir.path().join("started");
                let payload = format!("authorized-marker|{}\n", marker.display());
                let cancel_checks = Cell::new(0);
                let auth_checks = Cell::new(0);
                let out = run_confined_controlled_authorized(
                    &policy(),
                    &std::env::current_exe().unwrap(),
                    &dummy_args(),
                    Some(&payload),
                    || {
                        cancel_checks.set(cancel_checks.get() + 1);
                        false
                    },
                    || {
                        assert_eq!(
                            cancel_checks.get(),
                            2,
                            "authorization must follow command preparation"
                        );
                        auth_checks.set(auth_checks.get() + 1);
                        allow && auth_checks.get() == 1
                    },
                )
                .unwrap();
                assert_eq!(auth_checks.get(), 1);
                assert_eq!(marker.exists(), allow);
                if allow {
                    assert_eq!(out.end, ControlledEnd::Exited, "{out:?}");
                    assert!(out.status.unwrap().success());
                    assert!(out.stdout.contains("ECHO:authorized-payload"));
                    assert!(cancel_checks.get() > 2);
                } else {
                    assert_eq!(out.end, ControlledEnd::CancelledBeforeSpawn);
                    assert!(out.status.is_none() && out.pending.is_none());
                    assert!(out.stdout.is_empty() && out.stderr.is_empty());
                    assert_eq!(cancel_checks.get(), 2);
                }
            }
        }

        #[test]
        fn normal_stdin_and_both_outputs() {
            let out = launch("echo\nhello-controlled", || false, Timing::default());
            assert_eq!(out.end, ControlledEnd::Exited, "{out:?}");
            assert!(out.status.unwrap().success());
            assert!(out.stdout.contains("ECHO:hello-controlled"));
            assert!(out.stderr.contains("ERROR-MARKER"));
            assert!(out.stdout_eof && out.stderr_eof);
            assert!(out.pending.is_none() && out.problem.is_none(), "{out:?}");
        }

        #[test]
        fn output_is_bounded_but_excess_is_drained() {
            // A small OS pipe must not incur a full idle tick for every read.
            // Keep the same deadline as the service's output ownership fixture.
            let start = Instant::now();
            let out = launch(
                "flood\n",
                || start.elapsed() >= Duration::from_secs(5),
                Timing::default(),
            );
            assert_eq!(
                out.end,
                ControlledEnd::Exited,
                "elapsed={:?}, stdout={}, stderr={}, status={:?}",
                start.elapsed(),
                out.stdout.len(),
                out.stderr.len(),
                out.status
            );
            assert!(out.status.unwrap().success());
            assert_eq!(out.stdout.len(), LIMIT);
            assert_eq!(out.stderr.len(), LIMIT);
            assert!(out.stdout_truncated && out.stderr_truncated);
            assert!(out.stdout_eof && out.stderr_eof);
        }

        #[test]
        fn invalid_utf8_remains_bounded_after_replacement() {
            let out = launch("invalid-utf8\n", || false, Timing::default());
            assert_eq!(out.end, ControlledEnd::Exited, "{out:?}");
            assert!(out.status.unwrap().success());
            assert!(out.stdout.len() <= LIMIT && out.stderr.len() <= LIMIT);
            assert!(out.stdout.capacity() <= LIMIT && out.stderr.capacity() <= LIMIT);
            assert!(out.stdout.contains('\u{fffd}') && out.stderr.contains('\u{fffd}'));
            assert!(out.stdout_truncated && out.stderr_truncated);
            assert!(out.stdout_eof && out.stderr_eof);
        }

        #[test]
        fn nonzero_is_an_exit_not_cancellation() {
            let out = launch("fail\n", || false, Timing::default());
            assert_eq!(out.end, ControlledEnd::Exited);
            assert_eq!(out.status.unwrap().code(), Some(7));
            assert!(out.pending.is_none());
        }

        #[test]
        fn cancellation_term_and_kill_reap_only_owned_child() {
            use std::os::unix::process::ExitStatusExt;
            // A separate, unrelated child must survive both group signals.
            let mut unrelated = std::process::Command::new("/bin/sleep")
                .arg("10")
                .spawn()
                .unwrap();
            for (mode, expected) in [("term", libc::SIGTERM), ("ignore", libc::SIGKILL)] {
                let dir = tempfile::tempdir().unwrap();
                let ready = dir.path().join("ready");
                let start = Instant::now();
                let out = launch(
                    &format!("{mode}|{}\n", ready.display()),
                    || ready.exists(),
                    Timing::default(),
                );
                assert!(start.elapsed() < Duration::from_secs(4));
                assert_eq!(out.end, ControlledEnd::Cancelled, "{out:?}");
                assert_eq!(out.status.unwrap().signal(), Some(expected));
                let pid = std::fs::read_to_string(&ready).unwrap().parse().unwrap();
                assert_reaped(out.status, pid);
                assert!(unrelated.try_wait().unwrap().is_none());
            }
            unrelated.kill().unwrap();
            unrelated.wait().unwrap();
        }

        #[test]
        fn deadline_retains_handle_and_drop_parks_ownership() {
            let _serial = REGISTRY_TEST.lock().unwrap_or_else(|e| e.into_inner());
            let dir = tempfile::tempdir().unwrap();
            let ready = dir.path().join("ready");
            let start = Instant::now();
            let timing = Timing {
                defer_reap: true,
                ..Timing::default()
            };
            let mut out = launch(
                &format!("ignore|{}\n", ready.display()),
                || ready.exists(),
                timing,
            );
            assert!(start.elapsed() >= Duration::from_millis(2500));
            assert!(start.elapsed() < Duration::from_secs(5));
            assert_eq!(out.end, ControlledEnd::StopUnconfirmed);
            assert!(out.status.is_none());
            let handle = out.pending.take().unwrap();
            let id = handle.child_id().unwrap();
            drop(handle);
            let mut recovered = take_pending_cleanups();
            let index = recovered
                .iter()
                .position(|p| p.child_id() == Some(id))
                .unwrap();
            let mut handle = recovered.swap_remove(index);
            drop(recovered);
            let status = handle.try_reap().unwrap();
            assert_reaped(status, id);
            assert!(handle.child_id().is_none());
            assert!(PENDING.lock().unwrap().is_empty());
        }

        #[test]
        fn escaped_pipe_does_not_block_return() {
            let dir = tempfile::tempdir().unwrap();
            let ready = dir.path().join("ready");
            let start = Instant::now();
            let out = launch(
                &format!("escape|{}\n", ready.display()),
                || false,
                Timing::default(),
            );
            let pid: libc::pid_t = std::fs::read_to_string(&ready).unwrap().parse().unwrap();
            // Only this fixture-created PID, never an arbitrary process group.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
            let cleanup_deadline = Instant::now() + Duration::from_secs(2);
            while unsafe { libc::kill(pid, 0) } == 0 {
                assert!(
                    Instant::now() < cleanup_deadline,
                    "escaped fixture was not cleaned up"
                );
                std::thread::sleep(TICK);
            }
            assert!(start.elapsed() < Duration::from_secs(3));
            assert_eq!(out.end, ControlledEnd::Exited, "{out:?}");
            assert!(!out.stdout_eof || !out.stderr_eof);
        }

        #[test]
        fn callback_panic_preserves_child_in_registry() {
            let _serial = REGISTRY_TEST.lock().unwrap_or_else(|e| e.into_inner());
            let dir = tempfile::tempdir().unwrap();
            let ready = dir.path().join("ready");
            let result = std::panic::catch_unwind(|| {
                launch(
                    &format!("term|{}\n", ready.display()),
                    || {
                        assert!(!ready.exists(), "injected owner unwind");
                        false
                    },
                    Timing::default(),
                )
            });
            assert!(result.is_err());
            let id: u32 = std::fs::read_to_string(&ready).unwrap().parse().unwrap();
            let mut recovered = take_pending_cleanups();
            let index = recovered
                .iter()
                .position(|p| p.child_id() == Some(id))
                .unwrap();
            let mut handle = recovered.swap_remove(index);
            drop(recovered);
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                if let Some(status) = handle.try_reap().unwrap() {
                    assert_reaped(Some(status), id);
                    assert!(PENDING.lock().unwrap().is_empty());
                    break;
                }
                assert!(Instant::now() < deadline);
                std::thread::sleep(TICK);
            }
        }
    }
}
