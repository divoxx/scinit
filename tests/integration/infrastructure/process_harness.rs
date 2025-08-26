use anyhow::{Context, Result};
use nix::{sys::signal::Signal, unistd::Pid};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::process::Command;
use tokio::time::timeout;

/// Core testing harness for managing scinit processes during integration tests
pub struct ProcessTestHarness {
    scinit_binary: PathBuf,
    temp_dir: TempDir,
    environment: HashMap<String, String>,
    cleanup_pids: Vec<Pid>,
}

impl ProcessTestHarness {
    /// Create a new test harness, locating the scinit binary
    pub fn new() -> Result<Self> {
        let scinit_binary = Self::find_scinit_binary()?;
        let temp_dir = TempDir::new().context("Failed to create temporary directory")?;
        
        Ok(Self {
            scinit_binary,
            temp_dir,
            environment: HashMap::new(),
            cleanup_pids: Vec::new(),
        })
    }

    /// Set an environment variable for spawned processes
    pub fn set_environment(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.environment.insert(key.into(), value.into());
    }

    /// Get the temporary directory path for test files
    pub fn temp_path(&self) -> &std::path::Path {
        self.temp_dir.path()
    }

    /// Spawn scinit with the given arguments
    pub async fn spawn_scinit(&mut self, args: &[&str]) -> Result<TestProcess> {
        let mut cmd = Command::new(&self.scinit_binary);
        cmd.args(args);
        
        // Set environment variables
        for (key, value) in &self.environment {
            cmd.env(key, value);
        }

        // Spawn in a new process group for easier cleanup
        cmd.process_group(0);
        
        let start_time = Instant::now();
        let child = cmd.spawn()
            .context("Failed to spawn scinit process")?;
        
        let pid = Pid::from_raw(child.id()
            .ok_or_else(|| anyhow::anyhow!("Failed to get child PID"))? as i32);
        
        // Track PID for cleanup
        self.cleanup_pids.push(pid);
        
        Ok(TestProcess {
            pid,
            process_group: pid, // For simplicity, assume PID == PGID
            start_time,
            child: Some(child),
        })
    }

    /// Find the scinit binary, trying multiple locations
    fn find_scinit_binary() -> Result<PathBuf> {
        let candidates = [
            "target/debug/scinit",
            "target/release/scinit", 
            "./scinit",
        ];
        
        for candidate in &candidates {
            let path = PathBuf::from(candidate);
            if path.exists() {
                return Ok(path);
            }
        }
        
        // Try building if not found
        std::process::Command::new("cargo")
            .args(&["build", "--bin", "scinit"])
            .status()
            .context("Failed to build scinit")?;
            
        let debug_path = PathBuf::from("target/debug/scinit");
        if debug_path.exists() {
            Ok(debug_path)
        } else {
            Err(anyhow::anyhow!("Could not locate scinit binary after building"))
        }
    }
}

impl Drop for ProcessTestHarness {
    fn drop(&mut self) {
        // Clean up any remaining processes
        for pid in &self.cleanup_pids {
            let _ = nix::sys::signal::kill(*pid, Signal::SIGKILL);
        }
    }
}

/// Represents a spawned test process with timing and control capabilities
pub struct TestProcess {
    pub pid: Pid,
    pub process_group: Pid,
    pub start_time: Instant,
    child: Option<tokio::process::Child>,
}

impl TestProcess {
    /// Wait for process exit with a timeout
    pub async fn wait_for_exit_timeout(&mut self, duration: Duration) -> Result<Option<ExitStatus>> {
        if let Some(child) = &mut self.child {
            match timeout(duration, child.wait()).await {
                Ok(result) => Ok(Some(result.context("Process wait failed")?)),
                Err(_) => Ok(None), // Timeout occurred
            }
        } else {
            Ok(None)
        }
    }
    
    /// Get the runtime duration since process start
    pub fn runtime(&self) -> Duration {
        self.start_time.elapsed()
    }

    /// Check if the process is still running
    pub fn is_running(&mut self) -> bool {
        if let Some(child) = &mut self.child {
            child.try_wait().unwrap_or(None).is_none()
        } else {
            false
        }
    }
}