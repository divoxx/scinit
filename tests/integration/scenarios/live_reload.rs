//! Live-reload: file changes restart the child; nothing else does.
//!
//! Watching is non-recursive on a single `--watch-path` (file or directory),
//! only modifications of existing regular files count, and debouncing is
//! leading-edge. A restart is SIGTERM to the child's group, a
//! `--restart-delay-ms` pause, then a fresh spawn (ports are rebound).
//!
//! macOS FSEvents has delivery latency and can hand the watcher events from
//! just before it started, so every test waits for the child to start and
//! then settles briefly before touching the filesystem.
//!
//! Most tests pass `--zombie-reap-interval-ms 100`: scinit only drains file
//! events when its main `select!` wakes up, which in practice is the reap
//! timer (see `restart_is_prompt_with_default_reap_interval`).

use crate::integration::harness::{free_port, request, Scinit, ScinitBuilder, TEST_CHILD, TIMEOUT};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Time to let FSEvents settle after the child starts, before touching files
const SETTLE: Duration = Duration::from_millis(500);

/// How long to watch for a restart that must not happen
const QUIET: Duration = Duration::from_secs(3);

/// FSEvents (macOS) can report writes made just before the watcher starts as
/// fresh changes. Called right before spawning, after the test set up its files.
fn let_setup_writes_age() {
    std::thread::sleep(Duration::from_secs(1));
}

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
        .args(["--live-reload", "--watch-path"])
        .args([watch])
        .args(["--debounce-ms", &debounce_ms.to_string()])
        .args(["--restart-delay-ms", &restart_delay_ms.to_string()])
        .args(["--zombie-reap-interval-ms", "100"])
}

/// Live-reload with scinit's default zombie-reap interval
fn live_reload_default_reap(builder: ScinitBuilder, watch: &Path) -> ScinitBuilder {
    let_setup_writes_age();
    builder
        .args(["--live-reload", "--watch-path"])
        .args([watch])
        .args(["--debounce-ms", "200", "--restart-delay-ms", "100"])
}

/// A directory to watch, separate from the temp dir root where the child's
/// report file lives (report writes would otherwise trigger restarts)
fn watched_dir(builder: &ScinitBuilder) -> PathBuf {
    let dir = builder.dir().join("watched");
    std::fs::create_dir(&dir).unwrap();
    dir
}

/// Wait for the first child, then let pre-start filesystem events drain
fn start_and_settle(scinit: &Scinit) -> i32 {
    let pid = scinit.child_pid().unwrap();
    std::thread::sleep(SETTLE);
    pid
}

/// Write to an existing file in place (a plain content modification)
fn modify(path: &Path, content: &str) {
    std::fs::write(path, content).unwrap();
}

/// Assert no restart happens within `QUIET`
fn assert_no_restart(scinit: &mut Scinit, what: &str) {
    scinit.assert_running_for(QUIET);
    let pids = scinit.started_pids();
    assert_eq!(
        pids.len(),
        1,
        "{} must not restart the child\n{}",
        what,
        scinit.diagnostics()
    );
}

/// Modifying a watched file stops the old child with SIGTERM and starts a new one
#[test]
fn file_modify_restarts_child() {
    let b = Scinit::builder();
    let dir = watched_dir(&b);
    let file = dir.join("app.conf");
    modify(&file, "v1");
    let mut scinit = live_reload(b, &dir).child(["run"]).spawn().unwrap();

    let old = start_and_settle(&scinit);
    modify(&file, "v2");

    let new = scinit.wait_for_nth("started", 2, TIMEOUT).unwrap().pid();
    assert_ne!(old, new, "restart must spawn a new process");
    scinit.wait_for_signal(old, "TERM", TIMEOUT).unwrap();
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
    let scinit = live_reload(b, &file).child(["run"]).spawn().unwrap();

    let old = start_and_settle(&scinit);
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
    let scinit = live_reload_with(b, &dir, 200, DELAY.as_millis() as u64)
        .child(["run"])
        .spawn()
        .unwrap();

    let old = start_and_settle(&scinit);
    modify(&file, "v2");

    scinit
        .wait_for("old child exit", TIMEOUT, |evs| {
            evs.iter().any(|e| e.name == "exit" && e.pid() == old)
        })
        .unwrap();
    let exited = Instant::now();
    scinit.wait_for_nth("started", 2, TIMEOUT).unwrap();
    let gap = exited.elapsed();

    // Allow for the 20ms poll interval on both observations
    assert!(
        gap + Duration::from_millis(40) >= DELAY,
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

/// A burst of writes inside the debounce window causes exactly one restart
#[test]
fn burst_of_writes_restarts_once() {
    let b = Scinit::builder();
    let dir = watched_dir(&b);
    let file = dir.join("app.conf");
    modify(&file, "v0");
    let mut scinit = live_reload_with(b, &dir, 1500, 100)
        .child(["run"])
        .spawn()
        .unwrap();

    start_and_settle(&scinit);
    for i in 1..=5 {
        modify(&file, &format!("v{}", i));
        std::thread::sleep(Duration::from_millis(50));
    }

    scinit.wait_for_nth("started", 2, TIMEOUT).unwrap();
    // Well past debounce + restart delay: any second restart would show up
    scinit.assert_running_for(QUIET);
    assert_eq!(
        scinit.started_pids().len(),
        2,
        "a burst within the debounce window must restart exactly once\n{}",
        scinit.diagnostics()
    );
}

/// A change made inside the debounce window that follows a restart must still
/// be picked up once the window ends; otherwise the app keeps running with
/// the stale file
#[test]
#[ignore = "bug: debounce-drops-trailing-change (KNOWN-ISSUES.md)"]
fn change_within_debounce_window_is_not_lost() {
    let b = Scinit::builder();
    let dir = watched_dir(&b);
    let file = dir.join("app.conf");
    modify(&file, "v0");
    let scinit = live_reload_with(b, &dir, 1000, 100)
        .child(["run"])
        .spawn()
        .unwrap();

    start_and_settle(&scinit);
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
    let scinit = live_reload(b, &dir).child(["run"]).spawn().unwrap();

    start_and_settle(&scinit);
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
/// `is_relevant_change` accepts like a content change.
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "bug: metadata-modify-restart (KNOWN-ISSUES.md)"
)]
fn creating_file_does_not_restart() {
    let b = Scinit::builder();
    let dir = watched_dir(&b);
    let mut scinit = live_reload(b, &dir).child(["run"]).spawn().unwrap();

    start_and_settle(&scinit);
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
    let mut scinit = live_reload(b, &dir).child(["run"]).spawn().unwrap();

    start_and_settle(&scinit);
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
    let mut scinit = live_reload(b, &dir).child(["run"]).spawn().unwrap();

    start_and_settle(&scinit);
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

    assert_eq!(
        scinit.started_pids().len(),
        1,
        "an exited child must not be restarted\n{}",
        scinit.diagnostics()
    );
    crate::integration::harness::assert_exit_code(&scinit, status, 3);
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

    assert_eq!(
        scinit.started_pids().len(),
        1,
        "a crashed child must not be restarted\n{}",
        scinit.diagnostics()
    );
    crate::integration::harness::assert_exit_code(&scinit, status, 139);
}

/// scinit still exits (without restarting) when its zombie reaper wins the
/// race for the child's exit status. Today tokio's `child.wait()` then fails
/// with ECHILD, `app_main` errors, and dropping the runtime blocks forever on
/// the leaked sigwait threads, so scinit hangs.
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
        assert_eq!(scinit.started_pids().len(), 1, "{}", scinit.diagnostics());
    }
}

/// A listening child answers on the same port after a restart, from the new pid
#[test]
fn listener_survives_restart() {
    let port = free_port();
    let addr = format!("127.0.0.1:{}", port);
    let b = Scinit::builder();
    let dir = watched_dir(&b);
    let file = dir.join("app.conf");
    modify(&file, "v1");
    let scinit = live_reload(b, &dir)
        .args(["--ports", &port.to_string()])
        .child(["listen"])
        .spawn()
        .unwrap();

    scinit.wait_for_event("ready", TIMEOUT).unwrap();
    let old = scinit.child_pid().unwrap();
    let reply = request(&addr).unwrap();
    assert!(
        reply.starts_with(&format!("pid={} ", old)),
        "reply {:?} from pid {}",
        reply,
        old
    );

    std::thread::sleep(SETTLE);
    modify(&file, "v2");
    scinit.wait_for_nth("ready", 2, TIMEOUT).unwrap();
    let new = scinit.child_pid().unwrap();
    assert_ne!(old, new);

    let reply = request(&addr).unwrap_or_else(|e| {
        panic!(
            "port {} unreachable after restart: {}\n{}",
            port,
            e,
            scinit.diagnostics()
        )
    });
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
#[ignore = "bug: file-event-latency (KNOWN-ISSUES.md)"]
fn restart_is_prompt_with_default_reap_interval() {
    let b = Scinit::builder();
    let dir = watched_dir(&b);
    let file = dir.join("app.conf");
    modify(&file, "v1");
    // Default --zombie-reap-interval-ms (5000)
    let scinit = b
        .args(["--live-reload", "--watch-path"])
        .args([&dir])
        .args(["--debounce-ms", "200", "--restart-delay-ms", "100"])
        .child(["run"])
        .spawn()
        .unwrap();

    start_and_settle(&scinit);
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

    crate::integration::harness::assert_exit_code(&scinit, status, 1);
    assert!(
        scinit.started_pids().is_empty(),
        "no child should start when the watch fails\n{}",
        scinit.diagnostics()
    );
}

/// Without `--watch-path`, the watch path defaults to the command string. An
/// absolute command path is therefore watchable and scinit runs normally.
#[test]
fn default_watch_path_is_absolute_command() {
    let mut scinit = Scinit::builder()
        .args(["--live-reload", "--zombie-reap-interval-ms", "100"])
        .child(["run"])
        .spawn()
        .unwrap();

    scinit.child_pid().unwrap();
    scinit.assert_running_for(Duration::from_secs(1));
    assert_eq!(scinit.started_pids().len(), 1, "{}", scinit.diagnostics());
}

/// Current behaviour: without `--watch-path`, a bare command name (resolved via
/// PATH for spawning) is used verbatim as the watch path, relative to the cwd.
/// It does not exist there, so scinit fails at startup with exit 1 before
/// spawning anything. Phase 2 may resolve it via PATH or reject it clearly.
#[test]
fn default_watch_path_bare_command_exits_1() {
    let bin_dir = Path::new(TEST_CHILD).parent().unwrap();
    let path = match std::env::var_os("PATH") {
        Some(p) => {
            let mut dirs = vec![bin_dir.to_path_buf()];
            dirs.extend(std::env::split_paths(&p));
            std::env::join_paths(dirs).unwrap()
        }
        None => bin_dir.as_os_str().to_owned(),
    };
    let (scinit, status) = Scinit::builder()
        .env("PATH", path.to_str().unwrap())
        .args(["--live-reload"])
        .command(["scinit-test-child", "run"])
        .run(TIMEOUT)
        .unwrap();

    crate::integration::harness::assert_exit_code(&scinit, status, 1);
    assert!(scinit.started_pids().is_empty(), "{}", scinit.diagnostics());
}
