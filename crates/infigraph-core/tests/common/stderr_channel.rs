//! The two things a client hands an MCP server as its stderr: a pipe or a unix socket (`lsof` on the live
//! servers shows both). Shared by the core and MCP tests of `proclog::shield_stderr`.

#![allow(dead_code)] // each test binary uses the subset it needs

use std::fs::File;
use std::os::fd::{FromRawFd, OwnedFd};

/// What a child's stderr is.
#[derive(Clone, Copy, Debug)]
pub enum Transport {
    Pipe,
    Socket,
}

fn cloexec(fd: i32) {
    // SAFETY: fcntl on a descriptor the caller just created.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
    }
}

/// `(the parent's end, the end that becomes the child's stderr)`. Both are close-on-exec, so only the stdio
/// the `Command` installs reaches the child.
pub fn channel(transport: Transport) -> (File, OwnedFd) {
    let mut fds = [0i32; 2];
    // SAFETY: both calls fill a two-element array on success.
    let rc = unsafe {
        match transport {
            Transport::Pipe => libc::pipe(fds.as_mut_ptr()),
            Transport::Socket => {
                libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr())
            }
        }
    };
    assert_eq!(rc, 0, "could not create the {transport:?}");
    cloexec(fds[0]);
    cloexec(fds[1]);
    // SAFETY: the descriptors were just created and are owned here.
    unsafe { (File::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}
