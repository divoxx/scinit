//! Test fixture child process for scinit integration tests.
//!
//! scinit spawns this binary as its child. It appends newline-delimited events
//! to the file named by `$SCINIT_TEST_REPORT`, so tests can observe what the
//! child actually experienced (signals received, environment, fds, ...).
//!
//! Event format: `<name> pid=<pid> key=value ...`. A trailing `value=` field
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
use std::fs::OpenOptions;
use std::io::Write;
use std::net::TcpListener;
use std::os::fd::FromRawFd;
use std::str::FromStr;
use std::time::Duration;

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
    let line = if fields.is_empty() {
        format!("{} pid={}\n", event, unistd::getpid())
    } else {
        format!("{} pid={} {}\n", event, unistd::getpid(), fields)
    };
    if let Ok(path) = std::env::var("SCINIT_TEST_REPORT") {
        // O_APPEND with a single write keeps lines atomic across processes
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = f.write_all(line.as_bytes());
        }
    }
    eprint!("[test-child] {}", line);
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

/// Block `trap` signals and handle them synchronously, logging each one.
/// Exits with status 0 when a signal in `exit_on` arrives.
fn signal_loop(trap: &[Signal], exit_on: &[Signal]) -> ! {
    let mut set = SigSet::empty();
    for sig in trap {
        set.add(*sig);
    }
    set.thread_block().expect("failed to block signals");

    if trap.is_empty() {
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }

    loop {
        let sig = set.wait().expect("sigwait failed");
        report("signal", &format!("sig={}", short_name(sig)));
        if exit_on.contains(&sig) {
            report("exit", "code=0");
            std::process::exit(0);
        }
    }
}

fn cmd_run(args: &[String]) -> ! {
    let mut trap: Vec<Signal> = DEFAULT_TRAP.to_vec();
    let mut exit_on: Vec<Signal> = DEFAULT_EXIT_ON.to_vec();
    let mut ignore: Vec<Signal> = Vec::new();
    let mut grandchild = false;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--trap" => trap = parse_signals(it.next().expect("--trap needs a value")),
            "--exit-on" => exit_on = parse_signals(it.next().expect("--exit-on needs a value")),
            "--ignore" => ignore = parse_signals(it.next().expect("--ignore needs a value")),
            "--grandchild" => grandchild = true,
            other => panic!("unknown run option: {}", other),
        }
    }

    for sig in &ignore {
        unsafe { signal::signal(*sig, SigHandler::SigIgn) }.expect("failed to ignore signal");
    }
    trap.retain(|s| !ignore.contains(s));
    exit_on.retain(|s| !ignore.contains(s));

    let mut role = "child";
    if grandchild {
        // Single-threaded at this point, so fork is safe
        match unsafe { unistd::fork() }.expect("fork failed") {
            ForkResult::Child => role = "grandchild",
            ForkResult::Parent { .. } => {}
        }
    }

    report_started(role);
    signal_loop(&trap, &exit_on);
}

fn cmd_exit(args: &[String]) -> ! {
    let code: i32 = args
        .first()
        .expect("exit needs a code")
        .parse()
        .expect("exit code must be an integer");
    report_started("child");
    report("exit", &format!("code={}", code));
    std::process::exit(code);
}

fn cmd_kill_self(args: &[String]) -> ! {
    let sig = parse_signal(args.first().expect("kill-self needs a signal"));
    report_started("child");
    report("raise", &format!("sig={}", short_name(sig)));
    // KILL and STOP can't have their disposition changed (EINVAL)
    if !matches!(sig, Signal::SIGKILL | Signal::SIGSTOP) {
        unsafe { signal::signal(sig, SigHandler::SigDfl) }.expect("failed to reset handler");
    }
    let mut set = SigSet::empty();
    set.add(sig);
    let _ = set.thread_unblock();
    signal::raise(sig).expect("raise failed");
    // SIGSTOP-like signals could land here; never report success
    std::thread::sleep(Duration::from_secs(5));
    std::process::exit(99);
}

/// Open fds in 0..1024, split into all fds and socket fds.
fn scan_fds() -> (Vec<i32>, Vec<i32>) {
    let mut open = Vec::new();
    let mut sockets = Vec::new();
    for fd in 0..1024 {
        if unsafe { libc::fcntl(fd, libc::F_GETFD) } == -1 {
            continue;
        }
        open.push(fd);
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut st) } == 0 && (st.st_mode & libc::S_IFMT) == libc::S_IFSOCK
        {
            sockets.push(fd);
        }
    }
    (open, sockets)
}

fn join(fds: &[i32]) -> String {
    fds.iter()
        .map(|fd| fd.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

fn is_ignored(sig: Signal) -> bool {
    let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::sigaction(sig as libc::c_int, std::ptr::null(), &mut old) };
    rc == 0 && old.sa_sigaction == libc::SIG_IGN
}

fn cmd_dump(args: &[String]) -> ! {
    let mut env_keys: Vec<String> = Vec::new();
    let mut then_exit = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--env" => env_keys.push(it.next().expect("--env needs a key").clone()),
            "--then-exit" => then_exit = true,
            _ => {}
        }
    }

    // Capture process state before anything here changes it
    let mask = SigSet::thread_get_mask().expect("failed to read signal mask");
    let (open, sockets) = scan_fds();

    report_started("child");

    for (i, arg) in std::env::args().enumerate() {
        report("arg", &format!("index={} value={}", i, arg));
    }

    for (key, value) in std::env::vars() {
        if key.starts_with("LISTEN_") || env_keys.contains(&key) {
            report("env", &format!("key={} value={}", key, value));
        }
    }
    for key in &env_keys {
        if std::env::var_os(key).is_none() {
            report("env-missing", &format!("key={}", key));
        }
    }

    report("fds", &format!("open={} sockets={}", join(&open), join(&sockets)));

    let blocked: Vec<&str> = Signal::iterator()
        .filter(|s| mask.contains(*s))
        .map(short_name)
        .collect();
    report("sigmask", &format!("blocked={}", blocked.join(",")));

    for sig in [Signal::SIGTTIN, Signal::SIGTTOU] {
        report(
            "sigdisp",
            &format!("sig={} ignored={}", short_name(sig), is_ignored(sig)),
        );
    }

    if let Ok(cwd) = std::env::current_dir() {
        report("cwd", &format!("value={}", cwd.display()));
    }

    report("dump-done", "");

    if then_exit {
        report("exit", "code=0");
        std::process::exit(0);
    }
    signal_loop(DEFAULT_TRAP, DEFAULT_EXIT_ON);
}

/// A bound socket with no peer is a listener (SO_ACCEPTCONN is unreliable on macOS)
fn is_listening(fd: i32) -> bool {
    let mut addr: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    let bound = unsafe { libc::getsockname(fd, &mut addr as *mut _ as *mut libc::sockaddr, &mut len) } == 0
        && (addr.ss_family as i32 == libc::AF_INET || addr.ss_family as i32 == libc::AF_INET6);
    if !bound {
        return false;
    }
    let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    let rc = unsafe { libc::getpeername(fd, &mut addr as *mut _ as *mut libc::sockaddr, &mut len) };
    rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOTCONN)
}

fn cmd_listen() -> ! {
    // Block signals before spawning threads so every thread inherits the mask
    let mut set = SigSet::empty();
    for sig in DEFAULT_TRAP {
        set.add(*sig);
    }
    set.thread_block().expect("failed to block signals");

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

    loop {
        let sig = set.wait().expect("sigwait failed");
        report("signal", &format!("sig={}", short_name(sig)));
        if DEFAULT_EXIT_ON.contains(&sig) {
            report("exit", "code=0");
            std::process::exit(0);
        }
    }
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
        ForkResult::Child => match unsafe { unistd::fork() }.expect("fork failed") {
            ForkResult::Child => {
                std::thread::sleep(Duration::from_millis(300));
                unsafe { libc::_exit(0) };
            }
            ForkResult::Parent { child } => {
                report("orphan", &format!("orphan_pid={}", child));
                unsafe { libc::_exit(0) };
            }
        },
        ForkResult::Parent { child } => {
            let _ = nix::sys::wait::waitpid(child, None);
        }
    }

    // Find the orphan pid from our own report line
    let orphan = std::env::var("SCINIT_TEST_REPORT")
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| {
            s.lines()
                .filter_map(|l| l.split_whitespace().find_map(|f| f.strip_prefix("orphan_pid=")))
                .last()
                .and_then(|p| p.parse::<i32>().ok())
        })
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
