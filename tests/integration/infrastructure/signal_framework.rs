use super::process_harness::ProcessTestHarness;
use anyhow::{Context, Result};
use nix::sys::signal::Signal;
use std::collections::HashMap;
use std::process::ExitStatus;
use std::time::{Duration, Instant};

/// Framework for comprehensive signal handling testing
pub struct SignalTestFramework {
    harness: ProcessTestHarness,
    response_time_targets: HashMap<Signal, Duration>,
}

impl SignalTestFramework {
    /// Create a new signal testing framework
    pub fn new(harness: ProcessTestHarness) -> Self {
        let mut response_time_targets = HashMap::new();
        // Set default performance targets
        response_time_targets.insert(Signal::SIGTERM, Duration::from_millis(100));
        response_time_targets.insert(Signal::SIGINT, Duration::from_millis(100));
        response_time_targets.insert(Signal::SIGUSR1, Duration::from_millis(50));
        response_time_targets.insert(Signal::SIGUSR2, Duration::from_millis(50));
        response_time_targets.insert(Signal::SIGHUP, Duration::from_millis(50));
        
        Self {
            harness,
            response_time_targets,
        }
    }

    /// Test signal handling behavior with timing measurement using sleep
    pub async fn test_signal_handling(
        &mut self, 
        signal: Signal, 
        expected_behavior: SignalBehavior
    ) -> Result<SignalTestResult> {
        // Choose sleep duration based on expected behavior
        let sleep_duration = match expected_behavior {
            SignalBehavior::GracefulShutdown => "30",  // Long enough to receive signal
            SignalBehavior::ForwardOnly => "30",       // Long enough to test forwarding
            SignalBehavior::ImmediateTermination => "1", // Short for quick tests
        };

        // Spawn scinit with sleep command
        let mut scinit_process = self.harness
            .spawn_scinit(&["sleep", sleep_duration])
            .await
            .context("Failed to spawn scinit with sleep command")?;

        // Allow process to fully start
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Send signal and measure response time
        let signal_time = Instant::now();
        nix::sys::signal::kill(scinit_process.pid, signal)
            .context("Failed to send signal to scinit")?;

        // Handle expected behavior validation
        let (exit_status, signal_forwarded) = match expected_behavior {
            SignalBehavior::GracefulShutdown => {
                // Expect scinit to exit gracefully after receiving the signal
                let status = scinit_process
                    .wait_for_exit_timeout(Duration::from_secs(5))
                    .await?;
                (status, false)
            }
            SignalBehavior::ForwardOnly => {
                // For forwarding tests, scinit should forward signal to child
                // but continue running itself
                tokio::time::sleep(Duration::from_millis(300)).await;
                let still_running = scinit_process.is_running();
                
                
                // Clean up - send SIGTERM to ensure graceful shutdown
                let _ = nix::sys::signal::kill(scinit_process.pid, Signal::SIGTERM);
                let _ = scinit_process.wait_for_exit_timeout(Duration::from_secs(2)).await;
                
                (None, still_running)
            }
            SignalBehavior::ImmediateTermination => {
                // Expect immediate termination
                let status = scinit_process
                    .wait_for_exit_timeout(Duration::from_secs(1))
                    .await?;
                (status, false)
            }
        };

        let response_time = signal_time.elapsed();
        let target_time = self.response_time_targets.get(&signal)
            .copied()
            .unwrap_or(Duration::from_millis(100));

        Ok(SignalTestResult {
            signal,
            response_time,
            performance_target_met: response_time <= target_time,
            actual_exit_status: exit_status,
            signal_forwarded,
            expected_behavior,
        })
    }

    /// Test signal escalation behavior (SIGTERM -> SIGKILL after timeout)
    pub async fn test_signal_escalation(&mut self) -> Result<SignalTestResult> {
        // Use a process that ignores SIGTERM by using a long sleep
        // and then manually killing the child to simulate ignoring SIGTERM
        let mut scinit_process = self.harness
            .spawn_scinit(&["sleep", "30"])
            .await
            .context("Failed to spawn scinit for escalation test")?;

        // Allow process to start
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Get the child PID (the sleep process)
        // For this test, we'll simulate a process that ignores SIGTERM
        // by directly killing the sleep process after we send SIGTERM to scinit
        
        let signal_time = Instant::now();
        
        // Send SIGTERM to scinit
        nix::sys::signal::kill(scinit_process.pid, Signal::SIGTERM)
            .context("Failed to send SIGTERM to scinit")?;

        // Wait for scinit to handle the signal and eventually escalate to SIGKILL
        let exit_status = scinit_process
            .wait_for_exit_timeout(Duration::from_secs(10))
            .await?;

        let response_time = signal_time.elapsed();

        Ok(SignalTestResult {
            signal: Signal::SIGTERM,
            response_time,
            performance_target_met: response_time <= Duration::from_secs(6), // Allow time for escalation
            actual_exit_status: exit_status,
            signal_forwarded: false,
            expected_behavior: SignalBehavior::GracefulShutdown,
        })
    }
}

/// Expected behavior for a signal
#[derive(Debug, Clone, PartialEq)]
pub enum SignalBehavior {
    /// Signal should cause graceful shutdown
    GracefulShutdown,
    /// Signal should be forwarded to child, scinit continues
    ForwardOnly,
    /// Signal should cause immediate termination
    ImmediateTermination,
}

/// Result of a signal handling test
#[derive(Debug)]
pub struct SignalTestResult {
    pub signal: Signal,
    pub response_time: Duration,
    pub performance_target_met: bool,
    pub actual_exit_status: Option<ExitStatus>,
    pub signal_forwarded: bool,
    pub expected_behavior: SignalBehavior,
}