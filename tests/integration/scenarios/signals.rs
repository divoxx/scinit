//! Signal handling: forwarding to the child's process group, termination and
//! SIGKILL escalation, and the signal state the child inherits.

use crate::integration::harness::{wait_for_pid_gone, Event, Scinit, TIMEOUT};
use nix::sys::signal::Signal;
use std::time::{Duration, Instant};

/// Generous upper bound for scinit to exit after a termination signal
const EXIT_BOUND: Duration = Duration::from_secs(10);

/// Start scinit with `scinit_args` and the fixture `run` subcommand, and wait
/// for the child to come up
fn start_run(scinit_args: &[&str], run_args: &[&str]) -> (Scinit, i32) {
    let mut child = vec!["run"];
    child.extend_from_slice(run_args);
    let scinit = Scinit::builder()
        .args(scinit_args.iter().copied())
        .child(child)
        .spawn()
        .unwrap();
    let pid = scinit.child_pid().unwrap();
    (scinit, pid)
}

fn short(sig: Signal) -> &'static str {
    sig.as_str().trim_start_matches("SIG")
}

/// A forwarded signal reaches a trapping child, and neither scinit nor the
/// child goes away: a second signal is delivered too
fn assert_forwarded_twice(sig: Signal) {
    let (mut scinit, pid) = start_run(&[], &[]);
    let name = short(sig);

    scinit.signal(sig).unwrap();
    scinit.wait_for_signal(pid, name, TIMEOUT).unwrap();
    scinit.assert_running_for(Duration::from_millis(500));

    scinit.signal(sig).unwrap();
    scinit.wait_for_signal_count(pid, name, 2, TIMEOUT).unwrap();
    scinit.assert_running_for(Duration::from_millis(500));
    assert!(
        scinit.events_named("exit").is_empty(),
        "child exited after {}\n{}",
        name,
        scinit.diagnostics()
    );
}

/// Send `count` signals spaced out, checking each one arrives before the next
fn assert_repeated_delivery(scinit: &mut Scinit, pid: i32, sig: Signal, count: usize) {
    let name = short(sig);
    for i in 1..=count {
        scinit.signal(sig).unwrap();
        scinit.wait_for_signal_count(pid, name, i, TIMEOUT).unwrap();
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
    let (mut scinit, pid) = start_run(&[], &["--grandchild"]);
    let grandchild = scinit
        .wait_for("grandchild started", TIMEOUT, |evs| {
            evs.iter()
                .any(|e| e.name == "started" && e.get("role") == Some("grandchild"))
        })
        .unwrap()
        .into_iter()
        .find(|e| e.name == "started" && e.get("role") == Some("grandchild"))
        .unwrap();
    assert_eq!(
        grandchild.get("pgid"),
        Some(pid.to_string().as_str()),
        "grandchild should share the child's process group"
    );

    scinit.signal(Signal::SIGUSR1).unwrap();
    scinit
        .wait_for_signal(grandchild.pid(), "USR1", TIMEOUT)
        .unwrap();
    scinit.wait_for_signal(pid, "USR1", TIMEOUT).unwrap();
    scinit.assert_running_for(Duration::from_millis(300));
}

/// Signals keep being delivered after the default 5s zombie-reap tick has
/// fired (the tick cancels the pending sigwait branch of select!)
#[test]
fn repeated_usr1_after_reap_tick() {
    let (mut scinit, pid) = start_run(&[], &[]);
    std::thread::sleep(Duration::from_millis(5500));
    assert_repeated_delivery(&mut scinit, pid, Signal::SIGUSR1, 5);
}

/// A short reap interval cancels the sigwait branch many times over; every
/// signal must still arrive
#[test]
fn repeated_usr1_with_short_reap_interval() {
    let (mut scinit, pid) = start_run(&["--zombie-reap-interval-ms", "200"], &[]);
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
    // FSEvents can report a write made just before the watcher starts as a
    // change, causing a spurious restart; let it age first
    std::thread::sleep(Duration::from_secs(1));
    let mut scinit = builder
        .args(["--live-reload", "--watch-path"])
        .args([&watched])
        .child(["run"])
        .spawn()
        .unwrap();
    let pid = scinit.child_pid().unwrap();
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
    let (mut scinit, pid) = start_run(&[], &[]);
    let start = Instant::now();
    scinit.signal(Signal::SIGTERM).unwrap();
    scinit.wait_for_signal(pid, "TERM", TIMEOUT).unwrap();
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

/// A child that ignores SIGTERM is SIGKILLed after --graceful-timeout-secs
#[test]
fn sigterm_escalates_to_sigkill() {
    let (mut scinit, pid) = start_run(&["--graceful-timeout-secs", "1"], &["--ignore", "TERM"]);
    let start = Instant::now();
    scinit.signal(Signal::SIGTERM).unwrap();
    scinit.wait_exit(EXIT_BOUND).unwrap();
    let elapsed = start.elapsed();
    assert!(
        wait_for_pid_gone(pid, Duration::from_secs(3)),
        "child {} survived SIGTERM escalation\n{}",
        pid,
        scinit.diagnostics()
    );
    assert!(
        elapsed >= Duration::from_millis(900) && elapsed <= Duration::from_secs(4),
        "escalation took {:?}, expected ~1s graceful timeout",
        elapsed
    );
    assert!(scinit.events_named("exit").is_empty(), "child should not exit by itself");
}

fn assert_termination_forwarded(sig: Signal) {
    let (mut scinit, pid) = start_run(&[], &[]);
    scinit.signal(sig).unwrap();
    scinit.wait_for_signal(pid, short(sig), TIMEOUT).unwrap();
    scinit.wait_exit(EXIT_BOUND).unwrap();
    assert!(wait_for_pid_gone(pid, Duration::from_secs(3)), "child {} still alive", pid);
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
    let (mut scinit, pid) = start_run(&[], &[]);
    let start = Instant::now();
    scinit.signal(sig).unwrap();
    scinit.wait_for_signal(pid, short(sig), TIMEOUT).unwrap();
    scinit.wait_exit(EXIT_BOUND).unwrap();
    assert!(
        start.elapsed() < Duration::from_millis(1500),
        "scinit took {:?} to exit after {} though the child exited at once",
        start.elapsed(),
        short(sig)
    );
}

// scinit currently waits a fixed 2s after SIGINT/SIGQUIT (measured ~2.1s)
#[test]
#[ignore = "bug: sigint-fixed-delay (KNOWN-ISSUES.md)"]
fn sigint_exits_promptly_when_child_exits() {
    assert_prompt_exit(Signal::SIGINT);
}

#[test]
#[ignore = "bug: sigint-fixed-delay (KNOWN-ISSUES.md)"]
fn sigquit_exits_promptly_when_child_exits() {
    assert_prompt_exit(Signal::SIGQUIT);
}

fn dump_events() -> (Scinit, Vec<Event>) {
    let scinit = Scinit::builder().child(["dump"]).spawn().unwrap();
    let events = scinit
        .wait_for("dump-done", TIMEOUT, |evs| evs.iter().any(|e| e.name == "dump-done"))
        .unwrap();
    (scinit, events)
}

/// scinit ignores TTIN/TTOU and the ignored disposition survives exec
/// (current intended behaviour for container use)
#[test]
fn child_inherits_ignored_ttin_ttou() {
    let (scinit, events) = dump_events();
    for sig in ["TTIN", "TTOU"] {
        let disp = events
            .iter()
            .find(|e| e.name == "sigdisp" && e.get("sig") == Some(sig))
            .unwrap_or_else(|| panic!("no sigdisp for {}\n{}", sig, scinit.diagnostics()));
        assert_eq!(disp.get("ignored"), Some("true"), "{}\n{}", sig, scinit.diagnostics());
    }
}

/// scinit blocks signals for sigwait, but the child must start with an empty
/// signal mask
#[test]
fn child_signal_mask_is_empty() {
    let (scinit, events) = dump_events();
    let mask = events
        .iter()
        .find(|e| e.name == "sigmask")
        .unwrap_or_else(|| panic!("no sigmask event\n{}", scinit.diagnostics()));
    assert_eq!(mask.get("blocked"), Some(""), "{}", scinit.diagnostics());
}
