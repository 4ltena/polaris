//! The common bounded runner. Hosts own consent, the project lease and C43;
//! this runner never starts a nested Polaris session or asks a model for steps.
use crate::{
    Error, Stage,
    acquisition::Acquisition,
    adapter::{self, Operation, RecipeId, ResolvedPlan, Snapshot},
    definition::Definition,
    invocation::Layout,
    journal::Journal,
    process::{ProcessOwner, ProcessReceipt},
    publication::{self, Inventory},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::Write,
    path::Path,
};

mod publication_checks;
pub use publication_checks::validate_publishing_snapshot;
#[cfg(unix)]
mod filesystem;
#[cfg(unix)]
use filesystem::{create_file, merge_runtime, private_directory, relocate_venv};

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error(transparent)]
    Plan(#[from] Error),
    #[error(transparent)]
    Transfer(#[from] crate::transfer::TransferError),
    #[error(transparent)]
    Archive(#[from] crate::archive::ArchiveError),
    #[error(transparent)]
    Process(#[from] crate::process::ProcessError),
    #[error("environment setup stopped; retained state requires inspection")]
    State,
    #[error("the locked primary package does not support this declared runtime tuple")]
    PrimaryCompatibility,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub schema: u32,
    pub plan_sha256: String,
    pub candidate: usize,
    pub selected_version: String,
    pub fallback_reason: Option<crate::Failure>,
    pub postimages: BTreeMap<String, Inventory>,
    pub runtime_paths: Vec<String>,
    pub runtime_images: BTreeMap<String, Inventory>,
    pub probes: Vec<ProcessReceipt>,
}

#[derive(Default)]
pub struct Runner {
    processes: ProcessOwner,
    process_count: u32,
}

impl Runner {
    pub fn cleanup_pending(&self) -> bool {
        self.processes.cleanup_pending()
    }
    pub fn poll_cleanup(&mut self) -> Result<bool, crate::process::ProcessError> {
        self.processes.poll_cleanup()
    }

    /// Started is already durable in both journals. The supplied snapshot
    /// callback performs live host inspection, never echoes the saved snapshot.
    #[cfg(target_os = "macos")]
    pub async fn run(
        &mut self,
        definition: &Definition,
        plan: &ResolvedPlan,
        journal: &mut Journal,
        mut fresh: impl FnMut() -> Result<Snapshot, Error>,
        mut cancelled: impl FnMut() -> bool + Send,
    ) -> Result<Receipt, RunError> {
        let digest = plan.digest(definition)?;
        let mut permit = Acquisition::accepted(definition, plan, &fresh()?, &digest)?;
        loop {
            let result = self
                .attempt(
                    definition,
                    plan,
                    journal,
                    &permit,
                    &mut fresh,
                    &mut cancelled,
                )
                .await;
            let observed = match &result {
                Err(RunError::Process(crate::process::ProcessError::VersionMismatch {
                    observed_sha256,
                })) => observed_sha256.as_bytes(),
                Err(RunError::PrimaryCompatibility) => {
                    b"locked primary runtime constraint refused this tuple"
                }
                _ => return result,
            };
            let previous = Layout::for_plan(plan, permit.candidate_index())?;
            create_file(&previous.candidate().join("version_refusal.json"), observed)?;
            let runtime = permit.fallback(
                journal,
                definition,
                plan,
                &fresh()?,
                crate::Failure::RuntimeIncompatible,
            )?;
            let next = if runtime.is_none() && matches!(result, Err(RunError::PrimaryCompatibility))
            {
                permit.fallback(
                    journal,
                    definition,
                    plan,
                    &fresh()?,
                    crate::Failure::PrimaryIncompatible,
                )?
            } else {
                runtime
            };
            let Some(next) = next else { return result };
            let layout = Layout::for_plan(plan, next.candidate_index())?;
            private_directory(&layout.candidate())?;
            private_directory(&layout.stage())?;
            if plan.snapshot.existing_project {
                for image in plan.snapshot.inputs.values() {
                    let source = previous.stage().join(&image.relative);
                    let observed = publication::inspect(&source, &definition.limits)?;
                    if !matches!(observed.entries.as_slice(), [publication::ImageEntry::File { sha256, .. }] if sha256 == &image.sha256)
                    {
                        return Err(Error::StalePlan.into());
                    }
                    let bytes = fs::read(&source).map_err(|_| RunError::State)?;
                    create_file(&layout.stage().join(&image.relative), &bytes)?;
                }
            }
            permit = next;
        }
    }

    #[cfg(target_os = "macos")]
    async fn attempt(
        &mut self,
        definition: &Definition,
        plan: &ResolvedPlan,
        journal: &mut Journal,
        permit: &Acquisition,
        mut fresh: impl FnMut() -> Result<Snapshot, Error>,
        mut cancelled: impl FnMut() -> bool + Send,
    ) -> Result<Receipt, RunError> {
        let digest = plan.digest(definition)?;
        let fallback_reason = journal.progress().compatibility_failure;
        let index = permit.candidate_index();
        if journal.progress().stage != Stage::Started
            || journal.progress().candidate != index
            || journal.progress().plan_sha256 != digest
        {
            return Err(Error::RecoveryRequired.into());
        }
        let layout = Layout::for_plan(plan, index)?;
        let candidate = plan
            .candidates
            .iter()
            .find(|candidate| candidate.index == index)
            .ok_or(Error::StalePlan)?;
        let tuple = &definition.candidates[index];
        if !tuple.primary_runtime_compatible(definition.profile)? {
            return Err(RunError::PrimaryCompatibility);
        }
        permit.fetch(&mut cancelled).await?;
        for directory in [
            layout.scratch(),
            layout.home(),
            layout.tmp(),
            layout.candidate().join("runtime"),
        ] {
            private_directory(&directory)?;
        }
        let mut completed = BTreeSet::<RecipeId>::new();
        let mut probes = Vec::new();
        let mut postimages = BTreeMap::new();
        for (position, operation) in candidate.operations.iter().enumerate() {
            if cancelled() {
                return Err(crate::process::ProcessError::Cancelled.into());
            }
            // The one acquisition above already handled these fixed resources.
            // Do not rescan the project for an operation with no further effect.
            if matches!(operation, Operation::FetchArtifact { .. }) {
                continue;
            }
            let live = fresh()?;
            if journal.progress().stage == Stage::PublishIntent {
                validate_publishing_snapshot(plan, &live, &postimages)?;
            } else {
                adapter::revalidate(definition, plan, &live)?;
            }
            match operation {
                Operation::FetchArtifact { .. } => {}
                Operation::Unpack { component } => {
                    let artifact = tuple
                        .artifacts
                        .get(&component.artifact)
                        .ok_or(Error::StalePlan)?;
                    let extracted = crate::archive::extract_component(
                        &layout.artifact(&component.artifact),
                        artifact,
                        &layout
                            .candidate()
                            .join(format!("unpack_{}", artifact.sha256)),
                        component,
                    )?;
                    merge_runtime(
                        &extracted.directory,
                        &layout.runtime(&component.directory),
                        definition.adapter == crate::catalog::AdapterId::Cargo,
                    )?;
                }
                Operation::Materialize { template, .. } => {
                    let template = tuple.templates.get(template).ok_or(Error::StalePlan)?;
                    create_file(
                        &layout.stage().join(&template.relative),
                        template.content.as_bytes(),
                    )?;
                }
                Operation::CopyInput { input } => {
                    let image = candidate
                        .paths
                        .read_input
                        .get(input)
                        .ok_or(Error::StalePlan)?;
                    let observed = publication::inspect(
                        &layout.stage().join(&image.relative),
                        &definition.limits,
                    )?;
                    if !matches!(observed.entries.as_slice(), [publication::ImageEntry::File { sha256, .. }] if sha256 == &image.sha256)
                    {
                        return Err(Error::StalePlan.into());
                    }
                }
                Operation::InstallOffline { recipe, .. } => {
                    self.process_count =
                        self.process_count.checked_add(1).ok_or(RunError::State)?;
                    if self.process_count > definition.limits.processes {
                        return Err(RunError::State);
                    }
                    probes.push(self.processes.run_planned(
                        definition,
                        plan,
                        &fresh()?,
                        index,
                        position,
                        &completed,
                        &mut cancelled,
                    )?);
                    completed.insert(*recipe);
                }
                Operation::Probe { probe } => {
                    if *probe == adapter::ProbeId::BuildOutput {
                        for output in tuple.build_outputs.values() {
                            if !fs::symlink_metadata(layout.stage().join(output)).is_ok_and(
                                |metadata| metadata.is_file() && !metadata.file_type().is_symlink(),
                            ) {
                                return Err(RunError::State);
                            }
                        }
                    } else {
                        self.process_count =
                            self.process_count.checked_add(1).ok_or(RunError::State)?;
                        if self.process_count > definition.limits.processes {
                            return Err(RunError::State);
                        }
                        probes.push(self.processes.run_planned(
                            definition,
                            plan,
                            &fresh()?,
                            index,
                            position,
                            &completed,
                            &mut cancelled,
                        )?);
                    }
                }
                Operation::StageInventory => {
                    if definition.adapter == crate::catalog::AdapterId::Python {
                        relocate_venv(
                            &layout.stage(),
                            Path::new(&plan.snapshot.target.canonical_path),
                        )?;
                    }
                    for binding in candidate.paths.publish_target.values() {
                        let image = publication::inspect(
                            &layout.stage().join(&binding.relative),
                            &definition.limits,
                        )?;
                        if binding
                            .before_sha256
                            .as_ref()
                            .is_some_and(|before| image.digest().as_ref().ok() != Some(before))
                        {
                            return Err(Error::Unsupported.into());
                        }
                        if let Some(expected) = &binding.expected_template_sha256
                            && !matches!(image.entries.as_slice(), [publication::ImageEntry::File { sha256, .. }] if sha256 == expected)
                        {
                            return Err(Error::StalePlan.into());
                        }
                        postimages.insert(binding.relative.clone(), image);
                    }
                    let bytes = serde_json::to_vec(&postimages).map_err(|_| RunError::State)?;
                    create_file(&layout.root().join("postimages.json"), &bytes)?;
                    let inventory = format!("{:x}", Sha256::digest(&bytes));
                    journal.advance(Stage::Staged, Some(inventory.clone()))?;
                    journal.advance(Stage::PublishIntent, Some(inventory))?;
                }
                Operation::Publish { target, .. } => {
                    let binding = candidate
                        .paths
                        .publish_target
                        .get(target)
                        .ok_or(Error::StalePlan)?;
                    let image = postimages.get(&binding.relative).ok_or(Error::StalePlan)?;
                    let intent = serde_json::to_vec(&(digest.as_str(), &binding.relative, image))
                        .map_err(|_| RunError::State)?;
                    create_file(
                        &layout
                            .root()
                            .join(format!("publish_{}.intent.json", target.0)),
                        &intent,
                    )?;
                    if let Some(before) = &binding.before_sha256 {
                        let current = live
                            .installations
                            .get(&binding.relative)
                            .ok_or(Error::StalePlan)?;
                        if current != before || image.digest()? != *before {
                            return Err(Error::StalePlan.into());
                        }
                    } else {
                        publication::create_only(
                            Path::new(&plan.snapshot.target.canonical_path),
                            &layout.stage(),
                            &binding.relative,
                            image,
                            &definition.limits,
                        )?;
                    }
                    create_file(
                        &layout
                            .root()
                            .join(format!("publish_{}.receipt.json", target.0)),
                        &intent,
                    )?;
                }
            }
        }
        let inventory = journal
            .progress()
            .inventory_sha256
            .clone()
            .ok_or(Error::RecoveryRequired)?;
        journal.advance(Stage::Published, Some(inventory.clone()))?;
        for (path, image) in &postimages {
            if publication::inspect(
                &Path::new(&plan.snapshot.target.canonical_path).join(path),
                &definition.limits,
            )? != *image
            {
                return Err(Error::RecoveryRequired.into());
            }
        }
        let runtime_paths: Vec<String> = candidate
            .paths
            .private_stable
            .iter()
            .map(|id| layout.runtime(id).to_string_lossy().into_owned())
            .collect();
        let runtime_images = runtime_paths
            .iter()
            .map(|path| {
                Ok((
                    path.clone(),
                    publication::inspect(Path::new(path), &definition.limits)?,
                ))
            })
            .collect::<Result<BTreeMap<_, _>, Error>>()?;
        validate_publishing_snapshot(plan, &fresh()?, &postimages)?;
        let receipt = Receipt {
            schema: 2,
            plan_sha256: digest,
            candidate: index,
            selected_version: tuple.version.clone(),
            fallback_reason,
            postimages,
            probes,
            runtime_paths,
            runtime_images,
        };
        create_file(
            &layout.root().join("receipt.json"),
            &serde_json::to_vec(&receipt).map_err(|_| RunError::State)?,
        )?;
        journal.advance(Stage::Verified, Some(inventory))?;
        Ok(receipt)
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use crate::{
        adapter::InputImage,
        catalog::ProfileId,
        definition::{ArtifactId, ReadInputId},
        plan::TargetIdentity,
    };
    use std::os::unix::fs::MetadataExt;

    struct Fixture {
        _temporary: tempfile::TempDir,
        definition: Definition,
        plan: ResolvedPlan,
        journal: Journal,
        layout: Layout,
        archive: Vec<u8>,
    }
    impl Fixture {
        fn new() -> Self {
            let temporary = tempfile::tempdir().unwrap();
            let root = temporary.path().canonicalize().unwrap();
            let target = root.join("project");
            let private = root.join("private");
            fs::create_dir(&target).unwrap();
            fs::create_dir(&private).unwrap();
            let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            let mut tar = tar::Builder::new(encoder);
            for (path, body, mode) in [
                (
                    "fixture/bin/node",
                    "#!/bin/sh\nprintf x >> .calls\nif [ \"${1-}\" = --version ]; then printf 'v24.22.1\\n'; elif [ \"${2-}\" = --version ]; then printf '11.19.1\\n'; fi\nexit 0\n",
                    0o755,
                ),
                (
                    "fixture/lib/node_modules/npm/bin/npm-cli.js",
                    "synthetic fixture; never interpreted\n",
                    0o644,
                ),
            ] {
                let mut header = tar::Header::new_gnu();
                header.set_size(body.len() as u64);
                header.set_mode(mode);
                header.set_cksum();
                tar.append_data(&mut header, path, body.as_bytes()).unwrap();
            }
            let archive = tar.into_inner().unwrap().finish().unwrap();
            let mut definition = crate::bundled::load(ProfileId::Node).unwrap();
            let tuple = &mut definition.candidates[0];
            let old = tuple.runtime[0].artifact.clone();
            let mut artifact = tuple.artifacts.remove(&old).unwrap();
            artifact.url = "https://nodejs.org/dist/envkit-synthetic-fixture.tar.gz".into();
            artifact.bytes = archive.len() as u64;
            artifact.sha256 = format!("{:x}", Sha256::digest(&archive));
            let id = ArtifactId::of(&artifact);
            tuple.artifacts.insert(id.clone(), artifact);
            tuple.runtime[0].artifact = id.clone();
            tuple.runtime[0].strip_prefix = "fixture".into();
            for program in tuple.programs.values_mut() {
                program.artifact = id.clone();
            }
            let identity = |path: &Path| {
                let metadata = fs::metadata(path).unwrap();
                TargetIdentity {
                    canonical_path: path.to_str().unwrap().into(),
                    device: metadata.dev(),
                    inode: metadata.ino(),
                }
            };
            let snapshot = Snapshot {
                runner_binary_sha256: "a".repeat(64),
                host_os_revision: "d".repeat(64),
                target: identity(&target),
                private_root: identity(&private),
                platform: "darwin-arm64".into(),
                existing_project: false,
                inputs: BTreeMap::new(),
                installations: BTreeMap::new(),
                host_prerequisites: BTreeMap::new(),
            };
            let plan = adapter::resolve(
                &definition,
                &definition.profile.entry().parser.parse(None).unwrap(),
                &snapshot,
            )
            .unwrap();
            let mut journal =
                Journal::create(&private, &plan.digest(&definition).unwrap(), 0).unwrap();
            journal.advance(Stage::Started, None).unwrap();
            let layout = Layout::for_plan(&plan, 0).unwrap();
            for path in [
                layout.artifacts(),
                private.join("candidates"),
                layout.candidate(),
                layout.stage(),
            ] {
                fs::create_dir(&path).unwrap();
            }
            fs::write(layout.artifact(&id), &archive).unwrap();
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
            fs::set_permissions(layout.artifacts(), fs::Permissions::from_mode(0o700)).unwrap();
            Self {
                _temporary: temporary,
                definition,
                plan,
                journal,
                layout,
                archive,
            }
        }
        fn fresh(plan: &ResolvedPlan) -> Result<Snapshot, Error> {
            let mut live = plan.snapshot.clone();
            let root = Path::new(&live.target.canonical_path);
            let mut files = fs::read_dir(root)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect::<Vec<_>>();
            files.sort();
            live.existing_project = !files.is_empty();
            for (index, path) in files.iter().enumerate() {
                live.inputs.insert(
                    ReadInputId(format!("input_{index}")),
                    InputImage {
                        relative: path.file_name().unwrap().to_str().unwrap().into(),
                        sha256: format!("{:x}", Sha256::digest(fs::read(path).unwrap())),
                    },
                );
            }
            Ok(live)
        }
    }

    #[tokio::test]
    async fn verified_cache_runs_compiled_steps_and_publishes_exact_images_without_a_model() {
        let mut fixture = Fixture::new();
        let mut runner = Runner::default();
        let receipt = runner
            .run(
                &fixture.definition,
                &fixture.plan,
                &mut fixture.journal,
                || Fixture::fresh(&fixture.plan),
                || false,
            )
            .await
            .unwrap();
        assert_eq!(fixture.journal.progress().stage, Stage::Verified);
        assert_eq!(
            receipt.probes.len(),
            fixture.plan.candidates[0].commands.len()
        );
        assert_eq!(
            fs::read(fixture.layout.stage().join(".calls"))
                .unwrap()
                .len(),
            receipt.probes.len()
        );
        for template in fixture.definition.candidates[0].templates.values() {
            assert_eq!(
                fs::read(
                    Path::new(&fixture.plan.snapshot.target.canonical_path)
                        .join(&template.relative)
                )
                .unwrap(),
                template.content.as_bytes()
            );
        }
        assert!(fixture.layout.root().join("receipt.json").is_file());
        assert!(!runner.cleanup_pending());
    }

    #[tokio::test]
    async fn damaged_cache_and_changed_source_never_reach_execution_or_publication() {
        let mut fixture = Fixture::new();
        let artifact = fixture.definition.candidates[0]
            .artifacts
            .keys()
            .next()
            .unwrap();
        fs::write(fixture.layout.artifact(artifact), b"damaged").unwrap();
        let mut runner = Runner::default();
        assert!(
            runner
                .run(
                    &fixture.definition,
                    &fixture.plan,
                    &mut fixture.journal,
                    || Fixture::fresh(&fixture.plan),
                    || false
                )
                .await
                .is_err()
        );
        assert!(
            fs::read_dir(&fixture.plan.snapshot.target.canonical_path)
                .unwrap()
                .next()
                .is_none()
        );
        assert!(!fixture.layout.stage().join(".calls").exists());
        assert_ne!(fixture.archive, b"damaged");
        let mut fixture = Fixture::new();
        let mut checks = 0;
        let result = runner
            .run(
                &fixture.definition,
                &fixture.plan,
                &mut fixture.journal,
                || {
                    checks += 1;
                    if checks == 2 {
                        fs::write(
                            Path::new(&fixture.plan.snapshot.target.canonical_path)
                                .join("user.txt"),
                            b"preserve me",
                        )
                        .unwrap();
                    }
                    Fixture::fresh(&fixture.plan)
                },
                || false,
            )
            .await;
        assert!(result.is_err());
        assert_eq!(
            fs::read(Path::new(&fixture.plan.snapshot.target.canonical_path).join("user.txt"))
                .unwrap(),
            b"preserve me"
        );
        assert!(!fixture.layout.stage().join(".calls").exists());
    }
}
