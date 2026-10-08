# scinit

A small init system for containers, written in Rust, built to make remote development environments feel local. scinit runs as the container's PID 1 and starts your program as its one child. It does what any container init has to: forwarding signals, shutting down gracefully, reaping zombies, and exiting with the child's status. On top of that, it restarts your program when a new build of it lands in the container (live reload), and it keeps the program's listening sockets open across those restarts (socket inheritance), so nothing connected to it notices the swap.

## Why scinit

In an organization with many services, running everything on a laptop is a hassle and often impossible: there are too many services, too much data, and too many dependencies on the real infrastructure. So development moves to a remote environment, a Docker host or a Kubernetes cluster, and the existing tools mostly take one of two routes. Some keep your service running on your laptop and route network traffic between it and the cluster, as [Telepresence](https://www.telepresence.io) does. Others, such as [Garden](https://garden.io) and [Tilt](https://tilt.dev), are built around a deploy loop: a change rebuilds the image and re-applies the manifests, and the cluster replaces the pods.

scinit is part of a different approach. The service runs in the cluster, deployed once, and stays deployed: no traffic is routed to your laptop, and a code change doesn't touch the manifests or recreate the pod. Only the code moves, and the running process is swapped for the new build in place.

## How scinit fits in

A development environment built this way has four moving parts, and scinit is deliberately the smallest of them. A deployment tool sets up the development pod once: an application container whose entrypoint is scinit, a build sidecar next to it, and a volume the two share. A file-sync tool such as [Mutagen](https://mutagen.io), or something simpler, keeps the source code inside the pod in step with your editor. The build sidecar watches those sources, recompiles the program when they change, and writes the new binary into the shared volume. scinit runs the program in the application container and owns two things: the running process, and the listening sockets clients connect to.

```mermaid
flowchart LR
    Dev["Your editor"] -- "file sync" --> Builder
    subgraph Pod["Development pod, deployed once"]
        Builder["Build sidecar"] -- "new binary" --> Vol[("Shared volume")]
        subgraph App["Application container"]
            scinit["scinit (PID 1)<br/>owns the sockets"] -- "restart" --> Service["Your service"]
        end
        Vol -- "change detected" --> scinit
    end
    Clients["Browser, port-forward,<br/>other services"] -- "connections" --> scinit
```

scinit doesn't sync files, build code or talk to Kubernetes. Its only inputs are a file on disk changing and the ports it was asked to hold, so it works with any sync tool, any build command and any orchestrator, and each piece can be swapped without touching the others.

When you save a file, the change travels through each piece in turn. The sync tool copies it into the pod, and the build sidecar compiles a new binary into the shared volume. scinit sees the binary change, waits for the writes to settle, stops the old process the same way `docker stop` would, and starts the new build. That part is [live reload](docs/guides/live-reload.md), which covers what scinit watches, how the debounce works, and how a restart is sequenced.

```mermaid
sequenceDiagram
    participant E as Your editor
    participant B as Build sidecar
    participant S as scinit
    participant Old as Old process
    participant New as New process
    participant C as Client

    E->>B: synced source change
    B->>S: new binary in the shared volume
    S->>Old: SIGTERM, wait for exit
    C->>S: connect (queued in the socket's backlog)
    S->>New: start, passing the same listening sockets
    New->>C: accept and answer the queued connection
```

Restarting a server normally leaves a window in which nothing listens on its port, and every client in that window, from your browser to a `kubectl port-forward` to another service in the cluster, gets "connection refused". With `--ports`, scinit binds the listening sockets itself before starting your program and passes the same sockets to every new process, using the systemd socket-activation protocol. A connection made during a restart waits in the socket's backlog and is answered by the new build, so from the outside the service never went away. [Socket activation](docs/guides/socket-activation.md) explains the protocol, how your program picks the sockets up, and how restarts look to Kubernetes probes.

Underneath, scinit is still a proper container init, and that is what lets the same image and entrypoint go to production. The kernel treats PID 1 differently from every other process: a signal PID 1 has no handler for is simply dropped, so a server run directly as the container's command often ignores `docker stop` until it is killed with SIGKILL, and orphaned processes are re-parented to PID 1 and stay zombies unless it collects them. scinit handles the signals, forwards them to your program's process group, escalates to SIGKILL after a timeout, reaps orphans, and exits with your program's exit code, the same job [tini](https://github.com/krallin/tini) and [dumb-init](https://github.com/Yelp/dumb-init) do. Without live reload, a crash still ends the container. [Why a container needs an init](docs/guides/why-an-init.md) covers this side in depth.

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
