//! Settings-screen detection and registration. The user's action there is the
//! approval; reads, hashing and root walks stay on a worker thread.
use super::*;
use polaris_core::mcp::setup::{Admission, Detected, Detector, HOST_PATH, Proposal, Reason};
use polaris_desktop_protocol::mcp_settings::{
    MAX_SETUP_CANDIDATES, McpSetupCandidate, McpSetupCandidates, McpSetupReason, McpSetupStatus,
    McpSetupTransport,
};
use std::collections::BTreeMap;

const MAX_DESCRIPTION_BYTES: usize = 16 * 1024;

enum Outcome {
    Detected(Result<Vec<Detected>, polaris_mcp::Error>),
    Registered(Result<u64, polaris_mcp::Error>),
}

pub(crate) struct SetupJob {
    request: Request,
    worker: Option<std::thread::JoinHandle<Outcome>>,
}

impl Drop for SetupJob {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take()
            && let Err(payload) = worker.join()
        {
            std::mem::forget(payload);
        }
    }
}

/// The detector's key lives for this owner; candidate IDs for the latest
/// detection only.
#[derive(Default)]
pub(crate) struct McpSetupState {
    detector: Option<Arc<Detector>>,
    candidates: BTreeMap<String, Proposal>,
    pub(super) job: Option<SetupJob>,
}

fn reason(value: Reason) -> McpSetupReason {
    match value {
        Reason::LauncherNotSupported => McpSetupReason::LauncherNotSupported,
        Reason::EnvironmentReference => McpSetupReason::EnvironmentReference,
        Reason::CredentialInArguments => McpSetupReason::CredentialInArguments,
        Reason::UnsupportedTransport => McpSetupReason::UnsupportedTransport,
        Reason::UnsupportedAuth => McpSetupReason::UnsupportedAuth,
        Reason::CommandNotFound => McpSetupReason::CommandNotFound,
        Reason::UnsupportedInstallLocation => McpSetupReason::UnsupportedInstallLocation,
        Reason::AlreadyRegistered => McpSetupReason::AlreadyRegistered,
        // The CLI names the field that could not be read; the wire contract
        // does not carry that yet. It is folded into `Invalid` here rather
        // than extended, because the macOS side has not yet absorbed the
        // `RoleBinding.provider` → `runtime` change and a second contract
        // break before the first lands is two migrations, not one. The
        // extension is listed with that handoff.
        Reason::CommandNotAbsolute
        | Reason::CommandMissing
        | Reason::ArgumentNotText
        | Reason::UrlMissing
        | Reason::UrlNotUsable
        | Reason::Invalid => McpSetupReason::Invalid,
    }
}

impl Engine {
    pub(super) fn start_mcp_setup(&mut self, request: &Request) -> Result<(), ProtocolError> {
        validate_request(self.connection, request).map_err(|_| error(ErrorCode::InvalidRequest))?;
        let (RequestBody::McpSetupDetect(session) | RequestBody::McpSetupRegister(session, _)) =
            &request.body
        else {
            return Err(error(ErrorCode::InvalidRequest));
        };
        if self.client.as_ref() != Some(&request.client_id) || session != &self.session {
            return Err(error(ErrorCode::PermissionDenied));
        }
        let settings = self
            .mcp_settings
            .as_ref()
            .ok_or_else(|| error(ErrorCode::CapabilityUnavailable))?;
        if self.failed {
            return Err(error(ErrorCode::StorageFailed));
        }
        if self.draining
            || self.disconnected.load(Ordering::Acquire)
            || self.active.is_some()
            || self.mcp_catalog.is_some()
            || self.mcp_setup.job.is_some()
        {
            return Err(error(ErrorCode::SessionBusy));
        }
        let project = self
            .store
            .snapshot()
            .map_err(store_error)?
            .marker
            .project_id
            .as_str()
            .to_owned();
        let detector = self
            .mcp_setup
            .detector
            .get_or_insert_with(|| {
                Arc::new(Detector::new(
                    settings.home.clone(),
                    HOST_PATH.iter().map(PathBuf::from).collect(),
                ))
            })
            .clone();
        let work: Box<dyn FnOnce() -> Outcome + Send> = match &request.body {
            RequestBody::McpSetupRegister(_, target) => {
                // A stale or reused ID asks the screen to detect again.
                let proposal = self
                    .mcp_setup
                    .candidates
                    .remove(&target.candidate_id)
                    .ok_or_else(|| error(ErrorCode::RevisionConflict))?;
                let (home, runtime_roots, excluded_roots, limits) = (
                    settings.home.clone(),
                    settings.runtime_roots.clone(),
                    settings.excluded_roots.clone(),
                    settings.limits,
                );
                Box::new(move || {
                    Outcome::Registered(polaris_mcp::store::Store::open(&home).and_then(|store| {
                        detector.register(
                            &proposal,
                            &Admission {
                                store: &store,
                                home: &home,
                                runtime_roots: &runtime_roots,
                                excluded_roots: &excluded_roots,
                                limits,
                            },
                        )
                    }))
                })
            }
            _ => {
                let home = settings.home.clone();
                Box::new(move || {
                    Outcome::Detected(
                        polaris_mcp::store::Store::open(&home)
                            .and_then(|store| store.load_snapshot())
                            .map(|current| detector.detect(&project, &current)),
                    )
                })
            }
        };
        let worker = std::thread::Builder::new()
            .name("polaris-mcp-setup".into())
            .spawn(work)
            .map_err(|_| error(ErrorCode::CapabilityUnavailable))?;
        self.mcp_setup.job = Some(SetupJob {
            request: request.clone(),
            worker: Some(worker),
        });
        Ok(())
    }

    pub(super) fn poll_mcp_setup(
        &mut self,
        out: Option<&Outbox>,
        stopping: bool,
    ) -> Result<(), ServiceError> {
        if let Some(response) = self.finish_mcp_setup(stopping)?
            && let Some(out) = out
        {
            out.send(&response)?;
        }
        Ok(())
    }

    /// Answers once the worker ends; stopping suppresses the answer.
    fn finish_mcp_setup(&mut self, stopping: bool) -> Result<Option<Response>, ServiceError> {
        if !self
            .mcp_setup
            .job
            .as_ref()
            .and_then(|job| job.worker.as_ref())
            .is_some_and(|worker| worker.is_finished())
        {
            return Ok(None);
        }
        let mut job = self.mcp_setup.job.take().expect("setup job");
        let outcome = match job.worker.take().expect("finished worker").join() {
            Ok(Outcome::Detected(Err(_))) => Err(error(ErrorCode::CapabilityUnavailable)),
            Ok(Outcome::Detected(Ok(found))) => {
                self.mcp_setup.candidates.clear();
                let candidates = found
                    .into_iter()
                    .take(MAX_SETUP_CANDIDATES)
                    .map(|detected| self.setup_candidate(detected))
                    .collect();
                Ok(SuccessResult::McpSetupDetect(McpSetupCandidates {
                    candidates,
                }))
            }
            Ok(Outcome::Registered(Ok(_))) => self
                .mcp_registry_state()
                .map(SuccessResult::McpSetupRegister),
            Ok(Outcome::Registered(Err(e))) => {
                use polaris_mcp::Error;
                Err(error(match e {
                    Error::Auth | Error::RevisionConflict => ErrorCode::RevisionConflict,
                    Error::Protection | Error::InvalidRegistration | Error::Limit => {
                        ErrorCode::InvalidRequest
                    }
                    _ => ErrorCode::CapabilityUnavailable,
                }))
            }
            Err(payload) => {
                std::mem::forget(payload);
                Err(error(ErrorCode::CapabilityUnavailable))
            }
        };
        let suppress =
            stopping || self.draining || self.failed || self.disconnected.load(Ordering::Acquire);
        if suppress {
            return Ok(None);
        }
        Response::for_request(&job.request, outcome)
            .map(Some)
            .map_err(|_| ServiceError::Worker)
    }

    fn setup_candidate(&mut self, detected: Detected) -> McpSetupCandidate {
        let mut name: String = detected
            .name
            .chars()
            .filter(|c| !c.is_control())
            .take(64)
            .collect();
        if name.is_empty() {
            name = "-".into();
        }
        let transport = match detected.transport {
            "stdio" => McpSetupTransport::Stdio,
            "streamable_http" => McpSetupTransport::StreamableHttp,
            _ => McpSetupTransport::Unsupported,
        };
        let status = match detected.proposal {
            Some(proposal) => {
                let description = proposal.description(&detected.name);
                if description.len() > MAX_DESCRIPTION_BYTES {
                    McpSetupStatus::Unavailable {
                        reason: McpSetupReason::Invalid,
                    }
                } else {
                    self.mcp_setup
                        .candidates
                        .insert(detected.id.clone(), proposal);
                    McpSetupStatus::Importable { description }
                }
            }
            None => McpSetupStatus::Unavailable {
                reason: detected.reason.map_or(McpSetupReason::Invalid, reason),
            },
        };
        McpSetupCandidate {
            candidate_id: detected.id,
            name,
            transport,
            status,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_desktop_protocol::{
        ids::{ClientId, RequestId},
        mcp_settings::McpSetupTarget,
    };
    use std::time::{Duration, Instant};

    fn engine(home: &std::path::Path) -> (PrototypeRoot, Engine) {
        let root = PrototypeRoot::new().unwrap();
        let mut engine = Engine::create(&root, Options::default()).unwrap();
        engine.connection = ConnectionState::Ready;
        engine.client = Some(ClientId::new("client").unwrap());
        engine.mcp_settings = Some(mcp_settings::McpSettings {
            store: polaris_mcp::store::Store::open(home).unwrap(),
            home: home.to_owned(),
            active_registry_revision: 0,
            active_secrets_revision: 0,
            runtime_roots: vec![],
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

    fn request(body: RequestBody) -> Request {
        Request {
            protocol_version: Default::default(),
            client_id: ClientId::new("client").unwrap(),
            request_id: RequestId::new("setup").unwrap(),
            body,
        }
    }

    fn answer(engine: &mut Engine) -> Result<SuccessResult, ProtocolError> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(response) = engine.finish_mcp_setup(false).unwrap() {
                return response.outcome;
            }
            assert!(Instant::now() < deadline, "setup job did not answer");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn settings_detection_lists_reasons_and_registration_consumes_the_candidate() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().canonicalize().unwrap();
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(
            home.join(".codex/config.toml"),
            "[mcp_servers.docs]\nurl = \"https://docs.example.com/mcp\"\nhttp_headers = { authorization = \"Bearer synthetic-token\" }\n\n[mcp_servers.context7]\ncommand = \"npx\"\nargs = [\"-y\", \"context7\"]\n",
        )
        .unwrap();
        let (_root, mut engine) = engine(&home);
        let session = engine.session.clone();
        let detect = request(RequestBody::McpSetupDetect(session.clone()));
        engine.start_mcp_setup(&detect).unwrap();
        assert_eq!(
            engine.start_mcp_setup(&detect).unwrap_err().code,
            ErrorCode::SessionBusy
        );
        let Ok(SuccessResult::McpSetupDetect(found)) = answer(&mut engine) else {
            panic!("detect");
        };
        assert!(
            !serde_json::to_string(&found)
                .unwrap()
                .contains("synthetic-token")
        );
        let docs = found.candidates.iter().find(|c| c.name == "docs").unwrap();
        let McpSetupStatus::Importable { description } = &docs.status else {
            panic!("importable");
        };
        assert!(description.contains("https://docs.example.com/mcp"));
        assert!(found.candidates.iter().any(|c| c.name == "context7"
            && c.status
                == McpSetupStatus::Unavailable {
                    reason: McpSetupReason::LauncherNotSupported
                }));

        let register = request(RequestBody::McpSetupRegister(
            session,
            McpSetupTarget {
                candidate_id: docs.candidate_id.clone(),
            },
        ));
        engine.start_mcp_setup(&register).unwrap();
        let Ok(SuccessResult::McpSetupRegister(state)) = answer(&mut engine) else {
            panic!("register");
        };
        assert!(
            !serde_json::to_string(&state)
                .unwrap()
                .contains("synthetic-token")
        );
        assert_eq!(state.registry_revision.get(), 1);
        assert_eq!(state.secret_references.len(), 1);
        assert_eq!(
            engine.start_mcp_setup(&register).unwrap_err().code,
            ErrorCode::RevisionConflict
        );
        assert!(engine.mcp_setup.job.is_none());
    }
}
