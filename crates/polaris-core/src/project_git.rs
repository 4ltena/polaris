//! A read-only git call against the user's own repository.
//!
//! Detection for the bundled `git_setting` command and the workspace view
//! both read a project's git state. Neither may reuse the synchronization
//! repository, which fixes an author and an initial branch of its own.
use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;

/// As much as one call may return. Detection reads short answers; the
/// workspace view reads a diff, which is why this is not smaller.
pub const MAX_GIT_OUTPUT_BYTES: usize = 64 * 1024;

/// The call did not produce a usable answer: git was missing, it failed, it
/// ran past the deadline, or it wrote more than the bound. Which one is not
/// distinguished, because the message can name paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GitReadError;

impl std::fmt::Display for GitReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Cannot retrieve Git information")
    }
}

impl std::error::Error for GitReadError {}

/// One bounded, read-only git call inside `root`.
///
/// The repository belongs to the person using Polaris, so nothing here
/// writes, and nothing carries an identity: no author or committer is set,
/// no branch is fixed, and the user's own configuration is left out rather
/// than replaced. That is the difference from the synchronization
/// repository, which pins both on purpose and must never be pointed at a
/// project. Hooks, fsmonitor, external diff and pagers are off, so opening
/// a directory cannot run what someone left in it.
///
/// `root` is an open directory the child `fchdir`s into, so the call is
/// bound to the directory that was checked, not to a path that could be
/// swapped underneath it. The call stops at `deadline` or at
/// `MAX_GIT_OUTPUT_BYTES`, killing the child either way.
pub fn read_only_git(
    root: &File,
    args: &[&str],
    deadline: std::time::Instant,
) -> Result<Vec<u8>, GitReadError> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    if std::time::Instant::now() >= deadline {
        return Err(GitReadError);
    }
    let mut command = Command::new("/usr/bin/git");
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("GIT_NO_LAZY_FETCH", "1")
        .args([
            "--no-pager",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.attributesFile=/dev/null",
            "-c",
            "diff.external=",
            "-c",
            "log.showSignature=false",
            "-c",
            "core.worktree=.",
            "-c",
            "core.bare=false",
            "-c",
            "status.renames=false",
        ])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    polaris_sandbox::protect_child_descriptors(&mut command);
    let fd = root.as_raw_fd();
    unsafe {
        command.pre_exec(move || {
            if libc::fchdir(fd) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().map_err(|_| GitReadError)?;
    let mut out = child.stdout.take().ok_or(GitReadError)?;
    unsafe {
        libc::fcntl(out.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK);
    }
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match out.read(&mut chunk) {
            Ok(n) => {
                bytes.extend_from_slice(&chunk[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(GitReadError);
            }
        }
        if bytes.len() > MAX_GIT_OUTPUT_BYTES || std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(GitReadError);
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                loop {
                    match out.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => bytes.extend_from_slice(&chunk[..n]),
                        Err(_) => break,
                    }
                    if bytes.len() > MAX_GIT_OUTPUT_BYTES {
                        return Err(GitReadError);
                    }
                }
                return if status.success() {
                    Ok(bytes)
                } else {
                    Err(GitReadError)
                };
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(5)),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(GitReadError);
            }
        }
    }
}
