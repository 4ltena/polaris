//! Windows filesystem primitives remain private until owner integration.
//! The shared implementation lives in auth protection to avoid a reverse dependency.

#[cfg(test)]
mod tests {
    use polaris_auth::AuthError;
    use polaris_auth::protection::{WindowsPinnedDirectory, WindowsPrivateRoot};
    use std::ffi::OsStr;
    use std::fs;
    use std::io::{BufRead, BufReader, Write};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    const BEFORE: &[u8] = b"native-before-bytes\r\n";
    const AFTER: &[u8] = b"native-after-bytes\r\n";
    const WORKER: &str = "windows_fs::tests::native_fs_child_worker";
    const PHASES: &[&str] = &[
        "reservation",
        "file_flush",
        "source_retained",
        "target_publication",
        "final_read_back",
    ];

    fn base() -> PathBuf {
        let path = PathBuf::from(
            std::env::var_os("POLARIS_WINDOWS_STORAGE_TEST_ROOT")
                .expect("native storage acceptance needs an explicit disposable test root"),
        );
        assert!(path.is_absolute(), "test root must be absolute");
        assert!(path.is_dir(), "test root must already exist");
        WindowsPinnedDirectory::open_existing(&path).expect("test root must pass native pinning");
        path
    }

    fn case(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = base().join(format!("{name}-{}-{nonce}", std::process::id()));
        fs::create_dir(&path).unwrap();
        // Retain native fixtures and failed cases for review; do not erase evidence.
        path
    }

    fn private(name: &str) -> (PathBuf, WindowsPrivateRoot) {
        let path = case(name).join("state");
        let root = WindowsPrivateRoot::create_new(&path).unwrap();
        (path, root)
    }

    fn write(root: &WindowsPrivateRoot, name: &str, body: &[u8]) {
        let mut file = root.create_file(OsStr::new(name)).unwrap();
        file.write_all(body).unwrap();
        file.flush().unwrap();
    }

    #[test]
    fn windows_private_root_create_reopen_identity() {
        let (path, root) = private("private-positive");
        let identity = root.identity();
        root.revalidate().unwrap();
        write(&root, "payload", BEFORE);
        let reopened = WindowsPrivateRoot::open_existing(&path).unwrap();
        assert_eq!(reopened.identity(), identity);
        assert_eq!(
            reopened
                .open_file(OsStr::new("payload"))
                .unwrap()
                .read_bytes(1024)
                .unwrap(),
            BEFORE
        );
        assert!(WindowsPrivateRoot::create_new(&path).is_err());
        root.revalidate().unwrap();
    }

    #[test]
    fn windows_private_root_reject_inherited_without_repair() {
        let path = case("private-denial");
        fs::write(path.join("marker"), BEFORE).unwrap();
        let pin = WindowsPinnedDirectory::open_existing(&path).unwrap();
        let identity = pin.identity();
        assert!(matches!(
            WindowsPrivateRoot::open_existing(&path),
            Err(AuthError::Protection)
        ));
        assert_eq!(pin.identity(), identity);
        pin.revalidate().unwrap();
        assert_eq!(fs::read(path.join("marker")).unwrap(), BEFORE);
    }

    #[test]
    fn windows_private_root_reject_raw_final_alias_before_io() {
        for name in [
            r"D:\polaris-m2-invalid-parent\future\.",
            r"D:\polaris-m2-invalid-parent\future\..",
            r"D:\polaris-m2-invalid-parent\future\",
            r"D:\polaris-m2-invalid-parent\future.",
        ] {
            assert!(matches!(
                WindowsPrivateRoot::create_new(Path::new(name)),
                Err(AuthError::Protection)
            ));
        }
    }

    #[test]
    fn windows_reparse_component_ordinary_relative_read() {
        let path = case("path-positive");
        fs::create_dir(path.join("normal")).unwrap();
        fs::write(path.join("normal").join("payload"), BEFORE).unwrap();
        let root = WindowsPinnedDirectory::open_existing(&path).unwrap();
        let normal = root.child(OsStr::new("normal")).unwrap();
        let file = normal.open_file(OsStr::new("payload")).unwrap();
        assert_eq!(file.read_bytes(1024).unwrap(), BEFORE);
        normal.revalidate().unwrap();
        root.revalidate().unwrap();
    }

    #[test]
    fn windows_reparse_component_reject_both_retained_links() {
        let path = PathBuf::from(
            std::env::var_os("POLARIS_WINDOWS_LINK_FIXTURE")
                .expect("supply the retained approved L-01 path-binding fixture"),
        );
        assert!(
            fs::symlink_metadata(path.join("alias"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(
            fs::symlink_metadata(path.join("normal").join("alias-file"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_link(path.join("alias")).unwrap(),
            Path::new("normal")
        );
        assert_eq!(
            fs::read_link(path.join("normal").join("alias-file")).unwrap(),
            Path::new("payload")
        );
        let root = WindowsPinnedDirectory::open_existing(&path).unwrap();
        let normal = root.child(OsStr::new("normal")).unwrap();
        assert_eq!(
            normal
                .open_file(OsStr::new("payload"))
                .unwrap()
                .read_bytes(1024)
                .unwrap(),
            fs::read(path.join("normal").join("payload")).unwrap()
        );
        assert!(root.child(OsStr::new("alias")).is_err());
        assert!(normal.open_file(OsStr::new("alias-file")).is_err());
        // The absolute entry must not canonicalize away the directory link.
        assert!(WindowsPinnedDirectory::open_existing(&path.join("alias")).is_err());
        root.revalidate().unwrap();
        normal.revalidate().unwrap();
    }

    #[test]
    fn windows_reparse_component_reject_ambiguous_names_and_replacement() {
        let (path, root) = private("path-denial");
        for name in [
            "", ".", "..", "a\\b", "a/b", "x:stream", "NUL", "COM1.txt", "tail.", "tail ", "\0",
        ] {
            assert!(root.create_file(OsStr::new(name)).is_err(), "{name:?}");
        }
        assert!(fs::rename(&path, path.with_file_name("replaced")).is_err());
        root.revalidate().unwrap();
        assert!(fs::read_dir(&path).unwrap().next().is_none());
    }

    #[test]
    fn windows_no_clobber_publish_keeps_full_identity() {
        let (_path, root) = private("publish-positive");
        let mut staged = root.create_file(OsStr::new("stage")).unwrap();
        staged.write_all(AFTER).unwrap();
        staged.flush().unwrap();
        let identity = staged.identity();
        staged
            .rename_no_clobber(root.directory(), OsStr::new("target"))
            .unwrap();
        assert_eq!(staged.identity(), identity);
        assert_eq!(staged.read_bytes(1024).unwrap(), AFTER);
        drop(staged);
        assert!(root.open_file(OsStr::new("stage")).is_err());
        let target = root.open_file(OsStr::new("target")).unwrap();
        assert_eq!(target.identity(), identity);
        assert_eq!(target.read_bytes(1024).unwrap(), AFTER);
    }

    #[test]
    fn windows_no_clobber_collision_preserves_both_and_exclusive_create() {
        let (path, root) = private("publish-denial");
        write(&root, "target", BEFORE);
        assert!(root.create_file(OsStr::new("target")).is_err());
        let mut staged = root.create_file(OsStr::new("stage")).unwrap();
        staged.write_all(AFTER).unwrap();
        staged.flush().unwrap();
        let identity = staged.identity();
        assert!(
            staged
                .rename_no_clobber(root.directory(), OsStr::new("target"))
                .is_err()
        );
        assert_eq!(staged.identity(), identity);
        assert_eq!(staged.read_bytes(1024).unwrap(), AFTER);
        assert_eq!(fs::read(path.join("stage")).unwrap(), AFTER);
        assert_eq!(
            root.open_file(OsStr::new("target"))
                .unwrap()
                .read_bytes(1024)
                .unwrap(),
            BEFORE
        );
    }

    #[test]
    fn windows_cross_volume_same_volume_publication() {
        let path = case("volume-positive");
        let source = WindowsPrivateRoot::create_new(&path.join("source")).unwrap();
        let destination = WindowsPrivateRoot::create_new(&path.join("destination")).unwrap();
        let mut file = source.create_file(OsStr::new("stage")).unwrap();
        file.write_all(AFTER).unwrap();
        file.flush().unwrap();
        assert!(file.identity().same_volume(destination.identity()));
        file.rename_no_clobber(destination.directory(), OsStr::new("target"))
            .unwrap();
        assert_eq!(file.read_bytes(1024).unwrap(), AFTER);
        drop(file);
        assert_eq!(
            destination
                .open_file(OsStr::new("target"))
                .unwrap()
                .read_bytes(1024)
                .unwrap(),
            AFTER
        );
        assert!(source.open_file(OsStr::new("stage")).is_err());
    }

    #[test]
    fn windows_cross_volume_refuse_before_mutation() {
        let comparison = PathBuf::from(
            std::env::var_os("POLARIS_WINDOWS_COMPARISON_ROOT")
                .expect("supply a metadata-only directory on the other acceptance volume"),
        );
        let destination = WindowsPinnedDirectory::open_existing(&comparison).unwrap();
        let (path, source) = private("volume-denial");
        let mut file = source.create_file(OsStr::new("stage")).unwrap();
        file.write_all(AFTER).unwrap();
        file.flush().unwrap();
        let identity = file.identity();
        assert!(
            !identity.same_volume(destination.identity()),
            "comparison must be a different volume"
        );
        assert!(matches!(
            file.rename_no_clobber(&destination, OsStr::new("polaris-m2-cross-volume-refused")),
            Err(AuthError::Protection)
        ));
        assert_eq!(file.identity(), identity);
        assert_eq!(file.read_bytes(1024).unwrap(), AFTER);
        assert_eq!(fs::read(path.join("stage")).unwrap(), AFTER);
        destination.revalidate().unwrap();
    }

    #[test]
    fn windows_durability_flush_close_reopen_bytes() {
        let (path, root) = private("flush-positive");
        write(&root, "payload", AFTER);
        drop(root);
        let reopened = WindowsPrivateRoot::open_existing(&path).unwrap();
        assert_eq!(
            reopened
                .open_file(OsStr::new("payload"))
                .unwrap()
                .read_bytes(1024)
                .unwrap(),
            AFTER
        );
    }

    #[test]
    fn windows_durability_read_only_flush_failure_preserves_bytes() {
        let (_path, root) = private("flush-denial");
        write(&root, "payload", BEFORE);
        let file = root.open_file(OsStr::new("payload")).unwrap();
        assert!(file.flush().is_err());
        assert_eq!(file.read_bytes(1024).unwrap(), BEFORE);
        assert!(matches!(file.read_bytes(1), Err(AuthError::Protection)));
    }

    struct OwnedChild(Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            if matches!(self.0.try_wait(), Ok(None)) {
                let _ = self.0.kill();
            }
            let _ = self.0.wait();
        }
    }

    fn worker_marker(line: &str, marker: &str) -> bool {
        line == marker || line == format!("test {WORKER} ... {marker}")
    }

    fn child_command(path: &Path, nonce: &str, phase: &str, mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--ignored",
                "--exact",
                WORKER,
                "--nocapture",
                "--test-threads=1",
            ])
            .env("POLARIS_M2_CHILD_CASE", path)
            .env("POLARIS_M2_CHILD_NONCE", nonce)
            .env("POLARIS_M2_CHILD_PHASE", phase)
            .env("POLARIS_M2_CHILD_MODE", mode)
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .stdout(Stdio::piped());
        command
    }

    fn expected(path: &Path, phase: &str) {
        let source_retained = matches!(
            phase,
            "source_retained" | "target_publication" | "final_read_back"
        );
        let published = matches!(phase, "target_publication" | "final_read_back");
        assert_eq!(
            fs::read(path.join(if source_retained {
                "retained"
            } else {
                "source"
            }))
            .unwrap(),
            BEFORE
        );
        assert!(
            !path
                .join(if source_retained {
                    "source"
                } else {
                    "retained"
                })
                .exists()
        );
        assert_eq!(
            fs::read(path.join(if published { "target" } else { "stage" })).unwrap(),
            if phase == "reservation" {
                b"".as_slice()
            } else {
                AFTER
            }
        );
        assert!(
            !path
                .join(if published { "stage" } else { "target" })
                .exists()
        );
    }

    #[test]
    fn windows_durability_owned_process_termination_and_fresh_readback() {
        for phase in PHASES {
            let (path, root) = private(&format!("process-{phase}"));
            let identity = root.identity();
            write(&root, "source", BEFORE);
            let nonce = format!(
                "{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            let mut child = OwnedChild(
                child_command(&path, &nonce, phase, "write")
                    .spawn()
                    .unwrap(),
            );
            let stdout = child.0.stdout.take().unwrap();
            let (sender, receiver) = mpsc::channel();
            let checkpoint = format!("M2-CHECKPOINT:{nonce}:{phase}");
            let expected_checkpoint = checkpoint.clone();
            let reader = std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let line = line.unwrap();
                    if worker_marker(&line, &expected_checkpoint) {
                        let _ = sender.send(());
                        return;
                    }
                }
            });
            receiver
                .recv_timeout(Duration::from_secs(20))
                .expect("child did not reach the actual requested boundary");
            assert!(child.0.try_wait().unwrap().is_none());
            // Parent observes actual files while the writer is stopped at its checkpoint.
            expected(&path, phase);
            root.revalidate().unwrap();
            assert_eq!(root.identity(), identity);
            child.0.kill().unwrap();
            let status = child.0.wait().unwrap();
            assert!(
                !status.success(),
                "writer must terminate, not complete normally"
            );
            reader.join().unwrap();
            expected(&path, phase);
            let fresh = child_command(&path, &nonce, phase, "read")
                .output()
                .unwrap();
            assert!(
                fresh.status.success(),
                "fresh native reader failed: {}",
                String::from_utf8_lossy(&fresh.stderr)
            );
            assert!(
                String::from_utf8_lossy(&fresh.stdout)
                    .lines()
                    .any(|line| worker_marker(line, &format!("M2-READBACK:{nonce}:{phase}")))
            );
            root.revalidate().unwrap();
            assert_eq!(root.identity(), identity);
        }
    }

    #[test]
    #[ignore = "internal child worker, invoked only by the native process-boundary test"]
    fn native_fs_child_worker() {
        let path = PathBuf::from(std::env::var_os("POLARIS_M2_CHILD_CASE").expect("child case"));
        let nonce = std::env::var("POLARIS_M2_CHILD_NONCE").expect("child nonce");
        let phase = std::env::var("POLARIS_M2_CHILD_PHASE").expect("child phase");
        assert!(PHASES.contains(&phase.as_str()));
        let root = WindowsPrivateRoot::open_existing(&path).unwrap();
        if std::env::var("POLARIS_M2_CHILD_MODE").unwrap() == "read" {
            expected(&path, &phase);
            for (name, body) in [
                ("source", BEFORE),
                ("retained", BEFORE),
                ("stage", AFTER),
                ("target", AFTER),
            ] {
                if path.join(name).exists() {
                    let actual = root
                        .open_file(OsStr::new(name))
                        .unwrap()
                        .read_bytes(1024)
                        .unwrap();
                    assert_eq!(
                        actual,
                        if phase == "reservation" && name == "stage" {
                            b"".as_slice()
                        } else {
                            body
                        }
                    );
                }
            }
            println!("M2-READBACK:{nonce}:{phase}");
            return;
        }
        let checkpoint = |current: &str| {
            if current == phase {
                println!("M2-CHECKPOINT:{nonce}:{phase}");
                std::io::stdout().flush().unwrap();
                loop {
                    std::thread::park();
                }
            }
        };
        let mut staged = root.create_file(OsStr::new("stage")).unwrap();
        checkpoint("reservation");
        staged.write_all(AFTER).unwrap();
        staged.flush().unwrap();
        checkpoint("file_flush");
        let mut source = root.open_file_for_rename(OsStr::new("source")).unwrap();
        source
            .rename_no_clobber(root.directory(), OsStr::new("retained"))
            .unwrap();
        assert_eq!(source.read_bytes(1024).unwrap(), BEFORE);
        checkpoint("source_retained");
        staged
            .rename_no_clobber(root.directory(), OsStr::new("target"))
            .unwrap();
        checkpoint("target_publication");
        assert_eq!(staged.read_bytes(1024).unwrap(), AFTER);
        root.revalidate().unwrap();
        checkpoint("final_read_back");
        panic!("requested child checkpoint was not reached");
    }
}
