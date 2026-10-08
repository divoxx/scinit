//! Zombie reaping for scinit's children other than the managed one: the
//! orphans scinit inherits as PID 1 or, on Linux, as a child subreaper.

use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicI32, Ordering};
use tracing::{debug, warn};

/// PID of the managed child, or 0 when there is none.
///
/// The zombie reaper must leave this process alone: its exit status belongs to
/// tokio's `child.wait()`, which fails with ECHILD if the reaper takes it first.
static MANAGED_CHILD: AtomicI32 = AtomicI32::new(0);

/// Marks scinit as a child subreaper, so orphans of its descendants are
/// reparented to scinit and reaped here even when it isn't PID 1. Linux
/// only; macOS has no equivalent, so there it does nothing. Not inherited by
/// the child: the flag is cleared on fork.
pub fn become_subreaper() {
    #[cfg(target_os = "linux")]
    match nix::sys::prctl::set_child_subreaper(true) {
        Ok(()) => debug!("registered as child subreaper"),
        // Orphans then go to PID 1 or another subreaper, as without the flag
        Err(e) => warn!("failed to register as child subreaper: {}", e),
    }
}

/// Marks `pid` as the managed child
pub fn set_managed_child(pid: Pid) {
    MANAGED_CHILD.store(pid.as_raw(), Ordering::SeqCst);
}

/// Clears the managed child mark, unless another child has replaced `pid`
pub fn clear_managed_child(pid: Pid) {
    let _ = MANAGED_CHILD.compare_exchange(pid.as_raw(), 0, Ordering::SeqCst, Ordering::SeqCst);
}

/// Returns the PID of a child that has exited, without reaping it
fn peek_exited_child() -> Option<Pid> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::waitid(
            libc::P_ALL,
            0,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if rc != 0 {
        // ECHILD: no children at all
        return None;
    }
    #[cfg(target_os = "linux")]
    let pid = unsafe { info.si_pid() };
    #[cfg(not(target_os = "linux"))]
    let pid = info.si_pid;
    // With WNOHANG, si_pid stays 0 when no child has exited yet
    (pid != 0).then(|| Pid::from_raw(pid))
}

/// Reaps zombie processes to prevent process table exhaustion
///
/// Each exited child is peeked at before being reaped, so the managed child
/// is left for tokio's `child.wait()`. While the managed child is the zombie
/// returned first, other zombies wait for the next pass; tokio reaps the
/// managed child promptly and [`reap_before_exit`] runs a final pass.
pub fn reap_zombies() {
    let mut reaped_count = 0;

    while let Some(pid) = peek_exited_child() {
        if is_managed_child(pid) {
            break;
        }
        match reap_one(pid) {
            ControlFlow::Continue(reaped) => reaped_count += usize::from(reaped),
            ControlFlow::Break(()) => break,
        }
    }

    if reaped_count > 0 {
        debug!("reaped {} zombie processes", reaped_count);
    }
}

fn is_managed_child(pid: Pid) -> bool {
    pid.as_raw() == MANAGED_CHILD.load(Ordering::SeqCst)
}

/// Reaps exited child `pid`. Continues with whether it was reaped, or
/// breaks when the pass should stop.
fn reap_one(pid: Pid) -> ControlFlow<(), bool> {
    match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
        Ok(WaitStatus::Exited(pid, status)) => {
            debug!("reaped zombie process {} with exit status {}", pid, status);
            ControlFlow::Continue(true)
        }
        Ok(WaitStatus::Signaled(pid, signal, _)) => {
            debug!(
                "reaped zombie process {} killed by signal {:?}",
                pid, signal
            );
            ControlFlow::Continue(true)
        }
        Ok(WaitStatus::StillAlive) => ControlFlow::Break(()),
        Ok(other) => {
            debug!("ignoring wait status {:?}", other);
            ControlFlow::Continue(false)
        }
        // Reaped concurrently (e.g. by tokio)
        Err(nix::Error::ECHILD) => ControlFlow::Continue(false),
        Err(e) => {
            warn!("error reaping zombies: {}", e);
            ControlFlow::Break(())
        }
    }
}

/// Starts a zombie reap pass on a blocking thread, without waiting for it
pub fn spawn_zombie_reap() {
    tokio::task::spawn_blocking(reap_zombies);
}

/// The last reap pass, on every exit path. Runs inline, not in the
/// background: the runtime shuts down right after, so a background reap
/// might never run (WNOHANG keeps it from blocking)
pub fn reap_before_exit() {
    debug!("Reaping any remaining zombie processes before exit");
    reap_zombies();
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::errno::Errno;
    use std::process::Command;

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
    fn reap_before_exit_reaps_remaining_zombies() {
        if !isolated("reaper::tests::reap_before_exit_reaps_remaining_zombies") {
            return;
        }
        // An unmanaged child that has exited, like an orphan scinit inherited
        let orphan = Command::new("true").spawn().unwrap();
        let pid = Pid::from_raw(orphan.id() as i32);
        wait_until_zombie(pid);

        // As in scinit: called on the runtime, which shuts down right after
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async { reap_before_exit() });
        let reaped = waitpid(pid, Some(WaitPidFlag::WNOHANG));
        runtime.shutdown_background();

        assert_eq!(reaped, Err(Errno::ECHILD), "zombie {pid} was not reaped");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn become_subreaper_sets_the_flag() {
        // The flag would make this process adopt the orphans of other tests
        if !isolated("reaper::tests::become_subreaper_sets_the_flag") {
            return;
        }
        become_subreaper();
        assert_eq!(nix::sys::prctl::get_child_subreaper(), Ok(true));
    }
}
