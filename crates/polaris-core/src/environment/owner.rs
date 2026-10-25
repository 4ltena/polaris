//! Shared project-bound host owner for CLI, TUI and the typed environment tool.
use super::capture;
use crate::{
    management_effect::*,
    project_mutation::{ProjectActivityLeaseV1, ProjectMutationLeaseV1},
};
use polaris_envkit::{
    adapter::{ResolvedPlan, Snapshot},
    definition::{Definition, HostPrerequisite, HostPrerequisiteId},
    journal::Journal,
    plan::TargetIdentity,
    request::Request,
    runner::{Receipt, Runner},
};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub enum Lease<'a> {
    Direct(&'a ProjectMutationLeaseV1),
    Parent(&'a ProjectActivityLeaseV1),
}
pub struct EffectContext<'a> {
    pub store: &'a ManagementEffectStoreV1,
    pub lease: Lease<'a>,
    pub session: Option<ManagementEffectHashV1>,
}

pub struct Prepared {
    pub definition: Definition,
    pub plan: ResolvedPlan,
    root: PathBuf,
    project: PathBuf,
    journal: Journal,
    runner: Runner,
}

impl Prepared {
    pub fn load(project: &Path, state: &Path, root: &Path) -> Result<Self, String> {
        let (definition, plan, journal) = super::recovery::load(project, state, root)?;
        if journal.progress().stage != polaris_envkit::Stage::Prepared {
            return Err(
                "This operation has started; use environment recovery without repeating effects"
                    .into(),
            );
        }
        Ok(Self {
            definition,
            plan,
            root: root.into(),
            project: project.into(),
            journal,
            runner: Runner::default(),
        })
    }
    pub fn prepare(
        project: &Path,
        state: &Path,
        request: &Request,
        host: BTreeMap<HostPrerequisiteId, HostPrerequisite>,
    ) -> Result<Self, String> {
        request.validate().map_err(text)?;
        let project = project.canonicalize().map_err(text)?;
        let requested = request
            .target
            .as_ref()
            .map_or_else(|| project.clone(), |path| project.join(path));
        let target = requested.canonicalize().map_err(text)?;
        if !target.starts_with(&project) {
            return Err("Environment target is outside the project scope".into());
        }
        let definition = polaris_envkit::bundled::load(request.profile).map_err(text)?;
        use std::os::unix::fs::PermissionsExt;
        let private = tempfile::Builder::new()
            .prefix("envkit-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in(state)
            .map_err(text)?;
        let root = private.keep();
        let image = capture::inspect(&definition, &target).map_err(text)?;
        let snapshot = Snapshot {
            runner_binary_sha256: binary_digest()?,
            host_os_revision: os_revision()?,
            target: image.target,
            private_root: identity(&root)?,
            platform: platform().into(),
            existing_project: fs::read_dir(&target).map_err(text)?.next().is_some(),
            inputs: image.inputs,
            installations: capture::installations(&definition, &target)?,
            host_prerequisites: host,
        };
        if snapshot.target.device != snapshot.private_root.device {
            return Err("Environment staging and target must use the same filesystem".into());
        }
        let plan = polaris_envkit::adapter::resolve(
            &definition,
            &request.normalized_version().map_err(text)?,
            &snapshot,
        )
        .map_err(text)?;
        let digest = plan.digest(&definition).map_err(text)?;
        save(
            &root.join("plan.json"),
            &serde_json::to_vec(&plan).map_err(text)?,
        )?;
        save(
            &root.join("definition.json"),
            &serde_json::to_vec(&definition).map_err(text)?,
        )?;
        save(
            &root.join("owner.json"),
            &serde_json::to_vec(&ManagementEffectHashV1::digest(
                project.as_os_str().as_encoded_bytes(),
            ))
            .map_err(text)?,
        )?;
        let journal = Journal::create(&root, &digest, plan.candidates[0].index).map_err(text)?;
        Ok(Self {
            definition,
            plan,
            root,
            project,
            journal,
            runner: Runner::default(),
        })
    }

    pub fn retained_root(&self) -> &Path {
        &self.root
    }

    pub fn description(&self) -> Result<String, String> {
        let first = self
            .plan
            .candidates
            .first()
            .ok_or("No environment candidate")?;
        let tuple = &self.definition.candidates[first.index];
        let total: u64 = tuple
            .artifacts
            .values()
            .map(|artifact| artifact.bytes)
            .sum();
        let outputs: Vec<_> = first
            .paths
            .publish_target
            .values()
            .map(|binding| binding.relative.as_str())
            .collect();
        Ok(format!(
            "Environment: {} {} (unverified until this operation finishes)\nTarget: {}\nArtifacts: {} / {} bytes; credential-free pinned HTTPS\nOrigins: {}\nOffline execution; private runtime/home/cache; no global changes\nCreate-only outputs: {}\nPlan SHA-256: {}\nFull plan: {}\nFixed resources: {}\nRetained state: {}",
            self.definition.profile.entry().name,
            tuple.version,
            self.plan.snapshot.target.canonical_path,
            tuple.artifacts.len(),
            total,
            self.definition.origins.join(", "),
            outputs.join(", "),
            self.digest()?,
            self.root.join("plan.json").display(),
            self.root.join("definition.json").display(),
            self.root.display()
        ))
    }
    pub fn digest(&self) -> Result<String, String> {
        self.plan.digest(&self.definition).map_err(text)
    }
    pub fn cleanup_pending(&self) -> bool {
        self.runner.cleanup_pending()
    }
    pub fn poll_cleanup(&mut self) -> Result<bool, String> {
        self.runner.poll_cleanup().map_err(text)
    }

    /// The host invokes this on its retained worker after one plan-bound
    /// confirmation. Parent leases are borrowed, never upgraded or reacquired.
    #[cfg(target_os = "macos")]
    pub async fn execute(
        &mut self,
        context: &EffectContext<'_>,
        accepted_digest: &str,
        mut host: impl FnMut() -> Result<BTreeMap<HostPrerequisiteId, HostPrerequisite>, String>,
        cancelled: impl FnMut() -> bool + Send,
    ) -> Result<Receipt, String> {
        let digest = self.digest()?;
        if accepted_digest != digest
            || context.store.project_scope_hash()
                != ManagementEffectHashV1::digest(self.project.as_os_str().as_encoded_bytes())
        {
            return Err("Environment plan binding changed".into());
        }
        let cursor = context.store.cursor().map_err(text)?;
        let request = PrepareManagementEffectV1 {
            operation_id: ManagementOperationIdV1::random().map_err(text)?,
            inventory: ManagementInventoryIdV1::parse("C43").map_err(text)?,
            scope: ManagementEffectScopeV1 {
                project: context.store.project_scope_hash(),
                session: context.session,
                generation: cursor.generation(),
                revision: 0,
                permission: ManagementPermissionScopeV1::Project,
                account_binding: None,
            },
            request_hash: ManagementEffectHashV1::from_hex(&digest).map_err(text)?,
            rewind: ManagementRewindDispositionV1::IrreversibleRemote,
        };
        let prepared = match &context.lease {
            Lease::Direct(lease) => context.store.prepare(lease, cursor, request),
            Lease::Parent(lease) => context
                .store
                .prepare_environment_runtime(lease, cursor, request),
        }
        .map_err(text)?;
        let PrepareManagementEffectOutcomeV1::Prepared(prepared) = prepared else {
            return Err("Environment operation already exists; inspect recovery status".into());
        };
        let current = host()
            .and_then(|prerequisites| snapshot(&self.definition, &self.plan, prerequisites))
            .and_then(|fresh| {
                polaris_envkit::adapter::revalidate(&self.definition, &self.plan, &fresh)
                    .map_err(text)
            });
        if let Err(error) = current {
            match &context.lease {
                Lease::Direct(lease) => {
                    context
                        .store
                        .deny(lease, &prepared, ManagementDenialReasonV1::StaleRequest)
                }
                Lease::Parent(lease) => context.store.deny_environment_runtime(
                    lease,
                    &prepared,
                    ManagementDenialReasonV1::StaleRequest,
                ),
            }
            .map_err(text)?;
            return Err(error);
        }
        let started = match &context.lease {
            Lease::Direct(lease) => context.store.start(lease, &prepared),
            Lease::Parent(lease) => context.store.start_environment_runtime(lease, &prepared),
        }
        .map_err(text)?;
        let result = self.execute_started(&digest, &mut host, cancelled).await;
        match result {
            Ok(receipt) => {
                let evidence =
                    ManagementEffectHashV1::digest(&serde_json::to_vec(&receipt).map_err(text)?);
                match &context.lease {
                    Lease::Direct(lease) => {
                        context.store.complete_started(lease, &started, evidence)
                    }
                    Lease::Parent(lease) => context
                        .store
                        .complete_environment_runtime(lease, &started, evidence),
                }
                .map_err(text)?;
                Ok(receipt)
            }
            Err(error) => {
                let cleanup_deadline =
                    std::time::Instant::now() + std::time::Duration::from_secs(5);
                while self.runner.cleanup_pending() && std::time::Instant::now() < cleanup_deadline
                {
                    if self.runner.poll_cleanup().is_err() {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                }
                let known_local_failure = !self.runner.cleanup_pending()
                    && matches!(
                        self.journal.progress().stage,
                        polaris_envkit::Stage::Prepared
                            | polaris_envkit::Stage::Started
                            | polaris_envkit::Stage::Staged
                    )
                    && Journal::open(&self.root, &digest).is_ok_and(|observed| {
                        observed.progress().stage == self.journal.progress().stage
                    });
                if known_local_failure {
                    let failure = serde_json::to_vec(&serde_json::json!({ "schema":2, "plan_sha256":digest, "outcome":"failed", "cleanup_confirmed":true, "reason":error })).map_err(text)?;
                    save(&self.root.join("failure.json"), &failure)?;
                    let evidence = ManagementEffectHashV1::digest(&failure);
                    match &context.lease {
                        Lease::Direct(lease) => {
                            context.store.complete_started(lease, &started, evidence)
                        }
                        Lease::Parent(lease) => context
                            .store
                            .complete_environment_runtime(lease, &started, evidence),
                    }
                    .map_err(text)?;
                    return Err(format!(
                        "{error}; failed without publication; retained operation: {}",
                        self.root.display()
                    ));
                }
                // Retain partial publication and process ownership. A future
                // invocation must inspect the receipt, never repeat effects.
                match &context.lease {
                    Lease::Direct(lease) => context.store.mark_outcome_unknown(
                        lease,
                        &started,
                        ManagementUnknownReasonV1::InterruptedAfterStart,
                    ),
                    Lease::Parent(lease) => context.store.unknown_environment_runtime(
                        lease,
                        &started,
                        ManagementUnknownReasonV1::InterruptedAfterStart,
                    ),
                }
                .map_err(text)?;
                Err(format!(
                    "{error}; retained operation: {}",
                    self.root.display()
                ))
            }
        }
    }

    #[cfg(target_os = "macos")]
    async fn execute_started(
        &mut self,
        digest: &str,
        host: &mut impl FnMut() -> Result<BTreeMap<HostPrerequisiteId, HostPrerequisite>, String>,
        cancelled: impl FnMut() -> bool + Send,
    ) -> Result<Receipt, String> {
        let live = snapshot(&self.definition, &self.plan, host()?)?;
        polaris_envkit::adapter::revalidate(&self.definition, &self.plan, &live).map_err(text)?;
        self.journal
            .advance(polaris_envkit::Stage::Started, None)
            .map_err(text)?;
        let index = self.plan.candidates[0].index;
        let layout =
            polaris_envkit::invocation::Layout::for_plan(&self.plan, index).map_err(text)?;
        for path in [
            layout.artifacts(),
            self.root.join("candidates"),
            layout.candidate(),
            layout.stage(),
        ] {
            private_directory(&path)?;
        }
        if self.plan.snapshot.existing_project {
            capture::copy(
                &self.definition,
                &self.plan,
                &live,
                index,
                &identity(&layout.stage())?,
            )
            .map_err(text)?;
        }
        if self.digest()? != digest {
            return Err("Environment plan binding changed".into());
        }
        let definition = &self.definition;
        let plan = &self.plan;
        self.runner
            .run(
                definition,
                plan,
                &mut self.journal,
                || {
                    snapshot(
                        definition,
                        plan,
                        host().map_err(|_| polaris_envkit::Error::StalePlan)?,
                    )
                    .map_err(|_| polaris_envkit::Error::StalePlan)
                },
                cancelled,
            )
            .await
            .map_err(text)
    }
}

pub(super) fn snapshot(
    definition: &Definition,
    plan: &ResolvedPlan,
    host: BTreeMap<HostPrerequisiteId, HostPrerequisite>,
) -> Result<Snapshot, String> {
    let target = Path::new(&plan.snapshot.target.canonical_path);
    let image = capture::inspect(definition, target).map_err(text)?;
    let root = Path::new(&plan.snapshot.private_root.canonical_path);
    if identity(root)? != plan.snapshot.private_root {
        return Err("Environment operation identity changed".into());
    }
    let journal = Journal::open(root, &plan.digest(definition).map_err(text)?).map_err(text)?;
    let mut published = BTreeMap::new();
    if matches!(
        journal.progress().stage,
        polaris_envkit::Stage::PublishIntent
            | polaris_envkit::Stage::Published
            | polaris_envkit::Stage::Verified
    ) {
        let bytes = super::recovery::read_private(&root.join("postimages.json"), 32 * 1024 * 1024)?;
        if journal.progress().inventory_sha256.as_deref()
            != Some(format!("{:x}", Sha256::digest(&bytes)).as_str())
        {
            return Err("Published inventory binding changed".into());
        }
        published = serde_json::from_slice(&bytes).map_err(text)?;
        // Existing installations retain the original strict admission policy.
        for path in plan.snapshot.installations.keys() {
            published.remove(path);
        }
    }
    Ok(Snapshot {
        runner_binary_sha256: binary_digest()?,
        host_os_revision: os_revision()?,
        target: image.target,
        private_root: identity(Path::new(&plan.snapshot.private_root.canonical_path))?,
        platform: platform().into(),
        existing_project: fs::read_dir(target).map_err(text)?.next().is_some(),
        inputs: image.inputs,
        installations: capture::installations_with_postimages(definition, target, &published)?,
        host_prerequisites: host,
    })
}

fn platform() -> &'static str {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "darwin-arm64"
    } else {
        "unsupported"
    }
}
fn identity(path: &Path) -> Result<TargetIdentity, String> {
    use std::os::unix::fs::MetadataExt;
    let (file, _) =
        polaris_tools::path_policy::tree_admission::open_root_with_ancestors(path).map_err(text)?;
    let metadata = file.metadata().map_err(text)?;
    Ok(TargetIdentity {
        canonical_path: path.to_str().ok_or("Invalid environment path")?.into(),
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}
pub(super) fn binary_digest() -> Result<String, String> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = std::env::current_exe().map_err(text)?;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(text)?;
    let mut bytes = Vec::new();
    file.take(512 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(text)?;
    if bytes.len() > 512 * 1024 * 1024 {
        return Err("Environment runner exceeds the executable size limit".into());
    }
    Ok(format!("{:x}", Sha256::digest(&bytes)))
}
fn private_directory(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .map_err(text)?;
    File::open(path.parent().ok_or("Invalid environment state path")?)
        .and_then(|file| file.sync_all())
        .map_err(text)
}
fn save(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(path)
        .map_err(text)?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(text)?;
    File::open(path.parent().ok_or("Invalid environment state path")?)
        .and_then(|file| file.sync_all())
        .map_err(text)
}
fn text(error: impl std::fmt::Display) -> String {
    error.to_string()
}

pub(super) fn os_revision() -> Result<String, String> {
    let bytes = fs::read("/System/Library/CoreServices/SystemVersion.plist").map_err(text)?;
    if bytes.len() > 65536 {
        return Err("Invalid host OS identity".into());
    }
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use polaris_envkit::{catalog::ProfileId, request::Operation};
    use std::os::unix::fs::DirBuilderExt;

    #[test]
    fn generated_installation_admission_is_bound_to_the_journal_inventory_hash() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let project = root.join("project");
        let state = root.join("state");
        for path in [&project, &state] {
            fs::DirBuilder::new().mode(0o700).create(path).unwrap();
        }
        let request = Request {
            operation: Operation::Set,
            profile: ProfileId::Python,
            target: None,
            version: None,
        };
        let mut prepared = Prepared::prepare(&project, &state, &request, BTreeMap::new()).unwrap();
        let venv = project.join(".venv");
        fs::create_dir(&venv).unwrap();
        fs::write(
            venv.join("cacert.pem"),
            "synthetic public certificate bundle",
        )
        .unwrap();
        let image =
            polaris_envkit::publication::inspect(&venv, &prepared.definition.limits).unwrap();
        let published = BTreeMap::from([(".venv".to_string(), image)]);
        let bytes = serde_json::to_vec(&published).unwrap();
        save(&prepared.root.join("postimages.json"), &bytes).unwrap();
        prepared
            .journal
            .advance(polaris_envkit::Stage::Started, None)
            .unwrap();
        let digest = format!("{:x}", Sha256::digest(&bytes));
        prepared
            .journal
            .advance(polaris_envkit::Stage::Staged, Some(digest.clone()))
            .unwrap();
        prepared
            .journal
            .advance(polaris_envkit::Stage::PublishIntent, Some(digest))
            .unwrap();
        let current = snapshot(&prepared.definition, &prepared.plan, BTreeMap::new()).unwrap();
        polaris_envkit::runner::validate_publishing_snapshot(&prepared.plan, &current, &published)
            .unwrap();
        fs::write(prepared.root.join("postimages.json"), b"{}").unwrap();
        assert!(snapshot(&prepared.definition, &prepared.plan, BTreeMap::new()).is_err());
    }

    #[test]
    fn saved_plan_is_project_bound_and_started_work_is_never_reloaded_for_execution() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let project = root.join("project");
        let other = root.join("other");
        let state = root.join("state");
        for path in [&project, &other, &state] {
            fs::DirBuilder::new().mode(0o700).create(path).unwrap();
        }
        let request = Request {
            operation: Operation::Set,
            profile: ProfileId::Node,
            target: None,
            version: None,
        };
        let mut prepared = Prepared::prepare(&project, &state, &request, BTreeMap::new()).unwrap();
        let restored = Prepared::load(&project, &state, prepared.retained_root()).unwrap();
        assert_eq!(restored.digest().unwrap(), prepared.digest().unwrap());
        assert!(Prepared::load(&other, &state, prepared.retained_root()).is_err());
        assert!(fs::read_dir(&project).unwrap().next().is_none());
        fs::write(project.join("user.txt"), "changed after consent").unwrap();
        let current = snapshot(&prepared.definition, &prepared.plan, BTreeMap::new()).unwrap();
        assert!(
            polaris_envkit::adapter::revalidate(&prepared.definition, &prepared.plan, &current)
                .is_err()
        );
        prepared
            .journal
            .advance(polaris_envkit::Stage::Started, None)
            .unwrap();
        assert!(Prepared::load(&project, &state, prepared.retained_root()).is_err());
        assert_eq!(
            fs::read(project.join("user.txt")).unwrap(),
            b"changed after consent"
        );
    }

    #[test]
    fn existing_dependencies_are_cas_bound_without_changing_the_manifest_or_user_files() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let project = root.join("project");
        let state = root.join("state");
        for path in [&project, &state] {
            fs::DirBuilder::new().mode(0o700).create(path).unwrap();
        }
        let definition = polaris_envkit::bundled::load(ProfileId::Nextjs).unwrap();
        for template in definition.candidates[0].templates.values() {
            let path = project.join(&template.relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, &template.content).unwrap();
        }
        fs::create_dir(project.join("node_modules")).unwrap();
        fs::write(project.join("node_modules/synthetic.txt"), "before").unwrap();
        let request = Request {
            operation: Operation::Set,
            profile: ProfileId::Nextjs,
            target: None,
            version: None,
        };
        let prepared = Prepared::prepare(&project, &state, &request, BTreeMap::new()).unwrap();
        assert_eq!(prepared.plan.snapshot.installations.len(), 1);
        assert!(
            prepared.plan.candidates[0]
                .paths
                .publish_target
                .values()
                .all(|binding| binding.before_sha256.is_some())
        );
        let manifest = fs::read(project.join("package.json")).unwrap();
        fs::write(project.join("node_modules/synthetic.txt"), "after").unwrap();
        let current = snapshot(&prepared.definition, &prepared.plan, BTreeMap::new()).unwrap();
        assert!(
            polaris_envkit::adapter::revalidate(&prepared.definition, &prepared.plan, &current)
                .is_err()
        );
        assert_eq!(manifest, fs::read(project.join("package.json")).unwrap());
        fs::write(project.join("node_modules/.npmrc"), "synthetic-only").unwrap();
        assert!(capture::installations(&definition, &project).is_err());
    }
}
