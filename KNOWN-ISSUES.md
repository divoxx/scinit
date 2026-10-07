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
- **FSEvents replays writes made just before the watcher starts (macOS).**
  On macOS, a file written shortly before scinit starts watching can be
  reported as a change right after startup, causing one spurious restart.
  Linux inotify doesn't do this. The live-reload tests let their setup writes
  age for a second before starting scinit (`let_setup_writes_age`).
