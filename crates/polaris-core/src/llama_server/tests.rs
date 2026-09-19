//! The supervisor, against a stand-in server.
//!
//! Nothing here runs the real `llama-server`: what is being checked is the
//! refusals and the startup contract, and a 27MB binary loading Metal
//! would make the suite depend on the machine. The stand-in is a shell
//! script that speaks the three routes startup reads.

use super::*;

fn config(dir: &Path, server: PathBuf) -> LlamaCppConfig {
    let models = dir.join("models");
    std::fs::create_dir_all(&models).expect("mkdir");
    LlamaCppConfig {
        server,
        models_dir: models,
        build: "b11026-b49650adb".into(),
    }
}

/// Everything the supervisor needs is named in configuration, and a value
/// that is missing, relative, or a link is refused rather than guessed.
/// This module runs a program; a guess here is a guess about what to
/// execute.
#[tokio::test]
async fn nothing_starts_without_an_absolute_pinned_configuration() {
    let dir = tempfile::tempdir().expect("temp dir");
    let private = dir.path().join("private");
    let good = dir.path().join("llama-server");
    std::fs::write(&good, "#!/bin/sh\nsleep 60\n").expect("write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&good, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    let base = config(dir.path(), good.clone());

    for (label, broken) in [
        (
            "relative server",
            LlamaCppConfig {
                server: PathBuf::from("llama-server"),
                ..base.clone()
            },
        ),
        (
            "missing server",
            LlamaCppConfig {
                server: dir.path().join("absent"),
                ..base.clone()
            },
        ),
        (
            "models dir is a file",
            LlamaCppConfig {
                models_dir: good.clone(),
                ..base.clone()
            },
        ),
        (
            "relative models dir",
            LlamaCppConfig {
                models_dir: PathBuf::from("models"),
                ..base.clone()
            },
        ),
        (
            "no pinned build",
            LlamaCppConfig {
                build: "  ".into(),
                ..base.clone()
            },
        ),
    ] {
        assert!(
            LlamaServer::start(&broken, &private).await.is_err(),
            "{label} was accepted"
        );
    }

    #[cfg(unix)]
    {
        let link = dir.path().join("linked-server");
        std::os::unix::fs::symlink(&good, &link).expect("symlink");
        let via_link = LlamaCppConfig {
            server: link,
            ..base.clone()
        };
        assert!(
            LlamaServer::start(&via_link, &private).await.is_err(),
            "a symlinked server was accepted"
        );
    }
}

/// A child that dies must be reported as a death, not waited out. The
/// usual cause is a bad models directory, and the server has already
/// written the reason to its log by the time this is noticed.
#[tokio::test]
async fn a_server_that_exits_immediately_is_reported_rather_than_waited_out() {
    let dir = tempfile::tempdir().expect("temp dir");
    let private = dir.path().join("private");
    let path = dir.path().join("llama-server");
    std::fs::write(&path, "#!/bin/sh\nexit 3\n").expect("write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    let started = std::time::Instant::now();
    let error = match LlamaServer::start(&config(dir.path(), path), &private).await {
        Err(error) => error,
        Ok(_) => panic!("a dead child was treated as a live one"),
    };
    assert!(
        error.to_string().contains("終了"),
        "the message does not say the child exited: {error}"
    );
    assert!(
        started.elapsed() < READY_TIMEOUT,
        "it waited out the full readiness deadline for a process that was already gone"
    );
}

/// The token is the only thing between the model and everything else on
/// the machine, so it must not appear in a debug rendering that lands in a
/// log or an audit record.
#[test]
fn the_token_is_redacted_and_not_reused_between_runs() {
    let first = ApiToken::generate().expect("token");
    let second = ApiToken::generate().expect("token");
    assert_ne!(first.as_str(), second.as_str());
    assert_eq!(first.as_str().len(), 32);
    assert!(first.as_str().chars().all(|c| c.is_ascii_hexdigit()));
    let rendered = format!("{first:?}");
    assert!(
        !rendered.contains(first.as_str()),
        "the token leaked into Debug: {rendered}"
    );
}

/// Two servers must not be handed the same port. The check is weak by
/// construction — the port is released before the child binds it — so the
/// least this must do is not hand out one that is currently held.
#[test]
fn a_port_is_not_offered_while_something_is_listening_on_it() {
    let held = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let taken = held.local_addr().expect("addr").port();
    for _ in 0..16 {
        assert_ne!(free_loopback_port().expect("port"), taken);
    }
}

/// The build identity is the whole of the pin. A different llama-server
/// moves its routes, its flags and its chat templates, so talking to
/// whichever one is on disk is the same as not pinning at all.
#[test]
fn only_the_pinned_build_running_as_a_router_is_accepted() {
    let pinned = "b11026-b49650adb";
    let good = serde_json::json!({"role": "router", "build_info": pinned});
    assert!(build_matches(&good, pinned).is_ok());

    for (label, props) in [
        (
            "another build",
            serde_json::json!({"role": "router", "build_info": "b11500-deadbeef"}),
        ),
        ("no build reported", serde_json::json!({"role": "router"})),
        (
            "a single-model server, not a router",
            serde_json::json!({"role": "model", "build_info": pinned}),
        ),
        (
            "no role reported",
            serde_json::json!({"build_info": pinned}),
        ),
        ("nothing at all", serde_json::json!({})),
    ] {
        assert!(
            build_matches(&props, pinned).is_err(),
            "{label} was accepted"
        );
    }
}
