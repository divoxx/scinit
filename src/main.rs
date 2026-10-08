// scinit's output goes through tracing events (src/logging.rs), never directly
#![deny(clippy::print_stdout, clippy::print_stderr)]

type Result<T> = eyre::Result<T>;

mod cli;
mod environment;
mod exit_status;
mod fds;
mod file_watcher;
mod logging;
mod port_manager;
mod process_manager;
mod program;
mod reaper;
mod signals;
mod socket_activation;
mod terminal;

use clap::Parser;
use std::time::Duration;
use tokio::select;
use tokio::signal::unix::{signal, SignalKind};
use tokio::time::interval;
use tracing::{debug, error, info, warn};

use cli::{Cli, Config};
use exit_status::{exit_code, handle_child_exit, log_child_exit_while_watching, signal_exit_code};
use file_watcher::{FileChangeEvent, FileWatcher};
use port_manager::PortManager;
use process_manager::ProcessManager;
use reaper::{reap_before_exit, spawn_zombie_reap};
use signals::{Signal, SignalHandler};

fn main() {
    logging::init();
    // Mirror the child's exit status, like other container inits do. Errors
    // are logged like everything else scinit says: `ERROR scinit: ...`
    std::process::exit(run().unwrap_or_else(|e| {
        error!("{:#}", e);
        1
    }))
}

/// Runs scinit and returns the exit code to exit with
fn run() -> Result<i32> {
    info!("scinit starting");

    // Before any other thread exists, so every thread inherits the mask
    let mut signal_handler = SignalHandler::install()?;
    debug!(
        "blocked signals {:?} on all threads",
        signal_handler.handled_signals()
    );

    debug!("starting tokio runtime");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let result = rt.block_on(app_main(&mut signal_handler));
    // Shut down with a timeout on every path: blocking tasks parked in
    // sigwait never finish, so dropping the runtime would block forever
    rt.shutdown_timeout(Duration::from_millis(100));
    result
}

/// Returns the exit code scinit should exit with
async fn app_main(signal_handler: &mut SignalHandler) -> Result<i32> {
    // Parse CLI arguments
    let cli = Cli::parse();

    // Convert CLI to configuration
    let config = Config::from_cli(cli)?;

    // Setup components
    let port_manager = PortManager::new(config.port_binding.clone());
    let mut process_manager = ProcessManager::new(config.process_config(), port_manager);

    // Run the main event loop
    let result = run_main_loop(&config, &mut process_manager, signal_handler).await;
    // However the loop ended: the child's exit, a termination signal or an error
    reap_before_exit();
    let code = result?;

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
    let mut file_watcher = match config.file_watch_config() {
        Some(watch_config) => {
            let watcher = FileWatcher::start(watch_config)?;
            info!("File watching started for live-reload");
            Some(watcher)
        }
        None => {
            debug!("Live-reload disabled, no file watching");
            None
        }
    };

    let live_reload = file_watcher.is_some();

    // Spawn initial process. Failing here ends scinit, with or without
    // live reload
    process_manager.spawn_process().await?;

    loop {
        select! {
            // Check if subprocess has exited
            exit = process_manager.wait_for_exit() => {
                let exit = exit.inspect_err(|e| error!("error waiting for subprocess: {}", e))?;
                if !live_reload {
                    return Ok(handle_child_exit(exit.status));
                }
                // Keep running, with the sockets bound, until a file change
                // starts the next child
                log_child_exit_while_watching(exit.status, exit.ran_for);
            }

            // Synchronous signal handling - proper for init systems
            signal = signal_handler.wait_for_signal() => {
                let signal = signal?;
                match on_signal(signal, process_manager, signal_handler).await {
                    SignalAction::Exit => return Ok(exit_code_after_signal(process_manager, signal)),
                    SignalAction::Continue => {},
                }
            }

            // Live-reload: restart as soon as a (debounced) change arrives
            Some(event) = next_file_event(&mut file_watcher) => {
                if let Some(signal) = on_file_event(event, process_manager, signal_handler).await? {
                    // The restart was cancelled before the spawn: shut down
                    // as for any termination signal, which also stops the
                    // old child if it is still running
                    on_signal(signal, process_manager, signal_handler).await;
                    return Ok(exit_code_after_signal(process_manager, signal));
                }
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

/// What the main loop does after a signal
#[derive(Debug, PartialEq)]
enum SignalAction {
    /// Continue normal operation
    Continue,
    /// Exit the init system
    Exit,
}

/// Handles a signal according to init system semantics
async fn on_signal(
    signal: Signal,
    process_manager: &mut ProcessManager,
    signal_handler: &mut SignalHandler,
) -> SignalAction {
    match signal {
        Signal::SIGTERM | Signal::SIGINT | Signal::SIGQUIT => {
            info!(
                "received termination signal {:?}, initiating graceful shutdown",
                signal
            );
            // Forward the signal itself, escalating to SIGKILL if the child
            // outlives the graceful timeout or another termination signal
            // arrives
            info!(
                "Termination signal {:?} received, forwarding to child process (timeout: {}s)",
                signal,
                process_manager.graceful_shutdown_timeout().as_secs()
            );
            process_manager
                .shutdown_with_signal(signal, async || signal_handler.wait_for_signal().await)
                .await;

            info!("scinit exiting due to termination signal {:?}", signal);
            SignalAction::Exit
        }
        _ if !process_manager.is_running() => {
            // Between a child's exit and the next file change under live
            // reload: the next child starts without it
            debug!("no child running, dropping signal {:?}", signal);
            SignalAction::Continue
        }
        Signal::SIGUSR1 | Signal::SIGUSR2 | Signal::SIGHUP => {
            // These signals should be forwarded to the child process only
            info!("forwarding signal {:?} to child process", signal);
            process_manager.try_signal_group(signal);
            SignalAction::Continue
        }
        _ => {
            // Any other signals we somehow receive should be forwarded
            debug!("forwarding unexpected signal {:?} to child process", signal);
            process_manager.try_signal_group(signal);
            SignalAction::Continue
        }
    }
}

/// Handles one file watcher event, restarting the process on a change, or
/// starting it if the last child exited.
///
/// A termination signal cancels the restart if it arrives before the new
/// child is spawned; it is returned for the caller to shut down with. Other
/// signals arriving meanwhile wait for the new child. A failed spawn is
/// logged and leaves scinit waiting for the next change.
async fn on_file_event(
    event: FileChangeEvent,
    process_manager: &mut ProcessManager,
    signal_handler: &mut SignalHandler,
) -> Result<Option<Signal>> {
    match event {
        FileChangeEvent::FileChanged(path) => {
            info!("File changed: {:?}, triggering restart", path);
            let cancel = signal_handler.wait_for_termination_signal();
            match process_manager.restart_unless(cancel).await {
                Err(e) => {
                    error!("{:#}; waiting for a file change to try again", e);
                    Ok(None)
                }
                Ok(Some(signal)) => {
                    let signal = signal?;
                    info!(
                        "received termination signal {:?} during restart, not starting a new child",
                        signal
                    );
                    Ok(Some(signal))
                }
                Ok(None) => Ok(None),
            }
        }
        FileChangeEvent::WatchError(error) => {
            warn!("File watching error: {}", error);
            Ok(None)
        }
    }
}

/// The child's exit code if its exit was observed; otherwise death by the
/// signal that stopped scinit
fn exit_code_after_signal(process_manager: &ProcessManager, signal: Signal) -> i32 {
    process_manager
        .exit_status()
        .map(exit_code)
        .unwrap_or(signal_exit_code(signal as i32))
}

/// Next file watcher event, or never if live-reload is disabled
async fn next_file_event(file_watcher: &mut Option<FileWatcher>) -> Option<FileChangeEvent> {
    match file_watcher {
        Some(watcher) => watcher.next_event().await,
        None => std::future::pending().await,
    }
}
