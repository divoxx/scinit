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
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tempfile::TempDir;

pub const SCINIT: &str = env!("CARGO_BIN_EXE_scinit");
pub const TEST_CHILD: &str = env!("CARGO_BIN_EXE_scinit-test-child");

/// Default timeout for waiting on events; generous to tolerate loaded CI hosts
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// How often the polling helpers check again
pub const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Call `f` every `POLL_INTERVAL` until it returns `Some`, giving up with
/// `None` once `timeout` has passed
pub fn poll_until<T>(timeout: Duration, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = f() {
            return Some(value);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// One line from the child's report file: `<name> key=value ...`
#[derive(Debug, Clone)]
pub struct Event {
    pub name: String,
    pub fields: Vec<(String, String)>,
}

impl Event {
    pub(crate) fn parse(line: &str) -> Option<Self> {
        let line = line.trim_end();
        let (name, mut rest) = line.split_once(' ').unwrap_or((line, ""));
        let name = name.to_string();
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

    pub fn is(&self, name: &str) -> bool {
        self.name == name
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn field_is(&self, key: &str, value: &str) -> bool {
        self.get(key) == Some(value)
    }

    /// Field `key` parsed as a `T`; `None` if missing or unparseable
    pub fn parse_field<T: FromStr>(&self, key: &str) -> Option<T> {
        self.get(key)?.parse().ok()
    }

    pub fn pid(&self) -> i32 {
        self.parse_field("pid").expect("event has no pid")
    }

    pub fn pgid(&self) -> i32 {
        self.parse_field("pgid").expect("event has no pgid")
    }
}

/// Every event in the report file at `path` (none if it doesn't exist yet)
pub(crate) fn read_events(path: &Path) -> Vec<Event> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(Event::parse)
        .collect()
}

/// Report file, stdout and stderr, for assertion messages
pub(crate) fn format_diagnostics(report: &Path, stdout: &str, stderr: &str) -> String {
    format!(
        "--- report ---\n{}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        std::fs::read_to_string(report).unwrap_or_default(),
        stdout,
        stderr
    )
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

    /// `--ports` with the given ports, in order
    pub fn ports(self, ports: &[u16]) -> Self {
        let list = ports
            .iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join(",");
        self.args(["--ports", &list])
    }

    /// `--live-reload --watch-path <path>`. Call `let_setup_writes_age` first
    /// if the test just wrote files there.
    pub fn watch(self, path: &Path) -> Self {
        self.args(["--live-reload", "--watch-path"]).args([path])
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
        // Start scinit with only stdio open, as in a container. Some hosts
        // (e.g. GitHub's macOS runners) leak non-close-on-exec fds into every
        // process, which would otherwise reach the child as stray sockets.
        let max_fd = open_fd_limit();
        unsafe {
            cmd.pre_exec(move || {
                for fd in 3..max_fd {
                    // fcntl is async-signal-safe; unopened fds just fail
                    libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                }
                Ok(())
            });
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

    /// Spawn and wait for the child to start; returns scinit and the child's pid
    pub fn start(self) -> (Scinit, i32) {
        let scinit = self.spawn().unwrap();
        let pid = scinit.child_pid().unwrap();
        (scinit, pid)
    }

    /// Spawn the fixture's `dump` subcommand with `args` and wait for its dump
    pub fn spawn_dump(self, args: &[&str]) -> (Scinit, Vec<Event>) {
        let mut child = vec!["dump"];
        child.extend_from_slice(args);
        let scinit = self.child(child).spawn().unwrap();
        let events = scinit.wait_for_dump();
        (scinit, events)
    }

    /// Spawn, wait for exit and return the status (for short-lived invocations)
    pub fn run(self, timeout: Duration) -> Result<(Scinit, ExitStatus)> {
        let mut scinit = self.spawn()?;
        let status = scinit.wait_exit(timeout)?;
        Ok((scinit, status))
    }
}

/// Upper bound for fd numbers to scan, computed before forking
fn open_fd_limit() -> i32 {
    let limit = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
    if limit > 0 {
        limit.min(65536) as i32
    } else {
        1024
    }
}

fn canonical(path: &Path) -> PathBuf {
    // macOS temp dirs live under /var, a symlink to /private/var; file
    // watchers report canonical paths
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// FSEvents (macOS) can report writes made just before the watcher starts as
/// fresh changes. Call right before spawning, after the test set up its files.
pub fn let_setup_writes_age() {
    std::thread::sleep(Duration::from_secs(1));
}

/// Copy everything read from `src` into `dst` on a background thread
pub(crate) fn drain(mut src: impl Read + Send + 'static, dst: Arc<Mutex<String>>) -> JoinHandle<()> {
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
        read_events(&self.report)
    }

    pub fn events_named(&self, name: &str) -> Vec<Event> {
        self.events().into_iter().filter(|e| e.is(name)).collect()
    }

    /// Poll the report until `pred` holds, returning the events at that point
    pub fn wait_for<F>(&self, desc: &str, timeout: Duration, pred: F) -> Result<Vec<Event>>
    where
        F: Fn(&[Event]) -> bool,
    {
        if let Some(events) = poll_until(timeout, || Some(self.events()).filter(|evs| pred(evs))) {
            return Ok(events);
        }
        bail!(
            "timed out after {:?} waiting for {}\nevents: {:#?}\n{}",
            timeout,
            desc,
            self.events(),
            self.diagnostics()
        )
    }

    /// Wait until at least `n` events match `pred`; return the `n`th
    pub fn wait_for_nth_match<F>(&self, desc: &str, n: usize, timeout: Duration, pred: F) -> Result<Event>
    where
        F: Fn(&Event) -> bool,
    {
        let events = self.wait_for(desc, timeout, |evs| evs.iter().filter(|e| pred(e)).count() >= n)?;
        Ok(events.into_iter().filter(|e| pred(e)).nth(n - 1).unwrap())
    }

    /// Wait until at least `count` events named `name` exist; return the last
    pub fn wait_for_nth(&self, name: &str, count: usize, timeout: Duration) -> Result<Event> {
        self.wait_for_nth_match(&format!("{} x{}", name, count), count, timeout, |e| e.is(name))
    }

    pub fn wait_for_event(&self, name: &str, timeout: Duration) -> Result<Event> {
        self.wait_for_nth(name, 1, timeout)
    }

    /// Wait for the `dump` child to finish reporting; return the events
    pub fn wait_for_dump(&self) -> Vec<Event> {
        self.wait_for("dump-done", TIMEOUT, |evs| evs.iter().any(|e| e.is("dump-done")))
            .unwrap()
    }

    /// Wait for the child to report `started` and return its pid
    pub fn child_pid(&self) -> Result<i32> {
        let events = self.wait_for("child started", TIMEOUT, |evs| {
            evs.iter().any(is_child_started)
        })?;
        Ok(events.iter().rfind(|e| is_child_started(e)).unwrap().pid())
    }

    /// Pids of every `started` event from the direct child, in order
    pub fn started_pids(&self) -> Vec<i32> {
        self.events()
            .iter()
            .filter(|e| is_child_started(e))
            .map(Event::pid)
            .collect()
    }

    /// Assert the direct child has started exactly `n` times
    pub fn assert_start_count(&self, n: usize, what: &str) {
        let pids = self.started_pids();
        assert_eq!(
            pids.len(),
            n,
            "{}: expected {} child start(s), got {:?}\n{}",
            what,
            n,
            pids,
            self.diagnostics()
        );
    }

    /// Wait for `pid` to log a received `sig`
    pub fn wait_for_signal(&self, pid: i32, sig: Signal, timeout: Duration) -> Result<Event> {
        self.wait_for_nth_match(&format!("pid {} to receive {}", pid, sig), 1, timeout, |e| {
            is_signal(e, pid, sig)
        })
    }

    /// Wait until `pid` has logged `sig` at least `count` times
    pub fn wait_for_signal_count(&self, pid: i32, sig: Signal, count: usize, timeout: Duration) -> Result<()> {
        self.wait_for_nth_match(
            &format!("pid {} to receive {} x{}", pid, sig, count),
            count,
            timeout,
            |e| is_signal(e, pid, sig),
        )
        .map(|_| ())
    }

    /// `(fd, port)` of every listener the `listen` child reported
    pub fn listeners(&self) -> Vec<(i32, u16)> {
        self.events_named("listening")
            .iter()
            .map(|e| {
                (
                    e.parse_field("fd").expect("listening event has no fd"),
                    e.parse_field("port").expect("listening event has no port"),
                )
            })
            .collect()
    }

    /// Request `addr` and assert the `listen` child `pid` answered; returns the reply
    pub fn assert_reply_from(&self, addr: &str, pid: i32) -> String {
        let reply = request(addr)
            .unwrap_or_else(|e| panic!("request to {} failed: {}\n{}", addr, e, self.diagnostics()));
        assert!(
            reply.starts_with(&format!("pid={} ", pid)),
            "reply {:?} from {} did not come from child {}\n{}",
            reply,
            addr,
            pid,
            self.diagnostics()
        );
        reply
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
        if let Some(status) = self.exit {
            return Ok(status);
        }
        let status = poll_until(timeout, || self.child.try_wait().transpose()).transpose()?;
        let Some(status) = status else {
            return Err(anyhow!(
                "scinit did not exit within {:?}\n{}",
                timeout,
                self.diagnostics()
            ));
        };
        self.exit = Some(status);
        self.join_readers();
        Ok(status)
    }

    /// Assert scinit exited with the given code
    pub fn assert_exit_code(&self, status: ExitStatus, code: i32) {
        assert_eq!(
            status.code(),
            Some(code),
            "expected exit code {}, got {:?} (signal {:?})\n{}",
            code,
            status.code(),
            status.signal(),
            self.diagnostics()
        );
    }

    /// Let the output readers drain so diagnostics are complete. Bounded: an
    /// orphaned child still holding the pipes would otherwise block forever
    fn join_readers(&mut self) {
        poll_until(Duration::from_millis(500), || {
            self.readers.iter().all(|r| r.is_finished()).then_some(())
        });
        for reader in self.readers.drain(..) {
            if reader.is_finished() {
                let _ = reader.join();
            }
        }
    }

    /// Report file, stdout and stderr, for assertion messages
    pub fn diagnostics(&self) -> String {
        format_diagnostics(&self.report, &self.stdout(), &self.stderr())
    }
}

fn is_signal(e: &Event, pid: i32, sig: Signal) -> bool {
    e.is("signal") && e.pid() == pid && e.field_is("sig", sig.as_str().trim_start_matches("SIG"))
}

fn is_child_started(e: &Event) -> bool {
    e.is("started") && e.field_is("role", "child")
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
            self.events_named("started")
                .iter()
                .filter_map(|e| e.parse_field::<i32>("pgid")),
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

/// `127.0.0.1:<port>`
pub fn loopback(port: u16) -> String {
    format!("127.0.0.1:{}", port)
}

/// Value of a `dump` `env` event for `key`, if the child saw it
pub fn env_value(events: &[Event], key: &str) -> Option<String> {
    events
        .iter()
        .find(|e| e.is("env") && e.field_is("key", key))
        .and_then(|e| e.get("value"))
        .map(str::to_string)
}

/// The child's socket fds from the `dump` `fds` event
pub fn socket_fds(events: &[Event]) -> Vec<i32> {
    let fds = events.iter().find(|e| e.is("fds")).expect("no fds event");
    fds.get("sockets")
        .unwrap_or("")
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.parse().unwrap())
        .collect()
}

/// Poll until `pid` no longer exists
pub fn wait_for_pid_gone(pid: i32, timeout: Duration) -> bool {
    poll_until(timeout, || {
        (kill(Pid::from_raw(pid), None) == Err(nix::errno::Errno::ESRCH)).then_some(())
    })
    .is_some()
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
