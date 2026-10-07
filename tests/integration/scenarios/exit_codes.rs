//! scinit's exit status must mirror its child's

use crate::integration::harness::{assert_exit_code, Scinit, TIMEOUT};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;

/// Child exits 0 → scinit exits 0
#[test]
fn child_exit_zero() {
    let (scinit, status) = Scinit::builder().child(["exit", "0"]).run(TIMEOUT).unwrap();
    scinit.child_pid().unwrap();
    assert_exit_code(&scinit, status, 0);
}

/// Child exits 42 → scinit exits 42
#[test]
#[ignore = "bug: exit-code-propagation (KNOWN-ISSUES.md)"]
fn child_exit_code_propagates() {
    let (scinit, status) = Scinit::builder().child(["exit", "42"]).run(TIMEOUT).unwrap();
    scinit.child_pid().unwrap();
    assert_exit_code(&scinit, status, 42);
}

/// Child killed externally by SIGKILL → scinit exits 128 + 9
#[test]
#[ignore = "bug: exit-code-propagation (KNOWN-ISSUES.md)"]
fn child_killed_by_signal_exits_128_plus_signo() {
    let mut scinit = Scinit::builder().child(["run"]).spawn().unwrap();
    let pid = scinit.child_pid().unwrap();
    kill(Pid::from_raw(pid), Signal::SIGKILL).unwrap();
    let status = scinit.wait_exit(TIMEOUT).unwrap();
    assert_exit_code(&scinit, status, 137);
}

/// Child that raises SIGTERM on itself → scinit exits 128 + 15
#[test]
#[ignore = "bug: exit-code-propagation (KNOWN-ISSUES.md)"]
fn child_self_signal_exits_128_plus_signo() {
    let (scinit, status) = Scinit::builder()
        .child(["kill-self", "TERM"])
        .run(TIMEOUT)
        .unwrap();
    assert!(!scinit.events_named("raise").is_empty(), "{}", scinit.diagnostics());
    assert_exit_code(&scinit, status, 143);
}

/// Repeated clean exits never surface as failures (e.g. the background
/// reaper stealing the child's status from tokio's `wait`)
#[test]
fn repeated_clean_exits_stay_zero() {
    for i in 0..50 {
        let (scinit, status) = Scinit::builder().child(["exit", "0"]).run(TIMEOUT).unwrap();
        assert_eq!(
            status.code(),
            Some(0),
            "run {} exited with {:?}\n{}",
            i,
            status,
            scinit.diagnostics()
        );
    }
}
