//! helper自身の新規TempDirで実OS子プロセスのstdin/stdoutと終了を検証する。
#![cfg(unix)]

use polaris_desktop_protocol::{
    ProtocolVersion, codec,
    event::{Event, EventBody},
    ids::*,
    request::*,
    response::*,
    run_state::RunState,
};
use polaris_desktop_service::session_id;
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdin, ChildStdout, Command},
    time::{sleep, timeout},
};

struct PipeClient {
    child: Child,
    input: ChildStdin,
    output: ChildStdout,
}
impl PipeClient {
    fn spawn() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_polaris-fake-service"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        Self {
            child,
            input,
            output,
        }
    }
    async fn send(&mut self, id: &str, body: RequestBody) {
        let request = Request {
            protocol_version: ProtocolVersion,
            client_id: ClientId::new("subprocess-client").unwrap(),
            request_id: RequestId::new(id).unwrap(),
            body,
        };
        self.input
            .write_all(&codec::encode(&request).unwrap())
            .await
            .unwrap();
    }
    async fn frame(&mut self) -> serde_json::Value {
        timeout(Duration::from_secs(4), async {
            let size = self.output.read_u32().await.unwrap() as usize;
            assert!(size <= codec::MAX_FRAME_BYTES);
            let mut body = vec![0; size];
            self.output.read_exact(&mut body).await.unwrap();
            codec::from_json(&body).unwrap()
        })
        .await
        .unwrap()
    }
    async fn response(&mut self) -> SuccessResult {
        loop {
            let frame = self.frame().await;
            if frame["kind"] == "response" {
                return serde_json::from_value::<Response>(frame)
                    .unwrap()
                    .outcome
                    .unwrap();
            }
        }
    }
}

#[tokio::test]
async fn actual_child_pipe_stream_history_and_ready_exit_without_stdin_eof() {
    let mut client = PipeClient::spawn();
    client.send("hello", RequestBody::Hello).await;
    let SuccessResult::Hello(hello) = client.response().await else {
        panic!()
    };
    client
        .send(
            "subscribe",
            RequestBody::SessionSubscribe(session_id(), Subscribe::default()),
        )
        .await;
    assert!(matches!(
        client.response().await,
        SuccessResult::SessionSubscribe(_)
    ));
    client
        .send(
            "draft",
            RequestBody::DraftUpdate(
                session_id(),
                DraftUpdate {
                    expected_draft_revision: DecimalU64::new(0),
                    text: "子プロセスから送信".into(),
                    attachment_ids: vec![],
                },
            ),
        )
        .await;
    client.response().await;
    client
        .send(
            "start",
            RequestBody::RunStart(
                session_id(),
                RunStart {
                    expected_draft_revision: DecimalU64::new(1),
                    expected_configuration_revision: DecimalU64::new(0),
                    expected_policy_revision: DecimalU64::new(0),
                },
            ),
        )
        .await;
    assert!(matches!(
        client.response().await,
        SuccessResult::RunStart(_)
    ));
    let mut deltas = 0;
    loop {
        let event: Event = serde_json::from_value(client.frame().await).unwrap();
        match event.body {
            EventBody::MessageDelta(_) => deltas += 1,
            EventBody::RunState(r) if r.state == RunState::Succeeded => break,
            _ => {}
        }
    }
    assert!(deltas >= 2);
    client
        .send("snapshot", RequestBody::SessionSnapshot(session_id()))
        .await;
    let SuccessResult::SessionSnapshot(snapshot) = client.response().await else {
        panic!()
    };
    client
        .send(
            "history",
            RequestBody::HistoryPage(
                session_id(),
                HistoryPage {
                    snapshot_id: snapshot.snapshot_id,
                    cursor: snapshot.history_start_cursor,
                    limit: PageLimit::new(10).unwrap(),
                },
            ),
        )
        .await;
    let SuccessResult::HistoryPage(history) = client.response().await else {
        panic!()
    };
    assert_eq!(history.messages.len(), 2);
    assert_eq!(
        history.messages[1].text,
        "こんにちは。これはfakeの応答です。"
    );
    loop {
        client
            .send(
                "shutdown",
                RequestBody::ShutdownRequest(ShutdownRequest {
                    engine_epoch: hello.engine_epoch.clone(),
                }),
            )
            .await;
        let SuccessResult::ShutdownRequest(result) = client.response().await else {
            panic!()
        };
        if result.state == ShutdownState::Ready {
            break;
        }
        sleep(Duration::from_millis(60)).await;
    }
    assert!(
        timeout(Duration::from_secs(3), client.child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    assert_eq!(client.output.read(&mut [0; 1]).await.unwrap(), 0);
}

#[tokio::test]
async fn actual_child_exits_after_input_eof() {
    let mut client = PipeClient::spawn();
    client.send("hello", RequestBody::Hello).await;
    assert!(matches!(client.response().await, SuccessResult::Hello(_)));
    drop(client.input);
    assert!(
        timeout(Duration::from_secs(3), client.child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}
