# Live reload

Live reload was designed for remote development environments. When a system has too many services to run on a laptop, development moves to a remote environment, a Docker host or a Kubernetes cluster. It is built for a particular way of working there, described in [remote development environments](remote-development.md): the service then can stay deployed in the cluster and only the code or binary needs to change. No traffic needs to be routed to your laptop, and a code change doesn't need to re-apply manifests or recreate the pod. Instead, you can edit code locally, the changes can be synchronized into the running container and rebuilt there, and the running service then needs to pick up the new build. A typical setup is a development pod in which a sidecar container can recompile the program whenever its sources change and write the new binary into a volume shared with the application container.

That last step, swapping the running process for the new build, is hard to do from outside the container. Restarting the whole container or pod is slow, drops every open connection, and in Kubernetes can mean rescheduling. Running a separate file watcher inside the container is better, but that watcher then becomes PID 1 or has to be supervised by one, and you are back to the problems an init exists to solve.

scinit already sits between the container runtime and your application, so it does this job itself. With `--live-reload`, it watches a path (by default the program's own executable) and, when the contents change, stops the child the same way `docker stop` would and starts a fresh one from the new build. Combined with [socket activation](socket-activation.md), clients connecting during the restart, whether a browser, a `kubectl port-forward` or another service in the cluster, wait in the kernel's queue instead of being refused.

## How a change becomes a restart

Two things happen between a file being written and the new process starting: the change is debounced, then the child is restarted.

Editors and build tools rarely write a file once. A save can be a truncate followed by several writes, a compiler may write an output in chunks, and a `git checkout` touches many files in a burst. Restarting on the first event would start the new process against a half-written file, and restarting on every event would restart many times for one change. scinit uses a trailing-edge debounce instead. Each relevant change arms a timer of `--debounce-ms` (500 ms by default). Another change before the timer fires re-arms it from zero. Only when the path has been quiet for the full interval does scinit restart, so the restart always sees the last write.

The restart itself reuses scinit's normal shutdown path. scinit sends SIGTERM to the child's process group and waits up to `--graceful-timeout-secs` (30 s by default) for the child to exit. If it is still running, scinit sends SIGKILL. It then waits `--restart-delay-ms` (1000 ms by default) and spawns the command again with the same arguments and environment.

```mermaid
sequenceDiagram
    participant FS as Watched path
    participant W as Debounce timer
    participant S as scinit
    participant Old as Old child
    participant New as New child

    FS->>W: write (arms 500 ms timer)
    FS->>W: write (re-arms timer)
    FS->>W: write (re-arms timer)
    Note over W: 500 ms with no further changes
    W->>S: file changed
    S->>Old: SIGTERM to process group
    alt exits within --graceful-timeout-secs
        Old-->>S: exited
    else still running
        S->>Old: SIGKILL to process group
    end
    Note over S: sleep --restart-delay-ms
    S->>New: spawn the same command
```

## Choosing what to watch

`--watch-path` takes a single file or directory. If you leave it out, scinit watches the executable it is about to run, looked up in `PATH` the same way exec would. That default is convenient for compiled programs, but it is a single file, and on Linux a single-file watch stops working once a build replaces the file by renaming a new one over it (see [Things to know](#things-to-know)). Passing the build output directory with `--watch-path` is the reliable choice. The default does not suit interpreted programs at all. For `scinit --live-reload -- python app.py`, the executable is the Python interpreter, which never changes, so you need `--watch-path` pointing at your source.

Directories are watched non-recursively. scinit sees changes to files directly inside the directory, but not in its subdirectories. If your code lives in a tree, point `--watch-path` at the directory whose files change, such as the build output directory, rather than at the project root.

Not every event counts. A change triggers a restart if it modifies a file's contents or renames a file. Renames matter because many editors save by writing a temporary file and renaming it over the original. Creating an empty file, deleting one, and metadata-only changes such as `touch`, `chmod` or a new timestamp do not count. scinit also checks that the path in the event is a regular file when the event arrives, so changes to directories, and to files that are already gone, are ignored.

## Worked example

This example runs a small shell script, `/app/server`, that prints its config and exits cleanly on SIGTERM. The config lives in `/app/config/app.conf`, so that directory is what we watch. We write the config three times, 200 ms apart, then stop scinit with SIGTERM. The output was captured on Linux, with `SCINIT_LOG=info` to show scinit's own log lines. The lines starting with `server:` come from the child.

```console
$ SCINIT_LOG=info scinit --live-reload --watch-path /app/config -- server
 INFO scinit: scinit starting
 INFO scinit: init system started, managing subprocess: server
 INFO scinit::file_watcher: Started watching path: "/app/config"
 INFO scinit: File watching started for live-reload
 INFO scinit::process_manager: Spawning process: server []
 INFO scinit::process_manager: Process spawned with PID: 15
server: started, pid 15, config v1
 INFO scinit: File changed: "/app/config/app.conf", triggering restart
 INFO scinit::process_manager: Restarting process due to file change
 INFO scinit::process_manager: Initiating graceful shutdown of process 15 with SIGTERM
server: got SIGTERM, exiting
 INFO scinit::process_manager: Process exited gracefully
 INFO scinit::process_manager: Spawning process: server []
 INFO scinit::process_manager: Process spawned with PID: 45
server: started, pid 45, config v4
 INFO scinit: received termination signal SIGTERM, initiating graceful shutdown
 INFO scinit: Termination signal SIGTERM received, forwarding to child process (timeout: 30s)
 INFO scinit::process_manager: Initiating graceful shutdown of process 45 with SIGTERM
server: got SIGTERM, exiting
 INFO scinit::process_manager: Process exited gracefully
 INFO scinit: scinit exiting due to termination signal SIGTERM
 INFO scinit: scinit exiting with code 0
```

The three writes produced one restart, and the new process saw the last version (`v4`). The pause between `Process exited gracefully` and the next `Spawning process` is the one-second restart delay.

Without `--watch-path`, the watched path is the resolved executable:

```console
$ SCINIT_LOG=info scinit --live-reload -- server
 INFO scinit: scinit starting
 INFO scinit: init system started, managing subprocess: server
 INFO scinit::file_watcher: Started watching path: "/app/server"
 ...
```

If the command can't be found in `PATH`, scinit refuses to start rather than watching nothing:

```console
$ scinit --live-reload -- my-app
ERROR scinit: --live-reload: cannot find 'my-app' in PATH to watch; pass --watch-path
```

In a development container, the usual setup mounts the source or build output and watches it, with socket activation so the port stays open:

```sh
scinit --live-reload --watch-path /app/bin \
       --ports 8080 --bind-addr 0.0.0.0 \
       -- /app/bin/server
```

## Why it pairs with socket activation

When a server binds its own port, there is a gap during every restart. The old process closes its listening socket as it exits, the restart delay passes, and the new process binds the port again once it has started. A client that connects inside that gap gets "connection refused", and for a browser refreshing as you save, that gap is exactly when it connects.

With `--ports`, scinit binds the listening sockets itself, once, and hands the same sockets to every child. While no child is running, the sockets stay open and the kernel keeps accepting connections into their backlog. When the new child starts, it accepts them. The client sees a slower response instead of an error. The [socket activation guide](socket-activation.md) shows a connection made in the middle of a restart being answered by the new process.

## Things to know

If the child exits on its own, whether it crashed or finished cleanly, scinit exits with the child's status, even with live reload on. It does not wait for the next file change. This is deliberate: in a container, a crash should end the container so the orchestrator sees it and can restart or report it, rather than leaving a running container with nothing inside. The cost is in development loops, where a syntax error that makes the app exit immediately also stops the container. Let your container runtime restart it, or have your app stay up and report the error instead of exiting.

```console
$ SCINIT_LOG=info scinit --live-reload --watch-path config -- sh -c 'exit 3'
 INFO scinit: scinit starting
 INFO scinit: init system started, managing subprocess: sh
 INFO scinit::file_watcher: Started watching path: "config"
 INFO scinit: File watching started for live-reload
 INFO scinit::process_manager: Spawning process: sh ["-c", "exit 3"]
 INFO scinit::process_manager: Process spawned with PID: 95545
 INFO scinit::exit_status: Child process exited with error code 3, scinit exiting
 INFO scinit: scinit exiting with code 3
$ echo $?
3
```

The restart always sends SIGTERM, and it waits the full graceful timeout for a child that ignores it. With the default of 30 seconds, a server that doesn't handle SIGTERM makes every reload take 30 seconds. In development, either handle SIGTERM or lower `--graceful-timeout-secs`.

`--watch-path`, `--debounce-ms` and `--restart-delay-ms` only take effect with `--live-reload`. Without it they are accepted and silently ignored, so a missing `--live-reload` shows up as "nothing restarts", not as an error.

Because the debounce is trailing-edge, a path that keeps changing more often than the debounce interval never triggers a restart until it settles. Changes that arrive while a restart is in progress are not lost: they cause one more restart once the current one completes. A consequence is that if your app writes into the directory being watched, for example a log or PID file, every start causes a change and the app restarts in a loop. Keep such files outside the watched path. Editors can cause the same thing: a swap or backup file written next to the file you are editing, such as Vim's `.swp`, counts as a content change in a watched directory.

Watching a single file is fragile on Linux when the file is replaced by renaming a new one over it, which is how many editors save and how some build tools write their output. inotify watches the file itself, not its name, so after the rename the watch follows the old, deleted file. In our tests on Linux, replacing a watched file this way caused no restart, and later changes to the new file were not seen either. This also applies to the default watch path, which is the executable and so a single file. Watching the containing directory does not have this problem, so on Linux watch a directory: the directory your source or config files live in, or the one your build writes its output to. Watching a single file through its parent directory, so it survives replacement, is planned in [#19](https://github.com/divoxx/scinit/issues/19).

On macOS, file events come from FSEvents, which does not always report exactly what happened. It can report a file written shortly before scinit started as changed, which causes one restart right after startup. In our tests it also reported earlier writes along with later metadata-only changes, so a `touch` or `chmod` on a recently written file could restart the child, which doesn't happen on Linux. Inside a Linux container, even on a Mac host, you get inotify's behavior.

## Related

[Getting started](../getting-started.md) walks through a first live-reload loop, and the [CLI reference](../reference/cli.md) lists every flag and default. [Socket activation](socket-activation.md) covers keeping ports open across restarts, [signals and shutdown](signals-and-shutdown.md) covers the SIGTERM and SIGKILL sequence restarts reuse, [exit codes](exit-codes.md) explains what scinit exits with when the child stops on its own, and [logging](logging.md) shows how to see what the watcher is doing. For background, see [why an init](why-an-init.md), [zombie reaping](zombie-reaping.md) and [process isolation](process-isolation.md).
