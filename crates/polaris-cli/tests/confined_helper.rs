//! Integration test that launches the real `polaris` binary as a helper
//! under a real confinement profile.
//!
//! Without this, M2's central feature nearly shipped green while broken.
//! Every test at the tool layer uses a `/bin/sh` stand-in helper, and the
//! stand-in can launch under any profile. The real helper, on the other
//! hand, carries the Rust runtime. Under macOS's default
//! (`workspace-write`) and `read-only`, the profile lacked `(allow
//! sysctl-read)`, so `sysconf(_SC_PAGESIZE)` was refused right before
//! setting up the guard page, and the process SIGABRTed on startup. That
//! meant even `write` / `edit` *inside* the workspace failed, and that
//! abort reached the model in the exact same shape as "denied by policy"
//! (nonzero exit + stderr). Tests using the stand-in helper can't tell the
//! two apart.
//!
//! What this verifies is three things.
//!
//! 1. A change inside the writable root really lands (the file exists,
//!    and its contents are exactly `content` — meaning the real helper
//!    parsed the JSON)
//! 2. A change outside the root is really refused, and refused as
//!    "denied by policy", not "the helper failed to launch"
//! 3. An `edit` whose marker doesn't match comes back as a request
//!    problem, not a denial
//!
//! Why this location: the only way for a test to obtain the real binary's
//! path is `env!("CARGO_BIN_EXE_polaris")`, which only works in
//! integration tests of the crate that declares the binary
//! (`polaris-cli`). From `polaris-tools`'s unit tests there's no way to
//! reach it other than guessing the build artifact's path, and that guess
//! silently breaks with profile or `--target` choices.

use std::path::{Path, PathBuf};

use polaris_sandbox::{SandboxMode, SandboxPolicy};
use polaris_tools::ToolError;

/// The text that appears when the OS refuses the operation. macOS's
/// Seatbelt returns `EPERM`, Linux's landlock returns `EACCES`, and
/// Rust's `io::Error` displays each as `(os error 1)` / `(os error 13)`
/// respectively. When the child is `/bin/sh`, the shell's own wording
/// (`Operation not permitted`) appears.
const OS_REFUSAL: &[&str] = &[
    "Operation not permitted",
    "Permission denied",
    "(os error 1)",
    "(os error 13)",
];

/// The text that appears when the helper itself fails to launch. Used to
/// confirm this isn't being mistaken for a denial.
const STARTUP_FAILURE: &[&str] = &["panicked", "fatal runtime error", "guard page"];

/// Obtains the helper's path through the same path production uses.
/// `main.rs` goes through `staged_helper`, so the test calls its
/// underlying version (the one whose executable can be swapped) against
/// the real binary.
///
/// Passes the real binary in only *after placing it inside the writable
/// root*. This is exactly production's layout (launching
/// `<project>/target/debug/polaris` with `<project>` as the root). If
/// `env!("CARGO_BIN_EXE_polaris")` were passed directly, the root is a
/// fresh temp directory every time, so the binary would always already be
/// outside the root, and `staged_helper_from` would return early — never
/// running the copy, the permission setup, or the verification of the
/// staging location even once. That would make the claim "the staging
/// location is outside the root" structurally always true, verifying
/// nothing.
fn staged_real_binary(policy: &SandboxPolicy, state_dir: &Path) -> PathBuf {
    let inside_the_root = policy.writable_roots()[0].join("polaris");
    std::fs::copy(env!("CARGO_BIN_EXE_polaris"), &inside_the_root)
        .expect("could not place the real binary inside the writable root");

    polaris_sandbox::stage::staged_helper_from(policy, state_dir, &inside_the_root)
        .expect("could not prepare the helper")
}

fn workspace_write(root: &Path) -> SandboxPolicy {
    SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.to_path_buf()]).expect("policy")
}

#[test]
fn a_write_inside_the_root_lands_through_the_real_binary_under_a_real_profile() {
    let root = tempfile::tempdir().expect("temp directory");
    let state = tempfile::tempdir().expect("temp directory");
    let policy = workspace_write(root.path());
    let helper = staged_real_binary(&policy, state.path());

    // This checks that `staged_helper_from` actually made a copy. Since
    // the binary we passed in is inside the writable root, this claim
    // would be false if staging didn't work (leaving anyone who can write
    // to the workspace able to replace the helper used for the next
    // mutation).
    assert!(
        !helper.starts_with(policy.writable_roots()[0].as_path()),
        "helper is inside the writable root: {}",
        helper.display()
    );
    assert!(
        helper.exists(),
        "the helper that should have been staged is missing: {}",
        helper.display()
    );

    let target = policy.writable_roots()[0].join("in.txt");
    let msg = polaris_tools::write::write(&policy, &helper, &target, "body text").expect(
        "write inside the workspace failed (the real helper may not be able to launch under confinement)",
    );

    // Checks that the content is exactly `content`. What the stand-in
    // helper's tests check is "the serialized result that rode on
    // stdin", which is not this. This only matches when the real helper
    // parsed the JSON as a `Mutation` and `helper::apply` actually ran.
    assert_eq!(
        std::fs::read_to_string(&target).expect("file is missing"),
        "body text",
        "the content written is not exactly `content`"
    );
    assert!(!msg.trim().is_empty(), "result description is empty");
}

#[test]
fn a_write_outside_the_root_is_refused_by_the_policy_not_by_a_helper_that_could_not_start() {
    let root = tempfile::tempdir().expect("temp directory");
    let outside = tempfile::tempdir().expect("temp directory");
    let state = tempfile::tempdir().expect("temp directory");
    let policy = workspace_write(root.path());
    let helper = staged_real_binary(&policy, state.path());

    // Put a control run inside this test. A test that only looks at
    // failure outside the root can't distinguish "denied by policy" from
    // "the helper never ran at all" — the three signs of "it's an Err",
    // "the file is missing", and "the message names a path and a policy"
    // are all fully satisfied even by a helper that failed to launch (in
    // fact, the real binary behaved exactly that way before this fix). We
    // first confirm that a write inside the root succeeds with this same
    // policy and this same helper, so this test itself guarantees the
    // failure that follows isn't a launch failure.
    let control = policy.writable_roots()[0].join("control.txt");
    polaris_tools::write::write(&policy, &helper, &control, "control")
        .expect("control run failed: this helper cannot launch under this profile");
    assert_eq!(
        std::fs::read_to_string(&control).expect("control file is missing"),
        "control"
    );

    let target = outside
        .path()
        .canonicalize()
        .expect("canonicalize")
        .join("pwned.txt");
    let err = polaris_tools::write::write(&policy, &helper, &target, "body text")
        .expect_err("write outside the root succeeded");

    assert!(!target.exists(), "file was created: {}", target.display());

    let ToolError::WriteDenied { detail, .. } = &err else {
        panic!("not returned as denied by policy: {err:?}");
    };
    assert!(
        OS_REFUSAL.iter().any(|s| detail.contains(s)),
        "the child's output has no OS refusal (something that isn't a denial is being named a denial): {detail}"
    );
    assert!(
        !STARTUP_FAILURE.iter().any(|s| detail.contains(s)),
        "the helper failed to launch, and that is being reported as a denial: {detail}"
    );

    // The contents the spec requires the denial message to carry (path,
    // policy, writable root).
    let msg = err.to_string();
    assert!(
        msg.contains(&target.display().to_string()),
        "path is missing: {msg}"
    );
    assert!(msg.contains("workspace-write"), "policy is missing: {msg}");
    assert!(
        msg.contains(&policy.writable_roots()[0].display().to_string()),
        "writable root is missing: {msg}"
    );
}

#[test]
fn an_edit_whose_marker_is_absent_is_a_request_problem_not_a_policy_denial() {
    // Tries to replace, inside a file that's *inside* the root, using a
    // marker that doesn't exist. This is a failure that has nothing to do
    // with policy; what the model should do is pick a different marker.
    // Returning this as a denial sends the model off hunting for a
    // permissions problem and throws away a round trip.
    let root = tempfile::tempdir().expect("temp directory");
    let state = tempfile::tempdir().expect("temp directory");
    let policy = workspace_write(root.path());
    let helper = staged_real_binary(&policy, state.path());

    let target = policy.writable_roots()[0].join("f.txt");
    std::fs::write(&target, "original body text").expect("cannot write");

    let err = polaris_tools::edit::edit(
        &policy,
        &helper,
        &target,
        "a marker that does not exist",
        "new",
    )
    .expect_err("replacement succeeded despite a missing marker");

    let ToolError::MutationFailed { detail, .. } = &err else {
        panic!("returned as a different kind instead of a request problem: {err:?}");
    };
    assert!(
        detail.contains("not found"),
        "the reason the child reported did not come through: {detail}"
    );

    let msg = err.to_string();
    assert!(
        !msg.contains("workspace-write"),
        "naming the policy despite this being a request problem (the model would start hunting for a permissions problem): {msg}"
    );
    assert_eq!(
        std::fs::read_to_string(&target).expect("cannot read"),
        "original body text",
        "content changed despite the failure"
    );
}
