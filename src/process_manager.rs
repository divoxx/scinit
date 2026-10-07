use crate::environment::Environment;
use crate::port_manager::PortManager;
use crate::signals::signal_name;
use crate::socket_activation::SocketActivationExec;
use crate::Result;
use eyre::eyre;
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{getpgid, tcsetpgrp, Pid};
use std::collections::HashMap;
use std::fs::File;
use std::io::IsTerminal;
use std::process::Stdio;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout};
use tracing::{debug, error, info, warn};

use super::signals::Signal;

/// PID of the managed child, or 0 when there is none.
///
/// The zombie reaper must leave this process alone: its exit status belongs to
/// tokio's `child.wait()`, which fails with ECHILD if the reaper takes it first.
static MANAGED_CHILD: AtomicI32 = AtomicI32::new(0);

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
    /// Environment variables to set
    pub environment: Environment,
}

impl Default for ProcessConfig {
    fn default() -> Self {
        Self {
            command: String::new(),
            args: Vec::new(),
            restart_delay: Duration::from_millis(1000),
            graceful_shutdown_timeout: Duration::from_secs(30),
            environment: Environment::new(),
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
}

/// Information about a managed process
#[derive(Debug)]
pub struct ProcessInfo {
    /// Current state of the process
    pub state: ProcessState,
    /// Process ID (if running)
    pub pid: Option<Pid>,
    /// Exit status of the last process (if stopped)
    pub exit_status: Option<std::process::ExitStatus>,
}

/// Manages the lifecycle of child processes with graceful shutdown and port inheritance.
///
/// This manager handles spawning, monitoring, and restarting child processes.
/// Only file-change restarts are allowed in container environments.
pub struct ProcessManager {
    /// Configuration for process management
    config: ProcessConfig,
    /// Port manager for port inheritance
    port_manager: PortManager,
    /// Current process information
    process_info: ProcessInfo,
    /// Current child process handle
    child: Option<Child>,
}

impl ProcessManager {
    pub fn new(config: ProcessConfig, port_manager: PortManager) -> Self {
        Self {
            process_info: ProcessInfo {
                state: ProcessState::Stopped,
                pid: None,
                exit_status: None,
            },
            config,
            port_manager,
            child: None,
        }
    }

    /// Spawns a new process with port inheritance and proper signal mask reset
    pub async fn spawn_process(&mut self) -> Result<()> {
        self.process_info.state = ProcessState::Starting;
        info!("Spawning process: {} {:?}", self.config.command, self.config.args);

        // Bind ports before spawning
        self.port_manager.bind_ports()?;

        // Prepare environment variables using systemd socket activation
        let mut env_vars = Environment::from(std::env::vars().collect::<HashMap<_, _>>());

        // Add systemd socket activation environment variables
        let socket_env = self.port_manager.socket_activation_env();
        env_vars.extend(socket_env.clone());

        // Add custom environment variables
        env_vars.extend(self.config.environment.clone());

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

        // With sockets to pass, the child execs itself so it can move them to
        // fds 3.. and set LISTEN_PID to its own pid (see socket_activation)
        let listen_fds = self.port_manager.listen_fds();
        if !listen_fds.is_empty() {
            let mut exec = SocketActivationExec::new(
                &self.config.command,
                &self.config.args,
                listen_fds,
                socket_env,
                self.config.environment.clone(),
            )?;
            unsafe {
                command.pre_exec(move || Err(exec.exec_in_child()));
            }
        }

        // Set environment variables
        command.env_clear();
        for (key, value) in env_vars.into_inner() {
            command.env(key, value);
        }

        // Spawn the process
        let child = command.spawn()
            .map_err(|e| eyre!("Failed to spawn process '{}': {}", self.config.command, e))?;

        // Get the PID
        let pid = match child.id() {
            Some(pid) => Pid::from_raw(pid.try_into()?),
            None => return Err(eyre!("Failed to get process ID")),
        };

        // Update process info
        self.process_info.pid = Some(pid);
        self.process_info.state = ProcessState::Running;
        self.child = Some(child);
        MANAGED_CHILD.store(pid.as_raw(), Ordering::SeqCst);

        info!("Process spawned with PID: {}", pid);
        Ok(())
    }

    /// Waits for the current process to exit and returns its status, or
    /// `None` if there is no process
    pub async fn wait_for_exit(&mut self) -> Result<Option<std::process::ExitStatus>> {
        if let Some(ref mut child) = self.child {
            match child.wait().await {
                Ok(status) => {
                    self.process_info.exit_status = Some(status);
                    self.process_info.state = ProcessState::Stopped;
                    self.child = None;
                    MANAGED_CHILD.store(0, Ordering::SeqCst);

                    debug!("Process exited with status: {:?}", status);
                    Ok(Some(status))
                }
                Err(e) => {
                    error!("Error waiting for process: {}", e);
                    self.process_info.state = ProcessState::Stopped;
                    self.child = None;
                    MANAGED_CHILD.store(0, Ordering::SeqCst);
                    Err(e.into())
                }
            }
        } else {
            Ok(None)
        }
    }

    /// Performs a graceful shutdown of the current process with SIGTERM
    ///
    /// See [`ProcessManager::shutdown_with_signal`].
    pub async fn graceful_shutdown(&mut self) {
        self.shutdown_with_signal(Signal::SIGTERM).await
    }

    /// Stops the current process: sends `signal` to its process group and
    /// waits for it to exit. If it doesn't exit within the graceful shutdown
    /// timeout, it sends SIGKILL.
    pub async fn shutdown_with_signal(&mut self, signal: Signal) {
        if let Some(pid) = self.process_info.pid {
            self.process_info.state = ProcessState::Stopping;
            info!("Initiating graceful shutdown of process {} with {:?}", pid, signal);

            if let Err(e) = self.forward_signal(signal) {
                warn!("Failed to send {:?}: {}", signal, e);
            }

            // Wait for graceful shutdown
            match timeout(self.config.graceful_shutdown_timeout, self.wait_for_exit()).await {
                Ok(Ok(_)) => {
                    info!("Process exited gracefully");
                }
                Ok(Err(e)) => {
                    warn!("Error during graceful shutdown: {}", e);
                    self.force_kill().await;
                }
                Err(_) => {
                    warn!("Graceful shutdown timeout, forcing kill");
                    self.force_kill().await;
                }
            }
        }
    }

    /// Sends SIGKILL to the current process group and records the exit
    /// status if the process is gone 100ms later
    pub async fn force_kill(&mut self) {
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
                    MANAGED_CHILD.store(0, Ordering::SeqCst);
                    info!("Process killed, exit status: {:?}", status);
                }
            }
        }
    }

    /// Restarts the process after a file change: graceful shutdown, the
    /// restart delay, then a new spawn
    pub async fn restart(&mut self) -> Result<()> {
        info!("Restarting process due to file change");

        self.graceful_shutdown().await;
        sleep(self.config.restart_delay).await;
        self.spawn_process().await
    }

    /// Forwards a signal to the current process group
    pub fn forward_signal(&self, signal: Signal) -> Result<()> {
        self.send_signal_to_group(signal)
    }

    /// Sends a signal to the process group
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

    pub fn process_info(&self) -> &ProcessInfo {
        &self.process_info
    }

    #[cfg(test)]
    pub fn state(&self) -> ProcessState {
        self.process_info.state.clone()
    }

    #[cfg(test)]
    pub fn is_running(&self) -> bool {
        self.process_info.state == ProcessState::Running
    }
}

impl Drop for ProcessManager {
    fn drop(&mut self) {
        // Emergency cleanup when dropped with a child still running (e.g. on
        // an error return), so the child's process group isn't orphaned
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
    }
}

/// Sets the process group as the foreground process group if a terminal is available
pub fn process_group_to_foreground(pgid: Pid) -> Result<()> {
    match File::open("/dev/tty") {
        Ok(tty) => {
            if tty.is_terminal() {
                debug!("Setting process group {} as foreground", &pgid);
                if let Err(e) = tcsetpgrp(tty, pgid) {
                    error!("Failed to set process group {} as foreground: {}", &pgid, e);
                    return Err(e.into());
                }
            } else {
                debug!("Not a terminal, skipping foreground process group setup");
            }
        }
        Err(e) => {
            debug!(
                "Cannot open /dev/tty ({}), skipping foreground process group setup",
                e
            );
        }
    }
    Ok(())
}

/// Returns the PID of a child that has exited, without reaping it
fn peek_exited_child() -> Option<Pid> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::waitid(
            libc::P_ALL,
            0,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if rc != 0 {
        // ECHILD: no children at all
        return None;
    }
    #[cfg(target_os = "linux")]
    let pid = unsafe { info.si_pid() };
    #[cfg(not(target_os = "linux"))]
    let pid = info.si_pid;
    // With WNOHANG, si_pid stays 0 when no child has exited yet
    (pid != 0).then(|| Pid::from_raw(pid))
}

/// Reaps zombie processes to prevent process table exhaustion
///
/// Each exited child is peeked at before being reaped, so the managed child
/// is left for tokio's `child.wait()`. While the managed child is the zombie
/// returned first, other zombies wait for the next pass; tokio reaps the
/// managed child promptly and `handle_child_exit` runs a final pass.
pub fn reap_zombies() {
    let mut reaped_count = 0;

    while let Some(pid) = peek_exited_child() {
        if pid.as_raw() == MANAGED_CHILD.load(Ordering::SeqCst) {
            break;
        }
        match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(pid, status)) => {
                debug!("reaped zombie process {} with exit status {}", pid, status);
                reaped_count += 1;
            }
            Ok(WaitStatus::Signaled(pid, signal, _)) => {
                debug!(
                    "reaped zombie process {} killed by signal {:?}",
                    pid, signal
                );
                reaped_count += 1;
            }
            Ok(WaitStatus::StillAlive) => break,
            Ok(other) => {
                debug!("ignoring wait status {:?}", other);
            }
            Err(nix::Error::ECHILD) => {
                // Reaped concurrently (e.g. by tokio)
                continue;
            }
            Err(e) => {
                warn!("error reaping zombies: {}", e);
                break;
            }
        }
    }

    if reaped_count > 0 {
        debug!("reaped {} zombie processes", reaped_count);
    }
}

/// Shell-style exit code for a child's status: its exit code, or 128 + the
/// signal number if it was killed by a signal
pub fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .or_else(|| status.signal().map(|sig| 128 + sig))
        .unwrap_or(1)
}

/// Handles the child's exit, which ends scinit too.
///
/// In container environments, scinit's lifecycle is tied to the child process,
/// so scinit exits with the child's exit code (see [`exit_code`]) and
/// orchestrators can tell a crash from a clean shutdown.
pub fn handle_child_exit(status: std::process::ExitStatus) -> i32 {
    if status.success() {
        info!("Child process exited successfully, scinit exiting cleanly");
    } else if let Some(code) = status.code() {
        info!("Child process exited with error code {}, scinit exiting", code);
    } else {
        // Extract signal information from status
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            if let Some(signal) = status.signal() {
                info!("Child process terminated by signal {} ({}), scinit exiting",
                      signal, signal_name(signal));
            } else {
                info!("Child process terminated by signal, scinit exiting");
            }
        }
        #[cfg(not(unix))]
        {
            info!("Child process terminated by signal, scinit exiting");
        }
    }

    // Reap any remaining zombies before exiting
    debug!("Reaping any remaining zombie processes before exit");
    spawn_zombie_reap();

    exit_code(status)
}

/// Starts a zombie reap pass on a blocking thread, without waiting for it
pub fn spawn_zombie_reap() {
    tokio::task::spawn_blocking(reap_zombies);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::port_manager::PortBindingConfig;

    fn manager(config: ProcessConfig) -> ProcessManager {
        ProcessManager::new(config, PortManager::new(PortBindingConfig::default()))
    }

    #[tokio::test]
    async fn test_process_manager_creation() {
        let manager = manager(ProcessConfig::default());
        assert_eq!(manager.state(), ProcessState::Stopped);
        assert!(!manager.is_running());
    }

    #[tokio::test]
    async fn test_process_spawn() {
        let mut manager = manager(ProcessConfig {
            command: "echo".to_string(),
            args: vec!["hello".to_string()],
            ..Default::default()
        });
        assert!(manager.spawn_process().await.is_ok());

        // Wait for process to exit
        let exit_status = manager.wait_for_exit().await.unwrap();
        assert!(exit_status.is_some());
        assert_eq!(manager.state(), ProcessState::Stopped);
    }

    #[tokio::test]
    async fn test_process_restart() {
        let mut manager = manager(ProcessConfig {
            command: "echo".to_string(),
            args: vec!["hello".to_string()],
            restart_delay: Duration::from_millis(100),
            ..Default::default()
        });

        assert!(manager.restart().await.is_ok());
        assert!(manager.is_running());
    }

    #[tokio::test]
    async fn test_graceful_shutdown() {
        let mut manager = manager(ProcessConfig {
            command: "sleep".to_string(),
            args: vec!["10".to_string()],
            graceful_shutdown_timeout: Duration::from_millis(500),
            ..Default::default()
        });
        assert!(manager.spawn_process().await.is_ok());
        assert!(manager.is_running());

        manager.graceful_shutdown().await;
        assert_eq!(manager.state(), ProcessState::Stopped);
    }

    #[tokio::test]
    async fn test_process_info() {
        let mut manager = manager(ProcessConfig {
            command: "echo".to_string(),
            args: vec!["hello".to_string()],
            ..Default::default()
        });
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
        let mut env = Environment::new();
        env.set("TEST_VAR", "test_value");

        let mut manager = manager(ProcessConfig {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), "echo $TEST_VAR".to_string()],
            environment: env,
            ..Default::default()
        });
        assert!(manager.spawn_process().await.is_ok());

        let exit_status = manager.wait_for_exit().await.unwrap();
        assert!(exit_status.is_some());
    }
}
