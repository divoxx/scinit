//! Main integration test file for scinit
//! 
//! This file contains the entry point for integration tests.
//! Individual test scenarios are organized in the integration module.

mod integration;

// Re-export for convenience
pub use integration::*;

// A basic smoke test to verify the test framework itself works
#[tokio::test]
async fn test_framework_smoke_test() -> anyhow::Result<()> {
    use integration::ProcessTestHarness;
    use nix::sys::signal::Signal;
    
    // Initialize tracing for test output
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .try_init();
    
    // Simple test: spawn scinit with sleep and then kill it with SIGTERM
    let mut harness = ProcessTestHarness::new()?;
    let mut process = harness.spawn_scinit(&["sleep", "30"]).await?;
    
    // Allow process to start
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    
    // Verify process is running
    assert!(process.is_running(), "scinit should be running with sleep child");
    
    // Send SIGTERM to gracefully shut down
    nix::sys::signal::kill(process.pid, Signal::SIGTERM)?;
    
    // Wait for graceful shutdown
    let exit_status = process.wait_for_exit_timeout(std::time::Duration::from_secs(5)).await?;
    
    match exit_status {
        Some(status) => {
            println!("Process exited with status: {:?}", status);
            // For now, just verify it exited (don't require success since we're killing it)
        }
        None => {
            panic!("Process did not exit within timeout after SIGTERM");
        }
    }
    
    println!("✓ Integration test framework smoke test passed");
    Ok(())
}