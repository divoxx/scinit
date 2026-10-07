//! Exec of a socket-activated child, following the systemd protocol.
//!
//! The protocol needs two things only the forked child can provide:
//! `LISTEN_PID` must be the child's own PID, and the listening sockets must
//! sit at fds 3, 4, ... in `LISTEN_FDS` order. `std::process::Command` builds
//! the environment before forking, so the child execs itself from a
//! `pre_exec` hook instead.
//!
//! Everything is prepared before the fork. The child only runs
//! async-signal-safe operations: `fcntl`, `dup2`, `getpid`, writing digits
//! into a preallocated buffer, and `execve`.

use crate::environment::Environment;
use crate::program::resolve_program;
use crate::Result;
use eyre::eyre;
use std::ffi::{CString, OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::RawFd;

/// First fd passed to a socket-activated service (`SD_LISTEN_FDS_START`)
pub const LISTEN_FDS_START: RawFd = 3;

const LISTEN_PID_PREFIX: &[u8] = b"LISTEN_PID=";

/// Most decimal digits a `u64` can have
const MAX_DIGITS: usize = 20;

/// Room for any pid's decimal digits plus the terminating NUL
const PID_DIGITS: usize = MAX_DIGITS + 1;

/// Pre-built `execve` arguments for a socket-activated child
pub struct SocketActivationExec {
    program: CString,
    // Own the strings the pointer arrays point into
    _argv: Vec<CString>,
    _envp: Vec<CString>,
    argv_ptrs: Vec<*const libc::c_char>,
    envp_ptrs: Vec<*const libc::c_char>,
    /// `LISTEN_PID=` followed by space for the digits, filled in by the child
    pid_slot: Vec<u8>,
    listen_fds: Vec<RawFd>,
    /// Scratch space for the temporary duplicates made while remapping
    scratch_fds: Vec<RawFd>,
}

// The raw pointers point into heap buffers owned by this struct, which never
// move or change outside `exec_in_child` (run in the single-threaded child)
unsafe impl Send for SocketActivationExec {}
unsafe impl Sync for SocketActivationExec {}

impl SocketActivationExec {
    /// Prepares the exec of `command args` with `listen_fds` passed as the
    /// activated sockets and `overrides` (`LISTEN_FDS` and the configured
    /// variables) added to scinit's own environment
    pub fn new(
        command: &str,
        args: &[String],
        listen_fds: Vec<RawFd>,
        overrides: Environment,
    ) -> Result<Self> {
        let program = resolve_program(command)?;
        let env = child_env(overrides);

        // execve takes C strings, and the child must not allocate, so they
        // are converted here
        let program = c_string(program)?;
        let argv = c_strings(std::iter::once(command).chain(args.iter().map(String::as_str)))?;
        let envp = c_strings(env)?;

        let mut pid_slot = LISTEN_PID_PREFIX.to_vec();
        pid_slot.resize(LISTEN_PID_PREFIX.len() + PID_DIGITS, 0);

        let argv_ptrs = null_terminated(argv.iter().map(|s| s.as_ptr()));
        let envp_ptrs = null_terminated(
            envp.iter()
                .map(|s| s.as_ptr())
                .chain(std::iter::once(pid_slot.as_ptr() as *const libc::c_char)),
        );

        Ok(Self {
            program,
            _argv: argv,
            _envp: envp,
            argv_ptrs,
            envp_ptrs,
            pid_slot,
            scratch_fds: vec![-1; listen_fds.len()],
            listen_fds,
        })
    }

    /// Runs in the forked child: moves the sockets to fds 3.., fills in
    /// `LISTEN_PID` and execs. Only returns on failure.
    ///
    /// # Safety
    /// Must only be called in the child between fork and exec.
    pub unsafe fn exec_in_child(&mut self) -> std::io::Error {
        let n = self.listen_fds.len() as RawFd;

        // Duplicate above the target range first, so no source is clobbered
        // while the targets are filled. The duplicates are close-on-exec.
        for (i, &fd) in self.listen_fds.iter().enumerate() {
            let dup = libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, LISTEN_FDS_START + n);
            if dup < 0 {
                return std::io::Error::last_os_error();
            }
            self.scratch_fds[i] = dup;
        }
        // dup2 clears close-on-exec on the target, so exactly these survive exec
        for (i, &dup) in self.scratch_fds.iter().enumerate() {
            if libc::dup2(dup, LISTEN_FDS_START + i as RawFd) < 0 {
                return std::io::Error::last_os_error();
            }
        }

        write_decimal(
            &mut self.pid_slot[LISTEN_PID_PREFIX.len()..],
            libc::getpid() as u64,
        );

        libc::execve(
            self.program.as_ptr(),
            self.argv_ptrs.as_ptr(),
            self.envp_ptrs.as_ptr(),
        );
        std::io::Error::last_os_error()
    }
}

fn c_string(s: impl AsRef<OsStr>) -> Result<CString> {
    CString::new(s.as_ref().as_bytes())
        .map_err(|_| eyre!("argument or environment contains a NUL byte"))
}

fn c_strings<T: AsRef<OsStr>>(items: impl IntoIterator<Item = T>) -> Result<Vec<CString>> {
    items.into_iter().map(c_string).collect()
}

/// scinit's environment with `overrides` applied, as `KEY=value` entries.
/// Inherited `LISTEN_*` describe someone else's sockets, so they are dropped.
fn child_env(overrides: Environment) -> Vec<OsString> {
    let mut env: Vec<(OsString, OsString)> = std::env::vars_os()
        .filter(|(key, _)| !key.as_bytes().starts_with(b"LISTEN_"))
        .collect();
    for (key, value) in overrides.into_inner() {
        env.retain(|(k, _)| k != key.as_str());
        env.push((key.into(), value.into()));
    }
    env.iter().map(|(key, value)| env_entry(key, value)).collect()
}

/// `key=value`, as `execve` expects it
fn env_entry(key: &OsStr, value: &OsStr) -> OsString {
    let mut entry = key.to_os_string();
    entry.push("=");
    entry.push(value);
    entry
}

fn null_terminated(ptrs: impl Iterator<Item = *const libc::c_char>) -> Vec<*const libc::c_char> {
    ptrs.chain(std::iter::once(std::ptr::null())).collect()
}

/// Writes `value` as NUL-terminated decimal into `buf` without allocating
fn write_decimal(buf: &mut [u8], mut value: u64) {
    let mut digits = [0u8; MAX_DIGITS];
    let mut len = 0;
    loop {
        digits[len] = b'0' + (value % 10) as u8;
        len += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    for i in 0..len {
        buf[i] = digits[len - 1 - i];
    }
    buf[len] = 0;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_write_decimal() {
        let mut buf = [0xffu8; PID_DIGITS];
        write_decimal(&mut buf, 0);
        assert_eq!(&buf[..2], b"0\0");
        write_decimal(&mut buf, 4194304);
        assert_eq!(&buf[..8], b"4194304\0");
        write_decimal(&mut buf, u64::MAX);
        assert_eq!(&buf[..], b"18446744073709551615\0");
    }
}
