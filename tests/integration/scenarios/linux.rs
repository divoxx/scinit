//! Linux-only behavior: `/proc` introspection and running scinit as PID 1.
//!
//! The PID-1 tests run scinit as the init of a fresh PID namespace via
//! `unshare(1)`. When the host can't create one (no util-linux, no user
//! namespaces, a restrictive container seccomp/mask policy), they skip with a
//! log line instead of failing, unless `SCINIT_REQUIRE_PID1` is set (as
//! `scripts/test-linux.sh` does).

use crate::integration::harness::*;
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// Signals scinit claims to handle synchronously (src/signals.rs)
/// Signals scinit consumes on its sigwait thread. SIGCHLD is deliberately
/// not among them: tokio's SIGCHLD handler drives `Child::wait()`.
const HANDLED: &[Signal] = &[
    Signal::SIGTERM,
    Signal::SIGINT,
    Signal::SIGQUIT,
    Signal::SIGUSR1,
    Signal::SIGUSR2,
    Signal::SIGHUP,
];

/// Read a hex signal-set field (`SigBlk`, `SigIgn`, ...) from a status file
fn sigset_field(status_path: &Path, field: &str) -> u64 {
    let status = std::fs::read_to_string(status_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {}", status_path.display(), e));
    let line = status
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{}:", field)))
        .unwrap_or_else(|| panic!("no {} in {}", field, status_path.display()));
    u64::from_str_radix(line.trim(), 16).expect("bad signal set")
}

fn sig_bit(sig: Signal) -> u64 {
    1u64 << (sig as i32 - 1)
}

/// The child must start with an empty signal mask, as seen by the kernel
#[test]
fn child_sigblk_is_empty() {
    // grep reads its own status right after exec, with the mask scinit set
    let (scinit, status) = Scinit::builder()
        .command(["grep", "^SigBlk:", "/proc/self/status"])
        .run(TIMEOUT)
        .unwrap();
    assert_exit_code(&scinit, status, 0);

    let stdout = scinit.stdout();
    let line = stdout
        .lines()
        .find(|l| l.starts_with("SigBlk:"))
        .unwrap_or_else(|| panic!("no SigBlk line\n{}", scinit.diagnostics()));
    let blocked = u64::from_str_radix(line["SigBlk:".len()..].trim(), 16).unwrap();
    assert_eq!(
        blocked, 0,
        "child SigBlk should be 0, got {:016x}\n{}",
        blocked,
        scinit.diagnostics()
    );
}

/// Every scinit thread, including the main thread, must block the signals it
/// handles; otherwise they are delivered to an unmasked thread and take their
/// default action (or vanish when scinit is PID 1).
///
/// The sigwait thread is exempt: while a thread sits in `sigwait`, Linux
/// temporarily unblocks the awaited signals in its `SigBlk`.
#[test]
fn all_scinit_threads_block_handled_signals() {
    let scinit = Scinit::builder().child(["run"]).spawn().unwrap();
    scinit.child_pid().unwrap();

    let tasks = PathBuf::from(format!("/proc/{}/task", scinit.pid()));
    let mut unmasked = Vec::new();
    for entry in std::fs::read_dir(&tasks).unwrap() {
        let task = entry.unwrap().path();
        let comm = std::fs::read_to_string(task.join("comm")).unwrap_or_default();
        if comm.trim() == "scinit-sigwait" {
            continue;
        }
        let blocked = sigset_field(&task.join("status"), "SigBlk");
        let missing: Vec<&str> = HANDLED
            .iter()
            .filter(|s| blocked & sig_bit(**s) == 0)
            .map(|s| s.as_str())
            .collect();
        if !missing.is_empty() {
            unmasked.push(format!(
                "tid {} ({}) SigBlk={:016x} missing {:?}",
                task.file_name().unwrap().to_string_lossy(),
                comm.trim(),
                blocked,
                missing
            ));
        }
    }
    assert!(
        unmasked.is_empty(),
        "threads not blocking handled signals:\n{}\n{}",
        unmasked.join("\n"),
        scinit.diagnostics()
    );
}

// ---------------------------------------------------------------------------
// scinit as PID 1 of a new PID namespace
// ---------------------------------------------------------------------------

/// Find `unshare` flags that give us a PID namespace with its own /proc
fn pid_namespace_flags() -> Option<Vec<&'static str>> {
    let base = ["--pid", "--fork", "--mount-proc", "--kill-child"];
    let candidates: Vec<Vec<&'static str>> = if unsafe { libc::geteuid() } == 0 {
        vec![base.to_vec(), [&["--user", "--map-root-user"][..], &base].concat()]
    } else {
        vec![[&["--user", "--map-root-user"][..], &base].concat()]
    };
    let mut errors = Vec::new();
    for flags in candidates {
        // The probe must see itself as PID 1 to prove the namespace works
        match Command::new("unshare")
            .args(&flags)
            .args(["sh", "-c", "test $$ -eq 1"])
            .stdin(Stdio::null())
            .output()
        {
            Ok(out) if out.status.success() => return Some(flags),
            Ok(out) => errors.push(format!(
                "unshare {}: {}",
                flags.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            )),
            Err(e) => errors.push(format!("unshare: {}", e)),
        }
    }
    let msg = format!(
        "cannot create a PID namespace to run scinit as PID 1 ({})",
        errors.join("; ")
    );
    // scripts/test-linux.sh sets this so a missing namespace can't pass silently
    if std::env::var_os("SCINIT_REQUIRE_PID1").is_some() {
        panic!("{}", msg);
    }
    eprintln!("SKIP: {}; run scripts/test-linux.sh for PID 1 coverage", msg);
    None
}

/// scinit running as PID 1 under `unshare`, killed (with its namespace) on drop
struct Pid1Scinit {
    unshare: Child,
    _dir: TempDir,
    report: PathBuf,
    stdout: mpsc::Receiver<String>,
    stdout_lines: Vec<String>,
    stderr: std::sync::Arc<std::sync::Mutex<String>>,
}

impl Pid1Scinit {
    fn spawn(flags: &[&str], scinit_args: &[&str], child_args: &[&str]) -> Self {
        let dir = TempDir::new().unwrap();
        let report = dir.path().join("report.log");
        let mut unshare = Command::new("unshare")
            .args(flags)
            .arg(SCINIT)
            .args(scinit_args)
            .arg(TEST_CHILD)
            .args(child_args)
            .env("SCINIT_TEST_REPORT", &report)
            .env_remove("LISTEN_FDS")
            .env_remove("LISTEN_PID")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .expect("failed to spawn unshare");

        let (tx, rx) = mpsc::channel();
        let out = unshare.stdout.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let stderr = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let err = unshare.stderr.take().unwrap();
        let sink = stderr.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                let mut s = sink.lock().unwrap();
                s.push_str(&line);
                s.push('\n');
            }
        });

        Self {
            unshare,
            _dir: dir,
            report,
            stdout: rx,
            stdout_lines: Vec::new(),
            stderr,
        }
    }

    fn report(&self) -> String {
        std::fs::read_to_string(&self.report).unwrap_or_default()
    }

    fn diagnostics(&self) -> String {
        format!(
            "--- report ---\n{}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.report(),
            self.stdout_lines.join("\n"),
            self.stderr.lock().unwrap()
        )
    }

    /// Wait for a stdout line matching `pred`
    fn wait_stdout(&mut self, timeout: Duration, pred: impl Fn(&str) -> bool) -> Option<String> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.stdout.recv_timeout(left) {
                Ok(line) => {
                    self.stdout_lines.push(line.clone());
                    if pred(&line) {
                        return Some(line);
                    }
                }
                Err(_) => return None,
            }
        }
    }

    /// Wait until the report contains a line matching `pred`
    fn wait_report(&self, timeout: Duration, pred: impl Fn(&str) -> bool) -> Option<String> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(line) = self.report().lines().find(|l| pred(l)) {
                return Some(line.to_string());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }

    /// The child's `started` line, asserting scinit (its parent) is PID 1
    fn assert_child_of_pid1(&self) {
        let started = self
            .wait_report(TIMEOUT, |l| l.starts_with("started ") && l.contains("role=child"))
            .unwrap_or_else(|| panic!("child never started\n{}", self.diagnostics()));
        assert!(
            started.split_whitespace().any(|f| f == "ppid=1"),
            "child's parent should be scinit as PID 1: {}\n{}",
            started,
            self.diagnostics()
        );
    }

    /// scinit's pid as seen from our (ancestor) namespace: unshare's only child
    fn scinit_outer_pid(&self) -> Pid {
        let parent = self.unshare.id().to_string();
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            for entry in std::fs::read_dir("/proc").unwrap().flatten() {
                let Ok(status) = std::fs::read_to_string(entry.path().join("status")) else {
                    continue;
                };
                let ppid = status.lines().find_map(|l| l.strip_prefix("PPid:"));
                if ppid.map(str::trim) == Some(parent.as_str()) {
                    if let Ok(pid) = entry.file_name().to_string_lossy().parse() {
                        return Pid::from_raw(pid);
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("could not find scinit under unshare\n{}", self.diagnostics());
    }
}

impl Drop for Pid1Scinit {
    fn drop(&mut self) {
        // --kill-child takes scinit down with unshare, and the namespace with it
        let _ = kill(Pid::from_raw(-(self.unshare.id() as i32)), Signal::SIGKILL);
        let _ = self.unshare.wait();
    }
}

fn orphan_verdict(mut scinit: Pid1Scinit) {
    scinit.assert_child_of_pid1();
    // spawn-orphan checks the orphan ~2s after it exits
    let verdict = scinit.wait_stdout(Duration::from_secs(15), |l| l.starts_with("ORPHAN_"));
    assert_eq!(
        verdict.as_deref(),
        Some("ORPHAN_REAPED"),
        "orphan should be reaped by scinit as PID 1\n{}",
        scinit.diagnostics()
    );
}

/// An orphan reparented to scinit (PID 1) is reaped promptly on SIGCHLD, well
/// before the default 5s periodic reaper would get to it
#[test]
fn orphan_reaped_on_sigchld_as_pid1() {
    let Some(flags) = pid_namespace_flags() else {
        return;
    };
    orphan_verdict(Pid1Scinit::spawn(&flags, &[], &["spawn-orphan"]));
}

/// The periodic reaper alone also collects orphans when scinit is PID 1
#[test]
fn orphan_reaped_by_periodic_reaper_as_pid1() {
    let Some(flags) = pid_namespace_flags() else {
        return;
    };
    orphan_verdict(Pid1Scinit::spawn(
        &flags,
        &["--zombie-reap-interval-ms", "300"],
        &["spawn-orphan"],
    ));
}

/// SIGTERM sent to scinit as PID 1 (e.g. `docker stop`) reaches the child.
/// The kernel drops default-action signals aimed at a namespace init unless
/// they are blocked or handled, so an unmasked main thread loses them.
#[test]
fn sigterm_forwarded_as_pid1() {
    let Some(flags) = pid_namespace_flags() else {
        return;
    };
    let scinit = Pid1Scinit::spawn(&flags, &["--graceful-timeout-secs", "5"], &["run"]);
    scinit.assert_child_of_pid1();

    kill(scinit.scinit_outer_pid(), Signal::SIGTERM).unwrap();
    let got = scinit.wait_report(Duration::from_secs(5), |l| {
        l.starts_with("signal ") && l.contains("sig=TERM")
    });
    assert!(
        got.is_some(),
        "child never received SIGTERM sent to scinit as PID 1\n{}",
        scinit.diagnostics()
    );
}
