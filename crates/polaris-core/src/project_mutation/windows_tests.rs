//! Windows project mutation lease identity, exclusion and storage refusal tests.
use super::*;

fn assert_unsupported(result: Result<ProjectMutationOwnerV1, ProjectMutationError>) {
    match result {
        Err(ProjectMutationError::Io(error)) => {
            assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
        }
        _ => panic!("unsupported project mutation must refuse before accessing the root"),
    }
}

fn assert_root_unavailable(path: &Path) {
    assert_unsupported(ProjectMutationOwnerV1::for_project(path));
    match open_directory_nofollow(path) {
        Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::Unsupported),
        Ok(_) => panic!("unsupported directory access must fail"),
    }
}

use polaris_auth::protection::WindowsPinnedDirectory;
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const LEASE_WORKER: &str = "project_mutation::windows_tests::native_project_lease_child";
const DEADLINE: Duration = Duration::from_secs(5);

fn native_case(name: &str) -> PathBuf {
    let base = PathBuf::from(
        std::env::var_os("POLARIS_WINDOWS_STORAGE_TEST_ROOT")
            .expect("native lease tests require the explicit disposable test root"),
    );
    assert!(base.is_absolute());
    WindowsPinnedDirectory::open_existing(&base).unwrap();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let case = base.join(format!("lease-{name}-{}-{nonce}", std::process::id()));
    std::fs::create_dir(&case).unwrap();
    case // Retain native evidence, including failed cases.
}

fn native_owner(name: &str) -> (PathBuf, ProjectMutationOwnerV1) {
    let state = native_case(name).join("state");
    let root = WindowsPrivateRoot::create_new(&state).unwrap();
    let owner = ProjectMutationOwnerV1::at_store_root(&state.join("rewind-workspace")).unwrap();
    root.revalidate().unwrap();
    (state, owner)
}

#[test]
fn windows_project_lease_binding_and_shared_exclusion() {
    let (state, owner) = native_owner("shared");
    let other = ProjectMutationOwnerV1::open_existing(owner.store_root.as_path()).unwrap();
    let first = owner.try_activity().unwrap();
    let second = other.try_activity().unwrap();
    assert_eq!(first.store_identity(), second.store_identity());
    ProjectReadLeaseV1::Activity(&first).verify(&state).unwrap();
    assert!(matches!(
        owner.try_mutation(),
        Err(ProjectMutationError::Busy)
    ));
    drop(first);
    assert!(matches!(
        owner.try_mutation(),
        Err(ProjectMutationError::Busy)
    ));
    drop(second);
    let mutation = owner.try_mutation().unwrap();
    let identity = mutation.store_identity();
    let binding = mutation.bind_identity();
    let moved = mutation;
    binding.require_same_lease(&moved).unwrap();
    ProjectReadLeaseV1::Mutation(&moved).verify(&state).unwrap();
    assert!(
        ProjectReadLeaseV1::Mutation(&moved)
            .verify(&state.join("wrong"))
            .is_err()
    );
    assert!(matches!(
        other.try_mutation(),
        Err(ProjectMutationError::Busy)
    ));
    assert!(matches!(
        other.try_activity(),
        Err(ProjectMutationError::Busy)
    ));
    drop(owner); // The actual guard retains its store even when the owner is dropped.
    ProjectReadLeaseV1::Mutation(&moved).verify(&state).unwrap();
    drop(moved);
    let reacquired = other.try_mutation().unwrap();
    assert_eq!(reacquired.store_identity(), identity);
    assert!(matches!(
        binding.require_same_lease(&reacquired),
        Err(ProjectMutationError::InvalidStore(_))
    ));
}

#[test]
fn windows_project_lease_initializes_and_preserves_existing_lock_bytes() {
    let (state, owner) = native_owner("existing");
    let path = owner.store_root.join("writer.lease");
    assert_eq!(std::fs::read(&path).unwrap(), b"");
    let private = WindowsPrivateRoot::open_existing(&owner.store_root).unwrap();
    let mut file = private
        .open_file_for_rename(OsStr::new("writer.lease"))
        .unwrap();
    file.write_all(b"retained-lease\0\xff").unwrap();
    file.flush().unwrap();
    drop(file);
    let before = names(&owner.store_root);
    let reopened = ProjectMutationOwnerV1::at_store_root(&owner.store_root).unwrap();
    let lease = reopened.try_mutation().unwrap();
    ProjectReadLeaseV1::Mutation(&lease).verify(&state).unwrap();
    drop(lease);
    assert_eq!(std::fs::read(&path).unwrap(), b"retained-lease\0\xff");
    assert_eq!(names(&owner.store_root), before);
}

#[test]
fn windows_project_lease_rejects_replaced_named_lock() {
    let (_state, owner) = native_owner("replacement");
    let private = WindowsPrivateRoot::open_existing(&owner.store_root).unwrap();
    let mut original = private
        .open_file_for_rename(OsStr::new("writer.lease"))
        .unwrap();
    original
        .rename_no_clobber(private.directory(), OsStr::new("retained.lease"))
        .unwrap();
    let original_id = original.identity();
    drop(original);
    let mut replacement = private.create_file(OsStr::new("writer.lease")).unwrap();
    replacement.write_all(b"replacement").unwrap();
    replacement.flush().unwrap();
    assert_ne!(replacement.identity(), original_id);
    drop(replacement);
    let before = names(&owner.store_root);
    assert!(matches!(
        owner.try_mutation(),
        Err(ProjectMutationError::InvalidStore(_))
    ));
    assert!(matches!(
        owner.try_activity(),
        Err(ProjectMutationError::InvalidStore(_))
    ));
    assert_eq!(
        std::fs::read(owner.store_root.join("retained.lease")).unwrap(),
        b""
    );
    assert_eq!(
        std::fs::read(owner.store_root.join("writer.lease")).unwrap(),
        b"replacement"
    );
    assert_eq!(names(&owner.store_root), before);
}

#[test]
fn windows_project_lease_rejects_inherited_root_without_repair() {
    let path = native_case("unprotected");
    std::fs::write(path.join("writer.lease"), b"retained").unwrap();
    let pin = WindowsPinnedDirectory::open_existing(&path).unwrap();
    let identity = pin.identity();
    let before = names(&path);
    for result in [
        ProjectMutationOwnerV1::at_store_root(&path),
        ProjectMutationOwnerV1::open_existing(&path),
    ] {
        assert!(matches!(result, Err(ProjectMutationError::InvalidStore(_))));
    }
    assert_eq!(
        std::fs::read(path.join("writer.lease")).unwrap(),
        b"retained"
    );
    assert_eq!(names(&path), before);
    pin.revalidate().unwrap();
    assert_eq!(pin.identity(), identity);
}

#[test]
fn windows_project_lease_open_existing_never_initializes() {
    let case = native_case("missing");
    let absent = case.join("absent");
    assert!(ProjectMutationOwnerV1::open_existing(&absent).is_err());
    assert!(!absent.exists());
    let empty = case.join("empty");
    let private = WindowsPrivateRoot::create_new(&empty).unwrap();
    assert!(ProjectMutationOwnerV1::open_existing(&empty).is_err());
    assert!(names(&empty).is_empty());
    private.revalidate().unwrap();
}

struct OwnedLeaseChild {
    child: Child,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl OwnedLeaseChild {
    fn start(state: &Path, mode: &str) -> Self {
        let nonce = format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                LEASE_WORKER,
                "--nocapture",
                "--test-threads=1",
            ])
            .env("POLARIS_LEASE_CHILD_STATE", state)
            .env("POLARIS_LEASE_CHILD_MODE", mode)
            .env("POLARIS_LEASE_CHILD_NONCE", &nonce)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        // Own the child before a readiness failure can unwind.
        let mut owned = Self {
            child,
            reader: None,
        };
        let stdout = owned.child.stdout.take().unwrap();
        let marker = format!("LEASE-READY:{nonce}:{mode}");
        let (sender, receiver) = mpsc::channel();
        owned.reader = Some(std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if line == marker || line == format!("test {LEASE_WORKER} ... {marker}") {
                    let _ = sender.send(());
                }
                // Keep draining the pipe until EOF, including libtest completion.
            }
        }));
        receiver
            .recv_timeout(DEADLINE)
            .expect("actual lease child did not become ready");
        assert!(owned.child.try_wait().unwrap().is_none());
        owned
    }

    fn finish(&mut self, terminate: bool) {
        if terminate {
            self.child.kill().unwrap();
        } else {
            self.child
                .stdin
                .take()
                .unwrap()
                .write_all(b"release\n")
                .unwrap();
        }
        let deadline = Instant::now() + DEADLINE;
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "owned lease child failed to terminate"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(status.success(), !terminate);
        self.reader.take().unwrap().join().unwrap();
    }
}

impl Drop for OwnedLeaseChild {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
        }
        let deadline = Instant::now() + DEADLINE;
        while matches!(self.child.try_wait(), Ok(None)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[test]
fn windows_project_lease_process_exit_and_kill_release() {
    let (state, owner) = native_owner("process");
    let root_id = owner.windows.root.identity();
    let file_id = owner.windows.lease_identity;
    let before = names(&owner.store_root);
    let activity = owner.try_activity().unwrap();
    let mut shared_child = OwnedLeaseChild::start(&state, "shared");
    assert!(matches!(
        owner.try_mutation(),
        Err(ProjectMutationError::Busy)
    ));
    drop(activity);
    assert!(matches!(
        owner.try_mutation(),
        Err(ProjectMutationError::Busy)
    ));
    shared_child.finish(false);
    let mut exclusive_child = OwnedLeaseChild::start(&state, "exclusive");
    assert!(matches!(
        owner.try_mutation(),
        Err(ProjectMutationError::Busy)
    ));
    assert!(matches!(
        owner.try_activity(),
        Err(ProjectMutationError::Busy)
    ));
    exclusive_child.finish(true);
    let mut fresh_child = OwnedLeaseChild::start(&state, "exclusive");
    assert!(matches!(
        owner.try_mutation(),
        Err(ProjectMutationError::Busy)
    ));
    fresh_child.finish(false);
    let final_lease = owner.try_mutation().unwrap();
    ProjectReadLeaseV1::Mutation(&final_lease)
        .verify(&state)
        .unwrap();
    assert_eq!(owner.windows.root.identity(), root_id);
    assert_eq!(final_lease._guard.0.file.identity(), file_id);
    drop(final_lease);
    assert_eq!(names(&owner.store_root), before);
    assert_eq!(
        std::fs::read(owner.store_root.join("writer.lease")).unwrap(),
        b""
    );
}

#[test]
#[ignore = "internal actual lease child, invoked by the native process test"]
fn native_project_lease_child() {
    let state = PathBuf::from(std::env::var_os("POLARIS_LEASE_CHILD_STATE").expect("child state"));
    let mode = std::env::var("POLARIS_LEASE_CHILD_MODE").expect("child mode");
    let nonce = std::env::var("POLARIS_LEASE_CHILD_NONCE").expect("child nonce");
    let owner = ProjectMutationOwnerV1::open_existing(&state.join("rewind-workspace")).unwrap();
    let hold = |read: ProjectReadLeaseV1<'_>| {
        read.verify(&state).unwrap();
        println!("LEASE-READY:{nonce}:{mode}");
        std::io::stdout().flush().unwrap();
        let mut command = String::new();
        std::io::stdin().read_line(&mut command).unwrap();
        assert_eq!(command, "release\n");
        read.verify(&state).unwrap();
    };
    match mode.as_str() {
        "shared" => {
            let lease = owner.try_activity().unwrap();
            hold(ProjectReadLeaseV1::Activity(&lease));
        }
        "exclusive" => {
            let lease = owner.try_mutation().unwrap();
            hold(ProjectReadLeaseV1::Mutation(&lease));
        }
        _ => panic!("invalid lease child mode"),
    }
}

fn names(path: &Path) -> Vec<std::ffi::OsString> {
    let mut names: Vec<_> = std::fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    names.sort();
    names
}

#[test]
fn windows_project_mutation_unavailable_does_not_create_roots() {
    let parent = tempfile::tempdir().unwrap();
    let absent = parent.path().join("absent-project");
    let before = names(parent.path());
    assert_root_unavailable(&absent);
    assert!(!absent.exists());
    assert_eq!(names(parent.path()), before);
}

#[test]
fn windows_project_mutation_unavailable_preserves_existing_bytes_and_names() {
    let root = tempfile::tempdir().unwrap();
    let nested = root.path().join("nested");
    std::fs::create_dir(&nested).unwrap();
    let files = [
        root.path().join("writer.lease"),
        root.path().join("marker.bin"),
        nested.join("retained.bin"),
    ];
    let contents: [&[u8]; 3] = [
        b"retained-lease",
        b"retained\0\xffmarker",
        b"retained\0\xffnested",
    ];
    for (path, bytes) in files.iter().zip(contents) {
        std::fs::write(path, bytes).unwrap();
    }
    let before = [names(root.path()), names(&nested)];
    assert_root_unavailable(root.path());
    for (path, bytes) in files.iter().zip(contents) {
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
    assert_eq!([names(root.path()), names(&nested)], before);
}

#[test]
fn windows_project_mutation_unavailable_refuses_directory_enumeration_before_validation() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("retained.bin");
    let bytes = b"retained\0\xfffile";
    std::fs::write(&path, bytes).unwrap();
    let before = names(root.path());
    // A regular file verifies that refusal precedes even directory validation.
    let file = File::open(&path).unwrap();
    for limit in [0, usize::MAX] {
        let error = directory_names_bounded(&file, limit)
            .err()
            .expect("unsupported enumeration must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    }
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert_eq!(names(root.path()), before);
}
