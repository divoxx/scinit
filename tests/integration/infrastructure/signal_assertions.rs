use std::time::Duration;
use std::process::ExitStatus;

/// Signal-specific assertions for integration tests

/// Assert that a signal response time meets performance targets
pub fn assert_signal_response_time(actual: Duration, expected_max: Duration, signal_name: &str) {
    assert!(
        actual <= expected_max,
        "{} response time {:?} exceeded maximum {:?}",
        signal_name,
        actual,
        expected_max
    );
}

/// Assert that a process exited after receiving a signal
pub fn assert_process_exited(exit_status: Option<ExitStatus>, signal_name: &str) {
    assert!(
        exit_status.is_some(),
        "Process should have exited after receiving {}",
        signal_name
    );
}

/// Assert that a process is still running (for forwarded signals)
pub fn assert_process_still_running(is_running: bool, signal_name: &str) {
    assert!(
        is_running,
        "Process should still be running after receiving {} (signal should be forwarded)",
        signal_name
    );
}

/// Assert current buggy behavior (inverted logic for documenting known bugs)
pub fn assert_current_buggy_behavior(condition: bool, signal_name: &str, behavior_description: &str) {
    assert!(condition, "CURRENT (BUGGY) BEHAVIOR: {} - {}", signal_name, behavior_description);
}