//! Root-only typed port. The live turn host lends its existing activity lease;
//! neither a skill nor model arguments can manufacture this binding.
use super::{
    native,
    owner::{EffectContext, Lease, Prepared},
};
use crate::{
    approval::{Approver, Gate},
    management_effect::{ManagementEffectHashV1, ManagementEffectStoreV1},
    project_mutation::ProjectActivityLeaseV1,
};
use polaris_envkit::request::Request;
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone)]
struct Binding {
    effects: ManagementEffectStoreV1,
    activity: Arc<ProjectActivityLeaseV1>,
    session: ManagementEffectHashV1,
    used: Arc<AtomicBool>,
}

pub struct EnvironmentTool {
    project: PathBuf,
    default_target: PathBuf,
    binding: Arc<Mutex<Option<Binding>>>,
}
pub struct TurnBinding {
    slot: Arc<Mutex<Option<Binding>>>,
}
impl Drop for TurnBinding {
    fn drop(&mut self) {
        if let Ok(mut slot) = self.slot.lock() {
            *slot = None;
        }
    }
}
struct CancelOnDrop(Arc<AtomicBool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

impl EnvironmentTool {
    pub fn new(project: PathBuf, default_target: PathBuf) -> Self {
        Self {
            project,
            default_target,
            binding: Arc::new(Mutex::new(None)),
        }
    }
    pub fn bind(
        &self,
        effects: ManagementEffectStoreV1,
        activity: Arc<ProjectActivityLeaseV1>,
        session_id: &str,
    ) -> Result<TurnBinding, String> {
        let mut slot = self
            .binding
            .lock()
            .map_err(|_| "Environment binding is unavailable")?;
        if slot.is_some() {
            return Err("Environment turn binding is already active".into());
        }
        *slot = Some(Binding {
            effects,
            activity,
            session: ManagementEffectHashV1::digest(session_id.as_bytes()),
            used: Arc::new(AtomicBool::new(false)),
        });
        Ok(TurnBinding {
            slot: Arc::clone(&self.binding),
        })
    }

    #[cfg(target_os = "macos")]
    pub async fn call(
        &self,
        arguments: &serde_json::Value,
        sandbox: &polaris_sandbox::SandboxPolicy,
        gate: &mut Gate,
        approver: &mut dyn Approver,
    ) -> Result<String, String> {
        let binding = self
            .binding
            .lock()
            .map_err(|_| "Environment binding is unavailable")?
            .clone()
            .ok_or("The host has not lent an environment execution scope")?;
        if binding
            .used
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err("Only one environment tool invocation is permitted per root turn".into());
        }
        gate.environment_confirmation_available()?;
        let mut request: Request = serde_json::from_value(arguments.clone())
            .map_err(|_| "Invalid typed environment request")?;
        request.validate().map_err(|error| error.to_string())?;
        let target = request.target.as_ref().map_or_else(
            || self.default_target.clone(),
            |path| self.default_target.join(path),
        );
        let target = target
            .canonicalize()
            .map_err(|_| "Environment target is unavailable")?;
        if !target.starts_with(&self.project) || !sandbox.contains(&target) {
            return Err("Environment target exceeds the parent workspace scope".into());
        }
        request.target = Some(
            target
                .to_str()
                .ok_or("Invalid environment target path")?
                .into(),
        );
        let project = self.project.clone();
        let state = binding.effects.state_dir().to_owned();
        let profile = request.profile;
        let cancelled = Arc::new(AtomicBool::new(false));
        let _cancel = CancelOnDrop(Arc::clone(&cancelled));
        let preparing_cancel = Arc::clone(&cancelled);
        let held = binding.clone();
        let mut prepared = tokio::task::spawn_blocking(move || {
            let _held = held;
            Prepared::prepare(
                &project,
                &state,
                &request,
                native::capture_with_cancel(profile, || preparing_cancel.load(Ordering::Acquire))?,
            )
        })
        .await
        .map_err(|_| "Environment preparation worker failed")??;
        gate.confirm_environment(prepared.description()?, approver)
            .await?;
        let digest = prepared.digest()?;
        let runtime = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            let prerequisites = prepared.plan.snapshot.host_prerequisites.clone();
            let context = EffectContext { store: &binding.effects, lease: Lease::Parent(&binding.activity), session: Some(binding.session) };
            let result = runtime.block_on(prepared.execute(&context, &digest, || native::refresh(&prerequisites, || cancelled.load(Ordering::Acquire)), || cancelled.load(Ordering::Acquire)));
            while prepared.cleanup_pending() {
                let _ = prepared.poll_cleanup();
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            result.map(|receipt| serde_json::json!({ "verified": true, "plan_sha256": receipt.plan_sha256, "candidate": receipt.candidate, "selected_version": receipt.selected_version, "fallback_reason": receipt.fallback_reason, "runtime_paths": receipt.runtime_paths, "receipt": prepared.retained_root().join("receipt.json") }).to_string())
        }).await.map_err(|_| "Environment worker stopped; inspect retained state")?
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use crate::{approval::ApprovalPolicy, project_mutation::ProjectMutationOwnerV1};
    use std::{fs, os::unix::fs::DirBuilderExt};

    #[tokio::test]
    async fn a_turn_lends_one_operation_and_never_policy_cannot_be_overridden() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let project = root.join("project");
        let state = root.join("state");
        for path in [&project, &state] {
            fs::DirBuilder::new().mode(0o700).create(path).unwrap();
        }
        let mutation =
            ProjectMutationOwnerV1::at_store_root(&state.join("rewind-workspace")).unwrap();
        let lease = mutation.try_mutation().unwrap();
        let effects = ManagementEffectStoreV1::open_at_state_dir(&project, &state, &lease).unwrap();
        drop(lease);
        let activity = Arc::new(mutation.try_activity().unwrap());
        let tool = EnvironmentTool::new(project.clone(), project.clone());
        let binding = tool
            .bind(effects.clone(), Arc::clone(&activity), "synthetic-session")
            .unwrap();
        let sandbox = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::WorkspaceWrite,
            std::slice::from_ref(&project),
        )
        .unwrap();
        let mut gate = Gate::new(ApprovalPolicy::Never);
        let mut approver = crate::agent::AutoApprove;
        let request = serde_json::json!({"operation":"set","profile":"node"});
        assert!(
            tool.call(&request, &sandbox, &mut gate, &mut approver)
                .await
                .unwrap_err()
                .contains("approval")
        );
        assert!(
            tool.call(&request, &sandbox, &mut gate, &mut approver)
                .await
                .unwrap_err()
                .contains("Only one")
        );
        assert!(effects.effects().unwrap().is_empty());
        assert!(fs::read_dir(&project).unwrap().next().is_none());
        drop(binding);
        let _new_turn = tool
            .bind(effects.clone(), activity, "next-synthetic-session")
            .unwrap();
        assert!(
            tool.call(&request, &sandbox, &mut gate, &mut approver)
                .await
                .unwrap_err()
                .contains("approval")
        );
        assert!(effects.effects().unwrap().is_empty());
    }
}
