//! Confirmed package downloads feed verified private files into atomic import.
use super::*;
use polaris_http::artifact::{self, ArtifactSource};
use std::{
    io::Write,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

mod staging;

const TRANSACTION_TIMEOUT: Duration = Duration::from_secs(7200);

pub(in crate::managed_decision) fn prepare(
    profile: &str,
    repair: bool,
) -> Result<DownloadPlan, ManagedDecisionError> {
    // An unaccepted profile may not inspect home or create an installer.
    select_profile(LOCK, profile)?;
    let home = crate::os_home::trusted_user_home().map_err(|_| ManagedDecisionError::Install)?;
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).map_err(|_| ManagedDecisionError::Install)?;
    let nonce = nonce.iter().map(|b| format!("{b:02x}")).collect::<String>();
    make(LOCK, profile, &home, &nonce, repair)
}

fn make(
    bytes: &[u8],
    profile: &str,
    home: &Path,
    nonce: &str,
    repair: bool,
) -> Result<DownloadPlan, ManagedDecisionError> {
    if nonce.len() != 32 || !nonce.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ManagedDecisionError::Integrity);
    }
    let directory = home
        .join(".polaris/decision-runtime/downloads")
        .join(format!(".pending-{nonce}"));
    let import = plan::make_operation(
        bytes,
        profile,
        home,
        &directory.join("worker"),
        &directory.join("model.gguf"),
        nonce,
        if repair {
            ImportOperation::Repair
        } else {
            ImportOperation::Import
        },
    )?;
    let downloads = import
        .delivery
        .downloads
        .as_ref()
        .ok_or(ManagedDecisionError::MissingDownloadMetadata)?;
    for (source, expected) in [
        (&downloads.worker, &import.worker),
        (&downloads.model, &import.model),
    ] {
        source_for(source, expected.bytes)?;
    }
    let disk_required_bytes = import
        .installed_bytes
        .checked_mul(2)
        .and_then(|n| n.checked_add(import.copy_reserve_bytes))
        .ok_or(ManagedDecisionError::Integrity)?;
    Ok(DownloadPlan {
        transfer_bytes: import.installed_bytes,
        import,
        disk_required_bytes,
        transaction_timeout_seconds: TRANSACTION_TIMEOUT.as_secs(),
    })
}

fn source_for(source: &DownloadSource, bytes: u64) -> Result<ArtifactSource, ManagedDecisionError> {
    ArtifactSource::new(&source.url, &source.redirect_origins, bytes)
        .map_err(|_| ManagedDecisionError::MissingDownloadMetadata)
}

#[async_trait::async_trait]
trait Transfer: Sync {
    async fn receive(
        &self,
        source: &ArtifactSource,
        sink: &mut (dyn for<'chunk> FnMut(&'chunk [u8]) -> Result<(), ()> + Send),
        cancelled: &mut (dyn FnMut() -> bool + Send),
    ) -> Result<(), artifact::Error>;
}

struct Http;
#[async_trait::async_trait]
impl Transfer for Http {
    async fn receive(
        &self,
        source: &ArtifactSource,
        sink: &mut (dyn for<'chunk> FnMut(&'chunk [u8]) -> Result<(), ()> + Send),
        cancelled: &mut (dyn FnMut() -> bool + Send),
    ) -> Result<(), artifact::Error> {
        artifact::fetch(source, sink, cancelled).await
    }
}

pub(in crate::managed_decision) async fn execute(
    plan: DownloadPlan,
    consent: &str,
    cancelled: Arc<AtomicBool>,
) -> Result<(), ManagedDecisionError> {
    execute_with(LOCK, plan, consent, cancelled, &Http, || {
        crate::os_home::trusted_user_home().map_err(|_| ManagedDecisionError::Install)
    })
    .await
}

async fn execute_with(
    bytes: &[u8],
    plan: DownloadPlan,
    consent: &str,
    cancelled: Arc<AtomicBool>,
    transfer: &impl Transfer,
    home: impl FnOnce() -> Result<PathBuf, ManagedDecisionError>,
) -> Result<(), ManagedDecisionError> {
    execute_with_deadline(
        bytes,
        plan,
        consent,
        cancelled,
        transfer,
        home,
        TRANSACTION_TIMEOUT,
    )
    .await
}

async fn execute_with_deadline(
    bytes: &[u8],
    plan: DownloadPlan,
    consent: &str,
    cancelled: Arc<AtomicBool>,
    transfer: &impl Transfer,
    home: impl FnOnce() -> Result<PathBuf, ManagedDecisionError>,
    timeout: Duration,
) -> Result<(), ManagedDecisionError> {
    let mut scope = CancelOnDrop {
        flag: cancelled.clone(),
        finished: false,
    };
    let operation = execute_checked(bytes, plan, consent, cancelled.clone(), transfer, home);
    tokio::pin!(operation);
    let result = tokio::select! {
        result = &mut operation => result,
        _ = tokio::time::sleep(timeout) => {
            cancelled.store(true, Ordering::Release);
            // Keep ownership until the same network/copy operation settles.
            // Never detach a publisher or begin another download after timeout.
            let _ = operation.await;
            Err(ManagedDecisionError::DownloadTimeout)
        }
    };
    scope.finished = true;
    result
}

// Dropping the asynchronous owner must also cancel an already-started blocking
// copy. Dropping its JoinHandle alone would let that copy publish unattended.
struct CancelOnDrop {
    flag: Arc<AtomicBool>,
    finished: bool,
}
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if !self.finished {
            self.flag.store(true, Ordering::Release);
        }
    }
}

async fn execute_checked(
    bytes: &[u8],
    plan: DownloadPlan,
    consent: &str,
    cancelled: Arc<AtomicBool>,
    transfer: &impl Transfer,
    home: impl FnOnce() -> Result<PathBuf, ManagedDecisionError>,
) -> Result<(), ManagedDecisionError> {
    use ManagedDecisionError::{Cancelled, Integrity, PlanChanged};
    if !digest(consent) || plan.consent_sha256()? != consent {
        return Err(PlanChanged);
    }
    if cancelled.load(Ordering::Acquire) {
        return Err(Cancelled);
    }
    let home = home()?;
    let fresh = make(
        bytes,
        &plan.import.profile,
        &home,
        &plan.import.transaction_id,
        plan.import.operation == ImportOperation::Repair,
    )?;
    if fresh.consent_sha256()? != consent {
        return Err(PlanChanged);
    }
    let subject = select(bytes, &plan.import.profile)?;
    let directory = plan.import.worker.source.parent().ok_or(Integrity)?;
    let stage = staging::Staging::new(&home, directory, plan.disk_required_bytes)?;
    // Check conflicts before any network request; import repeats this under its
    // management lock at publication, so a late competing install is retained.
    if plan.import.replacement.is_none() && plan.import.destination.symlink_metadata().is_ok() {
        return Err(ManagedDecisionError::Conflict);
    }
    let downloads = plan
        .import
        .delivery
        .downloads
        .as_ref()
        .ok_or(ManagedDecisionError::MissingDownloadMetadata)?;
    for (name, source, expected) in [
        ("worker", &downloads.worker, &subject.worker),
        ("model.gguf", &downloads.model, &subject.model),
    ] {
        stage.verify()?;
        if cancelled.load(Ordering::Acquire) {
            return Err(Cancelled);
        }
        let source = source_for(source, expected.bytes)?;
        let mut file = stage.create(name)?;
        let mut hash = Sha256::new();
        let mut received = 0u64;
        let mut sink_error = None;
        let mut sink = |chunk: &[u8]| {
            received = received.checked_add(chunk.len() as u64).ok_or(())?;
            if received > expected.bytes {
                sink_error = Some(Integrity);
                return Err(());
            }
            if file.write_all(chunk).is_err() {
                sink_error = Some(ManagedDecisionError::Install);
                return Err(());
            }
            hash.update(chunk);
            Ok(())
        };
        let result = transfer
            .receive(&source, &mut sink, &mut || {
                cancelled.load(Ordering::Acquire)
            })
            .await;
        if cancelled.load(Ordering::Acquire) {
            return Err(Cancelled);
        }
        if let Some(error) = sink_error {
            return Err(error);
        }
        result.map_err(|error| {
            if error == artifact::Error::Cancelled {
                Cancelled
            } else {
                ManagedDecisionError::Download
            }
        })?;
        if received != expected.bytes || format!("{:x}", hash.finalize()) != expected.sha256 {
            return Err(Integrity);
        }
        file.sync_all().map_err(|_| ManagedDecisionError::Install)?;
    }
    // Hashing/copying large local files must not block the asynchronous owner.
    tokio::task::spawn_blocking(move || {
        stage.verify()?;
        install::import(
            &subject,
            &home,
            &plan.import.worker.source,
            &plan.import.model.source,
            plan.import.replacement.as_ref(),
            || cancelled.load(Ordering::Acquire),
            || stage.verify(),
        )
    })
    .await
    .map_err(|_| ManagedDecisionError::Install)?
}

#[cfg(test)]
mod tests;
