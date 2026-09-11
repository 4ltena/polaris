//! Production storage IPC across OS children; all roots/content are disposable dummy fixtures.
#![cfg(unix)]
use polaris_core::desktop_store::{DesktopRoot, InitialState, StoreError};
use polaris_desktop_protocol::{
    codec,
    ids::*,
    request::*,
    response::*,
    snapshot::{Configuration, Draft, Snapshot},
};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdin, ChildStdout, Command},
    time::timeout,
};

fn project() -> ProjectId {
    ProjectId::new("persistence-project").unwrap()
}
fn session() -> SessionId {
    SessionId::new("persistence-session").unwrap()
}
fn root() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap()
}
fn command(path: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_polaris-desktop-service"));
    command
        .args(["--store-root"])
        .arg(path)
        .args([
            "--project-id",
            project().as_str(),
            "--session-id",
            session().as_str(),
        ])
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}
struct Client {
    child: Child,
    input: ChildStdin,
    output: ChildStdout,
}
impl Client {
    fn spawn(path: &Path) -> Self {
        let mut child = command(path).spawn().unwrap();
        Self {
            input: child.stdin.take().unwrap(),
            output: child.stdout.take().unwrap(),
            child,
        }
    }
    async fn request(
        &mut self,
        id: &str,
        body: RequestBody,
    ) -> Result<SuccessResult, ProtocolError> {
        timeout(Duration::from_secs(5), async {
            let request = Request {
                protocol_version: Default::default(),
                client_id: ClientId::new("persistent-client").unwrap(),
                request_id: RequestId::new(id).unwrap(),
                body,
            };
            self.input
                .write_all(&codec::encode(&request).unwrap())
                .await
                .unwrap();
            loop {
                let size = self.output.read_u32().await.unwrap() as usize;
                assert!(size <= codec::MAX_FRAME_BYTES);
                let mut bytes = vec![0; size];
                self.output.read_exact(&mut bytes).await.unwrap();
                let frame: serde_json::Value = codec::from_json(&bytes).unwrap();
                if frame["kind"] == "response" {
                    let reply: Response = serde_json::from_value(frame).unwrap();
                    assert_eq!(reply.request_id, request.request_id);
                    return reply.outcome;
                }
            }
        })
        .await
        .unwrap()
    }
    async fn hello(&mut self) -> Hello {
        let SuccessResult::Hello(hello) = self.request("hello", RequestBody::Hello).await.unwrap()
        else {
            panic!("Hello")
        };
        assert!(!hello.capabilities.contains(&Capability::RunStart));
        assert!(!hello.capabilities.contains(&Capability::RunCancel));
        assert!(!hello.capabilities.contains(&Capability::ApprovalResolve));
        assert!(hello.engine_epoch.as_str().starts_with("desktop-"));
        hello
    }
    async fn snapshot(&mut self, id: &str) -> Snapshot {
        let SuccessResult::SessionSnapshot(snapshot) = self
            .request(id, RequestBody::SessionSnapshot(session()))
            .await
            .unwrap()
        else {
            panic!("snapshot")
        };
        *snapshot
    }
    async fn close(mut self, epoch: EngineEpoch) {
        timeout(Duration::from_secs(5), async {
            loop {
                let result = self
                    .request(
                        "close",
                        RequestBody::ShutdownRequest(ShutdownRequest {
                            engine_epoch: epoch.clone(),
                        }),
                    )
                    .await
                    .unwrap();
                match result {
                    SuccessResult::ShutdownRequest(result)
                        if result.state == ShutdownState::Ready =>
                    {
                        break;
                    }
                    SuccessResult::ShutdownRequest(_) => {
                        tokio::time::sleep(Duration::from_millis(10)).await
                    }
                    _ => panic!("shutdown"),
                }
            }
        })
        .await
        .unwrap();
        let exit = timeout(Duration::from_secs(5), self.child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(exit.success());
    }
}
#[tokio::test]
async fn same_root_across_os_children_preserves_draft_ledger_and_rejects_execution() {
    let temporary = root();
    let path = std::fs::canonicalize(temporary.path()).unwrap();
    let mut first = Client::spawn(&path);
    let hello = first.hello().await;
    let wrong = first
        .request(
            "wrong-session",
            RequestBody::SessionSnapshot(SessionId::new("other").unwrap()),
        )
        .await
        .unwrap_err();
    assert_eq!(wrong.code, ErrorCode::PermissionDenied);
    let before = first.snapshot("before").await;
    assert!(before.runs.is_empty());
    assert_eq!(before.configuration.provider, "unconfigured");
    let update = RequestBody::DraftUpdate(
        session(),
        DraftUpdate {
            expected_draft_revision: before.draft.draft_revision,
            text: "永続dummy入力".into(),
            attachment_ids: vec![],
        },
    );
    let receipt = first.request("draft", update.clone()).await.unwrap();
    let saved = first.snapshot("saved").await;
    let denied = first
        .request(
            "run",
            RequestBody::RunStart(
                session(),
                RunStart {
                    expected_draft_revision: saved.draft.draft_revision,
                    expected_configuration_revision: saved.configuration.configuration_revision,
                    expected_policy_revision: saved.policy_revision,
                },
            ),
        )
        .await
        .unwrap_err();
    assert_eq!(denied.code, ErrorCode::CapabilityUnavailable);
    assert_eq!(
        first.snapshot("after-denial").await.content_revision,
        saved.content_revision
    );
    // Separate OS child cannot acquire the live session's writer lock.
    let busy = timeout(Duration::from_secs(5), command(&path).output())
        .await
        .unwrap()
        .unwrap();
    assert!(!busy.status.success());
    assert!(busy.stdout.is_empty());
    first.close(hello.engine_epoch.clone()).await;
    let mut second = Client::spawn(&path);
    let reopened_hello = second.hello().await;
    assert_ne!(reopened_hello.engine_epoch, hello.engine_epoch);
    let reopened = second.snapshot("reopened").await;
    assert_eq!(reopened.draft, saved.draft);
    assert_eq!(reopened.session_revision, saved.session_revision);
    assert_eq!(reopened.content_revision, saved.content_revision);
    assert!(reopened.runs.is_empty());
    assert_eq!(second.request("draft", update).await.unwrap(), receipt);
    assert_eq!(
        second.snapshot("idempotent").await.session_revision,
        saved.session_revision
    );
    second.close(reopened_hello.engine_epoch).await;
    assert!(path.exists());
}

#[tokio::test]
async fn seeded_dummy_raw_survives_recovery_without_inference_or_reexecution() {
    let temporary = root();
    let path = std::fs::canonicalize(temporary.path()).unwrap();
    let owner = DesktopRoot::open_owned(&path).unwrap();
    let mut writer = owner
        .create(
            project(),
            session(),
            InitialState {
                draft: Draft {
                    draft_revision: DecimalU64::new(0),
                    text: "dummy原文".into(),
                    attachment_ids: vec![],
                },
                configuration: Configuration {
                    history_mode: Default::default(),
                    configuration_revision: DecimalU64::new(0),
                    provider: "unconfigured".into(),
                    model: String::new(),
                    effort: "none".into(),
                },
                policy_revision: DecimalU64::new(0),
            },
        )
        .unwrap();
    writer
        .apply(
            &Request {
                protocol_version: Default::default(),
                client_id: ClientId::new("seed").unwrap(),
                request_id: RequestId::new("seed").unwrap(),
                body: RequestBody::RunStart(
                    session(),
                    RunStart {
                        expected_draft_revision: DecimalU64::new(0),
                        expected_configuration_revision: DecimalU64::new(0),
                        expected_policy_revision: DecimalU64::new(0),
                    },
                ),
            },
            Some(RunTarget {
                run_id: RunId::new("dummy-run").unwrap(),
                attempt_id: AttemptId::new("dummy-attempt").unwrap(),
            }),
        )
        .unwrap();
    let published = writer.snapshot().unwrap();
    assert!(matches!(
        owner.open(&project(), &session()),
        Err(StoreError::Busy)
    ));
    drop(writer);
    drop(owner);
    for _ in 0..2 {
        let mut client = Client::spawn(&path);
        let hello = client.hello().await;
        let snapshot = client.snapshot("snapshot").await;
        assert_eq!(snapshot.runs.len(), 1);
        assert_eq!(
            snapshot.runs[0].state,
            polaris_desktop_protocol::run_state::RunState::Interrupted
        );
        assert_eq!(snapshot.content_revision, published.marker.content_revision);
        let SuccessResult::HistoryPage(history) = client
            .request(
                "history",
                RequestBody::HistoryPage(
                    session(),
                    HistoryPage {
                        snapshot_id: snapshot.snapshot_id,
                        cursor: snapshot.history_start_cursor,
                        limit: PageLimit::new(4).unwrap(),
                    },
                ),
            )
            .await
            .unwrap()
        else {
            panic!("history")
        };
        assert_eq!(history.messages.len(), 1);
        assert_eq!(history.messages[0].text, "dummy原文");
        client.close(hello.engine_epoch).await;
    }
    let reopened = DesktopRoot::open_owned(&path)
        .unwrap()
        .open(&project(), &session())
        .unwrap()
        .snapshot()
        .unwrap();
    assert_eq!(
        serde_json::to_value(reopened.raw).unwrap(),
        serde_json::to_value(published.raw).unwrap()
    );
}

#[tokio::test]
async fn production_cli_rejects_missing_flags_and_noncanonical_root_without_creation() {
    let temporary = root();
    let path = std::fs::canonicalize(temporary.path()).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_polaris-desktop-service"));
    let output = timeout(Duration::from_secs(5), command.env_clear().output())
        .await
        .unwrap()
        .unwrap();
    assert!(!output.status.success());
    let output = timeout(
        Duration::from_secs(5),
        self::command(&path.join("..")).output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!output.status.success());
    assert_eq!(std::fs::read_dir(&path).unwrap().count(), 0);
}
