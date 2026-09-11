//! Production startup resources, package aliases and real capability composition tests.
use super::*;
use polaris_core::desktop_store::{
    BootstrapIdentity, ExpectedBootstrapFile, InitialState, OwnerBootstrapDocument,
};
use polaris_desktop_protocol::{
    codec,
    ids::*,
    request::{Request, RequestBody},
    response::{Response, SuccessResult},
    snapshot::{Configuration, Draft},
};
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[test]
fn launchservices_fixed_alias_resolves_to_verified_package() {
    let base = tempfile::tempdir_in("/private/tmp").unwrap();
    let (_, _, executable) = fixture(base.path());
    let alias = Path::new("/tmp").join(executable.strip_prefix("/private/tmp").unwrap());
    assert!(read_package_manifest(&alias).is_err());
    let physical = physical_executable(&alias);
    assert_eq!(physical, executable);
    assert!(read_package_manifest(&physical).is_ok());
    assert_eq!(
        physical_executable(Path::new("/var/a")),
        PathBuf::from("/private/var/a")
    );
    assert_eq!(
        physical_executable(Path::new("/tmp-other/a")),
        PathBuf::from("/tmp-other/a")
    );
}

fn identity(p: &Path) -> BootstrapIdentity {
    let m = fs::metadata(p).unwrap();
    BootstrapIdentity {
        device: DecimalU64::new(m.dev()),
        inode: DecimalU64::new(m.ino()),
    }
}
fn hash(bytes: &[u8]) -> String {
    polaris_core::conversation_state::content_hash(bytes)
}
fn put(path: &Path, body: &[u8], mode: u32) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}
fn fixture(base: &Path) -> (OwnerLaunchArguments, PathBuf, PathBuf) {
    fixture_with_history(base, Default::default())
}
fn fixture_with_history(
    base: &Path,
    history_mode: polaris_desktop_protocol::snapshot::HistoryMode,
) -> (OwnerLaunchArguments, PathBuf, PathBuf) {
    let source = base.join("source");
    let store = base.join("store");
    let home = base.join("home");
    for p in [&source, &store, &home] {
        fs::create_dir(p).unwrap();
        fs::set_permissions(p, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let project = ProjectId::new("project").unwrap();
    let session = SessionId::new("session").unwrap();
    let root = DesktopRoot::open_owned(&store).unwrap();
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
                    history_mode,
                    configuration_revision: DecimalU64::new(1),
                    provider: "codex".into(),
                    model: "gpt-6-astra".into(),
                    effort: "medium".into(),
                },
                policy_revision: DecimalU64::new(0),
            },
        )
        .unwrap();
    drop(writer);
    let doc = OwnerBootstrapDocument {
        history_mode,
        schema_version: 1,
        project_id: project.clone(),
        session_id: session.clone(),
        store_identity: identity(&store),
        source_path: source.to_str().unwrap().into(),
        source_identity: identity(&source),
        tier: BootstrapTier::ReadOnly,
        policy_revision: DecimalU64::new(0),
        configuration_revision: DecimalU64::new(1),
        provider: BootstrapProvider::Codex,
        model: None,
        effort: None,
        local_endpoint: None,
    };
    let body = serde_json::to_vec(&doc).unwrap();
    let manifest = store.join("owner.json");
    put(&manifest, &body, 0o600);
    polaris_auth::store::save_to(
        &home.join(".polaris/auth.json"),
        &polaris_auth::Credentials {
            access_token: "synthetic-only".into(),
            refresh_token: "synthetic-refresh".into(),
            account_id: "synthetic-account".into(),
            expires_at: Some(u64::MAX),
        },
    )
    .unwrap();
    let exe = base.join("app/Contents/Helpers/polaris-desktop-service");
    let resources = base.join("app/Contents/Resources");
    let helper = b"synthetic executable, never launched by this test";
    put(&exe, b"synthetic service", 0o700);
    put(
        &exe.parent().unwrap().join("polaris-execution-helper"),
        helper,
        0o700,
    );
    put(
        &resources.join("execution-helper.json"),
        serde_json::to_string(&serde_json::json!({"schema_version":1,"sha256":hash(helper)}))
            .unwrap()
            .as_bytes(),
        0o600,
    );
    put(
        &resources.join("skills/verify/SKILL.md"),
        b"---\nname: verify\ndescription: Verify a change.\n---\nInspect the result.\n",
        0o600,
    );
    put(&resources.join("agents/inspector/SKILL.md"), b"---\nname: inspector\ndescription: Inspect one file.\nallowed-tools: read\nmetadata:\n  polaris-access: read\n  polaris-tier: low\n  polaris-wall-seconds: \"60\"\n  polaris-max-turns: \"4\"\n  polaris-continuation: denied\n  polaris-output: result.json\n---\nInspect one file.\n", 0o600);
    put(
        &resources.join("agents/inspector/result.json"),
        b"{\"type\":\"object\"}",
        0o600,
    );
    let digest = hash(&body);
    (
        OwnerLaunchArguments {
            store: ServiceConfig {
                store_root: store,
                project_id: project,
                session_id: session,
            },
            store_identity: doc.store_identity,
            bootstrap_name: "owner.json".into(),
            bootstrap_file: ExpectedBootstrapFile {
                identity: identity(&manifest),
                sha256: std::array::from_fn(|i| {
                    u8::from_str_radix(&digest[i * 2..i * 2 + 2], 16).unwrap()
                }),
            },
            confirmed_source_path: source,
            confirmed_source_identity: doc.source_identity,
            confirmed_tier: doc.tier,
            confirmed_policy_revision: doc.policy_revision,
        },
        home,
        exe,
    )
}

#[tokio::test]
async fn packaged_owner_advertises_real_runs_and_releases_store_after_eof() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let (args, home, exe) = fixture(&base);
    let coordinates = args.store.clone();
    let mut owner = tokio::task::spawn_blocking(move || start_at(args, &home, &exe))
        .await
        .unwrap()
        .unwrap();
    assert!(!owner._catalogs.is_empty());
    let (mut client, server) = tokio::io::duplex(65536);
    let (reader, writer) = tokio::io::split(server);
    let task = tokio::spawn(async move {
        owner.service.serve(reader, writer).await.unwrap();
    });
    let request = Request {
        protocol_version: Default::default(),
        client_id: ClientId::new("client").unwrap(),
        request_id: RequestId::new("hello").unwrap(),
        body: RequestBody::Hello,
    };
    client
        .write_all(&codec::encode(&request).unwrap())
        .await
        .unwrap();
    let response = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let n = client.read_u32().await.unwrap();
        let mut body = vec![0; n as usize];
        client.read_exact(&mut body).await.unwrap();
        codec::from_json::<Response>(&body).unwrap()
    })
    .await
    .unwrap();
    let SuccessResult::Hello(hello) = response.outcome.unwrap() else {
        panic!("hello");
    };
    assert!(
        serde_json::to_value(hello).unwrap()["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "run_start")
    );
    drop(client);
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    let root = DesktopRoot::open_owned(&coordinates.store_root).unwrap();
    assert!(
        root.open(&coordinates.project_id, &coordinates.session_id)
            .is_ok()
    );
}

#[tokio::test]
async fn legacy_owner_ignores_invalid_memory_config_without_creating_memory_resources() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let (args, home, exe) = fixture(&base);
    put(
        &home.join(".polaris/config.toml"),
        b"invalid memory TOML [",
        0o600,
    );
    let store = args.store.store_root.clone();
    let owner = tokio::task::spawn_blocking(move || start_at(args, &home, &exe))
        .await
        .unwrap()
        .unwrap();
    assert!(!store.join("embedding-helper").exists());
    assert!(!store.join("conversation-memory.sqlite3").exists());
    drop(owner);
}

#[tokio::test]
async fn strict_owner_reports_missing_memory_config_before_recovery_creation() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let (args, home, exe) = fixture_with_history(
        &base,
        polaris_desktop_protocol::snapshot::HistoryMode::Strict10,
    );
    let store = args.store.store_root.clone();
    let result = tokio::task::spawn_blocking(move || start_at(args, &home, &exe))
        .await
        .unwrap();
    let Err(StartupError::Memory(reason)) = result else {
        panic!("missing config must return the actionable memory error");
    };
    assert!(matches!(reason, crate::MemoryResourceError::Configuration));
    assert_eq!(reason.exit_code(), 81);
    assert!(!store.join("embedding-helper").exists());
    assert!(!store.join("conversation-memory.sqlite3").exists());
    assert!(fs::read_dir(store).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".polaris-recovery-")
    }));
}

#[test]
fn catalog_aliases_are_rejected_without_copying_their_contents() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let catalog = base.join("catalog");
    fs::create_dir(&catalog).unwrap();
    let outside = base.join("outside");
    put(&outside, b"must not become a skill", 0o600);
    std::os::unix::fs::symlink(&outside, catalog.join("SKILL.md")).unwrap();
    assert!(catalog_copies(&[catalog]).is_err());
}

#[tokio::test]
async fn changed_publication_is_refused_before_recovery_creation() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let (mut args, home, exe) = fixture(&base);
    args.confirmed_policy_revision = DecimalU64::new(99);
    let store = args.store.store_root.clone();
    assert!(
        tokio::task::spawn_blocking(move || start_at(args, &home, &exe))
            .await
            .unwrap()
            .is_err()
    );
    assert!(fs::read_dir(store).unwrap().all(|e| {
        !e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".polaris-recovery-")
    }));
}

#[test]
fn production_approval_clock_uses_unix_epoch() {
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let clock = Clock::new().unwrap();
    let observed = clock.now_ms();
    let after = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    assert!(
        observed >= before && observed <= after,
        "deadline clock must use Unix milliseconds"
    );
    assert!(clock.now_ms() >= observed);
}
