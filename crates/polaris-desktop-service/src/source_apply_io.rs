//! One owned source-I/O thread, without Writer access or dispatch authorization.
//!
//! The Engine's blocking owner must poll and retain Finished, including failures.
//! Drop is emergency join only: it may block, must never run on the async reactor,
//! and does not imply persistence readiness. It releases completed ownership after
//! joining; normal shutdown must instead retain Finished and save/handoff evidence.
use crate::source_apply::{
    PreparedSourceRecovery, SourceApplyController, SourceApplyError, SourceApplyRequest,
};
use std::{
    io,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Mutex, MutexGuard},
    thread::{self, JoinHandle},
};

pub enum Work {
    Collect {
        request: SourceApplyRequest,
        recovery: PreparedSourceRecovery,
    },
    Apply,
}

#[derive(Debug)]
pub enum Outcome {
    Completed,
    Failed(SourceApplyError),
    /// The controller is retained, but this never authorizes another dispatch.
    PanicUnknown,
}

pub struct Finished {
    pub controller: SourceApplyController,
    pub outcome: Outcome,
}

pub struct StartError {
    pub error: io::Error,
    /// Recovery ownership not yet handed to collect remains available on failure.
    pub work: Work,
}

#[must_use = "poll and retain Finished; dropping a Job only joins, never saves evidence"]
pub struct Job {
    worker: OwnedWorker<SourceApplyController, Work, SourceApplyError>,
}

#[allow(
    clippy::result_large_err,
    reason = "Return recovery ownership without another allocation when thread creation fails"
)]
pub fn start(
    controller: SourceApplyController,
    work: Work,
) -> Result<Job, (SourceApplyController, StartError)> {
    #[cfg(test)]
    let pause = NEXT_PAUSE.with(|slot| slot.borrow_mut().take());
    OwnedWorker::start(
        "polaris-source-apply",
        controller,
        work,
        move |controller, work| {
            #[cfg(test)]
            if let Some((entered, release)) = pause {
                let _ = entered.send(());
                let _ = release.recv();
            }
            match work {
                Work::Collect { request, recovery } => {
                    controller.collect(request, recovery).map(|_| ())
                }
                Work::Apply => controller.apply().map(|_| ()),
            }
        },
    )
    .map(|worker| Job { worker })
    .map_err(|(controller, work, error)| (controller, StartError { error, work }))
}

impl Job {
    /// Nonblocking until the OS thread is finished; returns ownership exactly once.
    pub fn poll(&mut self) -> Option<Finished> {
        self.worker.poll().map(|(controller, outcome)| Finished {
            controller,
            outcome: match outcome {
                WorkerOutcome::Completed => Outcome::Completed,
                WorkerOutcome::Failed(error) => Outcome::Failed(error),
                WorkerOutcome::PanicUnknown => Outcome::PanicUnknown,
            },
        })
    }
}

// Private test seam: it exercises the real spawning, escrow, unwind and join
// mechanics without manufacturing a trusted run-completion capability.
enum WorkerOutcome<E> {
    Completed,
    Failed(E),
    PanicUnknown,
}
struct Slot<T, W, E> {
    input: Option<(T, W)>,
    output: Option<(T, WorkerOutcome<E>)>,
}
struct OwnedWorker<T, W, E> {
    handle: Option<JoinHandle<()>>,
    slot: Arc<Mutex<Slot<T, W, E>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Recover ownership even if an unexpected unwind poisoned escrow. Never
    // convert poison into another panic that could discard the controller.
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

impl<T: Send + 'static, W: Send + 'static, E: Send + 'static> OwnedWorker<T, W, E> {
    fn start(
        name: &str,
        controller: T,
        work: W,
        operation: impl FnOnce(&mut T, W) -> Result<(), E> + Send + 'static,
    ) -> Result<Self, (T, W, io::Error)> {
        Self::start_with(controller, work, operation, |task| {
            thread::Builder::new().name(name.into()).spawn(task)
        })
    }

    fn start_with(
        controller: T,
        work: W,
        operation: impl FnOnce(&mut T, W) -> Result<(), E> + Send + 'static,
        spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> io::Result<JoinHandle<()>>,
    ) -> Result<Self, (T, W, io::Error)> {
        let slot = Arc::new(Mutex::new(Slot {
            input: Some((controller, work)),
            output: None,
        }));
        let worker_slot = Arc::clone(&slot);
        let task = Box::new(move || {
            let input = lock(&worker_slot).input.take();
            if let Some((mut controller, work)) = input {
                let outcome =
                    match catch_unwind(AssertUnwindSafe(|| operation(&mut controller, work))) {
                        Ok(Ok(())) => WorkerOutcome::Completed,
                        Ok(Err(error)) => WorkerOutcome::Failed(error),
                        Err(payload) => {
                            // An arbitrary panic payload can itself panic on Drop.
                            // Forget only that exceptional payload, not owned evidence.
                            std::mem::forget(payload);
                            WorkerOutcome::PanicUnknown
                        }
                    };
                lock(&worker_slot).output = Some((controller, outcome));
            }
        });
        match spawn(task) {
            Ok(handle) => Ok(Self {
                handle: Some(handle),
                slot,
            }),
            Err(error) => {
                // std::Builder returning Err means the closure never started.
                // This private spawner contract is also enforced by the test stub.
                let input = lock(&slot).input.take();
                match input {
                    Some((controller, work)) => Err((controller, work, error)),
                    None => unreachable!("failed spawn cannot consume escrow"),
                }
            }
        }
    }
}
impl<T, W, E> OwnedWorker<T, W, E> {
    fn poll(&mut self) -> Option<(T, WorkerOutcome<E>)> {
        if !self.handle.as_ref()?.is_finished() {
            return None;
        }
        self.join();
        lock(&self.slot).output.take()
    }
    fn join(&mut self) {
        if let Some(handle) = self.handle.take()
            && let Err(payload) = handle.join()
        {
            std::mem::forget(payload);
            // Method panics are caught inside the worker. Preserve any
            // already deposited ownership on an unexpected outer unwind.
            if let Some((_, outcome)) = &mut lock(&self.slot).output {
                *outcome = WorkerOutcome::PanicUnknown;
            }
        }
    }
}
impl<T, W, E> Drop for OwnedWorker<T, W, E> {
    fn drop(&mut self) {
        self.join();
    }
}

#[cfg(test)]
type PauseChannels = (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>);
#[cfg(test)]
thread_local! {
    static NEXT_PAUSE: std::cell::RefCell<Option<PauseChannels>> = const { std::cell::RefCell::new(None) };
}
/// Pause only the next job started by this test's owner, never another test.
#[cfg(test)]
pub(crate) fn pause_next_job() -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
    let (entered, observed) = std::sync::mpsc::channel();
    let (release, wait) = std::sync::mpsc::channel();
    NEXT_PAUSE.with(|slot| *slot.borrow_mut() = Some((entered, wait)));
    (observed, release)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    };
    use std::time::{Duration, Instant};

    struct Evidence(Arc<AtomicBool>);
    impl Drop for Evidence {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    struct Pause {
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }
    fn paused(_: &mut Evidence, pause: Pause) -> Result<(), ()> {
        pause.entered.send(()).unwrap();
        pause.release.recv().unwrap();
        Ok(())
    }
    fn finish<T, W, E>(job: &mut OwnedWorker<T, W, E>) -> (T, WorkerOutcome<E>) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(result) = job.poll() {
                return result;
            }
            assert!(Instant::now() < deadline, "worker completion deadline");
            thread::yield_now();
        }
    }

    #[test]
    fn paused_poll_returns_none_and_finished_keeps_evidence_until_owner_drops_it() {
        let dropped = Arc::new(AtomicBool::new(false));
        let (entered_tx, entered) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let mut job = OwnedWorker::start(
            "source-io-poll-test",
            Evidence(dropped.clone()),
            Pause {
                entered: entered_tx,
                release: release_rx,
            },
            paused,
        )
        .unwrap_or_else(|_| panic!("spawn"));
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        // If poll joins prematurely this cannot return while release is held.
        let (polled_tx, polled) = mpsc::channel();
        let owner = thread::spawn(move || {
            let none = job.poll().is_none();
            polled_tx.send(none).unwrap();
            job
        });
        let observation = polled.recv_timeout(Duration::from_secs(1));
        release.send(()).unwrap();
        let mut job = owner.join().unwrap();
        assert!(observation.unwrap());
        let (evidence, outcome) = finish(&mut job);
        assert!(matches!(outcome, WorkerOutcome::Completed));
        assert!(job.poll().is_none());
        drop(job);
        assert!(!dropped.load(Ordering::SeqCst));
        drop(evidence);
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn emergency_drop_waits_for_worker_instead_of_detaching() {
        let dropped = Arc::new(AtomicBool::new(false));
        let (entered_tx, entered) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let job = OwnedWorker::start(
            "source-io-drop-test",
            Evidence(dropped.clone()),
            Pause {
                entered: entered_tx,
                release: release_rx,
            },
            paused,
        )
        .unwrap_or_else(|_| panic!("spawn"));
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        let (dropping_tx, dropping) = mpsc::channel();
        let (done_tx, done) = mpsc::channel();
        let owner = thread::spawn(move || {
            dropping_tx.send(()).unwrap();
            drop(job);
            done_tx.send(()).unwrap();
        });
        dropping.recv_timeout(Duration::from_secs(5)).unwrap();
        let before_release = done.recv_timeout(Duration::from_millis(100));
        assert!(!dropped.load(Ordering::SeqCst));
        release.send(()).unwrap();
        owner.join().unwrap();
        assert!(matches!(
            before_release,
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        done.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn method_panic_and_failure_return_owned_evidence() {
        for panics in [false, true] {
            let dropped = Arc::new(AtomicBool::new(false));
            let mut job = OwnedWorker::start(
                "source-io-failure-test",
                Evidence(dropped.clone()),
                panics,
                |_, panics| {
                    if panics {
                        panic!("synthetic method panic");
                    }
                    Err("synthetic failure")
                },
            )
            .unwrap_or_else(|_| panic!("spawn"));
            let (evidence, outcome) = finish(&mut job);
            assert!(match outcome {
                WorkerOutcome::PanicUnknown => panics,
                WorkerOutcome::Failed("synthetic failure") => !panics,
                _ => false,
            });
            assert!(!dropped.load(Ordering::SeqCst));
            drop(evidence);
            assert!(dropped.load(Ordering::SeqCst));
        }
    }

    #[test]
    fn spawn_failure_returns_controller_and_unconsumed_work() {
        let dropped = Arc::new(AtomicBool::new(false));
        let result = OwnedWorker::<Evidence, String, ()>::start_with(
            Evidence(dropped.clone()),
            "recovery ownership".into(),
            |_, _| Ok(()),
            |task| {
                drop(task);
                Err(io::Error::other("synthetic spawn failure"))
            },
        );
        let (evidence, work, error) = match result {
            Err(e) => e,
            Ok(_) => panic!("unexpected worker"),
        };
        assert_eq!(work, "recovery ownership");
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(!dropped.load(Ordering::SeqCst));
        drop(evidence);
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn poisoned_escrow_still_returns_completed_ownership() {
        let slot = Arc::new(Mutex::new(Slot::<u32, (), ()> {
            input: None,
            output: Some((42, WorkerOutcome::Completed)),
        }));
        let poison = slot.clone();
        let _ = thread::spawn(move || {
            let _guard = poison.lock().unwrap();
            panic!("synthetic poison");
        })
        .join();
        let handle = thread::spawn(|| {});
        let mut job = OwnedWorker {
            handle: Some(handle),
            slot,
        };
        assert_eq!(finish(&mut job).0, 42);
    }
}
