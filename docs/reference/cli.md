# Command-line reference

This page lists everything you can pass to scinit and everything it reports back: flags, positional arguments, environment variables, and exit codes. It is meant for looking things up. The guides linked at the end explain the behavior behind each option.

## Synopsis

```
scinit [OPTIONS] [--] <COMMAND> [ARGS]...
```

scinit takes its own options first, then the command to run and its arguments. scinit stops reading options at the first argument that isn't one of its own: that argument is the command, and it and everything after it reach the child unchanged, including arguments that look like scinit flags. `scinit echo --help` runs `echo --help`, and `scinit my-server --ports 9` passes `--ports 9` to `my-server`.

`--` marks the end of scinit's options explicitly. It is optional, but it makes the split obvious to a reader, and it is required when the command name itself starts with `-`. The examples in these docs always write it.

A misspelled scinit option before the command is still an error (exit 2), not mistaken for the command.

## Positional arguments

| Argument | Description |
|---|---|
| `<COMMAND>` | The program to run as the child. Required. A name without a `/` is looked up in `PATH` when the child is spawned. If it can't be found or executed, scinit logs the error and exits with code 1. |
| `[ARGS]...` | Arguments passed to the child unchanged. |

## Options

| Option | Default | Description |
|---|---|---|
| `--graceful-timeout-secs <N>` | `30` | After a termination signal (or a live-reload restart), how many seconds to wait for the child to exit before sending SIGKILL to its process group. |
| `--zombie-reap-interval-ms <N>` | `5000` | Interval of the periodic sweep that reaps orphaned processes. Orphans are also reaped as soon as SIGCHLD arrives, so this is a fallback. Must be at least 1; `0` is a usage error. |
| `--live-reload` | off | Restart the child when `--watch-path` changes. |
| `--watch-path <PATH>` | the command's executable | The file, or directory (not recursive), to watch for changes. Without it, scinit watches the executable that `<COMMAND>` refers to, looked up in `PATH` the way exec does, and exits with an error if it can't find it. |
| `--debounce-ms <N>` | `500` | How long the watched path has to stay quiet after a change before the restart happens. Every new change starts the wait over. |
| `--restart-delay-ms <N>` | `1000` | Pause between the old child exiting and the new one starting during a live-reload restart. |
| `--ports <PORT>[,<PORT>...]` | none | TCP ports to bind and pass to the child as file descriptors 3, 4, ... in the order given. Takes a comma-separated list, and the flag can be repeated (`--ports 8080 --ports 8081` is the same as `--ports 8080,8081`). Each port must be between 0 and 65535. |
| `--bind-addr <ADDR>` | `127.0.0.1` | The address `--ports` are bound on. Must be an IP address literal, IPv4 or IPv6 (`0.0.0.0`, `::`, `::1`); a hostname such as `localhost` is rejected. |
| `--reuse-port` | off | Set `SO_REUSEPORT` on the `--ports` sockets, so other processes that also set it can bind the same ports. `SO_REUSEADDR` is always set. |
| `-h`, `--help` | | Print help, including a summary of `SCINIT_LOG`, and exit with code 0. |
| `-V`, `--version` | | Print the version and exit with code 0. |

`--watch-path`, `--debounce-ms` and `--restart-delay-ms` only take effect together with `--live-reload`. Without it they are accepted and ignored: no error, no warning, and nothing is watched. Since a child that exits is never restarted, the restart delay has no use outside live reload either.

`--bind-addr` is parsed at startup even when `--ports` is not given, so an invalid address is always an error:

```
$ scinit --bind-addr localhost --ports 8080 -- my-server
ERROR scinit: Invalid bind address 'localhost': invalid IP address syntax
$ echo $?
1
```

The ports themselves are bound just before the first child is spawned. A port that can't be bound (already in use, or below 1024 without the privilege to bind it) ends scinit with exit code 1 before the child ever starts.

## Environment variables scinit reads

| Variable | Effect |
|---|---|
| `SCINIT_LOG` | Filter for scinit's own log output on stderr, in [tracing's `EnvFilter` syntax](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html): a level (`error`, `warn`, `info`, `debug`, `trace`) and/or per-module directives such as `scinit::file_watcher=debug`. The default is `warn`, used when the variable is unset or empty. A value that can't be parsed, or that has a bare word that isn't a level (such as `inf`), is ignored with a warning and the default applies; see [Logging](../guides/logging.md#things-to-know). The fatal error that ends scinit is printed whatever the filter. |
| `NO_COLOR` | When set (to any value), scinit's log lines are never colored. Without it, they are colored only when stderr is a terminal. |
| `PATH` | Used to find `<COMMAND>` when it contains no `/`, both to spawn it and to pick the default `--watch-path`. |

scinit does not read `RUST_LOG`. It is passed to the child unchanged, so it configures your program's logging and not scinit's.

## Environment the child receives

The child inherits scinit's whole environment, with one exception: every variable whose name starts with `LISTEN_` is removed, because whatever scinit itself inherited describes somebody else's sockets. With `--ports`, scinit then sets the two variables of the systemd socket-activation protocol.

| Variable | Set when | Value |
|---|---|---|
| `LISTEN_FDS` | `--ports` is given | The number of sockets, which start at file descriptor 3. |
| `LISTEN_PID` | `--ports` is given | The child's own pid, so the check in `sd_listen_fds()` passes. |

Without `--ports`, the child sees no `LISTEN_*` variables at all. scinit does not set `LISTEN_FDNAMES`. There is no option for setting other variables for the child; set them in scinit's own environment (for example with `ENV` in a Dockerfile) and the child inherits them.

## Exit codes

scinit's exit code is designed to be the one your program would have produced if it had been the container's command.

| Code | When |
|---|---|
| The child's exit code | The child exited normally, on its own or after a forwarded signal. |
| 128 + signal number | The child was killed by a signal, for example 130 for SIGINT, 137 for SIGKILL, 143 for SIGTERM. Also used when scinit received a termination signal and the child's exit could not be observed even after SIGKILL; the number is then that of the signal scinit received. |
| 1 | An error in scinit itself: the command could not be found or executed (at startup or on a live-reload restart), `--bind-addr` is not an IP address, a port could not be bound, `--live-reload` found no executable to watch, the watch could not be set up, or the terminal could not be handed to the child. |
| 2 | A usage error reported by the argument parser, such as a missing `<COMMAND>`, an unknown option, or an out-of-range value. |
| 0 | `--help` or `--version`. |

Because the child's codes pass straight through, a child that itself exits with 1 or 2 can't be told apart from a scinit error by the code alone. scinit's own errors are always logged on stderr as an `ERROR scinit: ...` line, whatever `SCINIT_LOG` says, and usage errors are printed by the parser with a `Usage:` hint.

```
$ scinit -- sh -c 'exit 3'; echo $?
3
$ scinit -- sh -c 'kill -9 $$'; echo $?
137
$ scinit -- no-such-command; echo $?
ERROR scinit: Failed to spawn process 'no-such-command': No such file or directory (os error 2)
1
$ scinit --zombie-reap-interval-ms 0 -- my-server; echo $?
error: invalid value '0' for '--zombie-reap-interval-ms <ZOMBIE_REAP_INTERVAL_MS>': 0 is not in 1..18446744073709551615

For more information, try '--help'.
2
```

## Related

[Signals and shutdown](../guides/signals-and-shutdown.md) explains `--graceful-timeout-secs`, [Exit codes](../guides/exit-codes.md) the table above, [Zombie reaping](../guides/zombie-reaping.md) `--zombie-reap-interval-ms`, [Live reload](../guides/live-reload.md) the watch options, [Socket activation](../guides/socket-activation.md) `--ports`, `--bind-addr` and `--reuse-port`, and [Logging](../guides/logging.md) `SCINIT_LOG`.
