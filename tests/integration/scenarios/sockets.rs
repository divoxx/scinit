//! Socket activation: `--ports` binds listeners before spawning the child and
//! passes them via the systemd protocol (`LISTEN_FDS`, `LISTEN_PID`, fds 3..).

use crate::integration::harness::{
    env_value, free_port, free_ports, loopback, open_fds, socket_fds, wait_for_pid_gone, Event,
    Scinit, TIMEOUT,
};
use nix::sys::signal::Signal;
use std::net::TcpListener;

fn listen_fds_matches_port_count(n: usize) {
    let (scinit, events) = Scinit::builder().ports(&free_ports(n)).spawn_dump(&[]);
    assert_eq!(
        env_value(&events, "LISTEN_FDS"),
        Some(n.to_string()),
        "{}",
        scinit.diagnostics()
    );
}

/// `LISTEN_FDS` is the number of ports passed with `--ports` (one port)
#[test]
fn listen_fds_one_port() {
    listen_fds_matches_port_count(1);
}

/// `LISTEN_FDS` is the number of ports passed with `--ports` (three ports)
#[test]
fn listen_fds_three_ports() {
    listen_fds_matches_port_count(3);
}

/// `LISTEN_PID` must be the child's own pid, or sd_listen_fds() ignores the fds
#[test]
fn listen_pid_is_child_pid() {
    let (scinit, events) = Scinit::builder().ports(&free_ports(1)).spawn_dump(&[]);
    let pid = scinit.child_pid().unwrap();
    assert_eq!(
        env_value(&events, "LISTEN_PID"),
        Some(pid.to_string()),
        "{}",
        scinit.diagnostics()
    );
}

/// Inherited sockets occupy exactly fds 3..3+n, as the systemd protocol requires
#[test]
fn inherited_fds_start_at_3_and_are_contiguous() {
    let n = 3;
    let (scinit, events) = Scinit::builder().ports(&free_ports(n)).spawn_dump(&[]);
    let expected: Vec<i32> = (3..3 + n as i32).collect();
    assert_eq!(socket_fds(&events), expected, "{}", scinit.diagnostics());
}

/// Sockets are passed in `--ports` order: fd 3 is the first port listed
#[test]
fn fds_follow_ports_order() {
    let mut ports = free_ports(3);
    // Descending, so port order can't be mistaken for sorted order
    ports.sort_unstable_by(|a, b| b.cmp(a));
    let scinit = Scinit::builder()
        .ports(&ports)
        .child(["listen"])
        .spawn()
        .unwrap();
    scinit.wait_for_event("ready", TIMEOUT).unwrap();
    let listeners = scinit.listeners();
    for (i, port) in ports.iter().enumerate() {
        let fd = 3 + i as i32;
        assert!(
            listeners.contains(&(fd, *port)),
            "expected fd {} to be port {}\n{}",
            fd,
            port,
            scinit.diagnostics()
        );
    }
}

/// With `--ports`, socket activation variables inherited by scinit describe
/// someone else's sockets and are replaced, not passed through
#[test]
fn inherited_listen_vars_replaced_with_ports() {
    let (scinit, events) = Scinit::builder()
        .env("LISTEN_FDS", "7")
        .env("LISTEN_PID", "1")
        .env("LISTEN_FDNAMES", "stale")
        .ports(&free_ports(1))
        .spawn_dump(&[]);
    let pid = scinit.child_pid().unwrap();
    assert_eq!(env_value(&events, "LISTEN_FDS"), Some("1".to_string()));
    assert_eq!(env_value(&events, "LISTEN_PID"), Some(pid.to_string()));
    assert_eq!(env_value(&events, "LISTEN_FDNAMES"), None, "{}", scinit.diagnostics());
}

/// A bare command name is resolved through PATH, and the child sees its own
/// pid in LISTEN_PID (checked here by a shell against `$$`)
#[test]
fn listen_pid_matches_shell_pid_via_path_lookup() {
    let (scinit, status) = Scinit::builder()
        .ports(&free_ports(1))
        .command(["sh", "-c", "echo \"listen_pid=$LISTEN_PID self=$$\""])
        .run(TIMEOUT)
        .unwrap();
    scinit.assert_exit_code(status, 0);
    let stdout = scinit.stdout();
    let line = stdout
        .lines()
        .find(|l| l.starts_with("listen_pid="))
        .unwrap_or_else(|| panic!("no output\n{}", scinit.diagnostics()));
    let (listen_pid, own_pid) = line["listen_pid=".len()..].split_once(" self=").unwrap();
    assert_eq!(listen_pid, own_pid, "{}", scinit.diagnostics());
}

/// The child (not scinit's own listen backlog) accepts on every port
#[test]
fn listen_child_answers_on_every_port() {
    let ports = free_ports(3);
    let scinit = Scinit::builder()
        .ports(&ports)
        .child(["listen"])
        .spawn()
        .unwrap();
    scinit.wait_for_event("ready", TIMEOUT).unwrap();
    let pid = scinit.child_pid().unwrap();

    let listening: Vec<u16> = scinit.listeners().iter().map(|(_, port)| *port).collect();
    for port in &ports {
        assert!(
            listening.contains(port),
            "child did not inherit a listener for port {}\n{}",
            port,
            scinit.diagnostics()
        );
        let reply = scinit.assert_reply_from(&loopback(*port), pid);
        assert!(reply.ends_with(&format!("port={}", port)), "reply {:?}", reply);
    }
}

/// `--bind-addr ::1` binds IPv6 listeners the child can serve
#[test]
fn ipv6_bind_addr() {
    // Pick a free port on ::1; skip if the host has no IPv6 loopback
    let port = match TcpListener::bind("[::1]:0").and_then(|l| l.local_addr()) {
        Ok(addr) => addr.port(),
        Err(e) => {
            eprintln!("skipping ipv6_bind_addr: ::1 unavailable ({})", e);
            return;
        }
    };
    let scinit = Scinit::builder()
        .args(["--bind-addr", "::1"])
        .ports(&[port])
        .child(["listen"])
        .spawn()
        .unwrap();
    scinit.wait_for_event("ready", TIMEOUT).unwrap();
    let pid = scinit.child_pid().unwrap();
    scinit.assert_reply_from(&format!("[::1]:{}", port), pid);
}

/// Without `--ports`, a `LISTEN_FDS` already in scinit's environment reaches
/// the child unchanged.
///
/// Asserted so a change is noticed: a stale `LISTEN_FDS` makes the child
/// trust fds it doesn't have.
#[test]
fn inherited_listen_fds_leaks_without_ports() {
    let (scinit, events) = Scinit::builder()
        .env("LISTEN_FDS", "7")
        .spawn_dump(&["--then-exit"]);
    assert_eq!(
        env_value(&events, "LISTEN_FDS").as_deref(),
        Some("7"),
        "{}",
        scinit.diagnostics()
    );
}

/// Without `--ports`, scinit sets no `LISTEN_*` variables
#[test]
fn no_ports_no_listen_vars() {
    let (scinit, events) = Scinit::builder().spawn_dump(&["--then-exit"]);
    let listen: Vec<&Event> = events
        .iter()
        .filter(|e| e.is("env") && e.get("key").is_some_and(|k| k.starts_with("LISTEN_")))
        .collect();
    assert!(listen.is_empty(), "unexpected LISTEN_* vars: {:?}", listen);
    assert!(socket_fds(&events).is_empty(), "{}", scinit.diagnostics());
}

/// An inheritable fd scinit itself inherited doesn't reach the child: it
/// gets stdio only
#[test]
fn stray_inherited_fd_not_passed_without_ports() {
    let (scinit, events) = Scinit::builder().leak_fd().spawn_dump(&["--then-exit"]);
    assert_eq!(open_fds(&events), [0, 1, 2], "{}", scinit.diagnostics());
}

/// With `--ports`, the child gets stdio and the activated sockets only, not
/// the stray fd scinit inherited
#[test]
fn stray_inherited_fd_not_passed_with_ports() {
    let (scinit, events) = Scinit::builder()
        .leak_fd()
        .ports(&free_ports(2))
        .spawn_dump(&["--then-exit"]);
    assert_eq!(open_fds(&events), [0, 1, 2, 3, 4], "{}", scinit.diagnostics());
}

/// A port held by another listener cannot be bound: scinit fails with exit 1
/// and never starts the child
#[test]
fn port_in_use_exits_1() {
    let holder = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = holder.local_addr().unwrap().port();
    let (scinit, status) = Scinit::builder()
        .ports(&[port])
        .child(["dump", "--then-exit"])
        .run(TIMEOUT)
        .unwrap();
    scinit.assert_exit_code(status, 1);
    assert!(
        scinit.stderr().contains("Address already in use"),
        "expected EADDRINUSE\n{}",
        scinit.diagnostics()
    );
    scinit.assert_start_count(0, "child should not start when binding fails");
    drop(holder);
}

/// Start scinit with `args` and a `listen` child on `port`; wait until it answers
fn start_listening(port: u16, args: &[&str]) -> (Scinit, i32) {
    let scinit = Scinit::builder()
        .args(args)
        .ports(&[port])
        .child(["listen"])
        .spawn()
        .unwrap();
    scinit.wait_for_event("ready", TIMEOUT).unwrap();
    let pid = scinit.child_pid().unwrap();
    scinit.assert_reply_from(&loopback(port), pid);
    (scinit, pid)
}

/// Without `--reuse-port`, a second scinit can't bind a port that a running
/// scinit listens on
#[test]
fn second_scinit_cannot_share_port_by_default() {
    let port = free_port();
    let (_first, _) = start_listening(port, &[]);
    let (second, status) = Scinit::builder()
        .ports(&[port])
        .child(["dump", "--then-exit"])
        .run(TIMEOUT)
        .unwrap();
    second.assert_exit_code(status, 1);
    assert!(
        second.stderr().contains("Address already in use"),
        "expected EADDRINUSE\n{}",
        second.diagnostics()
    );
    second.assert_start_count(0, "child should not start when binding fails");
}

/// With `--reuse-port` on both, two scinits listen on the same port. Which
/// one answers is up to the kernel (Linux load-balances across listeners,
/// macOS prefers the latest), so only check that the second one bound.
#[test]
fn reuse_port_lets_two_scinits_share_port() {
    let port = free_port();
    let (_first, _) = start_listening(port, &["--reuse-port"]);
    let second = Scinit::builder()
        .args(["--reuse-port"])
        .ports(&[port])
        .child(["listen"])
        .spawn()
        .unwrap();
    second.wait_for_event("ready", TIMEOUT).unwrap();
    assert!(
        second.listeners().iter().any(|(_, p)| *p == port),
        "{}",
        second.diagnostics()
    );
}

/// A new scinit can bind a port right after the previous one exits, while the
/// connections it served (and closed first) are still in TIME_WAIT
#[test]
fn rebind_after_serving_connections() {
    let port = free_port();
    let (first, pid) = start_listening(port, &[]);
    for _ in 0..5 {
        first.assert_reply_from(&loopback(port), pid);
    }
    // Stop it gracefully so scinit itself reaps the child. Killing scinit
    // would orphan the child, and in the Linux test container the orphan is
    // re-parented to `cargo test` (PID 1), which never reaps it.
    let mut first = first;
    first.signal(Signal::SIGTERM).unwrap();
    first.wait_exit(TIMEOUT).unwrap();
    assert!(wait_for_pid_gone(pid, TIMEOUT), "listen child {} still running", pid);

    let (second, status) = Scinit::builder()
        .ports(&[port])
        .child(["dump", "--then-exit"])
        .run(TIMEOUT)
        .unwrap();
    second.assert_exit_code(status, 0);
    second.assert_start_count(1, "child should start once the port is bound");
}
