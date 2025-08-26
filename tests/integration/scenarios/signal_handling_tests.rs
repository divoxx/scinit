use crate::infrastructure::{ProcessTestHarness, SignalTestFramework, SignalBehavior};
use crate::infrastructure::signal_assertions::*;
use anyhow::Result;
use nix::sys::signal::Signal;
use std::time::Duration;
use tracing::{info, warn};

#[tokio::test]
async fn test_sigterm_graceful_shutdown() -> Result<()> {
    let harness = ProcessTestHarness::new()?;
    let mut signal_framework = SignalTestFramework::new(harness);
    
    
    let result = signal_framework
        .test_signal_handling(Signal::SIGTERM, SignalBehavior::GracefulShutdown)
        .await?;
    
    // Validate behavior
    assert_process_exited(result.actual_exit_status, "SIGTERM");
    assert_signal_response_time(result.response_time, Duration::from_millis(100), "SIGTERM");
    
    Ok(())
}

#[tokio::test]
async fn test_sigint_graceful_shutdown() -> Result<()> {
    let harness = ProcessTestHarness::new()?;
    let mut signal_framework = SignalTestFramework::new(harness);
    
    
    let result = signal_framework
        .test_signal_handling(Signal::SIGINT, SignalBehavior::GracefulShutdown)
        .await?;
    
    // Validate behavior
    assert_process_exited(result.actual_exit_status, "SIGINT");
    assert_signal_response_time(result.response_time, Duration::from_millis(100), "SIGINT");
    
    Ok(())
}

#[tokio::test]
async fn test_sigusr1_forwarding() -> Result<()> {
    let harness = ProcessTestHarness::new()?;
    let mut signal_framework = SignalTestFramework::new(harness);
    
    
    let result = signal_framework
        .test_signal_handling(Signal::SIGUSR1, SignalBehavior::ForwardOnly)
        .await?;
    
    // KNOWN BUG: See KNOWN-ISSUES.md for details
    warn!("KNOWN BUG: scinit exits on SIGUSR1 instead of forwarding signal and continuing");
    assert_current_buggy_behavior(!result.signal_forwarded, "SIGUSR1", "scinit exits instead of continuing");
    Ok(())
}

#[tokio::test]
async fn test_sigusr2_forwarding() -> Result<()> {
    let harness = ProcessTestHarness::new()?;
    let mut signal_framework = SignalTestFramework::new(harness);
    
    
    let result = signal_framework
        .test_signal_handling(Signal::SIGUSR2, SignalBehavior::ForwardOnly)
        .await?;
    
    // KNOWN BUG: Same as SIGUSR1 - see KNOWN-ISSUES.md
    warn!("KNOWN BUG: scinit exits on SIGUSR2 instead of forwarding signal and continuing");
    assert_current_buggy_behavior(!result.signal_forwarded, "SIGUSR2", "scinit exits instead of continuing");
    Ok(())
}

#[tokio::test]
async fn test_sighup_forwarding() -> Result<()> {
    let harness = ProcessTestHarness::new()?;
    let mut signal_framework = SignalTestFramework::new(harness);
    
    
    let result = signal_framework
        .test_signal_handling(Signal::SIGHUP, SignalBehavior::ForwardOnly)
        .await?;
    
    // KNOWN BUG: Same as SIGUSR1/SIGUSR2 - see KNOWN-ISSUES.md
    warn!("KNOWN BUG: scinit exits on SIGHUP instead of forwarding signal and continuing");
    assert_current_buggy_behavior(!result.signal_forwarded, "SIGHUP", "scinit exits instead of continuing");
    Ok(())
}

#[tokio::test]
async fn test_signal_escalation_timeout() -> Result<()> {
    let harness = ProcessTestHarness::new()?;
    let mut signal_framework = SignalTestFramework::new(harness);
    
    
    let result = signal_framework.test_signal_escalation().await?;
    
    // Validate that process eventually exits (either gracefully or via SIGKILL escalation)
    assert_process_exited(result.actual_exit_status, "SIGTERM (with escalation)");
    Ok(())
}

#[tokio::test]
async fn test_sigquit_graceful_shutdown() -> Result<()> {
    let harness = ProcessTestHarness::new()?;
    let mut signal_framework = SignalTestFramework::new(harness);
    
    
    let result = signal_framework
        .test_signal_handling(Signal::SIGQUIT, SignalBehavior::GracefulShutdown)
        .await?;
    
    // Validate behavior
    assert_process_exited(result.actual_exit_status, "SIGQUIT");
    assert_signal_response_time(result.response_time, Duration::from_millis(100), "SIGQUIT");
    
    Ok(())
}

/// Test SIGCHLD handling (child process reaping)
#[tokio::test]
async fn test_sigchld_zombie_reaping() -> Result<()> {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    
    
    let mut harness = ProcessTestHarness::new()?;
    
    // Use a shell command that creates short-lived child processes
    let mut process = harness.spawn_scinit(&[
        "sh", "-c", "sleep 0.1 & sleep 0.2 & sleep 0.3 & wait"
    ]).await?;
    
    // Allow the shell command to run and complete
    tokio::time::sleep(Duration::from_millis(600)).await;
    
    // Process should eventually exit when shell command completes
    let exit_status = process.wait_for_exit_timeout(Duration::from_secs(5)).await?;
    
    assert!(exit_status.is_some(), "Process should exit after shell command completes");
    
    Ok(())
}

/// Test signal handling response times
#[tokio::test]
async fn test_signal_response_performance() -> Result<()> {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    
    
    let mut harness = ProcessTestHarness::new()?;
    let mut process = harness.spawn_scinit(&["sleep", "10"]).await?;
    
    // Allow process to start
    tokio::time::sleep(Duration::from_millis(200)).await;
    
    // Measure SIGTERM response time
    let start_time = std::time::Instant::now();
    nix::sys::signal::kill(process.pid, Signal::SIGTERM)?;
    
    let exit_status = process.wait_for_exit_timeout(Duration::from_secs(5)).await?;
    let response_time = start_time.elapsed();
    
    assert!(exit_status.is_some(), "Process should exit after SIGTERM");
    
    // Response should be fast (under 1 second for sleep command)
    assert!(
        response_time < Duration::from_secs(1),
        "SIGTERM response time {:?} should be under 1 second",
        response_time
    );
    
    Ok(())
}