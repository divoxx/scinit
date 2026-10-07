//! Socket activation: `--ports` binds listeners before spawning the child and
//! passes them via the systemd protocol (`LISTEN_FDS`, `LISTEN_PID`, fds 3..).

use crate::integration::harness::{
    assert_exit_code, env_value, free_ports, request, socket_fds, Event, Scinit, TIMEOUT,
};
use std::net::TcpListener;

/// Comma-separated `--ports` value
fn ports_arg(ports: &[u16]) -> String {
    ports
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// Start scinit with `--ports` and a `dump` child; wait for the dump to finish
fn dump_with_ports(ports: &[u16]) -> (Scinit, Vec<Event>) {
    let scinit = Scinit::builder()
        .args(["--ports", &ports_arg(ports)])
        .child(["dump"])
        .spawn()
        .unwrap();
    let events = scinit
        .wait_for("dump-done", TIMEOUT, |evs| evs.iter().any(|e| e.name == "dump-done"))
        .unwrap();
    (scinit, events)
}

fn listen_fds_matches_port_count(n: usize) {
    let (scinit, events) = dump_with_ports(&free_ports(n));
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
    let (scinit, events) = dump_with_ports(&free_ports(1));
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
    let (scinit, events) = dump_with_ports(&free_ports(n));
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
        .args(["--ports", &ports_arg(&ports)])
        .child(["listen"])
        .spawn()
        .unwrap();
    scinit.wait_for_event("ready", TIMEOUT).unwrap();
    for (i, port) in ports.iter().enumerate() {
        let fd = 3 + i;
        assert!(
            scinit.events_named("listening").iter().any(|e| {
                e.get("fd") == Some(fd.to_string().as_str())
                    && e.get("port") == Some(port.to_string().as_str())
            }),
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
    let ports = free_ports(1);
    let scinit = Scinit::builder()
        .env("LISTEN_FDS", "7")
        .env("LISTEN_PID", "1")
        .env("LISTEN_FDNAMES", "stale")
        .args(["--ports", &ports_arg(&ports)])
        .child(["dump"])
        .spawn()
        .unwrap();
    let events = scinit
        .wait_for("dump-done", TIMEOUT, |evs| evs.iter().any(|e| e.name == "dump-done"))
        .unwrap();
    let pid = scinit.child_pid().unwrap();
    assert_eq!(env_value(&events, "LISTEN_FDS"), Some("1".to_string()));
    assert_eq!(env_value(&events, "LISTEN_PID"), Some(pid.to_string()));
    assert_eq!(env_value(&events, "LISTEN_FDNAMES"), None, "{}", scinit.diagnostics());
}

/// A bare command name is resolved through PATH, and the child sees its own
/// pid in LISTEN_PID (checked here by a shell against `$$`)
#[test]
fn listen_pid_matches_shell_pid_via_path_lookup() {
    let ports = free_ports(1);
    let (scinit, status) = Scinit::builder()
        .args(["--ports", &ports_arg(&ports)])
        .command(["sh", "-c", "echo \"listen_pid=$LISTEN_PID self=$$\""])
        .run(TIMEOUT)
        .unwrap();
    assert_exit_code(&scinit, status, 0);
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
        .args(["--ports", &ports_arg(&ports)])
        .child(["listen"])
        .spawn()
        .unwrap();
    scinit.wait_for_event("ready", TIMEOUT).unwrap();
    let pid = scinit.child_pid().unwrap();

    let listening: Vec<u16> = scinit
        .events_named("listening")
        .iter()
        .map(|e| e.get("port").unwrap().parse().unwrap())
        .collect();
    for port in &ports {
        assert!(
            listening.contains(port),
            "child did not inherit a listener for port {}\n{}",
            port,
            scinit.diagnostics()
        );
        let reply = request(&format!("127.0.0.1:{}", port))
            .unwrap_or_else(|e| panic!("request to {} failed: {}\n{}", port, e, scinit.diagnostics()));
        assert!(
            reply.starts_with(&format!("pid={} ", pid)),
            "reply {:?} on port {} did not come from child {}\n{}",
            reply,
            port,
            pid,
            scinit.diagnostics()
        );
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
        .args(["--bind-addr", "::1", "--ports", &port.to_string()])
        .child(["listen"])
        .spawn()
        .unwrap();
    scinit.wait_for_event("ready", TIMEOUT).unwrap();
    let pid = scinit.child_pid().unwrap();
    let reply = request(&format!("[::1]:{}", port))
        .unwrap_or_else(|e| panic!("request failed: {}\n{}", e, scinit.diagnostics()));
    assert!(
        reply.starts_with(&format!("pid={} ", pid)),
        "reply {:?} did not come from child {}\n{}",
        reply,
        pid,
        scinit.diagnostics()
    );
}

/// Without `--ports`, a `LISTEN_FDS` already in scinit's environment reaches
/// the child unchanged.
///
/// CURRENT behaviour, asserted so a change is noticed. Phase 2 may decide to
/// strip inherited `LISTEN_*` vars when scinit passes no sockets (a stale
/// `LISTEN_FDS` makes the child trust fds it doesn't have); if so, flip this
/// to assert `None` and rename it `inherited_listen_fds_stripped_without_ports`.
#[test]
fn inherited_listen_fds_leaks_without_ports() {
    let scinit = Scinit::builder()
        .env("LISTEN_FDS", "7")
        .child(["dump", "--then-exit"])
        .spawn()
        .unwrap();
    let events = scinit
        .wait_for("dump-done", TIMEOUT, |evs| evs.iter().any(|e| e.name == "dump-done"))
        .unwrap();
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
    let scinit = Scinit::builder()
        .child(["dump", "--then-exit"])
        .spawn()
        .unwrap();
    let events = scinit
        .wait_for("dump-done", TIMEOUT, |evs| evs.iter().any(|e| e.name == "dump-done"))
        .unwrap();
    let listen: Vec<&Event> = events
        .iter()
        .filter(|e| e.name == "env" && e.get("key").is_some_and(|k| k.starts_with("LISTEN_")))
        .collect();
    assert!(listen.is_empty(), "unexpected LISTEN_* vars: {:?}", listen);
    assert!(socket_fds(&events).is_empty(), "{}", scinit.diagnostics());
}

/// A port held by a listener without SO_REUSEPORT cannot be bound: scinit
/// fails with exit 1 and never starts the child.
///
/// scinit sets SO_REUSEPORT on its own socket, but both sockets must set it
/// for the bind to share the port, so EADDRINUSE is the correct outcome on
/// both macOS and Linux.
#[test]
fn port_in_use_exits_1() {
    let holder = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = holder.local_addr().unwrap().port();
    let (scinit, status) = Scinit::builder()
        .args(["--ports", &port.to_string()])
        .child(["dump", "--then-exit"])
        .run(TIMEOUT)
        .unwrap();
    assert_exit_code(&scinit, status, 1);
    assert!(
        scinit.stderr().contains("Address already in use"),
        "expected EADDRINUSE\n{}",
        scinit.diagnostics()
    );
    assert!(
        scinit.events_named("started").is_empty(),
        "child should not start when binding fails\n{}",
        scinit.diagnostics()
    );
    drop(holder);
}
