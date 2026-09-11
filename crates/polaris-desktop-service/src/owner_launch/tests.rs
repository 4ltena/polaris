//! Existing-store owner composition and authority mismatch tests.
use super::*;
use crate::{ServiceConfig, launch_arguments::OwnerLaunchArguments};
use polaris_core::desktop_store::{
    BootstrapProvider, ExpectedBootstrapFile, InitialState, OwnerBootstrapDocument,
};
use polaris_desktop_protocol::{
    ids::*,
    snapshot::{Configuration, Draft},
};
use polaris_provider::{ProviderError, Token, TokenSource};
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
};

struct Tokens(AtomicUsize);
#[async_trait::async_trait]
impl TokenSource for Tokens {
    async fn token(&self) -> Result<Token, ProviderError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(ProviderError::Auth("unused synthetic adapter".into()))
    }
    async fn refreshed(&self) -> Result<Token, ProviderError> {
        self.token().await
    }
}
struct Clock;
impl SourceApplyClock for Clock {
    fn now_ms(&self) -> u64 {
        1
    }
}
fn identity(path: &Path) -> BootstrapIdentity {
    let m = fs::metadata(path).unwrap();
    BootstrapIdentity {
        device: DecimalU64::new(m.dev()),
        inode: DecimalU64::new(m.ino()),
    }
}
fn hash(body: &[u8]) -> [u8; 32] {
    let hex = polaris_core::conversation_state::content_hash(body);
    std::array::from_fn(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap())
}
fn limits() -> Limits {
    Limits {
        max_entries: 16,
        max_files: 8,
        max_file_bytes: 1024,
        max_total_bytes: 4096,
        max_depth: 4,
    }
}
struct Fixture {
    _temp: tempfile::TempDir,
    args: OwnerLaunchArguments,
    root: DesktopRoot,
    writer: Writer,
    helper: PackagedExecutionHelper,
    resources: TrustedFactoryResources,
    recovery: PinnedRecoveryBase,
    tokens: Arc<Tokens>,
}
impl Fixture {
    fn new(configured: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let store = base.join("store");
        let source = base.join("source");
        let recovery = base.join("recovery");
        for p in [&store, &source, &recovery] {
            fs::create_dir(p).unwrap();
            fs::set_permissions(p, fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::write(source.join("ordinary"), "input").unwrap();
        let root = DesktopRoot::open_owned(&store).unwrap();
        let project = ProjectId::new("project").unwrap();
        let session = SessionId::new("session").unwrap();
        let writer = root
            .create(
                project.clone(),
                session.clone(),
                InitialState {
                    draft: Draft {
                        draft_revision: DecimalU64::new(0),
                        text: String::new(),
                        attachment_ids: vec![],
                    },
                    configuration: Configuration {
                        history_mode: Default::default(),
                        configuration_revision: DecimalU64::new(1),
                        provider: if configured { "codex" } else { "unconfigured" }.into(),
                        model: if configured { "gpt-6-astra" } else { "" }.into(),
                        effort: if configured { "medium" } else { "none" }.into(),
                    },
                    policy_revision: DecimalU64::new(0),
                },
            )
            .unwrap();
        let doc = OwnerBootstrapDocument {
            history_mode: Default::default(),
            schema_version: 1,
            project_id: project.clone(),
            session_id: session.clone(),
            store_identity: identity(&store),
            source_path: source.to_str().unwrap().into(),
            source_identity: identity(&source),
            tier: BootstrapTier::ReadCreate,
            policy_revision: DecimalU64::new(0),
            configuration_revision: DecimalU64::new(1),
            provider: BootstrapProvider::Codex,
            model: None,
            effort: None,
            local_endpoint: None,
        };
        let body = serde_json::to_vec(&doc).unwrap();
        let bootstrap = store.join("owner.json");
        fs::write(&bootstrap, &body).unwrap();
        fs::set_permissions(&bootstrap, fs::Permissions::from_mode(0o600)).unwrap();
        let args = OwnerLaunchArguments {
            store: ServiceConfig {
                store_root: store.clone(),
                project_id: project,
                session_id: session,
            },
            store_identity: identity(&store),
            bootstrap_name: "owner.json".into(),
            bootstrap_file: ExpectedBootstrapFile {
                identity: identity(&bootstrap),
                sha256: hash(&body),
            },
            confirmed_source_path: source,
            confirmed_source_identity: doc.source_identity,
            confirmed_tier: doc.tier,
            confirmed_policy_revision: doc.policy_revision,
        };
        let helper_path = base.join("helper");
        let helper_body = b"synthetic helper never executed";
        fs::write(&helper_path, helper_body).unwrap();
        fs::set_permissions(&helper_path, fs::Permissions::from_mode(0o700)).unwrap();
        let resources = TrustedFactoryResources {
            always_on: polaris_core::prompt::assemble_always_on("", "", &[]),
            audit: Arc::new(tokio::sync::Mutex::new(
                polaris_core::audit::AuditLog::open(&base.join("audit")).unwrap(),
            )),
            skills: vec![],
            agent_types: vec![],
            max_turns: 2,
            spawn_concurrency: 1,
            spawn_write_concurrency: 1,
            tool_memory: None,
            history: None,
        };
        let directory = File::open(&recovery).unwrap();
        directory.sync_all().unwrap();
        File::open(&base).unwrap().sync_all().unwrap();
        Self {
            _temp: temp,
            args,
            root,
            writer,
            helper: PackagedExecutionHelper {
                helper_path,
                sha256: hash(helper_body),
            },
            resources,
            recovery: PinnedRecoveryBase {
                directory,
                identity: identity(&recovery),
                provenance: recovery,
            },
            tokens: Arc::new(Tokens(AtomicUsize::new(0))),
        }
    }
    fn compose(self) -> Result<(DesktopService, tempfile::TempDir), OwnerLaunchError> {
        compose_existing(
            self.args,
            self.root,
            self.writer,
            self.helper,
            TrustedBackend::Codex {
                tokens: self.tokens,
            },
            self.resources,
            self.recovery,
            OwnerLaunchLimits {
                workspace: limits(),
                runtime_roots: vec![],
                toolchains: None,
                copy_mutations: true,
                source: limits(),
                clock: Arc::new(Clock),
                approval_ttl_ms: 1000,
            },
        )
        .map(|service| (service, self._temp))
    }
}

#[test]
fn owner_launch_composes_existing_without_token_request() {
    let f = Fixture::new(true);
    let tokens = f.tokens.clone();
    let (service, _temp) = f.compose().unwrap();
    assert!(service.source_apply_available());
    assert_eq!(tokens.0.load(Ordering::SeqCst), 0);
    drop(service);
}
#[test]
fn owner_launch_refuses_unconfigured_and_mismatched_authority() {
    for case in 0..6 {
        let mut f = Fixture::new(case != 0);
        match case {
            1 => f.args.store.session_id = SessionId::new("wrong").unwrap(),
            2 => f.args.confirmed_policy_revision = DecimalU64::new(9),
            3 => f.args.confirmed_tier = BootstrapTier::ReadOnly,
            4 => f.args.confirmed_source_identity.inode = DecimalU64::new(0),
            5 => f.helper.sha256 = [0; 32],
            _ => (),
        }
        let tokens = f.tokens.clone();
        assert!(f.compose().is_err(), "case {case}");
        assert_eq!(tokens.0.load(Ordering::SeqCst), 0);
    }
}
#[test]
fn owner_launch_refuses_same_coordinates_writer_from_other_store() {
    let mut f = Fixture::new(true);
    let mut other = Fixture::new(true);
    std::mem::swap(&mut f.writer, &mut other.writer);
    assert!(f.compose().is_err());
}
