//! Fixed resource registry. No project-supplied file or plugin can replace a
//! bundled definition. Missing resources remain planned, not successful.
use crate::{
    Error,
    catalog::{AdapterId, DefinitionState, ProfileId},
    definition::*,
};
use std::collections::BTreeMap;

pub fn load(id: ProfileId) -> Result<Definition, Error> {
    let definition = match id {
        ProfileId::Node | ProfileId::Nextjs => node_definition(id)?,
        ProfileId::Angular => {
            serde_json::from_str(include_str!("../resources/profiles/angular.json"))
                .map_err(|_| Error::InvalidProfile)?
        }
        ProfileId::React => serde_json::from_str(include_str!("../resources/profiles/react.json"))
            .map_err(|_| Error::InvalidProfile)?,
        ProfileId::Vue => serde_json::from_str(include_str!("../resources/profiles/vue.json"))
            .map_err(|_| Error::InvalidProfile)?,
        ProfileId::Svelte => {
            serde_json::from_str(include_str!("../resources/profiles/svelte.json"))
                .map_err(|_| Error::InvalidProfile)?
        }
        ProfileId::Astro => serde_json::from_str(include_str!("../resources/profiles/astro.json"))
            .map_err(|_| Error::InvalidProfile)?,
        ProfileId::Nuxt => serde_json::from_str(include_str!("../resources/profiles/nuxt.json"))
            .map_err(|_| Error::InvalidProfile)?,
        ProfileId::Python => {
            serde_json::from_str(include_str!("../resources/profiles/python.json"))
                .map_err(|_| Error::InvalidProfile)?
        }
        ProfileId::Rust => serde_json::from_str(include_str!("../resources/profiles/rust.json"))
            .map_err(|_| Error::InvalidProfile)?,
        ProfileId::Go => serde_json::from_str(include_str!("../resources/profiles/go.json"))
            .map_err(|_| Error::InvalidProfile)?,
    };
    definition.validate()?;
    Ok(definition)
}

pub fn state(id: ProfileId) -> Result<DefinitionState, Error> {
    match load(id) {
        Ok(_) => Ok(DefinitionState::Available),
        Err(Error::Unsupported) => Ok(DefinitionState::Planned),
        Err(error) => Err(error),
    }
}

fn node_definition(id: ProfileId) -> Result<Definition, Error> {
    let legacy = crate::profile::nextjs()?;
    let mut candidates = Vec::new();
    for old in legacy.candidates {
        let node = ArtifactId::of(&old.node);
        let node_version = old.node.version.clone();
        let mut artifacts = BTreeMap::from([(node.clone(), old.node)]);
        let templates = if id == ProfileId::Nextjs {
            for artifact in old.packages {
                artifacts.insert(ArtifactId::of(&artifact), artifact);
            }
            BTreeMap::from([
                template("manifest", "package.json", old.manifest, true),
                template("lock", "package-lock.json", old.lock, true),
                template(
                    "layout",
                    "app/layout.js",
                    String::from_utf8(crate::profile::TEMPLATE[2].1.to_vec())
                        .map_err(|_| Error::InvalidProfile)?,
                    false,
                ),
                template(
                    "page",
                    "app/page.js",
                    String::from_utf8(crate::profile::TEMPLATE[3].1.to_vec())
                        .map_err(|_| Error::InvalidProfile)?,
                    false,
                ),
            ])
        } else {
            let manifest = serde_json::json!({ "name": "envkit-node", "version": "1.0.0", "private": true,
                "scripts": { "build": "node --check index.js" }, "engines": { "node": format!("={node_version}") } });
            let lock = serde_json::json!({ "name": "envkit-node", "version": "1.0.0", "lockfileVersion": 3,
                "requires": true, "packages": { "": { "name": "envkit-node", "version": "1.0.0", "engines": manifest["engines"] } } });
            BTreeMap::from([
                template("manifest", "package.json", text(&manifest)?, true),
                template("lock", "package-lock.json", text(&lock)?, true),
                template(
                    "index",
                    "index.js",
                    "console.log('envkit ready');\n".into(),
                    false,
                ),
            ])
        };
        let mut tuple = Tuple {
            version: if id == ProfileId::Node {
                node_version.clone()
            } else {
                old.next
            },
            artifacts,
            runtime: vec![RuntimeComponent {
                artifact: node.clone(),
                directory: PrivateStableId("node".into()),
                archive: ArchiveFormat::TarGzip,
                strip_prefix: format!("node-v{node_version}-darwin-arm64"),
            }],
            programs: BTreeMap::from([
                (
                    ProgramId::Node,
                    Program {
                        artifact: node.clone(),
                        runtime: PrivateStableId("node".into()),
                        relative: "bin/node".into(),
                        version: node_version,
                    },
                ),
                (
                    ProgramId::NpmCli,
                    Program {
                        artifact: node,
                        runtime: PrivateStableId("node".into()),
                        relative: "lib/node_modules/npm/bin/npm-cli.js".into(),
                        version: old.npm_version,
                    },
                ),
            ]),
            templates,
            installation_roots: if id == ProfileId::Nextjs {
                BTreeMap::from([(
                    PublishTargetId("dependencies".into()),
                    "node_modules".into(),
                )])
            } else {
                BTreeMap::new()
            },
            dependency_closure_sha256: String::new(),
            build_outputs: BTreeMap::from([(
                StagedImageId("build_output".into()),
                if id == ProfileId::Nextjs {
                    ".next/BUILD_ID".into()
                } else {
                    "index.js".into()
                },
            )]),
        };
        tuple.dependency_closure_sha256 = tuple.closure_digest()?;
        if !candidates.contains(&tuple) {
            candidates.push(tuple);
        }
    }
    Ok(Definition {
        schema: 2,
        revision: 1,
        profile: id,
        adapter: AdapterId::Npm,
        adapter_revision: 1,
        platform: "darwin-arm64".into(),
        origins: if id == ProfileId::Node {
            vec!["https://nodejs.org".into()]
        } else {
            vec![
                "https://nodejs.org".into(),
                "https://registry.npmjs.org".into(),
            ]
        },
        redirect_origins: Vec::new(),
        limits: Limits::default(),
        required_host: crate::definition::required_host(AdapterId::Npm),
        candidates,
    })
}

fn template(id: &str, relative: &str, content: String, preserve: bool) -> (TemplateId, Template) {
    (
        TemplateId(id.into()),
        Template {
            path: CreatePathId(id.into()),
            publish: PublishTargetId(id.into()),
            relative: relative.into(),
            content,
            preserved_input: preserve.then(|| ReadInputId(id.into())),
        },
    )
}

fn text(value: &serde_json::Value) -> Result<String, Error> {
    serde_json::to_string_pretty(value)
        .map(|value| value + "\n")
        .map_err(|_| Error::InvalidProfile)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn primary_compatibility_is_locked_metadata_not_a_general_build_error() {
        let definition = load(ProfileId::Nextjs).unwrap();
        let mut candidate = definition.candidates[0].clone();
        assert!(
            candidate
                .primary_runtime_compatible(ProfileId::Nextjs)
                .unwrap()
        );
        candidate
            .programs
            .get_mut(&ProgramId::Node)
            .unwrap()
            .version = "18.0.0".into();
        assert!(
            !candidate
                .primary_runtime_compatible(ProfileId::Nextjs)
                .unwrap()
        );
        candidate
            .programs
            .get_mut(&ProgramId::Node)
            .unwrap()
            .version = "24.22.1".into();
        assert!(
            candidate
                .primary_runtime_compatible(ProfileId::Nextjs)
                .unwrap()
        );
    }
    #[test]
    fn every_catalog_entry_has_a_valid_fixed_definition() {
        for id in crate::catalog::ALL {
            let definition = load(id).unwrap();
            assert_eq!(definition.profile, id);
            assert_eq!(state(id).unwrap(), DefinitionState::Available);
            assert!(!definition.candidates.is_empty());
            assert_eq!(definition.digest().unwrap().len(), 64);
        }
    }
    #[test]
    fn missing_excluded_platform_metadata_is_refused_before_any_transfer() {
        let mut definition = load(ProfileId::Nextjs).unwrap();
        let lock = definition.candidates[0]
            .templates
            .values_mut()
            .find(|template| template.relative == "package-lock.json")
            .unwrap();
        let mut document: serde_json::Value = serde_json::from_str(&lock.content).unwrap();
        assert!(
            document["packages"]
                .as_object_mut()
                .unwrap()
                .remove("node_modules/@next/swc-darwin-x64")
                .is_some()
        );
        lock.content = serde_json::to_string(&document).unwrap();
        assert_eq!(definition.validate(), Err(Error::InvalidProfile));
    }

    #[test]
    fn node_and_next_use_one_adapter_but_have_distinct_versions_and_resources() {
        let node = load(ProfileId::Node).unwrap();
        let next = load(ProfileId::Nextjs).unwrap();
        assert_eq!(node.adapter, next.adapter);
        assert_eq!(node.candidates[0].artifacts.len(), 1);
        assert_eq!(next.candidates[0].artifacts.len(), 25);
        assert_ne!(node.candidates[0].version, next.candidates[0].version);
        assert_ne!(node.digest().unwrap(), next.digest().unwrap());
        assert!(node.candidates[0].installation_roots.is_empty());
    }
}
