//! Test harness for driving the scinit binary with the `scinit-test-child`
//! fixture as its child.
//!
//! The child appends events to a per-test report file (`$SCINIT_TEST_REPORT`),
//! which tests poll instead of sleeping for fixed durations.

use anyhow::{anyhow, bail, Context, Result};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use std::ffi::OsString;
use std::io::Read;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tempfile::TempDir;

pub const SCINIT: &str = env!("CARGO_BIN_EXE_scinit");
pub const TEST_CHILD: &str = env!("CARGO_BIN_EXE_scinit-test-child");

/// Default timeout for waiting on events; generous to tolerate loaded CI hosts
pub const TIMEOUT: Duration = Duration::from_secs(10);

const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// One line from the child's report file: `<name> key=value ...`
#[derive(Debug, Clone)]
pub struct Event {
    pub name: String,
    pub fields: Vec<(String, String)>,
}

impl Event {
    fn parse(line: &str) -> Option<Self> {
        let mut rest = line.trim_end();
        let (name, tail) = rest.split_once(' ').unwrap_or((rest, ""));
        let name = name.to_string();
        rest = tail;
        let mut fields = Vec::new();
        while !rest.is_empty() {
            // `value=` always runs to the end of the line
            if let Some(v) = rest.strip_prefix("value=") {
                fields.push(("value".to_string(), v.to_string()));
                break;
            }
            let (field, tail) = rest.split_once(' ').unwrap_or((rest, ""));
            let (k, v) = field.split_once('=').unwrap_or((field, ""));
            fields.push((k.to_string(), v.to_string()));
            rest = tail;
        }
        if name.is_empty() {
            None
        } else {
            Some(Self { name, fields })
        }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn pid(&self) -> i32 {
        self.get("pid")
            .and_then(|p| p.parse().ok())
            .expect("event has no pid")
    }
}

/// Builder for a scinit invocation
pub struct ScinitBuilder {
    dir: TempDir,
    // Kept apart from `dir` so watching `dir` never sees report writes
    report_dir: TempDir,
    scinit_args: Vec<OsString>,
    command: Vec<OsString>,
    env: Vec<(String, String)>,
}

impl ScinitBuilder {
    /// Per-test temp directory (cwd of scinit, watch paths, ...)
    pub fn dir(&self) -> PathBuf {
        canonical(self.dir.path())
    }

    /// Add scinit flags, placed before the child command
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.scinit_args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Run the test-child fixture with the given subcommand and args
    pub fn child<I, S>(self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let mut cmd = vec![OsString::from(TEST_CHILD)];
        cmd.extend(args.into_iter().map(Into::into));
        self.command(cmd)
    }

    /// Run an arbitrary command as scinit's child
    pub fn command<I, S>(mut self, cmd: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.command = cmd.into_iter().map(Into::into).collect();
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn spawn(self) -> Result<Scinit> {
        let dir = canonical(self.dir.path());
        let report = self.report_dir.path().join("report.log");

        let mut cmd = Command::new(SCINIT);
        cmd.args(&self.scinit_args)
            .args(&self.command)
            .current_dir(&dir)
            .env("SCINIT_TEST_REPORT", &report)
            // Keep the outer test environment from leaking socket activation vars
            .env_remove("LISTEN_FDS")
            .env_remove("LISTEN_PID")
            .env_remove("LISTEN_FDNAMES")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Own process group so the test runner's signals don't hit scinit
            .process_group(0);
        for (k, v) in &self.env {
            cmd.env(k, v);
        }

        let mut child = cmd.spawn().context("failed to spawn scinit")?;
        let stdout = Arc::new(Mutex::new(String::new()));
        let stderr = Arc::new(Mutex::new(String::new()));
        let readers = vec![
            drain(child.stdout.take().unwrap(), stdout.clone()),
            drain(child.stderr.take().unwrap(), stderr.clone()),
        ];

        Ok(Scinit {
            pid: Pid::from_raw(child.id() as i32),
            child,
            exit: None,
            _dirs: (self.dir, self.report_dir),
            report,
            stdout,
            stderr,
            readers,
        })
    }

    /// Spawn, wait for exit and return the status (for short-lived invocations)
    pub fn run(self, timeout: Duration) -> Result<(Scinit, ExitStatus)> {
        let mut scinit = self.spawn()?;
        let status = scinit.wait_exit(timeout)?;
        Ok((scinit, status))
    }
}

fn canonical(path: &Path) -> PathBuf {
    // macOS temp dirs live under /var, a symlink to /private/var; file
    // watchers report canonical paths
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn drain(mut src: impl Read + Send + 'static, dst: Arc<Mutex<String>>) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match src.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => dst
                    .lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&buf[..n])),
            }
        }
    })
}

/// A running scinit process
pub struct Scinit {
    pid: Pid,
    child: Child,
    exit: Option<ExitStatus>,
    _dirs: (TempDir, TempDir),
    report: PathBuf,
    stdout: Arc<Mutex<String>>,
    stderr: Arc<Mutex<String>>,
    readers: Vec<JoinHandle<()>>,
}

impl Scinit {
    pub fn builder() -> ScinitBuilder {
        ScinitBuilder {
            dir: TempDir::new().expect("failed to create temp dir"),
            report_dir: TempDir::new().expect("failed to create temp dir"),
            scinit_args: Vec::new(),
            command: Vec::new(),
            env: Vec::new(),
        }
    }

    pub fn pid(&self) -> Pid {
        self.pid
    }

    pub fn stdout(&self) -> String {
        self.stdout.lock().unwrap().clone()
    }

    pub fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }

    /// All events reported so far
    pub fn events(&self) -> Vec<Event> {
        std::fs::read_to_string(&self.report)
            .unwrap_or_default()
            .lines()
            .filter_map(Event::parse)
            .collect()
    }

    pub fn events_named(&self, name: &str) -> Vec<Event> {
        self.events().into_iter().filter(|e| e.name == name).collect()
    }

    /// Poll the report until `pred` holds, returning the events at that point
    pub fn wait_for<F>(&self, desc: &str, timeout: Duration, pred: F) -> Result<Vec<Event>>
    where
        F: Fn(&[Event]) -> bool,
    {
        let deadline = Instant::now() + timeout;
        loop {
            let events = self.events();
            if pred(&events) {
                return Ok(events);
            }
            if Instant::now() >= deadline {
                bail!(
                    "timed out after {:?} waiting for {}\nevents: {:#?}\n{}",
                    timeout,
                    desc,
                    events,
                    self.diagnostics()
                );
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Wait until at least `count` events named `name` exist; return the last
    pub fn wait_for_nth(&self, name: &str, count: usize, timeout: Duration) -> Result<Event> {
        let events = self.wait_for(&format!("{} x{}", name, count), timeout, |evs| {
            evs.iter().filter(|e| e.name == name).count() >= count
        })?;
        Ok(events.into_iter().filter(|e| e.name == name).nth(count - 1).unwrap())
    }

    pub fn wait_for_event(&self, name: &str, timeout: Duration) -> Result<Event> {
        self.wait_for_nth(name, 1, timeout)
    }

    /// Wait for the child to report `started` and return its pid
    pub fn child_pid(&self) -> Result<i32> {
        let events = self.wait_for("child started", TIMEOUT, |evs| {
            evs.iter().any(is_child_started)
        })?;
        Ok(events.iter().filter(|e| is_child_started(e)).last().unwrap().pid())
    }

    /// Pids of every `started` event from the direct child, in order
    pub fn started_pids(&self) -> Vec<i32> {
        self.events()
            .iter()
            .filter(|e| is_child_started(e))
            .map(Event::pid)
            .collect()
    }

    /// Wait for the child (or whoever reported) to log a received signal
    pub fn wait_for_signal(&self, pid: i32, sig: &str, timeout: Duration) -> Result<Event> {
        let events = self.wait_for(&format!("pid {} to receive {}", pid, sig), timeout, |evs| {
            evs.iter().any(|e| is_signal(e, pid, sig))
        })?;
        Ok(events.into_iter().find(|e| is_signal(e, pid, sig)).unwrap())
    }

    pub fn signal(&self, sig: Signal) -> Result<()> {
        kill(self.pid, sig).with_context(|| format!("failed to send {:?} to scinit", sig))
    }

    /// Non-blocking check that scinit has not exited
    pub fn is_running(&mut self) -> bool {
        if self.exit.is_some() {
            return false;
        }
        match self.child.try_wait() {
            Ok(Some(status)) => {
                self.exit = Some(status);
                false
            }
            Ok(None) => true,
            Err(_) => false,
        }
    }

    /// Assert scinit stays alive for the whole duration
    pub fn assert_running_for(&mut self, duration: Duration) {
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline {
            assert!(
                self.is_running(),
                "scinit exited unexpectedly with {:?}\n{}",
                self.exit,
                self.diagnostics()
            );
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    pub fn wait_exit(&mut self, timeout: Duration) -> Result<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.exit {
                return Ok(status);
            }
            if let Some(status) = self.child.try_wait()? {
                self.exit = Some(status);
                self.join_readers();
                return Ok(status);
            }
            if Instant::now() >= deadline {
                return Err(anyhow!(
                    "scinit did not exit within {:?}\n{}",
                    timeout,
                    self.diagnostics()
                ));
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Let the output readers drain so diagnostics are complete. Bounded: an
    /// orphaned child still holding the pipes would otherwise block forever
    fn join_readers(&mut self) {
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline && !self.readers.iter().all(|r| r.is_finished()) {
            std::thread::sleep(POLL_INTERVAL);
        }
        for reader in self.readers.drain(..) {
            if reader.is_finished() {
                let _ = reader.join();
            }
        }
    }

    /// Wait until `pid` has logged `sig` at least `count` times
    pub fn wait_for_signal_count(&self, pid: i32, sig: &str, count: usize, timeout: Duration) -> Result<()> {
        self.wait_for(&format!("pid {} to receive {} x{}", pid, sig, count), timeout, |evs| {
            evs.iter().filter(|e| is_signal(e, pid, sig)).count() >= count
        })
        .map(|_| ())
    }

    /// Report file, stdout and stderr, for assertion messages
    pub fn diagnostics(&self) -> String {
        format!(
            "--- report ---\n{}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            std::fs::read_to_string(&self.report).unwrap_or_default(),
            self.stdout(),
            self.stderr()
        )
    }
}

fn is_signal(e: &Event, pid: i32, sig: &str) -> bool {
    e.name == "signal" && e.pid() == pid && e.get("sig") == Some(sig)
}

fn is_child_started(e: &Event) -> bool {
    e.name == "started" && e.get("role") == Some("child")
}

impl Drop for Scinit {
    fn drop(&mut self) {
        // Children run in their own process groups (pgid == pid), so killing
        // scinit alone would orphan them. Freeze scinit first so it can't fork
        // a replacement (e.g. a live-reload restart in flight), then kill the
        // group of every live child, including ones that haven't reported yet.
        let running = self.is_running();
        if running {
            let _ = kill(self.pid, Signal::SIGSTOP);
        }
        let mut groups: Vec<i32> = if running { child_pids(self.pid) } else { Vec::new() };
        groups.extend(
            self.events()
                .iter()
                .filter(|e| e.name == "started")
                .filter_map(|e| e.get("pgid").and_then(|p| p.parse::<i32>().ok())),
        );
        for pgid in groups {
            let _ = kill(Pid::from_raw(-pgid), Signal::SIGKILL);
            let _ = kill(Pid::from_raw(pgid), Signal::SIGKILL);
        }
        if running {
            let _ = kill(Pid::from_raw(-self.pid.as_raw()), Signal::SIGKILL);
            let _ = self.child.wait();
        }
    }
}

/// Direct children of `pid`, via `pgrep -P` (works on macOS and Linux)
fn child_pids(pid: Pid) -> Vec<i32> {
    Command::new("pgrep")
        .args(["-P", &pid.to_string()])
        .output()
        .map(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|l| l.trim().parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Assert scinit exited with the given code
pub fn assert_exit_code(scinit: &Scinit, status: ExitStatus, code: i32) {
    assert_eq!(
        status.code(),
        Some(code),
        "expected exit code {}, got {:?} (signal {:?})\n{}",
        code,
        status.code(),
        status.signal(),
        scinit.diagnostics()
    );
}

/// Ask the OS for a free TCP port on 127.0.0.1
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .expect("failed to allocate a free port")
}

/// `n` distinct free ports
pub fn free_ports(n: usize) -> Vec<u16> {
    let mut ports = Vec::new();
    while ports.len() < n {
        let p = free_port();
        if !ports.contains(&p) {
            ports.push(p);
        }
    }
    ports
}

/// Value of a `dump` `env` event for `key`, if the child saw it
pub fn env_value(events: &[Event], key: &str) -> Option<String> {
    events
        .iter()
        .find(|e| e.name == "env" && e.get("key") == Some(key))
        .and_then(|e| e.get("value"))
        .map(str::to_string)
}

/// The child's socket fds from the `dump` `fds` event
pub fn socket_fds(events: &[Event]) -> Vec<i32> {
    let fds = events.iter().find(|e| e.name == "fds").expect("no fds event");
    fds.get("sockets")
        .unwrap_or("")
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.parse().unwrap())
        .collect()
}

/// Poll until `pid` no longer exists
pub fn wait_for_pid_gone(pid: i32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if kill(Pid::from_raw(pid), None) == Err(nix::errno::Errno::ESRCH) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Connect and return the trimmed reply line
pub fn request(addr: &str) -> Result<String> {
    use std::net::TcpStream;
    let addr = addr.parse().context("bad socket address")?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut out = String::new();
    stream.read_to_string(&mut out)?;
    Ok(out.trim().to_string())
}
