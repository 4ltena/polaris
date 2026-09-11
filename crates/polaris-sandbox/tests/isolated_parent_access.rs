#![cfg(target_os = "macos")]
//! Explicit macOS runtime test. No arbitrary PID or real parent environment is
//! inspected: probes target only their disposable env_clear testexe parent or
//! grandparent. The forked probe inherits its validated target, never a new PID.
//! task_for_pid denial alone does not distinguish Seatbelt from OS task policy.

use polaris_sandbox::{
    ControlledEnd, SandboxMode, SandboxPolicy, run_confined, run_confined_controlled,
};
use std::collections::BTreeMap;
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const SENTINEL: &str = "DUMMY_PARENT_ENV_729461";
const FIXTURE: &str = "isolated_parent_access_fixture";

// Helper output contains only fixed markers and syscall return codes, never the
// sysctl buffer. The sentinel is compiled in, not supplied in argv, so a positive
// match must come from the parsed environment region rather than the arguments.
const HELPER: &str = r#"
#include <errno.h>
#include <mach/mach.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/sysctl.h>
#include <sys/wait.h>
#include <unistd.h>

static int parent_has_sentinel(const unsigned char *buf, size_t size) {
    if (size < sizeof(int)) return -1;
    int argc = 0;
    memcpy(&argc, buf, sizeof(argc));
    if (argc < 1 || argc > 128) return -1;
    const unsigned char *p = buf + sizeof(argc), *end = buf + size;
    const unsigned char *nul = memchr(p, 0, (size_t)(end - p));
    if (!nul) return -1;
    p = nul + 1; /* executable path */
    while (p < end && *p == 0) ++p;
    for (int i = 0; i < argc; ++i) {
        nul = memchr(p, 0, (size_t)(end - p));
        if (!nul) return -1;
        p = nul + 1;
    }
    const char expected[] = "POLARIS_PARENT_ACCESS_SENTINEL=DUMMY_PARENT_ENV_729461";
    while (p < end) {
        nul = memchr(p, 0, (size_t)(end - p));
        if (!nul) return -1;
        if ((size_t)(nul - p) == sizeof(expected) - 1 &&
            memcmp(p, expected, sizeof(expected) - 1) == 0) return 1;
        p = nul + 1;
    }
    return 0;
}

static int probe(pid_t parent, int direct) {
    /* Positive Mach operation in every context, with no other process access. */
    struct task_basic_info info;
    mach_msg_type_number_t count = TASK_BASIC_INFO_COUNT;
    kern_return_t self_result = task_info(mach_task_self(), TASK_BASIC_INFO,
                                         (task_info_t)&info, &count);
    if (self_result != KERN_SUCCESS) return 24;
    puts("mach-self-ok");

    unsigned char *buf = calloc(1, 1024 * 1024);
    if (!buf) return 25;
    /* Exercise the self exception without printing any environment bytes. */
    size_t self_size = 1024 * 1024;
    int self_mib[3] = { CTL_KERN, KERN_PROCARGS2, getpid() };
    if (sysctl(self_mib, 3, buf, &self_size, NULL, 0) != 0) {
        free(buf);
        return 30;
    }
    puts("sysctl-self-ok");
    memset(buf, 0, 1024 * 1024);
    size_t size = 1024 * 1024;
    int mib[3] = { CTL_KERN, KERN_PROCARGS2, parent };
    int rc = sysctl(mib, 3, buf, &size, NULL, 0);
    int saved_errno = errno;
    int found = rc == 0 ? parent_has_sentinel(buf, size) : -1;
    free(buf);
    if (direct) {
        if (rc != 0 || found != 1) {
            fprintf(stderr, "direct-sysctl-control-failed rc=%d errno=%d match=%d\n",
                    rc, saved_errno, found);
            return 26;
        }
        /* The same task_for_pid primitive works on the helper's own task. */
        mach_port_t own = MACH_PORT_NULL;
        if (task_for_pid(mach_task_self(), getpid(), &own) != KERN_SUCCESS) return 27;
        mach_port_deallocate(mach_task_self(), own);
        puts("task-for-self-ok");
        puts("direct-parent-env-match");
        return 0;
    }
    int failed = 0;
    if (rc != -1 || (saved_errno != EPERM && saved_errno != EACCES)) {
        fprintf(stderr, "isolated-sysctl-not-denied rc=%d errno=%d match=%d\n",
                rc, saved_errno, found);
        failed = 28;
    } else {
        printf("isolated-parent-sysctl-denied errno=%d\n", saved_errno);
    }
    mach_port_t task = MACH_PORT_NULL;
    kern_return_t result = task_for_pid(mach_task_self(), parent, &task);
    if (result == KERN_SUCCESS || task != MACH_PORT_NULL) {
        if (task != MACH_PORT_NULL) mach_port_deallocate(mach_task_self(), task);
        fprintf(stderr, "isolated-parent-task-not-denied kr=%d\n", result);
        failed = 29;
    } else {
        printf("isolated-parent-task-denied kr=%d\n", result);
    }
    return failed;
}
int main(int argc, char **argv) {
    alarm(5);
    if (argc != 3) return 20;
    char *tail = NULL;
    long requested = strtol(argv[2], &tail, 10);
    pid_t parent = getppid();
    if (!tail || *tail || requested <= 1 || requested != (long)parent) return 21;
    int direct = strcmp(argv[1], "direct") == 0;
    if (!direct && strcmp(argv[1], "isolated") != 0) return 22;
    if (getenv("POLARIS_PARENT_ACCESS_SENTINEL") != NULL) return 23;

    int parent_result = probe(parent, direct);
    /* Flush before fork so child output cannot duplicate positive markers. */
    if (fflush(NULL) != 0) return 31;
    pid_t intermediary = getpid();
    pid_t child = fork();
    if (child < 0) return 32;
    if (child == 0) {
        alarm(5);
        /* parent was validated against getppid before fork. Our immediate
           parent remains alive in waitpid; the Rust fixture is our grandparent. */
        if (getppid() != intermediary) _exit(33);
        int result = probe(parent, direct);
        if (result == 0) {
            puts(direct ? "direct-grandparent-env-match" :
                          "isolated-grandparent-sysctl-and-task-denied");
        }
        if (fflush(NULL) != 0) _exit(34);
        _exit(result);
    }
    int status = 0;
    pid_t waited;
    do { waited = waitpid(child, &status, 0); } while (waited < 0 && errno == EINTR);
    if (waited != child || !WIFEXITED(status)) return 35;
    if (getppid() != parent) return 36;
    if (parent_result != 0) return parent_result;
    return WEXITSTATUS(status);
}

"#;

/// Bound disposable command lifetime and output without a pipe-filling deadlock.
/// Timeout cleanup targets only the unreaped child owned by this call.
fn bounded_output(command: &mut Command, timeout: Duration) -> Output {
    let mut stdout = tempfile::tempfile().unwrap();
    let mut stderr = tempfile::tempfile().unwrap();
    let mut child = command
        .stdin(Stdio::null())
        .stdout(stdout.try_clone().unwrap())
        .stderr(stderr.try_clone().unwrap())
        .spawn()
        .expect("explicit fixture command must be available");
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().expect("stop only the owned timed-out fixture");
            child.wait().unwrap();
            panic!("fixture command exceeded its deadline; no fallback attempted");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    fn read_output(file: &mut std::fs::File) -> Vec<u8> {
        assert!(
            file.metadata().unwrap().len() <= 64 * 1024,
            "fixture output exceeded bound"
        );
        file.rewind().unwrap();
        let mut bytes = Vec::new();
        file.take(64 * 1024 + 1).read_to_end(&mut bytes).unwrap();
        assert!(bytes.len() <= 64 * 1024);
        bytes
    }
    Output {
        status,
        stdout: read_output(&mut stdout),
        stderr: read_output(&mut stderr),
    }
}

fn developer_path(root: &Path, args: &[&str]) -> PathBuf {
    let out = bounded_output(
        Command::new("/usr/bin/xcrun")
            .args(args)
            .env_clear()
            .env("HOME", root)
            .env("TMPDIR", root)
            .env("PATH", "/usr/bin:/bin"),
        Duration::from_secs(10),
    );
    assert!(
        out.status.success(),
        "Apple developer tools unavailable: {}; no fallback",
        String::from_utf8_lossy(&out.stderr)
    );
    PathBuf::from(String::from_utf8(out.stdout).unwrap().trim())
        .canonicalize()
        .unwrap()
}

fn policy(root: &Path, mode: SandboxMode, runtimes: &[PathBuf]) -> SandboxPolicy {
    let home = root.join("home");
    let tmp = root.join("tmp");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&tmp).unwrap();
    SandboxPolicy::isolated(mode, root, runtimes)
        .unwrap()
        .with_isolated_environment(&home, &tmp)
        .unwrap()
}

#[test]
#[ignore = "explicit macOS developer-tools and Seatbelt verification; unavailable controls fail, never skip"]
fn isolated_children_cannot_inspect_synthetic_parent() {
    let root = tempfile::tempdir().unwrap();
    let out = bounded_output(
        Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", FIXTURE, "--nocapture"])
            .env_clear()
            .env("POLARIS_PARENT_ACCESS_FIXTURE", "1")
            .env("POLARIS_PARENT_ACCESS_SENTINEL", SENTINEL)
            .env("POLARIS_PARENT_ACCESS_ROOT", root.path())
            .env("HOME", root.path())
            .env("TMPDIR", root.path()),
        Duration::from_secs(90),
    );
    assert!(
        out.status.success(),
        "synthetic fixture failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("parent-access-verified normal=3 controlled=3 controls=8 parent-unchanged")
    );
    assert!(
        stdout.contains("ancestor-access-verified normal=3 controlled=3 controls=8 self-positive")
    );
    assert!(!stdout.contains(SENTINEL));
    println!("{stdout}");
}

#[test]
#[ignore = "disposable synthetic parent, entered only by the explicit outer test"]
fn isolated_parent_access_fixture() {
    assert!(
        std::env::var("POLARIS_PARENT_ACCESS_FIXTURE").as_deref() == Ok("1"),
        "fixture must be launched with env_clear by its owner"
    );
    let root = PathBuf::from(std::env::var_os("POLARIS_PARENT_ACCESS_ROOT").unwrap());
    let expected: BTreeMap<_, _> = [
        ("POLARIS_PARENT_ACCESS_FIXTURE", "1".into()),
        ("POLARIS_PARENT_ACCESS_SENTINEL", SENTINEL.into()),
        ("POLARIS_PARENT_ACCESS_ROOT", root.as_os_str().to_owned()),
        ("HOME", root.as_os_str().to_owned()),
        ("TMPDIR", root.as_os_str().to_owned()),
    ]
    .into_iter()
    .map(|(k, v)| (std::ffi::OsString::from(k), v))
    .collect();
    assert!(
        std::env::vars_os().collect::<BTreeMap<_, _>>() == expected,
        "only synthetic environment entries are allowed"
    );
    let parent_id = std::process::id();
    let clang = developer_path(&root, &["--find", "clang"]);
    let sdk = developer_path(&root, &["--show-sdk-path"]);
    let toolchain = clang.parent().unwrap().parent().unwrap().parent().unwrap();
    let mut runtimes: Vec<PathBuf> = ["/System", "/usr/lib", "/usr/share", "/bin", "/usr/bin"]
        .into_iter()
        .map(PathBuf::from)
        .collect();
    runtimes.extend([toolchain.to_path_buf(), sdk.clone()]);
    std::fs::write(root.join("probe.c"), HELPER).unwrap();
    let started = Instant::now();
    let build = run_confined_controlled(
        &policy(&root, SandboxMode::WorkspaceWrite, &runtimes),
        &clang,
        &[
            "-Wall".into(),
            "-Wextra".into(),
            "-Werror".into(),
            "-isysroot".into(),
            sdk.to_string_lossy().into_owned(),
            "probe.c".into(),
            "-o".into(),
            "probe".into(),
        ],
        None,
        || started.elapsed() > Duration::from_secs(30),
    )
    .expect("explicit trusted-runtime compilation must be enforced; no fallback");
    assert_eq!(build.end, ControlledEnd::Exited, "{build:?}");
    assert!(build.status.unwrap().success(), "{build:?}");
    assert!(
        build.pending.is_none() && build.problem.is_none(),
        "{build:?}"
    );
    let helper = root.join("probe").canonicalize().unwrap();

    let direct = || {
        let out = bounded_output(
            Command::new(&helper)
                .args(["direct", &parent_id.to_string()])
                .env_clear(),
            Duration::from_secs(8),
        );
        assert!(
            out.status.success(),
            "positive control failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let text = String::from_utf8(out.stdout).unwrap();
        for marker in [
            "mach-self-ok",
            "task-for-self-ok",
            "sysctl-self-ok",
            "direct-grandparent-env-match",
            "direct-parent-env-match",
        ] {
            assert!(text.contains(marker), "{text}");
        }
        assert!(!text.contains(SENTINEL));
    };
    direct();
    let args = vec!["isolated".into(), parent_id.to_string()];
    let mut failures = Vec::new();
    for mode in [
        SandboxMode::ReadOnly,
        SandboxMode::WorkspaceWrite,
        SandboxMode::FullAccess,
    ] {
        let policy = policy(&root, mode, &runtimes);
        let out = run_confined(&policy, &helper, &args, None)
            .expect("normal isolation must apply; no fallback");
        if out.status != 0 || !denials_reported(&out.stdout, &out.stderr) {
            failures.push(format!("normal {mode:?}: {out:?}"));
        }
        println!(
            "normal {mode:?}: status={} stdout={} stderr={}",
            out.status,
            out.stdout.trim().replace('\n', "; "),
            out.stderr.trim().replace('\n', "; ")
        );
        direct();
        let started = Instant::now();
        let out = run_confined_controlled(&policy, &helper, &args, None, || {
            started.elapsed() > Duration::from_secs(8)
        })
        .expect("controlled isolation must apply; no fallback");
        assert_eq!(out.end, ControlledEnd::Exited, "{out:?}");
        assert!(
            out.pending.is_none() && out.problem.is_none() && out.stdout_eof && out.stderr_eof,
            "{out:?}"
        );
        assert!(!out.stdout_truncated && !out.stderr_truncated);
        if !out.status.unwrap().success() || !denials_reported(&out.stdout, &out.stderr) {
            failures.push(format!("controlled {mode:?}: {out:?}"));
        }
        println!(
            "controlled {mode:?}: status={:?} stdout={} stderr={}",
            out.status,
            out.stdout.trim().replace('\n', "; "),
            out.stderr.trim().replace('\n', "; ")
        );
        direct();
        assert_eq!(std::process::id(), parent_id);
        assert!(
            std::env::vars_os().collect::<BTreeMap<_, _>>() == expected,
            "synthetic parent environment changed"
        );
    }
    direct();
    println!("parent-access-observed normal=3 controlled=3 controls=8 parent-unchanged");
    assert!(
        failures.is_empty(),
        "parent protection regression:\n{}",
        failures.join("\n")
    );
    println!("parent-access-verified normal=3 controlled=3 controls=8 parent-unchanged");
    println!("ancestor-access-verified normal=3 controlled=3 controls=8 self-positive");
}

fn denials_reported(stdout: &str, stderr: &str) -> bool {
    assert!(!stdout.contains(SENTINEL) && !stderr.contains(SENTINEL));
    [
        "mach-self-ok",
        "isolated-parent-sysctl-denied errno=",
        "isolated-parent-task-denied kr=",
        "sysctl-self-ok",
        "isolated-grandparent-sysctl-and-task-denied",
    ]
    .iter()
    .all(|marker| stdout.contains(marker))
}
