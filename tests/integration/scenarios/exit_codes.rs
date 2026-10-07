//! scinit's exit status must mirror its child's

use crate::integration::harness::{Scinit, TIMEOUT};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;

/// Child exits 0 → scinit exits 0
#[test]
fn child_exit_zero() {
    let (scinit, status) = Scinit::builder().child(["exit", "0"]).run(TIMEOUT).unwrap();
    scinit.child_pid().unwrap();
    scinit.assert_exit_code(status, 0);
}

/// Child exits 42 → scinit exits 42
#[test]
fn child_exit_code_propagates() {
    let (scinit, status) = Scinit::builder().child(["exit", "42"]).run(TIMEOUT).unwrap();
    scinit.child_pid().unwrap();
    scinit.assert_exit_code(status, 42);
}

/// Child killed externally by SIGKILL → scinit exits 128 + 9
#[test]
fn child_killed_by_signal_exits_128_plus_signo() {
    let (mut scinit, pid) = Scinit::builder().child(["run"]).start();
    kill(Pid::from_raw(pid), Signal::SIGKILL).unwrap();
    let status = scinit.wait_exit(TIMEOUT).unwrap();
    scinit.assert_exit_code(status, 137);
}

/// Child that raises SIGTERM on itself → scinit exits 128 + 15
#[test]
fn child_self_signal_exits_128_plus_signo() {
    let (scinit, status) = Scinit::builder()
        .child(["kill-self", "TERM"])
        .run(TIMEOUT)
        .unwrap();
    assert!(!scinit.events_named("raise").is_empty(), "{}", scinit.diagnostics());
    scinit.assert_exit_code(status, 143);
}

/// The exit log names the child's signal by its platform number (SIGUSR1 is
/// 10 on Linux, 30 on macOS)
#[test]
fn child_signal_is_logged_by_name() {
    let (scinit, status) = Scinit::builder()
        // SCINIT_LOG once scinit stops reading RUST_LOG (#2); both until then
        .env("SCINIT_LOG", "info")
        .env("RUST_LOG", "info")
        .child(["kill-self", "USR1"])
        .run(TIMEOUT)
        .unwrap();
    scinit.assert_exit_code(status, 128 + Signal::SIGUSR1 as i32);
    let expected = format!("terminated by signal {} (SIGUSR1)", Signal::SIGUSR1 as i32);
    let logs = scinit.stdout() + &scinit.stderr();
    assert!(logs.contains(&expected), "missing {:?}\n{}", expected, scinit.diagnostics());
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

/// SIGTERM to scinit, child handles it and exits 0 → scinit exits 0
#[test]
fn sigterm_with_clean_child_exit_exits_zero() {
    let (mut scinit, pid) = Scinit::builder().child(["run"]).start();
    scinit.signal(Signal::SIGTERM).unwrap();
    scinit.wait_for_signal(pid, Signal::SIGTERM, TIMEOUT).unwrap();
    let status = scinit.wait_exit(TIMEOUT).unwrap();
    scinit.assert_exit_code(status, 0);
}

/// SIGTERM to scinit, child dies from it (default action) → 128 + 15
#[test]
fn sigterm_killing_child_exits_143() {
    let (mut scinit, _) = Scinit::builder().child(["run", "--trap", "USR1"]).start();
    scinit.signal(Signal::SIGTERM).unwrap();
    let status = scinit.wait_exit(TIMEOUT).unwrap();
    scinit.assert_exit_code(status, 143);
}

/// Child ignores SIGTERM and is SIGKILLed after the graceful timeout → 128 + 9
#[test]
fn sigterm_escalation_exits_137() {
    let (mut scinit, _) = Scinit::builder()
        .args(["--graceful-timeout-secs", "1"])
        .child(["run", "--ignore", "TERM"])
        .start();
    scinit.signal(Signal::SIGTERM).unwrap();
    let status = scinit.wait_exit(TIMEOUT).unwrap();
    scinit.assert_exit_code(status, 137);
}
