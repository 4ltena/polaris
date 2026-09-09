//! Native exec must not inherit the harness's nonstandard descriptors.

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) const FAILURE_ERRNO: i32 = 0x7066_6473;

/// Install before any other native pre-exec callback. Rust has already mapped
/// explicit stdio to 0..=2 when this runs. Its private exec-error channel must
/// remain OPEN on failure: mark it CLOEXEC like every other FD, never close it.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) fn install(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: the callback runs only in the single-threaded fork child. It does
    // not allocate, log, lock, inspect environments, or close inherited FDs.
    // No callback/handler may open non-CLOEXEC FDs after this boundary. See the
    // Darwin-specific cancellation argument below; this is not a general
    // callable-in-a-signal-handler API.
    unsafe {
        command.pre_exec(|| {
            mark_inherited().map_err(|_| std::io::Error::from_raw_os_error(FAILURE_ERRNO))
        });
    }
}

#[cfg(target_os = "linux")]
fn mark_inherited() -> Result<(), ()> {
    // Linux 5.11 added CLOEXEC; our Landlock floor is 5.19. A missing or denied
    // syscall fails closed, including FullAccess. No finite FD scan fallback.
    // SAFETY: a scalar-only syscall in the fork child, whose FD table is private.
    let result = unsafe {
        libc::syscall(
            libc::SYS_close_range,
            3u32,
            u32::MAX,
            libc::CLOSE_RANGE_CLOEXEC,
        )
    };
    if result == 0 { Ok(()) } else { Err(()) }
}

#[cfg(target_os = "macos")]
fn mark_inherited() -> Result<(), ()> {
    // XNU devfs_devfd_readdir walks current_proc()->p_fd.fd_nfiles, not rlimit.
    // VFS converts its legacy records for getdirentries64. F_SETFD does not
    // remove entries, and our directory stays open until enumeration is over.
    // https://github.com/apple-oss-distributions/xnu/blob/main/bsd/miscfs/devfs/devfs_fdesc_support.c
    // https://github.com/apple-oss-distributions/xnu/blob/main/bsd/vfs/vfs_syscalls.c
    //
    // Darwin syscall's cerror calls _pthread_exit_if_canceled only for EINTR
    // AND a kernel cancellation mark. thread_create_internal initializes a NEW
    // thread from thread_template; uthread_init inherits the signal mask, not
    // UT_CANCEL. Hence ordinary fork children have no inherited cancellation
    // mark, even if the parent's userspace pthread state was pending. No code
    // here marks cancellation or invokes user handlers. Such behavior in an
    // atfork/signal handler is outside this callback's contract.
    // cerror_nocancel and __error access errno/TSD without allocation or locks.
    // https://github.com/apple-oss-distributions/libpthread/blob/main/src/pthread_cancelable.c
    // https://github.com/apple-oss-distributions/xnu/blob/main/libsyscall/custom/errno.c
    // https://github.com/apple-oss-distributions/xnu/blob/main/osfmk/kern/thread.c
    // https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/kern_fork.c
    //
    // This uses Darwin's syscall ABI (SDK marks syscall unsupported), not a
    // portable POSIX guarantee. All enumeration/ABI errors prevent execution.
    // SAFETY: static NUL-terminated path, ordinary scalar open flags.
    let directory = unsafe {
        libc::open(
            c"/dev/fd".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if directory < 0 {
        return Err(());
    }
    let result = mark_directory(directory);
    // SAFETY: this is the one descriptor we opened, not Rust's error channel.
    let closed = unsafe { libc::close(directory) };
    if closed != 0 { Err(()) } else { result }
}

#[cfg(target_os = "macos")]
fn mark_directory(directory: libc::c_int) -> Result<(), ()> {
    let mut buffer = [0u8; 4096];
    let mut previous_position = None;
    let mut saw_directory = false;
    loop {
        let mut position: libc::off_t = 0;
        // SAFETY: matches XNU getdirentries64(fd, buf, user_size_t, off_t*).
        // libc declares syscall(c_int, ...), including the fixed first arg
        // required by the arm64 variadic ABI. A 4KiB count fits its int return.
        // XNU may put EOF flags at buffer's tail: parse ONLY returned bytes.
        let count = unsafe {
            libc::syscall(
                344 as libc::c_int, // SYS_getdirentries64 in Darwin's SDK
                directory,
                buffer.as_mut_ptr(),
                buffer.len(),
                &mut position as *mut libc::off_t,
            )
        };
        if count < 0 || count as usize > buffer.len() {
            return Err(());
        }
        if count == 0 {
            return if saw_directory { Ok(()) } else { Err(()) };
        }
        if previous_position.is_some_and(|last| position <= last) {
            return Err(());
        }
        previous_position = Some(position);
        visit_records(&buffer[..count as usize], |fd| {
            saw_directory |= fd == directory;
            if fd >= 3 {
                // SAFETY: fork-private FD table; only descriptor flags change.
                // EBADF is unexpected in this stable table, so also fails closed.
                let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
                if flags < 0
                    || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0
                {
                    return Err(());
                }
            }
            Ok(())
        })?;
    }
}

#[cfg(target_os = "macos")]
fn visit_records(mut bytes: &[u8], mut visit: impl FnMut(i32) -> Result<(), ()>) -> Result<(), ()> {
    const NAME: usize = std::mem::offset_of!(libc::dirent, d_name);
    const RECORD: usize = std::mem::offset_of!(libc::dirent, d_reclen);
    const LENGTH: usize = std::mem::offset_of!(libc::dirent, d_namlen);
    while !bytes.is_empty() {
        if bytes.len() <= NAME {
            return Err(());
        }
        let record = u16::from_ne_bytes([bytes[RECORD], bytes[RECORD + 1]]) as usize;
        let length = u16::from_ne_bytes([bytes[LENGTH], bytes[LENGTH + 1]]) as usize;
        if record <= NAME || record > bytes.len() || length >= record - NAME {
            return Err(());
        }
        let name = &bytes[NAME..NAME + length];
        if bytes[NAME + length] != 0 || name.is_empty() {
            return Err(());
        }
        if name != b"." && name != b".." {
            let mut fd = 0i32;
            for &digit in name {
                if !digit.is_ascii_digit() {
                    return Err(());
                }
                fd = fd
                    .checked_mul(10)
                    .and_then(|n| n.checked_add((digit - b'0') as i32))
                    .ok_or(())?;
            }
            visit(fd)?;
        }
        bytes = &bytes[record..];
    }
    Ok(())
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::process::{Command, Stdio};

    const SENTINEL: &str = "dummy-fd-boundary-sentinel";

    #[test]
    #[ignore = "fixture entered explicitly by descriptor tests"]
    fn descriptor_probe() {
        let mut input = String::new();
        std::io::stdin().read_to_string(&mut input).unwrap();
        for number in input.split_whitespace() {
            let fd: i32 = number.parse().unwrap();
            let mut bytes = [0u8; 64];
            let count = unsafe { libc::pread(fd, bytes.as_mut_ptr().cast(), bytes.len(), 0) };
            if count == -1 {
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::EBADF)
                );
                println!("PREAD_CLOSED_EBADF");
            } else {
                println!(
                    "PREAD_LEAK:{}",
                    std::str::from_utf8(&bytes[..count as usize]).unwrap()
                );
            }
        }
    }

    fn probe_args() -> Vec<String> {
        [
            "--exact",
            "child_fds::tests::descriptor_probe",
            "--ignored",
            "--nocapture",
        ]
        .map(str::to_owned)
        .to_vec()
    }

    #[test]
    fn native_children_do_not_inherit_open_file_descriptors() {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(SENTINEL.as_bytes()).unwrap();
        // F_DUPFD reserves an unused descriptor and deliberately omits CLOEXEC.
        let raw = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD, 128) };
        assert!(raw >= 128);
        let descriptor = unsafe { OwnedFd::from_raw_fd(raw) };
        assert_eq!(unsafe { libc::fcntl(raw, libc::F_GETFD) }, 0);
        let executable = std::env::current_exe().unwrap();
        let input = raw.to_string();
        // The identical helper must observe the dummy FD without confinement.
        let mut direct = Command::new(&executable)
            .args(probe_args())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        direct
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let direct = direct.wait_with_output().unwrap();
        assert!(direct.status.success());
        assert!(
            String::from_utf8(direct.stdout)
                .unwrap()
                .contains(&format!("PREAD_LEAK:{SENTINEL}"))
        );
        let policy = crate::SandboxPolicy::new(crate::SandboxMode::FullAccess, &[]).unwrap();
        let mut leaks = Vec::new();
        for controlled in [false, true] {
            file.seek(SeekFrom::Start(0)).unwrap();
            let (stdout, success) = if controlled {
                let out = crate::run_confined_controlled(
                    &policy,
                    &executable,
                    &probe_args(),
                    Some(&input),
                    || false,
                )
                .unwrap();
                assert!(out.pending.is_none());
                (out.stdout, out.status.unwrap().success())
            } else {
                let out =
                    crate::run_confined(&policy, &executable, &probe_args(), Some(&input)).unwrap();
                (out.stdout, out.status == 0)
            };
            if stdout.contains(SENTINEL) {
                leaks.push(controlled);
            } else {
                assert!(
                    success && stdout.contains("PREAD_CLOSED_EBADF"),
                    "probe did not confirm a closed FD: {stdout}"
                );
            }
            assert_eq!(
                unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_GETFD) },
                0
            );
            file.seek(SeekFrom::Start(0)).unwrap();
            let mut parent = String::new();
            file.read_to_string(&mut parent).unwrap();
            assert_eq!(parent, SENTINEL);
        }
        assert!(
            leaks.is_empty(),
            "inherited descriptor leaked; controlled modes={leaks:?}"
        );
    }

    // FD pressure and resource-limit changes are confined to this explicitly
    // invoked process, never the cargo test runner (or the harness).
    #[test]
    #[ignore = "fixture entered explicitly by descriptor tests"]
    fn many_descriptors_with_lowered_limit() {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(SENTINEL.as_bytes()).unwrap();
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
            0
        );
        let available = limit.rlim_max.min(2048);
        assert!(
            available >= 1024,
            "fixture needs space for high and many FDs"
        );
        limit.rlim_cur = available;
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
        let mut descriptors = Vec::new();
        // 320 numeric direntry64 records exceed the production 4096-byte buffer.
        for minimum in std::iter::repeat_n(128, 320).chain([available as i32 - 1]) {
            let fd = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD, minimum) };
            assert!(fd >= minimum);
            descriptors.push(unsafe { OwnedFd::from_raw_fd(fd) });
        }
        limit.rlim_cur = 64;
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
        let input = descriptors
            .iter()
            .map(|fd| fd.as_raw_fd().to_string())
            .collect::<Vec<_>>()
            .join(" ");
        let executable = std::env::current_exe().unwrap();
        let mut direct = Command::new(&executable)
            .args(probe_args())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        direct
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let direct = direct.wait_with_output().unwrap();
        assert!(direct.status.success());
        assert_eq!(
            String::from_utf8(direct.stdout)
                .unwrap()
                .matches(&format!("PREAD_LEAK:{SENTINEL}"))
                .count(),
            descriptors.len()
        );
        let policy = crate::SandboxPolicy::new(crate::SandboxMode::FullAccess, &[]).unwrap();
        for controlled in [false, true] {
            let stdout = if controlled {
                let out = crate::run_confined_controlled(
                    &policy,
                    &executable,
                    &probe_args(),
                    Some(&input),
                    || false,
                )
                .unwrap();
                assert!(out.status.unwrap().success());
                assert!(out.pending.is_none() && out.problem.is_none());
                assert!(out.stdout_eof && out.stderr_eof);
                out.stdout
            } else {
                let out = crate::confine::run_confined_with_broker(
                    &policy,
                    &executable,
                    &probe_args(),
                    Some(&input),
                    None,
                )
                .unwrap();
                assert_eq!(out.status, 0);
                out.stdout
            };
            assert!(!stdout.contains(SENTINEL), "controlled={controlled}");
            assert_eq!(
                stdout.matches("PREAD_CLOSED_EBADF").count(),
                descriptors.len()
            );
        }
        for fd in descriptors {
            assert_eq!(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) }, 0);
            let mut bytes = [0; SENTINEL.len()];
            assert_eq!(
                unsafe { libc::pread(fd.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len(), 0) },
                bytes.len() as isize
            );
            assert_eq!(&bytes, SENTINEL.as_bytes());
        }
        println!("HIGH_AND_MULTIPAGE_OK");
    }

    fn isolated_fixture(name: &str, marker: &str) {
        let out = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--ignored", "--nocapture"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", "/tmp")
            .env("TMPDIR", "/tmp")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8_lossy(&out.stdout).contains(marker));
    }

    #[test]
    fn high_fds_and_multiple_batches_are_not_limited_by_rlimit() {
        isolated_fixture(
            "child_fds::tests::many_descriptors_with_lowered_limit",
            "HIGH_AND_MULTIPAGE_OK",
        );
    }

    #[test]
    fn subsequent_pre_exec_error_reaches_parent() {
        use std::os::unix::process::CommandExt;
        let policy = crate::SandboxPolicy::new(crate::SandboxMode::FullAccess, &[]).unwrap();
        let mut command = crate::confine::build_command(
            &policy,
            std::path::Path::new("/bin/echo"),
            &["must-not-run".into()],
        )
        .unwrap();
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        const LATER_ERROR: i32 = 0x706c_6174;
        unsafe {
            command.pre_exec(|| Err(std::io::Error::from_raw_os_error(LATER_ERROR)));
        }
        let error = command.spawn().expect_err("must report pre_exec failure");
        assert_eq!(error.raw_os_error(), Some(LATER_ERROR));
        assert!(matches!(
            crate::confine::classify_spawn_error(error),
            crate::SandboxError::Io(_)
        ));
    }

    #[test]
    fn fd_failure_sentinel_is_not_an_ordinary_spawn_error() {
        use std::os::unix::process::CommandExt;
        let mut command = Command::new("/bin/echo");
        super::install(&mut command);
        unsafe {
            command.pre_exec(|| Err(std::io::Error::from_raw_os_error(super::FAILURE_ERRNO)));
        }
        let error = command.spawn().expect_err("must report boundary failure");
        assert_eq!(error.raw_os_error(), Some(super::FAILURE_ERRNO));
        assert!(matches!(
            crate::confine::classify_spawn_error(error),
            crate::SandboxError::NotEnforced(_)
        ));
    }

    #[test]
    fn missing_exec_after_fd_marking_is_io() {
        let mut command = Command::new("/definitely/missing/polaris-fd-test");
        super::install(&mut command);
        let error = command.spawn().expect_err("missing binary must fail");
        assert_eq!(error.raw_os_error(), Some(libc::ENOENT));
        assert!(matches!(
            crate::confine::classify_spawn_error(error),
            crate::SandboxError::Io(_)
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn directory_syscall_failure_returns_through_exec_channel() {
        use std::os::unix::process::CommandExt;
        let mut command = Command::new("/bin/echo");
        super::install(&mut command);
        unsafe {
            command.pre_exec(|| {
                super::mark_directory(-1)
                    .map_err(|_| std::io::Error::from_raw_os_error(super::FAILURE_ERRNO))
            });
        }
        let error = command.spawn().expect_err("invalid directory must fail");
        assert_eq!(error.raw_os_error(), Some(super::FAILURE_ERRNO));
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "signal fixture entered only in an isolated process"]
    fn interrupted_raw_syscall_fixture() {
        use std::os::unix::process::CommandExt;
        extern "C" fn alarm_handler(_: libc::c_int) {}
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = alarm_handler as *const () as usize;
        assert_eq!(unsafe { libc::sigemptyset(&mut action.sa_mask) }, 0);
        assert_eq!(
            unsafe { libc::sigaction(libc::SIGALRM, &action, std::ptr::null_mut()) },
            0
        );
        let mut pipe = [-1; 2];
        assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
        let reader = unsafe { OwnedFd::from_raw_fd(pipe[0]) };
        let _writer = unsafe { OwnedFd::from_raw_fd(pipe[1]) };
        let fd = reader.as_raw_fd();
        let mut command = Command::new("/bin/echo");
        super::install(&mut command);
        const INTERRUPTED: i32 = 0x7069_6e74;
        unsafe {
            command.pre_exec(move || {
                libc::alarm(1);
                let mut byte = 0u8;
                // SYS_read exercises the same syscall/cerror EINTR path as
                // getdirentries64. Both pipe ends must still be open pre-exec.
                let result = libc::syscall(3 as libc::c_int, fd, &mut byte as *mut u8, 1usize);
                let interrupted = result == -1
                    && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR);
                libc::alarm(0);
                Err(std::io::Error::from_raw_os_error(if interrupted {
                    INTERRUPTED
                } else {
                    libc::EIO
                }))
            });
        }
        let error = command
            .spawn()
            .expect_err("EINTR must return through Rust's channel");
        assert_eq!(error.raw_os_error(), Some(INTERRUPTED));
        println!("RAW_EINTR_RETURNED");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn interrupted_raw_syscall_preserves_exec_error_channel() {
        isolated_fixture(
            "child_fds::tests::interrupted_raw_syscall_fixture",
            "RAW_EINTR_RETURNED",
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn malformed_directory_records_fail_closed() {
        fn record(name: &[u8]) -> Vec<u8> {
            let name_offset = std::mem::offset_of!(libc::dirent, d_name);
            let size = (name_offset + name.len() + 1).next_multiple_of(8);
            let mut bytes = vec![0; size];
            bytes[16..18].copy_from_slice(&(size as u16).to_ne_bytes());
            bytes[18..20].copy_from_slice(&(name.len() as u16).to_ne_bytes());
            bytes[name_offset..name_offset + name.len()].copy_from_slice(name);
            bytes
        }
        let good = record(b"2147483647");
        let mut seen = Vec::new();
        super::visit_records(&good, |fd| {
            seen.push(fd);
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, [i32::MAX]);
        for end in 1..good.len() {
            assert!(super::visit_records(&good[..end], |_| Ok(())).is_err());
        }
        for name in [b"".as_slice(), b"2147483648", b"-1", b"123x"] {
            assert!(super::visit_records(&record(name), |_| Ok(())).is_err());
        }
        let mut bad = record(b"128");
        bad[16..18].fill(0);
        assert!(super::visit_records(&bad, |_| Ok(())).is_err());
        let mut bad = record(b"128");
        bad[18..20].copy_from_slice(&u16::MAX.to_ne_bytes());
        assert!(super::visit_records(&bad, |_| Ok(())).is_err());
        assert!(super::visit_records(&good, |_| Err(())).is_err());
    }
}
