use crate::environment::Environment;
use crate::port_manager::PortManager;
use crate::signals::signal_name;
use crate::socket_activation::SocketActivationExec;
use crate::Result;
use eyre::eyre;
use nix::sys::signal::kill;
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{getpgid, tcsetpgrp, Pid};
use std::collections::HashMap;
use std::fs::File;
use std::io::IsTerminal;
use std::process::{ExitStatus, Stdio};
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

/// Marks `pid` as the managed child
fn set_managed_child(pid: Pid) {
    MANAGED_CHILD.store(pid.as_raw(), Ordering::SeqCst);
}

/// Clears the managed child mark, unless another child has replaced `pid`
fn clear_managed_child(pid: Pid) {
    let _ = MANAGED_CHILD.compare_exchange(pid.as_raw(), 0, Ordering::SeqCst, Ordering::SeqCst);
}

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

/// Lifecycle of the managed child; each state carries only the data valid in it
enum ChildState {
    NotStarted,
    Running(ManagedChild),
    Exited { pid: Pid, status: ExitStatus },
}

/// A running child the zombie reaper must leave alone. Creating one marks
/// its pid as managed and dropping it clears the mark, so the reaper's
/// view can't go stale.
struct ManagedChild {
    child: Child,
    pid: Pid,
}

impl ManagedChild {
    fn new(child: Child) -> Result<Self> {
        let pid = match child.id() {
            Some(pid) => Pid::from_raw(pid.try_into()?),
            None => return Err(eyre!("Failed to get process ID")),
        };
        set_managed_child(pid);
        Ok(Self { child, pid })
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        clear_managed_child(self.pid);
    }
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
    state: ChildState,
}

impl ProcessManager {
    pub fn new(config: ProcessConfig, port_manager: PortManager) -> Self {
        Self {
            config,
            port_manager,
            state: ChildState::NotStarted,
        }
    }

    /// Spawns a new process with port inheritance and proper signal mask reset
    pub async fn spawn_process(&mut self) -> Result<()> {
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
        let child = ManagedChild::new(child)?;
        let pid = child.pid;
        self.state = ChildState::Running(child);

        info!("Process spawned with PID: {}", pid);
        Ok(())
    }

    /// Waits for the running child to exit and returns its status. Never
    /// resolves when no child is running.
    pub async fn wait_for_exit(&mut self) -> Result<ExitStatus> {
        let ChildState::Running(running) = &mut self.state else {
            return std::future::pending().await;
        };
        match running.child.wait().await {
            Ok(status) => {
                self.record_exit(status);
                debug!("Process exited with status: {:?}", status);
                Ok(status)
            }
            Err(e) => {
                error!("Error waiting for process: {}", e);
                // Its status is lost, so there is no child left to manage
                self.state = ChildState::NotStarted;
                Err(e.into())
            }
        }
    }

    /// Moves a running child to `Exited`, which clears the reaper's mark
    fn record_exit(&mut self, status: ExitStatus) {
        if let ChildState::Running(child) = &self.state {
            self.state = ChildState::Exited {
                pid: child.pid,
                status,
            };
        }
    }

    /// Performs a graceful shutdown of the current process with SIGTERM
    ///
    /// See [`ProcessManager::shutdown_with_signal`].
    pub async fn graceful_shutdown(&mut self) {
        self.shutdown_with_signal(Signal::SIGTERM).await
    }

    /// Stops the running child: sends `signal` to its process group and
    /// waits for it to exit. If it doesn't exit within the graceful shutdown
    /// timeout, it sends SIGKILL.
    pub async fn shutdown_with_signal(&mut self, signal: Signal) {
        let Some(pid) = self.running_pid() else {
            return;
        };
        info!("Initiating graceful shutdown of process {} with {:?}", pid, signal);
        self.try_signal_group(signal);

        match timeout(self.config.graceful_shutdown_timeout, self.wait_for_exit()).await {
            Ok(Ok(_)) => {
                info!("Process exited gracefully");
                return;
            }
            Ok(Err(e)) => warn!("Error during graceful shutdown: {}", e),
            Err(_) => warn!("Graceful shutdown timeout, forcing kill"),
        }
        self.force_kill().await;
    }

    /// Sends SIGKILL to the running child's process group and records the
    /// exit status if the child is gone 100ms later
    async fn force_kill(&mut self) {
        let Some(pid) = self.running_pid() else {
            return;
        };
        info!("Force killing process {}", pid);
        self.try_signal_group(Signal::SIGKILL);

        // Wait a bit for the process to exit
        sleep(Duration::from_millis(100)).await;

        let ChildState::Running(running) = &mut self.state else {
            return;
        };
        if let Ok(Some(status)) = running.child.try_wait() {
            self.record_exit(status);
            info!("Process killed, exit status: {:?}", status);
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

    /// Sends a signal to the running child's process group
    pub fn signal_group(&self, signal: Signal) -> Result<()> {
        let Some(pid) = self.running_pid() else {
            return Err(eyre!("No process to send signal to"));
        };
        let pgid = getpgid(Some(pid))?;
        debug!("Sending signal {:?} to process group {}", signal, pgid);
        kill(Pid::from_raw(-pgid.as_raw()), signal)?;
        Ok(())
    }

    /// Like [`Self::signal_group`], but only logs a failure
    pub fn try_signal_group(&self, signal: Signal) {
        if let Err(e) = self.signal_group(signal) {
            warn!("failed to forward signal {:?} to child: {}", signal, e);
        }
    }

    /// PID of the running or last exited child
    pub fn pid(&self) -> Option<Pid> {
        match &self.state {
            ChildState::NotStarted => None,
            ChildState::Running(child) => Some(child.pid),
            ChildState::Exited { pid, .. } => Some(*pid),
        }
    }

    /// Exit status of the child, once its exit was observed
    pub fn exit_status(&self) -> Option<ExitStatus> {
        match self.state {
            ChildState::Exited { status, .. } => Some(status),
            _ => None,
        }
    }

    fn running_pid(&self) -> Option<Pid> {
        match &self.state {
            ChildState::Running(child) => Some(child.pid),
            _ => None,
        }
    }

    #[cfg(test)]
    pub fn is_running(&self) -> bool {
        matches!(self.state, ChildState::Running(_))
    }
}

impl Drop for ProcessManager {
    fn drop(&mut self) {
        // Emergency cleanup when dropped with a child still running (e.g. on
        // an error return or mid-shutdown), so its process group isn't orphaned
        let Some(pid) = self.running_pid() else {
            return;
        };
        eprintln!("ProcessManager dropped with running child (PID: {}), emergency cleanup", pid);

        // Emergency SIGKILL to process group - no graceful shutdown in Drop
        match self.signal_group(Signal::SIGKILL) {
            Ok(()) => eprintln!("Sent SIGKILL to process group {} during emergency cleanup", pid),
            // Process already dead - this is fine, no cleanup needed
            Err(e) if matches!(e.downcast_ref::<nix::Error>(), Some(nix::Error::ESRCH)) => {}
            Err(e) => eprintln!("Failed to send SIGKILL to process group during emergency cleanup: {}", e),
        }

        // Brief pause to let SIGKILL take effect
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Sets the process group as the foreground process group if a terminal is available
pub fn process_group_to_foreground(pgid: Pid) -> Result<()> {
    let tty = match File::open("/dev/tty") {
        Ok(tty) => tty,
        Err(e) => {
            debug!(
                "Cannot open /dev/tty ({}), skipping foreground process group setup",
                e
            );
            return Ok(());
        }
    };
    if !tty.is_terminal() {
        debug!("Not a terminal, skipping foreground process group setup");
        return Ok(());
    }
    debug!("Setting process group {} as foreground", &pgid);
    if let Err(e) = tcsetpgrp(tty, pgid) {
        error!("Failed to set process group {} as foreground: {}", &pgid, e);
        return Err(e.into());
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
pub fn exit_code(status: ExitStatus) -> i32 {
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
pub fn handle_child_exit(status: ExitStatus) -> i32 {
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

    fn command(command: &str, args: &[&str]) -> ProcessConfig {
        ProcessConfig {
            command: command.to_string(),
            args: args.iter().map(|a| a.to_string()).collect(),
            ..Default::default()
        }
    }

    /// Whether `pid` has exited (zombies count: an orphan may never be reaped
    /// when the tests run as a container's PID 1)
    fn is_gone(pid: Pid) -> bool {
        if kill(pid, None).is_err() {
            return true;
        }
        std::fs::read_to_string(format!("/proc/{}/stat", pid))
            .is_ok_and(|stat| stat.rsplit(')').next().is_some_and(|s| s.trim_start().starts_with('Z')))
    }

    /// Polls `f` every 10ms for up to 2s
    fn poll(mut f: impl FnMut() -> bool) -> bool {
        (0..200).any(|_| f() || {
            std::thread::sleep(Duration::from_millis(10));
            false
        })
    }

    #[tokio::test]
    async fn test_process_manager_creation() {
        let manager = manager(ProcessConfig::default());
        assert!(!manager.is_running());
        assert!(manager.pid().is_none());
        assert!(manager.exit_status().is_none());
    }

    #[tokio::test]
    async fn test_process_spawn() {
        let mut manager = manager(command("echo", &["hello"]));
        assert!(manager.spawn_process().await.is_ok());

        // Wait for process to exit
        let status = manager.wait_for_exit().await.unwrap();
        assert!(status.success());
        assert!(!manager.is_running());
    }

    #[tokio::test]
    async fn test_process_restart() {
        let mut manager = manager(ProcessConfig {
            restart_delay: Duration::from_millis(100),
            ..command("echo", &["hello"])
        });

        assert!(manager.restart().await.is_ok());
        assert!(manager.is_running());
    }

    #[tokio::test]
    async fn test_graceful_shutdown() {
        let mut manager = manager(ProcessConfig {
            graceful_shutdown_timeout: Duration::from_millis(500),
            ..command("sleep", &["10"])
        });
        assert!(manager.spawn_process().await.is_ok());
        assert!(manager.is_running());

        manager.graceful_shutdown().await;
        assert!(!manager.is_running());
    }

    #[tokio::test]
    async fn test_shutdown_records_exit() {
        let mut manager = manager(ProcessConfig {
            graceful_shutdown_timeout: Duration::from_secs(5),
            ..command("sleep", &["10"])
        });
        manager.spawn_process().await.unwrap();
        let pid = manager.pid().unwrap();

        manager.shutdown_with_signal(Signal::SIGTERM).await;
        assert!(!manager.is_running());
        assert_eq!(manager.pid(), Some(pid));
        assert_eq!(manager.exit_status().map(exit_code), Some(128 + Signal::SIGTERM as i32));
        // The exited child's pid may be reused, so it is never signalled again
        assert!(manager.signal_group(Signal::SIGTERM).is_err());
        // And there is nothing left to wait for
        assert!(timeout(Duration::from_millis(50), manager.wait_for_exit()).await.is_err());
    }

    #[tokio::test]
    async fn test_drop_kills_group_during_graceful_stop() {
        // The child and its own child (which only the group kill reaches,
        // kill_on_drop kills the leader alone) ignore SIGTERM, so the
        // graceful stop below can't finish
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("grandchild.pid");
        let mut env = Environment::new();
        env.set("PID_FILE", pid_file.to_str().unwrap());
        let mut manager = manager(ProcessConfig {
            environment: env,
            ..command("sh", &["-c", "trap '' TERM; sleep 10 & echo $! > \"$PID_FILE\"; wait"])
        });
        manager.spawn_process().await.unwrap();
        let mut grandchild = None;
        assert!(poll(|| {
            grandchild = std::fs::read_to_string(&pid_file).ok().and_then(|s| s.trim().parse().ok());
            grandchild.is_some()
        }));
        let grandchild = Pid::from_raw(grandchild.unwrap());

        let stop = manager.shutdown_with_signal(Signal::SIGTERM);
        assert!(timeout(Duration::from_millis(200), stop).await.is_err());
        assert!(manager.is_running());

        drop(manager);
        assert!(poll(|| is_gone(grandchild)), "grandchild {} survived the drop", grandchild);
    }

    #[tokio::test]
    async fn test_environment_variables() {
        let mut env = Environment::new();
        env.set("TEST_VAR", "test_value");

        let mut manager = manager(ProcessConfig {
            environment: env,
            ..command("sh", &["-c", "test \"$TEST_VAR\" = test_value"])
        });
        assert!(manager.spawn_process().await.is_ok());

        let status = manager.wait_for_exit().await.unwrap();
        assert!(status.success());
    }
}
