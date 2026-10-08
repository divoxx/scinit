//! Test fixture child process for scinit integration tests.
//!
//! scinit spawns this binary as its child. It appends newline-delimited events
//! to the file named by `$SCINIT_TEST_REPORT`, so tests can observe what the
//! child actually experienced (signals received, environment, fds, ...).
//!
//! Event format: `<name> pid=<pid> t=<µs since epoch> key=value ...`. A trailing `value=` field
//! extends to the end of the line and may contain spaces.
//!
//! Subcommands:
//!   run [--trap SIGS] [--ignore SIGS] [--exit-on SIGS] [--grandchild]
//!   exit <code>
//!   kill-self <SIG>
//!   dump [--env KEY]... [--then-exit]
//!   listen
//!   spawn-orphan

use nix::sys::signal::{self, SigHandler, SigSet, Signal};
use nix::unistd::{self, ForkResult, Pid};
use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::io::Write;
use std::net::TcpListener;
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::str::FromStr;
use std::time::{Duration, Instant};

const DEFAULT_TRAP: &[Signal] = &[
    Signal::SIGTERM,
    Signal::SIGINT,
    Signal::SIGQUIT,
    Signal::SIGUSR1,
    Signal::SIGUSR2,
    Signal::SIGHUP,
];
const DEFAULT_EXIT_ON: &[Signal] = &[Signal::SIGTERM, Signal::SIGINT, Signal::SIGQUIT];

fn report(event: &str, fields: &str) {
    // `t` (µs since the Unix epoch) lets tests time events by when they
    // happened rather than when a poll noticed them; it precedes `fields`
    // because a `value=` field must stay last
    let line = if fields.is_empty() {
        format!("{} pid={} t={}\n", event, unistd::getpid(), now_micros())
    } else {
        format!(
            "{} pid={} t={} {}\n",
            event,
            unistd::getpid(),
            now_micros(),
            fields
        )
    };
    if let Ok(path) = std::env::var("SCINIT_TEST_REPORT") {
        // O_APPEND with a single write keeps lines atomic across processes
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = f.write_all(line.as_bytes());
        }
    }
    eprint!("[test-child] {}", line);
}

fn now_micros() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0)
}

fn report_started(role: &str) {
    report(
        "started",
        &format!(
            "role={} pgid={} ppid={}",
            role,
            unistd::getpgrp(),
            unistd::getppid()
        ),
    );
}

/// Report `exit` and exit with `code`
fn exit_reported(code: i32) -> ! {
    report("exit", &format!("code={}", code));
    std::process::exit(code);
}

/// The value of the last `key=` field in the report file
fn last_reported_field(key: &str) -> Option<String> {
    let report = std::fs::read_to_string(std::env::var("SCINIT_TEST_REPORT").ok()?).ok()?;
    let prefix = format!("{}=", key);
    report
        .lines()
        .rev()
        .find_map(|l| l.split_whitespace().find_map(|f| f.strip_prefix(&prefix)))
        .map(str::to_string)
}

/// The argument following `flag`
fn flag_value<'a>(it: &mut impl Iterator<Item = &'a String>, flag: &str) -> &'a str {
    it.next()
        .unwrap_or_else(|| panic!("{} needs a value", flag))
}

fn parse_signal(name: &str) -> Signal {
    let name = name.trim().to_uppercase();
    let full = if name.starts_with("SIG") {
        name
    } else {
        format!("SIG{}", name)
    };
    Signal::from_str(&full).unwrap_or_else(|_| panic!("unknown signal: {}", full))
}

fn parse_signals(list: &str) -> Vec<Signal> {
    list.split(',')
        .filter(|s| !s.is_empty())
        .map(parse_signal)
        .collect()
}

fn short_name(sig: Signal) -> &'static str {
    sig.as_str().trim_start_matches("SIG")
}

fn sigset<'a>(sigs: impl IntoIterator<Item = &'a Signal>) -> SigSet {
    let mut set = SigSet::empty();
    for sig in sigs {
        set.add(*sig);
    }
    set
}

fn set_disposition(sig: Signal, handler: SigHandler) {
    unsafe { signal::signal(sig, handler) }
        .unwrap_or_else(|e| panic!("failed to set the disposition of {}: {}", sig, e));
}

fn is_ignored(sig: Signal) -> bool {
    let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::sigaction(sig as libc::c_int, std::ptr::null(), &mut old) };
    rc == 0 && old.sa_sigaction == libc::SIG_IGN
}

/// Block `trap` signals and handle them synchronously, logging each one.
/// Exits with status 0 when a signal in `exit_on` arrives.
fn signal_loop(trap: &[Signal], exit_on: &[Signal]) -> ! {
    wait_signals(&block_signals(trap), trap, exit_on)
}

/// Block `trap` and the other common signals (`DEFAULT_TRAP`) for `sigwait`,
/// returning the blocked set. Ignored signals are left alone.
///
/// The `DEFAULT_TRAP` signals outside `trap` keep their default action: they
/// are waited for too and re-raised with the default disposition, because on
/// macOS a default-action signal doesn't terminate a process whose thread is
/// parked in `sigwait` for other signals.
fn block_signals(trap: &[Signal]) -> SigSet {
    let set = sigset(trap.iter().chain(DEFAULT_TRAP).filter(|s| !is_ignored(**s)));
    set.thread_block().expect("failed to block signals");
    set
}

/// Wait on `set` forever: log signals in `trap`, exiting 0 on one in
/// `exit_on`, and die by the default action of any other
fn wait_signals(set: &SigSet, trap: &[Signal], exit_on: &[Signal]) -> ! {
    loop {
        let sig = set.wait().expect("sigwait failed");
        if !trap.contains(&sig) {
            die_by(sig);
        }
        report("signal", &format!("sig={}", short_name(sig)));
        if exit_on.contains(&sig) {
            exit_reported(0);
        }
    }
}

/// Unblock `sig` and raise it with its default disposition
fn raise_with_default(sig: Signal) {
    // KILL and STOP can't have their disposition changed (EINVAL)
    if !matches!(sig, Signal::SIGKILL | Signal::SIGSTOP) {
        set_disposition(sig, SigHandler::SigDfl);
    }
    let _ = sigset(&[sig]).thread_unblock();
    signal::raise(sig).expect("raise failed");
}

/// Terminate via `sig`'s default action
fn die_by(sig: Signal) -> ! {
    raise_with_default(sig);
    // Not reached for terminating signals
    std::process::exit(128 + sig as i32);
}

fn cmd_run(args: &[String]) -> ! {
    let mut trap: Vec<Signal> = DEFAULT_TRAP.to_vec();
    let mut exit_on: Vec<Signal> = DEFAULT_EXIT_ON.to_vec();
    let mut ignore: Vec<Signal> = Vec::new();
    let mut grandchild = false;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--trap" => trap = parse_signals(flag_value(&mut it, "--trap")),
            "--exit-on" => exit_on = parse_signals(flag_value(&mut it, "--exit-on")),
            "--ignore" => ignore = parse_signals(flag_value(&mut it, "--ignore")),
            "--grandchild" => grandchild = true,
            other => panic!("unknown run option: {}", other),
        }
    }

    for sig in &ignore {
        set_disposition(*sig, SigHandler::SigIgn);
    }
    trap.retain(|s| !ignore.contains(s));
    exit_on.retain(|s| !ignore.contains(s));

    // Block before reporting `started` (and before forking, so the grandchild
    // inherits the mask): tests signal as soon as they see `started`, and a
    // signal arriving before the block would take its default action
    let set = block_signals(&trap);

    let mut role = "child";
    if grandchild {
        // Single-threaded at this point, so fork is safe
        match unsafe { unistd::fork() }.expect("fork failed") {
            ForkResult::Child => role = "grandchild",
            ForkResult::Parent { .. } => {}
        }
    }

    report_started(role);
    wait_signals(&set, &trap, &exit_on);
}

fn cmd_exit(args: &[String]) -> ! {
    let code: i32 = args
        .first()
        .expect("exit needs a code")
        .parse()
        .expect("exit code must be an integer");
    report_started("child");
    exit_reported(code);
}

fn cmd_kill_self(args: &[String]) -> ! {
    let sig = parse_signal(args.first().expect("kill-self needs a signal"));
    report_started("child");
    report("raise", &format!("sig={}", short_name(sig)));
    raise_with_default(sig);
    // SIGSTOP-like signals could land here; never report success
    std::thread::sleep(Duration::from_secs(5));
    std::process::exit(99);
}

fn is_open(fd: i32) -> bool {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    flags != -1
}

fn is_socket(fd: i32) -> bool {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::fstat(fd, &mut st) };
    rc == 0 && (st.st_mode & libc::S_IFMT) == libc::S_IFSOCK
}

/// `getsockname` or `getpeername`
type SockAddrFn =
    unsafe extern "C" fn(libc::c_int, *mut libc::sockaddr, *mut libc::socklen_t) -> libc::c_int;

/// The address `query` returns for socket `fd`
fn sock_addr(fd: i32, query: SockAddrFn) -> std::io::Result<libc::sockaddr_storage> {
    let mut addr: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    if unsafe { query(fd, &mut addr as *mut _ as *mut libc::sockaddr, &mut len) } == 0 {
        Ok(addr)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Bound to an IPv4 or IPv6 address
fn is_inet_bound(fd: i32) -> bool {
    sock_addr(fd, libc::getsockname).is_ok_and(|addr| {
        let family = addr.ss_family as i32;
        family == libc::AF_INET || family == libc::AF_INET6
    })
}

/// Anything but `ENOTCONN` from `getpeername` counts as having a peer
fn has_peer(fd: i32) -> bool {
    !matches!(
        sock_addr(fd, libc::getpeername),
        Err(e) if e.raw_os_error() == Some(libc::ENOTCONN)
    )
}

/// Open fds in 0..1024, split into all fds and socket fds.
fn scan_fds() -> (Vec<i32>, Vec<i32>) {
    let open: Vec<i32> = (0..1024).filter(|fd| is_open(*fd)).collect();
    let sockets = open.iter().copied().filter(|fd| is_socket(*fd)).collect();
    (open, sockets)
}

fn join(fds: &[i32]) -> String {
    fds.iter()
        .map(|fd| fd.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// `value` as text, with the bytes of a non-UTF-8 value escaped (`\xff`)
fn escape_non_utf8(value: &OsStr) -> String {
    match value.to_str() {
        Some(s) => s.to_string(),
        None => value.as_bytes().escape_ascii().to_string(),
    }
}

fn cmd_dump(args: &[String]) -> ! {
    // Whether this process's group holds the terminal's foreground, checked
    // first thing after exec so a handover from outside has had no time
    let tty = match std::fs::File::open("/dev/tty") {
        Ok(tty) => format!(
            "foreground={}",
            unistd::tcgetpgrp(&tty).ok() == Some(unistd::getpgrp())
        ),
        Err(_) => "none".to_string(),
    };
    let mut env_keys: Vec<String> = Vec::new();
    let mut then_exit = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--env" => env_keys.push(flag_value(&mut it, "--env").to_string()),
            "--then-exit" => then_exit = true,
            // Unknown options are ignored, not rejected: tests pass arbitrary
            // args (scinit-like flags included) to check they reach the child
            // verbatim, as reported in the `arg` events below
            _ => {}
        }
    }

    // Capture process state before anything here changes it
    let mask = SigSet::thread_get_mask().expect("failed to read signal mask");
    let (open, sockets) = scan_fds();

    // Blocked before reporting, like `run`, so a signal sent right after
    // `started` waits for the loop below instead of killing the process
    let set = block_signals(DEFAULT_TRAP);
    report_started("child");

    for (i, arg) in std::env::args().enumerate() {
        report("arg", &format!("index={} value={}", i, arg));
    }

    for (key, value) in std::env::vars_os() {
        let key = key.to_string_lossy();
        if key.starts_with("LISTEN_") || env_keys.iter().any(|k| *k == key) {
            report(
                "env",
                &format!("key={} value={}", key, escape_non_utf8(&value)),
            );
        }
    }
    for key in &env_keys {
        if std::env::var_os(key).is_none() {
            report("env-missing", &format!("key={}", key));
        }
    }

    report(
        "fds",
        &format!("open={} sockets={}", join(&open), join(&sockets)),
    );

    let blocked: Vec<&str> = Signal::iterator()
        .filter(|s| mask.contains(*s))
        .map(short_name)
        .collect();
    report("sigmask", &format!("blocked={}", blocked.join(",")));

    for sig in [
        Signal::SIGTTIN,
        Signal::SIGTTOU,
        Signal::SIGINT,
        Signal::SIGQUIT,
    ] {
        report(
            "sigdisp",
            &format!("sig={} ignored={}", short_name(sig), is_ignored(sig)),
        );
    }

    report("tty", &tty);

    if let Ok(cwd) = std::env::current_dir() {
        report("cwd", &format!("value={}", cwd.display()));
    }

    report("dump-done", "");

    if then_exit {
        exit_reported(0);
    }
    wait_signals(&set, DEFAULT_TRAP, DEFAULT_EXIT_ON);
}

/// A bound socket with no peer is a listener (SO_ACCEPTCONN is unreliable on macOS)
fn is_listening(fd: i32) -> bool {
    is_inet_bound(fd) && !has_peer(fd)
}

fn cmd_listen() -> ! {
    // Block signals before spawning threads so every thread inherits the mask
    let set = block_signals(DEFAULT_TRAP);

    report_started("child");
    report(
        "listen-env",
        &format!(
            "fds={} listen_pid={}",
            std::env::var("LISTEN_FDS").unwrap_or_default(),
            std::env::var("LISTEN_PID").unwrap_or_default()
        ),
    );

    // Discover inherited listeners by scanning rather than trusting fd 3..,
    // so this works even when scinit does not remap fds
    let (_, sockets) = scan_fds();
    let pid = unistd::getpid();
    for fd in sockets {
        if !is_listening(fd) {
            report("not-listening", &format!("fd={}", fd));
            continue;
        }
        let listener = unsafe { TcpListener::from_raw_fd(fd) };
        let Ok(port) = listener.local_addr().map(|a| a.port()) else {
            continue;
        };
        report("listening", &format!("fd={} port={}", fd, port));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                // Reply without reading: closing with unread input would
                // reset the connection before the client reads the reply
                let _ = writeln!(stream, "pid={} fd={} port={}", pid, fd, port);
            }
        });
    }
    report("ready", "");

    wait_signals(&set, DEFAULT_TRAP, DEFAULT_EXIT_ON);
}

fn proc_state(pid: Pid) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    // Field 3, after the parenthesised command name
    let after = stat.rsplit_once(')')?.1;
    after.trim_start().chars().next()
}

fn cmd_spawn_orphan() -> ! {
    report_started("child");

    // Fork an intermediate process that forks the orphan and exits at once,
    // so the orphan is reparented to the nearest subreaper / PID 1
    match unsafe { unistd::fork() }.expect("fork failed") {
        ForkResult::Child => {
            let intermediate = unistd::getpid();
            match unsafe { unistd::fork() }.expect("fork failed") {
                ForkResult::Child => {
                    // Once the intermediate is gone, report who adopted the orphan
                    let deadline = Instant::now() + Duration::from_secs(1);
                    while unistd::getppid() == intermediate && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    report("orphan-parent", &format!("ppid={}", unistd::getppid()));
                    std::thread::sleep(Duration::from_millis(300));
                    unsafe { libc::_exit(0) };
                }
                ForkResult::Parent { child } => {
                    report("orphan", &format!("orphan_pid={}", child));
                    unsafe { libc::_exit(0) };
                }
            }
        }
        ForkResult::Parent { child } => {
            let _ = nix::sys::wait::waitpid(child, None);
        }
    }

    // Find the orphan pid from our own report line
    let orphan = last_reported_field("orphan_pid")
        .and_then(|p| p.parse().ok())
        .map(Pid::from_raw);

    // Give the orphan time to exit and scinit time to reap it
    std::thread::sleep(Duration::from_secs(2));

    let verdict = match orphan.map(proc_state) {
        Some(Some('Z')) => "ORPHAN_ZOMBIE",
        Some(_) => "ORPHAN_REAPED",
        None => "ORPHAN_UNKNOWN",
    };
    report("orphan-check", &format!("result={}", verdict));
    println!("{}", verdict);
    signal_loop(DEFAULT_TRAP, DEFAULT_EXIT_ON);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((cmd, rest)) = args.split_first() else {
        eprintln!("usage: scinit-test-child <run|exit|kill-self|dump|listen|spawn-orphan> ...");
        std::process::exit(64);
    };
    match cmd.as_str() {
        "run" => cmd_run(rest),
        "exit" => cmd_exit(rest),
        "kill-self" => cmd_kill_self(rest),
        "dump" => cmd_dump(rest),
        "listen" => cmd_listen(),
        "spawn-orphan" => cmd_spawn_orphan(),
        other => {
            eprintln!("unknown subcommand: {}", other);
            std::process::exit(64);
        }
    }
}
