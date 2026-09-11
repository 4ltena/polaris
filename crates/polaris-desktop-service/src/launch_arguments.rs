//! Strict native launch metadata. Parsing performs no I/O or authorization.
use crate::ServiceConfig;
use polaris_core::desktop_store::{BootstrapIdentity, BootstrapTier, ExpectedBootstrapFile};
use polaris_desktop_protocol::ids::{DecimalU64, ProjectId, SessionId};
use std::{ffi::OsString, path::PathBuf};

#[derive(Debug, thiserror::Error)]
#[error("invalid desktop launch arguments")]
pub struct LaunchArgumentsError;

pub enum LaunchArguments {
    Legacy {
        store: ServiceConfig,
        recovery_fd2: bool,
    },
    OwnerV1(OwnerLaunchArguments),
}
/// These values still require native authority/bootstrap/store validation.
/// OwnerV1 always requires the fixed recovery-fd2 flag; it grants no runtime.
pub struct OwnerLaunchArguments {
    pub store: ServiceConfig,
    pub store_identity: BootstrapIdentity,
    pub bootstrap_name: String,
    pub bootstrap_file: ExpectedBootstrapFile,
    pub confirmed_source_path: PathBuf,
    pub confirmed_source_identity: BootstrapIdentity,
    pub confirmed_tier: BootstrapTier,
    pub confirmed_policy_revision: DecimalU64,
}
const OWNER_FIELDS: [&str; 15] = [
    "--owner-launch-version",
    "--store-root",
    "--project-id",
    "--session-id",
    "--store-device",
    "--store-inode",
    "--bootstrap-name",
    "--bootstrap-device",
    "--bootstrap-inode",
    "--bootstrap-sha256",
    "--confirmed-source-path",
    "--confirmed-source-device",
    "--confirmed-source-inode",
    "--confirmed-tier",
    "--confirmed-policy-revision",
];
fn text(value: &OsString) -> Result<&str, LaunchArgumentsError> {
    value
        .to_str()
        .filter(|s| !s.is_empty() && !s.chars().any(char::is_control))
        .ok_or(LaunchArgumentsError)
}
fn decimal(value: &OsString) -> Result<DecimalU64, LaunchArgumentsError> {
    let s = text(value)?;
    if s.len() > 20 || !s.bytes().all(|b| b.is_ascii_digit()) || (s.len() > 1 && s.starts_with('0'))
    {
        return Err(LaunchArgumentsError);
    }
    Ok(DecimalU64::new(
        s.parse().map_err(|_| LaunchArgumentsError)?,
    ))
}
fn absolute(value: &OsString) -> Result<PathBuf, LaunchArgumentsError> {
    let s = text(value)?;
    if s.len() > 4096
        || !s.starts_with('/')
        || s == "/"
        || s.split('/')
            .skip(1)
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(LaunchArgumentsError);
    }
    Ok(s.into())
}
pub(crate) fn parse_sha256(s: &str) -> Result<[u8; 32], LaunchArgumentsError> {
    if s.len() != 64
        || !s
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(LaunchArgumentsError);
    }
    let mut hash = [0; 32];
    for (i, byte) in hash.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|_| LaunchArgumentsError)?;
    }
    Ok(hash)
}
pub fn parse_launch_args(args: &[OsString]) -> Result<LaunchArguments, LaunchArgumentsError> {
    if (args.len() == 6 || args.len() == 7)
        && args[0] == "--store-root"
        && args[2] == "--project-id"
        && args[4] == "--session-id"
        && (args.len() == 6 || args[6] == "--source-recovery-fd2")
    {
        // Preserve the legacy path representation; strict owner paths are separate.
        return Ok(LaunchArguments::Legacy {
            store: ServiceConfig {
                store_root: (&args[1]).into(),
                project_id: ProjectId::new(args[3].to_str().ok_or(LaunchArgumentsError)?)
                    .map_err(|_| LaunchArgumentsError)?,
                session_id: SessionId::new(args[5].to_str().ok_or(LaunchArgumentsError)?)
                    .map_err(|_| LaunchArgumentsError)?,
            },
            recovery_fd2: args.len() == 7,
        });
    }
    if args.len() != 31
        || args[30] != "--source-recovery-fd2"
        || OWNER_FIELDS
            .iter()
            .enumerate()
            .any(|(i, field)| args[i * 2] != *field)
        || args[1] != "1"
    {
        return Err(LaunchArgumentsError);
    }
    let name = text(&args[13])?;
    if name.len() > 255 || name.contains('/') || name == "." || name == ".." {
        return Err(LaunchArgumentsError);
    }
    let tier = match text(&args[27])? {
        "read_only" => BootstrapTier::ReadOnly,
        "read_create" => BootstrapTier::ReadCreate,
        "read_create_build" => BootstrapTier::ReadCreateBuild,
        "read_create_build_external" => BootstrapTier::ReadCreateBuildExternal,
        _ => return Err(LaunchArgumentsError),
    };
    Ok(LaunchArguments::OwnerV1(OwnerLaunchArguments {
        store: ServiceConfig {
            store_root: absolute(&args[3])?,
            project_id: ProjectId::new(text(&args[5])?).map_err(|_| LaunchArgumentsError)?,
            session_id: SessionId::new(text(&args[7])?).map_err(|_| LaunchArgumentsError)?,
        },
        store_identity: BootstrapIdentity {
            device: decimal(&args[9])?,
            inode: decimal(&args[11])?,
        },
        bootstrap_name: name.into(),
        bootstrap_file: ExpectedBootstrapFile {
            identity: BootstrapIdentity {
                device: decimal(&args[15])?,
                inode: decimal(&args[17])?,
            },
            sha256: parse_sha256(text(&args[19])?)?,
        },
        confirmed_source_path: absolute(&args[21])?,
        confirmed_source_identity: BootstrapIdentity {
            device: decimal(&args[23])?,
            inode: decimal(&args[25])?,
        },
        confirmed_tier: tier,
        confirmed_policy_revision: decimal(&args[29])?,
    }))
}
#[cfg(test)]
mod tests;
