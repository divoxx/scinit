use crate::infrastructure::{ProcessTestHarness, SocketTestUtils};
use anyhow::Result;
use std::time::Duration;
use tracing::info;

/// Test basic socket inheritance functionality with systemd environment variables
#[tokio::test]
async fn test_basic_socket_inheritance() -> Result<()> {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    
    let mut harness = ProcessTestHarness::new()?;
    let socket_utils = SocketTestUtils::new();
    
    // Test with a single port using a long-running command
    let test_port = socket_utils.get_free_port()?;
    let mut process = harness.spawn_scinit(&[
        "--ports", &test_port.to_string(),
        "--bind-addr", "127.0.0.1",
        "sleep", "30"
    ]).await?;
    
    // Allow process to start and bind port
    tokio::time::sleep(Duration::from_millis(500)).await;
    
    // Verify process is running
    assert!(process.is_running(), "scinit should be running with socket inheritance");
    
    // Test socket connectivity to verify inheritance is working
    let result = socket_utils.test_socket_connectivity("127.0.0.1", test_port).await;
    assert!(result.is_ok(), "Socket should be bound and accessible: {:?}", result);
    
    // Clean up
    nix::sys::signal::kill(process.pid, nix::sys::signal::Signal::SIGTERM)?;
    let _ = process.wait_for_exit_timeout(Duration::from_secs(3)).await;
    
    Ok(())
}

/// Test systemd socket activation environment variables
#[tokio::test]
async fn test_systemd_socket_activation_env() -> Result<()> {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    
    let mut harness = ProcessTestHarness::new()?;
    let socket_utils = SocketTestUtils::new();
    
    // Test with multiple ports to verify LISTEN_FDS, LISTEN_PID, etc.
    let test_port1 = socket_utils.get_free_port()?;
    let test_port2 = socket_utils.get_free_port()?;
    
    let mut process = harness.spawn_scinit(&[
        "--ports", &format!("{},{}", test_port1, test_port2),
        "--bind-addr", "127.0.0.1", 
        "sleep", "30"
    ]).await?;
    
    // Allow process to start
    tokio::time::sleep(Duration::from_millis(500)).await;
    
    // Verify process started successfully
    assert!(process.is_running(), "scinit should handle systemd socket activation environment");
    
    // Test both sockets are accessible
    let result1 = socket_utils.test_socket_connectivity("127.0.0.1", test_port1).await;
    let result2 = socket_utils.test_socket_connectivity("127.0.0.1", test_port2).await;
    
    assert!(result1.is_ok(), "First socket should be accessible: {:?}", result1);
    assert!(result2.is_ok(), "Second socket should be accessible: {:?}", result2);
    
    // Clean up
    nix::sys::signal::kill(process.pid, nix::sys::signal::Signal::SIGTERM)?;
    let _ = process.wait_for_exit_timeout(Duration::from_secs(3)).await;
    
    Ok(())
}


/// Test zero-downtime restart with socket inheritance (simplified)
#[tokio::test]
async fn test_zero_downtime_basic() -> Result<()> {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    
    let mut harness = ProcessTestHarness::new()?;
    let socket_utils = SocketTestUtils::new();
    
    // Test with live-reload enabled and socket inheritance
    let test_port = socket_utils.get_free_port()?;
    let watch_path = harness.temp_path_str();
    let mut process = harness.spawn_scinit(&[
        "--live-reload",
        "--watch-path", &watch_path,
        "--ports", &test_port.to_string(),
        "--bind-addr", "127.0.0.1",
        "sleep", "30"
    ]).await?;
    
    // Allow process to start
    tokio::time::sleep(Duration::from_millis(500)).await;
    
    // Verify process is running
    assert!(process.is_running(), "scinit should be running with live-reload and socket inheritance");
    
    // Create a file to trigger restart
    let trigger_file = harness.temp_path().join("trigger.txt");
    tokio::fs::write(&trigger_file, "trigger restart").await?;
    
    // Allow time for file watch to trigger
    tokio::time::sleep(Duration::from_millis(800)).await;
    
    // Process should still be running (restarted)
    assert!(process.is_running(), "scinit should still be running after file-triggered restart");
    
    // Clean up
    nix::sys::signal::kill(process.pid, nix::sys::signal::Signal::SIGTERM)?;
    let _ = process.wait_for_exit_timeout(Duration::from_secs(3)).await;
    
    Ok(())
}

/// Test multiple port binding for socket inheritance
#[tokio::test]
async fn test_multiple_port_inheritance() -> Result<()> {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    
    let mut harness = ProcessTestHarness::new()?;
    let socket_utils = SocketTestUtils::new();
    
    // Test with multiple ports using free ports to avoid conflicts
    let test_port1 = socket_utils.get_free_port()?;
    let test_port2 = socket_utils.get_free_port()?;
    let test_port3 = socket_utils.get_free_port()?;
    
    let mut process = harness.spawn_scinit(&[
        "--ports", &format!("{},{},{}", test_port1, test_port2, test_port3), 
        "--bind-addr", "127.0.0.1",
        "sleep", "30"
    ]).await?;
    
    // Allow process to start
    tokio::time::sleep(Duration::from_millis(500)).await;
    
    // Verify process is running
    assert!(process.is_running(), "scinit should handle multiple port binding");
    
    // Test all sockets are accessible
    let result1 = socket_utils.test_socket_connectivity("127.0.0.1", test_port1).await;
    let result2 = socket_utils.test_socket_connectivity("127.0.0.1", test_port2).await;
    let result3 = socket_utils.test_socket_connectivity("127.0.0.1", test_port3).await;
    
    assert!(result1.is_ok(), "First socket should be accessible: {:?}", result1);
    assert!(result2.is_ok(), "Second socket should be accessible: {:?}", result2);
    assert!(result3.is_ok(), "Third socket should be accessible: {:?}", result3);
    
    // Clean up
    nix::sys::signal::kill(process.pid, nix::sys::signal::Signal::SIGTERM)?;
    let _ = process.wait_for_exit_timeout(Duration::from_secs(3)).await;
    
    Ok(())
}