//! Real stdio cancellation with a synthetic unconfirmed cleanup registry entry.
//! Run in a child so the global recovery lease cannot affect concurrent tests.
use super::tests::{check, engine};
use super::*;
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

struct Release(Arc<AtomicBool>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[test]
fn cancelled_catalog_keeps_private_scratch_until_cleanup_confirmation() {
    const CHILD: &str = "POLARIS_CATALOG_CLEANUP_FIXTURE";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "engine::mcp_catalog::cleanup_tests::cancelled_catalog_keeps_private_scratch_until_cleanup_confirmation", "--nocapture"])
            .env(CHILD, "1").output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    for draining in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let home = base.join("account");
        let cwd = base.join("server");
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(&cwd).unwrap();
        std::fs::set_permissions(&cwd, std::fs::Permissions::from_mode(0o700)).unwrap();
        let server = polaris_mcp::registration::Server {
            server_id: "fixture".into(), enabled_projects: Default::default(),
            transport: polaris_mcp::registration::Destination::Stdio {
                command: "/bin/sh".into(),
                args: vec!["-c".into(), "printf ok > \"$HOME/marker\"; printf ok > \"$TMPDIR/marker\"; printf '%s\\n' \"$HOME\" > started; exec /bin/sleep 60".into()],
                cwd: cwd.clone(), sandbox_profile: "workspace-write".into(), secret_env: Default::default(),
                readable_roots: vec![], executable_sha256: None, script: None, script_sha256: None,
            },
            protocol_profile: Default::default(),
            capabilities: polaris_mcp::registration::Capabilities { catalog_metadata: true, ..Default::default() },
            timeouts: polaris_mcp::registration::Timeouts { connect_seconds: 60, connect_retries: 0, ..Default::default() },
        };
        let (_root, mut engine) = engine(&home, server);
        engine.mcp_settings.as_mut().unwrap().runtime_roots =
            ["/bin", "/usr/bin", "/usr/lib", "/System/Library"]
                .map(std::path::PathBuf::from)
                .to_vec();
        let before = engine.store.snapshot().unwrap();
        assert!(
            engine
                .start_mcp_catalog(&check(&engine, "fixture"))
                .unwrap()
                .is_none()
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        let home = loop {
            if let Ok(home) = std::fs::read_to_string(cwd.join("started"))
                && !home.trim().is_empty()
            {
                break std::path::PathBuf::from(home.trim());
            }
            assert!(Instant::now() < deadline, "stdio fixture did not start");
            std::thread::sleep(Duration::from_millis(5));
        };
        let scratch = home.parent().unwrap().to_owned();
        let hold = Release(engine.cleanups.inject_pending_cleanup_for_test());
        if draining {
            engine.draining = true;
        } else {
            assert_eq!(
                engine
                    .cancel_mcp_catalog(&McpCatalogTarget {
                        server_id: "fixture".into()
                    })
                    .state,
                McpCatalogCancelState::Cancelling
            );
        }
        let mut answers = Vec::new();
        loop {
            if let Some(response) = engine.finish_mcp_catalog(false) {
                answers.push(response);
            }
            let job = engine
                .mcp_catalog
                .as_ref()
                .expect("unconfirmed cleanup must retain the job");
            if job.worker.is_none() {
                break;
            }
            assert!(Instant::now() < deadline, "stdio cancellation did not join");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(scratch.join("home/marker").is_file());
        assert!(scratch.join("tmp/marker").is_file());
        if draining {
            assert!(answers.is_empty());
        } else {
            assert_eq!(answers.len(), 1);
            assert!(
                matches!(&answers[0].outcome, Ok(SuccessResult::McpCatalogCheck(result)) if result.outcome == McpCatalogOutcome::Cancelled)
            );
        }
        assert!(engine.finish_mcp_catalog(false).is_none());
        assert!(engine.mcp_catalog.is_some() && scratch.exists());
        assert_eq!(
            engine
                .start_mcp_catalog(&check(&engine, "fixture"))
                .unwrap_err()
                .code,
            ErrorCode::SessionBusy
        );
        assert!(
            engine
                .mcp_settings
                .as_ref()
                .unwrap()
                .store
                .load_catalog_index()
                .unwrap()
                .is_none()
        );
        drop(hold);
        while engine.mcp_catalog.is_some() {
            assert!(engine.finish_mcp_catalog(false).is_none());
            assert!(
                Instant::now() < deadline,
                "confirmed cleanup was not released"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!scratch.exists());
        let after = engine.store.snapshot().unwrap();
        assert_eq!(after.marker, before.marker);
        assert!(after.state.runs.is_empty());
    }
}
