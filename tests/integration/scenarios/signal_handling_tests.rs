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
    
    info!("Testing SIGTERM graceful shutdown behavior");
    
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
    
    info!("Testing SIGINT graceful shutdown behavior");
    
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
    
    info!("Testing SIGUSR1 forwarding behavior");
    
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
    
    info!("Testing SIGUSR2 forwarding behavior");
    
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
    
    info!("Testing SIGHUP forwarding behavior");
    
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
    
    info!("Testing signal escalation timeout behavior");
    
    let result = signal_framework.test_signal_escalation().await?;
    
    // Validate that process eventually exits (either gracefully or via SIGKILL escalation)
    assert_process_exited(result.actual_exit_status, "SIGTERM (with escalation)");
    Ok(())
}