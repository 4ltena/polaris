//! Fixed inherited endpoint setup, before runtime initialization or diagnostics.
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::net::UnixStream;

/// The explicit native launcher supplied a socket on stderr. Errors must not be
/// printed to stderr by the caller: setup may have failed before redirection.
/// Returns a CLOEXEC endpoint; this never accepts a model-supplied FD or path.
pub fn take_recovery_socket() -> io::Result<UnixStream> {
    // SAFETY: borrowing the standard descriptor does not assume ownership.
    let inherited = unsafe { BorrowedFd::borrow_raw(libc::STDERR_FILENO) };
    let owned = inherited.try_clone_to_owned()?;
    let null = std::fs::OpenOptions::new().write(true).open("/dev/null")?;
    // SAFETY: both descriptors are valid; dup2 replaces only standard error.
    if unsafe { libc::dup2(null.as_raw_fd(), libc::STDERR_FILENO) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let stream = UnixStream::from(owned);
    // Reject non-Unix/non-connected descriptors before entering protocol I/O.
    // SAFETY: sockaddr_storage is a plain C output buffer.
    let mut address: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut address_length = std::mem::size_of_val(&address) as libc::socklen_t;
    // SAFETY: output pointers refer to valid storage and its exact capacity.
    if unsafe {
        libc::getpeername(
            stream.as_raw_fd(),
            (&mut address as *mut libc::sockaddr_storage).cast(),
            &mut address_length,
        )
    } == -1
    {
        return Err(io::Error::last_os_error());
    }
    if address.ss_family as libc::c_int != libc::AF_UNIX {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "recovery socket family",
        ));
    }
    let mut kind: libc::c_int = 0;
    let mut length = std::mem::size_of_val(&kind) as libc::socklen_t;
    // SAFETY: the output buffer and its length are correctly sized and live.
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut kind as *mut libc::c_int).cast(),
            &mut length,
        )
    } == -1
    {
        return Err(io::Error::last_os_error());
    }
    if kind != libc::SOCK_STREAM {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "recovery socket type",
        ));
    }
    stream.set_nonblocking(true)?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::fd::OwnedFd;
    use std::process::{Command, Stdio};

    #[test]
    fn child_entry() {
        let Ok(mode) = std::env::var("POLARIS_RECOVERY_SOCKET_TEST") else {
            return;
        };
        let result = take_recovery_socket();
        if mode == "invalid" {
            assert!(result.is_err());
            return;
        }
        let mut stream = result.unwrap();
        // SAFETY: query the owned descriptor, without modifying it.
        assert_ne!(
            unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        stream.write_all(b"ok").unwrap();
    }

    fn child() -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "recovery_socket::tests::child_entry",
                "--nocapture",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        command
    }

    #[test]
    fn inherited_socket_is_owned_and_invalid_stderr_is_refused() {
        let (mut parent, child_end) = UnixStream::pair().unwrap();
        parent
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut process = child()
            .env("POLARIS_RECOVERY_SOCKET_TEST", "valid")
            .stderr(Stdio::from(OwnedFd::from(child_end)))
            .spawn()
            .unwrap();
        let mut result = [0; 2];
        let read = parent.read_exact(&mut result);
        let exit = process.wait().unwrap();
        read.unwrap();
        assert_eq!(&result, b"ok");
        assert!(exit.success());
        assert!(
            child()
                .env("POLARIS_RECOVERY_SOCKET_TEST", "invalid")
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        );
    }
}
