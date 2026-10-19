//! The only credential handoff executable used by C08/C10 Git children.
//!
//! It reads one value from an inherited anonymous pipe and writes it to Git's
//! askpass stdout. It never accepts a credential through argv, environment,
//! URL, or a filesystem path.

#[cfg(unix)]
use polaris_auth::git_credentials::MAX_GIT_CREDENTIAL_VALUE_BYTES;
#[cfg(unix)]
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::fd::FromRawFd;

#[cfg(any(unix, test))]
fn fd_for_prompt(prompt: &str) -> Option<&'static str> {
    if prompt.starts_with("Username for ") {
        Some("POLARIS_GIT_ASKPASS_USERNAME_FD")
    } else if prompt.starts_with("Password for ") {
        Some("POLARIS_GIT_ASKPASS_PASSWORD_FD")
    } else {
        None
    }
}

#[cfg(unix)]
fn run() -> Result<(), ()> {
    let mut args = std::env::args();
    let _program = args.next().ok_or(())?;
    let prompt = args.next().ok_or(())?;
    if args.next().is_some() {
        return Err(());
    }
    let name = fd_for_prompt(&prompt).ok_or(())?;
    let fd: i32 = std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|fd| *fd >= 3)
        .ok_or(())?;
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || flags & libc::O_ACCMODE != libc::O_RDONLY {
        return Err(());
    }
    let mut metadata: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut metadata) } != 0
        || metadata.st_mode & libc::S_IFMT != libc::S_IFIFO
    {
        return Err(());
    }
    // This takes ownership only in the helper process. The parent holds its
    // own descriptor until the Git child has been reaped.
    let mut pipe = unsafe { std::fs::File::from_raw_fd(fd) };
    let mut value = zeroize::Zeroizing::new(Vec::with_capacity(1024));
    if Read::by_ref(&mut pipe)
        .take(MAX_GIT_CREDENTIAL_VALUE_BYTES as u64 + 1)
        .read_to_end(&mut value)
        .is_err()
        || value.is_empty()
        || value.len() > MAX_GIT_CREDENTIAL_VALUE_BYTES
    {
        return Err(());
    }
    if value
        .iter()
        .any(|byte| matches!(byte, b'\0' | b'\r' | b'\n'))
    {
        return Err(());
    }
    let mut out = std::io::stdout().lock();
    if out
        .write_all(&value)
        .and_then(|_| out.write_all(b"\n"))
        .and_then(|_| out.flush())
        .is_err()
    {
        return Err(());
    }
    // `Zeroizing` erases the sole owned password copy when it drops.
    drop(pipe);
    Ok(())
}

#[cfg(not(unix))]
fn run() -> Result<(), ()> {
    Err(())
}

fn main() {
    if run().is_err() {
        eprintln!("polaris-git-askpass: credential handoff rejected");
        std::process::exit(71);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_prompts_select_only_their_bound_pipe_names() {
        assert_eq!(
            fd_for_prompt("Username for 'https://fixture.invalid': "),
            Some("POLARIS_GIT_ASKPASS_USERNAME_FD")
        );
        assert_eq!(
            fd_for_prompt("Password for 'https://fixture.invalid': "),
            Some("POLARIS_GIT_ASKPASS_PASSWORD_FD")
        );
    }

    #[test]
    fn unrelated_prompts_never_select_a_pipe() {
        for prompt in [
            "",
            "username for fixture",
            "Password",
            "Passphrase for fixture",
        ] {
            assert_eq!(fd_for_prompt(prompt), None);
        }
    }
}
