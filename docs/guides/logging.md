# Logging

An init's own log output shares the container's output streams with your application. Everything the container prints is your application's output, and that is what you want to read. Anything the init adds has to stay out of the way: it must not touch the app's stdout, it must not fill the logs during normal operation, and it must be easy to tell apart from the app's own lines. But when something goes wrong, such as a restart that didn't happen or a shutdown that took 30 seconds, the init is the only one that knows why.

So scinit is quiet by default and detailed on request. It only prints warnings and errors unless you ask for more with the `SCINIT_LOG` environment variable. It writes to stderr only, never stdout, and every line says `scinit` right after its status tag.

```mermaid
flowchart LR
    scinit["scinit"] -- "events that pass the SCINIT_LOG filter" --> err["stderr"]
    child["your program"] -- "its own output" --> out["stdout"]
    child -- "its own diagnostics" --> err
    out --> logs["container log"]
    err --> logs
```

## The format

Each event is one line without a timestamp: a status tag, `scinit:`, and the message.

```text
[info]  scinit: Spawning process: server []
  [ok]  scinit: Process spawned with PID: 15
[warn]  scinit: Graceful shutdown timeout, forcing kill
[fail]  scinit: Failed to spawn process 'server': No such file or directory (os error 2)
 [dbg]  scinit::reaper: reaped zombie process 16 with exit status 0
```

The tag is right-aligned in a 6-character column, so the messages line up at column 8. It shows the event's level:

| Tag | Level | Color on a terminal |
|---|---|---|
| `[fail]` | error | red |
| `[warn]` | warning | yellow |
| `[info]` | info | cyan |
| `[ok]` | info: a step that succeeded (the child spawned, the ports bound, file watching started) | green |
| `[dbg]` | debug | dimmed |
| `[trc]` | trace | dimmed |

At `debug` and `trace`, the module that logged the event replaces `scinit:`, such as `scinit::reaper:` or `scinit::file_watcher:`, which is the name to use in a per-module `SCINIT_LOG` directive (see below). Events from scinit's main module show `scinit:` at every level. Either way, the first word after the tag is always `scinit`, so scinit's lines are easy to pick out of the child's output, which is mixed into the same stderr, and to find with `grep scinit`:

```text
  [ok]  scinit: Process spawned with PID: 15
server: started, pid 15, config v1
[info]  scinit: File changed: "/app/config/app.conf", triggering restart
```

A message that spans several lines continues on the next ones at column 8, so the extra lines stay visibly part of the event.

There is no timestamp because container log drivers already record one per line, and a second one only adds noise. tini, dumb-init and catatonit leave it out for the same reason.

Output is colored only when stderr is a terminal and the `NO_COLOR` environment variable is unset. Only the tag gets a color; `scinit:` is dimmed and the message is plain. Container logs and files never get escape codes, and `NO_COLOR=1` turns color off on a terminal too. `CLICOLOR_FORCE` set to anything other than `0` turns color on when stderr isn't a terminal, for example when a tool that shows colors reads scinit's output through a pipe; `NO_COLOR` still wins over it.

Fatal errors use the same format. When scinit can't start, the reason is a single `[fail]` line and scinit exits with code 1. This line is printed whatever `SCINIT_LOG` says, even `off`, so an exit with code 1 always comes with a reason:

```console
$ scinit -- nonexistent-cmd
[fail]  scinit: Failed to spawn process 'nonexistent-cmd': No such file or directory (os error 2)
```

Panics, which would be bugs in scinit, are logged the same way, and also whatever `SCINIT_LOG` says, as a `[fail]` line that starts with `panic at` followed by the source location and the message, instead of Rust's default panic output.

## Choosing what to see

`SCINIT_LOG` takes the filter syntax of `tracing`'s `EnvFilter`. The simplest form is a level: `error`, `warn` (the default), `info`, `debug` or `trace`. Each level includes the ones above it. At `warn`, a normal run prints nothing; warnings report things such as a child killed after the graceful timeout or a signal that couldn't be forwarded.

`info` is the useful level for watching what scinit does. It shows each spawn, each signal and how it was handled, each restart and the exit code. `debug` adds detail such as every raw file system event, each zombie reaped, and the signal and terminal setup at startup.

You can also set the level per module, by naming the target: `scinit` for the main module, or a module such as `scinit::process_manager`, `scinit::port_manager`, `scinit::file_watcher` or `scinit::reaper`, as shown on `[dbg]` lines. Directives are separated by commas, and the most specific one wins. A common combination is a general level plus detail for one module:

```sh
SCINIT_LOG=info,scinit::file_watcher=debug
```

Include a general level whenever you name a module. A filter made only of module directives, such as `SCINIT_LOG=scinit::file_watcher=debug`, turns off everything else, including errors from other modules, except the fatal error that ends scinit.

`RUST_LOG` is not read by scinit. It is passed to the child unchanged, so setting it for a Rust application doesn't make scinit verbose, and setting `SCINIT_LOG` doesn't affect your application:

```console
$ RUST_LOG=info scinit -- sh -c 'echo child sees RUST_LOG=$RUST_LOG'
child sees RUST_LOG=info
```

## Troubleshooting

### My app didn't restart

Start with `SCINIT_LOG=info,scinit::file_watcher=debug`. At startup, look for the line naming the watched path:

```text
[info]  scinit: Started watching path: "/app/config"
```

If it is missing, live reload isn't on. `--watch-path`, `--debounce-ms` and `--restart-delay-ms` are silently ignored without `--live-reload`. If the path is not the one you expected, remember that without `--watch-path` scinit watches the command's executable, which for an interpreted app is the interpreter.

Then make the change and look at the events. This run on Linux watched `/app/config` and made three changes: a `touch` of `app.conf`, a write to a file in a subdirectory, and a write to `app.conf` itself. The output is shortened to the relevant lines, and the `attr:` fields at the end of each event are cut:

```text
+ touch config/app.conf
 [dbg]  scinit::file_watcher: File system event: Event { kind: Modify(Metadata(Any)), paths: ["/app/config/app.conf"], ...
+ echo x > config/nested/extra.conf
+ echo v5 > config/app.conf
 [dbg]  scinit::file_watcher: File system event: Event { kind: Modify(Data(Any)), paths: ["/app/config/app.conf"], ...
[info]  scinit: File changed: "/app/config/app.conf", triggering restart
```

Each case shows one of the reasons a change doesn't cause a restart. The `touch` produced an event, but a metadata-only one, which scinit ignores. The write to `config/nested/extra.conf` produced no event at all, because directories are watched non-recursively. Only the content change to a file directly in the watched directory led to `File changed`. A restart arrives `--debounce-ms` after the last change, so a file that keeps changing delays it.

If you see no events at all for a file you are sure changed, and you are watching a single file on Linux, the file may have been replaced by a rename, which loses the watch. Watch the directory instead. The [live reload guide](live-reload.md) covers this and the other cases in detail.

### The container takes 30 seconds to stop

That is the graceful timeout running out: the child didn't exit on the termination signal, so scinit waited `--graceful-timeout-secs` (30 by default) before sending SIGKILL. With `SCINIT_LOG=info` it is easy to confirm. This run used a child that ignores SIGTERM, with the timeout lowered to 3 seconds:

```text
[info]  scinit: received termination signal SIGTERM, initiating graceful shutdown
[info]  scinit: Termination signal SIGTERM received, forwarding to child process (timeout: 3s)
[info]  scinit: Initiating graceful shutdown of process 93999 with SIGTERM
[warn]  scinit: Graceful shutdown timeout, forcing kill
[info]  scinit: Force killing process 93999
[info]  scinit: Process killed, exit status: ExitStatus(unix_wait_status(9))
[info]  scinit: scinit exiting due to termination signal SIGTERM
[info]  scinit: scinit exiting with code 137
```

The `[warn] ... forcing kill` line is the sign. The fix is in the application: handle SIGTERM and exit. Common causes are a shell script wrapper that runs the app without `exec` and doesn't pass the signal on, or a runtime that installs its own handler and waits for open connections. The same timeout applies to every live-reload restart. The [signals and shutdown guide](signals-and-shutdown.md) has the details, and [exit codes](exit-codes.md) explains the 137.

### The client gets connection refused

First check whether scinit is holding the port at all. With `--ports`, the port lines say what was bound and where:

```text
[info]  scinit: Binding 2 ports to 127.0.0.1
[info]  scinit: Bound port 8080 to 127.0.0.1:8080
[info]  scinit: Bound port 8081 to 127.0.0.1:8081
  [ok]  scinit: Successfully bound 2 ports
```

`127.0.0.1` is the default, and it is the most common cause when the client is outside the container: published ports reach the container's external interface, not its loopback. Use `--bind-addr 0.0.0.0` (or `::`). Depending on the container runtime, the client may see an empty reply or a reset instead of a refusal.

If there are no such lines, scinit isn't binding anything and your app opens the port itself. Then the port is closed whenever the app isn't running, including during every live-reload restart, and connections in that window are refused. Passing the port with `--ports` and adopting the inherited socket in the app keeps it open; see [socket activation](socket-activation.md).

If the lines are there and the bind address is right, the app may be ignoring the inherited socket and binding the port itself, which fails because scinit already holds it, or listening on a different port. Check the app's own startup output.

## Things to know

In `EnvFilter` syntax, a bare word that isn't a level is a target name, so `SCINIT_LOG=inf` means "show events from a target named `inf`", which matches nothing. scinit keeps that meaning, since you may be selecting a target on purpose, but warns about any bare word that isn't a level name (other than `scinit` itself) at startup. The warning and the fatal error are shown even when the filter would hide them, so a typo can't make scinit exit 1 without a word:

```console
$ SCINIT_LOG=inf scinit -- nonexistent-cmd
[warn]  scinit: SCINIT_LOG: "inf" is not a level (error, warn, info, debug, trace, off), so it selects the target named "inf"
[fail]  scinit: Failed to spawn process 'nonexistent-cmd': No such file or directory (os error 2)
```

A value that can't be parsed at all is ignored with a warning, and the default `warn` applies.

scinit has no log file, no JSON output and no syslog support. Its lines go to stderr alongside your application's, and the container runtime collects both.

## Related

[Getting started](../getting-started.md) uses `SCINIT_LOG=info` to watch a shutdown, and the [CLI reference](../reference/cli.md) lists the environment variables scinit reads. The troubleshooting sections lead into [live reload](live-reload.md), [signals and shutdown](signals-and-shutdown.md), [exit codes](exit-codes.md) and [socket activation](socket-activation.md). See also [why an init](why-an-init.md), [zombie reaping](zombie-reaping.md) and [process isolation](process-isolation.md).
