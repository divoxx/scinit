//! Live-reload: file changes restart the child; nothing else does.
//!
//! Watching is non-recursive on a single `--watch-path` (file or directory).
//! Only content changes and renames of regular files count, not
//! metadata-only changes. Debouncing is trailing-edge: the restart fires once
//! changes have been quiet for `--debounce-ms`. A restart is SIGTERM to the
//! child's group, a `--restart-delay-ms` pause, then a fresh spawn that gets
//! the same listening sockets.
//!
//! macOS FSEvents has delivery latency and can hand the watcher events from
//! just before it started, so every test waits for the child to start and
//! then settles briefly before touching the filesystem.
//!
//! Most tests pass `--zombie-reap-interval-ms 100`;
//! `restart_is_prompt_with_default_reap_interval` covers the default.

use crate::integration::harness::{
    free_port, let_setup_writes_age, loopback, request, Scinit, ScinitBuilder, TEST_CHILD, TIMEOUT,
};
use nix::sys::signal::Signal;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Time to let FSEvents settle after the child starts, before touching files
const SETTLE: Duration = Duration::from_millis(500);

/// How long to watch for a restart that must not happen
const QUIET: Duration = Duration::from_secs(3);

/// Live-reload flags with short delays so restarts are quick
fn live_reload(builder: ScinitBuilder, watch: &Path) -> ScinitBuilder {
    live_reload_with(builder, watch, 200, 100)
}

fn live_reload_with(
    builder: ScinitBuilder,
    watch: &Path,
    debounce_ms: u64,
    restart_delay_ms: u64,
) -> ScinitBuilder {
    let_setup_writes_age();
    builder
        .watch(watch)
        .args(["--debounce-ms", &debounce_ms.to_string()])
        .args(["--restart-delay-ms", &restart_delay_ms.to_string()])
        .args(["--zombie-reap-interval-ms", "100"])
}

/// Live-reload with scinit's default zombie-reap interval
fn live_reload_default_reap(builder: ScinitBuilder, watch: &Path) -> ScinitBuilder {
    let_setup_writes_age();
    builder
        .watch(watch)
        .args(["--debounce-ms", "200", "--restart-delay-ms", "100"])
}

/// A directory to watch, separate from the temp dir root where the child's
/// report file lives (report writes would otherwise trigger restarts)
fn watched_dir(builder: &ScinitBuilder) -> PathBuf {
    let dir = builder.dir().join("watched");
    std::fs::create_dir(&dir).unwrap();
    dir
}

/// Start scinit and wait for the first child, then let pre-start filesystem
/// events drain
fn start_and_settle(builder: ScinitBuilder) -> (Scinit, i32) {
    let started = builder.start();
    std::thread::sleep(SETTLE);
    started
}

/// Write to an existing file in place (a plain content modification)
fn modify(path: &Path, content: &str) {
    std::fs::write(path, content).unwrap();
}

/// Assert no restart happens within `QUIET`
fn assert_no_restart(scinit: &mut Scinit, what: &str) {
    scinit.assert_running_for(QUIET);
    scinit.assert_start_count(1, &format!("{} must not restart the child", what));
}

/// Modifying a watched file stops the old child with SIGTERM and starts a new one
#[test]
fn file_modify_restarts_child() {
    let b = Scinit::builder();
    let dir = watched_dir(&b);
    let file = dir.join("app.conf");
    modify(&file, "v1");
    let (mut scinit, old) = start_and_settle(live_reload(b, &dir).child(["run"]));
    modify(&file, "v2");

    let new = scinit.wait_for_nth("started", 2, TIMEOUT).unwrap().pid();
    assert_ne!(old, new, "restart must spawn a new process");
    scinit
        .wait_for_signal(old, Signal::SIGTERM, TIMEOUT)
        .unwrap();
    assert!(
        scinit.is_running(),
        "scinit must survive the restart\n{}",
        scinit.diagnostics()
    );
}

/// Watching a single file (rather than a directory) also triggers restarts
#[test]
fn watch_single_file_restarts_child() {
    let b = Scinit::builder();
    let file = watched_dir(&b).join("app.conf");
    modify(&file, "v1");
    let (scinit, old) = start_and_settle(live_reload(b, &file).child(["run"]));
    modify(&file, "v2");

    let new = scinit.wait_for_nth("started", 2, TIMEOUT).unwrap().pid();
    assert_ne!(old, new);
}

/// The new child starts no sooner than `--restart-delay-ms` after the old one exits
#[test]
fn restart_delay_is_respected() {
    const DELAY: Duration = Duration::from_millis(800);
    let b = Scinit::builder();
    let dir = watched_dir(&b);
    let file = dir.join("app.conf");
    modify(&file, "v1");
    let (scinit, old) =
        start_and_settle(live_reload_with(b, &dir, 200, DELAY.as_millis() as u64).child(["run"]));
    modify(&file, "v2");

    let exited = scinit
        .wait_for_nth_match("old child exit", 1, TIMEOUT, |e| {
            e.is("exit") && e.pid() == old
        })
        .unwrap();
    let started = scinit.wait_for_nth("started", 2, TIMEOUT).unwrap();
    // From the events' own timestamps, so polling delays don't skew it
    let gap = started.time().saturating_sub(exited.time());

    assert!(
        gap >= DELAY,
        "new child started {:?} after the old one exited, expected >= {:?}\n{}",
        gap,
        DELAY,
        scinit.diagnostics()
    );
    assert!(
        gap < DELAY + Duration::from_secs(3),
        "restart took {:?}, far beyond the {:?} delay\n{}",
        gap,
        DELAY,
        scinit.diagnostics()
    );
}

/// A termination signal during the restart delay cancels the restart: no new
/// child is started (it would get the forwarded signal before it could
/// handle it), and scinit exits with the old child's status
#[test]
fn sigterm_during_restart_delay_cancels_restart() {
    let b = Scinit::builder();
    let dir = watched_dir(&b);
    let file = dir.join("app.conf");
    modify(&file, "v1");
    let (mut scinit, old) = start_and_settle(live_reload_with(b, &dir, 200, 3000).child(["run"]));
    modify(&file, "v2");

    scinit
        .wait_for_nth_match("old child exit", 1, TIMEOUT, |e| {
            e.is("exit") && e.pid() == old
        })
        .unwrap();
    scinit.signal(Signal::SIGTERM).unwrap();
    let status = scinit.wait_exit(TIMEOUT).unwrap();

    scinit.assert_start_count(1, "a cancelled restart must not start a child");
    scinit.assert_exit_code(status, 0);
}

/// A termination signal while the old child is still stopping cancels the
/// restart too: it is forwarded to the old child, and scinit exits with that
/// child's status
#[test]
fn sigint_while_old_child_stops_cancels_restart() {
    let b = Scinit::builder();
    let dir = watched_dir(&b);
    let file = dir.join("app.conf");
    modify(&file, "v1");
    // Traps SIGTERM without exiting, so the restart's stop keeps waiting
    let (mut scinit, old) = start_and_settle(
        live_reload(b, &dir)
            .args(["--restart-timeout-secs", "30"])
            .child(["run", "--exit-on", "SIGINT"]),
    );
    modify(&file, "v2");

    scinit
        .wait_for_signal(old, Signal::SIGTERM, TIMEOUT)
        .unwrap();
    scinit.signal(Signal::SIGINT).unwrap();
    scinit
        .wait_for_signal(old, Signal::SIGINT, TIMEOUT)
        .unwrap();
    let status = scinit.wait_exit(TIMEOUT).unwrap();

    scinit.assert_start_count(1, "a cancelled restart must not start a child");
    scinit.assert_exit_code(status, 0);
}

/// A restart SIGKILLs an old child that outlives `--restart-timeout-secs`,
/// while a termination signal still waits for `--graceful-timeout-secs`
#[test]
fn restart_timeout_applies_to_restarts_only() {
    const RESTART_TIMEOUT: Duration = Duration::from_secs(1);
    const GRACEFUL_TIMEOUT: Duration = Duration::from_secs(4);
    let b = Scinit::builder();
    let dir = watched_dir(&b);
    let file = dir.join("app.conf");
    modify(&file, "v1");
    // Traps SIGTERM without exiting, so only SIGKILL stops it
    let (mut scinit, old) = start_and_settle(
        live_reload(b, &dir)
            .args([
                "--restart-timeout-secs",
                "1",
                "--graceful-timeout-secs",
                "4",
            ])
            .child(["run", "--exit-on", "SIGINT"]),
    );
    modify(&file, "v2");

    let sigterm = scinit
        .wait_for_signal(old, Signal::SIGTERM, TIMEOUT)
        .unwrap();
    let started = scinit.wait_for_nth("started", 2, TIMEOUT).unwrap();
    // From the events' own timestamps, so polling delays don't skew it
    let gap = started.time().saturating_sub(sigterm.time());
    assert!(
        gap >= RESTART_TIMEOUT && gap < GRACEFUL_TIMEOUT - Duration::from_millis(500),
        "new child started {:?} after SIGTERM, expected ~{:?} restart timeout\n{}",
        gap,
        RESTART_TIMEOUT,
        scinit.diagnostics()
    );
    assert!(
        !scinit.events_named("exit").iter().any(|e| e.pid() == old),
        "the old child must be killed, not exit by itself\n{}",
        scinit.diagnostics()
    );

    let new = started.pid();
    let start = Instant::now();
    scinit.signal(Signal::SIGTERM).unwrap();
    scinit
        .wait_for_signal(new, Signal::SIGTERM, TIMEOUT)
        .unwrap();
    let status = scinit.wait_exit(TIMEOUT).unwrap();
    let elapsed = start.elapsed();
    assert!(
        elapsed >= GRACEFUL_TIMEOUT - Duration::from_millis(100),
        "shutdown took {:?}, expected the {:?} graceful timeout\n{}",
        elapsed,
        GRACEFUL_TIMEOUT,
        scinit.diagnostics()
    );
    scinit.assert_exit_code(status, 137);
}

/// A burst of writes inside the debounce window causes exactly one restart
#[test]
fn burst_of_writes_restarts_once() {
    let b = Scinit::builder();
    let dir = watched_dir(&b);
    let file = dir.join("app.conf");
    modify(&file, "v0");
    let (mut scinit, _) = start_and_settle(live_reload_with(b, &dir, 1500, 100).child(["run"]));
    for i in 1..=5 {
        modify(&file, &format!("v{}", i));
        std::thread::sleep(Duration::from_millis(50));
    }

    scinit.wait_for_nth("started", 2, TIMEOUT).unwrap();
    // Well past debounce + restart delay: any second restart would show up
    scinit.assert_running_for(QUIET);
    scinit.assert_start_count(
        2,
        "a burst within the debounce window must restart exactly once",
    );
}

/// A change made inside the debounce window that follows a restart must still
/// be picked up once the window ends; otherwise the app keeps running with
/// the stale file
#[test]
fn change_within_debounce_window_is_not_lost() {
    let b = Scinit::builder();
    let dir = watched_dir(&b);
    let file = dir.join("app.conf");
    modify(&file, "v0");
    let (scinit, _) = start_and_settle(live_reload_with(b, &dir, 1000, 100).child(["run"]));
    modify(&file, "v1");
    scinit.wait_for_nth("started", 2, TIMEOUT).unwrap();
    // Well inside the 1000ms window opened by the v1 change
    modify(&file, "v2");
    scinit
        .wait_for_nth("started", 3, Duration::from_secs(5))
        .unwrap_or_else(|e| panic!("the v2 change never caused a restart: {}", e));
}

/// Writes spaced further apart than the debounce window each restart the child
#[test]
fn writes_outside_debounce_window_each_restart() {
    let b = Scinit::builder();
    let dir = watched_dir(&b);
    let file = dir.join("app.conf");
    modify(&file, "v0");
    let (scinit, _) = start_and_settle(live_reload(b, &dir).child(["run"]));
    modify(&file, "v1");
    scinit.wait_for_nth("started", 2, TIMEOUT).unwrap();
    std::thread::sleep(SETTLE);
    modify(&file, "v2");
    scinit.wait_for_nth("started", 3, TIMEOUT).unwrap();

    let pids = scinit.started_pids();
    let mut unique = pids.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(
        unique.len(),
        pids.len(),
        "each restart must be a new process: {:?}",
        pids
    );
}

/// Creating a new (empty) file in the watched directory is not a modification.
/// On macOS creation also emits `Modify(Metadata(Extended))` (xattrs), which
/// `is_relevant_change` ignores as metadata-only.
#[test]
fn creating_file_does_not_restart() {
    let b = Scinit::builder();
    let dir = watched_dir(&b);
    let (mut scinit, _) = start_and_settle(live_reload(b, &dir).child(["run"]));
    std::fs::File::create(dir.join("new.conf")).unwrap();

    assert_no_restart(&mut scinit, "creating a file");
}

/// Deleting a file in the watched directory does not restart the child
#[test]
fn deleting_file_does_not_restart() {
    let b = Scinit::builder();
    let dir = watched_dir(&b);
    let file = dir.join("app.conf");
    modify(&file, "v1");
    // Let the creation event age out before the watcher starts
    std::thread::sleep(SETTLE);
    let (mut scinit, _) = start_and_settle(live_reload(b, &dir).child(["run"]));
    std::fs::remove_file(&file).unwrap();

    assert_no_restart(&mut scinit, "deleting a file");
}

/// The watch is non-recursive: edits inside a subdirectory are ignored
#[test]
fn modifying_file_in_subdirectory_does_not_restart() {
    let b = Scinit::builder();
    let dir = watched_dir(&b);
    let sub = dir.join("sub");
    std::fs::create_dir(&sub).unwrap();
    let file = sub.join("nested.conf");
    modify(&file, "v1");
    std::thread::sleep(SETTLE);
    let (mut scinit, _) = start_and_settle(live_reload(b, &dir).child(["run"]));
    modify(&file, "v2");

    assert_no_restart(&mut scinit, "modifying a file in a subdirectory");
}

/// Only file changes restart: a child that exits on its own ends scinit
#[test]
fn child_exit_is_not_restarted() {
    let b = Scinit::builder();
    let dir = watched_dir(&b);
    let (scinit, status) = live_reload_default_reap(b, &dir)
        .child(["exit", "3"])
        .run(TIMEOUT)
        .unwrap();

    scinit.assert_start_count(1, "an exited child must not be restarted");
    scinit.assert_exit_code(status, 3);
}

/// A child killed by a signal under live-reload is not restarted either
#[test]
fn child_crash_is_not_restarted() {
    let b = Scinit::builder();
    let dir = watched_dir(&b);
    let (scinit, status) = live_reload_default_reap(b, &dir)
        .child(["kill-self", "SEGV"])
        .run(TIMEOUT)
        .unwrap();

    scinit.assert_start_count(1, "a crashed child must not be restarted");
    scinit.assert_exit_code(status, 139);
}

/// scinit exits without restarting even with a zombie reaper tight enough to
/// race tokio's `child.wait()` for the child's exit status
#[test]
fn child_exit_with_fast_reaper_is_not_restarted_and_exits() {
    // The race is timing-dependent; a few attempts with a tight reap timer
    // make losing it near-certain
    for _ in 0..5 {
        let b = Scinit::builder();
        let dir = watched_dir(&b);
        let (scinit, _status) = live_reload_default_reap(b, &dir)
            .args(["--zombie-reap-interval-ms", "10"])
            .child(["exit", "3"])
            .run(Duration::from_secs(3))
            .unwrap();
        scinit.assert_start_count(1, "an exited child must not be restarted");
    }
}

/// Keep connecting to `addr` until `stop` is set and return every result.
/// Each connection runs on its own thread so connections queued in the
/// backlog overlap.
fn hammer(addr: &str, stop: &AtomicBool) -> Vec<Result<String, String>> {
    let mut attempts = Vec::new();
    while !stop.load(Ordering::SeqCst) {
        let addr = addr.to_string();
        attempts.push(std::thread::spawn(move || {
            request(&addr).map_err(|e| e.to_string())
        }));
        std::thread::sleep(Duration::from_millis(20));
    }
    attempts.into_iter().map(|t| t.join().unwrap()).collect()
}

/// No connection is refused or reset across a restart. Connections that
/// arrive while no child is running must wait in the listener's backlog and
/// be served by the new child, which requires passing it the same socket.
#[test]
fn restart_drops_no_connections() {
    let port = free_port();
    let addr = loopback(port);
    let b = Scinit::builder();
    let dir = watched_dir(&b);
    let file = dir.join("app.conf");
    modify(&file, "v1");
    // A long restart delay widens the window with no child accepting
    let scinit = live_reload_with(b, &dir, 200, 500)
        .ports(&[port])
        .child(["listen"])
        .spawn()
        .unwrap();
    scinit.wait_for_event("ready", TIMEOUT).unwrap();
    std::thread::sleep(SETTLE);

    // Keep connecting throughout the restart
    let stop = Arc::new(AtomicBool::new(false));
    let client = {
        let stop = stop.clone();
        std::thread::spawn(move || hammer(&addr, &stop))
    };

    modify(&file, "v2");
    scinit.wait_for_nth("ready", 2, TIMEOUT).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    stop.store(true, Ordering::SeqCst);
    let results = client.join().unwrap();

    let failures: Vec<&String> = results.iter().filter_map(|r| r.as_ref().err()).collect();
    assert!(
        failures.is_empty(),
        "{} of {} connections failed across the restart: {:?}\n{}",
        failures.len(),
        results.len(),
        failures,
        scinit.diagnostics()
    );
}

/// A listening child answers on the same port after a restart, from the new pid
#[test]
fn listener_survives_restart() {
    let port = free_port();
    let addr = loopback(port);
    let b = Scinit::builder();
    let dir = watched_dir(&b);
    let file = dir.join("app.conf");
    modify(&file, "v1");
    let scinit = live_reload(b, &dir)
        .ports(&[port])
        .child(["listen"])
        .spawn()
        .unwrap();

    scinit.wait_for_event("ready", TIMEOUT).unwrap();
    let old = scinit.child_pid().unwrap();
    scinit.assert_reply_from(&addr, old);

    std::thread::sleep(SETTLE);
    modify(&file, "v2");
    scinit.wait_for_nth("ready", 2, TIMEOUT).unwrap();
    let new = scinit.child_pid().unwrap();
    assert_ne!(old, new);

    let reply = scinit.assert_reply_from(&addr, new);
    assert!(
        reply.starts_with(&format!("pid={} fd=3 ", new)),
        "after restart, reply {:?} should come from new pid {} on fd 3\n{}",
        reply,
        new,
        scinit.diagnostics()
    );
}

/// A file change is acted on promptly, without waiting for the zombie-reap timer
#[test]
fn restart_is_prompt_with_default_reap_interval() {
    let b = Scinit::builder();
    let dir = watched_dir(&b);
    let file = dir.join("app.conf");
    modify(&file, "v1");
    // Default --zombie-reap-interval-ms (5000)
    let (scinit, _) = start_and_settle(
        b.watch(&dir)
            .args(["--debounce-ms", "200", "--restart-delay-ms", "100"])
            .child(["run"]),
    );
    let modified = Instant::now();
    modify(&file, "v2");
    scinit.wait_for_nth("started", 2, TIMEOUT).unwrap();
    let latency = modified.elapsed();

    assert!(
        latency < Duration::from_millis(2500),
        "restart took {:?} after the file change (debounce 200ms, delay 100ms)\n{}",
        latency,
        scinit.diagnostics()
    );
}

/// A `--watch-path` that does not exist is a startup error
#[test]
fn nonexistent_watch_path_exits_1() {
    let b = Scinit::builder();
    let missing = b.dir().join("does-not-exist");
    let (scinit, status) = live_reload(b, &missing)
        .child(["run"])
        .run(TIMEOUT)
        .unwrap();

    scinit.assert_exit_code(status, 1);
    scinit.assert_start_count(0, "no child should start when the watch fails");
}

/// Without `--watch-path`, the watch path defaults to the command string. An
/// absolute command path is therefore watchable and scinit runs normally.
#[test]
fn default_watch_path_is_absolute_command() {
    let (mut scinit, _) = Scinit::builder()
        .args(["--live-reload", "--zombie-reap-interval-ms", "100"])
        .child(["run"])
        .start();

    scinit.assert_running_for(Duration::from_secs(1));
    scinit.assert_start_count(1, "the child must keep running");
}

/// Without `--watch-path`, a bare command name is looked up in PATH, like
/// exec does, and the executable found there is watched.
#[test]
fn default_watch_path_bare_command_is_resolved_via_path() {
    let path = path_with(Path::new(TEST_CHILD).parent().unwrap());
    let (mut scinit, _) = Scinit::builder()
        .env("PATH", path.to_str().unwrap())
        // SCINIT_LOG once scinit stops reading RUST_LOG (#2); both until then
        .env("SCINIT_LOG", "info")
        .env("RUST_LOG", "info")
        .args(["--live-reload", "--zombie-reap-interval-ms", "100"])
        .command(["scinit-test-child", "run"])
        .start();

    scinit.assert_running_for(Duration::from_secs(1));
    scinit.assert_start_count(1, "the child must keep running");
    let watched = format!("Started watching path: {:?}", Path::new(TEST_CHILD));
    assert!(
        scinit.stdout().contains(&watched) || scinit.stderr().contains(&watched),
        "expected log line {:?}\n{}",
        watched,
        scinit.diagnostics()
    );
}

/// A bare command that isn't in PATH can't be watched: scinit fails at
/// startup with exit 1, naming the command and pointing at `--watch-path`.
#[test]
fn default_watch_path_unresolvable_command_exits_1() {
    let (scinit, status) = Scinit::builder()
        .args(["--live-reload"])
        .command(["scinit-definitely-not-a-command"])
        .run(TIMEOUT)
        .unwrap();

    scinit.assert_exit_code(status, 1);
    scinit.assert_start_count(0, "nothing can be spawned");
    let stderr = scinit.stderr();
    assert!(
        stderr.contains("scinit-definitely-not-a-command") && stderr.contains("--watch-path"),
        "expected an error naming the command and --watch-path\n{}",
        scinit.diagnostics()
    );
}

/// `$PATH` with `dir` prepended
fn path_with(dir: &Path) -> OsString {
    match std::env::var_os("PATH") {
        Some(p) => {
            let mut dirs = vec![dir.to_path_buf()];
            dirs.extend(std::env::split_paths(&p));
            std::env::join_paths(dirs).unwrap()
        }
        None => dir.as_os_str().to_owned(),
    }
}
