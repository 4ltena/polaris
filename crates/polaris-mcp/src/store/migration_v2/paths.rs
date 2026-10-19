//! Pure typed-path migration for registered and materialized servers.
//!
//! This does not open paths, inspect a source tree, verify executable/script
//! digests, or run admission. Before activation the caller must validate the
//! retired source and staging descriptors, pin digests and all regular files,
//! references, and side-effect-free runtime/resource admission, then revalidate
//! final paths after publication. Root migration must inventory every materialized
//! tree; the registered transform rejects extra pins, while the unregistered
//! transform checks one pin against the actual source registry.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::Error;
use crate::registration::{
    Destination, MAX_JSON_BYTES, MAX_SERVERS, MAX_STORE_BYTES, Registry, bounded_json,
    canonical_hash, valid_id,
};

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinnedPathsV1 {
    pub schema_version: u32,
    pub package: String,
    pub version: String,
    pub runtime: PathBuf,
    pub entry: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagedPathFieldV2 {
    Cwd,
    Command,
    Script,
    ReadableRoot { index: usize },
    ScriptArgument { index: usize },
    PinRuntime,
    PinEntry,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedPathFieldMappingV2 {
    pub field: ManagedPathFieldV2,
    pub old_value_hash: String,
    pub new_value_hash: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerManagedPathMappingV2 {
    pub server_id: String,
    pub old_destination_hash: String,
    pub new_destination_hash: String,
    pub fields: Vec<ManagedPathFieldMappingV2>,
}

/// Contains no source path strings, opaque argument values, or secret values.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedPathProjectionV2 {
    pub schema_version: u32,
    pub source_registry_payload_hash: String,
    pub new_registry_payload_hash: String,
    pub servers: Vec<ServerManagedPathMappingV2>,
}

pub struct ManagedPathTransformV2 {
    pub registry: Registry,
    pub pins: BTreeMap<String, PinnedPathsV1>,
    pub projection: ManagedPathProjectionV2,
}

pub struct UnregisteredPinTransformV2 {
    pub server_id: String,
    pub pin: PinnedPathsV1,
    pub fields: Vec<ManagedPathFieldMappingV2>,
}

/// Transforms logical paths only; the home is supplied, never read from the OS.
///
/// Pins must belong to registered stdio servers. Payload hashes use the same
/// fixed serde representation of Some(Registry) as the v2 document envelope;
/// they are not the raw source-file hash, which the caller binds separately.
pub fn transform_managed_paths_v2(
    source: &Registry,
    logical_home: &Path,
    pins: &BTreeMap<String, Vec<u8>>,
) -> Result<ManagedPathTransformV2, Error> {
    source.validate()?;
    require_normalized(logical_home)?;
    if pins.len() > MAX_SERVERS
        || pins.keys().any(|id| {
            !source.servers.iter().any(|server| {
                server.server_id == id.as_str()
                    && matches!(&server.transport, Destination::Stdio { .. })
            })
        })
    {
        return Err(Error::InvalidRegistration);
    }
    let old_root = logical_home.join(".polaris/mcp");
    let new_root = logical_home.join(".polaris/mcp-v2");
    let old_root_text = old_root.to_str().ok_or(Error::InvalidRegistration)?;
    let mut registry = source.clone();
    let mut mapped_pins = BTreeMap::new();
    let mut mappings = Vec::with_capacity(source.servers.len());

    for (old, new) in source.servers.iter().zip(&mut registry.servers) {
        let old_server = old_root.join("servers").join(&old.server_id);
        let new_server = new_root.join("servers").join(&old.server_id);
        let mut fields = Vec::new();
        if let (
            Destination::Stdio {
                command,
                args,
                cwd,
                readable_roots,
                script,
                ..
            },
            Destination::Stdio {
                command: new_command,
                args: new_args,
                cwd: new_cwd,
                readable_roots: new_readable_roots,
                script: new_script,
                ..
            },
        ) = (&old.transport, &mut new.transport)
        {
            *new_cwd = map_path(cwd, &old_root, &old_server, &new_server, true)?;
            *new_command = map_path(command, &old_root, &old_server, &new_server, false)?;
            *new_script = script
                .as_ref()
                .map(|path| map_path(path, &old_root, &old_server, &new_server, false))
                .transpose()?;
            fields.push(field_hash(ManagedPathFieldV2::Cwd, cwd, new_cwd)?);
            fields.push(field_hash(
                ManagedPathFieldV2::Command,
                command,
                new_command,
            )?);
            fields.push(field_hash(ManagedPathFieldV2::Script, script, new_script)?);
            for (index, (root, mapped)) in readable_roots.iter().zip(new_readable_roots).enumerate()
            {
                *mapped = map_path(root, &old_root, &old_server, &new_server, false)?;
                fields.push(field_hash(
                    ManagedPathFieldV2::ReadableRoot { index },
                    root,
                    mapped,
                )?);
            }
            let script_text = script
                .as_ref()
                .map(|path| path.to_str().ok_or(Error::InvalidRegistration))
                .transpose()?;
            let new_script_text = new_script
                .as_ref()
                .map(|path| path.to_str().ok_or(Error::InvalidRegistration))
                .transpose()?;
            for (index, (argument, mapped)) in args.iter().zip(new_args).enumerate() {
                if Some(argument.as_str()) == script_text {
                    *mapped = new_script_text
                        .ok_or(Error::InvalidRegistration)?
                        .to_owned();
                    fields.push(field_hash(
                        ManagedPathFieldV2::ScriptArgument { index },
                        argument,
                        mapped,
                    )?);
                } else if argument.contains(old_root_text) {
                    // An opaque argument has no typed path semantics to preserve.
                    return Err(Error::InvalidRegistration);
                }
            }
            if let Some(bytes) = pins.get(&old.server_id) {
                let pin = parse_pin_v1(bytes)?;
                if pin.runtime != *command
                    || script.as_ref() != Some(&pin.entry)
                    || !pin.entry.starts_with(&old_server)
                    || pin.entry == old_server
                    || !args.iter().any(|arg| Some(arg.as_str()) == script_text)
                {
                    return Err(Error::InvalidRegistration);
                }
                let mut mapped = pin.clone();
                mapped.runtime =
                    map_path(&pin.runtime, &old_root, &old_server, &new_server, false)?;
                mapped.entry = map_path(&pin.entry, &old_root, &old_server, &new_server, false)?;
                fields.push(field_hash(
                    ManagedPathFieldV2::PinRuntime,
                    &pin.runtime,
                    &mapped.runtime,
                )?);
                fields.push(field_hash(
                    ManagedPathFieldV2::PinEntry,
                    &pin.entry,
                    &mapped.entry,
                )?);
                mapped_pins.insert(old.server_id.clone(), mapped);
            }
        }
        new.validate()?;
        mappings.push(ServerManagedPathMappingV2 {
            server_id: old.server_id.clone(),
            old_destination_hash: old.destination_hash()?,
            new_destination_hash: new.destination_hash()?,
            fields,
        });
    }
    registry.validate()?;
    let projection = ManagedPathProjectionV2 {
        schema_version: 1,
        source_registry_payload_hash: payload_hash(source)?,
        new_registry_payload_hash: payload_hash(&registry)?,
        servers: mappings,
    };
    Ok(ManagedPathTransformV2 {
        registry,
        pins: mapped_pins,
        projection,
    })
}

/// Maps one complete pin from a materialized server absent from the source
/// registry. Each call is independent; the registry's server limit does not cap
/// the number of materialized directories.
///
/// The caller must supply the actual source registry, or None only when the
/// source document is absent, and must separately reject unfinished control
/// files and validate source/staging descriptors. Package/version text is
/// preserved, including opaque text that happens to contain an old-root path.
pub fn transform_unregistered_pin_v2(
    source_registry: Option<&Registry>,
    server_id: &str,
    complete_pin_bytes: &[u8],
    logical_home: &Path,
) -> Result<UnregisteredPinTransformV2, Error> {
    if !valid_id(server_id) {
        return Err(Error::InvalidRegistration);
    }
    if let Some(registry) = source_registry {
        registry.validate()?;
        if registry
            .servers
            .iter()
            .any(|server| server.server_id == server_id)
        {
            return Err(Error::InvalidRegistration);
        }
    }
    require_normalized(logical_home)?;
    let pin = parse_pin_v1(complete_pin_bytes)?;
    let old_root = logical_home.join(".polaris/mcp");
    let old_server = old_root.join("servers").join(server_id);
    let new_server = logical_home.join(".polaris/mcp-v2/servers").join(server_id);
    let mut mapped = pin.clone();
    mapped.runtime = map_path(&pin.runtime, &old_root, &old_server, &new_server, false)?;
    mapped.entry = map_path(&pin.entry, &old_root, &old_server, &new_server, false)?;
    let fields = vec![
        field_hash(
            ManagedPathFieldV2::PinRuntime,
            &pin.runtime,
            &mapped.runtime,
        )?,
        field_hash(ManagedPathFieldV2::PinEntry, &pin.entry, &mapped.entry)?,
    ];
    Ok(UnregisteredPinTransformV2 {
        server_id: server_id.to_owned(),
        pin: mapped,
        fields,
    })
}

fn parse_pin_v1(bytes: &[u8]) -> Result<PinnedPathsV1, Error> {
    if bytes.len() > MAX_JSON_BYTES {
        return Err(Error::Limit);
    }
    let pin: PinnedPathsV1 =
        serde_json::from_slice(bytes).map_err(|_| Error::InvalidRegistration)?;
    bounded_json(&pin, MAX_JSON_BYTES)?;
    require_normalized(&pin.runtime)?;
    require_normalized(&pin.entry)?;
    if pin.schema_version != 1
        || pin.package.is_empty()
        || pin.version.is_empty()
        || pin.package.contains('\0')
        || pin.version.contains('\0')
    {
        return Err(Error::InvalidRegistration);
    }
    Ok(pin)
}

fn require_normalized(path: &Path) -> Result<(), Error> {
    if !path.is_absolute()
        || path.as_os_str().len() > 16_384
        || path.as_os_str().as_encoded_bytes().contains(&0)
        || !path
            .components()
            .all(|part| matches!(part, Component::RootDir | Component::Normal(_)))
        || path.components().collect::<PathBuf>().as_os_str() != path.as_os_str()
        || path.to_str().is_none()
    {
        return Err(Error::InvalidRegistration);
    }
    Ok(())
}

fn map_path(
    path: &Path,
    old_root: &Path,
    old_server: &Path,
    new_server: &Path,
    exact_cwd: bool,
) -> Result<PathBuf, Error> {
    require_normalized(path)?;
    if !path.starts_with(old_root) {
        return Ok(path.to_owned());
    }
    if exact_cwd {
        return (path == old_server)
            .then(|| new_server.to_owned())
            .ok_or(Error::InvalidRegistration);
    }
    let relative = path
        .strip_prefix(old_server)
        .map_err(|_| Error::InvalidRegistration)?;
    if relative.as_os_str().is_empty() {
        Ok(new_server.to_owned())
    } else {
        Ok(new_server.join(relative))
    }
}

fn field_hash<T: Serialize>(
    field: ManagedPathFieldV2,
    old: &T,
    new: &T,
) -> Result<ManagedPathFieldMappingV2, Error> {
    let hash = |value| canonical_hash(&serde_json::to_value(value).map_err(|_| Error::Protocol)?);
    Ok(ManagedPathFieldMappingV2 {
        field,
        old_value_hash: hash(old)?,
        new_value_hash: hash(new)?,
    })
}

fn payload_hash(registry: &Registry) -> Result<String, Error> {
    let bytes = bounded_json(&Some(registry), MAX_STORE_BYTES)?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

#[cfg(test)]
#[path = "paths/tests.rs"]
mod tests;
