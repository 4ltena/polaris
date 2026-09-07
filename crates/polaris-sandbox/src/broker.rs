//! Client for the outer native-sandbox broker.
//!
//! The broker lives outside the model process so an already-confined Polaris
//! process never tries to apply a second native sandbox.  This module only
//! transports a fully normalized request; the broker remains responsible for
//! enforcing the command allowlist and applying a fresh native sandbox.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::SandboxError;
use crate::confine::Outcome;
use crate::policy::{SandboxMode, SandboxPolicy};

const SOCKET_ENV: &str = "POLARIS_SANDBOX_BROKER";
const TOKEN_ENV: &str = "POLARIS_SANDBOX_BROKER_TOKEN";
const REQUEST_LIMIT: usize = 1024 * 1024;
const RESPONSE_LIMIT: usize = 3 * 1024 * 1024;

#[cfg(unix)]
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

#[derive(Debug, Clone)]
pub(crate) struct BrokerConfig {
    socket_path: PathBuf,
    token: String,
}

/// Loads the opt-in configuration.  Absence leaves the historical execution
/// path intact; any partial or malformed opt-in is an enforcement failure.
pub(crate) fn from_environment() -> Result<Option<BrokerConfig>, SandboxError> {
    from_values(
        std::env::var_os(SOCKET_ENV),
        std::env::var_os(TOKEN_ENV),
        || std::env::var_os("HOME"),
    )
}

fn from_values(
    socket: Option<OsString>,
    token: Option<OsString>,
    home: impl FnOnce() -> Option<OsString>,
) -> Result<Option<BrokerConfig>, SandboxError> {
    match (socket, token) {
        (None, None) => Ok(None),
        (Some(socket), Some(token)) => {
            // HOME is launcher-owned configuration, like the socket and token.
            // The outer sandbox and broker still enforce the actual endpoint.
            let home =
                home().ok_or_else(|| protocol_error("sandbox broker configuration is invalid"))?;
            parse_config(socket, token, Path::new(&home)).map(Some)
        }
        _ => Err(protocol_error("sandbox broker configuration is invalid")),
    }
}

fn parse_config(
    socket: OsString,
    token: OsString,
    home: &Path,
) -> Result<BrokerConfig, SandboxError> {
    let socket = socket
        .into_string()
        .map_err(|_| protocol_error("sandbox broker configuration is invalid"))?;
    let token = token
        .into_string()
        .map_err(|_| protocol_error("sandbox broker configuration is invalid"))?;
    let socket_path = PathBuf::from(socket);

    if !valid_socket_path(&socket_path, home) || !valid_token(&token) {
        return Err(protocol_error("sandbox broker configuration is invalid"));
    }

    // The socket must name its actual endpoint. A symlinked run directory or
    // socket would otherwise let an opt-in configuration reach a broker
    // outside the approved benchmark run.
    if socket_path
        .canonicalize()
        .map(|canonical| canonical != socket_path)
        .unwrap_or(true)
    {
        return Err(protocol_error("sandbox broker configuration is invalid"));
    }

    Ok(BrokerConfig { socket_path, token })
}

fn valid_socket_path(path: &Path, home: &Path) -> bool {
    use std::path::Component;
    if !home.is_absolute()
        || !home.components().any(|c| matches!(c, Component::Normal(_)))
        || !home
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
    {
        return false;
    }
    let Some(run_id) = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|s| s.to_str())
    else {
        return false;
    };
    valid_lower_hex(run_id, 32)
        && path.file_name().is_some_and(|name| name == "broker.sock")
        && path
            .parent()
            .is_some_and(|parent| parent == home.join(".local/share/codex-benchmarks").join(run_id))
}

fn valid_token(token: &str) -> bool {
    valid_lower_hex(token, 64)
}

fn valid_lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn protocol_error(message: &'static str) -> SandboxError {
    SandboxError::NotEnforced(message.to_string())
}

#[derive(Serialize)]
struct Request<'a> {
    version: u8,
    token: &'a str,
    request_id: String,
    cwd: String,
    mode: &'static str,
    writable_roots: Vec<String>,
    program: String,
    args: &'a [String],
    stdin: Option<&'a str>,
}

#[derive(Deserialize)]
struct Response {
    version: u8,
    request_id: String,
    error: Option<String>,
    outcome: Option<ResponseOutcome>,
}

#[derive(Deserialize)]
struct ResponseOutcome {
    status: i32,
    stdout: String,
    stderr: String,
}

pub(crate) fn run(
    config: &BrokerConfig,
    policy: &SandboxPolicy,
    program: &Path,
    args: &[String],
    stdin: Option<&str>,
) -> Result<Outcome, SandboxError> {
    if policy.mode() == SandboxMode::FullAccess {
        return Err(protocol_error("sandbox broker refuses full-access"));
    }

    let cwd = std::env::current_dir()
        .and_then(|path| path.canonicalize())
        .map_err(|_| protocol_error("sandbox broker request is invalid"))?;
    let program = program
        .canonicalize()
        .map_err(|_| protocol_error("sandbox broker request is invalid"))?;
    let request_id = next_request_id();
    let request = Request {
        version: 1,
        token: &config.token,
        request_id: request_id.clone(),
        cwd: cwd.display().to_string(),
        mode: policy.mode().as_str(),
        writable_roots: policy
            .writable_roots()
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
        program: program.display().to_string(),
        args,
        stdin,
    };
    let request = serde_json::to_vec(&request)
        .map_err(|_| protocol_error("sandbox broker request is invalid"))?;
    if request.len() > REQUEST_LIMIT {
        return Err(protocol_error("sandbox broker request is too large"));
    }

    run_request(config, &request, &request_id)
}

fn next_request_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(unix)]
fn run_request(
    config: &BrokerConfig,
    request: &[u8],
    request_id: &str,
) -> Result<Outcome, SandboxError> {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::unix::net::UnixStream;

    let mut stream = UnixStream::connect(&config.socket_path)
        .map_err(|_| protocol_error("sandbox broker connection failed"))?;
    stream
        .set_read_timeout(Some(TIMEOUT))
        .and_then(|()| stream.set_write_timeout(Some(TIMEOUT)))
        .map_err(|_| protocol_error("sandbox broker connection failed"))?;
    stream
        .write_all(request)
        .and_then(|()| stream.write_all(b"\n"))
        .and_then(|()| stream.flush())
        .map_err(|_| protocol_error("sandbox broker protocol failed"))?;

    let mut response = Vec::new();
    let mut reader = BufReader::new(stream);
    reader
        .by_ref()
        .take((RESPONSE_LIMIT + 1) as u64)
        .read_until(b'\n', &mut response)
        .map_err(|_| protocol_error("sandbox broker protocol failed"))?;
    if response.is_empty() || response.len() > RESPONSE_LIMIT || response.last() != Some(&b'\n') {
        return Err(protocol_error("sandbox broker response is invalid"));
    }
    response.pop();
    let response: Response = serde_json::from_slice(&response)
        .map_err(|_| protocol_error("sandbox broker response is invalid"))?;
    if response.version != 1 || response.request_id != request_id || response.error.is_some() {
        return Err(protocol_error("sandbox broker response is invalid"));
    }
    let outcome = response
        .outcome
        .ok_or_else(|| protocol_error("sandbox broker response is invalid"))?;
    Ok(Outcome {
        status: outcome.status,
        stdout: outcome.stdout,
        stderr: outcome.stderr,
    })
}

#[cfg(not(unix))]
fn run_request(
    _config: &BrokerConfig,
    _request: &[u8],
    _request_id: &str,
) -> Result<Outcome, SandboxError> {
    Err(protocol_error(
        "sandbox broker is unsupported on this platform",
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;

    fn config(dir: &tempfile::TempDir) -> BrokerConfig {
        // Tests use an explicit configuration and replace its endpoint below.
        BrokerConfig {
            socket_path: dir.path().join("broker.sock"),
            token: "b".repeat(64),
        }
    }

    fn policy() -> SandboxPolicy {
        SandboxPolicy::new(SandboxMode::ReadOnly, &[]).expect("policy")
    }

    #[test]
    fn serializes_a_correlated_request_and_returns_the_outcome() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config = config(&dir);
        let listener = UnixListener::bind(&config.socket_path).expect("listener");
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut line = String::new();
            reader.read_line(&mut line).expect("read");
            let request: serde_json::Value = serde_json::from_str(&line).expect("json");
            assert_eq!(request["version"], 1);
            assert_eq!(request["mode"], "read-only");
            assert_eq!(request["program"], "/bin/sh");
            assert_eq!(request["args"], serde_json::json!(["-c", "echo brokered"]));
            let request_id = request["request_id"].as_str().expect("request id");
            let reply = serde_json::json!({
                "version": 1,
                "request_id": request_id,
                "error": null,
                "outcome": {"status": 7, "stdout": "out", "stderr": "err"}
            });
            let mut writer = stream;
            writeln!(writer, "{reply}").expect("write");
        });

        let outcome = run(
            &config,
            &policy(),
            Path::new("/bin/sh"),
            &["-c".into(), "echo brokered".into()],
            None,
        )
        .expect("broker result");
        server.join().expect("server");
        assert_eq!(outcome.status, 7);
        assert_eq!(outcome.stdout, "out");
        assert_eq!(outcome.stderr, "err");
    }

    #[test]
    fn mismatched_response_id_is_not_accepted() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config = config(&dir);
        let listener = UnixListener::bind(&config.socket_path).expect("listener");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut line = String::new();
            BufReader::new(stream.try_clone().expect("clone"))
                .read_line(&mut line)
                .expect("read");
            writeln!(stream, "{{\"version\":1,\"request_id\":\"wrong\",\"error\":null,\"outcome\":{{\"status\":0,\"stdout\":\"\",\"stderr\":\"\"}}}}").expect("write");
        });
        let error = run(&config, &policy(), Path::new("/bin/sh"), &[], None)
            .expect_err("mismatch accepted");
        server.join().expect("server");
        assert!(matches!(error, SandboxError::NotEnforced(_)));
    }

    #[test]
    fn full_access_is_refused_before_connecting() {
        let dir = tempfile::tempdir().expect("temp dir");
        let error = run(
            &config(&dir),
            &SandboxPolicy::new(SandboxMode::FullAccess, &[]).expect("policy"),
            Path::new("/bin/sh"),
            &[],
            None,
        )
        .expect_err("full access reached broker");
        assert!(matches!(error, SandboxError::NotEnforced(_)));
    }

    #[test]
    fn malformed_opt_in_configuration_is_rejected() {
        let error = parse_config(
            OsString::from("relative/broker.sock"),
            OsString::from("b".repeat(64)),
            Path::new("/Users/example"),
        )
        .expect_err("relative socket accepted");
        assert!(matches!(error, SandboxError::NotEnforced(_)));
        let error = parse_config(
            OsString::from(
                "/Users/example/.local/share/codex-benchmarks/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/broker.sock",
            ),
            OsString::from("not-a-token"),
            Path::new("/Users/example"),
        )
        .expect_err("malformed token accepted");
        assert!(matches!(error, SandboxError::NotEnforced(_)));
    }

    #[test]
    fn broker_paths_follow_only_the_configured_home_and_exact_run_shape() {
        let tail = ".local/share/codex-benchmarks/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/broker.sock";
        for home in [Path::new("/Users/alice"), Path::new("/home/bob")] {
            assert!(valid_socket_path(&home.join(tail), home));
            assert!(!valid_socket_path(&Path::new("/other").join(tail), home));
            assert!(!valid_socket_path(
                &home.join(tail).with_file_name("other.sock"),
                home
            ));
            assert!(!valid_socket_path(
                &home.join(tail.replace("aaaaaaaa", "AAAAAAAA")),
                home
            ));
        }
        for home in ["relative", "/Users/../alice", ".", "/"] {
            assert!(!valid_socket_path(
                &Path::new(home).join(tail),
                Path::new(home)
            ));
        }
    }

    #[test]
    fn absent_or_partial_broker_configuration_does_not_read_home() {
        assert!(
            from_values(None, None, || panic!("HOME read without opt-in"))
                .unwrap()
                .is_none()
        );
        assert!(
            from_values(Some("socket".into()), None, || panic!(
                "HOME read with partial opt-in"
            ))
            .is_err()
        );
        assert!(
            from_values(None, Some("token".into()), || panic!(
                "HOME read with partial opt-in"
            ))
            .is_err()
        );
        assert!(from_values(Some("socket".into()), Some("token".into()), || None).is_err());
    }

    #[test]
    fn configured_endpoint_must_be_canonical_and_cannot_follow_links() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let run = home.join(".local/share/codex-benchmarks/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        std::fs::create_dir_all(&run).unwrap();
        let socket = run.join("broker.sock");
        // Path validation precedes the Unix connect, so a plain file suffices here.
        std::fs::write(&socket, b"").unwrap();
        assert!(
            parse_config(
                socket.clone().into_os_string(),
                "b".repeat(64).into(),
                &home
            )
            .is_ok()
        );
        std::fs::remove_file(&socket).unwrap();
        let target = home.join("other-endpoint");
        std::fs::write(&target, b"").unwrap();
        std::os::unix::fs::symlink(&target, &socket).unwrap();
        assert!(parse_config(socket.into_os_string(), "b".repeat(64).into(), &home).is_err());
    }

    #[test]
    fn broker_refusal_never_falls_back_to_direct_execution() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config = config(&dir);
        let listener = UnixListener::bind(&config.socket_path).expect("listener");
        let marker = dir.path().join("direct-fallback-marker");
        let script = dir.path().join("would-run-if-fallback.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\necho ran > {}\n", marker.display()),
        )
        .expect("script");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut line = String::new();
            BufReader::new(stream.try_clone().expect("clone"))
                .read_line(&mut line)
                .expect("read");
            let request: serde_json::Value = serde_json::from_str(&line).expect("json");
            let request_id = request["request_id"].as_str().expect("request id");
            writeln!(stream, "{{\"version\":1,\"request_id\":\"{request_id}\",\"error\":\"refused\",\"outcome\":null}}")
                .expect("write");
        });
        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[dir.path().to_path_buf()])
            .expect("policy");
        let error =
            crate::confine::run_confined_with_broker(&policy, &script, &[], None, Some(&config))
                .expect_err("broker refusal fell back to direct execution");
        server.join().expect("server");
        assert!(matches!(error, SandboxError::NotEnforced(_)));
        assert!(
            !marker.exists(),
            "the direct program ran after broker refusal"
        );
        // This fake server verifies client routing only. It does not verify
        // the broker's native confinement, which needs host-side tests.
    }

    #[test]
    fn request_and_response_limits_are_enforced() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config = config(&dir);
        let error = run(
            &config,
            &policy(),
            Path::new("/bin/sh"),
            &[],
            Some(&"x".repeat(REQUEST_LIMIT)),
        )
        .expect_err("oversized request accepted");
        assert!(matches!(error, SandboxError::NotEnforced(_)));

        let listener = UnixListener::bind(&config.socket_path).expect("listener");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut line = String::new();
            BufReader::new(stream.try_clone().expect("clone"))
                .read_line(&mut line)
                .expect("read");
            stream
                .write_all(&vec![b'x'; RESPONSE_LIMIT + 1])
                .expect("write");
        });
        let error = run(&config, &policy(), Path::new("/bin/sh"), &[], None)
            .expect_err("oversized response accepted");
        server.join().expect("server");
        assert!(matches!(error, SandboxError::NotEnforced(_)));
    }
}
