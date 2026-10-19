//! Git credential runner authorization, execution and replay regression tests.
use super::*;

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

static ENVIRONMENT_LOCK: Mutex<()> = Mutex::new(());

struct EnvironmentMarkerGuard;

impl EnvironmentMarkerGuard {
    fn set() -> Self {
        // The test lock keeps this synthetic marker isolated from this module's
        // other tests; the child itself must prove that env_clear removed it.
        unsafe {
            std::env::set_var(
                "POLARIS_RUNNER_TEST_PARENT_MARKER",
                "inherited-parent-value",
            );
        }
        Self
    }
}

impl Drop for EnvironmentMarkerGuard {
    fn drop(&mut self) {
        unsafe { std::env::remove_var("POLARIS_RUNNER_TEST_PARENT_MARKER") };
    }
}

struct RunnerFixture {
    root: tempfile::TempDir,
    git: PathBuf,
    askpass: PathBuf,
}

impl RunnerFixture {
    fn owner(&self) -> GitCredentialOwnerV1 {
        GitCredentialOwnerV1 {
            binding_key_path: self.root.path().join("binding.key"),
            git: ExecutableIdentityV1::inspect(&self.git).expect("inspect git fixture"),
            askpass: ExecutableIdentityV1::inspect(&self.askpass).expect("inspect askpass fixture"),
        }
    }
}

fn fixture(git_body: &str) -> RunnerFixture {
    let root = tempfile::tempdir().expect("create fixture directory");
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700))
        .expect("protect fixture directory");
    assert_eq!(root.path().canonicalize().unwrap(), root.path());

    let git = root.path().join("bound-git");
    let askpass = root.path().join("bound-askpass");
    write_script(&git, git_body);
    write_script(&askpass, "#!/bin/sh\nexit 0\n");
    RunnerFixture { root, git, askpass }
}

fn write_script(path: &Path, body: &str) {
    assert!(path.is_absolute());
    std::fs::write(path, body).expect("write owned script");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o500))
        .expect("protect owned script");
    assert_eq!(std::fs::metadata(path).unwrap().mode() & 0o777, 0o500);
}

fn material() -> GitCredentialMaterialV1 {
    GitCredentialMaterialV1 {
        username: Zeroizing::new(b"runner-user".to_vec()),
        password: Zeroizing::new(b"runner-password".to_vec()),
    }
}

fn run(
    owner: &GitCredentialOwnerV1,
    args: &[OsString],
    timeout: Duration,
) -> Result<(), GitCredentialErrorV1> {
    owner.run_bound_git_with_timeout(args, &material(), timeout)
}

fn assert_pid_gone(pid: i32) {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let result = unsafe { libc::kill(pid, 0) };
        if result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "owned descendant {pid} remained alive"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn runner_hands_off_separate_readonly_pipes_with_cleared_environment_and_null_stdin() {
    let _environment = ENVIRONMENT_LOCK.lock().unwrap();
    let _marker = EnvironmentMarkerGuard::set();
    let fixture = fixture(
        "#!/bin/sh\n\
         test -z \"${POLARIS_RUNNER_TEST_PARENT_MARKER+x}\" || exit 10\n\
         test \"$(/bin/cat <&3)\" = runner-user || exit 11\n\
         test \"$(/bin/cat <&4)\" = runner-password || exit 12\n\
         if printf x >&3 2>/dev/null; then exit 13; fi\n\
         if printf x >&4 2>/dev/null; then exit 14; fi\n\
         IFS= read -r unexpected && exit 15\n\
         exit 0\n",
    );
    let owner = fixture.owner();
    let started = Instant::now();
    let result = run_git_child(
        &owner.git,
        Some(&owner.askpass),
        Some(&material()),
        &[],
        None,
        // This verifies FD semantics, not sub-second process startup.
        CHILD_TIMEOUT,
    )
    .map(|output| output.exit_code());
    assert!(
        matches!(result, Ok(Some(0))),
        "anonymous FD handoff must succeed: {result:?}, elapsed {:?}",
        started.elapsed()
    );
}

#[test]
fn runner_rejects_secret_argument_before_spawn() {
    let fixture = fixture("#!/bin/sh\nprintf ran > \"$1\"\n");
    let marker = fixture.root.path().join("spawned");
    let owner = fixture.owner();
    let args = [
        OsString::from("runner-password"),
        marker.as_os_str().to_owned(),
    ];
    assert!(matches!(
        run(&owner, &args, Duration::from_millis(400)),
        Err(GitCredentialErrorV1::Protection)
    ));
    assert!(!marker.exists(), "secret argv must prevent spawn");
}

#[test]
fn runner_enforces_independent_stdout_and_stderr_64kib_limits() {
    let fixture = fixture(
        "#!/bin/sh\n\
         case \"$1\" in\n\
           stdout) printf '%65536d' 0 ;;\n\
           stderr) printf '%65536d' 0 >&2 ;;\n\
           stdout-over) printf '%65537d' 0 ;;\n\
           stderr-over) printf '%65537d' 0 >&2 ;;\n\
         esac\n",
    );
    let owner = fixture.owner();
    // Output boundaries must not also depend on sub-second process startup.
    for argument in ["stdout", "stderr"] {
        let result = run(&owner, &[OsString::from(argument)], CHILD_TIMEOUT);
        assert!(
            result.is_ok(),
            "{argument} must accept exactly 64 KiB: {result:?}"
        );
    }
    for argument in ["stdout-over", "stderr-over"] {
        let result = run(&owner, &[OsString::from(argument)], CHILD_TIMEOUT);
        assert!(
            matches!(&result, Err(GitCredentialErrorV1::Protection)),
            "{argument} must reject 64 KiB plus one byte: {result:?}"
        );
    }
}

#[test]
fn runner_rejects_invalid_utf8_child_output() {
    let fixture = fixture("#!/bin/sh\nprintf '\\377'\n");
    let owner = fixture.owner();
    assert!(matches!(
        run(&owner, &[], Duration::from_millis(400)),
        Err(GitCredentialErrorV1::Protection)
    ));
}

#[test]
fn runner_times_out_when_child_does_not_read_credential_pipes() {
    let fixture = fixture("#!/bin/sh\n/bin/sleep 5\n");
    let owner = fixture.owner();
    let started = Instant::now();
    assert!(matches!(
        run(&owner, &[], Duration::from_millis(100)),
        Err(GitCredentialErrorV1::Protection)
    ));
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn runner_reaps_group_when_exited_child_descendant_keeps_output_open() {
    let fixture = fixture(
        "#!/bin/sh\n\
         /bin/cat <&3 >/dev/null\n\
         /bin/cat <&4 >/dev/null\n\
         /bin/sleep 10 &\n\
         descendant=$!\n\
         printf '%s' \"$descendant\" > \"$1\"\n\
         exit 0\n",
    );
    let pid_file = fixture.root.path().join("descendant.pid");
    let owner = fixture.owner();
    // Allow the shell/FD consumers to record the descendant before the
    // deadline; its ten-second open pipes must still be cut off early.
    let started = Instant::now();
    assert!(matches!(
        run(
            &owner,
            &[pid_file.as_os_str().to_owned()],
            Duration::from_secs(3)
        ),
        Err(GitCredentialErrorV1::Protection)
    ));
    assert!(started.elapsed() < Duration::from_secs(5));
    let pid = std::fs::read_to_string(pid_file)
        .expect("owned fixture records descendant PID")
        .parse::<i32>()
        .expect("numeric descendant PID");
    assert_pid_gone(pid);
}

#[test]
fn runner_refuses_replaced_or_unsafe_bound_binary_before_spawn() {
    let replacement = fixture("#!/bin/sh\nprintf replacement-ran > \"$1\"\n");
    let replaced_marker = replacement.root.path().join("replaced-marker");
    let replaced_owner = replacement.owner();
    let new_git = replacement.root.path().join("new-bound-git");
    write_script(&new_git, "#!/bin/sh\nprintf replacement-ran > \"$1\"\n");
    std::fs::rename(&new_git, &replacement.git).expect("replace bound binary");
    assert!(matches!(
        run(
            &replaced_owner,
            &[replaced_marker.as_os_str().to_owned()],
            Duration::from_millis(400)
        ),
        Err(GitCredentialErrorV1::BindingChanged)
    ));
    assert!(!replaced_marker.exists());

    let unsafe_fixture = fixture("#!/bin/sh\nprintf unsafe-ran > \"$1\"\n");
    let unsafe_marker = unsafe_fixture.root.path().join("unsafe-marker");
    let unsafe_owner = unsafe_fixture.owner();
    std::fs::set_permissions(&unsafe_fixture.git, std::fs::Permissions::from_mode(0o522))
        .expect("make fixture binary unsafe");
    assert!(matches!(
        run(
            &unsafe_owner,
            &[unsafe_marker.as_os_str().to_owned()],
            Duration::from_millis(400)
        ),
        Err(GitCredentialErrorV1::Protection)
    ));
    assert!(!unsafe_marker.exists());
}

#[test]
fn typed_runner_returns_stdout_and_nonzero_exit_without_exposing_stderr() {
    let fixture = fixture(
        "#!/bin/sh\n/bin/cat <&3 >/dev/null\n/bin/cat <&4 >/dev/null\nprintf exact-object-id\nprintf untrusted-diagnostic >&2\nexit 7\n",
    );
    let owner = fixture.owner();
    let output = owner.run_bound_git_output(&[], &material()).unwrap();
    assert_eq!(output.stdout(), b"exact-object-id");
    assert!(!output.success());
    assert_eq!(output.exit_code(), Some(7));
    assert!(matches!(
        run(&owner, &[], Duration::from_millis(400)),
        Err(GitCredentialErrorV1::Protection)
    ));
}

#[test]
fn typed_runner_before_spawn_failures_preserve_zero_effect() {
    let fixture = fixture("#!/bin/sh\nprintf spawned > \"$1\"\n");
    let marker = fixture.root.path().join("spawned");
    let owner = fixture.owner();
    let args = [
        marker.as_os_str().to_owned(),
        OsString::from("runner-password"),
    ];
    let failure = owner
        .run_bound_git_output(&args, &material())
        .err()
        .unwrap();
    assert_eq!(failure.phase(), GitChildFailurePhaseV1::BeforeSpawn);
    assert_eq!(failure.to_string(), "git_child_failed_before_spawn");
    assert!(!marker.exists());
    std::fs::remove_file(&fixture.askpass).unwrap();
    let failure = owner
        .run_bound_git_output(&[marker.as_os_str().to_owned()], &material())
        .err()
        .unwrap();
    assert_eq!(failure.phase(), GitChildFailurePhaseV1::BeforeSpawn);
    assert!(!marker.exists());
}

#[test]
fn typed_runner_after_spawn_uncertainty_cannot_claim_zero_send() {
    for script in [
        "#!/bin/sh\n/bin/sleep 2\n",
        "#!/bin/sh\nprintf '%65537d' 0\n",
        "#!/bin/sh\nprintf '\\377'\n",
        "#!/bin/sh\nkill -TERM $$\n",
    ] {
        let fixture = fixture(script);
        let owner = fixture.owner();
        let material = material();
        let failure = run_git_child(
            &owner.git,
            Some(&owner.askpass),
            Some(&material),
            &[],
            None,
            Duration::from_millis(100),
        )
        .err()
        .unwrap();
        assert_eq!(failure.phase(), GitChildFailurePhaseV1::AfterSpawn);
        assert_eq!(failure.to_string(), "git_child_failed_after_spawn");
    }
}

#[test]
fn local_runner_clears_credential_environment_and_sets_only_pinned_index_and_private_umask() {
    let _environment = ENVIRONMENT_LOCK.lock().unwrap();
    let _marker = EnvironmentMarkerGuard::set();
    let fixture = fixture(
        "#!/bin/sh\n\
        test -z \"${POLARIS_RUNNER_TEST_PARENT_MARKER+x}\" || exit 10\n\
        test -z \"${POLARIS_GIT_ASKPASS_USERNAME_FD+x}\" || exit 11\n\
        test -z \"${POLARIS_GIT_ASKPASS_PASSWORD_FD+x}\" || exit 12\n\
        test \"$GIT_ASKPASS\" = '' || exit 13\n\
        test \"$GIT_INDEX_FILE\" = \"$1\" || exit 14\n\
        test \"$GIT_OPTIONAL_LOCKS\" = 0 || exit 15\n\
        test \"$GIT_CONFIG_GLOBAL\" = /dev/null || exit 16\n\
        test \"$GIT_CONFIG_SYSTEM\" = /dev/null || exit 17\n\
        test -z \"${GIT_AUTHOR_NAME+x}\" || exit 18\n\
        IFS= read -r unexpected && exit 19\n\
        printf private > \"$2\"\nprintf local-object-id\n",
    );
    let owner = fixture.owner();
    let index = fixture.root.path().join("explicit-index");
    let created = fixture.root.path().join("owned-created");
    let output = run_bound_local_git(
        &owner.git,
        &[index.as_os_str().to_owned(), created.as_os_str().to_owned()],
        &index,
    )
    .unwrap();
    assert!(output.success());
    assert_eq!(output.stdout(), b"local-object-id");
    assert_eq!(std::fs::metadata(created).unwrap().mode() & 0o777, 0o600);
    assert!(!index.exists());
    assert!(!owner.binding_key_path.exists());
}

#[test]
fn local_runner_rejects_ambiguous_index_before_spawn() {
    let fixture = fixture("#!/bin/sh\nprintf spawned > \"$1\"\n");
    let owner = fixture.owner();
    let marker = fixture.root.path().join("spawned");
    for index in [
        PathBuf::from("relative-index"),
        fixture.root.path().join("./index"),
        fixture.root.path().join("../index"),
        PathBuf::from(format!("{}/index/", fixture.root.path().display())),
        PathBuf::from(format!("{}//index", fixture.root.path().display())),
    ] {
        let failure = run_bound_local_git(&owner.git, &[marker.as_os_str().to_owned()], &index)
            .err()
            .unwrap();
        assert_eq!(failure.phase(), GitChildFailurePhaseV1::BeforeSpawn);
        assert!(!marker.exists());
    }
}

#[test]
fn local_runner_rechecks_bound_binary_and_output_protection() {
    let fixture = fixture("#!/bin/sh\nprintf spawned > \"$1\"\n");
    let owner = fixture.owner();
    let marker = fixture.root.path().join("spawned");
    let index = fixture.root.path().join("index");
    let replacement = fixture.root.path().join("replacement");
    write_script(&replacement, "#!/bin/sh\nprintf spawned > \"$1\"\n");
    std::fs::rename(replacement, &fixture.git).unwrap();
    let failure = run_bound_local_git(&owner.git, &[marker.as_os_str().to_owned()], &index)
        .err()
        .unwrap();
    assert_eq!(failure.phase(), GitChildFailurePhaseV1::BeforeSpawn);
    assert!(!marker.exists());
    for script in [
        "#!/bin/sh\nprintf '%65537d' 0\n",
        "#!/bin/sh\nprintf '\\377'\n",
    ] {
        let other = self::fixture(script);
        let owner = other.owner();
        let failure = run_git_child(
            &owner.git,
            None,
            None,
            &[],
            Some(&other.root.path().join("index")),
            Duration::from_millis(400),
        )
        .err()
        .unwrap();
        assert_eq!(failure.phase(), GitChildFailurePhaseV1::AfterSpawn);
    }
}

#[cfg(target_os = "macos")]
#[test]
fn bound_material_missing_fingerprint_key_never_creates_state() {
    let fixture = fixture("#!/bin/sh\nexit 0\n");
    let mut owner = fixture.owner();
    owner.binding_key_path = fixture
        .root
        .path()
        .join(".polaris/git-credential-bindings-v1/fingerprint.key");
    let binding = GitCredentialBindingV1 {
        namespace: NAMESPACE.into(),
        credential_item_id: "1".repeat(64),
        credential_item_version: "2".repeat(64),
        remote: CanonicalHttpsRemoteV1::parse("https://owned.invalid:443/owned").unwrap(),
        account: "runner-user".into(),
        authentication_type: AuthenticationTypeV1::Default,
        security_domain: OptionalAttributeHashV1::Absent,
        access_group: OptionalAttributeHashV1::Absent,
        synchronizable: false,
        generation: 1,
        git: owner.git.clone(),
        askpass: owner.askpass.clone(),
    };
    let mut before: Vec<_> = std::fs::read_dir(fixture.root.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    before.sort();
    assert!(owner.read_bound_material(&binding).is_err());
    assert!(owner.revalidate_binding(&binding).is_err());
    let mut after: Vec<_> = std::fs::read_dir(fixture.root.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    after.sort();
    assert_eq!(before, after);
    assert!(!fixture.root.path().join(".polaris").exists());
}

#[test]
fn typed_runner_terminates_closed_output_descendants_on_every_exit_kind() {
    // Keep descendants alive beyond the runner's normal deadline so a PID can
    // disappear only through cleanup, even when process startup is slow.
    let descendant_lifetime = CHILD_TIMEOUT.as_secs() * 2;
    for (terminal, expected_code) in [
        ("exit 0", Some(0)),
        ("exit 7", Some(7)),
        ("kill -TERM $$", None),
    ] {
        let script = format!(
            "#!/bin/sh\n/bin/cat <&3 >/dev/null\n/bin/cat <&4 >/dev/null\n/bin/sleep {descendant_lifetime} </dev/null >/dev/null 2>&1 &\nprintf '%s' \"$!\" > \"$1\"\n{terminal}\n"
        );
        let fixture = fixture(&script);
        let owner = fixture.owner();
        let pid_file = fixture.root.path().join("closed-output-descendant.pid");
        let material = material();
        let started = Instant::now();
        let result = run_git_child(
            &owner.git,
            Some(&owner.askpass),
            Some(&material),
            &[pid_file.as_os_str().to_owned()],
            None,
            CHILD_TIMEOUT,
        );
        let outcome = result
            .as_ref()
            .map(GitChildOutputV1::exit_code)
            .map_err(|failure| failure.phase());
        let pid = std::fs::read_to_string(&pid_file)
            .unwrap_or_else(|error| {
                panic!(
                    "{terminal}: fixture must record descendant PID: {error}; \
                     runner outcome {outcome:?}, elapsed {:?}",
                    started.elapsed()
                )
            })
            .parse::<i32>()
            .expect("numeric descendant PID");
        assert_pid_gone(pid);
        match expected_code {
            Some(code) => assert_eq!(result.unwrap().exit_code(), Some(code)),
            None => assert_eq!(
                result.err().unwrap().phase(),
                GitChildFailurePhaseV1::AfterSpawn
            ),
        }
    }
}

#[test]
fn authenticated_index_entry_preserves_binding_and_rejects_ambiguous_index_before_spawn() {
    let fixture = fixture(
        "#!/bin/sh\n/bin/cat <&3 >/dev/null\n/bin/cat <&4 >/dev/null\ntest \"$GIT_INDEX_FILE\" = \"$1\" || exit 9\nprintf spawned > \"$2\"\nprintf pinned-index\n",
    );
    let owner = fixture.owner();
    assert_eq!(owner.git_identity(), &owner.git);
    let index = fixture.root.path().join("index");
    let marker = fixture.root.path().join("spawned");
    let arguments = [index.as_os_str().to_owned(), marker.as_os_str().to_owned()];
    let output = owner
        .run_bound_git_output_at_index(&arguments, &material(), &index)
        .unwrap();
    assert!(output.success());
    assert_eq!(output.stdout(), b"pinned-index");
    assert_eq!(std::fs::read(&marker).unwrap(), b"spawned");
    std::fs::remove_file(&marker).unwrap();
    let failure = owner
        .run_bound_git_output_at_index(
            &arguments,
            &material(),
            &fixture.root.path().join("./index"),
        )
        .err()
        .unwrap();
    assert_eq!(failure.phase(), GitChildFailurePhaseV1::BeforeSpawn);
    assert!(!marker.exists());
    assert!(!index.exists());
    assert!(!owner.binding_key_path.exists());
}

#[test]
fn authenticated_runner_uses_fixed_home_and_clears_parent_netrc_environment() {
    struct RestoreEnvironment {
        home: Option<OsString>,
        netrc: Option<OsString>,
    }
    impl Drop for RestoreEnvironment {
        fn drop(&mut self) {
            for (name, previous) in [("HOME", self.home.take()), ("NETRC", self.netrc.take())] {
                unsafe {
                    match previous {
                        Some(value) => std::env::set_var(name, value),
                        None => std::env::remove_var(name),
                    }
                }
            }
        }
    }
    let _lock = ENVIRONMENT_LOCK.lock().unwrap();
    let fixture = fixture(
        "#!/bin/sh\n/bin/cat <&3 >/dev/null\n/bin/cat <&4 >/dev/null\ntest \"$HOME\" = / || exit 10\ntest -z \"${NETRC+x}\" || exit 11\nexit 0\n",
    );
    let netrc = fixture.root.path().join(".netrc");
    std::fs::write(&netrc, b"synthetic unrelated credential fixture").unwrap();
    let _restore = RestoreEnvironment {
        home: std::env::var_os("HOME"),
        netrc: std::env::var_os("NETRC"),
    };
    unsafe {
        std::env::set_var("HOME", fixture.root.path());
        std::env::set_var("NETRC", &netrc);
    }
    let output = fixture
        .owner()
        .run_bound_git_output(&[], &material())
        .unwrap();
    assert!(output.success());
    assert!(output.stdout().is_empty());
    assert_eq!(
        std::fs::read(&netrc).unwrap(),
        b"synthetic unrelated credential fixture"
    );
}

#[test]
fn network_home_guard_rejects_every_netrc_entry_without_reading_it() {
    let root = tempfile::tempdir().unwrap();
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(root.path())
        .unwrap();
    require_netrc_absent(&directory).unwrap();
    let netrc = root.path().join(".netrc");
    std::os::unix::fs::symlink("missing-unrelated-target", &netrc).unwrap();
    assert!(matches!(
        require_netrc_absent(&directory),
        Err(GitCredentialErrorV1::Protection)
    ));
    std::fs::remove_file(&netrc).unwrap();
    std::fs::write(&netrc, b"synthetic private bytes").unwrap();
    std::fs::set_permissions(&netrc, std::fs::Permissions::from_mode(0o000)).unwrap();
    assert!(matches!(
        require_netrc_absent(&directory),
        Err(GitCredentialErrorV1::Protection)
    ));
    assert_eq!(std::fs::metadata(&netrc).unwrap().mode() & 0o777, 0);
    std::fs::set_permissions(&netrc, std::fs::Permissions::from_mode(0o600)).unwrap();
}
