//! Turning the child's exit into scinit's own exit.

use nix::sys::signal::Signal;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use tracing::info;

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
    exit_code(status)
}

fn log_child_exit(status: ExitStatus) {
    if status.success() {
        info!("Child process exited successfully, scinit exiting cleanly");
    } else if let Some(code) = status.code() {
        info!(
            "Child process exited with error code {}, scinit exiting",
            code
        );
    } else if let Some(signal) = status.signal() {
        info!(
            "Child process terminated by signal {} ({}), scinit exiting",
            signal,
            signal_name(signal)
        );
    } else {
        info!("Child process terminated by signal, scinit exiting");
    }
}

/// Platform name of signal number `signal`, e.g. "SIGUSR1"
fn signal_name(signal: i32) -> &'static str {
    Signal::try_from(signal).map_or("UNKNOWN", Signal::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_name_uses_platform_numbers() {
        assert_eq!(signal_name(Signal::SIGUSR1 as i32), "SIGUSR1");
        assert_eq!(signal_name(Signal::SIGUSR2 as i32), "SIGUSR2");
        assert_eq!(signal_name(Signal::SIGCHLD as i32), "SIGCHLD");
        assert_eq!(signal_name(Signal::SIGKILL as i32), "SIGKILL");
    }

    #[test]
    fn signal_name_covers_signals_outside_the_common_set() {
        assert_eq!(signal_name(Signal::SIGSEGV as i32), "SIGSEGV");
        assert_eq!(signal_name(Signal::SIGABRT as i32), "SIGABRT");
    }

    #[test]
    fn signal_name_of_invalid_number_is_unknown() {
        assert_eq!(signal_name(0), "UNKNOWN");
        assert_eq!(signal_name(999), "UNKNOWN");
    }
}
