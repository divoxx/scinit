type Result<T> = eyre::Result<T>;

mod cli;
mod environment;
mod file_watcher;
mod port_manager;
mod process_manager;
mod signals;
mod socket_activation;

use clap::Parser;
use environment::Environment;
use nix::unistd::getpgid;
use std::time::Duration;
use tokio::select;
use tokio::signal::unix::{signal, SignalKind};
use tokio::time::interval;
use tracing::{debug, error, info};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use cli::{Cli, Config};
use file_watcher::{handle_file_event, FileChangeEvent, FileWatcher};
use port_manager::PortManager;
use process_manager::{
    exit_code, handle_child_exit, process_group_to_foreground, spawn_zombie_reap, ProcessConfig,
    ProcessManager,
};
use signals::{Signal, SignalAction, SignalHandler};

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
        // Restarts only happen with live reload
        restart_delay: config
            .live_reload
            .as_ref()
            .map_or(Duration::ZERO, |live_reload| {
                Duration::from_millis(live_reload.restart_delay_ms)
            }),
        graceful_shutdown_timeout: Duration::from_secs(config.graceful_timeout_secs),
        environment: Environment::new(),
    };

    let mut process_manager = ProcessManager::new(process_config, port_manager);

    // Run the main event loop
    let code = run_main_loop(&config, &mut process_manager, signal_handler).await?;

    info!("scinit exiting with code {}", code);
    Ok(code)
}

/// Main event loop orchestration
async fn run_main_loop(
    config: &Config,
    process_manager: &mut ProcessManager,
    signal_handler: &mut SignalHandler,
) -> Result<i32> {
    let mut zombie_reap_interval = interval(config.zombie_reap_interval);
    // Shares tokio's SIGCHLD handler, which also drives `Child::wait()`
    let mut sigchld = signal(SignalKind::child())?;

    info!(
        "init system started, managing subprocess: {}",
        config.command
    );

    // Start file watching if enabled
    let mut file_watcher = config.file_watch_config().map(FileWatcher::start).transpose()?;
    if file_watcher.is_some() {
        info!("File watching started for live-reload");
    } else {
        debug!("Live-reload disabled, no file watching");
    }

    // Spawn initial process
    process_manager.spawn_process().await?;

    foreground_child(process_manager).await?;

    loop {
        select! {
            // Check if subprocess has exited
            exit_status = process_manager.wait_for_exit() => {
                let status = exit_status.inspect_err(|e| error!("error waiting for subprocess: {}", e))?;
                return Ok(handle_child_exit(status));
            }

            // Synchronous signal handling - proper for init systems
            signal = signal_handler.wait_for_signal() => {
                let signal = signal?;
                match signal_handler.process_signal(signal, process_manager, config.graceful_timeout_secs).await? {
                    SignalAction::Exit => return Ok(exit_code_after_signal(process_manager, signal)),
                    SignalAction::Continue => {},
                }
            }

            // Live-reload: restart as soon as a (debounced) change arrives
            Some(event) = next_file_event(&mut file_watcher) => {
                handle_file_event(event, process_manager).await?;
            }

            // Reap orphans as soon as they exit (matters when scinit is PID 1)
            _ = sigchld.recv() => {
                debug!("received SIGCHLD, reaping zombie processes");
                spawn_zombie_reap();
            }

            // Periodic zombie reaping (less frequent, non-blocking)
            _ = zombie_reap_interval.tick() => {
                spawn_zombie_reap();
            }
        }
    }
}

/// Hands the terminal (if any) to the child's process group
async fn foreground_child(process_manager: &ProcessManager) -> Result<()> {
    let Some(pid) = process_manager.pid() else {
        return Ok(());
    };
    let pgid = getpgid(Some(pid))?;
    tokio::task::spawn_blocking(move || process_group_to_foreground(pgid)).await?
}

/// The child's exit code if its exit was observed; otherwise death by the
/// signal that stopped scinit
fn exit_code_after_signal(process_manager: &ProcessManager, signal: Signal) -> i32 {
    process_manager
        .exit_status()
        .map(exit_code)
        .unwrap_or(128 + signal as i32)
}

/// Next file watcher event, or never if live-reload is disabled
async fn next_file_event(file_watcher: &mut Option<FileWatcher>) -> Option<FileChangeEvent> {
    match file_watcher {
        Some(watcher) => watcher.next_event().await,
        None => std::future::pending().await,
    }
}
