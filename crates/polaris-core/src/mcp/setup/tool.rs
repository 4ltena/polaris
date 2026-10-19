//! Root-run `mcp_setup`. Arguments carry an operation and a candidate ID only;
//! results never include URLs, commands, paths or values.
use super::*;
#[cfg(unix)]
use crate::execution_owner::{RegistrationApproval, RegistrationError};
use crate::{
    approval::{Approver, Gate},
    execution_owner::RegistrationPort,
    isolated_workspace::Limits,
    mcp::operation::ExternalCeiling,
};
#[cfg(unix)]
use polaris_mcp::store::{Snapshot, Store};
#[cfg(unix)]
use serde::Deserialize;
use serde_json::json;
use std::{future::Future, sync::Mutex};

#[cfg(unix)]
const MAX_CANDIDATES: usize = 64;
/// Fixed host PATH; never the process or project environment.
pub const HOST_PATH: &[&str] = &["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"];

#[cfg(unix)]
#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Operation {
    Detect,
    Register { candidate: String },
}

/// Desktop runs use the owner's durable approval; CLI and TUI runs ask through
/// the host Gate and record the result in the session history.
pub enum Authorization {
    Owner(RegistrationPort),
    Local(ExternalCeiling),
}

pub struct SetupTool {
    #[cfg(unix)]
    detector: Detector,
    home: PathBuf,
    runtime_roots: Vec<PathBuf>,
    excluded_roots: Vec<PathBuf>,
    limits: Limits,
    project: String,
    authorization: Authorization,
    candidates: Mutex<BTreeMap<String, (String, Proposal)>>,
}

fn error(code: &str) -> String {
    json!({ "error": code }).to_string()
}

#[cfg(unix)]
const MAX_DESCRIPTION_BYTES: usize = 16 * 1024;

impl SetupTool {
    #[cfg(not(unix))]
    pub async fn call(
        &self,
        _arguments: &serde_json::Value,
        _cancelled: impl Future<Output = ()>,
        _gate: &mut Gate,
        _approver: &mut dyn Approver,
    ) -> Result<String, String> {
        Err(error("unavailable"))
    }

    pub fn new(
        home: PathBuf,
        runtime_roots: Vec<PathBuf>,
        excluded_roots: Vec<PathBuf>,
        limits: Limits,
        project: String,
        authorization: Authorization,
    ) -> Self {
        Self {
            #[cfg(unix)]
            detector: Detector::new(home.clone(), HOST_PATH.iter().map(PathBuf::from).collect()),
            home,
            runtime_roots,
            excluded_roots,
            limits,
            project,
            authorization,
            candidates: Mutex::default(),
        }
    }

    /// Detection reads a store only if one exists; only an approved save creates it.
    #[cfg(unix)]
    fn current(&self) -> Result<Snapshot, String> {
        if !Store::exists(&self.home) {
            return Ok(Snapshot {
                registry: None,
                secrets: None,
            });
        }
        Store::open(&self.home)
            .and_then(|store| store.load_snapshot())
            .map_err(|_| error("unavailable"))
    }

    /// Both arms are JSON tool results; only `Ok` counts as success. A new
    /// detection replaces earlier candidate IDs.
    #[cfg(unix)]
    pub async fn call(
        &self,
        arguments: &serde_json::Value,
        cancelled: impl Future<Output = ()>,
        gate: &mut Gate,
        approver: &mut dyn Approver,
    ) -> Result<String, String> {
        let operation: Operation =
            serde_json::from_value(arguments.clone()).map_err(|_| error("invalid_arguments"))?;
        match operation {
            Operation::Detect => {
                let found = self.detector.detect(&self.project, &self.current()?);
                let mut candidates = self.candidates.lock().map_err(|_| error("unavailable"))?;
                candidates.clear();
                let listed: Vec<_> = found
                    .into_iter()
                    .take(MAX_CANDIDATES)
                    .map(|detected| {
                        let name: String = detected
                            .name
                            .chars()
                            .filter(|c| !c.is_control())
                            .take(64)
                            .collect();
                        let mut entry = json!({
                            "candidate": detected.id,
                            "name": name,
                            "transport": detected.transport,
                            "origin": "detected",
                        });
                        if let Some(reason) = detected.reason {
                            entry["reason"] = json!(reason.as_str());
                        }
                        if let Some(proposal) = detected.proposal {
                            candidates.insert(detected.id, (detected.name, proposal));
                        }
                        entry
                    })
                    .collect();
                Ok(json!({ "candidates": listed }).to_string())
            }
            Operation::Register { candidate } => {
                let (name, mut proposal) = self
                    .candidates
                    .lock()
                    .map_err(|_| error("unavailable"))?
                    .remove(&candidate)
                    .ok_or_else(|| error("unknown_candidate"))?;
                // An earlier registration moves the revisions. Rebind the same
                // source entry before asking, so the card shows what is saved.
                let current = self.current()?;
                if (proposal.expected_registry, proposal.expected_secrets)
                    != (
                        current.registry.as_ref().map_or(0, |r| r.revision),
                        current.secrets.as_ref().map_or(0, |s| s.revision()),
                    )
                {
                    let server_id = proposal.server.server_id.clone();
                    proposal = self
                        .detector
                        .detect(&self.project, &current)
                        .into_iter()
                        .find_map(|d| d.proposal.filter(|p| p.server.server_id == server_id))
                        .ok_or_else(|| error("registry_changed"))?;
                }
                let description = proposal.description(&name);
                if description.len() > MAX_DESCRIPTION_BYTES {
                    return Err(error("too_large"));
                }
                let approval = RegistrationApproval {
                    description,
                    scope: self.project.clone(),
                    payload_hash: proposal.payload_hash().map_err(|_| error("invalid"))?,
                };
                let grant = match &self.authorization {
                    Authorization::Owner(port) => {
                        Some(port.authorize(approval, cancelled).await.map_err(|e| {
                            error(match e {
                                RegistrationError::Denied => "denied",
                                RegistrationError::Cancelled => "cancelled",
                                RegistrationError::Invalid => "too_large",
                                RegistrationError::Unavailable => "unavailable",
                            })
                        })?)
                    }
                    // The reason carries the full card; the model gets only a code.
                    Authorization::Local(ceiling) => {
                        gate.check_mcp_registration_async(
                            *ceiling,
                            &approval.description,
                            approver,
                        )
                        .await
                        .map_err(|_| error("denied"))?;
                        None
                    }
                };
                let saved = Store::open(&self.home).and_then(|store| {
                    self.detector.register(
                        &proposal,
                        &Admission {
                            store: &store,
                            home: &self.home,
                            runtime_roots: &self.runtime_roots,
                            excluded_roots: &self.excluded_roots,
                            limits: self.limits,
                        },
                    )
                });
                drop(grant);
                let revision = saved.map_err(|e| {
                    error(match e {
                        polaris_mcp::Error::Auth => "credential_changed",
                        polaris_mcp::Error::Protection => "unsafe_location",
                        polaris_mcp::Error::RevisionConflict => "registry_changed",
                        polaris_mcp::Error::InvalidRegistration => "invalid",
                        _ => "unavailable",
                    })
                })?;
                Ok(json!({
                    "registered": proposal.server.server_id,
                    "registry_revision": revision.to_string(),
                    "applies": "next_connection",
                })
                .to_string())
            }
        }
    }
}
