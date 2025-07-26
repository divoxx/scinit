use super::Result;
use crate::port_manager::PortManager;
use eyre::eyre;
use nix::unistd::{getpgid, Pid};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout};
use tracing::{debug, error, info, warn};

use super::signals::Signal;

/// Configuration for process management behavior
#[derive(Debug, Clone)]
pub struct ProcessConfig {
    /// Command to execute
    pub command: String,
    /// Arguments for the command
    pub args: Vec<String>,
    /// Delay before restart after graceful shutdown
    pub restart_delay: Duration,
    /// Timeout for graceful shutdown
    pub graceful_shutdown_timeout: Duration,
    /// Working directory for the process
    pub working_directory: Option<PathBuf>,
    /// Environment variables to set
    pub environment: HashMap<String, String>,
}

impl Default for ProcessConfig {
    fn default() -> Self {
        Self {
            command: String::new(),
            args: Vec::new(),
            restart_delay: Duration::from_millis(1000),
            graceful_shutdown_timeout: Duration::from_secs(30),
            working_directory: None,
            environment: HashMap::new(),
        }
    }
}

/// State of a managed process
#[derive(Debug, Clone, PartialEq)]
pub enum ProcessState {
    /// Process is starting
    Starting,
    /// Process is running
    Running,
    /// Process is stopping (graceful shutdown)
    Stopping,
    /// Process has stopped
    Stopped,
    /// Process has failed and exceeded restart attempts
    Failed,
}

/// Information about a managed process
#[derive(Debug)]
pub struct ProcessInfo {
    /// Current state of the process
    pub state: ProcessState,
    /// Process ID (if running)
    pub pid: Option<Pid>,
    /// Start time of the current process
    pub start_time: std::time::Instant,
    /// Exit status of the last process (if stopped)
    pub exit_status: Option<std::process::ExitStatus>,
}

/// Manages the lifecycle of child processes with support for graceful restarts
/// 
/// This manager handles spawning, monitoring, and restarting child processes.
/// It supports graceful shutdown, port inheritance, and restart limiting.
pub struct ProcessManager {
    /// Configuration for process management
    config: ProcessConfig,
    /// Port manager for port inheritance
    port_manager: PortManager,
    /// Current process information
    process_info: ProcessInfo,
    /// Current child process handle
    child: Option<Child>,
    /// Whether the manager should stop managing processes
    should_stop: bool,
}

impl ProcessManager {
    /// Creates a new process manager with the given configuration
    /// 
    /// # Arguments
    /// * `config` - Configuration for process management
    /// * `port_manager` - Port manager for port inheritance
    /// 
    /// # Returns
    /// * `Self` - The process manager instance
    pub fn new(config: ProcessConfig, port_manager: PortManager) -> Self {
        Self {
            process_info: ProcessInfo {
                state: ProcessState::Stopped,
                pid: None,
                start_time: std::time::Instant::now(),
                exit_status: None,
            },
            config,
            port_manager,
            child: None,
            should_stop: false,
        }
    }

    /// Spawns a new process with the current configuration
    /// 
    /// This method spawns a new child process with port inheritance
    /// and updates the process state accordingly.
    /// 
    /// # Returns
    /// * `Result<()>` - Success or error
    pub async fn spawn_process(&mut self) -> Result<()> {
        if self.should_stop {
            return Ok(());
        }

        self.process_info.state = ProcessState::Starting;
        info!("Spawning process: {} {:?}", self.config.command, self.config.args);

        // Bind ports before spawning
        self.port_manager.bind_ports().await?;

        // Prepare environment variables
        let mut env_vars = std::env::vars().collect::<HashMap<_, _>>();
        
        // Add inherited file descriptors to environment
        let inherited_fds = self.port_manager.get_inherited_fds_string();
        if !inherited_fds.is_empty() {
            env_vars.insert("SCINIT_INHERITED_FDS".to_string(), inherited_fds);
        }

        // Add custom environment variables
        for (key, value) in &self.config.environment {
            env_vars.insert(key.clone(), value.clone());
        }

        // Create command
        let mut command = Command::new(&self.config.command);
        command.args(&self.config.args);

        // Set up process group and inheritance
        // process_group(0) creates a new process group with child as leader
        // This isolates the child from scinit's process group for proper signal handling
        command.process_group(0);
        command.kill_on_drop(true);
        command.stdin(Stdio::inherit());
        command.stdout(Stdio::inherit());
        command.stderr(Stdio::inherit());

        // CRITICAL: Reset signal mask for child process
        // Child processes inherit the parent's signal mask, but we want them to handle signals normally
        // This is essential for terminal signals like Ctrl+C to work in child processes
        unsafe {
            command.pre_exec(|| {
                use nix::sys::signal::{pthread_sigmask, SigmaskHow, SigSet};
                
                // Create empty signal mask (unblock all signals)
                let empty_mask = SigSet::empty();
                
                // Reset signal mask to default state for child process
                pthread_sigmask(SigmaskHow::SIG_SETMASK, Some(&empty_mask), None)
                    .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
                
                Ok(())
            });
        }

        // Set working directory if specified
        if let Some(ref work_dir) = self.config.working_directory {
            command.current_dir(work_dir);
        }

        // Set environment variables
        command.env_clear();
        for (key, value) in env_vars {
            command.env(key, value);
        }

        // Spawn the process
        let child = command.spawn()?;
        
        // Get the PID
        let pid = match child.id() {
            Some(pid) => Pid::from_raw(pid.try_into()?),
            None => return Err(eyre!("Failed to get process ID")),
        };

        // Update process info
        self.process_info.pid = Some(pid);
        self.process_info.state = ProcessState::Running;
        self.process_info.start_time = std::time::Instant::now();
        self.child = Some(child);

        info!("Process spawned with PID: {}", pid);
        Ok(())
    }

    /// Waits for the current process to exit
    /// 
    /// This method waits for the child process to exit and returns
    /// the exit status.
    /// 
    /// # Returns
    /// * `Result<Option<std::process::ExitStatus>>` - Exit status or None if no process
    pub async fn wait_for_exit(&mut self) -> Result<Option<std::process::ExitStatus>> {
        if let Some(ref mut child) = self.child {
            match child.wait().await {
                Ok(status) => {
                    self.process_info.exit_status = Some(status);
                    self.process_info.state = ProcessState::Stopped;
                    self.child = None;
                    
                    info!("Process exited with status: {:?}", status);
                    Ok(Some(status))
                }
                Err(e) => {
                    error!("Error waiting for process: {}", e);
                    self.process_info.state = ProcessState::Failed;
                    self.child = None;
                    Err(e.into())
                }
            }
        } else {
            Ok(None)
        }
    }

    /// Performs a graceful shutdown of the current process
    /// 
    /// This method sends SIGTERM to the process and waits for it to exit
    /// gracefully. If the process doesn't exit within the timeout,
    /// it sends SIGKILL.
    /// 
    /// # Returns
    /// * `Result<()>` - Success or error
    pub async fn graceful_shutdown(&mut self) -> Result<()> {
        if let Some(pid) = self.process_info.pid {
            self.process_info.state = ProcessState::Stopping;
            info!("Initiating graceful shutdown of process {}", pid);

            // Send SIGTERM
            if let Err(e) = self.forward_signal(Signal::SIGTERM) {
                warn!("Failed to send SIGTERM: {}", e);
            }

            // Wait for graceful shutdown
            match timeout(self.config.graceful_shutdown_timeout, self.wait_for_exit()).await {
                Ok(Ok(_)) => {
                    info!("Process exited gracefully");
                    Ok(())
                }
                Ok(Err(e)) => {
                    warn!("Error during graceful shutdown: {}", e);
                    self.force_kill().await?;
                    Ok(())
                }
                Err(_) => {
                    warn!("Graceful shutdown timeout, forcing kill");
                    self.force_kill().await?;
                    Ok(())
                }
            }
        } else {
            Ok(())
        }
    }

    /// Force kills the current process
    /// 
    /// This method sends SIGKILL to the process to force it to exit immediately.
    /// 
    /// # Returns
    /// * `Result<()>` - Success or error
    pub async fn force_kill(&mut self) -> Result<()> {
        if let Some(pid) = self.process_info.pid {
            info!("Force killing process {}", pid);

            // Send SIGKILL
            if let Err(e) = self.forward_signal(Signal::SIGKILL) {
                warn!("Failed to send SIGKILL: {}", e);
            }

            // Wait a bit for the process to exit
            sleep(Duration::from_millis(100)).await;
            
            // Check if process is still running
            if let Some(ref mut child) = self.child {
                if let Ok(Some(status)) = child.try_wait() {
                    self.process_info.exit_status = Some(status);
                    self.process_info.state = ProcessState::Stopped;
                    self.child = None;
                    info!("Process killed, exit status: {:?}", status);
                }
            }
        }

        Ok(())
    }


    /// Restarts the current process with a specific reason
    /// 
    /// This method performs a graceful shutdown of the current process and
    /// spawns a new one. Only file-change restarts are allowed in container environments.
    /// 
    /// # Arguments
    /// * `reason` - The reason for the restart (for logging and limit checking)
    /// 
    /// # Returns
    /// * `Result<bool>` - True if restart was successful, false if restart not allowed
    pub async fn restart_process_with_reason(&mut self, reason: &str) -> Result<bool> {
        if self.should_stop {
            return Ok(false);
        }

        // Only allow file-change restarts, not crash restarts
        let is_file_change_restart = reason == "file_change";
        
        if !is_file_change_restart {
            error!("Process restart not allowed for reason: {} (only file-change restarts are allowed)", reason);
            return Ok(false);
        }

        info!("Restarting process due to file change");

        // Graceful shutdown current process
        self.graceful_shutdown().await?;

        // Wait for restart delay
        sleep(self.config.restart_delay).await;

        // Spawn new process
        self.spawn_process().await?;

        Ok(true)
    }

    /// Forwards a signal to the current process
    /// 
    /// # Arguments
    /// * `signal` - The signal to forward
    /// 
    /// # Returns
    /// * `Result<()>` - Success or error
    pub fn forward_signal(&self, signal: Signal) -> Result<()> {
        self.send_signal_to_group(signal)
    }

    /// Sends a signal to the process group (synchronous version for Drop)
    /// 
    /// # Arguments
    /// * `signal` - The signal to send
    /// 
    /// # Returns
    /// * `Result<()>` - Success or error
    pub fn send_signal_to_group(&self, signal: Signal) -> Result<()> {
        if let Some(pid) = self.process_info.pid {
            use nix::sys::signal::kill;
            let pgid = getpgid(Some(pid))?;
            debug!("Sending signal {:?} to process group {}", signal, pgid);
            
            // Send signal to the entire process group
            kill(Pid::from_raw(-pgid.as_raw()), signal)?;
            Ok(())
        } else {
            Err(eyre!("No process to send signal to"))
        }
    }

    /// Gets the current process information
    /// 
    /// # Returns
    /// * `&ProcessInfo` - Current process information
    pub fn process_info(&self) -> &ProcessInfo {
        &self.process_info
    }

    /// Gets the current process state
    /// 
    /// # Returns
    /// * `ProcessState` - Current process state
    pub fn state(&self) -> ProcessState {
        self.process_info.state.clone()
    }

    /// Checks if the process is running
    /// 
    /// # Returns
    /// * `bool` - True if the process is running
    pub fn is_running(&self) -> bool {
        self.process_info.state == ProcessState::Running
    }

    /// Stops the process manager
    /// 
    /// This method sets the should_stop flag, which will prevent
    /// further process restarts.
    pub fn stop(&mut self) {
        self.should_stop = true;
        info!("Process manager stopped");
    }

}

impl Drop for ProcessManager {
    fn drop(&mut self) {
        // Scenario C: Emergency cleanup when ProcessManager is dropped unexpectedly
        // Only attempt cleanup if we still have a running process
        if let Some(pid) = self.process_info.pid {
            // Check if process is actually still running before emergency cleanup
            if self.process_info.state == ProcessState::Running || 
               self.process_info.state == ProcessState::Starting {
                eprintln!("ProcessManager dropped with running child (PID: {}), emergency cleanup", pid);
                
                // Emergency SIGKILL to process group - no graceful shutdown in Drop
                if let Err(e) = self.send_signal_to_group(Signal::SIGKILL) {
                    // Only log SIGKILL errors if they're not "process already dead" errors
                    match e.downcast_ref::<nix::Error>() {
                        Some(nix::Error::ESRCH) => {
                            // Process already dead - this is fine, no cleanup needed
                        }
                        _ => {
                            eprintln!("Failed to send SIGKILL to process group during emergency cleanup: {}", e);
                        }
                    }
                } else {
                    eprintln!("Sent SIGKILL to process group {} during emergency cleanup", pid);
                }
                
                // Brief pause to let SIGKILL take effect
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        
        // Stop the process manager to prevent further operations
        self.should_stop = true;
        
        // Note: Child process resources are automatically cleaned up by Rust's Drop
        // But we've ensured the process group is killed to prevent orphans
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::port_manager::PortBindingConfig;

    #[tokio::test]
    async fn test_process_manager_creation() {
        let config = ProcessConfig::default();
        let port_config = PortBindingConfig::default();
        let port_manager = PortManager::new(port_config);
        
        let manager = ProcessManager::new(config, port_manager);
        assert_eq!(manager.state(), ProcessState::Stopped);
        assert!(!manager.is_running());
    }

    #[tokio::test]
    async fn test_process_spawn() {
        let config = ProcessConfig {
            command: "echo".to_string(),
            args: vec!["hello".to_string()],
            ..Default::default()
        };
        let port_config = PortBindingConfig::default();
        let port_manager = PortManager::new(port_config);
        
        let mut manager = ProcessManager::new(config, port_manager);
        assert!(manager.spawn_process().await.is_ok());
        
        // Wait for process to exit
        let exit_status = manager.wait_for_exit().await.unwrap();
        assert!(exit_status.is_some());
        assert_eq!(manager.state(), ProcessState::Stopped);
    }

    #[tokio::test]
    async fn test_process_restart() {
        let config = ProcessConfig {
            command: "echo".to_string(),
            args: vec!["hello".to_string()],
            restart_delay: Duration::from_millis(100),
            ..Default::default()
        };
        let port_config = PortBindingConfig::default();
        let port_manager = PortManager::new(port_config);
        
        let mut manager = ProcessManager::new(config, port_manager);
        
        // Test file-change restart (should work)
        let restart_result = manager.restart_process_with_reason("file_change").await.unwrap();
        assert!(restart_result);
        
        // Test crash restart (should fail)
        let restart_result = manager.restart_process_with_reason("crash").await.unwrap();
        assert!(!restart_result);
    }

    #[tokio::test]
    async fn test_graceful_shutdown() {
        let config = ProcessConfig {
            command: "sleep".to_string(),
            args: vec!["10".to_string()],
            graceful_shutdown_timeout: Duration::from_millis(500),
            ..Default::default()
        };
        let port_config = PortBindingConfig::default();
        let port_manager = PortManager::new(port_config);
        
        let mut manager = ProcessManager::new(config, port_manager);
        assert!(manager.spawn_process().await.is_ok());
        assert!(manager.is_running());
        
        // Graceful shutdown should work
        assert!(manager.graceful_shutdown().await.is_ok());
        assert_eq!(manager.state(), ProcessState::Stopped);
    }

    #[tokio::test]
    async fn test_process_info() {
        let config = ProcessConfig {
            command: "echo".to_string(),
            args: vec!["hello".to_string()],
            ..Default::default()
        };
        let port_config = PortBindingConfig::default();
        let port_manager = PortManager::new(port_config);
        
        let mut manager = ProcessManager::new(config, port_manager);
        let info = manager.process_info();
        
        assert_eq!(info.state, ProcessState::Stopped);
        
        // Spawn process
        assert!(manager.spawn_process().await.is_ok());
        let info = manager.process_info();
        assert_eq!(info.state, ProcessState::Running);
        assert!(info.pid.is_some());
        
        // Wait for exit
        manager.wait_for_exit().await.unwrap();
        let info = manager.process_info();
        assert_eq!(info.state, ProcessState::Stopped);
        assert!(info.exit_status.is_some());
    }

    #[tokio::test]
    async fn test_environment_variables() {
        let mut env = HashMap::new();
        env.insert("TEST_VAR".to_string(), "test_value".to_string());
        
        let config = ProcessConfig {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), "echo $TEST_VAR".to_string()],
            environment: env,
            ..Default::default()
        };
        let port_config = PortBindingConfig::default();
        let port_manager = PortManager::new(port_config);
        
        let mut manager = ProcessManager::new(config, port_manager);
        assert!(manager.spawn_process().await.is_ok());
        
        let exit_status = manager.wait_for_exit().await.unwrap();
        assert!(exit_status.is_some());
    }

    #[tokio::test]
    async fn test_stop_management() {
        let config = ProcessConfig {
            command: "sleep".to_string(),
            args: vec!["10".to_string()],
            ..Default::default()
        };
        let port_config = PortBindingConfig::default();
        let port_manager = PortManager::new(port_config);
        
        let mut manager = ProcessManager::new(config, port_manager);
        
        assert!(!manager.should_stop);
        manager.stop();
        assert!(manager.should_stop);
        
        // Should not restart after stop
        let restart_result = manager.restart_process_with_reason("manual").await.unwrap();
        assert!(!restart_result);
    }
} 