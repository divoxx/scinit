type Result<T> = color_eyre::eyre::Result<T>;

mod cli;
mod environment;
mod file_watcher;
mod port_manager;
mod process_manager;
mod signals;
mod socket_activation;

use clap::Parser;
use environment::Environment;
use std::time::Duration;
use tokio::select;
use tokio::signal::unix::{signal, SignalKind};
use tokio::time::interval;
use tracing::{debug, error, info};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use cli::{Cli, Config};
use file_watcher::{handle_file_events, FileWatcher};
use port_manager::PortManager;
use process_manager::{
    exit_code, handle_child_exit, process_group_to_foreground, reap_zombies_async,
    ProcessConfig, ProcessManager,
};
use signals::{SignalAction, SignalHandler};

fn main() -> Result<()> {
    // Initialize error handling and logging
    color_eyre::install()?;

    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    info!("scinit starting");

    // Before any other thread exists, so every thread inherits the mask
    let mut signal_handler = SignalHandler::install()?;
    debug!("blocked signals {:?} on all threads", signal_handler.handled_signals());

    debug!("starting tokio runtime");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let result = rt.block_on(app_main(&mut signal_handler));
    // Shut down with a timeout on every path: blocking tasks parked in
    // sigwait never finish, so dropping the runtime would block forever
    rt.shutdown_timeout(Duration::from_millis(100));

    // Mirror the child's exit status, like other container inits do
    std::process::exit(result?)
}

/// Returns the exit code scinit should exit with
async fn app_main(signal_handler: &mut SignalHandler) -> Result<i32> {
    // Parse CLI arguments
    let cli = Cli::parse();

    // Convert CLI to configuration
    let config = Config::from_cli(cli)?;

    // Setup components
    let port_manager = PortManager::new(config.port_binding.clone());

    let process_config = ProcessConfig {
        command: config.command.clone(),
        args: config.args.clone(),
        restart_delay: Duration::from_millis(config.live_reload.restart_delay_ms),
        graceful_shutdown_timeout: Duration::from_secs(config.live_reload.graceful_timeout_secs),
        working_directory: None,
        environment: Environment::new(),
    };

    let mut process_manager = ProcessManager::new(process_config, port_manager);

    // Create file watcher if live-reload is enabled
    let mut file_watcher = if let Some(watch_config) = config.file_watch_config() {
        Some(FileWatcher::new(watch_config)?)
    } else {
        None
    };

    // Run the main event loop
    let code = run_main_loop(
        config,
        &mut process_manager,
        signal_handler,
        &mut file_watcher,
    )
    .await?;

    info!("scinit exiting with code {}", code);
    Ok(code)
}

/// Main event loop orchestration
async fn run_main_loop(
    config: Config,
    process_manager: &mut ProcessManager,
    signal_handler: &mut SignalHandler,
    file_watcher: &mut Option<FileWatcher>,
) -> Result<i32> {
    let mut zombie_reap_interval = interval(config.zombie_reap_interval);
    // Shares tokio's SIGCHLD handler, which also drives `Child::wait()`
    let mut sigchld = signal(SignalKind::child())?;

    info!(
        "init system started, managing subprocess: {}",
        config.command
    );

    // Start file watching if enabled
    if let Some(ref mut file_watcher) = file_watcher {
        file_watcher.start_watching().await?;
        info!("File watching started for live-reload");
    } else {
        debug!("Live-reload disabled, no file watching");
    }

    // Spawn initial process
    process_manager.spawn_process().await?;

    // Setup process group
    if let Some(pid) = process_manager.process_info().pid {
        use nix::unistd::getpgid;
        let pgid = getpgid(Some(pid))?;
        tokio::task::spawn_blocking(move || process_group_to_foreground(pgid)).await??;
    }

    loop {
        // Check for file events first (if enabled)
        if file_watcher.is_some() && handle_file_events(file_watcher, process_manager).await? {
            return Ok(1); // Restart limit exceeded
        }

        select! {
            // Check if subprocess has exited
            exit_status = process_manager.wait_for_exit() => {
                match exit_status {
                    Ok(Some(status)) => {
                        // Scenario A: Child process exit handling
                        return handle_child_exit(status).await;
                    }
                    Ok(None) => {
                        // No process to wait for, continue
                        continue;
                    }
                    Err(e) => {
                        error!("error waiting for subprocess: {}", e);
                        return Err(e);
                    }
                }
            }

            // Synchronous signal handling - proper for init systems
            signal = signal_handler.wait_for_signal() => {
                let signal = signal?;
                match signal_handler.process_signal(signal, process_manager, config.live_reload.graceful_timeout_secs).await? {
                    SignalAction::Exit => {
                        // The child's status if it was observed; otherwise
                        // report death by the signal that stopped us
                        return Ok(process_manager
                            .process_info()
                            .exit_status
                            .map(exit_code)
                            .unwrap_or(128 + signal as i32));
                    }
                    SignalAction::Continue => {},
                }
            }

            // Reap orphans as soon as they exit (matters when scinit is PID 1)
            _ = sigchld.recv() => {
                debug!("received SIGCHLD, reaping zombie processes");
                reap_zombies_async().await;
            }

            // Periodic zombie reaping (less frequent, non-blocking)
            _ = zombie_reap_interval.tick() => {
                reap_zombies_async().await;
            }
        }
    }
}

