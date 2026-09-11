//! Actual packaged OwnerV1 startup with synthetic local metadata and both IPC channels.
#![cfg(target_os = "macos")]
use polaris_core::desktop_store::*;
use polaris_desktop_protocol::{
    codec,
    ids::*,
    request::{Request, RequestBody},
    response::{Response, SuccessResult},
    snapshot::{Configuration, Draft},
    source_recovery,
};
use std::{
    fs,
    os::{
        fd::OwnedFd,
        unix::{
            fs::{MetadataExt, PermissionsExt},
            net::UnixStream,
        },
    },
    path::Path,
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    time::timeout,
};
fn identity(path: &Path) -> BootstrapIdentity {
    let m = fs::metadata(path).unwrap();
    BootstrapIdentity {
        device: DecimalU64::new(m.dev()),
        inode: DecimalU64::new(m.ino()),
    }
}
fn put(path: &Path, bytes: &[u8], mode: u32) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}
async fn frame(reader: &mut (impl AsyncRead + Unpin)) -> Vec<u8> {
    timeout(Duration::from_secs(30), async {
        let length = reader.read_u32().await.unwrap() as usize;
        assert!(length <= codec::MAX_FRAME_BYTES);
        let mut bytes = vec![0; length];
        reader.read_exact(&mut bytes).await.unwrap();
        bytes
    })
    .await
    .unwrap()
}
#[tokio::test]
async fn native_owner_main_and_recovery_hello_then_eof_release_store() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        for expected in ["GET /api/tags ", "POST /api/show "] {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            loop {
                let byte = socket.read_u8().await.unwrap();
                bytes.push(byte);
                assert!(bytes.len() < 8192);
                if bytes.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            let headers = String::from_utf8(bytes).unwrap();
            assert!(headers.starts_with(expected));
            assert!(!headers.to_ascii_lowercase().contains("authorization:"));
            if let Some(length) = headers.lines().find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().parse::<usize>().unwrap())
            }) {
                let mut body = vec![0; length];
                socket.read_exact(&mut body).await.unwrap();
            }
            let body = if expected.starts_with("GET") {
                r#"{"models":[{"name":"synthetic-local"}]}"#
            } else {
                r#"{"capabilities":["completion","tools"]}"#
            };
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        }
    });
    let temporary = tempfile::tempdir_in("/private/tmp").unwrap();
    let base = temporary.path();
    let store = base.join("store");
    let source = base.join("source");
    for p in [&store, &source] {
        fs::create_dir(p).unwrap();
        fs::set_permissions(p, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let project = ProjectId::new("project").unwrap();
    let session = SessionId::new("session").unwrap();
    let root = DesktopRoot::open_owned(&store).unwrap();
    drop(
        root.create(
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
                    provider: "ollama".into(),
                    model: "synthetic-local".into(),
                    effort: "medium".into(),
                },
                policy_revision: DecimalU64::new(0),
            },
        )
        .unwrap(),
    );
    let document = OwnerBootstrapDocument {
        history_mode: Default::default(),
        schema_version: 1,
        project_id: project.clone(),
        session_id: session.clone(),
        store_identity: identity(&store),
        source_path: source.to_str().unwrap().into(),
        source_identity: identity(&source),
        tier: BootstrapTier::ReadOnly,
        policy_revision: DecimalU64::new(0),
        configuration_revision: DecimalU64::new(1),
        provider: BootstrapProvider::Ollama,
        model: Some("synthetic-local".into()),
        effort: None,
        local_endpoint: Some(endpoint),
    };
    let bootstrap = store.join("owner.json");
    let bytes = serde_json::to_vec(&document).unwrap();
    put(&bootstrap, &bytes, 0o600);
    let proof = identity(&bootstrap);
    let contents = base.join("Owner.app/Contents");
    let executable = contents.join("Helpers/polaris-desktop-service");
    fs::create_dir_all(executable.parent().unwrap()).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_polaris-desktop-service"), &executable).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let hash = polaris_core::conversation_state::content_hash;
    put(
        &contents.join("Helpers/polaris-execution-helper"),
        b"synthetic helper, never executed",
        0o700,
    );
    put(&contents.join("Resources/execution-helper.json"),serde_json::to_string(&serde_json::json!({"schema_version":1,"sha256":hash(b"synthetic helper, never executed")})).unwrap().as_bytes(),0o600);
    put(
        &contents.join("Resources/skills/verify/SKILL.md"),
        b"---\nname: verify\ndescription: Verify a change.\n---\nInspect the result.\n",
        0o600,
    );
    put(&contents.join("Resources/agents/inspector/SKILL.md"),b"---\nname: inspector\ndescription: Inspect one file.\nallowed-tools: read\nmetadata:\n  polaris-access: read\n  polaris-tier: low\n  polaris-wall-seconds: \"60\"\n  polaris-max-turns: \"4\"\n  polaris-continuation: denied\n  polaris-output: result.json\n---\nInspect one file.\n",0o600);
    put(
        &contents.join("Resources/agents/inspector/result.json"),
        b"{\"type\":\"object\"}",
        0o600,
    );
    let fields = [
        ("--owner-launch-version", "1".into()),
        ("--store-root", store.to_str().unwrap().into()),
        ("--project-id", "project".into()),
        ("--session-id", "session".into()),
        (
            "--store-device",
            document.store_identity.device.get().to_string(),
        ),
        (
            "--store-inode",
            document.store_identity.inode.get().to_string(),
        ),
        ("--bootstrap-name", "owner.json".into()),
        ("--bootstrap-device", proof.device.get().to_string()),
        ("--bootstrap-inode", proof.inode.get().to_string()),
        ("--bootstrap-sha256", hash(&bytes)),
        ("--confirmed-source-path", document.source_path.clone()),
        (
            "--confirmed-source-device",
            document.source_identity.device.get().to_string(),
        ),
        (
            "--confirmed-source-inode",
            document.source_identity.inode.get().to_string(),
        ),
        ("--confirmed-tier", "read_only".into()),
        ("--confirmed-policy-revision", "0".into()),
    ];
    let args: Vec<String> = fields
        .into_iter()
        .flat_map(|(k, v)| [k.to_owned(), v])
        .chain(["--source-recovery-fd2".into()])
        .collect();
    let (control, inherited) = UnixStream::pair().unwrap();
    control.set_nonblocking(true).unwrap();
    let mut control = tokio::net::UnixStream::from_std(control).unwrap();
    // Exercise LaunchServices' fixed /tmp alias, not just the physical test path.
    let alias = Path::new("/tmp").join(executable.strip_prefix("/private/tmp").unwrap());
    let mut child = Command::new(alias)
        .env_clear()
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(OwnedFd::from(inherited)))
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = child.stdout.take().unwrap();
    input
        .write_all(
            &codec::encode(&Request {
                protocol_version: Default::default(),
                client_id: ClientId::new("client").unwrap(),
                request_id: RequestId::new("hello").unwrap(),
                body: RequestBody::Hello,
            })
            .unwrap(),
        )
        .await
        .unwrap();
    let response: Response = codec::from_json(&frame(&mut output).await).unwrap();
    let SuccessResult::Hello(hello) = response.outcome.unwrap() else {
        panic!("expected hello")
    };
    assert!(
        serde_json::to_value(&hello).unwrap()["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "run_start")
    );
    control
        .write_all(
            &source_recovery::encode(&source_recovery::Request::Hello {
                version: Default::default(),
                request_id: RequestId::new("recovery").unwrap(),
            })
            .unwrap(),
        )
        .await
        .unwrap();
    let recovery: source_recovery::Response =
        serde_json::from_slice(&frame(&mut control).await).unwrap();
    assert_eq!(recovery.engine_epoch, hello.engine_epoch);
    assert_eq!(recovery.project_id, project);
    assert_eq!(recovery.session_id, session);
    assert!(recovery.error.is_none());
    drop(input);
    assert!(
        timeout(Duration::from_secs(10), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    let mut rest = Vec::new();
    timeout(Duration::from_secs(5), control.read_to_end(&mut rest))
        .await
        .unwrap()
        .unwrap();
    assert!(rest.is_empty());
    timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    assert!(root.open(&project, &session).is_ok());
}
