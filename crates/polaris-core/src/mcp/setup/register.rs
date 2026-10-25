//! Approved proposals: the approval text, its payload binding and the save order.
use super::*;
use crate::isolated_workspace::Limits;
#[cfg(unix)]
use crate::mcp::runtime::RuntimeConfig;
#[cfg(unix)]
use polaris_mcp::registration::Registry;
use polaris_mcp::{registration::canonical_hash, store::Store};
use std::collections::BTreeMap;
#[cfg(unix)]
use std::collections::BTreeSet;

/// Owner startup's stdio rules, applied again before a proposal is saved.
pub struct Admission<'a> {
    pub store: &'a Store,
    pub home: &'a Path,
    pub runtime_roots: &'a [PathBuf],
    pub excluded_roots: &'a [PathBuf],
    pub limits: Limits,
}

#[cfg(unix)]
fn references(server: &Server) -> BTreeSet<String> {
    match &server.transport {
        Destination::Stdio { secret_env, .. } => secret_env.values().cloned().collect(),
        Destination::StreamableHttp { secret_headers, .. } => {
            secret_headers.values().cloned().collect()
        }
    }
}

#[cfg(unix)]
fn remove(store: &Store, stale: BTreeSet<String>) -> Result<(), polaris_mcp::Error> {
    if stale.is_empty() {
        return Ok(());
    }
    let revision = store.load_secrets()?.map_or(0, |s| s.revision());
    store.update_secrets(revision, stale.into_iter().map(|r| (r, None)).collect())
}

fn visible(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

impl Proposal {
    /// Everything saved except secret values and their MACs.
    pub fn payload_hash(&self) -> Result<String, polaris_mcp::Error> {
        let secrets: Vec<_> = self
            .secrets
            .iter()
            .map(|b| (&b.reference, &b.source, &b.key_path))
            .collect();
        canonical_hash(&serde_json::json!({
            "server": self.server,
            "secrets": secrets,
            "expected": [self.expected_registry, self.expected_secrets],
            "replaces": self.replaces,
        }))
    }

    /// Every saved field, untruncated, in the approval card's order. Source
    /// text is shown with control characters escaped.
    pub fn description(&self, name: &str) -> String {
        let server = &self.server;
        let path = |path: &Path| visible(&path.to_string_lossy());
        let mut lines = vec![format!("Name: {} ({})", visible(name), server.server_id)];
        if let Some(app) = crate::mcp::apps::find(server) {
            lines.push(format!(
                "App: {} (definition unverified on a real device; arbitrary code execution tools require confirmation for every call)",
                app.label
            ));
        }
        if self.replaces {
            lines.push("Replacement: replaces the existing registration from the same source with this configuration".into());
        }
        let mut delivery = BTreeMap::new();
        let (effects, network) = match &server.transport {
            Destination::StreamableHttp {
                url,
                secret_headers,
            } => {
                lines.push(format!("Endpoint: {}", visible(url)));
                if url.starts_with("http://") {
                    lines.push(
                        "Note: localhost is not a trust boundary. Other processes running as the same user on this Mac can also connect"
                            .into(),
                    );
                }
                for (header, reference) in secret_headers {
                    delivery.insert(reference, format!("{} header", visible(header)));
                }
                ("No file writes or command launches", "used")
            }
            Destination::Stdio {
                command,
                args,
                cwd,
                sandbox_profile,
                secret_env,
                readable_roots,
                executable_sha256,
                script,
                script_sha256,
            } => {
                lines.push(format!("Command: {}", path(command)));
                lines.push(format!("Arguments: {args:?}"));
                if let Some(digest) = executable_sha256 {
                    lines.push(format!("Executable SHA-256: {digest}"));
                }
                if let (Some(script), Some(digest)) = (script, script_sha256) {
                    lines.push(format!("Script: {} (SHA-256 {digest})", path(script)));
                }
                lines.push(format!("Working directory: {}", path(cwd)));
                for root in readable_roots {
                    lines.push(format!("Readable locations: {}", path(root)));
                }
                lines.push(format!("sandbox: {sandbox_profile}"));
                for (variable, reference) in secret_env {
                    delivery.insert(
                        reference,
                        format!("Environment variable {}", visible(variable)),
                    );
                }
                (
                    "Launches a command on this computer (writes restricted to the working directory)",
                    match sandbox_profile.as_str() {
                        "full-access" => "used",
                        "loopback" => "Uses only connections and listeners within this Mac",
                        _ => "not used",
                    },
                )
            }
        };
        if self.secrets.is_empty() {
            lines.push("Authentication: none".into());
        }
        for binding in &self.secrets {
            let prefix = if binding.key_path.last().is_some_and(|k| k == "bearer_token") {
                "with a leading \"Bearer \" "
            } else {
                ""
            };
            lines.push(format!(
                "Authentication: sends the value from source {} at {} {prefix}as {}. The value is stored but hidden (reference {})",
                path(&binding.source),
                visible(&binding.key_path.join(".")),
                delivery.get(&binding.reference).map_or("", String::as_str),
                binding.reference
            ));
        }
        let capabilities = &server.capabilities;
        let (enabled, disabled): (Vec<_>, Vec<_>) = [
            (capabilities.catalog_metadata, "catalog retrieval"),
            (capabilities.resource_read, "resource reads"),
            (capabilities.prompt_selection, "prompt selection"),
            (
                capabilities.tool_call,
                "tool calls (confirmation required for every call)",
            ),
        ]
        .into_iter()
        .partition(|(on, _)| *on);
        let labels = |items: Vec<(bool, &str)>| {
            items
                .into_iter()
                .map(|(_, label)| label)
                .collect::<Vec<_>>()
                .join("、")
        };
        lines.push(format!("Enabled operations: {}", labels(enabled)));
        if !disabled.is_empty() {
            lines.push(format!("Disabled operations: {}", labels(disabled)));
        }
        lines.push(format!("This computer: {effects}"));
        lines.push(format!("Network: {network}"));
        lines.push(format!(
            "Transport: {}",
            serde_json::to_value(server.protocol_profile)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default()
        ));
        let t = &server.timeouts;
        lines.push(format!(
            "Timeouts: connection {} s ({} retries), catalog {} s, operation {} s, catalog retention {} s",
            t.connect_seconds,
            t.connect_retries,
            t.catalog_seconds,
            t.operation_seconds,
            t.catalog_ttl_seconds
        ));
        let projects: Vec<_> = server.enabled_projects.iter().map(|p| visible(p)).collect();
        lines.push(format!("Target project: {}", projects.join("、")));
        lines.push("Effective from the next connection. Not added to the current run".into());
        lines.join("\n")
    }
}

#[cfg(unix)]
impl Detector {
    /// Values are re-read and must match their MACs. Both documents must still
    /// be at the detected revisions; secrets precede the registry CAS and a
    /// replaced server's stale references go last.
    pub fn register(
        &self,
        proposal: &Proposal,
        admission: &Admission<'_>,
    ) -> Result<u64, polaris_mcp::Error> {
        use polaris_mcp::Error;
        let values = self.secret_values(proposal)?;
        let (store, server) = (admission.store, &proposal.server);
        if let Destination::Stdio { cwd, .. } = &server.transport
            && store.server_directory(&server.server_id)? != *cwd
        {
            return Err(Error::InvalidRegistration);
        }
        let project = server
            .enabled_projects
            .first()
            .ok_or(Error::InvalidRegistration)?;
        RuntimeConfig::validate_servers(
            [server],
            project,
            admission.home,
            admission.runtime_roots,
            admission.excluded_roots,
            admission.limits,
        )?;
        let current = store.load_registry()?;
        let expected = current.as_ref().map_or(0, |r| r.revision);
        if expected != proposal.expected_registry {
            return Err(Error::RevisionConflict);
        }
        let mut servers = current.map_or_else(Vec::new, |r| r.servers);
        let live: BTreeSet<String> = servers.iter().flat_map(references).collect();
        let replaced = servers
            .iter()
            .position(|s| s.server_id == server.server_id)
            .map(|i| servers.remove(i));
        if replaced.is_some() != proposal.replaces {
            return Err(Error::RevisionConflict);
        }
        servers.push(server.clone());
        let registry = Registry {
            schema_version: 1,
            revision: expected.checked_add(1).ok_or(Error::Limit)?,
            servers,
        };
        registry.validate()?;
        if store.load_secrets()?.map_or(0, |s| s.revision()) != proposal.expected_secrets {
            return Err(Error::RevisionConflict);
        }
        let written: BTreeSet<String> = values.iter().map(|(r, _)| r.clone()).collect();
        if !written.is_disjoint(&live) {
            return Err(Error::InvalidRegistration);
        }
        if !values.is_empty() {
            store.update_secrets(
                proposal.expected_secrets,
                values.into_iter().map(|(r, v)| (r, Some(v))).collect(),
            )?;
        }
        if let Err(error) = store.save_registry(expected, &registry) {
            // Unreferenced leftovers are harmless; removal is best effort.
            let _ = remove(store, written);
            return Err(error);
        }
        if let Some(old) = replaced {
            let kept = references(server);
            let _ = remove(
                store,
                references(&old)
                    .into_iter()
                    .filter(|r| !kept.contains(r))
                    .collect(),
            );
        }
        Ok(registry.revision)
    }
}
