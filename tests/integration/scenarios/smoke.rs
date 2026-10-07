use crate::integration::harness::{Scinit, TIMEOUT};

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
