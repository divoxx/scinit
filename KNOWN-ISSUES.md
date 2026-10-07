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
- **Stray inherited fds reach the child.** Any non-close-on-exec fd that
  scinit itself inherited (e.g. sockets leaked by GitHub's macOS runner) is
  passed on to the child, next to the activated sockets at 3... Container
  runtimes start scinit with only stdio open, so this rarely matters there;
  systemd closes everything except the passed fds. The test harness starts
  scinit with a clean fd table. Consider marking all fds above 2 (other than
  the activated sockets) close-on-exec in the child's `pre_exec`.
- **FSEvents replays writes made just before the watcher starts (macOS).**
  On macOS, a file written shortly before scinit starts watching can be
  reported as a change right after startup, causing one spurious restart.
  Linux inotify doesn't do this. The live-reload tests let their setup writes
  age for a second before starting scinit (`let_setup_writes_age`).
