//! Windows refusal is tested at the real product entry before input or state I/O.
#![cfg(windows)]

use std::fs;
use std::io::Read;
use std::process::{Child, Command, Output, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[path = "support/product_binary.rs"]
mod product_binary;
#[path = "support/windows_fixture.rs"]
mod windows_fixture;

const UNAVAILABLE: &str =
    "Windows lacks the required execution and mutation protections. No changes were made.";

fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    for directory in ["project/.polaris", "home", "data/state", "temporary"] {
        fs::create_dir_all(root.path().join(directory)).unwrap();
    }
    for (path, bytes) in [
        ("project/file.rs", "project before"),
        ("project/.polaris/config.toml", "[malformed config"),
        ("data/state/session.json", "session before"),
        ("data/state/journal.json", "journal before"),
    ] {
        fs::write(root.path().join(path), bytes).unwrap();
    }
    root
}

fn command(root: &std::path::Path, arguments: &[&str]) -> Command {
    let mut command = product_binary::command();
    command
        .args(arguments)
        .current_dir(root.join("project"))
        .env("POLARIS_DATA_DIR", root.join("data"))
        .env("POLARIS_PROVIDER", "unavailable-fixture-provider")
        .env_remove("POLARIS_API_KEY");
    for name in ["HOME", "USERPROFILE", "APPDATA", "LOCALAPPDATA"] {
        command.env(name, root.join("home"));
    }
    for name in ["TMP", "TEMP", "TMPDIR"] {
        command.env(name, root.join("temporary"));
    }
    command
}

struct RunningChild {
    child: Option<Child>,
    stdout: Option<JoinHandle<Vec<u8>>>,
    stderr: Option<JoinHandle<Vec<u8>>>,
}

impl Drop for RunningChild {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
        for reader in [&mut self.stdout, &mut self.stderr] {
            if let Some(reader) = reader.take() {
                let _ = reader.join();
            }
        }
    }
}

fn read_output(mut pipe: impl Read) -> Vec<u8> {
    let mut bytes = Vec::new();
    pipe.read_to_end(&mut bytes).unwrap();
    bytes
}

fn output_before_stdin_eof(command: &mut Command) -> Output {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("launch Windows product");
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let mut running = RunningChild {
        child: Some(child),
        stdout: Some(std::thread::spawn(move || read_output(stdout))),
        stderr: Some(std::thread::spawn(move || read_output(stderr))),
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        // Keep the parent's stdin pipe open until the child has exited.
        if let Some(status) = running.child.as_mut().unwrap().try_wait().unwrap() {
            drop(running.child.take());
            return Output {
                status,
                stdout: running.stdout.take().unwrap().join().unwrap(),
                stderr: running.stderr.take().unwrap().join().unwrap(),
            };
        }
        assert!(
            Instant::now() < deadline,
            "product waited for stdin or hung"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn windows_unavailable_owner_blocks_turn_before_effect() {
    for arguments in [
        vec!["-p", "fixture instruction"],
        vec!["--audit", "audit.jsonl"],
        vec!["--model", "fixture"],
        vec!["--account", "fixture"],
        vec!["--remember"],
        vec!["--tool-memory", "history"],
        vec!["--mcp-external"],
        vec!["--mcp-prompt", "server/prompt"],
        vec!["--max-turns", "20"],
        vec!["--tool-memory", "off"],
        vec!["--web-search", "disabled"],
        vec!["--sandbox", "workspace-write"],
        vec!["--approval", "on-request"],
        vec!["--resume", "missing-session"],
        vec!["--resume", "missing-session", "-p", "continue"],
        vec!["--fork", "missing-session"],
        vec!["--fork", "missing-session", "-p", "continue"],
        vec!["exec", "fixture instruction"],
        vec!["sandbox", "--", "missing-fixture-program.exe"],
        vec!["common-tests", "missing-fixture-plan.json"],
    ] {
        let root = fixture();
        let before = windows_fixture::contents(root.path());
        let output = output_before_stdin_eof(&mut command(root.path(), &arguments));
        assert!(!output.status.success(), "{arguments:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(UNAVAILABLE),
            "{arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(windows_fixture::contents(root.path()), before);
    }
}

#[test]
fn windows_unavailable_owner_refuses_before_reading_stdin() {
    for arguments in [
        vec!["exec"],
        vec!["exec", "-"],
        vec!["common-tests", "-"],
        vec!["--confined-read"],
        vec!["--confined-apply"],
    ] {
        let root = fixture();
        let before = windows_fixture::contents(root.path());
        let output = output_before_stdin_eof(&mut command(root.path(), &arguments));
        assert!(!output.status.success(), "{arguments:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains(UNAVAILABLE));
        assert_eq!(windows_fixture::contents(root.path()), before);
    }
}

#[test]
fn windows_read_only_cli_metadata_remains_available() {
    for arguments in [
        vec!["--help"],
        vec!["--version"],
        vec!["completion", "powershell"],
    ] {
        let root = fixture();
        let before = windows_fixture::contents(root.path());
        let output = output_before_stdin_eof(&mut command(root.path(), &arguments));
        assert!(output.status.success(), "{arguments:?}: {output:?}");
        assert!(!output.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&output.stderr).contains(UNAVAILABLE));
        assert_eq!(windows_fixture::contents(root.path()), before);
    }
}

#[test]
fn windows_read_only_cli_reaches_terminal_check_without_state_setup() {
    let root = fixture();
    let before = windows_fixture::contents(root.path());
    let output = output_before_stdin_eof(&mut command(root.path(), &[]));
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("non-interactive terminal"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(UNAVAILABLE));
    assert_eq!(windows_fixture::contents(root.path()), before);
}

#[test]
fn windows_native_cli_commands_refuse_before_input_or_state_io() {
    let operation = "00000000000000000000000000000001";
    for arguments in [
        vec!["account", "list"],
        vec!["account", "login", "--label", "fixture"],
        vec!["account", "reauth", "fixture"],
        vec!["account", "logout", "fixture"],
        vec!["account", "label", "fixture", "label"],
        vec!["sync", "status"],
        vec!["sync", "push"],
        vec!["sync", "pull"],
        vec!["sync", "apply"],
        vec!["sync", "leave"],
        vec![
            "sync",
            "join",
            "--remote",
            "https://example.invalid/fixture.git",
            "--share-id",
            "share-0001",
            "--invitation-id",
            "invite-0001",
            "--expires",
            "4102444800",
        ],
        vec!["sync", "join", "--recover", operation, "--yes"],
        vec!["sync", "join", "--abandon", operation, "--yes"],
        vec!["sync", "join", "--rebind-credential", "--yes"],
        vec!["sync", "push", "--recover", operation],
        vec!["sync", "push", "--abandon", operation, "--yes"],
        vec!["sync", "recover", operation],
        vec!["sync", "abandon", operation, "--yes"],
        vec!["mcp", "list"],
        vec!["mcp", "enable", "fixture"],
        vec!["mcp", "disable", "fixture"],
        vec!["mcp", "secret-set", "fixture-reference"],
        vec!["mcp", "secret-delete", "fixture-reference"],
        vec!["mcp", "check", "fixture"],
        vec!["mcp", "setup", "--yes"],
        vec!["mcp", "materialize", "fixture", "--yes"],
        vec!["memory", "import", "missing.jsonl"],
        vec!["memory", "embed", "session", "id"],
        vec!["memory", "index", "session"],
        vec!["memory", "forget", "session"],
        vec![
            "memory",
            "source",
            "--session",
            "fixture-session",
            "fixture-source",
        ],
        vec![
            "memory",
            "search",
            "fixture",
            "--embedding-url",
            "http://127.0.0.1:9",
            "--embedding-model",
            "fixture",
        ],
        vec!["decided", "add", "fixture"],
        vec!["decided", "fixture"],
        vec!["setting", "git"],
        vec!["handoff", "--summary", "fixture", "--check", "fixture"],
    ] {
        let expected = match arguments[0] {
            "account" => {
                "Protected account storage is unavailable on Windows. No changes were made."
            }
            "sync" if arguments[1] == "apply" => {
                "Applying shared content is unsupported on Windows. No changes were made."
            }
            "sync" => "Windows lacks shared-state protection support. No changes were made.",
            "mcp" => "Protected MCP storage is unavailable on Windows. No changes were made.",
            "memory" => {
                "This platform lacks the protections required for memory mutation, source retrieval, and embedding requests"
            }
            "decided" => "Appending decisions is unsupported on Windows. No changes were made.",
            "setting" => {
                "This platform lacks the protections required for configuration changes. No changes were made."
            }
            "handoff" => {
                "This platform lacks the protections required to retrieve Git handoff information. No changes were made."
            }
            _ => unreachable!(),
        };
        let root = fixture();
        let before = windows_fixture::contents(root.path());
        let output = output_before_stdin_eof(&mut command(root.path(), &arguments));
        assert_eq!(output.status.code(), Some(1), "{arguments:?}: {output:?}");
        assert!(output.stdout.is_empty(), "{arguments:?}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected),
            "{arguments:?}: {output:?}"
        );
        assert_eq!(
            windows_fixture::contents(root.path()),
            before,
            "{arguments:?}"
        );
    }
}

#[test]
fn windows_memory_get_range_and_lexical_search_read_real_records_without_changes() {
    let root = fixture();
    let project = root.path().join("project").canonicalize().unwrap();
    let state = root
        .path()
        .join("data/state")
        .join(polaris_core::project::project_id(&project));
    fs::create_dir_all(&state).unwrap();
    let record = polaris_memory::Record {
        project_id: polaris_tui::persist::project_identity(&project).unwrap(),
        session_id: "fixture-session".into(),
        id: "fixture-record".into(),
        role: "user".into(),
        timestamp: "2026-10-08".into(),
        source: "fixture#record".into(),
        text: "A日本語🙂 needle".into(),
    };
    let mut store = polaris_memory::MemoryStore::open(state.join("memory.sqlite3")).unwrap();
    store.upsert(&record, None).unwrap();
    drop(store);
    let before = windows_fixture::contents(root.path());
    for arguments in [
        vec!["memory", "get", "fixture-session", "fixture-record"],
        vec![
            "memory",
            "get",
            "fixture-session",
            "fixture-record",
            "--start",
            "1",
            "--max-bytes",
            "7",
        ],
        vec!["memory", "search", "needle", "--session", "fixture-session"],
    ] {
        let output = output_before_stdin_eof(&mut command(root.path(), &arguments));
        assert!(output.status.success(), "{arguments:?}: {output:?}");
        assert!(output.stderr.is_empty(), "{arguments:?}: {output:?}");
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["kind"], "historical_evidence");
        if arguments[1] == "search" {
            let results = value["results"].as_array().unwrap();
            assert_eq!(results.len(), 1);
            for field in ["project_id", "session_id", "id", "source"] {
                assert_eq!(
                    results[0][field],
                    serde_json::to_value(&record).unwrap()[field]
                );
            }
            assert!(results[0]["excerpt"].as_str().unwrap().contains("needle"));
        } else if arguments.len() == 4 {
            assert_eq!(value["record"], serde_json::to_value(&record).unwrap());
            assert!(value.get("range").is_none());
        } else {
            assert_eq!(value["record"]["text"], "日本");
            assert_eq!(value["record"]["source"], record.source);
            assert_eq!(
                value["range"],
                serde_json::json!({
                    "start": 1, "end": 7, "total_bytes": record.text.len()
                })
            );
        }
        assert_eq!(
            windows_fixture::contents(root.path()),
            before,
            "{arguments:?}"
        );
    }
}

#[test]
fn windows_decided_and_compare_read_existing_files_without_changes() {
    let root = fixture();
    let project = root.path().join("project");
    let stamp = "2026-10-08T00:00:00+09:00";
    let decision = "Windows閲覧fixture";
    polaris_core::decisions::append(&project, stamp, decision).unwrap();
    for folder in ["a", "b"] {
        let directory = project.join(folder);
        fs::create_dir_all(directory.join("src")).unwrap();
        fs::write(directory.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(directory.join("README.md"), "fixture").unwrap();
    }
    let before = windows_fixture::contents(root.path());
    let mut decided_output = None;
    for arguments in [
        vec!["decided"],
        vec!["decided", "list"],
        vec!["compare", "a", "b"],
    ] {
        let output = output_before_stdin_eof(&mut command(root.path(), &arguments));
        assert!(output.status.success(), "{arguments:?}: {output:?}");
        assert!(output.stderr.is_empty(), "{arguments:?}: {output:?}");
        let text = String::from_utf8(output.stdout).unwrap();
        if arguments[0] == "decided" {
            assert!(text.contains(&format!("{stamp}  {decision}")), "{text}");
            assert!(text.contains("1 items"), "{text}");
            if let Some(previous) = &decided_output {
                assert_eq!(&text, previous);
            }
            decided_output = Some(text);
        } else {
            assert!(text.contains("a\tb\tsimilarity "), "{text}");
            assert!(text.contains("2 shared items"), "{text}");
            assert!(text.contains("Nothing is moved or deleted"), "{text}");
        }
        assert_eq!(
            windows_fixture::contents(root.path()),
            before,
            "{arguments:?}"
        );
    }
}

#[test]
fn windows_git_askpass_refuses_before_input_and_keeps_credentials_off_stdout() {
    for arguments in [
        vec![],
        vec!["Username for 'https://fixture.invalid': "],
        vec!["Password for 'https://fixture.invalid': "],
        vec!["unrecognized prompt"],
        vec!["Password for fixture", "extra argument"],
    ] {
        let root = fixture();
        let before = windows_fixture::contents(root.path());
        let mut child = Command::new(env!("CARGO_BIN_EXE_polaris-git-askpass"));
        child
            .args(&arguments)
            .current_dir(root.path().join("project"))
            .env("POLARIS_GIT_ASKPASS_USERNAME_FD", "3")
            .env("POLARIS_GIT_ASKPASS_PASSWORD_FD", "4")
            .env("HOME", root.path().join("home"))
            .env("USERPROFILE", root.path().join("home"));
        let output = output_before_stdin_eof(&mut child);
        assert_eq!(output.status.code(), Some(71), "{arguments:?}");
        assert!(output.stdout.is_empty(), "{arguments:?}");
        assert_eq!(
            String::from_utf8(output.stderr).unwrap().trim_end(),
            "polaris-git-askpass: credential handoff rejected",
            "{arguments:?}"
        );
        assert_eq!(windows_fixture::contents(root.path()), before);
    }
}
