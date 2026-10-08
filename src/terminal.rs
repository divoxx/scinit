//! Handing the controlling terminal to the child.

use std::fs::File;
use std::io::IsTerminal;
use std::os::fd::RawFd;
use tracing::debug;

/// scinit's controlling terminal, if it has one. The child takes the
/// foreground through it before exec (see [`take_foreground`]).
pub fn controlling_terminal() -> Option<File> {
    let tty = match File::open("/dev/tty") {
        Ok(tty) => tty,
        Err(e) => {
            debug!(
                "Cannot open /dev/tty ({}), skipping foreground process group setup",
                e
            );
            return None;
        }
    };
    if !tty.is_terminal() {
        debug!("Not a terminal, skipping foreground process group setup");
        return None;
    }
    Some(tty)
}

/// In the child, between fork and exec: makes the child's process group (it
/// leads its own) the foreground process group of terminal `tty`, so it gets
/// Ctrl-C and terminal input.
///
/// It must run before the child's signal dispositions are reset: SIGTTOU,
/// still ignored as in scinit, would otherwise stop a background process
/// calling `tcsetpgrp`. Doing it in the child rather than in scinit after
/// the spawn means the child never runs in the background, where reading
/// or configuring the terminal would stop it.
///
/// Async-signal-safe: a failure is returned as the OS error, which fails the
/// spawn.
pub fn take_foreground(tty: RawFd) -> std::io::Result<()> {
    if unsafe { libc::tcsetpgrp(tty, libc::getpid()) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
