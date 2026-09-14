//! Long-lived stdio uses the same native preparation, reservation and unreaped
//! process-group identity as controlled one-shot execution.
use super::*;
use std::collections::BTreeMap;
use std::process::{ChildStderr, ChildStdin, ChildStdout};
use tokio::io::{Interest, unix::AsyncFd};

/// Move these pipes to the protocol transport. Keep ManagedStdio in the run
/// owner, independently of the model/transport Future. Drive wait_and_poll
/// while awaiting protocol messages. Stderr is discarded, never returned.
pub struct ManagedStdioPipes {
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
}

#[must_use = "retain in the run owner until stop is confirmed or pending cleanup is transferred"]
pub struct ManagedStdio {
    owner: Option<PendingCleanup>,
    stderr: Option<Stderr>,
    term_at: Option<Instant>,
    kill_at: Option<Instant>,
    cancelled: bool,
    exited: bool,
    problem: bool,
    complete: Option<ControlledEnd>,
}

// The original stderr pipe and its reactor registration stay private. A
// duplicated descriptor would still grant callers access to credential bytes.
enum Stderr {
    Plain(ChildStderr),
    Registered(AsyncFd<ChildStderr>),
}

impl Stderr {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(pipe) => pipe.read(bytes),
            // Preserve readiness after partial progress or a full quantum;
            // only an actual WouldBlock clears it.
            Self::Registered(pipe) => pipe.try_io_mut(Interest::READABLE, |p| p.read(bytes)),
        }
    }
}

impl ManagedStdio {
    /// Preparation and the last authorization check are synchronous. None
    /// means authorization was revoked before spawn; no child was started.
    pub fn spawn_authorized(
        policy: &SandboxPolicy,
        program: &Path,
        args: &[String],
        cwd: &Path,
        secret_environment: &BTreeMap<String, String>,
        mut authorized: impl FnMut() -> bool,
    ) -> Result<Option<(Self, ManagedStdioPipes)>, SandboxError> {
        if crate::broker::from_environment()?.is_some() {
            return Err(SandboxError::NotEnforced(
                "broker v1 cannot own MCP stdio".into(),
            ));
        }
        if !program.is_absolute()
            || !cwd.is_absolute()
            || !program.is_file()
            || !cwd.is_dir()
            || program.canonicalize().ok().as_deref() != Some(program)
            || cwd.canonicalize().ok().as_deref() != Some(cwd)
            || args.iter().any(|arg| arg.contains('\0'))
        {
            return Err(SandboxError::NotEnforced(
                "invalid MCP process destination".into(),
            ));
        }
        if let Some(boundary) = policy.isolated_boundary()
            && cwd != boundary.workspace
        {
            return Err(SandboxError::NotEnforced(
                "MCP cwd differs from isolated workspace".into(),
            ));
        }
        let reservation = Reservation::acquire()?;
        let mut command = crate::confine::build_command(policy, program, args)?;
        for (name, value) in secret_environment {
            if !secret_name(name) || value.contains(['\0', '\r', '\n']) {
                return Err(SandboxError::NotEnforced(
                    "invalid MCP secret environment".into(),
                ));
            }
            command.env(name, value);
        }
        command
            .current_dir(cwd)
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if !authorized() {
            return Ok(None);
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
        let child = &mut owner.owned.as_mut().expect("owner").child;
        let pipes = ManagedStdioPipes {
            stdin: child.stdin.take().expect("piped stdin"),
            stdout: child.stdout.take().expect("piped stdout"),
        };
        let stderr = child.stderr.take();
        nonblocking(stderr.as_ref().expect("piped stderr"))?;
        Ok(Some((
            Self {
                owner: Some(owner),
                stderr: stderr.map(Stderr::Plain),
                term_at: None,
                kill_at: None,
                cancelled: false,
                exited: false,
                problem: false,
                complete: None,
            },
            pipes,
        )))
    }

    /// Latches cancellation. The run owner closes protocol pipes and continues
    /// polling; stopping the protocol Future alone does not establish shutdown.
    pub fn request_stop(&mut self) {
        self.cancelled = true;
        self.stderr = self.stderr.take().map(|stderr| match stderr {
            Stderr::Registered(pipe) => Stderr::Plain(pipe.into_inner()),
            plain => plain,
        });
    }

    /// Wait for stderr readiness or the next lifecycle tick, then perform one
    /// bounded nonblocking poll. Call on a Tokio runtime with I/O and time
    /// enabled. Dropping this future never drops or transfers the child owner.
    pub async fn wait_and_poll(&mut self) -> Option<ControlledEnd> {
        if self.complete.is_some() {
            return self.complete;
        }
        if !self.problem && !self.cancelled {
            self.stderr = match self.stderr.take() {
                Some(Stderr::Plain(pipe)) => {
                    match AsyncFd::try_with_interest(pipe, Interest::READABLE) {
                        Ok(pipe) => Some(Stderr::Registered(pipe)),
                        Err(error) => {
                            // Do not expose OS diagnostics or lose the pipe on
                            // registration failure. The owner must stop/reap.
                            self.problem = true;
                            let (pipe, _) = error.into_parts();
                            Some(Stderr::Plain(pipe))
                        }
                    }
                }
                other => other,
            };
        }
        if !self.problem {
            if let Some(Stderr::Registered(pipe)) = self.stderr.as_mut() {
                tokio::select! {
                    ready = pipe.readable() => {
                        // Dropping the guard retains readiness. poll() clears
                        // it through try_io_mut only after a real WouldBlock.
                        if ready.is_err() { self.problem = true; }
                    }
                    _ = tokio::time::sleep(TICK) => {}
                }
            } else {
                tokio::time::sleep(TICK).await;
            }
        } else {
            tokio::time::sleep(TICK).await;
        }
        let end = self.poll();
        // AsyncFd::readable does not charge Tokio's cooperative budget. A
        // continuously ready child must still let timers and other tasks run.
        tokio::task::coop::consume_budget().await;
        end
    }

    /// One bounded, nonblocking owner tick. None means the child remains owned.
    /// StopUnconfirmed retains ownership for take_pending; never means success.
    pub fn poll(&mut self) -> Option<ControlledEnd> {
        if let Some(end) = self.complete {
            return Some(end);
        }
        let now = Instant::now();
        let owned = self.owner.as_mut()?.owned.as_mut()?;
        if !self.exited {
            match exited_without_reap(owned) {
                Ok(exited) => self.exited = exited,
                Err(_) => self.problem = true,
            }
        }
        if (self.cancelled || self.exited || self.problem) && self.term_at.is_none() {
            if owned.may_signal {
                let _ = signal(owned.pgid, libc::SIGTERM);
            }
            self.term_at = Some(now);
        }
        if self
            .term_at
            .is_some_and(|at| now.duration_since(at) >= Timing::default().term)
            && self.kill_at.is_none()
        {
            if owned.may_signal {
                let _ = signal(owned.pgid, libc::SIGKILL);
            }
            owned.may_signal = false;
            self.kill_at = Some(now);
        }
        // No credential-bearing stderr retention, even on failure.
        if let Some(stderr) = self.stderr.as_mut() {
            let mut bytes = [0; 8192];
            for _ in 0..(QUANTUM / 8192) {
                match stderr.read(&mut bytes) {
                    Ok(0) => {
                        self.stderr = None;
                        break;
                    }
                    Ok(_) => {}
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) =>
                    {
                        break;
                    }
                    Err(_) => {
                        self.problem = true;
                        self.stderr = None;
                        break;
                    }
                }
            }
        }
        let killed = self.kill_at?;
        if now.duration_since(killed) >= Timing::default().drain {
            self.stderr = None;
        }
        if owned.status.is_none() {
            match owned.child.try_wait() {
                Ok(status) => owned.status = status,
                Err(_) => self.problem = true,
            }
        }
        if owned.status.is_some()
            && group_absent(owned.pgid).unwrap_or(false)
            && self.stderr.is_none()
        {
            self.owner.as_mut().expect("owner").owned = None;
            self.complete = Some(if self.cancelled {
                ControlledEnd::Cancelled
            } else {
                ControlledEnd::Exited
            });
        } else if now.duration_since(killed) >= Timing::default().kill {
            self.complete = Some(ControlledEnd::StopUnconfirmed);
        }
        self.complete
    }

    pub fn has_io_problem(&self) -> bool {
        self.problem
    }

    pub fn take_pending(&mut self) -> Option<PendingCleanup> {
        if self.complete == Some(ControlledEnd::StopUnconfirmed) {
            self.owner.take()
        } else {
            None
        }
    }
}

fn secret_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.is_empty()
        || !(bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
        || !bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
    {
        return false;
    }
    // Do not let registered credentials override the native runtime envelope
    // or select loader/interpreter startup code before the server starts.
    !crate::child_environment::is_runtime_variable(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SandboxMode;
    use std::io::BufRead;

    #[test]
    #[ignore = "synthetic child launched by managed owner tests"]
    fn fixture_child() {
        let mut mode = String::new();
        std::io::stdin().lock().read_line(&mut mode).unwrap();
        if matches!(mode.trim(), "ignore" | "flood") {
            unsafe {
                libc::signal(libc::SIGTERM, libc::SIG_IGN);
            }
        }
        // Exercise bounded draining without returning credential-bearing data.
        std::io::stderr()
            .write_all(&vec![b'x'; 512 * 1024])
            .unwrap();
        println!(
            "READY:{}",
            std::env::var("FIXTURE_API_KEY").unwrap_or_default()
        );
        std::io::stdout().flush().unwrap();
        if mode.trim() == "flood" {
            let bytes = vec![b'x'; QUANTUM];
            loop {
                std::io::stderr().write_all(&bytes).unwrap();
            }
        }
        if mode.trim() == "ignore" {
            loop {
                std::thread::sleep(TICK);
            }
        }
    }

    fn args() -> Vec<String> {
        [
            "--exact",
            "controlled::native::managed::tests::fixture_child",
            "--ignored",
            "--nocapture",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    }
    fn policy() -> SandboxPolicy {
        SandboxPolicy::new(SandboxMode::ReadOnly, &[]).unwrap()
    }

    #[test]
    fn rejects_runtime_override_before_authorization_and_honors_revocation() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let program = std::env::current_exe().unwrap().canonicalize().unwrap();
        assert!(
            ManagedStdio::spawn_authorized(
                &policy(),
                &program,
                &args(),
                &cwd,
                &BTreeMap::from([("NODE_OPTIONS".into(), "synthetic-secret".into())]),
                || panic!("invalid environment reached authorization")
            )
            .is_err()
        );
        assert!(
            ManagedStdio::spawn_authorized(
                &policy(),
                &program,
                &args(),
                &cwd,
                &BTreeMap::new(),
                || false
            )
            .unwrap()
            .is_none()
        );
    }

    #[tokio::test]
    async fn drains_stderr_and_reaps_normal_and_term_ignoring_children() {
        use std::os::unix::process::ExitStatusExt;
        for mode in ["normal\n", "ignore\n"] {
            let dir = tempfile::tempdir().unwrap();
            let cwd = dir.path().canonicalize().unwrap();
            let program = std::env::current_exe().unwrap().canonicalize().unwrap();
            let (mut owner, mut pipes) = ManagedStdio::spawn_authorized(
                &policy(),
                &program,
                &args(),
                &cwd,
                &BTreeMap::from([("FIXTURE_API_KEY".into(), "synthetic-secret".into())]),
                || true,
            )
            .unwrap()
            .unwrap();
            let pid = owner.owner.as_ref().unwrap().child_id().unwrap();
            pipes.stdin.write_all(mode.as_bytes()).unwrap();
            nonblocking(&pipes.stdout).unwrap();
            let mut text = Vec::new();
            let started = Instant::now();
            let end = loop {
                let mut bytes = [0; 4096];
                match pipes.stdout.read(&mut bytes) {
                    Ok(n) => text.extend_from_slice(&bytes[..n]),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => panic!("{e}"),
                }
                if mode == "ignore\n"
                    && !owner.cancelled
                    && String::from_utf8_lossy(&text).contains("READY:synthetic-secret")
                {
                    owner.request_stop();
                    assert!(matches!(owner.stderr, Some(Stderr::Plain(_))));
                }
                let end = owner.wait_and_poll().await;
                if owner.cancelled {
                    assert!(!matches!(owner.stderr, Some(Stderr::Registered(_))));
                }
                if let Some(end) = end {
                    break end;
                }
                assert!(
                    started.elapsed() < Duration::from_secs(8),
                    "mode={mode:?} pid={pid} ready={} stdout_bytes={} exited={} cancelled={} problem={} term={:?} kill={:?} complete={:?}",
                    String::from_utf8_lossy(&text).contains("READY:synthetic-secret"),
                    text.len(),
                    owner.exited,
                    owner.cancelled,
                    owner.problem,
                    owner.term_at,
                    owner.kill_at,
                    owner.complete,
                );
            };
            assert!(String::from_utf8_lossy(&text).contains("READY:synthetic-secret"));
            assert_eq!(
                end,
                if mode == "ignore\n" {
                    ControlledEnd::Cancelled
                } else {
                    ControlledEnd::Exited
                }
            );
            assert!(!owner.has_io_problem());
            assert!(owner.take_pending().is_none());
            // waitpid must report already reaped, not just a missing output pipe.
            let mut status = 0;
            assert_eq!(
                unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) },
                -1
            );
            assert_eq!(
                io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
            // Keep the platform trait exercised without converting raw wait status.
            let _ = std::process::ExitStatus::from_raw(status).signal();
        }
    }

    #[test]
    fn sync_cleanup_retains_ownership_after_async_runtime_drop() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let program = std::env::current_exe().unwrap().canonicalize().unwrap();
        // Spawning must not require a Tokio runtime.
        let (mut owner, mut pipes) = ManagedStdio::spawn_authorized(
            &policy(),
            &program,
            &args(),
            &cwd,
            &BTreeMap::new(),
            || true,
        )
        .unwrap()
        .unwrap();
        let pid = owner.owner.as_ref().unwrap().child_id().unwrap();
        pipes.stdin.write_all(b"ignore\n").unwrap();
        nonblocking(&pipes.stdout).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let mut text = Vec::new();
            let started = Instant::now();
            while !String::from_utf8_lossy(&text).contains("READY:") {
                let mut bytes = [0; 4096];
                match pipes.stdout.read(&mut bytes) {
                    Ok(n) => text.extend_from_slice(&bytes[..n]),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => panic!("{e}"),
                }
                assert!(owner.wait_and_poll().await.is_none());
                assert!(started.elapsed() < Duration::from_secs(8));
            }
            // Poll and then discard a driver future while the child is idle.
            // Neither dropping the wait nor its runtime may release the owner.
            let wait = owner.wait_and_poll();
            tokio::pin!(wait);
            std::future::poll_fn(|cx| {
                let _ = std::future::Future::poll(wait.as_mut(), cx);
                std::task::Poll::Ready(())
            })
            .await;
        });
        drop(runtime);
        assert!(!owner.cancelled);
        owner.request_stop();
        let started = Instant::now();
        let end = loop {
            if let Some(end) = owner.poll() {
                break end;
            }
            assert!(started.elapsed() < Duration::from_secs(3));
            std::thread::sleep(TICK);
        };
        assert_eq!(end, ControlledEnd::Cancelled);
        assert!(!owner.has_io_problem());
        assert!(owner.take_pending().is_none());
        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }
    #[tokio::test]
    async fn ready_stderr_charges_cooperative_budget() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let program = std::env::current_exe().unwrap().canonicalize().unwrap();
        let (mut owner, mut pipes) = ManagedStdio::spawn_authorized(
            &policy(),
            &program,
            &args(),
            &cwd,
            &BTreeMap::new(),
            || true,
        )
        .unwrap()
        .unwrap();
        pipes.stdin.write_all(b"ignore\n").unwrap();
        assert!(owner.wait_and_poll().await.is_none());
        let Some(Stderr::Registered(pipe)) = &owner.stderr else {
            panic!("stderr not registered")
        };
        // Establish retained readiness without consuming the bytes. This
        // deterministically exercises an immediately-ready owner poll.
        drop(
            tokio::time::timeout(Duration::from_secs(8), pipe.readable())
                .await
                .unwrap()
                .unwrap(),
        );
        // Leave exactly one unit. select! already checks an exhausted budget;
        // the regression is whether successful draining charges that budget.
        loop {
            let permit = std::future::poll_fn(tokio::task::coop::poll_proceed).await;
            if !tokio::task::coop::has_budget_remaining() {
                drop(permit); // Restore the final unit without making progress.
                break;
            }
            permit.made_progress();
        }
        let (ready, remaining_budget) = {
            let wait = owner.wait_and_poll();
            tokio::pin!(wait);
            std::future::poll_fn(|cx| {
                let ready = std::future::Future::poll(wait.as_mut(), cx).is_ready();
                std::task::Poll::Ready((ready, tokio::task::coop::has_budget_remaining()))
            })
            .await
        };
        owner.request_stop();
        let started = Instant::now();
        let end = loop {
            if let Some(end) = owner.wait_and_poll().await {
                break end;
            }
            assert!(started.elapsed() < Duration::from_secs(3));
        };
        assert_eq!(end, ControlledEnd::Cancelled);
        assert!(!owner.has_io_problem());
        assert!(owner.take_pending().is_none());
        assert!(
            ready,
            "retained readiness did not exercise an immediate owner tick"
        );
        assert!(
            !remaining_budget,
            "ready stderr did not charge cooperative budget"
        );
    }

    #[tokio::test]
    async fn continuous_stderr_allows_timer_cancellation_and_reaping() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let program = std::env::current_exe().unwrap().canonicalize().unwrap();
        let (mut owner, mut pipes) = ManagedStdio::spawn_authorized(
            &policy(),
            &program,
            &args(),
            &cwd,
            &BTreeMap::new(),
            || true,
        )
        .unwrap()
        .unwrap();
        let pid = owner.owner.as_ref().unwrap().child_id().unwrap();
        pipes.stdin.write_all(b"flood\n").unwrap();
        nonblocking(&pipes.stdout).unwrap();
        let started = Instant::now();
        let mut text = Vec::new();
        while !String::from_utf8_lossy(&text).contains("READY:") {
            let mut bytes = [0; 4096];
            match pipes.stdout.read(&mut bytes) {
                Ok(n) => text.extend_from_slice(&bytes[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => panic!("{e}"),
            }
            assert!(owner.wait_and_poll().await.is_none());
            assert!(started.elapsed() < Duration::from_secs(8));
        }
        // The child is now continuously producing stderr, with no sleep.
        let started = Instant::now();
        let cancellation = tokio::time::sleep(Duration::from_millis(100));
        tokio::pin!(cancellation);
        let cancelled_by_timer = loop {
            tokio::select! {
                biased;
                _ = &mut cancellation => break true,
                end = owner.wait_and_poll() => assert!(end.is_none()),
            }
            // A wall-clock guard lets a broken cooperative path fail without
            // hanging its current-thread runtime or leaving a live fixture.
            if started.elapsed() >= Duration::from_secs(3) {
                break false;
            }
        };
        owner.request_stop();
        let cleanup = Instant::now();
        let end = loop {
            if let Some(end) = owner.wait_and_poll().await {
                break end;
            }
            assert!(cleanup.elapsed() < Duration::from_secs(3));
        };
        assert_eq!(end, ControlledEnd::Cancelled);
        assert!(!owner.has_io_problem());
        assert!(owner.take_pending().is_none());
        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
        assert!(
            cancelled_by_timer,
            "continuous stderr starved the cancellation timer"
        );
    }
}
