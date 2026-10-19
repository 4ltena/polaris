//! Controlled Git transport, ancestry and remote-change regression tests.
use super::*;

#[cfg(unix)]
use std::collections::VecDeque;
#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::sync::{Arc, Mutex};

#[cfg(unix)]
use polaris_auth::git_credentials::{ExecutableIdentityV1, run_bound_local_git};

const REMOTE: &str = "https://transport-fixture.invalid:443/owned.git";
const SECRET: &str = "synthetic-transport-secret";

#[derive(Debug, Eq, PartialEq)]
#[cfg(unix)]
enum Event {
    Ancestor(String, String),
    Child(Vec<OsString>),
}

#[cfg(unix)]
enum Reply {
    Output { stdout: Vec<u8>, code: i32 },
    BeforeSpawn,
    AfterSpawn,
}

#[cfg(unix)]
struct FixtureRunner {
    _directory: tempfile::TempDir,
    root: PathBuf,
    marker: PathBuf,
    events: Arc<Mutex<Vec<Event>>>,
    replies: VecDeque<Reply>,
    ancestry: Result<(), AncestorFailureV1>,
}

#[cfg(unix)]
impl NetworkChildV1 for FixtureRunner {
    fn run(&mut self, args: &[OsString]) -> Result<GitChildOutputV1, NetworkFailureV1> {
        self.events
            .lock()
            .unwrap()
            .push(Event::Child(args.to_vec()));
        let reply = self
            .replies
            .pop_front()
            .expect("fixture reply for every child");
        let git = self.root.join("bound-git");
        let marker = shell_quote(self.marker.to_str().unwrap());
        let mut body = format!("#!/bin/sh\nprintf '%s\\n' invoked >> {marker}\n");
        match &reply {
            Reply::Output { stdout, code } => {
                let encoded = stdout
                    .iter()
                    .map(|byte| format!("\\0{byte:03o}"))
                    .collect::<String>();
                body.push_str(&format!("printf '%b' '{encoded}'\n"));
                body.push_str(&format!("printf '%s' '{SECRET}' >&2\nexit {code}\n"));
            }
            Reply::BeforeSpawn => body.push_str("exit 0\n"),
            Reply::AfterSpawn => body.push_str("kill -TERM $$\n"),
        }
        if git.exists() {
            fs::remove_file(&git).unwrap();
        }
        fs::write(&git, body).unwrap();
        fs::set_permissions(&git, fs::Permissions::from_mode(0o500)).unwrap();
        let bound = ExecutableIdentityV1::inspect(&git).unwrap();
        if matches!(reply, Reply::BeforeSpawn) {
            fs::remove_file(&git).unwrap();
        }
        run_bound_local_git(&bound, args, &self.root.join("pinned-index"))
            .map_err(NetworkFailureV1::from)
    }

    fn require_ancestor(
        &mut self,
        expected: &SyncGitObjectIdV1,
        intended: &SyncGitObjectIdV1,
    ) -> Result<(), AncestorFailureV1> {
        self.events.lock().unwrap().push(Event::Ancestor(
            expected.as_str().to_owned(),
            intended.as_str().to_owned(),
        ));
        self.ancestry
    }
}

#[cfg(unix)]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(unix)]
type Events = Arc<Mutex<Vec<Event>>>;

#[cfg(unix)]
fn fixture(
    replies: impl IntoIterator<Item = Reply>,
    ancestry: Result<(), AncestorFailureV1>,
) -> (C10CasTransportV1<FixtureRunner>, Events) {
    let directory = tempfile::tempdir().unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let root = directory.path().canonicalize().unwrap();
    let marker = root.join("spawn-marker");
    let events = Arc::new(Mutex::new(Vec::new()));
    let runner = FixtureRunner {
        _directory: directory,
        root,
        marker,
        events: events.clone(),
        replies: replies.into_iter().collect(),
        ancestry,
    };
    (
        C10CasTransportV1::new(CanonicalHttpsRemoteV1::parse(REMOTE).unwrap(), runner),
        events,
    )
}

#[cfg(unix)]
fn output(stdout: impl AsRef<[u8]>, code: i32) -> Reply {
    Reply::Output {
        stdout: stdout.as_ref().to_vec(),
        code,
    }
}

#[cfg(unix)]
fn oid(byte: char, length: usize) -> SyncGitObjectIdV1 {
    SyncGitObjectIdV1::parse(byte.to_string().repeat(length)).unwrap()
}

#[cfg(unix)]
fn observation(oid: &SyncGitObjectIdV1) -> String {
    format!("{}\t{C10_REMOTE_REF}\n", oid.as_str())
}

#[cfg(unix)]
fn spawned(transport: &C10CasTransportV1<FixtureRunner>) -> usize {
    match fs::read(&transport.runner.marker) {
        Ok(bytes) => bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .count(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => panic!("read fixture spawn marker: {error}"),
    }
}

#[cfg(unix)]
fn child_args(events: &Events) -> Vec<Vec<OsString>> {
    events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match event {
            Event::Child(args) => Some(args.clone()),
            Event::Ancestor(_, _) => None,
        })
        .collect()
}

#[cfg(unix)]
fn args(values: &[&str]) -> Vec<OsString> {
    values.iter().map(|value| OsString::from(*value)).collect()
}

#[test]
#[cfg(unix)]
fn exact_observation_accepts_only_missing_or_one_sha1_or_sha256_ref() {
    let sha1 = oid('a', 40);
    let sha256 = oid('b', 64);
    let (mut transport, _) = fixture(
        [
            output("", 2),
            output(observation(&sha1), 0),
            output(observation(&sha256), 0),
        ],
        Ok(()),
    );
    assert_eq!(
        transport.observe_exact().unwrap(),
        ExpectedSyncRemoteOidV1::Missing
    );
    assert_eq!(
        transport.observe_exact().unwrap(),
        ExpectedSyncRemoteOidV1::Present { oid: sha1 }
    );
    assert_eq!(
        transport.observe_exact().unwrap(),
        ExpectedSyncRemoteOidV1::Present { oid: sha256 }
    );
    assert_eq!(spawned(&transport), 3);
}

#[test]
#[cfg(unix)]
fn observation_constructs_only_exact_ref_read_only_argv() {
    let (mut transport, events) = fixture([output("", 2)], Ok(()));
    transport.observe_exact().unwrap();
    assert_eq!(
        child_args(&events),
        vec![args(&[
            "ls-remote",
            "--refs",
            "--exit-code",
            "--",
            REMOTE,
            C10_REMOTE_REF,
        ])]
    );
    assert_eq!(events.lock().unwrap().len(), 1);
}

#[test]
#[cfg(unix)]
fn exact_observation_rejects_duplicate_peeled_foreign_and_malformed_lines() {
    let valid = observation(&oid('a', 40));
    let malformed = [
        format!("{valid}{valid}"),
        format!("{}\t{C10_REMOTE_REF}^{{}}\n", oid('a', 40).as_str()),
        format!("{}\trefs/heads/peer\n", oid('a', 40).as_str()),
        format!("{}\t{C10_REMOTE_REF}\nextra\n", oid('a', 40).as_str()),
        format!("{}\t{C10_REMOTE_REF}\n\n", oid('a', 40).as_str()),
        valid.trim_end_matches('\n').to_owned(),
        valid.replace('\n', "\r\n"),
        valid.replace('\t', " "),
        format!("{}\t{C10_REMOTE_REF}\n", "a".repeat(39)),
        format!("{}\t{C10_REMOTE_REF}\n", "a".repeat(41)),
        format!("{}\t{C10_REMOTE_REF}\n", "a".repeat(63)),
        format!("{}\t{C10_REMOTE_REF}\n", "a".repeat(65)),
        format!("{}\t{C10_REMOTE_REF}\n", "A".repeat(40)),
        format!("{}\t{C10_REMOTE_REF}\n", "g".repeat(40)),
        format!("{}\t{C10_REMOTE_REF}\t\n", oid('a', 40).as_str()),
        format!("{}\t{C10_REMOTE_REF}\0\n", oid('a', 40).as_str()),
        format!(" {valid}"),
    ];
    for line in malformed {
        let (mut transport, _) = fixture([output(line, 0)], Ok(()));
        assert_eq!(
            transport.observe_exact().unwrap_err(),
            TransportFailureV1::InvalidObservation
        );
        assert_eq!(spawned(&transport), 1);
    }
}

#[test]
#[cfg(unix)]
fn observation_rejects_empty_success_nonempty_missing_and_unexpected_exit() {
    let valid = observation(&oid('a', 40));
    for reply in [
        output("", 0),
        output(&valid, 2),
        output("", 1),
        output(&valid, 1),
        output("", 3),
    ] {
        let (mut transport, _) = fixture([reply], Ok(()));
        assert_eq!(
            transport.observe_exact().unwrap_err(),
            TransportFailureV1::InvalidObservation
        );
    }
}

#[test]
#[cfg(unix)]
fn observation_preserves_actual_before_and_after_spawn_failures() {
    for (reply, phase, expected_spawned) in [
        (Reply::BeforeSpawn, NetworkFailureV1::BeforeSpawn, 0),
        (Reply::AfterSpawn, NetworkFailureV1::AfterSpawn, 1),
    ] {
        let (mut transport, _) = fixture([reply], Ok(()));
        assert_eq!(
            transport.observe_exact().unwrap_err(),
            TransportFailureV1::Child(phase)
        );
        assert_eq!(spawned(&transport), expected_spawned);
    }
}

#[test]
#[cfg(unix)]
fn existing_cas_proves_ancestry_before_one_explicit_refspec_with_exact_lease() {
    let expected = oid('a', 40);
    let intended = oid('b', 40);
    let (mut transport, events) = fixture([output("transport accepted only", 0)], Ok(()));
    transport
        .send_once(
            &intended,
            &ExpectedSyncRemoteOidV1::Present {
                oid: expected.clone(),
            },
        )
        .unwrap();
    let lease = format!("--force-with-lease={C10_REMOTE_REF}:{}", expected.as_str());
    let refspec = format!("{}:{C10_REMOTE_REF}", intended.as_str());
    assert_eq!(
        *events.lock().unwrap(),
        vec![
            Event::Ancestor(expected.as_str().to_owned(), intended.as_str().to_owned()),
            Event::Child(args(&[
                "push",
                "--porcelain",
                "--no-verify",
                "--no-follow-tags",
                "--recurse-submodules=no",
                &lease,
                "--",
                REMOTE,
                &refspec,
            ])),
        ]
    );
    assert_eq!(spawned(&transport), 1);
}

#[test]
#[cfg(unix)]
fn missing_cas_uses_empty_exact_lease_without_ancestry_or_implicit_branch() {
    let intended = oid('c', 64);
    let (mut transport, events) = fixture([output("", 0)], Err(AncestorFailureV1::NotAncestor));
    transport
        .send_once(&intended, &ExpectedSyncRemoteOidV1::Missing)
        .unwrap();
    let lease = format!("--force-with-lease={C10_REMOTE_REF}:");
    let refspec = format!("{}:{C10_REMOTE_REF}", intended.as_str());
    assert_eq!(
        *events.lock().unwrap(),
        vec![Event::Child(args(&[
            "push",
            "--porcelain",
            "--no-verify",
            "--no-follow-tags",
            "--recurse-submodules=no",
            &lease,
            "--",
            REMOTE,
            &refspec,
        ]))]
    );
    assert_eq!(spawned(&transport), 1);
}

#[test]
#[cfg(unix)]
fn ancestry_refusal_or_unavailable_spawns_zero_push_and_consumes_attempt() {
    let expected = ExpectedSyncRemoteOidV1::Present { oid: oid('a', 40) };
    let intended = oid('b', 40);
    for (failure, category) in [
        (
            AncestorFailureV1::NotAncestor,
            TransportFailureV1::AncestryRejected,
        ),
        (
            AncestorFailureV1::Unavailable,
            TransportFailureV1::AncestryUnavailable,
        ),
    ] {
        let (mut transport, events) = fixture([], Err(failure));
        assert_eq!(transport.send_once(&intended, &expected), Err(category));
        assert_eq!(
            transport.send_once(&intended, &expected),
            Err(TransportFailureV1::AlreadyAttempted)
        );
        assert!(child_args(&events).is_empty());
        assert_eq!(events.lock().unwrap().len(), 1);
        assert_eq!(spawned(&transport), 0);
    }
}

#[test]
#[cfg(unix)]
fn object_format_mismatch_blocks_before_ancestry_or_child_and_consumes_attempt() {
    let (mut transport, events) = fixture([], Ok(()));
    let expected = ExpectedSyncRemoteOidV1::Present { oid: oid('a', 40) };
    let intended = oid('b', 64);
    assert_eq!(
        transport.send_once(&intended, &expected),
        Err(TransportFailureV1::ObjectFormatMismatch)
    );
    assert_eq!(
        transport.send_once(&intended, &expected),
        Err(TransportFailureV1::AlreadyAttempted)
    );
    assert!(events.lock().unwrap().is_empty());
    assert_eq!(spawned(&transport), 0);
}

#[test]
#[cfg(unix)]
fn every_send_result_allows_exactly_one_attempt_including_before_spawn_failure() {
    let intended = oid('a', 40);
    for (reply, first_result, expected_spawned) in [
        (output("", 0), Ok(()), 1),
        (
            output("lease rejected", 1),
            Err(TransportFailureV1::PushRejected),
            1,
        ),
        (
            Reply::BeforeSpawn,
            Err(TransportFailureV1::Child(NetworkFailureV1::BeforeSpawn)),
            0,
        ),
        (
            Reply::AfterSpawn,
            Err(TransportFailureV1::Child(NetworkFailureV1::AfterSpawn)),
            1,
        ),
    ] {
        let (mut transport, events) = fixture([reply], Ok(()));
        assert_eq!(
            transport
                .send_once(&intended, &ExpectedSyncRemoteOidV1::Missing)
                .map(|_| ()),
            first_result
        );
        assert_eq!(
            transport.send_once(&intended, &ExpectedSyncRemoteOidV1::Missing),
            Err(TransportFailureV1::AlreadyAttempted)
        );
        assert_eq!(child_args(&events).len(), 1);
        assert_eq!(spawned(&transport), expected_spawned);
    }
}

#[test]
#[cfg(unix)]
fn successful_push_does_not_observe_or_receipt_and_observation_never_resends() {
    let intended = oid('a', 40);
    let (mut transport, events) =
        fixture([output("", 0), output(observation(&intended), 0)], Ok(()));
    transport
        .send_once(&intended, &ExpectedSyncRemoteOidV1::Missing)
        .unwrap();
    assert_eq!(child_args(&events).len(), 1);
    assert_eq!(
        transport.observe_exact().unwrap(),
        ExpectedSyncRemoteOidV1::Present {
            oid: intended.clone()
        }
    );
    assert_eq!(
        transport.send_once(&intended, &ExpectedSyncRemoteOidV1::Missing),
        Err(TransportFailureV1::AlreadyAttempted)
    );
    let commands = child_args(&events);
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[0][0], "push");
    assert_eq!(commands[1][0], "ls-remote");
}

#[test]
#[cfg(unix)]
fn recovery_fetch_uses_same_management_operation_hash_and_isolated_namespace_without_push() {
    let operation = "d".repeat(32).parse::<ManagementOperationIdV1>().unwrap();
    let operation_hash = SyncDigestV1::digest(operation.as_hex().as_bytes()).as_hex();
    assert_eq!(operation.as_hex().len(), 32);
    assert_eq!(
        operation_hash,
        "fbbbb6de2aa74c3c9570d2d8db1de31eadb66113c96034a7adb21243754d7683"
    );
    let (mut transport, events) = fixture([output("", 0), output("", 0)], Ok(()));
    transport.fetch_exact_for_recovery(&operation).unwrap();
    transport.fetch_exact_for_recovery(&operation).unwrap();
    let refspec = format!("{C10_REMOTE_REF}:refs/polaris/recovery/{operation_hash}");
    let expected_args = args(&[
        "fetch",
        "--no-tags",
        "--no-recurse-submodules",
        "--no-write-fetch-head",
        "--no-auto-maintenance",
        "--",
        REMOTE,
        &refspec,
    ]);
    assert_eq!(
        *events.lock().unwrap(),
        vec![
            Event::Child(expected_args.clone()),
            Event::Child(expected_args)
        ],
    );
    assert_eq!(spawned(&transport), 2);
    assert!(!transport.attempted);
}

#[test]
#[cfg(unix)]
fn invalid_management_operation_strings_reject_before_any_recovery_child() {
    let (transport, events) = fixture([], Ok(()));
    for operation in [
        "a".repeat(31),
        "a".repeat(33),
        "a".repeat(64),
        "A".repeat(32),
        "g".repeat(32),
        format!("{}x", "a".repeat(31)),
        "nested/operation".into(),
        "..".into(),
    ] {
        assert!(operation.parse::<ManagementOperationIdV1>().is_err());
        assert!(events.lock().unwrap().is_empty());
        assert_eq!(spawned(&transport), 0);
    }
}

#[test]
#[cfg(unix)]
fn recovery_failure_preserves_phase_and_never_pushes_or_observes() {
    let operation = "d".repeat(32).parse::<ManagementOperationIdV1>().unwrap();
    for (reply, expected, expected_spawned) in [
        (
            output("fetch rejected", 1),
            TransportFailureV1::RecoveryFetchRejected,
            1,
        ),
        (
            Reply::BeforeSpawn,
            TransportFailureV1::Child(NetworkFailureV1::BeforeSpawn),
            0,
        ),
        (
            Reply::AfterSpawn,
            TransportFailureV1::Child(NetworkFailureV1::AfterSpawn),
            1,
        ),
    ] {
        let (mut transport, events) = fixture([reply], Ok(()));
        assert_eq!(
            transport.fetch_exact_for_recovery(&operation),
            Err(expected)
        );
        let calls = child_args(&events);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0][0], "fetch");
        assert_eq!(spawned(&transport), expected_spawned);
    }
}

#[test]
fn fixed_errors_never_format_locator_secret_or_child_diagnostics() {
    #[cfg(unix)]
    let (mut transport, _) = fixture([output(format!("{REMOTE}\n{SECRET}\n"), 0)], Ok(()));
    #[cfg(unix)]
    let observed_error = transport.observe_exact().unwrap_err();
    let errors = [
        #[cfg(unix)]
        observed_error,
        TransportFailureV1::AlreadyAttempted,
        TransportFailureV1::ObjectFormatMismatch,
        TransportFailureV1::AncestryRejected,
        TransportFailureV1::AncestryUnavailable,
        TransportFailureV1::Child(NetworkFailureV1::BeforeSpawn),
        TransportFailureV1::Child(NetworkFailureV1::AfterSpawn),
        TransportFailureV1::PushRejected,
        TransportFailureV1::RecoveryFetchRejected,
    ];
    for error in errors {
        let formatted = format!("{error:?} {error}");
        assert!(!formatted.contains(REMOTE));
        assert!(!formatted.contains("transport-fixture.invalid"));
        assert!(!formatted.contains(SECRET));
        assert!(std::error::Error::source(&error).is_none());
    }
}

#[test]
#[cfg(unix)]
fn observation_overflow_from_bounded_runner_is_after_spawn_uncertain() {
    let (mut transport, _) = fixture([output(vec![b'a'; 64 * 1024 + 1], 0)], Ok(()));
    assert_eq!(
        transport.observe_exact().unwrap_err(),
        TransportFailureV1::Child(NetworkFailureV1::AfterSpawn),
    );
    assert_eq!(spawned(&transport), 1);
}
