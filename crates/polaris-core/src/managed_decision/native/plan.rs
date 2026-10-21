//! Fresh consent plans bind the package, sources, destination and inventory.
use super::*;
use std::path::Path;

pub(in crate::managed_decision) fn registered_profiles() -> Result<Vec<String>, ManagedDecisionError>
{
    let lock: ApprovedLock =
        serde_json::from_slice(LOCK).map_err(|_| ManagedDecisionError::Integrity)?;
    if lock.schema_version != 1 || lock.profiles.len() > 64 {
        return Err(ManagedDecisionError::Integrity);
    }
    let mut ids = BTreeSet::new();
    for p in lock.profiles {
        match select(LOCK, &p.subject.id) {
            Ok(s) => {
                ids.insert(s.id);
            }
            Err(ManagedDecisionError::UnverifiedProfile) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(ids.into_iter().collect())
}

pub(in crate::managed_decision) fn prepare(
    profile: &str,
    worker: &Path,
    model: &Path,
) -> Result<ImportPlan, ManagedDecisionError> {
    select_profile(LOCK, profile)?;
    let home = crate::os_home::trusted_user_home().map_err(|_| ManagedDecisionError::Install)?;
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).map_err(|_| ManagedDecisionError::Install)?;
    let nonce = nonce.iter().map(|b| format!("{b:02x}")).collect::<String>();
    make(LOCK, profile, &home, worker, model, &nonce)
}

fn make(
    bytes: &[u8],
    profile: &str,
    home: &Path,
    worker: &Path,
    model: &Path,
    nonce: &str,
) -> Result<ImportPlan, ManagedDecisionError> {
    let p = select_profile(bytes, profile)?;
    let delivery = p
        .delivery
        .ok_or(ManagedDecisionError::MissingDeliveryMetadata)?;
    validate_delivery(&delivery)?;
    if matches!(p.subject.device, Device::Metal) {
        return Err(ManagedDecisionError::UnsupportedDevice);
    }
    for path in [home, worker, model] {
        let name = path.to_str().ok_or(ManagedDecisionError::Integrity)?;
        if !path.is_absolute()
            || name.len() > 4096
            || name.chars().any(char::is_control)
            || path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(ManagedDecisionError::Integrity);
        }
    }
    let s = p.subject;
    let installed_bytes = s
        .worker
        .bytes
        .checked_add(s.model.bytes)
        .ok_or(ManagedDecisionError::Integrity)?;
    Ok(ImportPlan {
        transaction_id: nonce.into(),
        app_version: env!("CARGO_PKG_VERSION").into(),
        manifest_sha256: format!("{:x}", Sha256::digest(bytes)),
        subject_sha256: p.acceptance.subject_sha256,
        profile: s.id,
        destination: home
            .join(".polaris/decision-runtime/generations")
            .join(&s.generation),
        generation: s.generation,
        platform: s.platform,
        architecture: s.architecture,
        model_repository: s.model_repository,
        model_revision: s.model_revision,
        quantization: s.quantization,
        runtime_revision: s.runtime_revision,
        device: "cpu".into(),
        context_tokens: s.context_tokens,
        peak_bytes: s.peak_bytes,
        reserve_bytes: s.reserve_bytes,
        worker: ImportFile {
            source: worker.to_owned(),
            bytes: s.worker.bytes,
            sha256: s.worker.sha256,
        },
        model: ImportFile {
            source: model.to_owned(),
            bytes: s.model.bytes,
            sha256: s.model.sha256,
        },
        installed_bytes,
        copy_reserve_bytes: 256 * 1024 * 1024,
        delivery,
    })
}

pub(in crate::managed_decision) fn execute(
    plan: ImportPlan,
    consent: &str,
    cancelled: impl FnMut() -> bool,
) -> Result<(), ManagedDecisionError> {
    execute_with(LOCK, plan, consent, cancelled, || {
        crate::os_home::trusted_user_home().map_err(|_| ManagedDecisionError::Install)
    })
}

fn execute_with(
    bytes: &[u8],
    plan: ImportPlan,
    consent: &str,
    cancelled: impl FnMut() -> bool,
    home: impl FnOnce() -> Result<PathBuf, ManagedDecisionError>,
) -> Result<(), ManagedDecisionError> {
    if !digest(consent) || plan.consent_sha256()? != consent {
        return Err(ManagedDecisionError::PlanChanged);
    }
    let home = home()?;
    let fresh = make(
        bytes,
        &plan.profile,
        &home,
        &plan.worker.source,
        &plan.model.source,
        &plan.transaction_id,
    )?;
    if fresh.consent_sha256()? != consent {
        return Err(ManagedDecisionError::PlanChanged);
    }
    let subject = select(bytes, &plan.profile)?;
    super::install::import(
        &subject,
        &home,
        &plan.worker.source,
        &plan.model.source,
        cancelled,
        || Ok(()),
    )
}

fn validate_delivery(d: &DeliveryMetadata) -> Result<(), ManagedDecisionError> {
    let url = |s: &str| {
        if s.len() > 2048 || s.chars().any(char::is_control) {
            return false;
        }
        let Ok(u) = url::Url::parse(s) else {
            return false;
        };
        u.scheme() == "https"
            && u.host_str().is_some()
            && u.username().is_empty()
            && u.password().is_none()
            && u.query().is_none()
            && u.fragment().is_none()
    };
    if !text(&d.purpose)
        || !text(&d.license)
        || d.components.is_empty()
        || d.components.len() > 64
        || ![
            &d.worker_source,
            &d.model_source,
            &d.license_source,
            &d.notice_source,
        ]
        .into_iter()
        .all(|s| url(s))
        || !d
            .components
            .iter()
            .all(|c| text(&c.name) && text(&c.version) && text(&c.license) && url(&c.source))
    {
        return Err(ManagedDecisionError::MissingDeliveryMetadata);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
