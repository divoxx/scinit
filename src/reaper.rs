//! Zombie reaping for scinit's children other than the managed one, which
//! matters when scinit runs as PID 1 and inherits orphans.

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
/// managed child promptly and `handle_child_exit` runs a final pass.
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
