//! Turning the child's exit into scinit's own exit.

use crate::reaper::spawn_zombie_reap;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use tracing::{debug, info};

/// Shell-style exit code for a child's status: its exit code, or 128 + the
/// signal number if it was killed by a signal
pub fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(signal_exit_code))
        .unwrap_or(1)
}

/// Shell-style exit code for death by signal number `signal`
pub fn signal_exit_code(signal: i32) -> i32 {
    128 + signal
}

/// Handles the child's exit, which ends scinit too.
///
/// In container environments, scinit's lifecycle is tied to the child process,
/// so scinit exits with the child's exit code (see [`exit_code`]) and
/// orchestrators can tell a crash from a clean shutdown.
pub fn handle_child_exit(status: ExitStatus) -> i32 {
    log_child_exit(status);

    // Reap any remaining zombies before exiting
    debug!("Reaping any remaining zombie processes before exit");
    spawn_zombie_reap();

    exit_code(status)
}

fn log_child_exit(status: ExitStatus) {
    if status.success() {
        info!("Child process exited successfully, scinit exiting cleanly");
    } else if let Some(code) = status.code() {
        info!("Child process exited with error code {}, scinit exiting", code);
    } else if let Some(signal) = status.signal() {
        info!("Child process terminated by signal {} ({}), scinit exiting",
              signal, signal_name(signal));
    } else {
        info!("Child process terminated by signal, scinit exiting");
    }
}

/// Converts signal number to human-readable name
fn signal_name(signal: i32) -> &'static str {
    match signal {
        2 => "SIGINT",
        9 => "SIGKILL",
        15 => "SIGTERM",
        3 => "SIGQUIT",
        1 => "SIGHUP",
        10 => "SIGUSR1",
        12 => "SIGUSR2",
        17 => "SIGCHLD",
        _ => "UNKNOWN",
    }
}
