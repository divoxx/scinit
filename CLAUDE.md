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

# Run specific integration tests
cargo test --test integration_test

# Run specific test scenarios
cargo test --test integration_test test_live_reload_integration
cargo test --test integration_test test_socket_inheritance
cargo test --test integration_test test_graceful_shutdown
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

- **`InitSystem`** (`src/main.rs:142-193`): Main orchestrator managing subprocess lifecycle, signal handling, and event loop coordination
- **`ProcessManager`** (`src/process_manager.rs:80-443`): Handles subprocess spawning, monitoring, graceful restarts, and signal forwarding with process group management
- **`SignalHandler`** (`src/signals.rs:17-165`): Async signal handling using tokio streams, with proper signal masking excluding critical signals (SIGFPE, SIGILL, SIGSEGV, etc.)
- **`FileWatcher`** (`src/file_watcher.rs:46-255`): Live-reload functionality using the `notify` crate with debouncing to prevent excessive restarts
- **`PortManager`** (`src/port_manager.rs:35-274`): Socket inheritance system for zero-downtime restarts, supporting SO_REUSEPORT and multiple ports

### Key Architecture Principles

1. **Async-First Design**: Uses tokio's async runtime throughout, with event-driven signal handling instead of polling
2. **Container-Optimized**: Only allows file-change restarts, not crash restarts (crashes exit the container)
3. **Process Group Management**: Creates isolated process groups and handles terminal control properly
4. **Socket Inheritance**: Supports binding ports before process spawn and passing file descriptors via `SCINIT_INHERITED_FDS` environment variable
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

The project includes comprehensive Rust-based testing:

- **Unit Tests**: Individual component testing in each module
- **Integration Tests**: Full workflow testing in `tests/integration_test.rs`
- **Echo Server**: Test server at `src/bin/echo_server.rs` for socket inheritance validation
- **Legacy Shell Scripts**: Manual testing scripts for debugging

### Socket Inheritance Testing

The echo server demonstrates socket inheritance by:
1. Reading `SCINIT_INHERITED_FDS` environment variable
2. Converting raw file descriptors to tokio TcpListeners  
3. Echoing back messages with server metadata (PID, inherited FDs)

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