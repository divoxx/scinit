//! scinit's exit status must mirror its child's

use crate::integration::harness::{
    free_ports, let_setup_writes_age, path_with, Scinit, ScinitBuilder, TEST_CHILD, TIMEOUT,
};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Child exits 0 → scinit exits 0
#[test]
fn child_exit_zero() {
    let (scinit, status) = Scinit::builder().child(["exit", "0"]).run(TIMEOUT).unwrap();
    scinit.child_pid().unwrap();
    scinit.assert_exit_code(status, 0);
}

/// Child exits 42 → scinit exits 42
#[test]
fn child_exit_code_propagates() {
    let (scinit, status) = Scinit::builder()
        .child(["exit", "42"])
        .run(TIMEOUT)
        .unwrap();
    scinit.child_pid().unwrap();
    scinit.assert_exit_code(status, 42);
}

/// Child killed externally by SIGKILL → scinit exits 128 + 9
#[test]
fn child_killed_by_signal_exits_128_plus_signo() {
    let (mut scinit, pid) = Scinit::builder().child(["run"]).start();
    kill(Pid::from_raw(pid), Signal::SIGKILL).unwrap();
    let status = scinit.wait_exit(TIMEOUT).unwrap();
    scinit.assert_exit_code(status, 137);
}

/// Child that raises SIGTERM on itself → scinit exits 128 + 15
#[test]
fn child_self_signal_exits_128_plus_signo() {
    let (scinit, status) = Scinit::builder()
        .child(["kill-self", "TERM"])
        .run(TIMEOUT)
        .unwrap();
    assert!(
        !scinit.events_named("raise").is_empty(),
        "{}",
        scinit.diagnostics()
    );
    scinit.assert_exit_code(status, 143);
}

/// The exit log names the child's signal by its platform number (SIGUSR1 is
/// 10 on Linux, 30 on macOS)
#[test]
fn child_signal_is_logged_by_name() {
    let (scinit, status) = Scinit::builder()
        // SCINIT_LOG once scinit stops reading RUST_LOG (#2); both until then
        .env("SCINIT_LOG", "info")
        .env("RUST_LOG", "info")
        .child(["kill-self", "USR1"])
        .run(TIMEOUT)
        .unwrap();
    scinit.assert_exit_code(status, 128 + Signal::SIGUSR1 as i32);
    let expected = format!("terminated by signal {} (SIGUSR1)", Signal::SIGUSR1 as i32);
    let logs = scinit.stdout() + &scinit.stderr();
    assert!(
        logs.contains(&expected),
        "missing {:?}\n{}",
        expected,
        scinit.diagnostics()
    );
}

/// Repeated clean exits never surface as failures (e.g. the background
/// reaper stealing the child's status from tokio's `wait`)
#[test]
fn repeated_clean_exits_stay_zero() {
    for i in 0..50 {
        let (scinit, status) = Scinit::builder().child(["exit", "0"]).run(TIMEOUT).unwrap();
        assert_eq!(
            status.code(),
            Some(0),
            "run {} exited with {:?}\n{}",
            i,
            status,
            scinit.diagnostics()
        );
    }
}

/// SIGTERM to scinit, child handles it and exits 0 → scinit exits 0
#[test]
fn sigterm_with_clean_child_exit_exits_zero() {
    let (mut scinit, pid) = Scinit::builder().child(["run"]).start();
    scinit.signal(Signal::SIGTERM).unwrap();
    scinit
        .wait_for_signal(pid, Signal::SIGTERM, TIMEOUT)
        .unwrap();
    let status = scinit.wait_exit(TIMEOUT).unwrap();
    scinit.assert_exit_code(status, 0);
}

/// SIGTERM to scinit, child dies from it (default action) → 128 + 15
#[test]
fn sigterm_killing_child_exits_143() {
    let (mut scinit, _) = Scinit::builder().child(["run", "--trap", "USR1"]).start();
    scinit.signal(Signal::SIGTERM).unwrap();
    let status = scinit.wait_exit(TIMEOUT).unwrap();
    scinit.assert_exit_code(status, 143);
}

/// Child ignores SIGTERM and is SIGKILLed after the graceful timeout → 128 + 9
#[test]
fn sigterm_escalation_exits_137() {
    let (mut scinit, _) = Scinit::builder()
        .args(["--graceful-timeout-secs", "1"])
        .child(["run", "--ignore", "TERM"])
        .start();
    scinit.signal(Signal::SIGTERM).unwrap();
    let status = scinit.wait_exit(TIMEOUT).unwrap();
    scinit.assert_exit_code(status, 137);
}

/// Write `content` to `name` in `dir` with mode `mode`
fn write_file(dir: &Path, name: &str, content: &str, mode: u32) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, content).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    path
}

/// scinit fails to spawn `command` (made in the builder's temp dir) and exits
/// `code` with an error containing `message`, the same error with and
/// without `--ports`
fn assert_spawn_failure(command: impl Fn(&ScinitBuilder) -> OsString, code: i32, message: &str) {
    let mut errors = Vec::new();
    for with_ports in [false, true] {
        let mut builder = Scinit::builder();
        if with_ports {
            builder = builder.ports(&free_ports(1));
        }
        let command = command(&builder);
        let (scinit, status) = builder.command([&command]).run(TIMEOUT).unwrap();
        let what = format!("--ports: {}", with_ports);
        scinit.assert_exit_code(status, code);
        scinit.assert_start_count(0, &what);
        let stderr = scinit.stderr();
        assert!(
            stderr.contains(message),
            "{}: missing {:?}\n{}",
            what,
            message,
            scinit.diagnostics()
        );
        // The temp dir in the command differs between the runs
        errors.push(stderr.replace(command.to_str().unwrap(), "<command>"));
    }
    assert_eq!(errors[0], errors[1], "the error differs with --ports");
}

/// A bare command name not in PATH → 127, as for a shell's "command not found"
#[test]
fn command_not_in_path_exits_127() {
    assert_spawn_failure(
        |_| "scinit-definitely-not-a-command".into(),
        127,
        "ERROR scinit: Failed to spawn process 'scinit-definitely-not-a-command': not found",
    );
}

/// A command path that doesn't exist → 127
#[test]
fn nonexistent_command_path_exits_127() {
    assert_spawn_failure(
        |b| b.dir().join("no-such-binary").into(),
        127,
        "no-such-binary': not found",
    );
}

/// A file without execute permission → 126
#[test]
fn non_executable_file_exits_126() {
    assert_spawn_failure(
        |b| write_file(&b.dir(), "plain", "#!/bin/sh\n", 0o644).into(),
        126,
        "plain': not executable: Permission denied",
    );
}

/// A directory → 126
#[test]
fn directory_exits_126() {
    assert_spawn_failure(
        |b| {
            let dir = b.dir().join("subdir");
            std::fs::create_dir(&dir).unwrap();
            dir.into()
        },
        126,
        "subdir': not executable: Is a directory",
    );
}

/// An executable script without a `#!` line isn't run through `/bin/sh`:
/// the child's `execve` refuses it (ENOEXEC) → 126
#[test]
fn script_without_shebang_exits_126() {
    assert_spawn_failure(
        |b| write_file(&b.dir(), "script", "echo ran\n", 0o755).into(),
        126,
        "script': not executable: Exec format error",
    );
}

/// A bare command name found through PATH keeps its name as argv[0], with
/// and without `--ports`
#[test]
fn bare_command_keeps_argv0_as_typed() {
    let path = path_with(Path::new(TEST_CHILD).parent().unwrap());
    for with_ports in [false, true] {
        let mut builder = Scinit::builder().env("PATH", &path);
        if with_ports {
            builder = builder.ports(&free_ports(1));
        }
        let (scinit, status) = builder
            .command(["scinit-test-child", "dump", "--then-exit"])
            .run(TIMEOUT)
            .unwrap();
        scinit.assert_exit_code(status, 0);
        let argv0 = scinit
            .wait_for_dump()
            .into_iter()
            .find(|e| e.is("arg") && e.field_is("index", "0"))
            .and_then(|e| e.get("value").map(str::to_string));
        assert_eq!(
            argv0.as_deref(),
            Some("scinit-test-child"),
            "--ports: {}\n{}",
            with_ports,
            scinit.diagnostics()
        );
    }
}

/// A live-reload restart whose command can no longer be executed ends
/// scinit with 126, as the first spawn would
#[test]
fn restart_of_non_executable_command_exits_126() {
    let builder = Scinit::builder();
    let dir = builder.dir();
    let watched = dir.join("watched");
    std::fs::create_dir(&watched).unwrap();
    let conf = write_file(&watched, "app.conf", "v1", 0o644);
    let script = format!("#!/bin/sh\nexec '{}' run\n", TEST_CHILD);
    let app = write_file(&dir, "app", &script, 0o755);
    let_setup_writes_age();
    let (mut scinit, _) = builder
        .watch(&watched)
        .args(["--debounce-ms", "200", "--restart-delay-ms", "100"])
        .command([&app])
        .start();
    // Let FSEvents settle before touching the watched directory
    std::thread::sleep(Duration::from_millis(500));

    // Outside the watched directory: only the write after it restarts
    write_file(&dir, "app", "echo ran\n", 0o755);
    std::fs::write(&conf, "v2").unwrap();

    let status = scinit.wait_exit(TIMEOUT).unwrap();
    scinit.assert_exit_code(status, 126);
    scinit.assert_start_count(1, "the restart can't spawn");
    assert!(
        scinit
            .stderr()
            .contains("not executable: Exec format error"),
        "{}",
        scinit.diagnostics()
    );
}
