//! Command-line parsing and argument passthrough

use crate::integration::harness::{Scinit, SCINIT, TEST_CHILD, TIMEOUT};
use nix::sys::signal::Signal;
use std::process::{Command, Output};
use std::time::Duration;

/// Run scinit directly (no child expected) and capture its output
fn scinit_output(args: &[&str]) -> Output {
    Command::new(SCINIT)
        .args(args)
        .output()
        .expect("failed to run scinit")
}

/// `--help` prints usage and exits 0
#[test]
fn help_exits_zero() {
    let out = scinit_output(&["--help"]);
    assert_eq!(out.status.code(), Some(0), "{:?}", out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Usage"),
        "help output missing usage:\n{}",
        stdout
    );
    assert!(
        stdout.contains("--live-reload"),
        "help output missing flags:\n{}",
        stdout
    );
    assert!(
        stdout.contains("SCINIT_LOG"),
        "help output missing SCINIT_LOG:\n{}",
        stdout
    );
}

/// `--version` prints the crate version and exits 0
#[test]
fn version_exits_zero() {
    let out = scinit_output(&["--version"]);
    assert_eq!(out.status.code(), Some(0), "{:?}", out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(env!("CARGO_PKG_VERSION")),
        "version output missing {}:\n{}",
        env!("CARGO_PKG_VERSION"),
        stdout
    );
}

/// Without a command clap reports a usage error (exit 2)
#[test]
fn missing_command_exits_two() {
    let out = scinit_output(&[]);
    assert_eq!(out.status.code(), Some(2), "{:?}", out);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("COMMAND") || stderr.contains("command"),
        "{}",
        stderr
    );
}

/// An unparseable `--bind-addr` fails config validation (exit 1) before the child starts
#[test]
fn invalid_bind_addr_exits_one() {
    let (scinit, status) = Scinit::builder()
        .args(["--bind-addr", "not-an-ip"])
        .child(["exit", "0"])
        .run(TIMEOUT)
        .unwrap();
    scinit.assert_exit_code(status, 1);
    scinit.assert_start_count(0, "child must not start with an invalid config");
    assert!(
        scinit.stderr().contains("Invalid bind address"),
        "{}",
        scinit.diagnostics()
    );
}

/// scinit logs to stderr, leaving stdout to the child
#[test]
fn logs_go_to_stderr() {
    let (scinit, status) = Scinit::builder()
        .env("SCINIT_LOG", "info")
        .command(["echo", "child-output"])
        .run(TIMEOUT)
        .unwrap();
    scinit.assert_exit_code(status, 0);
    assert!(
        scinit.stderr().contains("INFO scinit: scinit starting"),
        "{}",
        scinit.diagnostics()
    );
    assert_eq!(
        scinit.stdout(),
        "child-output\n",
        "{}",
        scinit.diagnostics()
    );
}

/// `RUST_LOG` is the child's: it doesn't change scinit's verbosity and
/// reaches the child unchanged
#[test]
fn rust_log_is_left_to_the_child() {
    let (scinit, status) = Scinit::builder()
        .env("RUST_LOG", "debug")
        .command(["sh", "-c", "echo \"child RUST_LOG=$RUST_LOG\""])
        .run(TIMEOUT)
        .unwrap();
    scinit.assert_exit_code(status, 0);
    assert_eq!(
        scinit.stdout(),
        "child RUST_LOG=debug\n",
        "{}",
        scinit.diagnostics()
    );
    // The child writes nothing to stderr, so any output there is scinit's
    assert!(
        scinit.stderr().is_empty(),
        "scinit logged at RUST_LOG's level\n{}",
        scinit.diagnostics()
    );
}

/// Off a terminal, log lines carry no ANSI color codes
#[test]
fn logs_have_no_color_when_not_a_terminal() {
    let (scinit, status) = Scinit::builder()
        .env("SCINIT_LOG", "debug")
        .command(["true"])
        .run(TIMEOUT)
        .unwrap();
    scinit.assert_exit_code(status, 0);
    let stderr = scinit.stderr();
    assert!(stderr.contains("DEBUG scinit"), "{}", scinit.diagnostics());
    assert!(
        !stderr.contains('\x1b'),
        "escape codes in logs:\n{:?}",
        stderr
    );
}

/// Fatal errors use the same format as other logs
#[test]
fn fatal_errors_are_logged_as_events() {
    let (scinit, status) = Scinit::builder()
        .command(["/nonexistent/scinit-no-such-binary"])
        .run(TIMEOUT)
        .unwrap();
    scinit.assert_exit_code(status, 1);
    let stderr = scinit.stderr();
    assert!(
        stderr.starts_with("ERROR scinit: Failed to spawn process"),
        "{}",
        scinit.diagnostics()
    );
    assert!(
        !stderr.contains('\x1b'),
        "escape codes in error:\n{:?}",
        stderr
    );
}

/// Args after the command, including ones that look like scinit flags, reach the child verbatim
#[test]
fn trailing_and_hyphenated_args_reach_child() {
    let child_args = [
        "dump",
        "--then-exit",
        "--ports",
        "1",
        "-x",
        "--foo=bar",
        "a b",
        "--",
        "--live-reload",
    ];
    let (scinit, status) = Scinit::builder().child(child_args).run(TIMEOUT).unwrap();
    assert!(status.success(), "{:?}\n{}", status, scinit.diagnostics());
    scinit.wait_for_dump();

    let argv: Vec<String> = scinit
        .events_named("arg")
        .iter()
        .map(|e| e.get("value").unwrap_or_default().to_string())
        .collect();
    let mut expected = vec![TEST_CHILD.to_string()];
    expected.extend(child_args.iter().map(|s| s.to_string()));
    assert_eq!(argv, expected, "{}", scinit.diagnostics());
}

/// `--watch-path` is ignored unless `--live-reload` is set: even a nonexistent
/// path doesn't stop the child from starting and scinit keeps running
#[test]
fn watch_path_without_live_reload_is_ignored() {
    let builder = Scinit::builder();
    let missing = builder.dir().join("does-not-exist");
    let (mut scinit, _) = builder
        .args(["--watch-path".into(), missing.into_os_string()])
        .child(["run"])
        .start();
    scinit.assert_running_for(Duration::from_millis(1000));
    scinit.assert_start_count(1, "--watch-path alone must not restart the child");
}

/// A command that cannot be executed makes scinit exit 1
#[test]
fn nonexistent_command_exits_one() {
    let (scinit, status) = Scinit::builder()
        .command(["/nonexistent/scinit-no-such-binary"])
        .run(TIMEOUT)
        .unwrap();
    scinit.assert_exit_code(status, 1);
}

/// Run scinit with stderr on a pseudo-terminal and return what it wrote there
fn stderr_on_terminal(envs: &[(&str, &str)]) -> String {
    use std::io::Read;
    let pty = nix::pty::openpty(None, None).expect("openpty");
    let mut cmd = Command::new(SCINIT);
    cmd.args(["true"])
        .env("SCINIT_LOG", "info")
        .env_remove("NO_COLOR")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(pty.slave));
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("failed to run scinit");
    // The command holds our copy of the slave; drop it so reading the master
    // ends once scinit (and its child) exit
    drop(cmd);
    let mut out = Vec::new();
    let mut master = std::fs::File::from(pty.master);
    // Linux reports the closed slave as EIO rather than EOF
    let _ = master.read_to_end(&mut out);
    child.wait().unwrap();
    String::from_utf8_lossy(&out).into_owned()
}

/// On a terminal, log lines are colored (here: the green INFO level)
#[test]
fn logs_are_colored_on_a_terminal() {
    let out = stderr_on_terminal(&[]);
    assert!(
        out.contains("\x1b[32m INFO\x1b[0m"),
        "no colored level in:\n{:?}",
        out
    );
}

/// `NO_COLOR` turns color off even on a terminal
#[test]
fn no_color_disables_color_on_a_terminal() {
    let out = stderr_on_terminal(&[("NO_COLOR", "1")]);
    assert!(out.contains("INFO scinit: scinit starting"), "{:?}", out);
    assert!(
        !out.contains('\x1b'),
        "escape codes despite NO_COLOR:\n{:?}",
        out
    );
}

/// A zero `--zombie-reap-interval-ms` is a usage error (exit 2), not a panic, and the child never starts
#[test]
fn zero_zombie_reap_interval_exits_two() {
    let (scinit, status) = Scinit::builder()
        .args(["--zombie-reap-interval-ms", "0"])
        .child(["exit", "0"])
        .run(TIMEOUT)
        .unwrap();
    scinit.assert_exit_code(status, 2);
    scinit.assert_start_count(0, "child must not start with an invalid config");
    assert!(
        scinit.stderr().contains("--zombie-reap-interval-ms"),
        "{}",
        scinit.diagnostics()
    );
}

/// Write a script to `dir` that prints each of its arguments on its own line
fn print_args_script(dir: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("print-args");
    std::fs::write(&path, "#!/bin/sh\nprintf '%s\\n' \"$@\"\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Flags right after the command name, even ones scinit has, reach the child
/// instead of being parsed by scinit
#[test]
fn flags_right_after_the_command_reach_child() {
    let builder = Scinit::builder();
    let script = print_args_script(&builder.dir());
    let (scinit, status) = builder
        .command([
            script.into_os_string(),
            "--help".into(),
            "-V".into(),
            "--ports".into(),
            "9".into(),
            "hi".into(),
        ])
        .run(TIMEOUT)
        .unwrap();
    scinit.assert_exit_code(status, 0);
    assert_eq!(
        scinit.stdout(),
        "--help\n-V\n--ports\n9\nhi\n",
        "{}",
        scinit.diagnostics()
    );
}

/// scinit's options before the command are its own; the same option after
/// the command is the child's
#[test]
fn options_split_at_the_command_name() {
    let builder = Scinit::builder();
    let script = print_args_script(&builder.dir());
    let (scinit, status) = builder
        .args(["--graceful-timeout-secs", "3"])
        .command([
            script.into_os_string(),
            "--graceful-timeout-secs".into(),
            "not-a-number".into(),
        ])
        .run(TIMEOUT)
        .unwrap();
    scinit.assert_exit_code(status, 0);
    assert_eq!(
        scinit.stdout(),
        "--graceful-timeout-secs\nnot-a-number\n",
        "{}",
        scinit.diagnostics()
    );
}

/// After `--`, the command and all its args (including another `--`) reach the child
#[test]
fn double_dash_before_the_command_is_consumed_once() {
    let builder = Scinit::builder();
    let script = print_args_script(&builder.dir());
    let (scinit, status) = builder
        .args(["--"])
        .command([script.into_os_string(), "--".into(), "--help".into()])
        .run(TIMEOUT)
        .unwrap();
    scinit.assert_exit_code(status, 0);
    assert_eq!(scinit.stdout(), "--\n--help\n", "{}", scinit.diagnostics());
}

/// A misspelled scinit option is a usage error (exit 2), not taken as the command
#[test]
fn misspelled_option_exits_two() {
    let (scinit, status) = Scinit::builder()
        .args(["--live-relaod"])
        .child(["exit", "0"])
        .run(TIMEOUT)
        .unwrap();
    scinit.assert_exit_code(status, 2);
    scinit.assert_start_count(0, "child must not start after a usage error");
    assert!(
        scinit.stderr().contains("--live-relaod"),
        "{}",
        scinit.diagnostics()
    );
}

/// Run scinit with `SCINIT_LOG=value` on a command that doesn't exist, and
/// return its stderr
fn spawn_failure_stderr(value: &str) -> String {
    let (scinit, status) = Scinit::builder()
        .env("SCINIT_LOG", value)
        .command(["/nonexistent/scinit-no-such-binary"])
        .run(TIMEOUT)
        .unwrap();
    scinit.assert_exit_code(status, 1);
    scinit.stderr()
}

/// The error that ends scinit is printed whatever SCINIT_LOG says, so exit
/// code 1 always comes with a reason
#[test]
fn fatal_error_is_shown_whatever_scinit_log_says() {
    for value in ["", "off", "scinit::reaper=debug", "inf"] {
        let stderr = spawn_failure_stderr(value);
        assert!(
            stderr.contains("ERROR scinit: Failed to spawn process"),
            "no fatal error with SCINIT_LOG={:?}:\n{}",
            value,
            stderr
        );
    }
}

/// A bare word that isn't a level (`inf`) is ignored with a warning, and the
/// default applies, instead of silently matching nothing
#[test]
fn mistyped_scinit_log_warns_and_uses_the_default() {
    let stderr = spawn_failure_stderr("inf");
    assert!(
        stderr.contains("WARN scinit::logging: ignoring SCINIT_LOG: \"inf\" is not a level"),
        "{}",
        stderr
    );
}

/// The default level is warn: a forced kill is reported without SCINIT_LOG
#[test]
fn default_level_shows_warnings() {
    let (mut scinit, _) = Scinit::builder()
        .args(["--graceful-timeout-secs", "1"])
        .child(["run", "--ignore", "TERM"])
        .start();
    scinit.signal(Signal::SIGTERM).unwrap();
    let status = scinit.wait_exit(TIMEOUT).unwrap();
    scinit.assert_exit_code(status, 137);
    assert!(
        scinit
            .stderr()
            .contains("WARN scinit::process_manager: Graceful shutdown timeout, forcing kill"),
        "{}",
        scinit.diagnostics()
    );
}
