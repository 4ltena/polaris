//! Pure adapter resolution. Every operation references approved resources or
//! path capabilities. Only the common host owner performs effects.
use crate::{
    Error,
    catalog::{AdapterId, ProfileId},
    definition::*,
    plan::TargetIdentity,
    version::Constraint,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InputImage {
    pub relative: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub runner_binary_sha256: String,
    pub host_os_revision: String,
    pub target: TargetIdentity,
    pub private_root: TargetIdentity,
    pub platform: String,
    pub existing_project: bool,
    pub inputs: BTreeMap<ReadInputId, InputImage>,
    pub installations: BTreeMap<String, String>,
    pub host_prerequisites: BTreeMap<HostPrerequisiteId, HostPrerequisite>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PublishBinding {
    pub relative: String,
    /// None is create-only. Existing installation trees are never replaced.
    pub before_sha256: Option<String>,
    pub expected_template_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Paths {
    pub read_input: BTreeMap<ReadInputId, InputImage>,
    pub create_only: BTreeMap<CreatePathId, String>,
    pub publish_target: BTreeMap<PublishTargetId, PublishBinding>,
    pub private_stable: BTreeSet<PrivateStableId>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum RecipeId {
    NpmCache,
    NpmInstall,
    NpmBuild,
    PythonVenv,
    PythonInstall,
    CargoBuild,
    GoBuild,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProbeId {
    NodeVersion,
    NpmVersion,
    PythonVersion,
    PythonSmoke,
    PublishedPython,
    RustcVersion,
    CargoVersion,
    CargoSmoke,
    GoVersion,
    GoSmoke,
    BuildOutput,
    LinkerSelection,
    ClangResourceSelection,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    FetchArtifact {
        artifact: ArtifactId,
    },
    Unpack {
        component: RuntimeComponent,
    },
    Materialize {
        template: TemplateId,
        destination: CreatePathId,
    },
    CopyInput {
        input: ReadInputId,
    },
    InstallOffline {
        recipe: RecipeId,
        artifacts: Vec<ArtifactId>,
    },
    Probe {
        probe: ProbeId,
    },
    StageInventory,
    Publish {
        source: StagedImageId,
        target: PublishTargetId,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CandidatePlan {
    pub index: usize,
    pub paths: Paths,
    pub operations: Vec<Operation>,
    pub commands: BTreeMap<usize, crate::recipes::Command>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ResolvedPlan {
    pub schema: u32,
    pub profile: ProfileId,
    pub definition_sha256: String,
    pub adapter: AdapterId,
    pub adapter_revision: u32,
    pub normalized_version: Constraint,
    pub snapshot: Snapshot,
    pub candidates: Vec<CandidatePlan>,
    pub system_read_roots: Vec<String>,
    pub process_control: ProcessControl,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProcessControl {
    ManagedGroup,
}

/// Re-resolve and compare immediately before performing any effect. A loaded
/// plan or caller-supplied operation list cannot expand owner permissions.
pub fn revalidate(
    definition: &Definition,
    plan: &ResolvedPlan,
    fresh: &Snapshot,
) -> Result<(), Error> {
    if fresh != &plan.snapshot {
        return Err(Error::StalePlan);
    }
    validate_shape(definition, plan)
}

fn validate_shape(definition: &Definition, plan: &ResolvedPlan) -> Result<(), Error> {
    if &resolve(definition, &plan.normalized_version, &plan.snapshot)? != plan {
        return Err(Error::StalePlan);
    }
    Ok(())
}

pub fn resolve(
    definition: &Definition,
    constraint: &Constraint,
    snapshot: &Snapshot,
) -> Result<ResolvedPlan, Error> {
    definition.validate()?;
    validate_snapshot(snapshot)?;
    if snapshot.platform != definition.platform {
        return Err(Error::Unsupported);
    }
    if definition.adapter == AdapterId::Cargo
        && snapshot.inputs.values().any(|input| {
            input
                .relative
                .split('/')
                .any(|part| matches!(part, ".cargo" | "rust-toolchain" | "rust-toolchain.toml"))
        })
    {
        return Err(Error::Unsupported);
    }
    if snapshot
        .host_prerequisites
        .keys()
        .copied()
        .collect::<BTreeSet<_>>()
        != definition.required_host
    {
        return Err(Error::Unsupported);
    }
    let known: BTreeSet<_> = definition
        .candidates
        .iter()
        .flat_map(|tuple| tuple.installation_roots.values())
        .collect();
    if snapshot
        .installations
        .keys()
        .any(|path| !known.contains(path))
    {
        return Err(Error::StalePlan);
    }
    let mut plans = Vec::new();
    for index in definition.candidates_for(constraint, snapshot.existing_project)? {
        let tuple = &definition.candidates[index];
        let mut paths = Paths {
            read_input: snapshot.inputs.clone(),
            create_only: BTreeMap::new(),
            publish_target: BTreeMap::new(),
            private_stable: BTreeSet::new(),
        };
        let mut operations = Vec::new();
        for artifact in tuple.artifacts.keys() {
            operations.push(Operation::FetchArtifact {
                artifact: artifact.clone(),
            });
        }
        for component in &tuple.runtime {
            paths.private_stable.insert(component.directory.clone());
            operations.push(Operation::Unpack {
                component: component.clone(),
            });
        }
        if snapshot.existing_project {
            for template in tuple.templates.values() {
                if let Some(input) = &template.preserved_input {
                    let image = snapshot.inputs.get(input).ok_or(Error::Unsupported)?;
                    let expected = format!("{:x}", Sha256::digest(template.content.as_bytes()));
                    // Initial supported existing projects retain a known exact
                    // dependency definition; no lock regeneration or upgrade.
                    if image.relative != template.relative || image.sha256 != expected {
                        return Err(Error::Unsupported);
                    }
                }
            }
            for input in snapshot.inputs.keys() {
                operations.push(Operation::CopyInput {
                    input: input.clone(),
                });
            }
        } else {
            if !snapshot.inputs.is_empty() {
                return Err(Error::Unsupported);
            }
            for (id, template) in &tuple.templates {
                paths
                    .create_only
                    .insert(template.path.clone(), template.relative.clone());
                paths.publish_target.insert(
                    template.publish.clone(),
                    PublishBinding {
                        relative: template.relative.clone(),
                        before_sha256: None,
                        expected_template_sha256: Some(format!(
                            "{:x}",
                            Sha256::digest(template.content.as_bytes())
                        )),
                    },
                );
                operations.push(Operation::Materialize {
                    template: id.clone(),
                    destination: template.path.clone(),
                });
            }
        }
        for (id, relative) in &tuple.installation_roots {
            paths.publish_target.insert(
                id.clone(),
                PublishBinding {
                    relative: relative.clone(),
                    before_sha256: snapshot.installations.get(relative).cloned(),
                    expected_template_sha256: None,
                },
            );
        }
        resolve_recipes(definition, tuple, &mut operations)?;
        operations.push(Operation::StageInventory);
        for id in paths.publish_target.keys() {
            operations.push(Operation::Publish {
                source: StagedImageId(id.0.clone()),
                target: id.clone(),
            });
        }
        if definition.adapter == AdapterId::Python {
            operations.push(Operation::Probe {
                probe: ProbeId::PublishedPython,
            });
        }
        let mut commands = BTreeMap::new();
        for (index, operation) in operations.iter().enumerate() {
            if let Some(command) =
                crate::recipes::expand(definition, tuple, operation, &snapshot.host_prerequisites)?
            {
                commands.insert(index, command);
            }
        }
        if commands.len() > definition.limits.processes as usize {
            return Err(Error::InvalidProfile);
        }
        plans.push(CandidatePlan {
            index,
            paths,
            operations,
            commands,
        });
    }
    Ok(ResolvedPlan {
        schema: 2,
        profile: definition.profile,
        definition_sha256: definition.digest()?,
        adapter: definition.adapter,
        adapter_revision: definition.adapter_revision,
        normalized_version: constraint.clone(),
        snapshot: snapshot.clone(),
        candidates: plans,
        system_read_roots: ["/bin", "/usr/bin", "/usr/lib", "/System/Library"]
            .map(str::to_owned)
            .to_vec(),
        process_control: ProcessControl::ManagedGroup,
    })
}

impl ResolvedPlan {
    pub fn digest(&self, definition: &Definition) -> Result<String, Error> {
        validate_shape(definition, self)?;
        Ok(format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(self).map_err(|_| Error::StalePlan)?)
        ))
    }
}

fn resolve_recipes(
    definition: &Definition,
    tuple: &Tuple,
    operations: &mut Vec<Operation>,
) -> Result<(), Error> {
    let runtime: BTreeSet<_> = tuple
        .runtime
        .iter()
        .map(|runtime| &runtime.artifact)
        .collect();
    let packages: Vec<_> = tuple
        .artifacts
        .keys()
        .filter(|id| !runtime.contains(id))
        .cloned()
        .collect();
    let required: &[ProgramId] = match definition.adapter {
        AdapterId::Npm => &[ProgramId::Node, ProgramId::NpmCli],
        AdapterId::Python => &[ProgramId::Python],
        AdapterId::Cargo => &[ProgramId::Cargo, ProgramId::Rustc],
        AdapterId::Go => &[ProgramId::Go],
    };
    if required
        .iter()
        .any(|program| !tuple.programs.contains_key(program))
    {
        return Err(Error::InvalidProfile);
    }
    match definition.adapter {
        AdapterId::Npm => {
            operations.push(Operation::Probe {
                probe: ProbeId::NodeVersion,
            });
            operations.push(Operation::Probe {
                probe: ProbeId::NpmVersion,
            });
            for batch in packages.chunks(64) {
                operations.push(Operation::InstallOffline {
                    recipe: RecipeId::NpmCache,
                    artifacts: batch.to_vec(),
                });
            }
            operations.push(Operation::InstallOffline {
                recipe: RecipeId::NpmInstall,
                artifacts: Vec::new(),
            });
            operations.push(Operation::InstallOffline {
                recipe: RecipeId::NpmBuild,
                artifacts: Vec::new(),
            });
            operations.push(Operation::Probe {
                probe: ProbeId::BuildOutput,
            });
        }
        AdapterId::Python => {
            operations.push(Operation::Probe {
                probe: ProbeId::PythonVersion,
            });
            operations.push(Operation::InstallOffline {
                recipe: RecipeId::PythonVenv,
                artifacts: Vec::new(),
            });
            if !packages.is_empty() {
                operations.push(Operation::InstallOffline {
                    recipe: RecipeId::PythonInstall,
                    artifacts: packages,
                });
            }
            operations.push(Operation::Probe {
                probe: ProbeId::PythonSmoke,
            });
        }
        AdapterId::Cargo => {
            operations.push(Operation::Probe {
                probe: ProbeId::LinkerSelection,
            });
            operations.push(Operation::Probe {
                probe: ProbeId::ClangResourceSelection,
            });
            operations.push(Operation::Probe {
                probe: ProbeId::RustcVersion,
            });
            operations.push(Operation::Probe {
                probe: ProbeId::CargoVersion,
            });
            operations.push(Operation::InstallOffline {
                recipe: RecipeId::CargoBuild,
                artifacts: packages,
            });
            operations.push(Operation::Probe {
                probe: ProbeId::CargoSmoke,
            });
        }
        AdapterId::Go => {
            operations.push(Operation::Probe {
                probe: ProbeId::GoVersion,
            });
            operations.push(Operation::InstallOffline {
                recipe: RecipeId::GoBuild,
                artifacts: packages,
            });
            operations.push(Operation::Probe {
                probe: ProbeId::GoSmoke,
            });
        }
    }
    let processes = operations
        .iter()
        .filter(|op| {
            matches!(
                op,
                Operation::InstallOffline { .. } | Operation::Probe { .. }
            )
        })
        .count();
    if processes > definition.limits.processes as usize {
        return Err(Error::InvalidProfile);
    }
    Ok(())
}

fn validate_snapshot(snapshot: &Snapshot) -> Result<(), Error> {
    use std::path::{Component, Path};
    if !crate::is_digest(&snapshot.runner_binary_sha256)
        || !crate::is_digest(&snapshot.host_os_revision)
    {
        return Err(Error::StalePlan);
    }
    if snapshot.installations.len() > 16
        || snapshot
            .installations
            .iter()
            .any(|(path, hash)| !relative(path) || !crate::is_digest(hash))
    {
        return Err(Error::StalePlan);
    }
    for identity in [&snapshot.target, &snapshot.private_root] {
        let path = Path::new(&identity.canonical_path);
        if !path.is_absolute()
            || path.parent().is_none()
            || identity.inode == 0
            || identity.device == 0
            || identity.canonical_path.len() > 4096
            || identity.canonical_path.chars().any(char::is_control)
            || path
                .components()
                .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
        {
            return Err(Error::StalePlan);
        }
    }
    for prerequisite in snapshot.host_prerequisites.values() {
        let path = Path::new(&prerequisite.identity.canonical_path);
        if !path.is_absolute()
            || path.parent().is_none()
            || prerequisite.identity.device == 0
            || prerequisite.identity.inode == 0
            || prerequisite.identity.canonical_path.len() > 4096
            || prerequisite
                .identity
                .canonical_path
                .chars()
                .any(char::is_control)
            || path
                .components()
                .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
            || !crate::is_digest(&prerequisite.sha256)
            || prerequisite.version.is_empty()
            || prerequisite.version.len() > 128
            || prerequisite.version.chars().any(char::is_control)
        {
            return Err(Error::StalePlan);
        }
        if path.starts_with(&snapshot.target.canonical_path)
            || path.starts_with(&snapshot.private_root.canonical_path)
        {
            return Err(Error::StalePlan);
        }
    }
    let target = Path::new(&snapshot.target.canonical_path);
    let private = Path::new(&snapshot.private_root.canonical_path);
    if private.starts_with(target) || target.starts_with(private) || snapshot.inputs.len() > 10_000
    {
        return Err(Error::StalePlan);
    }
    let mut seen = BTreeSet::new();
    for image in snapshot.inputs.values() {
        if !relative(&image.relative)
            || !crate::is_digest(&image.sha256)
            || !seen.insert(&image.relative)
            || image.relative.split('/').any(|part| {
                matches!(part, ".npmrc" | ".git" | "node_modules" | ".venv")
                    || part.starts_with(".env")
            })
        {
            return Err(Error::StalePlan);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
