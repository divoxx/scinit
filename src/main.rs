type Result<T> = color_eyre::eyre::Result<T>;

mod cli;
mod file_watcher;
mod port_manager;
mod process_manager;
mod signals;

use clap::Parser;
use std::collections::HashMap;
use std::time::Duration;
use tokio::select;
use tokio::time::interval;
use tracing::{debug, error, info};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use cli::{Cli, Config};
use file_watcher::{FileWatcher, handle_file_events};
use port_manager::PortManager;
use process_manager::{ProcessConfig, ProcessManager, process_group_to_foreground, handle_child_exit, reap_zombies_async};
use signals::{SignalHandler, SignalAction};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize error handling and logging
    color_eyre::install()?;

    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    // Note: We don't create a separate process group for scinit to allow Ctrl+C during development
    // Signal masking is handled when SignalHandler is created (pthread_sigmask affects only calling thread)

    info!("scinit starting");

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
        environment: HashMap::new(),
    };
    
    let mut process_manager = ProcessManager::new(process_config, port_manager);
    let mut signal_handler = SignalHandler::new()?;

    // Create file watcher if live-reload is enabled
    let mut file_watcher = if let Some(watch_config) = config.file_watch_config() {
        Some(FileWatcher::new(watch_config)?)
    } else {
        None
    };

    // Run the main event loop
    run_main_loop(config, &mut process_manager, &mut signal_handler, &mut file_watcher).await?;

    info!("scinit exiting");
    Ok(())
}

/// Main event loop orchestration
async fn run_main_loop(
    config: Config,
    process_manager: &mut ProcessManager,
    signal_handler: &mut SignalHandler, 
    file_watcher: &mut Option<FileWatcher>
) -> Result<()> {
    let mut zombie_reap_interval = interval(config.zombie_reap_interval);

    info!("init system started, managing subprocess: {}", config.command);

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
        if file_watcher.is_some()
            && handle_file_events(file_watcher, process_manager).await? {
            return Ok(()); // Exit requested
        }

        select! {
            // Check if subprocess has exited
            exit_status = process_manager.wait_for_exit() => {
                match exit_status {
                    Ok(Some(status)) => {
                        // Scenario A: Child process exit handling
                        handle_child_exit(status).await?;
                        return Ok(());
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
            signal = signal_handler.wait_for_signal(config.signal_poll_interval) => {
                match signal? {
                    Some(signal) => {
                        info!("received signal: {:?}", signal);
                        match signal_handler.process_signal(signal, process_manager, config.live_reload.graceful_timeout_secs).await? {
                            SignalAction::Exit => return Ok(()),
                            SignalAction::ReapZombies => reap_zombies_async().await,
                            SignalAction::Continue => {},
                        }
                    }
                    None => {
                        // No signal received, continue
                    }
                }
            }

            // Periodic zombie reaping (less frequent, non-blocking)
            _ = zombie_reap_interval.tick() => {
                reap_zombies_async().await;
            }
        }
    }
}