//! Keeps fds scinit itself inherited from reaching the child.
//!
//! Only stdio and the activated sockets should be passed on, as with
//! systemd. The child marks every other fd close-on-exec between fork and
//! exec; the activated sockets are `dup2`ed to fds 3.. afterwards, which
//! clears the flag on them again.

use std::os::unix::io::RawFd;

/// First fd that isn't stdio
const FIRST_NON_STDIO_FD: RawFd = 3;

/// Cap on the fds scanned when `close_range` isn't available, so a huge
/// `RLIMIT_NOFILE` doesn't make every spawn slow
const MAX_SCANNED_FDS: RawFd = 65536;

/// Upper bound (exclusive) for the fd numbers to scan; call before forking
pub fn open_fd_limit() -> RawFd {
    let limit = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
    if limit > 0 {
        limit.min(MAX_SCANNED_FDS as libc::c_long) as RawFd
    } else {
        1024
    }
}

/// Marks every fd above stdio close-on-exec, using `close_range` where the
/// kernel has it and an `fcntl` scan below `limit` otherwise
///
/// # Safety
/// Must only be called in the child between fork and exec (async-signal-safe,
/// no allocation).
pub unsafe fn mark_non_stdio_cloexec(limit: RawFd) {
    #[cfg(target_os = "linux")]
    {
        let marked = libc::syscall(
            libc::SYS_close_range,
            FIRST_NON_STDIO_FD as libc::c_uint,
            libc::c_uint::MAX,
            libc::CLOSE_RANGE_CLOEXEC,
        );
        if marked == 0 {
            return;
        }
    }
    for fd in FIRST_NON_STDIO_FD..limit {
        // Unopened fds just fail with EBADF
        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
    }
}
