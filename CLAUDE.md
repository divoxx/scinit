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

# Run the suite on Linux in rootless podman, plus the PID 1 checks
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

- **`main` / `run_main_loop`** (`src/main.rs`): Builds the tokio runtime and runs the event loop coordinating subprocess lifecycle, signal handling and file events
- **`ProcessManager`** (`src/process_manager.rs`): Handles subprocess spawning, monitoring, graceful restarts, and signal forwarding with process group management
- **`SignalHandler`** (`src/signals.rs`): Synchronous `sigwait`-based signal handling, with proper signal masking excluding critical signals (SIGFPE, SIGILL, SIGSEGV, etc.)
- **`FileWatcher`** (`src/file_watcher.rs`): Live-reload functionality using the `notify` crate with debouncing to prevent excessive restarts
- **`PortManager`** (`src/port_manager.rs`): Socket inheritance system for zero-downtime restarts, supporting SO_REUSEPORT and multiple ports

### Key Architecture Principles

1. **Async-First Design**: Uses tokio's async runtime throughout, with event-driven signal handling instead of polling
2. **Container-Optimized**: Only allows file-change restarts, not crash restarts (crashes exit the container)
3. **Process Group Management**: Creates isolated process groups and handles terminal control properly
4. **Socket Inheritance**: Supports binding ports before process spawn and passing them to the child using systemd-style socket activation (`LISTEN_FDS`/`LISTEN_PID`)
5. **Graceful Shutdown**: Implements proper SIGTERM → SIGKILL escalation with configurable timeouts

### Signal Flow Architecture

The signal handling follows proper init system semantics:
1. **Signal Blocking**: Only specific signals are blocked for synchronous handling (SIGTERM, SIGINT, SIGQUIT, SIGUSR1, SIGUSR2, SIGHUP, SIGCHLD)
2. **Signal Detection**: Uses `sigtimedwait()` for synchronous signal handling (proper for init systems)
3. **Signal Categories**:
   - **Termination signals** (SIGTERM, SIGINT, SIGQUIT): Forward to child, then graceful shutdown
   - **Forwarding signals** (SIGUSR1, SIGUSR2, SIGHUP): Forward to child process group only
   - **Child signals** (SIGCHLD): Handled by init for zombie reaping
   - **Critical signals** (SIGFPE, SIGILL, SIGSEGV, etc.): Never blocked, cause immediate termination
4. **Signal Forwarding**: Sent to entire process group using negative PID

### Live-Reload Architecture

The live-reload system integrates:
- File system monitoring with debouncing
- Socket inheritance for zero-downtime restarts
- Process lifecycle management
- Only file-change triggers are allowed (not crashes)

## Testing Infrastructure

- **Unit Tests**: Individual component testing in each module
- **Fixture child** (`tests/fixtures/test_child.rs`, bin `scinit-test-child`): purpose-built child that scinit runs in tests. It appends events (`started`, `signal`, `env`, `fds`, `sigmask`, `exit`, ...) to the file in `$SCINIT_TEST_REPORT`. Subcommands: `run` (trap/ignore signals), `exit <code>`, `kill-self <SIG>`, `dump` (argv, `LISTEN_*` env, fds, signal mask), `listen` (answers on inherited sockets), `spawn-orphan` (PID 1 reaping check)
- **Harness** (`tests/integration/harness.rs`): `Scinit::builder()` spawns the real scinit binary with the fixture as child, captures stdout/stderr, and offers polling helpers (`wait_for`, `child_pid`, `wait_exit`) instead of fixed sleeps. Plain `#[test]`, no tokio
- **Scenarios** (`tests/integration/scenarios/`): `cli`, `exit_codes`, `signals`, `sockets`, `live_reload`, and `linux` (Linux only: `/proc` checks and scinit as PID 1 via `unshare`). All compile into the single `integration_test` target
- **Linux runner** (`scripts/test-linux.sh`, `tests/container/Containerfile`): builds a test image and runs `cargo test` in rootless podman (args pass through), with the permissions the `linux` PID-1 tests need; `SCINIT_REQUIRE_PID1=1` makes them fail rather than skip

### Known Bugs

Tests that fail because of a known scinit bug are marked `#[ignore = "bug: <anchor> (KNOWN-ISSUES.md)"]`. Each anchor is a section in `KNOWN-ISSUES.md`. When fixing a bug, remove the `#[ignore]` from its tests (`grep -rn 'bug: <anchor>' tests/`) and delete its entry.

### Socket Activation Testing

The `listen` fixture mode verifies socket inheritance end to end:
1. Reads `LISTEN_FDS`/`LISTEN_PID` and scans its open fds for inherited listeners
2. Accepts on each one
3. Replies with `pid=<pid> fd=<n> port=<port>`, so tests can tell which process answered

## Performance Characteristics

- **Signal Response**: ~100ms (vs ~1000ms with polling)
- **CPU Usage**: Event-driven (vs constant polling overhead)
- **Memory**: Optimized with async streams
- **Blocking**: Fully non-blocking operations

## Critical Implementation Notes

- Never allow crash-based restarts in container environments
- Always use process groups for proper signal forwarding
- File descriptors must have FD_CLOEXEC cleared for inheritance
- **Signal masking**: Only block signals that init should handle synchronously, never block critical/synchronous signals
- **Signal handling**: Use `sigtimedwait()` for proper init system signal semantics, not async signal handlers
- Zombie reaping runs in background tasks to avoid blocking main loop
- Terminal signals (SIGTTIN, SIGTTOU) are ignored to prevent blocking in containers