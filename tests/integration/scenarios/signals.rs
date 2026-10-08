//! Signal handling: forwarding to the child's process group, termination and
//! SIGKILL escalation, and the signal state the child inherits.

use crate::integration::harness::{let_setup_writes_age, wait_for_pid_gone, Scinit, TIMEOUT};
use nix::sys::signal::Signal;
use std::time::{Duration, Instant};

/// Generous upper bound for scinit to exit after a termination signal
const EXIT_BOUND: Duration = Duration::from_secs(10);

/// A forwarded signal reaches a trapping child, and neither scinit nor the
/// child goes away: a second signal is delivered too
fn assert_forwarded_twice(sig: Signal) {
    let (mut scinit, pid) = Scinit::builder().child(["run"]).start();

    scinit.signal(sig).unwrap();
    scinit.wait_for_signal(pid, sig, TIMEOUT).unwrap();
    scinit.assert_running_for(Duration::from_millis(500));

    scinit.signal(sig).unwrap();
    scinit.wait_for_signal_count(pid, sig, 2, TIMEOUT).unwrap();
    scinit.assert_running_for(Duration::from_millis(500));
    assert!(
        scinit.events_named("exit").is_empty(),
        "child exited after {}\n{}",
        sig,
        scinit.diagnostics()
    );
}

/// Send `count` signals spaced out, checking each one arrives before the next
fn assert_repeated_delivery(scinit: &mut Scinit, pid: i32, sig: Signal, count: usize) {
    for i in 1..=count {
        scinit.signal(sig).unwrap();
        scinit.wait_for_signal_count(pid, sig, i, TIMEOUT).unwrap();
        std::thread::sleep(Duration::from_millis(300));
    }
    scinit.assert_running_for(Duration::from_millis(300));
}

#[test]
fn usr1_forwarded_to_child() {
    assert_forwarded_twice(Signal::SIGUSR1);
}

#[test]
fn usr2_forwarded_to_child() {
    assert_forwarded_twice(Signal::SIGUSR2);
}

#[test]
fn hup_forwarded_to_child() {
    assert_forwarded_twice(Signal::SIGHUP);
}

/// Forwarding targets the whole process group, so a grandchild in the
/// child's group receives the signal too
#[test]
fn usr1_forwarded_to_grandchild_in_group() {
    let (mut scinit, pid) = Scinit::builder().child(["run", "--grandchild"]).start();
    let grandchild = scinit
        .wait_for_nth_match("grandchild started", 1, TIMEOUT, |e| {
            e.is("started") && e.field_is("role", "grandchild")
        })
        .unwrap();
    assert_eq!(
        grandchild.pgid(),
        pid,
        "grandchild should share the child's process group"
    );

    scinit.signal(Signal::SIGUSR1).unwrap();
    scinit
        .wait_for_signal(grandchild.pid(), Signal::SIGUSR1, TIMEOUT)
        .unwrap();
    scinit
        .wait_for_signal(pid, Signal::SIGUSR1, TIMEOUT)
        .unwrap();
    scinit.assert_running_for(Duration::from_millis(300));
}

/// Signals keep being delivered after the default 5s zombie-reap tick has
/// fired (the tick cancels the pending sigwait branch of select!)
#[test]
fn repeated_usr1_after_reap_tick() {
    let (mut scinit, pid) = Scinit::builder().child(["run"]).start();
    std::thread::sleep(Duration::from_millis(5500));
    assert_repeated_delivery(&mut scinit, pid, Signal::SIGUSR1, 5);
}

/// A short reap interval cancels the sigwait branch many times over; every
/// signal must still arrive
#[test]
fn repeated_usr1_with_short_reap_interval() {
    let (mut scinit, pid) = Scinit::builder()
        .args(["--zombie-reap-interval-ms", "200"])
        .child(["run"])
        .start();
    std::thread::sleep(Duration::from_millis(1500));
    assert_repeated_delivery(&mut scinit, pid, Signal::SIGUSR1, 5);
}

/// With live-reload on, the main loop also polls file events; signals must
/// still be delivered after the reap tick
#[test]
fn repeated_usr1_with_live_reload() {
    let builder = Scinit::builder();
    let watched = builder.dir().join("watched.txt");
    std::fs::write(&watched, "v1").unwrap();
    let_setup_writes_age();
    let (mut scinit, pid) = builder.watch(&watched).child(["run"]).start();
    std::thread::sleep(Duration::from_millis(5500));
    assert_repeated_delivery(&mut scinit, pid, Signal::SIGUSR1, 5);
    assert_eq!(
        scinit.started_pids(),
        vec![pid],
        "no restart expected\n{}",
        scinit.diagnostics()
    );
}

/// SIGTERM is forwarded; the child exits and scinit follows promptly.
///
/// Guards against tokio's `child.wait()` not being woken on child exit
/// (it relies on tokio's SIGCHLD handler, so scinit must not consume SIGCHLD).
#[test]
fn sigterm_forwarded_then_scinit_exits() {
    let (mut scinit, pid) = Scinit::builder().child(["run"]).start();
    let start = Instant::now();
    scinit.signal(Signal::SIGTERM).unwrap();
    scinit
        .wait_for_signal(pid, Signal::SIGTERM, TIMEOUT)
        .unwrap();
    let status = scinit.wait_exit(EXIT_BOUND).unwrap();
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "scinit took {:?} to exit after SIGTERM",
        start.elapsed()
    );
    assert!(
        status.success(),
        "child exited 0 on TERM, scinit got {:?}\n{}",
        status,
        scinit.diagnostics()
    );
}

/// The final reap pass runs before scinit exits, both after a termination
/// signal and when the child exits on its own. Whether it reaps can't be
/// observed from outside (the zombies it would find are gone with scinit, or
/// its PID namespace), so this checks that it runs; the unit test in
/// src/reaper.rs checks that it reaps.
#[test]
fn final_reap_runs_on_every_exit_path() {
    const FINAL_PASS: &str = "Reaping any remaining zombie processes before exit";

    let (mut scinit, pid) = Scinit::builder()
        .env("SCINIT_LOG", "scinit::reaper=debug")
        .child(["run"])
        .start();
    scinit.signal(Signal::SIGTERM).unwrap();
    scinit
        .wait_for_signal(pid, Signal::SIGTERM, TIMEOUT)
        .unwrap();
    scinit.wait_exit(EXIT_BOUND).unwrap();
    assert!(
        scinit.stderr().contains(FINAL_PASS),
        "no final reap pass after SIGTERM\n{}",
        scinit.diagnostics()
    );

    let (scinit, _) = Scinit::builder()
        .env("SCINIT_LOG", "scinit::reaper=debug")
        .child(["exit", "0"])
        .run(TIMEOUT)
        .unwrap();
    assert!(
        scinit.stderr().contains(FINAL_PASS),
        "no final reap pass after the child exited\n{}",
        scinit.diagnostics()
    );
}

/// A child that ignores the termination signal is SIGKILLed once
/// --graceful-timeout-secs expires, not before. The 3s timeout is long enough
/// that escalating after any shorter fixed delay fails the timing check.
fn assert_escalates_to_sigkill(sig: Signal) {
    let (mut scinit, pid) = Scinit::builder()
        .args(["--graceful-timeout-secs", "3"])
        .child(["run", "--ignore", sig.as_str()])
        .start();
    let start = Instant::now();
    scinit.signal(sig).unwrap();
    scinit.wait_exit(EXIT_BOUND).unwrap();
    let elapsed = start.elapsed();
    assert!(
        wait_for_pid_gone(pid, Duration::from_secs(3)),
        "child {} survived {:?} escalation\n{}",
        pid,
        sig,
        scinit.diagnostics()
    );
    assert!(
        elapsed >= Duration::from_millis(2900) && elapsed <= Duration::from_secs(6),
        "{:?} escalation took {:?}, expected ~3s graceful timeout",
        sig,
        elapsed
    );
    assert!(
        scinit.events_named("exit").is_empty(),
        "child should not exit by itself"
    );
}

#[test]
fn sigterm_escalates_to_sigkill() {
    assert_escalates_to_sigkill(Signal::SIGTERM);
}

#[test]
fn sigint_escalates_to_sigkill() {
    assert_escalates_to_sigkill(Signal::SIGINT);
}

#[test]
fn sigquit_escalates_to_sigkill() {
    assert_escalates_to_sigkill(Signal::SIGQUIT);
}

/// A second termination signal during the graceful wait SIGKILLs the child
/// at once instead of waiting out the timeout
fn assert_second_signal_kills(first: Signal, second: Signal) {
    // Traps the termination signals without exiting on them
    let (mut scinit, pid) = Scinit::builder()
        .args(["--graceful-timeout-secs", "30"])
        .child(["run", "--exit-on", "USR2"])
        .start();
    scinit.signal(first).unwrap();
    scinit.wait_for_signal(pid, first, TIMEOUT).unwrap();
    let start = Instant::now();
    scinit.signal(second).unwrap();
    let status = scinit.wait_exit(EXIT_BOUND).unwrap();

    assert!(
        start.elapsed() < Duration::from_secs(3),
        "{:?} after {:?} took {:?} to stop the child, expected an immediate SIGKILL",
        second,
        first,
        start.elapsed()
    );
    assert!(
        wait_for_pid_gone(pid, Duration::from_secs(3)),
        "child {} survived the SIGKILL\n{}",
        pid,
        scinit.diagnostics()
    );
    scinit.assert_exit_code(status, 137);
}

#[test]
fn second_sigterm_kills_at_once() {
    assert_second_signal_kills(Signal::SIGTERM, Signal::SIGTERM);
}

#[test]
fn sigint_after_sigterm_kills_at_once() {
    assert_second_signal_kills(Signal::SIGTERM, Signal::SIGINT);
}

/// SIGUSR1 during the graceful wait is forwarded to the stopping child
/// rather than dropped, and doesn't end the wait
#[test]
fn usr1_forwarded_during_graceful_shutdown() {
    let (mut scinit, pid) = Scinit::builder()
        .args(["--graceful-timeout-secs", "30"])
        .child(["run", "--exit-on", "USR2"])
        .start();
    scinit.signal(Signal::SIGTERM).unwrap();
    scinit
        .wait_for_signal(pid, Signal::SIGTERM, TIMEOUT)
        .unwrap();
    scinit.signal(Signal::SIGUSR1).unwrap();
    scinit
        .wait_for_signal(pid, Signal::SIGUSR1, TIMEOUT)
        .unwrap();
    assert!(
        scinit.is_running(),
        "SIGUSR1 must not end the graceful wait\n{}",
        scinit.diagnostics()
    );
    // Lets the child exit by itself
    scinit.signal(Signal::SIGUSR2).unwrap();
    let status = scinit.wait_exit(EXIT_BOUND).unwrap();
    scinit.assert_exit_code(status, 0);
}

fn assert_termination_forwarded(sig: Signal) {
    let (mut scinit, pid) = Scinit::builder().child(["run"]).start();
    scinit.signal(sig).unwrap();
    scinit.wait_for_signal(pid, sig, TIMEOUT).unwrap();
    scinit.wait_exit(EXIT_BOUND).unwrap();
    assert!(
        wait_for_pid_gone(pid, Duration::from_secs(3)),
        "child {} still alive",
        pid
    );
}

#[test]
fn sigint_forwarded_then_scinit_exits() {
    assert_termination_forwarded(Signal::SIGINT);
}

#[test]
fn sigquit_forwarded_then_scinit_exits() {
    assert_termination_forwarded(Signal::SIGQUIT);
}

/// When the child exits promptly on SIGINT/SIGQUIT, scinit should not wait
/// out a fixed delay
fn assert_prompt_exit(sig: Signal) {
    let (mut scinit, pid) = Scinit::builder().child(["run"]).start();
    let start = Instant::now();
    scinit.signal(sig).unwrap();
    scinit.wait_for_signal(pid, sig, TIMEOUT).unwrap();
    scinit.wait_exit(EXIT_BOUND).unwrap();
    assert!(
        start.elapsed() < Duration::from_millis(1500),
        "scinit took {:?} to exit after {} though the child exited at once",
        start.elapsed(),
        sig
    );
}

// scinit must not linger once the child has exited
#[test]
fn sigint_exits_promptly_when_child_exits() {
    assert_prompt_exit(Signal::SIGINT);
}

#[test]
fn sigquit_exits_promptly_when_child_exits() {
    assert_prompt_exit(Signal::SIGQUIT);
}

/// The child starts with every signal at its default disposition: not the
/// SIGTTIN/SIGTTOU scinit ignores for itself, nor the SIGINT/SIGQUIT scinit
/// was started with ignored
#[test]
fn child_starts_with_default_dispositions() {
    let (scinit, events) = Scinit::builder()
        .ignore_signals(&[Signal::SIGINT, Signal::SIGQUIT])
        .spawn_dump(&[]);
    for sig in ["TTIN", "TTOU", "INT", "QUIT"] {
        let disp = events
            .iter()
            .find(|e| e.is("sigdisp") && e.field_is("sig", sig))
            .unwrap_or_else(|| panic!("no sigdisp for {}\n{}", sig, scinit.diagnostics()));
        assert_eq!(
            disp.get("ignored"),
            Some("false"),
            "{} is ignored in the child\n{}",
            sig,
            scinit.diagnostics()
        );
    }
}

/// With a terminal, the child's group holds its foreground when the child
/// starts. The child takes it itself before exec: with SIGTTIN/SIGTTOU at
/// their defaults, a child still in the background would be stopped by
/// reading or configuring the terminal. (Handing it over from scinit after
/// the spawn usually won that race too, so this checks the handoff works,
/// not that the race is gone.)
#[test]
fn child_starts_in_terminal_foreground() {
    let (scinit, events) = Scinit::builder().controlling_terminal().spawn_dump(&[]);
    let tty = events
        .iter()
        .find(|e| e.is("tty"))
        .unwrap_or_else(|| panic!("no tty event\n{}", scinit.diagnostics()));
    assert_eq!(
        tty.get("foreground"),
        Some("true"),
        "the child started outside the terminal's foreground\n{}",
        scinit.diagnostics()
    );
}

/// scinit started with SIGINT and SIGQUIT ignored (`scinit ... &` from a
/// script) still receives them and forwards them to the child
fn assert_received_though_ignored(sig: Signal) {
    let (mut scinit, pid) = Scinit::builder()
        .ignore_signals(&[Signal::SIGINT, Signal::SIGQUIT])
        .child(["run"])
        .start();
    scinit.signal(sig).unwrap();
    scinit.wait_for_signal(pid, sig, TIMEOUT).unwrap();
    let status = scinit.wait_exit(EXIT_BOUND).unwrap();
    scinit.assert_exit_code(status, 0);
}

#[test]
fn sigint_received_though_started_ignored() {
    assert_received_though_ignored(Signal::SIGINT);
}

#[test]
fn sigquit_received_though_started_ignored() {
    assert_received_though_ignored(Signal::SIGQUIT);
}

#[test]
fn child_signal_mask_is_empty() {
    let (scinit, events) = Scinit::builder().spawn_dump(&[]);
    let mask = events
        .iter()
        .find(|e| e.is("sigmask"))
        .unwrap_or_else(|| panic!("no sigmask event\n{}", scinit.diagnostics()));
    assert_eq!(mask.get("blocked"), Some(""), "{}", scinit.diagnostics());
}
