use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use anyhow::{Context, Result};

/// Simplified socket testing utilities
pub struct SocketTestUtils {
    next_port: std::sync::atomic::AtomicU16,
}

impl SocketTestUtils {
    /// Create a new SocketTestUtils instance
    pub fn new() -> Self {
        Self {
            next_port: std::sync::atomic::AtomicU16::new(9000),
        }
    }

    /// Get a free port for testing (simplified approach)
    pub fn get_free_port(&self) -> Result<u16> {
        use std::sync::atomic::Ordering;
        let port = self.next_port.fetch_add(1, Ordering::SeqCst);
        Ok(port)
    }

    /// Test socket connectivity to a specific address and port
    pub async fn test_socket_connectivity(&self, addr: &str, port: u16) -> Result<()> {
        let socket_addr = format!("{}:{}", addr, port);
        let addr = socket_addr.parse::<SocketAddr>()
            .context("Invalid socket address")?;
        
        let timeout_duration = Duration::from_millis(500);
        
        match tokio::time::timeout(timeout_duration, TcpStream::connect(addr)).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(anyhow::anyhow!("Failed to connect: {}", e)),
            Err(_) => Err(anyhow::anyhow!("Connection timeout")),
        }
    }

    /// Simple connectivity test to a port
    pub async fn test_port_connectivity(port: u16) -> Result<bool> {
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let timeout_duration = Duration::from_millis(200);
        
        match tokio::time::timeout(timeout_duration, TcpStream::connect(addr)).await {
            Ok(Ok(_)) => Ok(true),
            Ok(Err(_)) => Ok(false),
            Err(_) => Ok(false), // Timeout
        }
    }

    /// Simple echo test to verify server functionality
    pub async fn test_echo_response(port: u16, message: &str) -> Result<String> {
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let timeout_duration = Duration::from_millis(200);
        
        let mut stream = tokio::time::timeout(timeout_duration, TcpStream::connect(addr))
            .await
            .context("Connection timeout")?
            .context("Failed to connect")?;

        // Send message
        stream.write_all(message.as_bytes()).await?;
        stream.write_all(b"\n").await?;
        
        // Read response
        let mut buffer = vec![0; 1024];
        let n = stream.read(&mut buffer).await?;
        
        Ok(String::from_utf8_lossy(&buffer[..n]).trim().to_string())
    }
}

/// Result of connectivity testing  
#[derive(Debug)]
pub struct ConnectivityResult {
    pub port_results: HashMap<u16, bool>,
    pub all_successful: bool,
}

/// Environment variable utilities for socket inheritance
pub struct SocketInheritanceEnv;

impl SocketInheritanceEnv {
    /// Parse SCINIT_INHERITED_FDS environment variable
    pub fn parse_inherited_fds(env_value: &str) -> Result<Vec<i32>> {
        if env_value.is_empty() {
            return Ok(Vec::new());
        }
        
        env_value
            .split(',')
            .map(|s| s.trim().parse::<i32>().context("Failed to parse FD"))
            .collect()
    }

    /// Format file descriptors for SCINIT_INHERITED_FDS
    pub fn format_inherited_fds(fds: &[i32]) -> String {
        fds.iter()
            .map(|fd| fd.to_string())
            .collect::<Vec<_>>()
            .join(",")
    }
}