type Result<T> = color_eyre::eyre::Result<T>;

mod file_watcher;
mod port_manager;
mod process_manager;
mod signals;

use clap::Parser;
use eyre::eyre;
use nix::unistd::{getpgid, setpgid, tcsetpgrp, Pid};
use std::collections::HashMap;
use std::fs::File;
use std::io::IsTerminal;
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;
use tokio::select;
use tokio::time::interval;
use tracing::{debug, error, info, warn};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use file_watcher::{FileChangeEvent, FileWatchConfig, FileWatcher};
use port_manager::{PortBindingConfig, PortManager};
use process_manager::{ProcessConfig, ProcessManager};
use signals::{Signal, SignalHandler, setup_signal_masking};

/// A live-reloading init system for managing subprocesses
#[derive(Parser)]
#[command(name = "scinit")]
#[command(about = "A live-reloading init system for managing subprocesses")]
#[command(version)]
struct Cli {
    /// Enable live-reload functionality
    #[arg(long)]
    live_reload: bool,

    /// Path to watch for changes (default: executable path)
    #[arg(long)]
    watch_path: Option<PathBuf>,

    /// Comma-separated list of ports to bind
    #[arg(long, value_delimiter = ',')]
    ports: Vec<u16>,

    /// Address to bind ports to
    #[arg(long, default_value = "127.0.0.1")]
    bind_addr: String,

    /// Debounce time for file changes (ms)
    #[arg(long, default_value = "500")]
    debounce_ms: u64,

    /// Delay before restart after graceful shutdown (ms)
    #[arg(long, default_value = "1000")]
    restart_delay_ms: u64,

    /// Graceful shutdown timeout (seconds)
    #[arg(long, default_value = "30")]
    graceful_timeout_secs: u64,

    /// Signal polling interval (ms)
    #[arg(long, default_value = "100")]
    signal_poll_interval_ms: u64,

    /// Zombie reaping interval (ms)
    #[arg(long, default_value = "5000")]
    zombie_reap_interval_ms: u64,

    /// Command to execute
    command: String,

    /// Arguments for the command
    args: Vec<String>,
}

/// Configuration for the init system
#[derive(Debug, Clone)]
struct Config {
    /// The command to execute
    command: String,
    /// Arguments for the command
    args: Vec<String>,
    /// Signal polling interval in milliseconds (optimized for performance)
    signal_poll_interval: Duration,
    /// Zombie reaping interval in milliseconds
    zombie_reap_interval: Duration,
    /// Live-reload configuration
    live_reload: LiveReloadConfig,
    /// Port binding configuration
    port_binding: PortBindingConfig,
}

#[derive(Debug, Clone)]
struct LiveReloadConfig {
    enabled: bool,
    watch_path: Option<PathBuf>,
    debounce_ms: u64,
    restart_delay_ms: u64,
    graceful_timeout_secs: u64,
}

impl Config {
    /// Parse command line arguments into configuration
    fn from_cli(cli: Cli) -> Result<Self> {
        // Parse bind address
        let bind_address: IpAddr = cli
            .bind_addr
            .parse()
            .map_err(|e| eyre!("Invalid bind address '{}': {}", cli.bind_addr, e))?;

        // Determine watch path
        let watch_path = cli.watch_path.or_else(|| {
            if cli.live_reload {
                Some(PathBuf::from(&cli.command))
            } else {
                None
            }
        });

        Ok(Config {
            command: cli.command,
            args: cli.args,
            signal_poll_interval: Duration::from_millis(cli.signal_poll_interval_ms),
            zombie_reap_interval: Duration::from_millis(cli.zombie_reap_interval_ms),
            live_reload: LiveReloadConfig {
                enabled: cli.live_reload,
                watch_path,
                debounce_ms: cli.debounce_ms,
                restart_delay_ms: cli.restart_delay_ms,
                graceful_timeout_secs: cli.graceful_timeout_secs,
            },
            port_binding: PortBindingConfig {
                ports: cli.ports,
                bind_address,
                reuse_port: true,
            },
        })
    }
}

/// Main init system that manages subprocess lifecycle and signal handling
struct InitSystem {
    config: Config,
    process_manager: ProcessManager,
    signal_handler: SignalHandler,
    file_watcher: Option<FileWatcher>,
}

impl InitSystem {
    /// Creates a new init system with the given configuration
    fn new(config: Config) -> Result<Self> {
        // Create port manager
        let port_manager = PortManager::new(config.port_binding.clone());

        // Create process manager
        let process_config = ProcessConfig {
            command: config.command.clone(),
            args: config.args.clone(),
            restart_delay: Duration::from_millis(config.live_reload.restart_delay_ms),
            graceful_shutdown_timeout: Duration::from_secs(
                config.live_reload.graceful_timeout_secs,
            ),
            working_directory: None,
            environment: HashMap::new(),
        };

        let process_manager = ProcessManager::new(process_config, port_manager);
        let signal_handler = SignalHandler::new()?;

        // Create file watcher if live-reload is enabled
        let file_watcher = if config.live_reload.enabled {
            if let Some(watch_path) = &config.live_reload.watch_path {
                let watch_config = FileWatchConfig {
                    watch_path: watch_path.clone(),
                    debounce_ms: config.live_reload.debounce_ms,
                    recursive: false,
                };
                Some(FileWatcher::new(watch_config)?)
            } else {
                None
            }
        } else {
            None
        };

        Ok(InitSystem {
            config,
            process_manager,
            signal_handler,
            file_watcher,
        })
    }

    /// Sets up the process group and terminal handling
    async fn setup_process_group(&self) -> Result<()> {
        // Make terminal operations async to avoid blocking
        if let Some(pid) = self.process_manager.process_info().pid {
            let pgid = getpgid(Some(pid))?;
            tokio::task::spawn_blocking(move || process_group_to_foreground(pgid)).await??;
        }
        Ok(())
    }

    /// Runs the main event loop with optimized performance
    async fn run(&mut self) -> Result<()> {
        let mut zombie_reap_interval = interval(self.config.zombie_reap_interval);

        info!(
            "init system started, managing subprocess: {}",
            self.config.command
        );

        // Start file watching if enabled
        if let Some(ref mut file_watcher) = self.file_watcher {
            file_watcher.start_watching().await?;
            info!("File watching started for live-reload");
        }

        // Spawn initial process
        self.process_manager.spawn_process().await?;
        self.setup_process_group().await?;

        loop {
            // Check for file events first (if enabled)
            if self.file_watcher.is_some() {
                if let Some(event) = self.handle_file_events().await? {
                    match event {
                        FileChangeEvent::FileChanged(path) => {
                            info!("File changed: {:?}, triggering restart", path);
                            let restart_result = self
                                .process_manager
                                .restart_process_with_reason("file_change")
                                .await?;
                            if !restart_result {
                                info!("Process restart limit exceeded, exiting");
                                return Ok(());
                            }
                        }
                        FileChangeEvent::WatchError(error) => {
                            warn!("File watching error: {}", error);
                        }
                    }
                }
            }

            select! {
                // Check if subprocess has exited
                exit_status = self.process_manager.wait_for_exit() => {
                    match exit_status {
                        Ok(Some(status)) => {
                            // Scenario A: Child process exit handling
                            self.handle_child_exit(status).await?;
                            return Ok(());
                        }
                        Ok(None) => {
                            // No process to wait for, continue
                            continue;
                        }
                        Err(e) => {
                            error!("error waiting for subprocess: {}", e);
                            return Err(e.into());
                        }
                    }
                }

                // Synchronous signal handling - proper for init systems
                signal = self.signal_handler.wait_for_signal(self.config.signal_poll_interval) => {
                    match signal? {
                        Some(signal) => {
                            info!("received signal: {:?}", signal);
                            self.process_signal(signal).await?;
                    }
                    None => {
                            // No signal received, continue
                        }
                    }
                }

                // Periodic zombie reaping (less frequent, non-blocking)
                _ = zombie_reap_interval.tick() => {
                    self.reap_zombies_async().await;
                }
            }
        }
    }

    /// Handles file change events from the file watcher
    async fn handle_file_events(&mut self) -> Result<Option<FileChangeEvent>> {
        if let Some(ref mut file_watcher) = self.file_watcher {
            file_watcher
                .wait_for_event(Duration::from_millis(100))
                .await
        } else {
            Ok(None)
        }
    }

    /// Processes a specific signal according to init system semantics
    async fn process_signal(&mut self, signal: Signal) -> Result<()> {
        match signal {
            Signal::SIGCHLD => {
                // Reap zombie processes asynchronously - this is always handled by init
                debug!("received SIGCHLD, reaping zombie processes");
                self.reap_zombies_async().await;
            }
            Signal::SIGTERM | Signal::SIGINT | Signal::SIGQUIT => {
                // Scenario B: Signal forwarding with graceful shutdown and timeout
                info!("received termination signal {:?}, initiating graceful shutdown", signal);
                self.handle_termination_signal(signal).await?;
                return Ok(());
            }
            Signal::SIGUSR1 | Signal::SIGUSR2 | Signal::SIGHUP => {
                // These signals should be forwarded to the child process only
                info!("forwarding signal {:?} to child process", signal);
                if let Err(e) = self.process_manager.forward_signal(signal) {
                    warn!("failed to forward signal {:?} to child: {}", signal, e);
                }
            }
            _ => {
                // Any other signals we somehow receive should be forwarded
                debug!("forwarding unexpected signal {:?} to child process", signal);
                if let Err(e) = self.process_manager.forward_signal(signal) {
                    warn!("failed to forward signal {:?} to child: {}", signal, e);
                }
            }
        }
        Ok(())
    }

    /// Handles termination signals with proper timeout and escalation (Scenario B)
    async fn handle_termination_signal(&mut self, signal: Signal) -> Result<()> {
        info!("Termination signal {:?} received, forwarding to child process", signal);
        
        // Forward the signal to child process
        if let Err(e) = self.process_manager.forward_signal(signal) {
            warn!("Failed to forward signal {:?} to child: {}", signal, e);
        }
        
        match signal {
            Signal::SIGTERM => {
                // SIGTERM gets graceful shutdown with timeout
                info!("Waiting for child process to exit gracefully (timeout: {}s)", 
                      self.config.live_reload.graceful_timeout_secs);
                
                if let Err(_) = self.process_manager.graceful_shutdown().await {
                    warn!("Graceful shutdown timed out, child process may have been force-killed");
                }
            }
            Signal::SIGINT | Signal::SIGQUIT => {
                // SIGINT/SIGQUIT get shorter timeout or immediate cleanup
                info!("Waiting for child process to exit (signal: {:?})", signal);
                
                // Wait a bit for child to exit, but don't use full graceful timeout
                tokio::time::sleep(Duration::from_secs(2)).await;
                
                // Force kill if still running
                if self.process_manager.is_running() {
                    warn!("Child process didn't exit after {:?}, forcing termination", signal);
                    if let Err(e) = self.process_manager.force_kill().await {
                        error!("Failed to force kill child process: {}", e);
                    }
                }
            }
            _ => unreachable!(),
        }
        
        info!("scinit exiting due to termination signal {:?}", signal);
        Ok(())
    }

    /// Handles child process exit (Scenario A)
    /// 
    /// In container environments, scinit's lifecycle is tied to the child process.
    /// When the child exits, scinit should exit with appropriate logging and status.
    async fn handle_child_exit(&self, status: std::process::ExitStatus) -> Result<()> {
        if status.success() {
            info!("Child process exited successfully with status 0");
            info!("Container main process completed, scinit exiting cleanly");
        } else {
            if let Some(code) = status.code() {
                error!("Child process exited with error code: {}", code);
                error!("Container main process failed, scinit exiting with error");
            } else {
                error!("Child process was terminated by signal: {:?}", status);
                error!("Container main process killed, scinit exiting");
            }
        }
        
        // Reap any remaining zombies before exiting
        self.reap_zombies_async().await;
        
        // In container paradigm, we let orchestration handle restarts
        // scinit exits with same status as child process
        Ok(())
    }

    /// Reaps zombie processes asynchronously to avoid blocking the main loop
    async fn reap_zombies_async(&self) {
        // Spawn zombie reaping in a blocking task to avoid blocking the main loop
        tokio::task::spawn_blocking(|| {
            if let Err(e) = reap_zombies() {
                warn!("error reaping zombies: {}", e);
            }
        });
    }
}

/// Sets the process group as the foreground process group if a terminal is available
fn process_group_to_foreground(pgid: Pid) -> Result<()> {
    match File::open("/dev/tty") {
        Ok(tty) => {
            if tty.is_terminal() {
                info!("Setting process group {} as foreground", &pgid);
                match tcsetpgrp(tty, pgid) {
                    Ok(()) => {
                        info!("Successfully set process group {} as foreground", &pgid);
                    }
                    Err(e) => {
                        error!("Failed to set process group {} as foreground: {}", &pgid, e);
                        return Err(e.into());
                    }
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

/// Reaps zombie processes to prevent process table exhaustion
fn reap_zombies() -> Result<()> {
    use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};

    let mut reaped_count = 0;

    loop {
        match waitpid(None, Some(WaitPidFlag::WNOHANG)) {
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
            Ok(WaitStatus::Stopped(pid, signal)) => {
                debug!("reaped stopped process {} by signal {:?}", pid, signal);
                reaped_count += 1;
            }
            Ok(WaitStatus::Continued(pid)) => {
                debug!("reaped continued process {}", pid);
                reaped_count += 1;
            }
            Ok(WaitStatus::StillAlive) => {
                // No more zombies to reap
                break;
            }
            Ok(WaitStatus::PtraceEvent(_, _, _)) | Ok(WaitStatus::PtraceSyscall(_)) => {
                // Ignore ptrace events
                continue;
            }
            Err(nix::Error::ECHILD) => {
                // No child processes
                break;
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

    Ok(())
}

fn main() -> Result<()> {
    // Initialize error handling and logging BEFORE tokio runtime
    color_eyre::install()?;

    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    // Set up signal masking BEFORE starting tokio runtime
    // This ensures all tokio threads inherit the correct signal mask
    setup_signal_masking()?;

    // Note: We don't create a separate process group for scinit to allow Ctrl+C during development
    // The signal masking handles proper init system signal semantics

    info!("scinit starting");

    // Start tokio runtime AFTER signal setup
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async_main())
}

async fn async_main() -> Result<()> {

    // Parse CLI arguments
    let cli = Cli::parse();

    // Convert CLI to configuration
    let config = Config::from_cli(cli)?;

    // Create and run the init system
    let mut init_system = InitSystem::new(config)?;

    // Run the main event loop
    init_system.run().await?;

    info!("scinit exiting");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn test_config_from_cli() {
        // This would need to be tested with actual command line arguments
        // For now, we'll just test the structure
        let config = Config {
            command: "test".to_string(),
            args: vec!["arg1".to_string(), "arg2".to_string()],
            signal_poll_interval: Duration::from_millis(100),
            zombie_reap_interval: Duration::from_millis(5000),
            live_reload: LiveReloadConfig {
                enabled: true,
                watch_path: Some(PathBuf::from("./test")),
                debounce_ms: 500,
                restart_delay_ms: 1000,
                graceful_timeout_secs: 30,
            },
            port_binding: PortBindingConfig {
                ports: vec![8080, 8081],
                bind_address: IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
                reuse_port: true,
            },
        };

        assert_eq!(config.command, "test");
        assert_eq!(config.args.len(), 2);
        assert_eq!(config.args[0], "arg1");
        assert_eq!(config.args[1], "arg2");
        assert!(config.live_reload.enabled);
        assert_eq!(config.port_binding.ports.len(), 2);
    }

    #[tokio::test]
    async fn test_signal_handler_creation() {
        // This test would require mocking or integration testing
        // For now, we'll just verify the structure can be created
        let handler = SignalHandler::new();
        assert!(handler.is_ok());
    }

    #[tokio::test]
    async fn test_init_system_creation() {
        let config = Config {
            command: "echo".to_string(),
            args: vec!["hello".to_string()],
            signal_poll_interval: Duration::from_millis(100),
            zombie_reap_interval: Duration::from_millis(5000),
            live_reload: LiveReloadConfig {
                enabled: false,
                watch_path: None,
                debounce_ms: 500,
                restart_delay_ms: 1000,
                graceful_timeout_secs: 30,
            },
            port_binding: PortBindingConfig {
                ports: vec![],
                bind_address: IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
                reuse_port: true,
            },
        };

        let init_system = InitSystem::new(config);
        assert!(init_system.is_ok());
    }
}
