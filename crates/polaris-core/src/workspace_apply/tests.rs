//! Conflict-safe apply-back, durable recovery artifacts, and failure injection tests.
use super::*;
use std::fs;
use std::os::unix::fs::symlink;

thread_local! {
    static PANIC_AT: std::cell::Cell<Option<Point>> = const { std::cell::Cell::new(None) };
}

pub(super) fn panic_at_checkpoint(point: Point) {
    PANIC_AT.with(|at| assert_ne!(at.get(), Some(point), "dummy apply panic"));
}

fn protected_fixture(case: &str) {
    let home = tempfile::tempdir().unwrap();
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "workspace_apply::tests::protected_apply_fixture",
            "--nocapture",
        ])
        .env_clear()
        .env("HOME", home.path().canonicalize().unwrap())
        .env("POLARIS_PROTECTED_APPLY_FIXTURE", case)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stdout).contains("protected-apply-verified"));
}

#[test]
fn protected_apply_allows_ordinary_authorized_fixture() {
    protected_fixture("ordinary");
}

#[test]
fn protected_apply_pinned_allows_ordinary_authorized_fixture() {
    protected_fixture("pinned");
}

#[test]
fn protected_apply_rejects_registered_rotated_identity() {
    protected_fixture("rotated");
}

#[test]
fn protected_apply_refuses_inaccessible_registry_without_fallback() {
    protected_fixture("poisoned");
}

#[test]
fn protected_apply_keeps_registry_lock_through_source_install() {
    protected_fixture("lock_scope");
}

#[test]
#[ignore = "subprocess fixture with isolated HOME and dummy credentials"]
fn protected_apply_fixture() {
    let case = match std::env::var("POLARIS_PROTECTED_APPLY_FIXTURE") {
        Ok(case)
            if matches!(
                case.as_str(),
                "ordinary" | "pinned" | "rotated" | "poisoned" | "lock_scope"
            ) =>
        {
            case
        }
        _ => return,
    };
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    let store = home.join("custom-store");
    polaris_auth::api_key::save_to(&store, "DUMMY_OLD_TOKEN").unwrap();
    let f = Fixture::new();
    if case == "rotated" {
        fs::rename(&store, f.source.join("ordinary.rs")).unwrap();
        polaris_auth::api_key::save_to(&store, "DUMMY_NEW_TOKEN").unwrap();
    } else {
        fs::write(f.source.join("ordinary.rs"), b"before").unwrap();
    }
    let before = fs::read(f.source.join("ordinary.rs")).unwrap();
    let before_identity = identity(&fs::metadata(f.source.join("ordinary.rs")).unwrap());
    // Deliberately use an empty caller registry to build a stale request that
    // contains the dummy rotated inode. protected_apply must import the real
    // process registry rather than trusting that omission or only current names.
    let snapshot = f.snapshot();
    fs::write(snapshot.path().join("ordinary.rs"), b"candidate").unwrap();
    let request = changes(&snapshot);
    assert_eq!(request.changes.len(), 1);
    if case == "poisoned" {
        let poisoned = std::panic::catch_unwind(|| {
            let _ =
                polaris_auth::protection::with_protected_paths(|_| panic!("dummy registry poison"));
        });
        assert!(poisoned.is_err());
    }
    if case == "lock_scope" {
        // A panic after install poisons the registry iff the wrapper still
        // holds its lock at that point. No scheduling or timeout assumption.
        PANIC_AT.with(|point| point.set(Some(Point::AfterInstall)));
        let interrupted = std::panic::catch_unwind(|| {
            protected_apply(&snapshot, &request, limits(), &f.recovery)
        });
        PANIC_AT.with(|point| point.set(None));
        assert!(interrupted.is_err());
        assert!(polaris_auth::protection::with_protected_paths(|_| ()).is_err());
        assert_eq!(
            fs::read(f.source.join("ordinary.rs")).unwrap(),
            b"candidate"
        );
        let recovery = fs::read_dir(&f.recovery)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(fs::read(recovery.join("old-0")).unwrap(), before);
        println!("protected-apply-verified");
        return;
    }
    let report = if case == "pinned" {
        protected_apply_pinned(&snapshot, &request, limits(), &pinned_parent(&f))
    } else {
        protected_apply(&snapshot, &request, limits(), &f.recovery)
    };
    match case.as_str() {
        "ordinary" | "pinned" => {
            assert!(report.failure.is_none(), "{report:?}");
            assert!(report.entries[0].installed);
            assert_eq!(
                fs::read(f.source.join("ordinary.rs")).unwrap(),
                b"candidate"
            );
            assert_eq!(fs::read(old_path(&report, 0)).unwrap(), before);
            assert_eq!(
                polaris_auth::api_key::load_from(&store).unwrap().as_deref(),
                Some("DUMMY_OLD_TOKEN")
            );
        }
        "rotated" | "poisoned" => {
            assert!(report.failure.is_some(), "{report:?}");
            assert_eq!(
                report.failure.as_ref().unwrap().kind,
                if case == "poisoned" {
                    FailureKind::ProtectionUnavailable
                } else {
                    FailureKind::InvalidChangeSet
                }
            );
            assert!(report.entries.is_empty());
            assert!(report.recovery_path.is_none());
            assert!(fs::read_dir(&f.recovery).unwrap().next().is_none());
            assert_eq!(fs::read(f.source.join("ordinary.rs")).unwrap(), before);
            assert_eq!(
                identity(&fs::metadata(f.source.join("ordinary.rs")).unwrap()),
                before_identity
            );
            if case == "rotated" {
                assert_eq!(
                    polaris_auth::api_key::load_from(&store).unwrap().as_deref(),
                    Some("DUMMY_NEW_TOKEN")
                );
            }
        }
        _ => unreachable!(),
    }
    println!("protected-apply-verified");
}

struct Fixture {
    _temp: tempfile::TempDir,
    source: PathBuf,
    recovery: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let source = base.join("source");
        let recovery = base.join("recovery");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&recovery).unwrap();
        Self {
            _temp: temp,
            source,
            recovery,
        }
    }
    fn snapshot(&self) -> Snapshot {
        copy::snapshot(&self.source, &secrets(), limits()).unwrap()
    }
}
fn limits() -> Limits {
    Limits {
        max_entries: 64,
        max_files: 32,
        max_file_bytes: 4096,
        max_total_bytes: 32768,
        max_depth: 8,
    }
}
fn secrets() -> RegisteredSecrets {
    RegisteredSecrets::new([]).unwrap()
}
fn changes(snapshot: &Snapshot) -> ChangeSet {
    copy::collect_changes(snapshot, &secrets(), limits()).unwrap()
}
fn failure() -> std::io::Error {
    std::io::Error::other("injected")
}
fn old_path(report: &ApplyReport, index: usize) -> PathBuf {
    report
        .recovery_path
        .as_ref()
        .unwrap()
        .join(&report.entries[index].old_name)
}
fn candidate_path(report: &ApplyReport, index: usize) -> PathBuf {
    report
        .recovery_path
        .as_ref()
        .unwrap()
        .join(report.entries[index].candidate_name.as_ref().unwrap())
}
fn run_fixture(f: &Fixture, snapshot: &Snapshot, hook: &mut Hook<'_>) -> ApplyReport {
    apply_with_hook(
        snapshot,
        &changes(snapshot),
        &secrets(),
        limits(),
        &f.recovery,
        hook,
    )
}

#[test]
fn applies_only_plain_files_and_keeps_originals_and_fixed_candidates_after_success() {
    let f = Fixture::new();
    fs::write(f.source.join("modify"), b"before").unwrap();
    fs::write(f.source.join("delete"), b"retained-delete").unwrap();
    fs::write(f.source.join("mode"), b"unchanged").unwrap();
    fs::set_permissions(f.source.join("mode"), Permissions::from_mode(0o600)).unwrap();
    let snapshot = f.snapshot();
    fs::write(snapshot.path().join("modify"), b"after").unwrap();
    fs::write(snapshot.path().join("create"), b"created").unwrap();
    fs::remove_file(snapshot.path().join("delete")).unwrap();
    fs::set_permissions(snapshot.path().join("mode"), Permissions::from_mode(0o751)).unwrap();
    let report = run_fixture(&f, &snapshot, &mut |_, _, _| Ok(()));
    assert!(report.failure.is_none(), "{report:?}");
    assert_eq!(report.entries.len(), 4);
    assert_eq!(fs::read(f.source.join("modify")).unwrap(), b"after");
    assert_eq!(fs::read(f.source.join("create")).unwrap(), b"created");
    assert!(!f.source.join("delete").exists());
    assert_eq!(
        fs::metadata(f.source.join("mode")).unwrap().mode() & 0o7777,
        0o751
    );
    for (i, entry) in report.entries.iter().enumerate() {
        if entry.relative_path != Path::new("create") {
            assert!(old_path(&report, i).is_file());
        }
        if entry.installed {
            assert!(candidate_path(&report, i).is_file());
            assert_ne!(
                identity(&fs::metadata(candidate_path(&report, i)).unwrap()),
                identity(&fs::metadata(f.source.join(&entry.relative_path)).unwrap())
            );
        }
    }
    let recovery = report.recovery_path.clone().unwrap();
    drop(snapshot);
    drop(report);
    assert!(
        recovery.join("journal.jsonl").is_file(),
        "no automatic cleanup"
    );
}

#[test]
fn rejects_tampered_omitted_duplicate_and_foreign_changesets_before_any_source_move() {
    for tamper in 0..5 {
        let f = Fixture::new();
        fs::write(f.source.join("file"), b"before").unwrap();
        let snapshot = f.snapshot();
        fs::write(snapshot.path().join("file"), b"after").unwrap();
        let mut request = changes(&snapshot);
        match tamper {
            0 => request.changes[0].after.as_mut().unwrap().sha256 = Some([0; 32]),
            1 => request.changes.clear(),
            2 => request.changes.push(request.changes[0].clone()),
            3 => request.changes[0].relative_path = "../outside".into(),
            _ => {
                let other = f.snapshot();
                fs::write(other.path().join("file"), b"after").unwrap();
                request = changes(&other);
            }
        }
        let report = apply(&snapshot, &request, &secrets(), limits(), &f.recovery);
        assert!(report.failure.is_some(), "{tamper}");
        assert!(report.recovery_path.is_none());
        assert_eq!(fs::read(f.source.join("file")).unwrap(), b"before");
    }
}

#[test]
fn rejects_directory_deletion_type_replacement_and_external_recovery_scope() {
    for replace in [false, true] {
        let f = Fixture::new();
        fs::create_dir(f.source.join("dir")).unwrap();
        let snapshot = f.snapshot();
        fs::remove_dir(snapshot.path().join("dir")).unwrap();
        if replace {
            fs::write(snapshot.path().join("dir"), b"file").unwrap();
        }
        let report = apply(
            &snapshot,
            &changes(&snapshot),
            &secrets(),
            limits(),
            &f.recovery,
        );
        assert_eq!(report.failure.unwrap().kind, FailureKind::UnsupportedEntry);
        assert!(f.source.join("dir").is_dir());
        assert!(report.recovery_path.is_none());
    }
    let f = Fixture::new();
    let snapshot = f.snapshot();
    fs::write(snapshot.path().join("new"), b"new").unwrap();
    for recovery in [f.source.as_path(), snapshot.scratch_home()] {
        assert!(
            apply(
                &snapshot,
                &changes(&snapshot),
                &secrets(),
                limits(),
                recovery
            )
            .failure
            .is_some()
        );
    }
}

#[test]
fn candidate_is_fixed_before_source_moves_and_detects_copy_races() {
    for mutate_before in [false, true] {
        let f = Fixture::new();
        fs::write(f.source.join("file"), b"before").unwrap();
        let snapshot = f.snapshot();
        fs::write(snapshot.path().join("file"), b"approved").unwrap();
        let report = run_fixture(&f, &snapshot, &mut |point, _, _| {
            if point
                == if mutate_before {
                    Point::BeforePrepare
                } else {
                    Point::AfterPrepared
                }
            {
                fs::write(snapshot.path().join("file"), b"unapproved")?;
            }
            Ok(())
        });
        if mutate_before {
            assert!(report.failure.is_some());
            assert_eq!(fs::read(f.source.join("file")).unwrap(), b"before");
        } else {
            assert!(report.failure.is_none(), "{report:?}");
            assert_eq!(fs::read(f.source.join("file")).unwrap(), b"approved");
            assert_eq!(fs::read(candidate_path(&report, 0)).unwrap(), b"approved");
        }
    }
}

#[test]
fn source_entry_swapped_after_preflight_is_exclusively_restored() {
    let f = Fixture::new();
    fs::write(f.source.join("file"), b"baseline").unwrap();
    let snapshot = f.snapshot();
    fs::write(snapshot.path().join("file"), b"candidate").unwrap();
    let report = run_fixture(&f, &snapshot, &mut |point, _, _| {
        if point == Point::BeforePreserve {
            fs::write(f.source.join("editor-save"), b"editor")?;
            fs::rename(f.source.join("editor-save"), f.source.join("file"))?;
        }
        Ok(())
    });
    assert_eq!(report.failure.as_ref().unwrap().kind, FailureKind::Conflict);
    assert!(report.entries[0].restored);
    assert!(!report.entries[0].installed);
    assert_eq!(fs::read(f.source.join("file")).unwrap(), b"editor");
    assert_eq!(fs::read(candidate_path(&report, 0)).unwrap(), b"candidate");
}

#[test]
fn exclusive_install_and_restore_never_overwrite_a_racing_editor() {
    for restoring in [false, true] {
        let f = Fixture::new();
        fs::write(f.source.join("file"), b"baseline").unwrap();
        let snapshot = f.snapshot();
        fs::write(snapshot.path().join("file"), b"candidate").unwrap();
        let report = run_fixture(&f, &snapshot, &mut |point, _, _| {
            if restoring && point == Point::BeforePreserve {
                fs::write(f.source.join("file"), b"changed-before-capture")?;
            }
            if point
                == if restoring {
                    Point::BeforeRestore
                } else {
                    Point::BeforeInstall
                }
            {
                fs::write(f.source.join("file"), b"editor-current")?;
            }
            Ok(())
        });
        assert!(report.failure.is_some());
        assert!(report.entries[0].old_retained);
        assert!(!report.entries[0].installed);
        assert_eq!(fs::read(f.source.join("file")).unwrap(), b"editor-current");
        assert_eq!(
            fs::read(old_path(&report, 0)).unwrap(),
            if restoring {
                &b"changed-before-capture"[..]
            } else {
                &b"baseline"[..]
            }
        );
        assert_eq!(fs::read(candidate_path(&report, 0)).unwrap(), b"candidate");
    }
}

#[test]
fn opened_editor_fd_keeps_writing_retained_inode_even_after_success() {
    let f = Fixture::new();
    fs::write(f.source.join("file"), b"baseline").unwrap();
    let mut editor = fs::OpenOptions::new()
        .write(true)
        .open(f.source.join("file"))
        .unwrap();
    let snapshot = f.snapshot();
    fs::write(snapshot.path().join("file"), b"candidate").unwrap();
    let report = run_fixture(&f, &snapshot, &mut |point, _, _| {
        if point == Point::AfterVerified {
            editor.set_len(0)?;
            editor.write_all(b"late-editor")?;
            editor.sync_all()?;
        }
        Ok(())
    });
    assert!(report.failure.is_none(), "{report:?}");
    assert_eq!(fs::read(f.source.join("file")).unwrap(), b"candidate");
    assert_eq!(fs::read(old_path(&report, 0)).unwrap(), b"late-editor");
    editor.write_all(b"-after-return").unwrap();
    editor.sync_all().unwrap();
    assert_eq!(
        fs::read(old_path(&report, 0)).unwrap(),
        b"late-editor-after-return"
    );
}

#[test]
fn parent_directory_rename_stops_and_preserves_without_targeting_replacement_directory() {
    let f = Fixture::new();
    fs::create_dir(f.source.join("dir")).unwrap();
    fs::write(f.source.join("dir/file"), b"baseline").unwrap();
    let snapshot = f.snapshot();
    fs::write(snapshot.path().join("dir/file"), b"candidate").unwrap();
    let report = run_fixture(&f, &snapshot, &mut |point, _, _| {
        if point == Point::AfterVerified {
            fs::rename(f.source.join("dir"), f.source.join("moved"))?;
            fs::create_dir(f.source.join("dir"))?;
            fs::write(f.source.join("dir/file"), b"replacement-dir")?;
        }
        Ok(())
    });
    assert_eq!(
        report.failure.as_ref().unwrap().kind,
        FailureKind::AncestorChanged
    );
    assert!(report.entries[0].old_retained);
    assert_eq!(fs::read(old_path(&report, 0)).unwrap(), b"baseline");
    assert_eq!(
        fs::read(f.source.join("dir/file")).unwrap(),
        b"replacement-dir"
    );
    assert!(!f.source.join("moved/file").exists());
}

#[test]
fn post_install_ancestor_change_reports_effect_and_keeps_recovery_handle() {
    for move_recovery in [false, true] {
        let f = Fixture::new();
        fs::write(f.source.join("file"), b"baseline").unwrap();
        let snapshot = f.snapshot();
        fs::write(snapshot.path().join("file"), b"candidate").unwrap();
        let moved = f.source.parent().unwrap().join("moved");
        let report = run_fixture(&f, &snapshot, &mut |point, _, _| {
            if point == Point::AfterInstall {
                fs::rename(
                    if move_recovery {
                        &f.recovery
                    } else {
                        &f.source
                    },
                    &moved,
                )?;
            }
            Ok(())
        });
        assert_eq!(
            report.failure.as_ref().unwrap().kind,
            FailureKind::AncestorChanged
        );
        assert!(report.entries[0].installed);
        let pinned = report.recovery_directory().unwrap();
        assert_eq!(
            Some(identity(&pinned.metadata().unwrap())),
            report.recovery_identity
        );
        let mut old = open_at(
            pinned,
            OsStr::new(&report.entries[0].old_name),
            false,
            false,
        )
        .unwrap();
        let mut body = String::new();
        old.read_to_string(&mut body).unwrap();
        assert_eq!(body, "baseline");
        assert_eq!(
            fs::read(if move_recovery {
                f.source.join("file")
            } else {
                moved.join("file")
            })
            .unwrap(),
            b"candidate"
        );
    }
}

#[test]
fn late_secret_inode_or_symlink_swap_is_not_hashed_and_is_restored() {
    for link in [false, true] {
        let f = Fixture::new();
        fs::write(f.source.join("file"), b"baseline").unwrap();
        let protected = f.recovery.join("registered");
        // Larger than the read cap: success of the Secret branch, rather than
        // a size/hash conflict, shows registration was checked before content.
        fs::write(&protected, vec![b'x'; 8192]).unwrap();
        let registry = RegisteredSecrets::new([protected.clone()]).unwrap();
        let snapshot = copy::snapshot(&f.source, &registry, limits()).unwrap();
        fs::write(snapshot.path().join("file"), b"candidate").unwrap();
        let request = copy::collect_changes(&snapshot, &registry, limits()).unwrap();
        let report = apply_with_hook(
            &snapshot,
            &request,
            &registry,
            limits(),
            &f.recovery,
            &mut |point, _, _| {
                if point == Point::BeforePreserve {
                    fs::remove_file(f.source.join("file"))?;
                    if link {
                        symlink(&protected, f.source.join("file"))?;
                    } else {
                        fs::rename(&protected, f.source.join("file"))?;
                    }
                }
                Ok(())
            },
        );
        assert!(report.failure.is_some());
        if !link {
            assert_eq!(report.failure.as_ref().unwrap().kind, FailureKind::Secret);
        }
        assert!(report.entries[0].restored);
        assert!(!report.entries[0].installed);
    }
}

#[test]
fn delete_and_create_races_preserve_every_existing_version() {
    for deleting in [false, true] {
        let f = Fixture::new();
        if deleting {
            fs::write(f.source.join("file"), b"old").unwrap();
        }
        let snapshot = f.snapshot();
        if deleting {
            fs::remove_file(snapshot.path().join("file")).unwrap();
        } else {
            fs::write(snapshot.path().join("file"), b"candidate").unwrap();
        }
        let report = run_fixture(&f, &snapshot, &mut |point, _, _| {
            if point
                == if deleting {
                    Point::BeforeDelete
                } else {
                    Point::BeforeInstall
                }
            {
                fs::write(f.source.join("file"), b"racing")?;
            }
            Ok(())
        });
        assert!(report.failure.is_some());
        assert_eq!(fs::read(f.source.join("file")).unwrap(), b"racing");
        assert!(!report.entries[0].deleted && !report.entries[0].installed);
        if deleting {
            assert_eq!(fs::read(old_path(&report, 0)).unwrap(), b"old");
        } else {
            assert_eq!(fs::read(candidate_path(&report, 0)).unwrap(), b"candidate");
        }
    }
}

#[test]
fn failures_after_rename_report_partial_apply_and_leave_durable_intent_without_blind_rollback() {
    for point in [Point::AfterPreserve, Point::AfterInstall] {
        let f = Fixture::new();
        for name in ["a", "b"] {
            fs::write(f.source.join(name), b"old").unwrap();
        }
        let snapshot = f.snapshot();
        for name in ["a", "b"] {
            fs::write(snapshot.path().join(name), b"new").unwrap();
        }
        let report = run_fixture(&f, &snapshot, &mut |at, index, _| {
            if at == point && index == 1 {
                Err(failure())
            } else {
                Ok(())
            }
        });
        assert_eq!(report.failure.as_ref().unwrap().entry, Some(1));
        assert!(report.entries[0].installed);
        assert_eq!(fs::read(f.source.join("a")).unwrap(), b"new");
        assert_eq!(fs::read(old_path(&report, 0)).unwrap(), b"old");
        assert_eq!(fs::read(old_path(&report, 1)).unwrap(), b"old");
        assert_eq!(report.entries[1].installed, point == Point::AfterInstall);
        let log = fs::read_to_string(report.recovery_path.as_ref().unwrap().join("journal.jsonl"))
            .unwrap();
        let last: serde_json::Value = serde_json::from_str(log.lines().last().unwrap()).unwrap();
        assert_eq!(
            last["phase"],
            if point == Point::AfterInstall {
                "InstallIntent"
            } else {
                "PreserveIntent"
            }
        );
        assert!(!report.entries[1].restored);
    }
}

#[test]
fn journal_failure_prevents_source_mutation_and_exdev_has_no_copy_unlink_fallback() {
    for cross_device in [false, true] {
        let f = Fixture::new();
        fs::write(f.source.join("file"), b"old").unwrap();
        let snapshot = f.snapshot();
        fs::write(snapshot.path().join("file"), b"new").unwrap();
        let report = run_fixture(&f, &snapshot, &mut |point, _, _| {
            if point
                == if cross_device {
                    Point::BeforeInstall
                } else {
                    Point::BeforeJournal
                }
            {
                return Err(if cross_device {
                    std::io::Error::from_raw_os_error(libc::EXDEV)
                } else {
                    failure()
                });
            }
            Ok(())
        });
        assert_eq!(
            report.failure.as_ref().unwrap().kind,
            if cross_device {
                FailureKind::CrossDevice
            } else {
                FailureKind::Io
            }
        );
        assert!(!report.entries[0].installed);
        if cross_device {
            assert_eq!(fs::read(old_path(&report, 0)).unwrap(), b"old");
            assert!(!f.source.join("file").exists());
        } else {
            assert_eq!(fs::read(f.source.join("file")).unwrap(), b"old");
        }
    }
}

fn pinned_parent(f: &Fixture) -> RecoveryParent {
    fs::set_permissions(&f.recovery, Permissions::from_mode(0o700)).unwrap();
    let directory = File::open(&f.recovery).unwrap();
    RecoveryParent::pin(
        &directory,
        identity(&directory.metadata().unwrap()),
        &f.recovery,
    )
    .unwrap()
}

#[test]
fn pinned_recovery_rejects_wrong_identity_before_source_changes() {
    let f = Fixture::new();
    fs::write(f.source.join("a"), b"before").unwrap();
    let snapshot = f.snapshot();
    fs::write(snapshot.path().join("a"), b"after").unwrap();
    let request = changes(&snapshot);
    let expected = pinned_parent(&f);
    let wrong = tempfile::tempdir().unwrap();
    let wrong_fd = File::open(wrong.path()).unwrap();
    assert!(RecoveryParent::pin(&wrong_fd, expected.expected, &f.recovery).is_err());
    // Also exercise the apply boundary independently of the public constructor.
    let wrong = RecoveryParent {
        directory: wrong_fd,
        expected: expected.expected,
        provenance: f.recovery.clone(),
    };
    let report = apply_pinned_with_hook(
        &snapshot,
        &request,
        &secrets(),
        limits(),
        &wrong,
        &mut |_, _, _| Ok(()),
    );
    assert_eq!(report.failure.unwrap().kind, FailureKind::AncestorChanged);
    assert!(report.recovery_path.is_none());
    assert_eq!(fs::read(f.source.join("a")).unwrap(), b"before");
    assert!(fs::read_dir(&f.recovery).unwrap().next().is_none());
}

#[test]
fn pinned_recovery_checks_private_directory_and_actual_ancestry() {
    let f = Fixture::new();
    fs::write(f.source.join("a"), b"before").unwrap();
    let snapshot = f.snapshot();
    fs::write(snapshot.path().join("a"), b"after").unwrap();
    let request = changes(&snapshot);
    let file = File::open(f.source.join("a")).unwrap();
    assert!(RecoveryParent::pin(&file, identity(&file.metadata().unwrap()), &f.recovery).is_err());
    let pinned = pinned_parent(&f);
    assert_eq!(
        pinned.identity(),
        identity(&fs::metadata(&f.recovery).unwrap())
    );
    pinned.validate_identity().unwrap();
    fs::set_permissions(&f.recovery, Permissions::from_mode(0o755)).unwrap();
    assert!(pinned.validate_identity().is_err());
    assert!(
        apply_pinned_with_hook(
            &snapshot,
            &request,
            &secrets(),
            limits(),
            &pinned,
            &mut |_, _, _| Ok(()),
        )
        .failure
        .is_some()
    );
    for inside in [f.source.join("private"), snapshot.path().join("private")] {
        fs::create_dir(&inside).unwrap();
        fs::set_permissions(&inside, Permissions::from_mode(0o700)).unwrap();
        let file = File::open(&inside).unwrap();
        // An innocent-looking provenance path must not authorize an inside FD.
        let pinned =
            RecoveryParent::pin(&file, identity(&file.metadata().unwrap()), &f.recovery).unwrap();
        assert!(pinned.validate_for_snapshot(&snapshot).is_err());
        let report = apply_pinned_with_hook(
            &snapshot,
            &request,
            &secrets(),
            limits(),
            &pinned,
            &mut |_, _, _| Ok(()),
        );
        assert_eq!(report.failure.unwrap().kind, FailureKind::InvalidChangeSet);
        assert!(fs::read_dir(&inside).unwrap().next().is_none());
    }
    assert_eq!(fs::read(f.source.join("a")).unwrap(), b"before");
}

#[test]
fn pinned_recovery_parent_replacement_never_selects_new_path() {
    for at in [None, Some(Point::AfterPreserve), Some(Point::AfterInstall)] {
        let f = Fixture::new();
        fs::write(f.source.join("a"), b"before").unwrap();
        let snapshot = f.snapshot();
        fs::write(snapshot.path().join("a"), b"after").unwrap();
        let request = changes(&snapshot);
        let pinned = pinned_parent(&f);
        let moved = f.recovery.with_file_name("moved-recovery");
        let replace = || {
            fs::rename(&f.recovery, &moved).unwrap();
            fs::create_dir(&f.recovery).unwrap();
            fs::write(f.recovery.join("sentinel"), b"untouched").unwrap();
        };
        if at.is_none() {
            replace();
        }
        pinned.validate_for_snapshot(&snapshot).unwrap();
        let mut replaced = at.is_none();
        let report = apply_pinned_with_hook(
            &snapshot,
            &request,
            &secrets(),
            limits(),
            &pinned,
            &mut |point, _, _| {
                if !replaced && at == Some(point) {
                    replace();
                    replaced = true;
                }
                Ok(())
            },
        );
        assert!(replaced);
        assert!(report.failure.is_none(), "{:?}", report.failure);
        assert!(report.entries[0].installed && report.entries[0].old_retained);
        drop(pinned);
        assert_eq!(fs::read(f.source.join("a")).unwrap(), b"after");
        assert_eq!(fs::read_dir(&f.recovery).unwrap().count(), 1);
        assert_eq!(fs::read(f.recovery.join("sentinel")).unwrap(), b"untouched");
        let recovery = report.recovery_directory().unwrap();
        assert_eq!(
            Some(identity(&recovery.metadata().unwrap())),
            report.recovery_identity
        );
        let mut old = open_at(recovery, OsStr::new("old-0"), false, false).unwrap();
        let mut body = String::new();
        old.read_to_string(&mut body).unwrap();
        assert_eq!(body, "before");
        assert!(open_at(recovery, OsStr::new("journal.jsonl"), false, false).is_ok());
        let actual = moved.join(report.recovery_path.as_ref().unwrap().file_name().unwrap());
        assert_eq!(
            identity(&fs::metadata(actual).unwrap()),
            report.recovery_identity.unwrap()
        );
    }
}

#[test]
fn pinned_pre_intent_scope_rejects_replaced_source() {
    let f = Fixture::new();
    fs::write(f.source.join("a"), b"before").unwrap();
    let snapshot = f.snapshot();
    let pinned = pinned_parent(&f);
    pinned.validate_for_snapshot(&snapshot).unwrap();
    let moved = f.source.with_file_name("old-source");
    fs::rename(&f.source, &moved).unwrap();
    fs::create_dir(&f.source).unwrap();
    fs::write(f.source.join("sentinel"), b"untouched").unwrap();
    assert!(pinned.validate_for_snapshot(&snapshot).is_err());
    assert_eq!(fs::read(f.source.join("sentinel")).unwrap(), b"untouched");
    assert!(fs::read_dir(&f.recovery).unwrap().next().is_none());
}
