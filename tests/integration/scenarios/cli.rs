//! Command-line parsing and argument passthrough

use crate::integration::harness::{Scinit, SCINIT, TEST_CHILD, TIMEOUT};
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
    assert!(stdout.contains("Usage"), "help output missing usage:\n{}", stdout);
    assert!(stdout.contains("--live-reload"), "help output missing flags:\n{}", stdout);
    assert!(stdout.contains("SCINIT_LOG"), "help output missing SCINIT_LOG:\n{}", stdout);
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
    assert!(stderr.contains("COMMAND") || stderr.contains("command"), "{}", stderr);
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
        scinit.stderr().contains("[scinit] INFO scinit starting"),
        "{}",
        scinit.diagnostics()
    );
    assert_eq!(scinit.stdout(), "child-output\n", "{}", scinit.diagnostics());
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
    assert_eq!(scinit.stdout(), "child RUST_LOG=debug\n", "{}", scinit.diagnostics());
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
    assert!(stderr.contains("[scinit] DEBUG"), "{}", scinit.diagnostics());
    assert!(!stderr.contains('\x1b'), "escape codes in logs:\n{:?}", stderr);
}

/// Fatal errors use the same format as other logs
#[test]
fn fatal_errors_are_prefixed() {
    let (scinit, status) = Scinit::builder()
        .command(["/nonexistent/scinit-no-such-binary"])
        .run(TIMEOUT)
        .unwrap();
    scinit.assert_exit_code(status, 1);
    let stderr = scinit.stderr();
    assert!(
        stderr.starts_with("[scinit] ERROR Failed to spawn process"),
        "{}",
        scinit.diagnostics()
    );
    assert!(!stderr.contains('\x1b'), "escape codes in error:\n{:?}", stderr);
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
