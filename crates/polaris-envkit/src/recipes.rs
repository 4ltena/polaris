//! Compiled recipes; profiles cannot supply raw commands. Expanded program,
//! argv, environment and success conditions participate in plan consent.
use crate::{
    Error,
    adapter::{Operation, ProbeId, RecipeId},
    catalog::AdapterId,
    definition::*,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Argument {
    Literal(String),
    ProgramFile(ProgramId),
    ArtifactFile(ArtifactId),
    StagePath(String),
    StageRoot,
    HomePath(String),
    HostPath(HostPrerequisiteId),
    Prefixed {
        prefix: String,
        value: Box<Argument>,
    },
    EncodedRustflags(Vec<Argument>),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Executable {
    Runtime {
        id: ProgramId,
        identity: Program,
    },
    /// The creator must finish and journal its file identity/hash before use.
    Generated {
        relative: String,
        creator: RecipeId,
    },
    Published {
        relative: String,
        creator: RecipeId,
    },
    Host {
        id: HostPrerequisiteId,
        identity: HostPrerequisite,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Success {
    ExitZero,
    Stdout { expected: String },
    VersionToken { prefix: String, version: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Command {
    pub revision: u32,
    pub executable: Executable,
    pub arguments: Vec<Argument>,
    pub program_inputs: BTreeMap<ProgramId, Program>,
    pub environment: BTreeMap<String, Argument>,
    pub host_inputs: BTreeMap<HostPrerequisiteId, HostPrerequisite>,
    pub success: Success,
    pub seconds: u32,
    pub output_bytes_per_stream: u32,
}

pub fn expand(
    definition: &Definition,
    tuple: &Tuple,
    operation: &Operation,
    prerequisites: &BTreeMap<HostPrerequisiteId, HostPrerequisite>,
) -> Result<Option<Command>, Error> {
    use Argument::{ArtifactFile, HomePath, Literal, ProgramFile, StagePath};
    if matches!(
        operation,
        Operation::Probe {
            probe: ProbeId::PublishedPython
        }
    ) {
        if definition.adapter != AdapterId::Python {
            return Err(Error::InvalidProfile);
        }
        return Ok(Some(Command {
            revision: 1,
            executable: Executable::Published {
                relative: ".venv/bin/python".into(),
                creator: RecipeId::PythonVenv,
            },
            arguments: literals(&[
                "-I",
                "-c",
                "import sys; assert sys.prefix != sys.base_prefix; print('envkit ready')",
            ]),
            program_inputs: BTreeMap::from([(
                ProgramId::Python,
                required(tuple, ProgramId::Python)?.clone(),
            )]),
            environment: environment(AdapterId::Python),
            host_inputs: prerequisites.clone(),
            success: Success::Stdout {
                expected: "envkit ready".into(),
            },
            seconds: definition.limits.seconds_per_process,
            output_bytes_per_stream: definition.limits.output_bytes_per_stream,
        }));
    }
    if let Operation::Probe {
        probe: ProbeId::LinkerSelection | ProbeId::ClangResourceSelection,
    } = operation
    {
        if definition.adapter != AdapterId::Cargo {
            return Err(Error::InvalidProfile);
        }
        let Operation::Probe { probe } = operation else {
            return Err(Error::InvalidProfile);
        };
        let id = if *probe == ProbeId::LinkerSelection {
            HostPrerequisiteId::Linker
        } else {
            HostPrerequisiteId::ClangResources
        };
        let expected = prerequisites
            .get(&id)
            .ok_or(Error::Unsupported)?
            .identity
            .canonical_path
            .clone();
        let mut arguments = linker_arguments();
        arguments.push(Literal(
            if *probe == ProbeId::LinkerSelection {
                "--print-prog-name=ld"
            } else {
                "-print-resource-dir"
            }
            .into(),
        ));
        return Ok(Some(Command {
            revision: 1,
            executable: Executable::Host {
                id: HostPrerequisiteId::CompilerDriver,
                identity: prerequisites
                    .get(&HostPrerequisiteId::CompilerDriver)
                    .ok_or(Error::Unsupported)?
                    .clone(),
            },
            arguments,
            program_inputs: BTreeMap::new(),
            environment: environment(AdapterId::Cargo),
            host_inputs: prerequisites.clone(),
            success: Success::Stdout { expected },
            seconds: definition.limits.seconds_per_process,
            output_bytes_per_stream: definition.limits.output_bytes_per_stream,
        }));
    }
    let mut arguments = Vec::new();
    let mut success = Success::ExitZero;
    let mut generated = None;
    let program = match operation {
        Operation::InstallOffline { recipe, artifacts } => match recipe {
            RecipeId::NpmCache | RecipeId::NpmInstall | RecipeId::NpmBuild
                if definition.adapter == AdapterId::Npm =>
            {
                arguments.push(ProgramFile(ProgramId::NpmCli));
                for (flag, path) in [
                    ("--cache", "npm-cache"),
                    ("--userconfig", "user.npmrc"),
                    ("--globalconfig", "global.npmrc"),
                ] {
                    arguments.extend([Literal(flag.into()), HomePath(path.into())]);
                }
                arguments.extend(literals(&[
                    "--offline",
                    "--ignore-scripts",
                    "--no-audit",
                    "--no-fund",
                    "--no-progress",
                    "--update-notifier=false",
                    "--loglevel=error",
                ]));
                match recipe {
                    RecipeId::NpmCache => {
                        if artifacts.is_empty() || artifacts.len() > 64 {
                            return Err(Error::InvalidProfile);
                        }
                        arguments.extend(literals(&["cache", "add"]));
                        for artifact in artifacts {
                            if !tuple.artifacts.contains_key(artifact)
                                || tuple
                                    .runtime
                                    .iter()
                                    .any(|runtime| &runtime.artifact == artifact)
                            {
                                return Err(Error::InvalidProfile);
                            }
                            arguments.push(ArtifactFile(artifact.clone()));
                        }
                    }
                    RecipeId::NpmInstall | RecipeId::NpmBuild => {
                        if !artifacts.is_empty() {
                            return Err(Error::InvalidProfile);
                        }
                        arguments.extend(if *recipe == RecipeId::NpmInstall {
                            literals(&["ci"])
                        } else {
                            literals(&["run", "build"])
                        });
                    }
                    _ => return Err(Error::InvalidProfile),
                }
                ProgramId::Node
            }
            RecipeId::PythonVenv
                if definition.adapter == AdapterId::Python && artifacts.is_empty() =>
            {
                // ensurepip uses only wheels bundled in the pinned interpreter.
                arguments.extend(literals(&["-I", "-m", "venv", "--copies"]));
                arguments.push(StagePath(".venv".into()));
                ProgramId::Python
            }
            RecipeId::PythonInstall
                if definition.adapter == AdapterId::Python && !artifacts.is_empty() =>
            {
                generated = Some((".venv/bin/python", RecipeId::PythonVenv));
                arguments.extend(literals(&[
                    "-I",
                    "-m",
                    "pip",
                    "--isolated",
                    "install",
                    "--no-index",
                    "--require-hashes",
                    "--only-binary=:all:",
                    "--find-links",
                ]));
                arguments.extend([
                    StagePath(".envkit-wheelhouse".into()),
                    Literal("-r".into()),
                    StagePath("requirements.lock".into()),
                ]);
                ProgramId::Python
            }
            RecipeId::CargoBuild if definition.adapter == AdapterId::Cargo => {
                arguments.extend(literals(&["build", "--offline", "--frozen"]));
                ProgramId::Cargo
            }
            RecipeId::GoBuild if definition.adapter == AdapterId::Go => {
                arguments.extend(literals(&[
                    "build",
                    "-mod=readonly",
                    "-buildvcs=false",
                    "-o",
                ]));
                arguments.extend([StagePath("envkit-app".into()), Literal("./...".into())]);
                ProgramId::Go
            }
            _ => return Err(Error::InvalidProfile),
        },
        Operation::Probe { probe } => match probe {
            ProbeId::PublishedPython => return Err(Error::InvalidProfile),
            ProbeId::NodeVersion => {
                arguments = literals(&["--version"]);
                success = Success::Stdout {
                    expected: format!("v{}", required(tuple, ProgramId::Node)?.version),
                };
                ProgramId::Node
            }
            ProbeId::NpmVersion => {
                arguments = vec![ProgramFile(ProgramId::NpmCli), Literal("--version".into())];
                success = Success::Stdout {
                    expected: required(tuple, ProgramId::NpmCli)?.version.clone(),
                };
                ProgramId::Node
            }
            ProbeId::PythonVersion => {
                arguments = literals(&["-I", "--version"]);
                success = Success::Stdout {
                    expected: format!("Python {}", required(tuple, ProgramId::Python)?.version),
                };
                ProgramId::Python
            }
            ProbeId::PythonSmoke => {
                generated = Some((".venv/bin/python", RecipeId::PythonVenv));
                arguments = vec![Literal("-I".into()), StagePath("main.py".into())];
                success = Success::Stdout {
                    expected: "envkit ready".into(),
                };
                ProgramId::Python
            }
            ProbeId::RustcVersion | ProbeId::CargoVersion => {
                let program = if *probe == ProbeId::RustcVersion {
                    ProgramId::Rustc
                } else {
                    ProgramId::Cargo
                };
                arguments = literals(&["--version"]);
                success = Success::VersionToken {
                    prefix: if program == ProgramId::Rustc {
                        "rustc ".into()
                    } else {
                        "cargo ".into()
                    },
                    version: required(tuple, program)?.version.clone(),
                };
                program
            }
            ProbeId::CargoSmoke => {
                generated = Some(("target/debug/envkit", RecipeId::CargoBuild));
                success = Success::Stdout {
                    expected: "envkit ready".into(),
                };
                ProgramId::Cargo
            }
            ProbeId::GoVersion => {
                arguments = literals(&["version"]);
                success = Success::Stdout {
                    expected: format!(
                        "go version go{} darwin/arm64",
                        required(tuple, ProgramId::Go)?.version
                    ),
                };
                ProgramId::Go
            }
            ProbeId::GoSmoke => {
                generated = Some(("envkit-app", RecipeId::GoBuild));
                success = Success::Stdout {
                    expected: "envkit ready".into(),
                };
                ProgramId::Go
            }
            ProbeId::BuildOutput => return Ok(None),
            ProbeId::LinkerSelection | ProbeId::ClangResourceSelection => {
                return Err(Error::InvalidProfile);
            }
        },
        _ => return Ok(None),
    };
    let identity = required(tuple, program)?.clone();
    let mut program_inputs = BTreeMap::from([(program, identity.clone())]);
    for argument in &arguments {
        if let ProgramFile(program) = argument {
            program_inputs.insert(*program, required(tuple, *program)?.clone());
        }
    }
    let executable = match generated {
        Some((relative, creator)) => Executable::Generated {
            relative: relative.into(),
            creator,
        },
        None => Executable::Runtime {
            id: program,
            identity,
        },
    };
    let mut fixed_environment = environment(definition.adapter);
    if definition.profile == crate::catalog::ProfileId::Nextjs {
        // The pinned Next release otherwise probes Git/workspace ancestors
        // outside the stage. Its explicit trace root keeps discovery local.
        fixed_environment.insert("NEXT_PRIVATE_OUTPUT_TRACE_ROOT".into(), Argument::StageRoot);
    }
    Ok(Some(Command {
        revision: 1,
        executable,
        arguments,
        program_inputs,
        environment: fixed_environment,
        host_inputs: prerequisites.clone(),
        success,
        seconds: definition.limits.seconds_per_process,
        output_bytes_per_stream: definition.limits.output_bytes_per_stream,
    }))
}

fn required(tuple: &Tuple, program: ProgramId) -> Result<&Program, Error> {
    tuple.programs.get(&program).ok_or(Error::InvalidProfile)
}
fn literals(values: &[&str]) -> Vec<Argument> {
    values
        .iter()
        .map(|value| Argument::Literal((*value).into()))
        .collect()
}

fn environment(adapter: AdapterId) -> BTreeMap<String, Argument> {
    use Argument::{HomePath, Literal};
    let fixed: &[(&str, &str)] = match adapter {
        AdapterId::Npm => &[
            ("CI", "1"),
            ("NEXT_TELEMETRY_DISABLED", "1"),
            ("NUXT_TELEMETRY_DISABLED", "1"),
            ("ASTRO_TELEMETRY_DISABLED", "1"),
        ],
        AdapterId::Python => &[
            ("PYTHONNOUSERSITE", "1"),
            ("PIP_NO_INDEX", "1"),
            ("PIP_CONFIG_FILE", "/dev/null"),
            ("PIP_DISABLE_PIP_VERSION_CHECK", "1"),
        ],
        AdapterId::Cargo => &[("CARGO_NET_OFFLINE", "true")],
        AdapterId::Go => &[
            ("GOTOOLCHAIN", "local"),
            ("GOPROXY", "off"),
            ("GOSUMDB", "off"),
            ("CGO_ENABLED", "0"),
            ("GOENV", "off"),
            ("GOWORK", "off"),
        ],
    };
    let mut environment: BTreeMap<_, _> = fixed
        .iter()
        .map(|(key, value)| ((*key).into(), Literal((*value).into())))
        .collect();
    match adapter {
        AdapterId::Cargo => {
            environment.insert("CARGO_HOME".into(), HomePath("cargo".into()));
            environment.insert("RUSTC_WRAPPER".into(), Literal(String::new()));
            environment.insert("RUSTC_WORKSPACE_WRAPPER".into(), Literal(String::new()));
            environment.insert(
                "CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER".into(),
                Argument::HostPath(HostPrerequisiteId::CompilerDriver),
            );
            let mut flags = Vec::new();
            for argument in linker_arguments() {
                flags.push(Literal("-C".into()));
                flags.push(Argument::Prefixed {
                    prefix: "link-arg=".into(),
                    value: Box::new(argument),
                });
            }
            environment.insert(
                "CARGO_ENCODED_RUSTFLAGS".into(),
                Argument::EncodedRustflags(flags),
            );
            environment.insert(
                "SDKROOT".into(),
                Argument::HostPath(HostPrerequisiteId::MacSdk),
            );
        }
        AdapterId::Go => {
            environment.insert("GOPATH".into(), HomePath("go".into()));
            environment.insert("GOCACHE".into(), HomePath("go-cache".into()));
        }
        _ => {}
    }
    environment
}

fn linker_arguments() -> Vec<Argument> {
    vec![
        Argument::Prefixed {
            prefix: "-fuse-ld=".into(),
            value: Box::new(Argument::HostPath(HostPrerequisiteId::Linker)),
        },
        Argument::Prefixed {
            prefix: "-resource-dir=".into(),
            value: Box::new(Argument::HostPath(HostPrerequisiteId::ClangResources)),
        },
    ]
}
