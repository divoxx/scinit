# scinit

A small init system for containers, written in Rust. scinit runs one child process as PID 1 and handles what an init has to: forwarding signals, shutting down gracefully, reaping zombies, and exiting with the child's status. It can also restart the child when files change (live reload), and pass it listening sockets using the systemd socket-activation protocol, so restarts don't drop connections.

It is in the same family as [tini](https://github.com/krallin/tini) and [dumb-init](https://github.com/Yelp/dumb-init), with live reload and socket activation added for development and zero-downtime restarts.

## Usage

```bash
scinit [OPTIONS] <COMMAND> [ARGS]...
```

Everything after the command is passed to the child unchanged, including arguments that look like scinit flags.

```bash
# Run a process under scinit
scinit my-server --config /etc/my-server.toml

# Restart the child when files in ./config change
scinit --live-reload --watch-path ./config my-server

# Bind ports 8080 and 8081 and pass them to the child (fds 3 and 4)
scinit --ports 8080,8081 --bind-addr 0.0.0.0 my-server
```

As a container entrypoint:

```dockerfile
COPY scinit /usr/local/bin/scinit
ENTRYPOINT ["/usr/local/bin/scinit", "--"]
CMD ["my-server"]
```

### Options

| Option | Default | Description |
|---|---|---|
| `--graceful-timeout-secs <N>` | `30` | How long to wait for the child to exit after a termination signal before sending SIGKILL |
| `--zombie-reap-interval-ms <N>` | `5000` | Interval for the periodic zombie reaper (orphans are also reaped on SIGCHLD) |
| `--live-reload` | off | Restart the child when the watched path changes |
| `--watch-path <PATH>` | the command as given | File or directory to watch (non-recursive) |
| `--debounce-ms <N>` | `500` | Wait this long after the last change before restarting |
| `--restart-delay-ms <N>` | `1000` | Pause between the old child exiting and the new one starting |
| `--ports <P1,P2,...>` | none | Ports to bind and pass to the child |
| `--bind-addr <ADDR>` | `127.0.0.1` | Address to bind `--ports` on (IPv4 or IPv6) |

### Logging

scinit writes its own messages to **stderr** only, so stdout carries nothing but the child's output (`docker run img cmd | jq` stays clean). Each line looks like:

```
[scinit] ERROR Failed to spawn process 'my-app': No such file or directory (os error 2)
```

Lines carry no timestamp: container log drivers (Docker's `json-file`, Kubernetes' CRI log files, journald) record one per line, shown with `docker logs -t` or `kubectl logs --timestamps`. Attached runs (`docker run` without `-d`) and runs outside a container get none. Color is used only when stderr is a terminal (`NO_COLOR` turns it off).

Verbosity is set with **`SCINIT_LOG`**, using [tracing's `EnvFilter` syntax](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html). The default is `error`.

```bash
SCINIT_LOG=info scinit my-server                        # lifecycle events: spawn, restart, signals
SCINIT_LOG=debug scinit my-server                       # everything
SCINIT_LOG=scinit::file_watcher=debug scinit my-server  # one module
```

scinit doesn't read `RUST_LOG`: it's passed to the child unchanged, so setting it for your app doesn't make scinit verbose, and the other way around.

## Behavior

### Child process

- The child runs in its own process group, with an empty signal mask, and inherits scinit's environment, working directory and stdio.
- When there is a terminal, each child's process group (including after a live-reload restart) is made the foreground group, so it receives Ctrl-C and terminal input.
- scinit exits when the child exits, **with the child's exit code**, or 128 + the signal number if the child was killed by a signal (e.g. 137 for SIGKILL).
- A child that exits or crashes is never restarted; only file changes trigger restarts (with `--live-reload`). In a container, a crash should end the container.

### Signals

| Signal | What scinit does |
|---|---|
| SIGTERM, SIGINT, SIGQUIT | Forwards it to the child's process group, waits up to `--graceful-timeout-secs` for the child to exit, then SIGKILLs the group. Exits with the child's status. |
| SIGUSR1, SIGUSR2, SIGHUP | Forwards it to the child's process group and keeps running |
| SIGCHLD | Reaps orphaned processes (the child's exit status is left to scinit's own wait) |
| SIGTTIN, SIGTTOU | Ignored (inherited by the child), so background terminal I/O doesn't stop processes |
| SIGFPE, SIGILL, SIGSEGV, SIGBUS, SIGABRT, ... | Never blocked: they keep their default action |

The handled signals are blocked on every scinit thread and consumed by one dedicated `sigwait` thread, so they are never lost or delivered to the wrong place. This also works when scinit is PID 1, where the kernel would otherwise drop signals it doesn't handle.

### Zombie reaping

Processes orphaned inside the container are re-parented to scinit (as PID 1) and reaped as soon as SIGCHLD arrives, with a periodic sweep (`--zombie-reap-interval-ms`) as a fallback.

### Live reload

With `--live-reload`, scinit watches `--watch-path` (one file, or the entries of one directory) and restarts the child when it changes:

- Only content changes and renames count (editors often save by renaming over the file). Metadata-only changes, such as creating an empty file or changing permissions, don't.
- The debounce is trailing-edge: every change restarts the `--debounce-ms` timer, and the restart happens once changes have gone quiet. A burst of saves causes one restart, and the last edit is never lost.
- A restart sends SIGTERM to the child's group (escalating to SIGKILL after `--graceful-timeout-secs`), waits `--restart-delay-ms`, then starts a new child.

### Socket activation

With `--ports`, scinit binds the listening sockets itself and passes them to the child following the [systemd socket-activation protocol](https://www.freedesktop.org/software/systemd/man/latest/sd_listen_fds.html), so any server that supports `sd_listen_fds()` (or a library like `listenfd`) can use them:

- The sockets are at fds **3, 4, ...** in `--ports` order.
- `LISTEN_FDS` is the number of sockets, and `LISTEN_PID` is the child's own pid. Any `LISTEN_*` variables scinit itself inherited are replaced.
- Each port is bound **once** and the same sockets are passed to every child. During a live-reload restart, connections wait in the socket's backlog and the new child serves them, so restarts don't drop or refuse connections.
- Sockets are bound with `SO_REUSEPORT`.

## Platforms

scinit targets Linux containers. macOS is supported for development, and the test suite runs on both. Behavior specific to PID 1 (orphan re-parenting, the kernel dropping unhandled signals) only applies on Linux.

## Building

```bash
cargo build --release   # target/release/scinit
```

## Testing

The integration suite drives the real `scinit` binary with a purpose-built child process, and asserts on what the child actually experienced (signals, environment, fds, sockets) plus scinit's own exit status.

```bash
# Run all tests (unit + integration)
cargo test

# Run the integration suite, or one scenario module
cargo test --test integration_test
cargo test --test integration_test signals::

# Run the suite on Linux in rootless podman, including scinit-as-PID-1 tests
scripts/test-linux.sh
scripts/test-linux.sh --test integration_test sockets::   # args go to cargo test
```

- **Fixture child** (`tests/fixtures/test_child.rs`, built as `scinit-test-child`): runs as scinit's child and appends what it observes (`started`, `signal`, `env`, `fds`, `exit`, ...) to the file named by `$SCINIT_TEST_REPORT`. Subcommands: `run`, `exit <code>`, `kill-self <SIG>`, `dump`, `listen`, `spawn-orphan`.
- **Harness** (`tests/integration/harness.rs`): spawns scinit with the fixture, captures its output, polls the report instead of sleeping, and cleans up every process group on drop.
- **Scenarios** (`tests/integration/scenarios/`): `cli`, `exit_codes`, `signals`, `sockets`, `live_reload`, and the Linux-only `linux` (`/proc` signal masks, scinit as PID 1 in a new PID namespace).
- **Linux runner** (`scripts/test-linux.sh`, `tests/container/Containerfile`): builds a test image and runs `cargo test` in rootless podman, with the permissions the PID-1 tests need. Requires podman.
- **CI** (`.github/workflows/ci.yml`): runs `cargo test` on macOS and `scripts/test-linux.sh` on Linux for every pull request and push to `main`.

Known issues and open design decisions are tracked in [KNOWN-ISSUES.md](KNOWN-ISSUES.md). Tests for a known bug are marked `#[ignore = "bug: <anchor> (KNOWN-ISSUES.md)"]` and can be run with `cargo test -- --ignored`.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
