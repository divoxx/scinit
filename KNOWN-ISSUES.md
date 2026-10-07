# Known Issues in scinit

This document tracks known bugs in scinit that the integration suite exposes.
Each bug has an anchor id (the `###` heading). Tests that fail because of a bug
are marked `#[ignore = "bug: <anchor> (KNOWN-ISSUES.md)"]`, so they stay out of
the default run but are easy to find and run:

```bash
# Every test tagged with a given bug
grep -rn 'bug: main-thread-unmasked' tests/

# Run only the known-bug tests (they are expected to fail until fixed)
cargo test -- --ignored
```

When a fix lands, remove the `#[ignore]` from its tests and delete the entry here.

**Fix order:** fix main-thread-unmasked, sigwait-thread-leak and
sigchld-blocked-tokio-wait together, since fixing one alone exposes the next.

The reproductions below use the test fixture built alongside scinit:

```bash
cargo build
SCINIT=target/debug/scinit
CHILD=target/debug/scinit-test-child
export SCINIT_TEST_REPORT=/tmp/scinit-report.log   # the child logs events here
```

## Bugs

### exit-code-propagation

**Description:** scinit always exits 0, whatever happened to its child. A child
that exits 42 or dies from SIGKILL still produces a successful exit, so
container orchestrators can't tell a crash from a clean shutdown.

**Location:** `src/process_manager.rs` `handle_child_exit` only logs the status
and returns `Ok(())`; `src/main.rs` `app_main` / `main` then return `Ok(())`
unconditionally. The SIGTERM path (`SignalAction::Exit`) does the same.

**Reproduction:**
```bash
$SCINIT $CHILD exit 42; echo "exit=$?"        # expected 42, prints 0
$SCINIT $CHILD kill-self KILL; echo "exit=$?" # expected 137, prints 0
```

**Affected tests:** see `#[ignore]` tests tagged with this anchor:
`grep -rn 'bug: exit-code-propagation' tests/`

**Fix sketch:** have `handle_child_exit` return the code (or 128 + signal
number) and call `std::process::exit` with it from `main` after the runtime is
shut down. The termination-signal path should return the child's status too.

### main-thread-unmasked

**Description:** The handled signals (TERM, INT, QUIT, USR1, USR2, HUP, CHLD)
are only blocked on tokio worker threads, via `on_thread_start`. The main
thread, which runs `block_on`, keeps an empty mask, so the kernel delivers
process-directed signals to it, where they take their default action instead
of reaching the `sigwait` loop. Confirmed on macOS, where SIGTERM killed
scinit with exit 143 on every attempt (5/5), so on macOS scinit currently
never runs its own signal handling:

- Every handled signal kills scinit itself instead of being forwarded. With
  default dispositions scinit dies from TERM, INT, QUIT, HUP, USR1 and USR2
  (killed by signal 15, 2, 3, 1, 30, 31 on macOS; exit 143 for SIGTERM), and
  the child never receives anything. The old "SIGUSR1/SIGUSR2/SIGHUP cause
  unexpected exit" issue is this bug.
- The child, in its own process group, is orphaned and keeps running.

On Linux the same root cause looks different when scinit is PID 1 (a
container entrypoint), because the kernel discards default-action signals
aimed at a namespace init that neither blocks nor handles them:

- SIGTERM (`docker stop`/`podman stop`) is silently dropped, never forwarded,
  and the container is SIGKILLed after the stop timeout.
- SIGCHLD lands in tokio's own SIGCHLD handler on the main thread, so scinit's
  `ReapZombies` action never runs and orphans stay zombies until the periodic
  reaper (default 5s) collects them.

Both are confirmed by the PID-1 tests in
`tests/integration/scenarios/linux.rs` (`sigterm_forwarded_as_pid1`,
`orphan_reaped_on_sigchld_as_pid1`), run via `scripts/test-linux.sh`.

**Location:** `src/main.rs` around lines 45-54 (`on_thread_start` calls
`setup_thread_signals`; nothing masks the main thread before the runtime is
built). `src/signals.rs` `setup_thread_signals`.

**Reproduction:**
```bash
$SCINIT $CHILD run & pid=$!; sleep 1
kill -TERM $pid; wait $pid; echo "exit=$?"   # 143: scinit died from SIGTERM
cat $SCINIT_TEST_REPORT                       # no "signal ... sig=TERM" line
pkill -f 'scinit-test-child run'              # clean up the orphaned child
```
On Linux, compare `SigBlk` in `/proc/<scinit-pid>/task/<scinit-pid>/status`
(main thread, 0) with any worker thread.

**Affected tests:** see `#[ignore]` tests tagged with this anchor:
`grep -rn 'bug: main-thread-unmasked' tests/`

Every ignored test in `signals.rs` is tagged with this anchor because it is
the first bug that blocks them, even when a later bug (for example
[sigint-fixed-delay](#sigint-fixed-delay)) would also fail them.

**Fix sketch:** block the handled signals with `pthread_sigmask` on the main
thread before building the runtime, so every thread (workers and the blocking
pool) inherits the mask. Fix together with
[sigwait-thread-leak](#sigwait-thread-leak) and
[sigchld-blocked-tokio-wait](#sigchld-blocked-tokio-wait): fixing one alone
exposes the next.

### sigwait-thread-leak

**Description:** `SignalHandler::wait_for_signal` starts a new
`spawn_blocking` task that calls `sigwait` every time the main loop's
`select!` runs. When another branch wins (child exit, reap timer, file
event), the future is dropped but the blocking thread stays parked in
`sigwait`. These leaked threads pile up (one per loop iteration, every 5s
with the default reap interval) and consume later signals; the signal is
taken by a thread whose result nobody reads, so it is neither forwarded nor
acted on. They also keep the runtime from shutting down cleanly.

Confirmed in a build with the main thread masked: the first signal is always
lost, because the zombie-reap interval ticks immediately at startup, cancels
the first `select!` iteration, and leaves a stale sigwait thread that
consumes the next signal. Of 6 USR1 sent, the child received 5.

**Location:** `src/signals.rs` `wait_for_signal` (around lines 101-117) and the
`select!` in `src/main.rs` `run_main_loop`.

**Reproduction:**
```bash
# Live-reload plus a short reap interval makes the loop spin, leaking threads
$SCINIT --live-reload --watch-path /tmp --zombie-reap-interval-ms 100 $CHILD run & pid=$!
sleep 6
for i in 1 2 3; do kill -USR1 $pid; sleep 0.5; done
grep -c 'sig=USR1' $SCINIT_TEST_REPORT       # fewer than 3 (often 0)
kill -KILL $pid; pkill -f 'scinit-test-child run'
```

**Affected tests:** see `#[ignore]` tests tagged with this anchor:
`grep -rn 'bug: sigwait-thread-leak' tests/`

**Fix sketch:** replace the per-call `spawn_blocking` with one dedicated thread
that loops on `sigwait` and sends each signal into an mpsc channel; the
`select!` branch reads from the channel, which is cancel-safe. Fix together
with [main-thread-unmasked](#main-thread-unmasked) and
[sigchld-blocked-tokio-wait](#sigchld-blocked-tokio-wait).

### sigchld-blocked-tokio-wait

**Description (suspected):** Only visible once
[main-thread-unmasked](#main-thread-unmasked) and
[sigwait-thread-leak](#sigwait-thread-leak) are fixed. When SIGCHLD is
blocked on every thread and consumed by scinit's `sigwait`, tokio's own
SIGCHLD handler never runs, so `child.wait()` (used by `graceful_shutdown`
and the main loop) isn't woken promptly and only notices the exit on some
later wakeup. Evidence: in a patched build the child exited at about T+0, but
"Process exited with status" was logged about 4s later.

**Location:** `src/signals.rs` (SIGCHLD in `handled_signals`) versus
`ProcessManager::wait_for_exit` / `graceful_shutdown` in
`src/process_manager.rs`, which rely on tokio's `child.wait()`.

**Reproduction:** apply fixes for the two bugs above, then:
```bash
RUST_LOG=info $SCINIT $CHILD run & pid=$!; sleep 1
kill -TERM $pid   # the child logs exit at once; scinit logs the exit seconds later
```

**Affected tests:** see `#[ignore]` tests tagged with this anchor:
`grep -rn 'bug: sigchld-blocked-tokio-wait' tests/`

**Fix sketch:** don't consume SIGCHLD via `sigwait` while relying on tokio's
`child.wait()`. Either leave SIGCHLD to tokio, or make scinit's own reaper the
single source of child status. Keep the managed child excluded from scinit's
own zombie reaper (`MANAGED_CHILD` in `src/process_manager.rs`) either way.

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

### sigint-fixed-delay

**Description:** On SIGINT or SIGQUIT scinit forwards the signal, then always
sleeps 2 seconds before force-killing, even if the child exited immediately,
and ignores `--graceful-timeout-secs`. Shutdown is slower than necessary, and a
child that needs longer than 2s to clean up is killed early. Confirmed in a
build with main-thread-unmasked and sigwait-thread-leak fixed: the
prompt-exit tests measured 2.11s and 2.12s. Those tests are tagged
`main-thread-unmasked`, so they keep failing until this bug is fixed too.

**Location:** `src/signals.rs` `handle_termination_signal`, the
`Signal::SIGINT | Signal::SIGQUIT` arm (`tokio::time::sleep(Duration::from_secs(2))`).

**Reproduction:**
```bash
$SCINIT --graceful-timeout-secs 10 $CHILD run & pid=$!; sleep 1
time (kill -INT $pid; wait $pid)   # ~2s even though the child exits at once
```
(Run this after [main-thread-unmasked](#main-thread-unmasked) is fixed;
until then SIGINT kills scinit outright.)

**Affected tests:** see `#[ignore]` tests tagged with this anchor:
`grep -rn 'bug: sigint-fixed-delay' tests/`

**Fix sketch:** send SIGINT/SIGQUIT through the same graceful path as SIGTERM:
wait for the child to exit, up to `--graceful-timeout-secs`, then SIGKILL.

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
each loop iteration, then blocks in a `select!` that has no file-event branch.
Signals don't reliably wake it either (see
[sigwait-thread-leak](#sigwait-thread-leak)), so a change waits for the next
zombie-reap tick (default 5s) before the restart starts. Observed: a restart
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
