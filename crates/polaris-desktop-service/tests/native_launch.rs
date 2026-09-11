//! Real binary boundary: inherited control fd must never receive diagnostics.
#![cfg(unix)]
use polaris_desktop_protocol::{
    codec,
    ids::*,
    request::{Request, RequestBody},
    response::{Response, SuccessResult},
};
use std::{
    os::{
        fd::OwnedFd,
        unix::{fs::PermissionsExt, net::UnixStream},
    },
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    time::timeout,
};

#[tokio::test]
async fn native_legacy_recovery_flag_reaches_storage_hello_and_clean_eof() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().canonicalize().unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (control, inherited) = UnixStream::pair().unwrap();
    control.set_nonblocking(true).unwrap();
    let mut control = tokio::net::UnixStream::from_std(control).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_polaris-desktop-service"))
        .env_clear()
        .args([
            "--store-root",
            path.to_str().unwrap(),
            "--project-id",
            "project",
            "--session-id",
            "session",
            "--source-recovery-fd2",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(OwnedFd::from(inherited)))
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = child.stdout.take().unwrap();
    let request = Request {
        protocol_version: Default::default(),
        client_id: ClientId::new("client").unwrap(),
        request_id: RequestId::new("hello").unwrap(),
        body: RequestBody::Hello,
    };
    input
        .write_all(&codec::encode(&request).unwrap())
        .await
        .unwrap();
    let response = timeout(Duration::from_secs(5), async {
        let n = output.read_u32().await.unwrap();
        assert!(n as usize <= codec::MAX_FRAME_BYTES);
        let mut body = vec![0; n as usize];
        output.read_exact(&mut body).await.unwrap();
        codec::from_json::<Response>(&body).unwrap()
    })
    .await
    .unwrap();
    assert!(matches!(response.outcome.unwrap(), SuccessResult::Hello(_)));
    drop(input);
    assert!(
        timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    let mut bytes = Vec::new();
    timeout(Duration::from_secs(5), control.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert!(
        bytes.is_empty(),
        "control channel received diagnostic bytes"
    );
}

#[tokio::test]
async fn invalid_owner_arguments_never_write_diagnostics_into_control_channel() {
    let (control, inherited) = UnixStream::pair().unwrap();
    control.set_nonblocking(true).unwrap();
    let mut control = tokio::net::UnixStream::from_std(control).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_polaris-desktop-service"))
        .env_clear()
        .args(["--owner-launch-version", "bad", "--source-recovery-fd2"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(OwnedFd::from(inherited)))
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    assert!(
        !timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    let mut bytes = Vec::new();
    timeout(Duration::from_secs(5), control.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert!(
        bytes.is_empty(),
        "control channel received diagnostic bytes"
    );
}
