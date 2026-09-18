//! Owner-served MCP registry and write-only secrets. Saved documents apply from
//! the next owner start; this owner keeps its startup snapshot.
use super::*;
use polaris_desktop_protocol::mcp_settings::McpRegistryState;

/// Owner startup's roots and limits, reused to reject registrations that the
/// next owner start would refuse.
pub(crate) struct McpSettings {
    pub store: polaris_mcp::store::Store,
    pub home: PathBuf,
    pub active_registry_revision: u64,
    pub active_secrets_revision: u64,
    pub runtime_roots: Vec<PathBuf>,
    pub excluded_roots: Vec<PathBuf>,
    pub limits: polaris_core::isolated_workspace::Limits,
}

impl DesktopService {
    pub(crate) fn attach_mcp_settings(&mut self, settings: McpSettings) {
        if let Some(engine) = &mut self.engine {
            engine.mcp_settings = Some(settings);
        }
    }
}

const MAX_SERVER_BYTES: usize = 256 * 1024;
const MAX_REFERENCES: usize = 1024;

// Settings faults are this surface's errors only: never StorageFailed or
// RecoveryRequired, which would stop the session owner and its run.
fn settings_error(value: polaris_mcp::Error) -> ProtocolError {
    use polaris_mcp::Error;
    error(match value {
        Error::RevisionConflict => ErrorCode::RevisionConflict,
        Error::InvalidRegistration | Error::Auth | Error::Limit => ErrorCode::InvalidRequest,
        _ => ErrorCode::CapabilityUnavailable,
    })
}

impl Engine {
    pub(super) fn mcp_settings_request(
        &mut self,
        request: &Request,
    ) -> Result<SuccessResult, ProtocolError> {
        let settings = self
            .mcp_settings
            .as_ref()
            .ok_or_else(|| error(ErrorCode::CapabilityUnavailable))?;
        if self.draining && !matches!(request.body, RequestBody::McpRegistryRead(_)) {
            return Err(error(ErrorCode::SessionBusy));
        }
        let store = &settings.store;
        match &request.body {
            RequestBody::McpRegistryRead(_) => {}
            RequestBody::McpRegistrySave(_, params) => {
                let servers = params
                    .servers
                    .iter()
                    .map(|server| serde_json::from_value(serde_json::Value::Object(server.clone())))
                    .collect::<Result<_, _>>()
                    .map_err(|_| error(ErrorCode::InvalidRequest))?;
                let expected = params.expected_revision.get();
                let registry = polaris_mcp::registration::Registry {
                    schema_version: 1,
                    revision: expected
                        .checked_add(1)
                        .ok_or_else(|| error(ErrorCode::InvalidRequest))?,
                    servers,
                };
                registry.validate().map_err(settings_error)?;
                // Bound the next response before writing. Only new destinations
                // and new enablement for this project are walked here; other
                // toggles and deletions never block on a later environment change.
                if serde_json::to_vec(&registry.servers)
                    .map_or(true, |b| b.len() > MAX_SERVER_BYTES)
                {
                    return Err(error(ErrorCode::InvalidRequest));
                }
                let current = store.load_registry().map_err(settings_error)?;
                let project = self
                    .store
                    .snapshot()
                    .map_err(store_error)?
                    .marker
                    .project_id;
                polaris_core::mcp::runtime::RuntimeConfig::validate_servers(
                    registry.servers.iter().filter(|server| {
                        current.as_ref().is_none_or(|current| {
                            !current.servers.iter().any(|saved| {
                                saved.server_id == server.server_id
                                    && saved.transport == server.transport
                                    && (saved.enabled_projects.contains(project.as_str())
                                        || !server.enabled_projects.contains(project.as_str()))
                            })
                        })
                    }),
                    project.as_str(),
                    &settings.home,
                    &settings.runtime_roots,
                    &settings.excluded_roots,
                    settings.limits,
                )
                .map_err(|_| error(ErrorCode::InvalidRequest))?;
                store
                    .save_registry(expected, &registry)
                    .map_err(settings_error)?;
            }
            RequestBody::McpSecretSet(_, params) => {
                if store
                    .load_secrets()
                    .map_err(settings_error)?
                    .is_some_and(|secrets| secrets.references().count() >= MAX_REFERENCES)
                {
                    return Err(error(ErrorCode::InvalidRequest));
                }
                store
                    .update_secret(
                        params.expected_revision.get(),
                        &params.reference,
                        Some(polaris_mcp::store::SecretValue::new(params.value.expose())),
                    )
                    .map_err(settings_error)?
            }
            RequestBody::McpSecretDelete(_, params) => store
                .update_secret(params.expected_revision.get(), &params.reference, None)
                .map_err(settings_error)?,
            _ => return Err(error(ErrorCode::InvalidRequest)),
        }
        let state = self.mcp_registry_state()?;
        Ok(match request.body {
            RequestBody::McpRegistrySave(..) => SuccessResult::McpRegistrySave(state),
            RequestBody::McpSecretSet(..) => SuccessResult::McpSecretSet(state),
            RequestBody::McpSecretDelete(..) => SuccessResult::McpSecretDelete(state),
            _ => SuccessResult::McpRegistryRead(state),
        })
    }

    /// Saved documents with this owner's startup revisions, bounded for one frame.
    pub(super) fn mcp_registry_state(&self) -> Result<McpRegistryState, ProtocolError> {
        let settings = self
            .mcp_settings
            .as_ref()
            .ok_or_else(|| error(ErrorCode::CapabilityUnavailable))?;
        let store = &settings.store;
        let snapshot = store.load_snapshot().map_err(settings_error)?;
        let project = self
            .store
            .snapshot()
            .map_err(store_error)?
            .marker
            .project_id;
        // A busy or damaged index leaves every project server unchecked.
        let indexed = snapshot.registry.as_ref().and_then(|registry| {
            store
                .load_catalog_index()
                .ok()
                .flatten()
                .map(|index| index.verified(registry))
        });
        let (registry_revision, servers) = snapshot.registry.map_or((0, vec![]), |registry| {
            (registry.revision, registry.servers)
        });
        let unchecked_servers = servers
            .iter()
            .filter(|server| {
                server.enabled_projects.contains(project.as_str())
                    && indexed
                        .as_ref()
                        .is_none_or(|indexed| !indexed.contains_key(&server.server_id))
            })
            .map(|server| server.server_id.clone())
            .collect();
        let state = McpRegistryState {
            registry_revision: DecimalU64::new(registry_revision),
            servers: servers
                .iter()
                .map(|server| match serde_json::to_value(server) {
                    Ok(serde_json::Value::Object(object)) => Ok(object),
                    _ => Err(error(ErrorCode::CapabilityUnavailable)),
                })
                .collect::<Result<_, _>>()?,
            secrets_revision: DecimalU64::new(
                snapshot.secrets.as_ref().map_or(0, |s| s.revision()),
            ),
            secret_references: snapshot
                .secrets
                .as_ref()
                .map_or_else(Vec::new, |s| s.references().map(str::to_owned).collect()),
            active_registry_revision: DecimalU64::new(settings.active_registry_revision),
            active_secrets_revision: DecimalU64::new(settings.active_secrets_revision),
            unchecked_servers,
        };
        // Leave room in one frame for the response envelope.
        if serde_json::to_vec(&state).map_or(true, |bytes| bytes.len() > 512 * 1024) {
            return Err(error(ErrorCode::CapabilityUnavailable));
        }
        Ok(state)
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use polaris_desktop_protocol::{
        ids::{ClientId, DecimalU64, RequestId},
        mcp_settings::{McpRegistrySave, McpSecretDelete, McpSecretSet},
    };
    use serde_json::json;

    fn engine(home: &std::path::Path) -> (PrototypeRoot, Engine) {
        let root = PrototypeRoot::new().unwrap();
        let mut engine = Engine::create(&root, Options::default()).unwrap();
        engine.connection = ConnectionState::Ready;
        engine.client = Some(ClientId::new("client").unwrap());
        engine.mcp_settings = Some(McpSettings {
            store: polaris_mcp::store::Store::open(home).unwrap(),
            home: home.to_owned(),
            active_registry_revision: 0,
            active_secrets_revision: 0,
            runtime_roots: ["/bin", "/usr/bin", "/usr/lib", "/System/Library"]
                .map(PathBuf::from)
                .to_vec(),
            excluded_roots: vec![],
            limits: polaris_core::isolated_workspace::Limits {
                max_entries: 8,
                max_files: 4,
                max_file_bytes: 1024,
                max_total_bytes: 4096,
                max_depth: 2,
            },
        });
        (root, engine)
    }

    fn request(id: &str, body: RequestBody) -> Request {
        Request {
            protocol_version: Default::default(),
            client_id: ClientId::new("client").unwrap(),
            request_id: RequestId::new(id).unwrap(),
            body,
        }
    }

    fn state(result: SuccessResult) -> polaris_desktop_protocol::mcp_settings::McpRegistryState {
        match result {
            SuccessResult::McpRegistryRead(s)
            | SuccessResult::McpRegistrySave(s)
            | SuccessResult::McpSecretSet(s)
            | SuccessResult::McpSecretDelete(s) => s,
            _ => panic!("unexpected result"),
        }
    }

    #[test]
    fn settings_are_cas_saved_without_returning_or_recording_secret_values() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().canonicalize().unwrap();
        let (_root, mut engine) = engine(&home);
        let session = engine.session.clone();
        let before = engine.store.snapshot().unwrap();
        let mut events = vec![];
        let set = engine
            .handle(
                &request(
                    "secret",
                    RequestBody::McpSecretSet(
                        session.clone(),
                        serde_json::from_value::<McpSecretSet>(json!({
                            "expected_revision":"0","reference":"credential","value":"synthetic-secret"
                        }))
                        .unwrap(),
                    ),
                ),
                &mut events,
            )
            .unwrap();
        let wire = serde_json::to_string(&set).unwrap();
        assert!(!wire.contains("synthetic-secret"));
        let set = state(set);
        assert_eq!(set.secrets_revision, DecimalU64::new(1));
        assert_eq!(set.secret_references, ["credential"]);
        let server = json!({"server_id":"fixture","enabled_projects":["fake-project"],
            "transport":{"kind":"streamable_http","url":"https://example.com/mcp","secret_headers":{"authorization":"credential"}},
            "protocol_profile":"discover_2026",
            "capabilities":{"catalog_metadata":true,"resource_read":false,"prompt_selection":false,"tool_call":false},
            "timeouts":{"catalog_ttl_seconds":300,"connect_seconds":10,"catalog_seconds":30,"operation_seconds":120,"connect_retries":2}});
        let save = |engine: &mut Engine, id: &str, expected: u64, server: serde_json::Value| {
            engine.handle(
                &request(
                    id,
                    RequestBody::McpRegistrySave(
                        session.clone(),
                        McpRegistrySave {
                            expected_revision: DecimalU64::new(expected),
                            servers: vec![server.as_object().unwrap().clone()],
                        },
                    ),
                ),
                &mut vec![],
            )
        };
        let saved = state(save(&mut engine, "save", 0, server.clone()).unwrap());
        assert_eq!(saved.registry_revision, DecimalU64::new(1));
        assert_eq!(saved.servers[0], *server.as_object().unwrap());
        assert_eq!(saved.active_registry_revision, DecimalU64::new(0));
        assert_eq!(saved.unchecked_servers, ["fixture"]);
        assert_eq!(
            save(&mut engine, "stale", 0, server.clone())
                .unwrap_err()
                .code,
            ErrorCode::RevisionConflict
        );
        let mut unknown = server.clone();
        unknown["transport"]["secret_headers"]["authorization"] = json!("missing");
        assert_eq!(
            save(&mut engine, "unknown-reference", 1, unknown)
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
        let mut field = server;
        field["unexpected"] = json!(true);
        assert_eq!(
            save(&mut engine, "unknown-field", 1, field)
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
        let delete = |engine: &mut Engine, id: &str| {
            engine.handle(
                &request(
                    id,
                    RequestBody::McpSecretDelete(
                        session.clone(),
                        McpSecretDelete {
                            expected_revision: DecimalU64::new(1),
                            reference: "credential".into(),
                        },
                    ),
                ),
                &mut vec![],
            )
        };
        assert_eq!(
            delete(&mut engine, "referenced").unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        let read = state(
            engine
                .handle(
                    &request("read", RequestBody::McpRegistryRead(session.clone())),
                    &mut vec![],
                )
                .unwrap(),
        );
        assert_eq!(read, saved);
        assert!(events.is_empty());
        let after = engine.store.snapshot().unwrap();
        assert_eq!(after.marker, before.marker);
        assert!(after.state.requests.is_empty());
        assert!(
            !std::fs::read_to_string(home.join(".polaris/mcp/servers.json"))
                .unwrap()
                .contains("synthetic-secret")
        );
    }

    #[test]
    fn store_faults_and_unstartable_registrations_never_poison_the_owner() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().canonicalize().unwrap();
        let (_root, mut engine) = engine(&home);
        let session = engine.session.clone();
        let cwd = home.join("server");
        std::fs::create_dir(&cwd).unwrap();
        std::fs::set_permissions(&cwd, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        for (command, profile) in [
            ("/bin/sh", "strict"),
            ("/opt/homebrew/bin/npx", "read-only"),
        ] {
            let server = json!({"server_id":"local","enabled_projects":["project"],
                "transport":{"kind":"stdio","command":command,"args":[],"cwd":cwd,"sandbox_profile":profile,"secret_env":{}},
                "capabilities":{"catalog_metadata":true,"resource_read":false,"prompt_selection":false,"tool_call":false}});
            let code = engine
                .handle(
                    &request(
                        "unstartable",
                        RequestBody::McpRegistrySave(
                            session.clone(),
                            McpRegistrySave {
                                expected_revision: DecimalU64::new(0),
                                servers: vec![server.as_object().unwrap().clone()],
                            },
                        ),
                    ),
                    &mut vec![],
                )
                .unwrap_err()
                .code;
            assert_eq!(code, ErrorCode::InvalidRequest, "{command} {profile}");
        }
        assert!(!home.join(".polaris/mcp/servers.json").exists());
        let mut server = json!({"server_id":"local","enabled_projects":["project"],
            "transport":{"kind":"stdio","command":"/bin/sh","args":[],"cwd":cwd,"sandbox_profile":"read-only","secret_env":{}},
            "capabilities":{"catalog_metadata":true,"resource_read":false,"prompt_selection":false,"tool_call":false}});
        let mut save = |id: &str, expected: u64, server: &serde_json::Value| {
            engine.handle(
                &request(
                    id,
                    RequestBody::McpRegistrySave(
                        session.clone(),
                        McpRegistrySave {
                            expected_revision: DecimalU64::new(expected),
                            servers: vec![server.as_object().unwrap().clone()],
                        },
                    ),
                ),
                &mut vec![],
            )
        };
        save("valid", 0, &server).unwrap();
        std::fs::write(cwd.join(".env"), "synthetic").unwrap();
        server["capabilities"]["tool_call"] = json!(true);
        server["enabled_projects"] = json!(["project", "other"]);
        save("toggle", 1, &server).unwrap();
        server["enabled_projects"] = json!(["project", "other", "fake-project"]);
        assert_eq!(
            save("enable-here", 2, &server).unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        server["enabled_projects"] = json!(["project", "other"]);
        server["transport"]["args"] = json!(["-c", "true"]);
        assert_eq!(
            save("destination", 2, &server).unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        std::fs::write(home.join(".polaris/mcp/secrets.json.tmp"), b"interrupted").unwrap();
        let code = engine
            .handle(
                &request("read", RequestBody::McpRegistryRead(session.clone())),
                &mut vec![],
            )
            .unwrap_err()
            .code;
        assert!(!matches!(
            code,
            ErrorCode::StorageFailed | ErrorCode::RecoveryRequired
        ));
        engine
            .handle(
                &request("snapshot", RequestBody::SessionSnapshot(session)),
                &mut vec![],
            )
            .unwrap();
    }

    #[test]
    fn settings_capability_is_advertised_only_with_a_store() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().canonicalize().unwrap();
        let (_root, mut engine) = engine(&home);
        let session = engine.session.clone();
        engine.connection = ConnectionState::AwaitingHello;
        engine.client = None;
        let hello = |engine: &mut Engine| match engine
            .handle(&request("hello", RequestBody::Hello), &mut vec![])
            .unwrap()
        {
            SuccessResult::Hello(hello) => hello.capabilities,
            _ => panic!("hello"),
        };
        assert!(hello(&mut engine).contains(&Capability::McpSettings));
        let root = PrototypeRoot::new().unwrap();
        let mut plain = Engine::create(&root, Options::default()).unwrap();
        assert!(!hello(&mut plain).contains(&Capability::McpSettings));
        assert_eq!(
            plain
                .handle(
                    &request("read", RequestBody::McpRegistryRead(session)),
                    &mut vec![]
                )
                .unwrap_err()
                .code,
            ErrorCode::CapabilityUnavailable
        );
    }
}
