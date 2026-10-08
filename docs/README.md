# scinit documentation

These pages explain how scinit works and why it behaves the way it does. They are plain Markdown, written to be read on GitHub. If you only want to look up a flag, go straight to the [command-line reference](reference/cli.md).

scinit is a container init: it does what tini and dumb-init do, sends SIGKILL to your program when it doesn't exit within a timeout after a shutdown signal, and adds two features designed for development, live reload and socket inheritance. Those were built for remote development environments, where a service then can stay deployed in a Docker host or Kubernetes cluster and only its code or binary needs to change: your edits are synchronized into the running container, rebuilt there, and scinit swaps the running process for the new build while keeping its listening sockets open. The same entrypoint then runs the service in production. The [README](../README.md) compares scinit with other inits and lists its use cases.

The order below starts with getting scinit running and then works from the core job of an init towards the development features.

## Start here

[Getting started](getting-started.md) builds scinit from source, runs a first command under it, puts it into a container image as the entrypoint, and walks through a graceful shutdown and a first live-reload loop with logging turned on. It is the quickest way to see every part of scinit once before reading about each in depth.

## The init system

[Why a container needs an init](guides/why-an-init.md) explains the problem scinit exists to solve. The kernel gives PID 1 no default signal actions, so a program that relies on them ignores `docker stop`; orphaned processes are re-parented to PID 1 and turn into zombies if nobody collects them; and the container's exit code is whatever PID 1 exits with. Read this first if you have never wondered why tini exists.

[Signals and shutdown](guides/signals-and-shutdown.md) covers what scinit does with each signal it receives. Termination signals are forwarded to the child's process group and followed by a graceful shutdown that escalates to SIGKILL after `--graceful-timeout-secs`; a few other signals are only forwarded; and the signals that report a crash are never touched. It also describes how scinit receives signals on one dedicated thread so none are lost.

[Exit codes](guides/exit-codes.md) describes how the child's outcome becomes scinit's exit status: the child's own exit code, 128 plus the signal number when it was killed by a signal, and the codes scinit uses for its own errors. This is what your orchestrator sees, so it is worth knowing when you set up restart policies or alerts.

[Zombie reaping](guides/zombie-reaping.md) explains when scinit collects orphaned processes: as soon as SIGCHLD arrives, on a periodic sweep, and once more when the child exits on its own. It also explains why orphans are only re-parented to scinit when it is actually PID 1.

[Process isolation](guides/process-isolation.md) describes the environment the child starts in: its own process group, an empty signal mask, the terminal's foreground (so Ctrl-C reaches it directly), and no file descriptors other than stdio and any activated sockets.

## The development features

[Remote development environments](guides/remote-development.md) is the setup live reload and socket inheritance were designed for: a service that then can stay deployed in a cluster while only its code or binary needs to change, with a build sidecar recompiling it in place and scinit swapping in each new build. It describes the pieces, what scinit is and isn't responsible for, and how one change travels from your editor to the running service.

[Live reload](guides/live-reload.md) covers `--live-reload`: what scinit watches by default, which file system events count as a change, how the trailing-edge debounce turns a burst of saves into one restart, and the sequence scinit follows to stop the old child and start a new one.

[Socket activation](guides/socket-activation.md) covers `--ports`: scinit binds the listening sockets once and passes them to each child at file descriptor 3 onwards, following the systemd protocol, so connections wait in the backlog during a restart instead of being refused. It includes short examples of picking the sockets up from an application, and what restarts mean for Kubernetes probes.

## Operating scinit

[Logging](guides/logging.md) describes scinit's own log output on stderr, how to make it more or less verbose with `SCINIT_LOG`, and recipes for answering questions such as why your program did not restart.

The [command-line reference](reference/cli.md) lists every flag with its default, the environment variables scinit reads and sets, and its exit codes, in tables you can scan.

## Working on scinit

[Development](development.md) is for contributors: how to build and test scinit, how the integration suite drives the real binary with a purpose-built child process, how to run the Linux-only tests in rootless podman, and how known bugs are tracked in [GitHub issues](https://github.com/divoxx/scinit/issues).
