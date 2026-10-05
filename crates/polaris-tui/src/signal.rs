//! Linux process-signal capture for the interactive terminal lifetime.
//!
//! The OS handler only records the first signal. Terminal and task cleanup run
//! after `pending_linux_process_shutdown` is observed in ordinary code.

#![cfg(target_os = "linux")]

#[cfg(not(target_has_atomic = "8"))]
compile_error!("the Linux TUI signal handler requires a lock-free 8-bit atomic");

use std::fmt;
use std::io;
use std::mem::MaybeUninit;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

const SIGINT_CAPTURED: u8 = 1;
const SIGTERM_CAPTURED: u8 = 2;

// target_has_atomic = "8" guarantees this compare/exchange is available as a
// lock-free target atomic. The two C handlers do no other work.
static FIRST_SIGNAL: AtomicU8 = AtomicU8::new(0);
// There is exactly one interactive TUI per CLI process. Never reuse the
// handler's global atomic after teardown: a handler already in flight can
// finish after sigaction has restored the prior disposition.
static INSTALL_ATTEMPTED: AtomicBool = AtomicBool::new(false);
static OWNER_ACTIVE: AtomicBool = AtomicBool::new(false);

extern "C" fn capture_sigint(_: libc::c_int) {
    let _ = FIRST_SIGNAL.compare_exchange(0, SIGINT_CAPTURED, Ordering::SeqCst, Ordering::SeqCst);
}

extern "C" fn capture_sigterm(_: libc::c_int) {
    let _ = FIRST_SIGNAL.compare_exchange(0, SIGTERM_CAPTURED, Ordering::SeqCst, Ordering::SeqCst);
}

#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinuxProcessShutdown {
    SigInt,
    SigTerm,
}

#[doc(hidden)]
#[derive(Debug)]
pub struct LinuxSignalInstallError {
    operation: &'static str,
    source: io::Error,
}

impl LinuxSignalInstallError {
    fn new(operation: &'static str, source: io::Error) -> Self {
        Self { operation, source }
    }
}

impl fmt::Display for LinuxSignalInstallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.operation, self.source)
    }
}

impl std::error::Error for LinuxSignalInstallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// The single owner of SIGINT and SIGTERM while the Linux TUI is interactive.
#[doc(hidden)]
pub struct LinuxProcessSignalOwner {
    previous_sigint: libc::sigaction,
    previous_sigterm: libc::sigaction,
    sigint_installed: bool,
    sigterm_installed: bool,
    active: bool,
}

impl LinuxProcessSignalOwner {
    /// Restore both previous dispositions. CLI shutdown should call this and
    /// give an error status precedence over the clean signal exit code.
    #[doc(hidden)]
    pub fn try_uninstall(&mut self) -> io::Result<Option<LinuxProcessShutdown>> {
        self.try_uninstall_with(|| {})
    }

    // The test hook injects a signal while one owned disposition is still
    // installed. Production passes an empty hook; it is never a signal handler.
    fn try_uninstall_with(
        &mut self,
        before_sigint_restore: impl FnOnce(),
    ) -> io::Result<Option<LinuxProcessShutdown>> {
        if !self.active {
            return Ok(captured_shutdown());
        }
        let mut first_error = None;

        // Attempt both restorations even if one fails. The flags make retries
        // and the eventual Drop idempotent for dispositions already restored.
        if self.sigterm_installed {
            match restore_action(libc::SIGTERM, &self.previous_sigterm) {
                Ok(()) => self.sigterm_installed = false,
                Err(error) => first_error = Some(error),
            }
        }
        if self.sigint_installed {
            before_sigint_restore();
            match restore_action(libc::SIGINT, &self.previous_sigint) {
                Ok(()) => self.sigint_installed = false,
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
        }

        // Snapshot after the last owned disposition has been restored. A
        // signal captured during teardown must determine the clean exit code.
        let captured = captured_shutdown();
        if !self.sigint_installed && !self.sigterm_installed {
            OWNER_ACTIVE.store(false, Ordering::SeqCst);
            self.active = false;
        }

        match first_error {
            Some(error) => Err(error),
            None => Ok(captured),
        }
    }
}

impl Drop for LinuxProcessSignalOwner {
    fn drop(&mut self) {
        if let Err(error) = self.try_uninstall() {
            // Leaving an installed signal disposition without an owner would
            // make later process work unsafe. This is normal context, not the
            // signal handler. A cleanup failure wins over status 130/143.
            eprintln!("Linux signal handler restoration failed: {error}");
            std::process::exit(1);
        }
    }
}

/// Install the Linux process-signal owner before any terminal mutation.
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub fn install_linux_process_signal_owner()
-> Result<LinuxProcessSignalOwner, LinuxSignalInstallError> {
    if INSTALL_ATTEMPTED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err(LinuxSignalInstallError::new(
            "Linux signal owner installation already attempted in this process",
            io::Error::from(io::ErrorKind::AlreadyExists),
        ));
    }

    let previous_sigint = match install_action(libc::SIGINT, capture_sigint) {
        Ok(previous) => previous,
        Err(error) => {
            return Err(LinuxSignalInstallError::new(
                "install SIGINT handler",
                error,
            ));
        }
    };

    let previous_sigterm = match install_action(libc::SIGTERM, capture_sigterm) {
        Ok(previous) => previous,
        Err(error) => {
            if let Err(rollback_error) = restore_action(libc::SIGINT, &previous_sigint) {
                // The caller must exit 1. The attempted flag permanently
                // rejects another owner even if this rollback failed.
                return Err(LinuxSignalInstallError::new(
                    "install SIGTERM handler and roll back SIGINT handler",
                    io::Error::other(format!(
                        "SIGTERM: {error}; SIGINT rollback: {rollback_error}"
                    )),
                ));
            }
            return Err(LinuxSignalInstallError::new(
                "install SIGTERM handler",
                error,
            ));
        }
    };

    // A signal received between the two sigaction calls remains latched.
    OWNER_ACTIVE.store(true, Ordering::SeqCst);
    Ok(LinuxProcessSignalOwner {
        previous_sigint,
        previous_sigterm,
        sigint_installed: true,
        sigterm_installed: true,
        active: true,
    })
}

/// Return the first OS shutdown signal captured by the active owner.
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub fn pending_linux_process_shutdown() -> Option<LinuxProcessShutdown> {
    if !OWNER_ACTIVE.load(Ordering::SeqCst) {
        return None;
    }
    captured_shutdown()
}

fn captured_shutdown() -> Option<LinuxProcessShutdown> {
    match FIRST_SIGNAL.load(Ordering::SeqCst) {
        SIGINT_CAPTURED => Some(LinuxProcessShutdown::SigInt),
        SIGTERM_CAPTURED => Some(LinuxProcessShutdown::SigTerm),
        _ => None,
    }
}

fn install_action(
    signal: libc::c_int,
    handler: extern "C" fn(libc::c_int),
) -> io::Result<libc::sigaction> {
    // SAFETY: all sigaction fields are integer/pointer data; zero is valid
    // before assigning the handler, flags and an initialized empty mask.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = handler as *const () as libc::sighandler_t;
    action.sa_flags = libc::SA_RESTART;
    // SAFETY: `action.sa_mask` is writable and owned by this stack frame.
    if unsafe { libc::sigemptyset(&mut action.sa_mask) } != 0 {
        return Err(io::Error::last_os_error());
    }

    let mut previous = MaybeUninit::<libc::sigaction>::uninit();
    // SAFETY: both pointers are valid for the call; libc initializes
    // `previous` only when the installation succeeds.
    if unsafe { libc::sigaction(signal, &action, previous.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful sigaction wrote the previous disposition.
    Ok(unsafe { previous.assume_init() })
}

fn restore_action(signal: libc::c_int, previous: &libc::sigaction) -> io::Result<()> {
    // SAFETY: `previous` came from a successful sigaction call for this same
    // signal, and the old-action output is unused.
    if unsafe { libc::sigaction(signal, previous, ptr::null_mut()) } != 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::process::Command;

    use super::*;

    const CHILD_CASE_ENV: &str = "POLARIS_SIGNAL_OWNER_UNIT_CHILD";

    fn in_fresh_test_process(name: &str, variant: &str, case: impl FnOnce()) {
        let marker = format!("{name}:{variant}");
        if let Some(inherited_marker) = std::env::var_os(CHILD_CASE_ENV) {
            if inherited_marker == OsStr::new(&marker) {
                case();
            } else {
                assert!(
                    inherited_marker
                        .to_string_lossy()
                        .starts_with(&format!("{name}:")),
                    "unexpected isolated signal-test marker"
                );
            }
            return;
        }

        let output = Command::new(std::env::current_exe().unwrap())
            .env(CHILD_CASE_ENV, marker)
            .args(["--exact", name, "--nocapture"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "isolated {name} failed: {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn current_disposition(signal: libc::c_int) -> libc::sigaction {
        let mut result = MaybeUninit::<libc::sigaction>::uninit();
        // SAFETY: a null new-action pointer only queries the disposition;
        // the result pointer is valid and initialized when sigaction succeeds.
        assert_eq!(
            unsafe { libc::sigaction(signal, ptr::null(), result.as_mut_ptr()) },
            0
        );
        // SAFETY: the successful query initialized result.
        unsafe { result.assume_init() }
    }

    fn assert_same_disposition(expected: &libc::sigaction, actual: &libc::sigaction) {
        assert_eq!(expected.sa_sigaction, actual.sa_sigaction);
        // glibc may add its private SA_RESTORER trampoline when restoring
        // a disposition. It does not change the application-visible flags.
        #[cfg(any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64"))]
        let application_flags = !0x0400_0000;
        #[cfg(not(any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64")))]
        let application_flags = !0;
        assert_eq!(
            expected.sa_flags & application_flags,
            actual.sa_flags & application_flags
        );
        for signal in 1..=libc::SIGRTMAX() {
            // SAFETY: both masks came from successful sigaction queries.
            let expected_member = unsafe { libc::sigismember(&expected.sa_mask, signal) };
            let actual_member = unsafe { libc::sigismember(&actual.sa_mask, signal) };
            assert_eq!(expected_member, actual_member, "mask member {signal}");
        }
    }

    #[test]
    fn linux_signal_owner_sigint_is_typed_once() {
        in_fresh_test_process(
            "signal::tests::linux_signal_owner_sigint_is_typed_once",
            "only",
            || {
                let mut owner = install_linux_process_signal_owner().unwrap();
                capture_sigint(libc::SIGINT);
                capture_sigint(libc::SIGINT);
                assert_eq!(
                    pending_linux_process_shutdown(),
                    Some(LinuxProcessShutdown::SigInt)
                );
                assert_eq!(
                    owner.try_uninstall().unwrap(),
                    Some(LinuxProcessShutdown::SigInt)
                );
                assert_eq!(
                    owner.try_uninstall().unwrap(),
                    Some(LinuxProcessShutdown::SigInt)
                );
                drop(owner);
                assert_eq!(pending_linux_process_shutdown(), None);
            },
        );
    }

    #[test]
    fn linux_signal_owner_sigterm_is_typed_once() {
        in_fresh_test_process(
            "signal::tests::linux_signal_owner_sigterm_is_typed_once",
            "only",
            || {
                let owner = install_linux_process_signal_owner().unwrap();
                capture_sigterm(libc::SIGTERM);
                capture_sigterm(libc::SIGTERM);
                assert_eq!(
                    pending_linux_process_shutdown(),
                    Some(LinuxProcessShutdown::SigTerm)
                );
                drop(owner);
                assert_eq!(pending_linux_process_shutdown(), None);
            },
        );
    }

    #[test]
    fn linux_signal_owner_first_signal_wins() {
        in_fresh_test_process(
            "signal::tests::linux_signal_owner_first_signal_wins",
            "sigint_first",
            || {
                let owner = install_linux_process_signal_owner().unwrap();
                capture_sigint(libc::SIGINT);
                capture_sigterm(libc::SIGTERM);
                assert_eq!(
                    pending_linux_process_shutdown(),
                    Some(LinuxProcessShutdown::SigInt)
                );
                drop(owner);
            },
        );
        in_fresh_test_process(
            "signal::tests::linux_signal_owner_first_signal_wins",
            "sigterm_first",
            || {
                let owner = install_linux_process_signal_owner().unwrap();
                capture_sigterm(libc::SIGTERM);
                capture_sigint(libc::SIGINT);
                assert_eq!(
                    pending_linux_process_shutdown(),
                    Some(LinuxProcessShutdown::SigTerm)
                );
                drop(owner);
            },
        );
        in_fresh_test_process(
            "signal::tests::linux_signal_owner_first_signal_wins",
            "during_uninstall",
            || {
                let mut owner = install_linux_process_signal_owner().unwrap();
                let captured = owner
                    .try_uninstall_with(|| capture_sigint(libc::SIGINT))
                    .unwrap();
                assert_eq!(captured, Some(LinuxProcessShutdown::SigInt));
                assert_eq!(pending_linux_process_shutdown(), None);
            },
        );
    }

    #[test]
    fn linux_signal_owner_dispatcher_drop_has_no_stale_delivery() {
        in_fresh_test_process(
            "signal::tests::linux_signal_owner_dispatcher_drop_has_no_stale_delivery",
            "only",
            || {
                let before_sigint = current_disposition(libc::SIGINT);
                let before_sigterm = current_disposition(libc::SIGTERM);
                let owner = install_linux_process_signal_owner().unwrap();
                assert_eq!(
                    current_disposition(libc::SIGINT).sa_sigaction,
                    capture_sigint as *const () as libc::sighandler_t
                );
                assert_eq!(
                    current_disposition(libc::SIGTERM).sa_sigaction,
                    capture_sigterm as *const () as libc::sighandler_t
                );
                capture_sigint(libc::SIGINT);
                assert_eq!(
                    pending_linux_process_shutdown(),
                    Some(LinuxProcessShutdown::SigInt)
                );
                drop(owner);
                assert_same_disposition(&before_sigint, &current_disposition(libc::SIGINT));
                assert_same_disposition(&before_sigterm, &current_disposition(libc::SIGTERM));
                assert_eq!(pending_linux_process_shutdown(), None);

                // Model a handler already in flight as teardown completed.
                capture_sigterm(libc::SIGTERM);
                assert_eq!(pending_linux_process_shutdown(), None);
                for _ in 0..2 {
                    let error = match install_linux_process_signal_owner() {
                        Err(error) => error,
                        Ok(_) => panic!("process-global signal owner was reinstalled"),
                    };
                    assert_eq!(error.source.kind(), io::ErrorKind::AlreadyExists);
                }
            },
        );
    }
}
