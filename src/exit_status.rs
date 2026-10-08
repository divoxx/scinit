//! Turning the child's exit into scinit's own exit.

use crate::reaper::reap_zombies;
use nix::sys::signal::Signal;
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

    // Reap now, not in the background: the runtime shuts down right after
    // this, so a background reap might never run (WNOHANG keeps it from blocking)
    debug!("Reaping any remaining zombie processes before exit");
    reap_zombies();

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
    use nix::errno::Errno;
    use nix::sys::wait::{waitpid, WaitPidFlag};
    use nix::unistd::Pid;
    use std::process::Command;

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

    /// Set when this test binary runs a single test on its own
    const ISOLATED: &str = "SCINIT_UNIT_TEST_ISOLATED";

    /// Reruns test `name` alone in a fresh test process, so reaping every
    /// exited child can't take the children of tests running in parallel.
    /// Returns whether the caller is that fresh process.
    fn isolated(name: &str) -> bool {
        if std::env::var_os(ISOLATED).is_some() {
            return true;
        }
        let output = Command::new(std::env::current_exe().unwrap())
            .args([name, "--exact", "--test-threads=1"])
            .env(ISOLATED, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "isolated run of {name} failed:\n{stdout}{}",
            String::from_utf8_lossy(&output.stderr)
        );
        false
    }

    /// Blocks until child `pid` has exited, leaving it a zombie
    fn wait_until_zombie(pid: Pid) {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                pid.as_raw() as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        assert_eq!(rc, 0, "waitid: {}", Errno::last());
    }

    #[test]
    #[allow(clippy::zombie_processes, reason = "the code under test reaps it")]
    fn handle_child_exit_reaps_remaining_zombies() {
        if !isolated("exit_status::tests::handle_child_exit_reaps_remaining_zombies") {
            return;
        }
        // An unmanaged child that has exited, like an orphan scinit inherited
        let orphan = Command::new("true").spawn().unwrap();
        let pid = Pid::from_raw(orphan.id() as i32);
        wait_until_zombie(pid);

        // As in scinit: called on the runtime, which shuts down right after
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let code = runtime.block_on(async { handle_child_exit(ExitStatus::from_raw(0)) });
        let reaped = waitpid(pid, Some(WaitPidFlag::WNOHANG));
        runtime.shutdown_background();

        assert_eq!(code, 0);
        assert_eq!(reaped, Err(Errno::ECHILD), "zombie {pid} was not reaped");
    }
}
