use crate::environment::Environment;
use crate::fds;
use crate::port_manager::PortManager;
use crate::reaper::{clear_managed_child, set_managed_child};
use crate::signals::{is_termination, Signal};
use crate::socket_activation::SocketActivationExec;
use crate::terminal;
use crate::Result;
use eyre::eyre;
use nix::sys::signal::{kill, sigaction, SaFlags, SigAction, SigHandler, SigSet};
use nix::unistd::{getpgid, Pid};
use std::future::Future;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::select;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

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
    Exited { status: ExitStatus },
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
        info!(
            "Spawning process: {} {:?}",
            self.config.command, self.config.args
        );

        // Bind ports before spawning
        self.port_manager.bind_ports()?;

        let overrides = self.child_env_overrides();
        let mut command = self.build_command(&overrides);
        // These hooks must run before the socket-activation exec, and the
        // terminal must be taken before the dispositions are reset
        let tty = terminal::controlling_terminal();
        if let Some(tty) = &tty {
            take_terminal_on_exec(&mut command, tty.as_raw_fd());
        }
        reset_signal_dispositions_on_exec(&mut command);
        reset_signal_mask_on_exec(&mut command);
        close_stray_fds_on_exec(&mut command);
        self.install_socket_activation(&mut command, overrides)?;

        let child = command
            .spawn()
            .map_err(|e| eyre!("Failed to spawn process '{}': {}", self.config.command, e))?;
        // Close-on-exec, so only scinit held it; the child has its own
        // terminal through stdio
        drop(tty);
        self.track_child(child)
    }

    /// Variables the child gets on top of scinit's environment: systemd
    /// socket activation's, then the configured ones
    fn child_env_overrides(&self) -> Environment {
        let mut overrides = self.port_manager.socket_activation_env();
        overrides.extend(self.config.environment.clone());
        overrides
    }

    /// The child's command, in its own process group, with scinit's stdio
    /// and environment plus `overrides`
    fn build_command(&self, overrides: &Environment) -> Command {
        let mut command = Command::new(&self.config.command);
        command.args(&self.config.args);

        // process_group(0) creates a new process group with child as leader
        // This isolates the child from scinit's process group for proper signal handling
        command.process_group(0);
        command.kill_on_drop(true);
        command.stdin(Stdio::inherit());
        command.stdout(Stdio::inherit());
        command.stderr(Stdio::inherit());

        // Inherited as is (non-UTF-8 variables included), plus the overrides
        command.envs(overrides.clone().into_inner());
        remove_inherited_listen_vars(&mut command, overrides);
        command
    }

    /// With sockets to pass, makes the child exec itself so it can move them
    /// to fds 3.. and set LISTEN_PID to its own pid (see socket_activation)
    fn install_socket_activation(
        &self,
        command: &mut Command,
        overrides: Environment,
    ) -> Result<()> {
        let listen_fds = self.port_manager.listen_fds();
        if listen_fds.is_empty() {
            return Ok(());
        }
        let mut exec = SocketActivationExec::new(
            &self.config.command,
            &self.config.args,
            listen_fds,
            overrides,
        )?;
        unsafe {
            command.pre_exec(move || Err(exec.exec_in_child()));
        }
        Ok(())
    }

    fn track_child(&mut self, child: Child) -> Result<()> {
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
        if let ChildState::Running(_) = &self.state {
            self.state = ChildState::Exited { status };
        }
    }

    /// Performs a graceful shutdown of the current process with SIGTERM,
    /// ignoring signals scinit receives meanwhile
    ///
    /// See [`ProcessManager::shutdown_with_signal`].
    pub async fn graceful_shutdown(&mut self) {
        let no_signals = async || std::future::pending::<Result<Signal>>().await;
        self.shutdown_with_signal(Signal::SIGTERM, no_signals).await
    }

    /// Stops the running child: sends `signal` to its process group and
    /// waits for it to exit. If it doesn't exit within the graceful shutdown
    /// timeout, or `next_signal` yields another termination signal first, it
    /// sends SIGKILL. Other signals from `next_signal` are forwarded to the
    /// stopping child.
    pub async fn shutdown_with_signal(
        &mut self,
        signal: Signal,
        mut next_signal: impl AsyncFnMut() -> Result<Signal>,
    ) {
        let Some(pid) = self.running_pid() else {
            return;
        };
        info!(
            "Initiating graceful shutdown of process {} with {:?}",
            pid, signal
        );
        self.try_signal_group(signal);

        let deadline = sleep(self.config.graceful_shutdown_timeout);
        tokio::pin!(deadline);
        let mut signals_open = true;
        loop {
            select! {
                exit = self.wait_for_exit() => match exit {
                    Ok(_) => {
                        info!("Process exited gracefully");
                        return;
                    }
                    Err(e) => {
                        warn!("Error during graceful shutdown: {}", e);
                        break;
                    }
                },
                _ = &mut deadline => {
                    warn!("Graceful shutdown timeout, forcing kill");
                    break;
                }
                next = next_signal(), if signals_open => match next {
                    Ok(next) if is_termination(next) => {
                        warn!("Received {:?} during graceful shutdown, forcing kill", next);
                        break;
                    }
                    Ok(next) => {
                        info!("Forwarding signal {:?} to the stopping process", next);
                        self.try_signal_group(next);
                    }
                    Err(e) => {
                        error!("{}; waiting out the graceful shutdown timeout", e);
                        signals_open = false;
                    }
                },
            }
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
    /// restart delay, then a new spawn. Returns `None` once the new child is
    /// spawned.
    ///
    /// If `cancel` completes first, gives up without spawning and returns
    /// its output. The old child may then still be stopping: it has been
    /// sent SIGTERM, but isn't waited for.
    pub async fn restart_unless<T>(
        &mut self,
        cancel: impl Future<Output = T>,
    ) -> Result<Option<T>> {
        info!("Restarting process due to file change");
        tokio::pin!(cancel);

        select! {
            _ = self.graceful_shutdown() => {}
            output = &mut cancel => return Ok(Some(output)),
        }
        select! {
            _ = sleep(self.config.restart_delay) => {}
            output = &mut cancel => return Ok(Some(output)),
        }
        self.spawn_process().await?;
        Ok(None)
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

    pub fn graceful_shutdown_timeout(&self) -> Duration {
        self.config.graceful_shutdown_timeout
    }

    /// Exit status of the child, once its exit was observed
    pub fn exit_status(&self) -> Option<ExitStatus> {
        match self.state {
            ChildState::Exited { status } => Some(status),
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

/// Drops the `LISTEN_*` variables scinit inherited, unless `overrides` sets
/// them: they describe someone else's sockets, so the child would trust fds
/// it doesn't have
fn remove_inherited_listen_vars(command: &mut Command, overrides: &Environment) {
    for (key, _) in std::env::vars_os() {
        let inherited_listen_var = key.as_bytes().starts_with(b"LISTEN_");
        if inherited_listen_var && !key.to_str().is_some_and(|k| overrides.contains(k)) {
            command.env_remove(key);
        }
    }
}

/// Makes the child take the foreground of terminal `tty` (see
/// [`terminal::take_foreground`])
fn take_terminal_on_exec(command: &mut Command, tty: RawFd) {
    unsafe {
        command.pre_exec(move || terminal::take_foreground(tty));
    }
}

/// Resets every signal's disposition to the default in the child. Signals
/// scinit ignores (SIGTTIN, SIGTTOU, and any it was started with ignored)
/// would otherwise stay ignored across exec; handlers are reset by exec.
fn reset_signal_dispositions_on_exec(command: &mut Command) {
    // Built before the fork: nothing in the hook allocates
    let default_action = SigAction::new(SigHandler::SigDfl, SaFlags::empty(), SigSet::empty());
    unsafe {
        command.pre_exec(move || {
            for signal in Signal::iterator() {
                if !matches!(signal, Signal::SIGKILL | Signal::SIGSTOP) {
                    // sigaction is async-signal-safe
                    let _ = sigaction(signal, &default_action);
                }
            }
            Ok(())
        });
    }
}

/// Unblocks all signals in the child: it inherits scinit's mask, but must
/// handle signals normally (e.g. Ctrl+C)
fn reset_signal_mask_on_exec(command: &mut Command) {
    unsafe {
        command.pre_exec(|| {
            use nix::sys::signal::{pthread_sigmask, SigSet, SigmaskHow};

            pthread_sigmask(SigmaskHow::SIG_SETMASK, Some(&SigSet::empty()), None)
                .map_err(|e| std::io::Error::from_raw_os_error(e as i32))
        });
    }
}

/// Keeps fds scinit inherited (other than stdio) from reaching the child.
/// The activated sockets are moved to fds 3.. after this, which clears the
/// flag on them.
fn close_stray_fds_on_exec(command: &mut Command) {
    let limit = fds::open_fd_limit();
    unsafe {
        command.pre_exec(move || {
            fds::mark_non_stdio_cloexec(limit);
            Ok(())
        });
    }
}

impl Drop for ProcessManager {
    fn drop(&mut self) {
        // Emergency cleanup when dropped with a child still running (e.g. on
        // an error return or mid-shutdown), so its process group isn't orphaned
        let Some(pid) = self.running_pid() else {
            return;
        };
        warn!(
            "ProcessManager dropped with running child (PID: {}), emergency cleanup",
            pid
        );

        // Emergency SIGKILL to process group - no graceful shutdown in Drop
        match self.signal_group(Signal::SIGKILL) {
            Ok(()) => warn!(
                "Sent SIGKILL to process group {} during emergency cleanup",
                pid
            ),
            // Process already dead - this is fine, no cleanup needed
            Err(e) if matches!(e.downcast_ref::<nix::Error>(), Some(nix::Error::ESRCH)) => {}
            Err(e) => error!(
                "Failed to send SIGKILL to process group during emergency cleanup: {}",
                e
            ),
        }

        // Brief pause to let SIGKILL take effect
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exit_status::exit_code;
    use crate::port_manager::PortBindingConfig;
    use tokio::time::timeout;

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
        std::fs::read_to_string(format!("/proc/{}/stat", pid)).is_ok_and(|stat| {
            stat.rsplit(')')
                .next()
                .is_some_and(|s| s.trim_start().starts_with('Z'))
        })
    }

    /// Polls `f` every 10ms for up to 2s
    fn poll(mut f: impl FnMut() -> bool) -> bool {
        (0..200).any(|_| {
            f() || {
                std::thread::sleep(Duration::from_millis(10));
                false
            }
        })
    }

    #[tokio::test]
    async fn test_process_manager_creation() {
        let manager = manager(ProcessConfig::default());
        assert!(!manager.is_running());
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

        let cancelled = manager.restart_unless(std::future::pending::<()>()).await;
        assert!(cancelled.unwrap().is_none());
        assert!(manager.is_running());
    }

    #[tokio::test]
    async fn test_cancelled_restart_spawns_nothing() {
        let mut manager = manager(ProcessConfig {
            restart_delay: Duration::from_secs(10),
            ..command("sleep", &["10"])
        });
        manager.spawn_process().await.unwrap();

        // Cancelled during the restart delay, after the old child exited
        let cancel = sleep(Duration::from_millis(300));
        let cancelled = timeout(Duration::from_secs(5), manager.restart_unless(cancel)).await;
        assert!(cancelled.unwrap().unwrap().is_some());
        assert!(!manager.is_running());
        assert_eq!(
            manager.exit_status().map(exit_code),
            Some(128 + Signal::SIGTERM as i32)
        );
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

        manager.graceful_shutdown().await;
        assert!(!manager.is_running());
        assert_eq!(
            manager.exit_status().map(exit_code),
            Some(128 + Signal::SIGTERM as i32)
        );
        // The exited child's pid may be reused, so it is never signalled again
        assert!(manager.signal_group(Signal::SIGTERM).is_err());
        // And there is nothing left to wait for
        assert!(timeout(Duration::from_millis(50), manager.wait_for_exit())
            .await
            .is_err());
    }

    /// Spawns `sh -c script` and waits until the script has touched
    /// `$READY`, so its traps are in place before it is signalled
    async fn spawn_ready(script: &str) -> (ProcessManager, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let ready = dir.path().join("ready");
        let mut env = Environment::new();
        env.set("READY", ready.to_str().unwrap());
        let mut manager = manager(ProcessConfig {
            environment: env,
            graceful_shutdown_timeout: Duration::from_secs(30),
            ..command("sh", &["-c", script])
        });
        manager.spawn_process().await.unwrap();
        assert!(poll(|| ready.exists()), "the script never became ready");
        (manager, dir)
    }

    /// A `next_signal` that yields `signal` once, after `delay`, then nothing
    fn signal_once(signal: Signal, delay: Duration) -> impl AsyncFnMut() -> Result<Signal> {
        let mut sent = false;
        async move || {
            if sent {
                return std::future::pending().await;
            }
            sent = true;
            sleep(delay).await;
            Ok(signal)
        }
    }

    #[tokio::test]
    async fn test_second_termination_signal_kills() {
        let (mut manager, _dir) =
            spawn_ready("trap '' TERM INT; touch \"$READY\"; sleep 10 & wait; wait").await;

        let next = signal_once(Signal::SIGINT, Duration::from_millis(200));
        let stop = manager.shutdown_with_signal(Signal::SIGTERM, next);
        assert!(timeout(Duration::from_secs(5), stop).await.is_ok());
        assert_eq!(
            manager.exit_status().map(exit_code),
            Some(128 + Signal::SIGKILL as i32)
        );
    }

    #[tokio::test]
    async fn test_other_signals_reach_stopping_child() {
        let (mut manager, _dir) =
            spawn_ready("trap '' TERM; trap 'exit 7' USR1; touch \"$READY\"; sleep 10 & wait")
                .await;

        let next = signal_once(Signal::SIGUSR1, Duration::from_millis(200));
        let stop = manager.shutdown_with_signal(Signal::SIGTERM, next);
        assert!(timeout(Duration::from_secs(5), stop).await.is_ok());
        assert_eq!(manager.exit_status().map(exit_code), Some(7));
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
            ..command(
                "sh",
                &[
                    "-c",
                    "trap '' TERM; sleep 10 & echo $! > \"$PID_FILE\"; wait",
                ],
            )
        });
        manager.spawn_process().await.unwrap();
        let mut grandchild = None;
        assert!(poll(|| {
            grandchild = std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|s| s.trim().parse().ok());
            grandchild.is_some()
        }));
        let grandchild = Pid::from_raw(grandchild.unwrap());

        let stop = manager.graceful_shutdown();
        assert!(timeout(Duration::from_millis(200), stop).await.is_err());
        assert!(manager.is_running());

        drop(manager);
        assert!(
            poll(|| is_gone(grandchild)),
            "grandchild {} survived the drop",
            grandchild
        );
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
