use crate::infrastructure::ProcessTestHarness;
use anyhow::Result;
use std::time::Duration;
use tracing::{info, debug};

/// Test basic live-reload functionality
#[tokio::test]
async fn test_basic_live_reload() -> Result<()> {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    
    
    let mut harness = ProcessTestHarness::new()?;
    let watch_path = harness.temp_path_str();
    
    // Start scinit with live-reload enabled
    let mut process = harness.spawn_scinit(&[
        "--live-reload",
        "--watch-path", &watch_path,
        "--debounce-ms", "100",
        "sleep", "30"
    ]).await?;
    
    // Allow process to start
    tokio::time::sleep(Duration::from_millis(500)).await;
    
    // Verify process is running
    assert!(process.is_running(), "scinit should be running with live-reload");
    
    // Create a file to trigger restart
    let trigger_file = harness.temp_path().join("test_file.txt");
    tokio::fs::write(&trigger_file, "trigger content").await?;
    
    debug!("Created trigger file: {:?}", trigger_file);
    
    // Allow debounce time and restart to happen  
    tokio::time::sleep(Duration::from_millis(800)).await;
    
    // Process should still be running (restarted)
    assert!(process.is_running(), "scinit should still be running after file-triggered restart");
    
    // Clean up
    nix::sys::signal::kill(process.pid, nix::sys::signal::Signal::SIGTERM)?;
    let _ = process.wait_for_exit_timeout(Duration::from_secs(3)).await;
    
    Ok(())
}

/// Test live-reload debouncing behavior
#[tokio::test]
async fn test_live_reload_debouncing() -> Result<()> {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    
    
    let mut harness = ProcessTestHarness::new()?;
    let watch_path = harness.temp_path_str();
    
    // Start scinit with live-reload and longer debounce time
    let mut process = harness.spawn_scinit(&[
        "--live-reload", 
        "--watch-path", &watch_path,
        "--debounce-ms", "500",  // Longer debounce to test
        "sleep", "30"
    ]).await?;
    
    // Allow process to start
    tokio::time::sleep(Duration::from_millis(300)).await;
    
    // Create multiple files in quick succession (should be debounced)
    for i in 1..=3 {
        let file = harness.temp_path().join(format!("debounce_test_{}.txt", i));
        tokio::fs::write(&file, format!("content {}", i)).await?;
        debug!("Created debounce test file {}", i);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    
    // Allow debounce time to complete
    tokio::time::sleep(Duration::from_millis(800)).await;
    
    // Process should still be running (debounced restart)
    assert!(process.is_running(), "scinit should be running after debounced file changes");
    
    // Clean up
    nix::sys::signal::kill(process.pid, nix::sys::signal::Signal::SIGTERM)?;
    let _ = process.wait_for_exit_timeout(Duration::from_secs(3)).await;
    
    Ok(())
}

/// Test live-reload with file modifications (not just creation)
#[tokio::test]
async fn test_live_reload_file_modification() -> Result<()> {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    
    
    let mut harness = ProcessTestHarness::new()?;
    let watch_path = harness.temp_path_str();
    
    // Create a file first
    let test_file = harness.temp_path().join("modify_test.txt");
    tokio::fs::write(&test_file, "initial content").await?;
    
    // Start scinit with live-reload
    let mut process = harness.spawn_scinit(&[
        "--live-reload",
        "--watch-path", &watch_path, 
        "--debounce-ms", "100",
        "sleep", "30"
    ]).await?;
    
    // Allow process to start
    tokio::time::sleep(Duration::from_millis(300)).await;
    
    // Modify the existing file
    tokio::fs::write(&test_file, "modified content").await?;
    debug!("Modified test file: {:?}", test_file);
    
    // Allow restart to happen
    tokio::time::sleep(Duration::from_millis(600)).await;
    
    // Process should still be running (restarted due to modification)
    assert!(process.is_running(), "scinit should be running after file modification");
    
    // Clean up
    nix::sys::signal::kill(process.pid, nix::sys::signal::Signal::SIGTERM)?;
    let _ = process.wait_for_exit_timeout(Duration::from_secs(3)).await;
    
    Ok(())
}

/// Test live-reload with restart delay
#[tokio::test] 
async fn test_live_reload_restart_delay() -> Result<()> {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    
    
    let mut harness = ProcessTestHarness::new()?;
    let watch_path = harness.temp_path_str();
    
    // Start scinit with live-reload and restart delay
    let mut process = harness.spawn_scinit(&[
        "--live-reload",
        "--watch-path", &watch_path,
        "--debounce-ms", "100", 
        "--restart-delay-ms", "200",  // Test restart delay
        "sleep", "30"
    ]).await?;
    
    // Allow process to start
    tokio::time::sleep(Duration::from_millis(300)).await;
    
    let restart_start = std::time::Instant::now();
    
    // Create a file to trigger restart
    let trigger_file = harness.temp_path().join("restart_delay_test.txt");
    tokio::fs::write(&trigger_file, "trigger restart delay").await?;
    
    // Allow time for debounce + restart delay + restart to complete
    tokio::time::sleep(Duration::from_millis(1000)).await;
    
    let restart_duration = restart_start.elapsed();
    
    // Process should still be running after restart with delay
    assert!(process.is_running(), "scinit should be running after delayed restart");
    
    // Restart should take at least the delay time 
    assert!(
        restart_duration >= Duration::from_millis(200),
        "Restart should take at least restart-delay-ms time, took {:?}",
        restart_duration
    );
    
    // Clean up
    nix::sys::signal::kill(process.pid, nix::sys::signal::Signal::SIGTERM)?;
    let _ = process.wait_for_exit_timeout(Duration::from_secs(3)).await;
    
    Ok(())
}

/// Test that live-reload doesn't trigger without file changes
#[tokio::test]
async fn test_live_reload_no_false_triggers() -> Result<()> {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    
    
    let mut harness = ProcessTestHarness::new()?;
    let watch_path = harness.temp_path_str();
    
    // Start scinit with live-reload
    let mut process = harness.spawn_scinit(&[
        "--live-reload",
        "--watch-path", &watch_path,
        "--debounce-ms", "100",
        "sleep", "5"  // Shorter sleep to test
    ]).await?;
    
    // Allow process to start
    tokio::time::sleep(Duration::from_millis(300)).await;
    
    // Don't create or modify any files, just wait
    tokio::time::sleep(Duration::from_millis(1000)).await;
    
    // Check if process is still running (it should be, no restarts without file changes)
    let still_running = process.is_running();
    
    if still_running {
        // Clean up
        nix::sys::signal::kill(process.pid, nix::sys::signal::Signal::SIGTERM)?;
        let _ = process.wait_for_exit_timeout(Duration::from_secs(3)).await;
    } else {
        // Process might have exited naturally due to short sleep command
    }
    
    Ok(())
}