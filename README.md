# scinit

A small init system for containers, written in Rust. scinit runs as the container's PID 1, starts your program as its one child, and handles what an init has to: forwarding signals, shutting down gracefully, reaping zombies, and exiting with the child's status. It can also restart the child when files change (live reload), and pass it listening sockets using the systemd socket-activation protocol, so restarts don't drop connections.

## Why scinit

The first process in a container runs as PID 1, and the kernel treats PID 1 differently from every other process. A signal that PID 1 has not installed a handler for is simply dropped, so the default action that would normally terminate a process never happens. Most programs don't install a SIGTERM handler because they rely on that default, which is why a server run directly as the container's command often ignores `docker stop`: Docker waits out its 10 second timeout and then kills it with SIGKILL, skipping any cleanup.

PID 1 also inherits every orphaned process in the container. When a process exits, its parent has to collect its exit status with `wait()`, or it stays in the process table as a zombie. A server that spawns helpers (a shell script, a worker pool, a health check) and doesn't expect to be an init never collects the orphans that get re-parented to it, and the zombies pile up.

scinit sits in front of your program and does that job. It handles the signals, forwards them to your program's process group, escalates to SIGKILL if the program doesn't exit within a timeout, reaps orphans, and exits with your program's exit code, so the orchestrator can tell a crash from a clean shutdown. That is the same job [tini](https://github.com/krallin/tini) and [dumb-init](https://github.com/Yelp/dumb-init) do. scinit adds a development loop on top: with `--live-reload` it restarts your program when its files change, and with `--ports` it binds the listening sockets itself and hands the same sockets to every restarted child, so clients wait in the socket's backlog during a restart instead of getting "connection refused".

## Examples

As a container entrypoint, with your program as the command:

```dockerfile
COPY scinit /usr/local/bin/scinit
ENTRYPOINT ["/usr/local/bin/scinit", "--"]
CMD ["my-server", "--config", "/etc/my-server.toml"]
```

Running a program directly. The `--` ends scinit's own options, so everything after it is the command and its arguments, passed through unchanged:

```bash
scinit -- my-server --config /etc/my-server.toml
```

Restarting the program whenever a file in `./config` changes:

```bash
scinit --live-reload --watch-path ./config -- my-server
```

Binding port 8080 on every interface and passing it to the program as file descriptor 3, with `LISTEN_FDS=1` and `LISTEN_PID` set as systemd does:

```bash
scinit --ports 8080 --bind-addr 0.0.0.0 -- my-server
```

## Documentation

- [Documentation index](docs/README.md), with a suggested reading order
- [Getting started](docs/getting-started.md): build scinit, run it, put it in a container
- [Why a container needs an init](docs/guides/why-an-init.md)
- [Signals and shutdown](docs/guides/signals-and-shutdown.md)
- [Exit codes](docs/guides/exit-codes.md)
- [Zombie reaping](docs/guides/zombie-reaping.md)
- [Process isolation](docs/guides/process-isolation.md): process groups, the terminal, file descriptors
- [Live reload](docs/guides/live-reload.md)
- [Socket activation](docs/guides/socket-activation.md)
- [Logging](docs/guides/logging.md)
- [Command-line reference](docs/reference/cli.md): every flag, environment variable and exit code
- [Development](docs/development.md): building and testing scinit itself

Known issues and planned work are tracked in [GitHub issues](https://github.com/divoxx/scinit/issues).

## Building

scinit has no prebuilt binaries yet. Build it with `cargo build --release`, which produces `target/release/scinit`. Running the test suite and the Linux container runner are covered in [docs/development.md](docs/development.md).

scinit targets Linux containers. macOS is supported for development, and the test suite runs on both; the behavior that is specific to PID 1 only applies on Linux.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
