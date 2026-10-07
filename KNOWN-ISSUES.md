# Known Issues in scinit

This document tracks known bugs in scinit that the integration suite exposes.
Each bug has an anchor id (the `###` heading). Tests that fail because of a bug
are marked `#[ignore = "bug: <anchor> (KNOWN-ISSUES.md)"]`, so they stay out of
the default run but are easy to find and run:

```bash
# Every test tagged with a given bug
grep -rn 'bug: fd-remap' tests/

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

### listen-pid-zero

**Description:** With `--ports`, the child receives `LISTEN_PID=0`. Under the
systemd socket-activation protocol a process must ignore `LISTEN_FDS` unless
`LISTEN_PID` equals its own pid, so libraries such as `sd_listen_fds` refuse
the sockets.

**Location:** `src/process_manager.rs` `spawn_process` calls
`get_socket_activation_env(0)` (around line 137); the real pid is not known
until after fork.

**Reproduction:**
```bash
$SCINIT --ports 18080 $CHILD dump --then-exit
grep LISTEN_PID $SCINIT_TEST_REPORT   # value=0, expected the child's pid
```

**Affected tests:** see `#[ignore]` tests tagged with this anchor:
`grep -rn 'bug: listen-pid-zero' tests/`

**Fix sketch:** set `LISTEN_PID` in `pre_exec` with `getpid()` (via `setenv`
just before exec), or exec through a tiny wrapper that sets it.

### fd-remap

**Description:** Inherited listening sockets are not moved to fd 3 onwards.
They arrive at whatever fd numbers scinit happened to get (9 and 10 observed
on macOS), while `LISTEN_FDS=n` tells the child to use 3..3+n. Their order is
also nondeterministic, because the listeners are kept in a `HashMap`.

**Location:** `src/process_manager.rs` `spawn_process` / `pre_exec` (CLOEXEC is
cleared but no `dup2`), `src/port_manager.rs` (listener storage and
`get_socket_activation_env`).

**Reproduction:**
```bash
$SCINIT --ports 18080,18081 $CHILD dump --then-exit
grep '^fds' $SCINIT_TEST_REPORT   # sockets=9,10 (for example), expected 3,4
```

**Affected tests:** see `#[ignore]` tests tagged with this anchor:
`grep -rn 'bug: fd-remap' tests/`

**Fix sketch:** in `pre_exec`, `dup2` the listeners to 3..3+n in sorted port
order and clear CLOEXEC on the targets. Sort the listeners by port so
`LISTEN_FDNAMES` and the fd order are deterministic.

### metadata-modify-restart

**Platform:** macOS only (FSEvents). Linux inotify reports no metadata change
for this case, so the test is ignored only on macOS.

**Description:** With `--live-reload`, creating an empty file in the watched
directory triggers a restart. `is_relevant_change` accepts any `Modify` event,
including metadata-only ones. On macOS, creating an empty file with `touch`
emits `Create(File)` followed by `Modify(Metadata(Extended))`, and the latter
triggers a restart.

**Location:** `src/file_watcher.rs` `is_relevant_change`.

**Reproduction:**
```bash
mkdir -p /tmp/watch && $SCINIT --live-reload --watch-path /tmp/watch $CHILD run & pid=$!
sleep 2; touch /tmp/watch/new-file; sleep 3
grep -c '^started' $SCINIT_TEST_REPORT   # 2 on macOS, expected 1
kill -KILL $pid; pkill -f 'scinit-test-child run'
```

**Affected tests:** see `#[ignore]` tests tagged with this anchor:
`grep -rn 'bug: metadata-modify-restart' tests/`

**Fix sketch:** only count `ModifyKind::Data` (and possibly `ModifyKind::Name`)
as relevant; ignore `ModifyKind::Metadata`.

### file-event-latency

**Description:** File changes are only acted on between `select!` iterations.
`run_main_loop` calls `handle_file_events` (a 100ms poll) only at the top of
each loop iteration, then blocks in a `select!` that has no file-event branch,
so a change waits until something else (a signal, SIGCHLD or the next
zombie-reap tick, default 5s) wakes the loop before the restart starts. Observed: a restart
took 4.46s after the change with `--debounce-ms 200 --restart-delay-ms 100`.

**Location:** `src/main.rs` `run_main_loop`; `src/file_watcher.rs`
`handle_file_events`.

**Reproduction:**
```bash
mkdir -p /tmp/watch && $SCINIT --live-reload --watch-path /tmp/watch \
  --debounce-ms 200 --restart-delay-ms 100 $CHILD run & pid=$!
sleep 2; echo v2 > /tmp/watch/app.conf
# the second "started" line appears up to ~5s later, not ~0.3s
```

**Affected tests:** see `#[ignore]` tests tagged with this anchor:
`grep -rn 'bug: file-event-latency' tests/`

**Fix sketch:** make the watcher's file-event channel a `select!` branch
instead of polling it before the `select!`.

### debounce-drops-trailing-change

**Description:** The live-reload debounce is leading-edge only: the first
change fires immediately and every change within `--debounce-ms` of it is
dropped, with no trailing restart once the window ends. A config edit made
shortly after a restart (or right after a burst that already fired) is lost,
and the app keeps running with the stale file until some later change. On
macOS this also bites at startup: FSEvents can replay writes made just before
the watcher started, which opens a debounce window that swallows the first
real edits.

**Location:** `src/file_watcher.rs` `start_watching`, the event task's
`last_change` check.

**Reproduction:**
```bash
mkdir -p /tmp/watch && echo v0 > /tmp/watch/app.conf
$SCINIT --live-reload --watch-path /tmp/watch --debounce-ms 1000 $CHILD run & pid=$!
sleep 2; echo v1 > /tmp/watch/app.conf; sleep 0.5; echo v2 > /tmp/watch/app.conf; sleep 3
grep -c '^started' $SCINIT_TEST_REPORT   # 2: the v2 edit never restarted it
kill -KILL $pid; pkill -f 'scinit-test-child run'
```

**Affected tests:** see `#[ignore]` tests tagged with this anchor:
`grep -rn 'bug: debounce-drops-trailing-change' tests/`

**Fix sketch:** trailing-edge debounce: on each relevant change, (re)arm a
timer for `--debounce-ms`, and restart when it fires with no newer change.

## Minor

Smaller issues, not (yet) covered by `#[ignore]` tests:

- **`signal_name` uses Linux signal numbers.** `src/signals.rs` hardcodes
  10/12/17 for USR1/USR2/CHLD, so log lines are wrong on macOS. Use
  `Signal::try_from`.
- **`LISTEN_*` leaks without `--ports`.** A `LISTEN_FDS`/`LISTEN_PID` already in
  scinit's environment is passed to the child unchanged even when scinit
  binds nothing. Strip them when no ports are configured.
- **`--zombie-reap-interval-ms 0` (probably) panics.** `tokio::time::interval`
  panics on a zero period. Reject 0 during argument parsing.
- **`--live-reload` without `--watch-path` watches the command string.** The
  default watch path is `PathBuf::from(command)`, which only works when the
  command is a path; a bare name like `my-app` is watched relative to the cwd.
  Resolve the command via `PATH`, or error clearly.
