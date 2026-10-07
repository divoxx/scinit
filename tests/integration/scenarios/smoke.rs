use crate::integration::harness::{env_value, free_ports, Scinit, ScinitBuilder, TIMEOUT};
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;

/// The harness can start scinit, observe the child, and see scinit exit
#[test]
fn harness_smoke() {
    let (scinit, _status) = Scinit::builder().child(["exit", "0"]).run(TIMEOUT).unwrap();
    let started = scinit.events_named("started");
    assert_eq!(started.len(), 1, "{}", scinit.diagnostics());
    assert_ne!(started[0].pid(), scinit.pid().as_raw());
    assert_eq!(
        started[0].pgid(),
        started[0].pid(),
        "child should lead its own process group"
    );
}

/// A variable that isn't valid UTF-8 reaches the child byte for byte
fn assert_non_utf8_env_passed_through(builder: ScinitBuilder) {
    let value = OsString::from_vec(b"caf\xff\xfe".to_vec());
    let (scinit, status) = builder
        .env("SCINIT_TEST_BYTES", value)
        .child(["dump", "--env", "SCINIT_TEST_BYTES", "--then-exit"])
        .run(TIMEOUT)
        .unwrap();
    assert_eq!(
        env_value(&scinit.events(), "SCINIT_TEST_BYTES").as_deref(),
        Some(r"caf\xff\xfe"),
        "{}",
        scinit.diagnostics()
    );
    scinit.assert_exit_code(status, 0);
}

#[test]
fn non_utf8_env_passed_through() {
    assert_non_utf8_env_passed_through(Scinit::builder());
}

#[test]
fn non_utf8_env_passed_through_with_ports() {
    assert_non_utf8_env_passed_through(Scinit::builder().ports(&free_ports(1)));
}
