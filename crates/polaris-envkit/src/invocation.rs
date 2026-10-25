//! Render only revalidated compiled recipes. This module performs no I/O and
//! grants no spawn permission; the owner checks live file proofs before spawn.
use crate::{
    Error,
    adapter::RecipeId,
    adapter::{ResolvedPlan, Snapshot},
    definition::*,
    recipes::{Argument, Command, Executable},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

#[derive(Debug, Clone)]
pub struct Layout {
    root: PathBuf,
    index: usize,
}

impl Layout {
    pub fn for_plan(plan: &ResolvedPlan, index: usize) -> Result<Self, Error> {
        if !plan
            .candidates
            .iter()
            .any(|candidate| candidate.index == index)
        {
            return Err(Error::StalePlan);
        }
        Ok(Self {
            root: PathBuf::from(&plan.snapshot.private_root.canonical_path),
            index,
        })
    }
    pub fn root(&self) -> &std::path::Path {
        &self.root
    }
    pub fn candidate(&self) -> PathBuf {
        self.root.join("candidates").join(self.index.to_string())
    }
    pub fn stage(&self) -> PathBuf {
        self.candidate().join("stage")
    }
    pub fn scratch(&self) -> PathBuf {
        self.candidate().join("scratch")
    }
    pub fn home(&self) -> PathBuf {
        self.scratch().join("home")
    }
    pub fn tmp(&self) -> PathBuf {
        self.scratch().join("tmp")
    }
    pub fn artifacts(&self) -> PathBuf {
        self.root.join("artifacts")
    }
    pub fn artifact(&self, id: &ArtifactId) -> PathBuf {
        self.artifacts().join(format!("{}.tgz", id.0))
    }
    pub fn runtime(&self, id: &PrivateStableId) -> PathBuf {
        self.candidate().join("runtime").join(&id.0)
    }
}

pub struct Invocation {
    pub program: PathBuf,
    pub arguments: Vec<String>,
    pub environment: BTreeMap<String, String>,
}

pub fn render(
    command: &Command,
    tuple: &Tuple,
    layout: &Layout,
    snapshot: &Snapshot,
    completed: &BTreeSet<RecipeId>,
) -> Result<Invocation, Error> {
    if layout.root.to_str() != Some(snapshot.private_root.canonical_path.as_str())
        || command.host_inputs != snapshot.host_prerequisites
    {
        return Err(Error::StalePlan);
    }
    let program = match &command.executable {
        Executable::Runtime { id, identity } => {
            if tuple.programs.get(id) != Some(identity) {
                return Err(Error::StalePlan);
            }
            program_file(tuple, layout, *id)?
        }
        Executable::Host { id, identity } => {
            if snapshot.host_prerequisites.get(id) != Some(identity) {
                return Err(Error::StalePlan);
            }
            PathBuf::from(&identity.identity.canonical_path)
        }
        Executable::Generated {
            relative: path,
            creator,
        } => {
            if !completed.contains(creator) || !relative(path) {
                return Err(Error::RecoveryRequired);
            }
            layout.stage().join(path)
        }
        Executable::Published {
            relative: path,
            creator,
        } => {
            if !completed.contains(creator) || !relative(path) {
                return Err(Error::RecoveryRequired);
            }
            PathBuf::from(&snapshot.target.canonical_path).join(path)
        }
    };
    for (id, identity) in &command.program_inputs {
        if tuple.programs.get(id) != Some(identity) {
            return Err(Error::StalePlan);
        }
    }
    let arguments = command
        .arguments
        .iter()
        .map(|argument| argument_value(argument, tuple, layout, snapshot, 0))
        .collect::<Result<Vec<_>, _>>()?;
    if arguments.len() > 256
        || arguments.iter().map(String::len).sum::<usize>() > 128 * 1024
        || arguments
            .iter()
            .any(|argument| argument.chars().any(char::is_control))
    {
        return Err(Error::InvalidProfile);
    }
    let environment = command
        .environment
        .iter()
        .map(|(key, value)| {
            Ok((
                key.clone(),
                argument_value(value, tuple, layout, snapshot, 0)?,
            ))
        })
        .collect::<Result<BTreeMap<_, _>, Error>>()?;
    Ok(Invocation {
        program,
        arguments,
        environment,
    })
}

fn program_file(tuple: &Tuple, layout: &Layout, id: ProgramId) -> Result<PathBuf, Error> {
    let program = tuple.programs.get(&id).ok_or(Error::InvalidProfile)?;
    if !relative(&program.relative)
        || !tuple.runtime.iter().any(|component| {
            component.directory == program.runtime && component.artifact == program.artifact
        })
    {
        return Err(Error::InvalidProfile);
    }
    Ok(layout.runtime(&program.runtime).join(&program.relative))
}

fn argument_value(
    argument: &Argument,
    tuple: &Tuple,
    layout: &Layout,
    snapshot: &Snapshot,
    depth: u8,
) -> Result<String, Error> {
    if depth > 8 {
        return Err(Error::InvalidProfile);
    }
    let path = match argument {
        Argument::Literal(value) => return Ok(value.clone()),
        Argument::ProgramFile(id) => program_file(tuple, layout, *id)?,
        Argument::StageRoot => layout.stage(),
        Argument::ArtifactFile(id) => {
            if !tuple.artifacts.contains_key(id) || !crate::is_digest(&id.0) {
                return Err(Error::InvalidProfile);
            }
            layout.artifact(id)
        }
        Argument::StagePath(path) | Argument::HomePath(path) => {
            if !relative(path) {
                return Err(Error::InvalidProfile);
            }
            if matches!(argument, Argument::StagePath(_)) {
                layout.stage().join(path)
            } else {
                layout.home().join(path)
            }
        }
        Argument::HostPath(id) => PathBuf::from(
            &snapshot
                .host_prerequisites
                .get(id)
                .ok_or(Error::Unsupported)?
                .identity
                .canonical_path,
        ),
        Argument::Prefixed { prefix, value } => {
            let value = argument_value(value, tuple, layout, snapshot, depth + 1)?;
            if prefix.len() + value.len() > 16 * 1024 {
                return Err(Error::InvalidProfile);
            }
            return Ok(prefix.clone() + &value);
        }
        Argument::EncodedRustflags(values) => {
            if values.len() > 32 {
                return Err(Error::InvalidProfile);
            }
            let values = values
                .iter()
                .map(|value| argument_value(value, tuple, layout, snapshot, depth + 1))
                .collect::<Result<Vec<_>, _>>()?;
            if values
                .iter()
                .any(|value| value.chars().any(char::is_control))
            {
                return Err(Error::InvalidProfile);
            }
            return Ok(values.join("\u{1f}"));
        }
    };
    path.to_str()
        .map(str::to_owned)
        .ok_or(Error::InvalidProfile)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encoded_flags_preserve_spaces_without_shell_splitting() {
        let definition = crate::bundled::load(crate::catalog::ProfileId::Rust).unwrap();
        let mut prerequisites = BTreeMap::new();
        for (index, id) in definition.required_host.iter().enumerate() {
            prerequisites.insert(
                *id,
                HostPrerequisite {
                    identity: crate::plan::TargetIdentity {
                        canonical_path: format!("/tmp/Native Tools/item-{index}"),
                        device: 1,
                        inode: 10 + index as u64,
                    },
                    sha256: "b".repeat(64),
                    version: "synthetic".into(),
                },
            );
        }
        let snapshot = Snapshot {
            runner_binary_sha256: "a".repeat(64),
            host_os_revision: "d".repeat(64),
            target: crate::plan::TargetIdentity {
                canonical_path: "/tmp/project".into(),
                device: 1,
                inode: 2,
            },
            private_root: crate::plan::TargetIdentity {
                canonical_path: "/tmp/private".into(),
                device: 1,
                inode: 3,
            },
            platform: "darwin-arm64".into(),
            existing_project: false,
            inputs: BTreeMap::new(),
            installations: BTreeMap::new(),
            host_prerequisites: prerequisites,
        };
        let request = definition.profile.entry().parser.parse(None).unwrap();
        let plan = crate::adapter::resolve(&definition, &request, &snapshot).unwrap();
        let candidate = &plan.candidates[0];
        let layout = Layout::for_plan(&plan, candidate.index).unwrap();
        let command = candidate
            .commands
            .values()
            .find(|command| command.environment.contains_key("CARGO_ENCODED_RUSTFLAGS"))
            .unwrap();
        let invocation = render(
            command,
            &definition.candidates[candidate.index],
            &layout,
            &snapshot,
            &BTreeSet::new(),
        )
        .unwrap();
        let flags: Vec<_> = invocation.environment["CARGO_ENCODED_RUSTFLAGS"]
            .split('\u{1f}')
            .collect();
        assert_eq!(flags.len(), 4);
        assert!(flags[1].contains("Native Tools"));
        assert!(flags[1].starts_with("link-arg=-fuse-ld="));
    }
}
