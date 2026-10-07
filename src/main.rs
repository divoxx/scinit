type Result<T> = eyre::Result<T>;

mod cli;
mod environment;
mod exit_status;
mod file_watcher;
mod port_manager;
mod process_manager;
mod reaper;
mod signals;
mod socket_activation;

use clap::Parser;
use nix::unistd::{getpgid, tcsetpgrp, Pid};
use std::fs::File;
use std::io::IsTerminal;
use std::time::Duration;
use tokio::select;
use tokio::signal::unix::{signal, SignalKind};
use tokio::time::interval;
use tracing::{debug, error, info, warn};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use cli::{Cli, Config};
use exit_status::{exit_code, handle_child_exit, signal_exit_code};
use file_watcher::{FileChangeEvent, FileWatcher};
use port_manager::PortManager;
use process_manager::ProcessManager;
use reaper::spawn_zombie_reap;
use signals::{Signal, SignalHandler};

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
    let mut process_manager = ProcessManager::new(config.process_config(), port_manager);

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
                match on_signal(signal, process_manager).await {
                    SignalAction::Exit => return Ok(exit_code_after_signal(process_manager, signal)),
                    SignalAction::Continue => {},
                }
            }

            // Live-reload: restart as soon as a (debounced) change arrives
            Some(event) = next_file_event(&mut file_watcher) => {
                on_file_event(event, process_manager).await?;
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
async fn on_signal(signal: Signal, process_manager: &mut ProcessManager) -> SignalAction {
    match signal {
        Signal::SIGTERM | Signal::SIGINT | Signal::SIGQUIT => {
            info!(
                "received termination signal {:?}, initiating graceful shutdown",
                signal
            );
            // Forward the signal itself, escalating to SIGKILL if the child
            // outlives the graceful timeout
            info!(
                "Termination signal {:?} received, forwarding to child process (timeout: {}s)",
                signal,
                process_manager.graceful_shutdown_timeout().as_secs()
            );
            process_manager.shutdown_with_signal(signal).await;

            info!("scinit exiting due to termination signal {:?}", signal);
            SignalAction::Exit
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

/// Handles one file watcher event, restarting the process on a change
async fn on_file_event(event: FileChangeEvent, process_manager: &mut ProcessManager) -> Result<()> {
    match event {
        FileChangeEvent::FileChanged(path) => {
            info!("File changed: {:?}, triggering restart", path);
            process_manager.restart().await?;
        }
        FileChangeEvent::WatchError(error) => {
            warn!("File watching error: {}", error);
        }
    }
    Ok(())
}

/// Hands the terminal (if any) to the child's process group
async fn foreground_child(process_manager: &ProcessManager) -> Result<()> {
    let Some(pid) = process_manager.pid() else {
        return Ok(());
    };
    let pgid = getpgid(Some(pid))?;
    tokio::task::spawn_blocking(move || process_group_to_foreground(pgid)).await?
}

/// Sets the process group as the foreground process group if a terminal is available
fn process_group_to_foreground(pgid: Pid) -> Result<()> {
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
