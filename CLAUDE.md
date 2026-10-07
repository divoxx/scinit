# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Development Commands

### Building and Testing
```bash
# Build in debug mode
cargo build

# Build in release mode  
cargo build --release

# Run all tests (unit + integration)
cargo test

# Run tests with output
cargo test -- --nocapture

# Run the integration suite only
cargo test --test integration_test

# Run one scenario module
cargo test --test integration_test signals::
cargo test --test integration_test sockets::

# Run the known-bug tests (expected to fail, see KNOWN-ISSUES.md)
cargo test -- --ignored

# Run the suite on Linux in rootless podman, including the scinit-as-PID-1 tests
scripts/test-linux.sh
```

### Running the Application
```bash
# Basic usage
cargo run -- echo "hello world"

# Run with live-reload enabled
cargo run -- --live-reload --watch-path ./my-app my-app

# Run with port binding for socket inheritance
cargo run -- --live-reload --ports 8080,8081 --bind-addr 127.0.0.1 my-server

# Run with custom debounce and restart delays
cargo run -- --live-reload --debounce-ms 1000 --restart-delay-ms 500 my-app
```

## Architecture Overview

### Core Components

**scinit** is a lightweight async init system designed for container environments, built around several key modules:

- **`main` / `run_main_loop`** (`src/main.rs`): Builds the tokio runtime and runs the event loop; dispatches signals (`on_signal`) and file events (`on_file_event`) to the `ProcessManager`
- **`ProcessManager`** (`src/process_manager.rs`): Spawns, monitors, restarts and stops the child, and forwards signals to its process group. The child's lifecycle is a `ChildState` enum (`NotStarted` / `Running(ManagedChild)` / `Exited { pid, status }`). Every spawn hands the terminal to the new child (`src/terminal.rs`)
- **`SignalHandler`** (`src/signals.rs`): Receives signals only: masks the handled signals on all threads and consumes them on a dedicated `sigwait` thread, never blocking critical signals (SIGFPE, SIGILL, SIGSEGV, etc.)
- **`FileWatcher`** (`src/file_watcher.rs`): Live-reload file watching using the `notify` crate, with a trailing-edge debounce
- **`reaper`** (`src/reaper.rs`): Zombie reaping, leaving the managed child to tokio's `Child::wait()`
- **`exit_status`** (`src/exit_status.rs`): Maps the child's status to scinit's exit code (code, or 128 + signal)
- **`PortManager`** (`src/port_manager.rs`): Socket inheritance system for zero-downtime restarts, binding each port once and keeping it for scinit's lifetime
- **`SocketActivationExec`** (`src/socket_activation.rs`): Execs a child with sockets from a `pre_exec` hook, so it can move them to fds 3.. and set `LISTEN_PID` to the child's own pid

### Key Architecture Principles

1. **Async-First Design**: Uses tokio's async runtime throughout, with event-driven signal handling instead of polling
2. **Container-Optimized**: Only allows file-change restarts, not crash restarts (crashes exit the container)
3. **Process Group Management**: Creates isolated process groups and handles terminal control properly
4. **Socket Inheritance**: Supports binding ports before process spawn and passing them to the child using systemd socket activation: fds 3, 4, ... in `--ports` order, `LISTEN_FDS`, and `LISTEN_PID` set to the child's pid
5. **Graceful Shutdown**: Implements proper SIGTERM → SIGKILL escalation with configurable timeouts

### Signal Flow Architecture

The signal handling follows proper init system semantics:
1. **Signal Blocking**: `SignalHandler::install()` blocks SIGTERM, SIGINT, SIGQUIT, SIGUSR1, SIGUSR2 and SIGHUP on the main thread before the tokio runtime starts, so every thread inherits the mask
2. **Signal Detection**: One dedicated `scinit-sigwait` thread loops on `sigwait()` and forwards signals over a channel to the main loop's `select!` (cancel-safe, so no signal is lost while the loop is busy)
3. **Signal Categories**:
   - **Termination signals** (SIGTERM, SIGINT, SIGQUIT): Forward to child, then graceful shutdown
   - **Forwarding signals** (SIGUSR1, SIGUSR2, SIGHUP): Forward to child process group only
   - **Child signals** (SIGCHLD): Never blocked. tokio's SIGCHLD handler drives `Child::wait()`; the main loop observes SIGCHLD through a tokio signal stream to reap orphans. The reaper skips the managed child (marked while a `ManagedChild` guard exists) so tokio gets its exit status
   - **Critical signals** (SIGFPE, SIGILL, SIGSEGV, etc.): Never blocked, cause immediate termination
4. **Signal Forwarding**: Sent to entire process group using negative PID

### Live-Reload Architecture

The live-reload system integrates:
- File system monitoring (non-recursive) with a trailing-edge debounce: each content change or rename re-arms a `--debounce-ms` timer, and the restart fires once changes go quiet. Metadata-only changes and creating empty files are ignored
- File events are a branch of the main loop's `select!`, so a change is acted on as soon as the debounce fires
- Socket inheritance for zero-downtime restarts: listeners are bound once and the same sockets are passed to every child, so connections queue in the backlog while no child is running
- Process lifecycle management
- Only file-change triggers are allowed (not crashes)

## Testing Infrastructure

- **Unit Tests**: Individual component testing in each module
- **Fixture child** (`tests/fixtures/test_child.rs`, bin `scinit-test-child`): purpose-built child that scinit runs in tests. It appends events (`started`, `signal`, `env`, `fds`, `sigmask`, `exit`, ...) to the file in `$SCINIT_TEST_REPORT`. Subcommands: `run` (trap/ignore signals), `exit <code>`, `kill-self <SIG>`, `dump` (argv, `LISTEN_*` env, fds, signal mask), `listen` (answers on inherited sockets), `spawn-orphan` (PID 1 reaping check)
- **Harness** (`tests/integration/harness.rs`): `Scinit::builder()` spawns the real scinit binary with the fixture as child (with only stdio open), captures stdout/stderr, and offers builder shortcuts (`start`, `spawn_dump`, `ports`, `watch`), polling helpers (`wait_for`, `wait_for_nth_match`, `wait_exit`, `poll_until`) and assertions (`assert_exit_code`, `assert_start_count`, `assert_reply_from`) instead of fixed sleeps. Plain `#[test]`, no tokio
- **Scenarios** (`tests/integration/scenarios/`): `cli`, `exit_codes`, `signals`, `sockets`, `live_reload`, and `linux` (Linux only: `/proc` checks and scinit as PID 1 via `unshare`). All compile into the single `integration_test` target
- **Linux runner** (`scripts/test-linux.sh`, `tests/container/Containerfile`): builds a test image and runs `cargo test` in rootless podman (args pass through), with the permissions the `linux` PID-1 tests need; `SCINIT_REQUIRE_PID1=1` makes them fail rather than skip
- **CI** (`.github/workflows/ci.yml`): on every PR and push to `main`, runs `cargo test` on a macOS runner and `scripts/test-linux.sh` on an Ubuntu runner

### Known Bugs

Tests that fail because of a known scinit bug are marked `#[ignore = "bug: <anchor> (KNOWN-ISSUES.md)"]`. Each anchor is a section in `KNOWN-ISSUES.md`. When fixing a bug, remove the `#[ignore]` from its tests (`grep -rn 'bug: <anchor>' tests/`) and delete its entry.

### Socket Activation Testing

The `listen` fixture mode verifies socket inheritance end to end:
1. Reads `LISTEN_FDS`/`LISTEN_PID` and scans its open fds for inherited listeners
2. Accepts on each one
3. Replies with `pid=<pid> fd=<n> port=<port>`, so tests can tell which process answered

## Critical Implementation Notes

- Never allow crash-based restarts in container environments
- Always use process groups for proper signal forwarding
- Bound sockets stay close-on-exec; the child gets `dup2` copies at fds 3.. (which clears the flag), so only those are inherited
- Code in the child between fork and exec (`pre_exec`) must be async-signal-safe: build everything before forking, never allocate there
- **Signal masking**: Block handled signals on the main thread before any other thread exists; never block critical/synchronous signals or SIGCHLD
- **Signal handling**: Consume handled signals only on the dedicated sigwait thread; never call `sigwait` from per-iteration tasks (cancelled waits leave threads that swallow signals)
- Zombie reaping runs in background tasks to avoid blocking main loop, except the final pass on child exit, which runs inline so it completes before the runtime shuts down
- Terminal signals (SIGTTIN, SIGTTOU) are ignored to prevent blocking in containers