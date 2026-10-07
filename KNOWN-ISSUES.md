# Known Issues in scinit

This document tracks known bugs in scinit that the integration suite exposes.
Each bug has an anchor id (the `###` heading). Tests that fail because of a bug
are marked `#[ignore = "bug: <anchor> (KNOWN-ISSUES.md)"]`, so they stay out of
the default run but are easy to find and run:

```bash
# Every test tagged with a given bug
grep -rn 'bug: <anchor>' tests/

# Run only the known-bug tests (they are expected to fail until fixed)
cargo test -- --ignored
```

When a fix lands, remove the `#[ignore]` from its tests and delete the entry here.

The reproductions below use the test fixture built alongside scinit:

```bash
cargo build
SCINIT=target/debug/scinit
CHILD=target/debug/scinit-test-child
export SCINIT_TEST_REPORT=/tmp/scinit-report.log   # the child logs events here
```

## Bugs

None currently tracked: every integration test passes. New bugs go here, one
`###` section each (description, location, reproduction, affected tests, fix
sketch), with their failing tests marked `#[ignore]`.

## Minor

Smaller issues, not (yet) covered by `#[ignore]` tests:

- **Non-UTF-8 environment variables crash scinit (without `--ports`).**
  `spawn_process` builds the child environment with `std::env::vars()`
  (`src/process_manager.rs`), which panics on a variable that isn't valid
  UTF-8, so scinit dies before starting the child. The `--ports` path uses
  `vars_os()` and is unaffected. Use `vars_os()` (or `Command::envs` on top
  of the inherited environment) on both paths.
- **`LISTEN_*` leaks without `--ports`.** A `LISTEN_FDS`/`LISTEN_PID` already in
  scinit's environment is passed to the child unchanged even when scinit
  binds nothing, so the child may treat unrelated fds 3.. as listeners. The
  `--ports` path already replaces them. Strip them when no ports are
  configured, and flip `inherited_listen_fds_leaks_without_ports`.
- **The final zombie reap isn't awaited.** On child exit, `handle_child_exit`
  (`src/exit_status.rs`) calls `spawn_zombie_reap()`, which starts a
  `spawn_blocking` reap without waiting for it; scinit then shuts the runtime
  down within 100ms and exits. Orphans that exited just before the child may
  be left unreaped. Await the reap before returning the exit code.
- **`signal_name` uses Linux signal numbers.** `src/exit_status.rs` hardcodes
  10/12/17 for USR1/USR2/CHLD, so on macOS a child killed by SIGUSR1 (30) is
  logged as `UNKNOWN`, and signals outside the table (e.g. SIGSEGV) are
  `UNKNOWN` everywhere. Logs only; exit codes are correct. Use
  `Signal::try_from(n).map(Signal::as_str)`.
- **The terminal is handed to the child only once.** `foreground_child`
  (`src/main.rs`) makes the first child's process group the terminal's
  foreground group, but it isn't called again after a live-reload restart, so
  later children don't get Ctrl-C or terminal input when run interactively.
  No effect in containers without a TTY. Call it after every spawn.
- **`--zombie-reap-interval-ms 0` (probably) panics.** `tokio::time::interval`
  panics on a zero period. Reject 0 during argument parsing.
- **`--live-reload` without `--watch-path` watches the command string.** The
  default watch path is `PathBuf::from(command)` (`src/cli.rs`), which only
  works when the command is a path; `--live-reload my-app` watches `./my-app`
  relative to the cwd and fails to start if it doesn't exist. Resolve the
  command via `PATH`, or error clearly.
- **FSEvents replays writes made just before the watcher starts (macOS).**
  On macOS, a file written shortly before scinit starts watching can be
  reported as a change right after startup, causing one spurious restart.
  Linux inotify doesn't do this. The live-reload tests let their setup writes
  age for a second before starting scinit (`let_setup_writes_age`).
- **README describes the old signal handling.** The README's signal sections
  say scinit uses tokio's async signal streams and compare it to
  `sigtimedwait` polling; scinit actually blocks the handled signals on every
  thread and consumes them on a dedicated `sigwait` thread (see CLAUDE.md).

## Design decisions to make

- **`SO_REUSEPORT` is always on.** It lets several sockets bind the same port
  at once. Zero-downtime restarts no longer need it: scinit binds each port
  once and passes the same socket to every child, so connections queue in its
  backlog between children. Keeping it on lets any other process silently
  bind the same port and take a share of the connections. It is only needed
  for running old and new children side by side, handing over between two
  scinit instances, or deliberate multi-process load balancing. Proposal:
  default to `SO_REUSEADDR` only (so a restarted container can rebind while
  old connections sit in TIME_WAIT), and add an opt-in flag for
  `SO_REUSEPORT`.
