use super::Result;
use crate::process_manager::ProcessManager;

pub use nix::sys::signal::Signal;

use nix::sys::signal::{pthread_sigmask, SaFlags, SigAction, SigHandler, SigSet, SigmaskHow};
use std::time::Duration;
use tracing::{debug, error, info, warn};

/// Converts signal number to human-readable name
pub fn signal_name(signal: i32) -> &'static str {
    match signal {
        2 => "SIGINT",
        9 => "SIGKILL",
        15 => "SIGTERM",
        3 => "SIGQUIT",
        1 => "SIGHUP",
        10 => "SIGUSR1",
        12 => "SIGUSR2",
        17 => "SIGCHLD",
        _ => "UNKNOWN",
    }
}

/// Signal handler for the init system with proper init semantics.
///
/// This handler uses platform-appropriate signal handling that maintains
/// proper init system semantics: synchronous, deterministic signal processing
/// with guaranteed delivery order.
#[derive(Debug)]
pub(super) struct SignalHandler {
    /// Set of signals we handle (blocked for synchronous handling)
    handled_signals: SigSet,
}

impl SignalHandler {
    /// Creates a new signal handler with proper init system signal handling.
    ///
    /// This function:
    /// - Blocks signals that should be handled synchronously by init
    /// - Leaves critical signals (SIGFPE, SIGILL, etc.) unblocked
    /// - Uses platform-appropriate synchronous signal handling
    /// - Maintains proper init system semantics across platforms
    pub fn new() -> Self {
        // Create signal set with the signals we want to handle
        let mut handled_signals = SigSet::empty();

        // Signals that init should handle synchronously:
        // - SIGTERM, SIGINT, SIGQUIT: Termination signals for graceful shutdown
        // - SIGUSR1, SIGUSR2: User-defined signals to forward
        // - SIGHUP: Hangup signal to forward
        // - SIGCHLD: Child status changes (always handled by init)
        let signals_to_handle = [
            Signal::SIGTERM,
            Signal::SIGINT,
            Signal::SIGQUIT,
            Signal::SIGUSR1,
            Signal::SIGUSR2,
            Signal::SIGHUP,
            Signal::SIGCHLD,
        ];

        // Add signals to the set
        for &sig in &signals_to_handle {
            handled_signals.add(sig);
        }

        SignalHandler { handled_signals }
    }

    pub fn setup_thread_signals(&self) -> Result<()> {
        let thread = std::thread::current();

        // Block these signals for synchronous handling
        pthread_sigmask(SigmaskHow::SIG_SETMASK, Some(&self.handled_signals), None)?;
        debug!(
            "Successfully blocked signals for thread {} {:?}",
            thread.name().unwrap(),
            thread.id(),
        );

        // Ignore SIGTTIN and SIGTTOU to prevent blocking on terminal operations
        // This is critical for init systems running in containers
        let ignore_action = SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty());
        unsafe {
            nix::sys::signal::sigaction(Signal::SIGTTIN, &ignore_action)?;
            nix::sys::signal::sigaction(Signal::SIGTTOU, &ignore_action)?;
        }

        Ok(())
    }

    /// Waits for a signal with timeout using proper init system semantics.
    ///
    /// This function provides synchronous, deterministic signal handling that
    /// maintains init system guarantees for signal ordering and delivery.
    pub async fn wait_for_signal(&self) -> Result<Signal> {
        // Use spawn_blocking to maintain init semantics while being async-compatible
        let signals = self.handled_signals;

        tokio::task::spawn_blocking(move || -> Result<Signal> {
            // Use sigwait for synchronous signal waiting
            match signals.wait() {
                Ok(signal) => {
                    debug!("Received signal: {:?} (init semantics)", signal);
                    Ok(signal)
                }
                Err(e) => Err(e.into()),
            }
        })
        .await?
    }
}

impl SignalHandler {
    /// Processes a specific signal according to init system semantics
    pub async fn process_signal(
        &self,
        signal: Signal,
        process_manager: &mut ProcessManager,
        graceful_timeout_secs: u64,
    ) -> Result<SignalAction> {
        match signal {
            Signal::SIGCHLD => {
                // Reap zombie processes asynchronously - this is always handled by init
                debug!("received SIGCHLD, reaping zombie processes");
                Ok(SignalAction::ReapZombies)
            }
            Signal::SIGTERM | Signal::SIGINT | Signal::SIGQUIT => {
                // Scenario B: Signal forwarding with graceful shutdown and timeout
                info!(
                    "received termination signal {:?}, initiating graceful shutdown",
                    signal
                );
                self.handle_termination_signal(signal, process_manager, graceful_timeout_secs)
                    .await?;
                Ok(SignalAction::Exit)
            }
            Signal::SIGUSR1 | Signal::SIGUSR2 | Signal::SIGHUP => {
                // These signals should be forwarded to the child process only
                info!("forwarding signal {:?} to child process", signal);
                if let Err(e) = process_manager.forward_signal(signal) {
                    warn!("failed to forward signal {:?} to child: {}", signal, e);
                }
                Ok(SignalAction::Continue)
            }
            _ => {
                // Any other signals we somehow receive should be forwarded
                debug!("forwarding unexpected signal {:?} to child process", signal);
                if let Err(e) = process_manager.forward_signal(signal) {
                    warn!("failed to forward signal {:?} to child: {}", signal, e);
                }
                Ok(SignalAction::Continue)
            }
        }
    }

    /// Handles termination signals with proper timeout and escalation (Scenario B)
    async fn handle_termination_signal(
        &self,
        signal: Signal,
        process_manager: &mut ProcessManager,
        graceful_timeout_secs: u64,
    ) -> Result<()> {
        info!(
            "Termination signal {:?} received, forwarding to child process",
            signal
        );

        // Forward the signal to child process
        if let Err(e) = process_manager.forward_signal(signal) {
            warn!("Failed to forward signal {:?} to child: {}", signal, e);
        }

        match signal {
            Signal::SIGTERM => {
                // SIGTERM gets graceful shutdown with timeout
                info!(
                    "Waiting for child process to exit gracefully (timeout: {}s)",
                    graceful_timeout_secs
                );

                if (process_manager.graceful_shutdown().await).is_err() {
                    warn!("Graceful shutdown timed out, child process may have been force-killed");
                }
            }
            Signal::SIGINT | Signal::SIGQUIT => {
                // SIGINT/SIGQUIT get shorter timeout or immediate cleanup
                info!("Waiting for child process to exit (signal: {:?})", signal);

                // Wait a bit for child to exit, but don't use full graceful timeout
                tokio::time::sleep(Duration::from_secs(2)).await;

                // Force kill if still running
                if process_manager.is_running() {
                    warn!(
                        "Child process didn't exit after {:?}, forcing termination",
                        signal
                    );
                    if let Err(e) = process_manager.force_kill().await {
                        error!("Failed to force kill child process: {}", e);
                    }
                }
            }
            _ => unreachable!(),
        }

        info!("scinit exiting due to termination signal {:?}", signal);
        Ok(())
    }
}

/// Actions that signal processing can return
#[derive(Debug, Clone, PartialEq)]
pub enum SignalAction {
    /// Continue normal operation
    Continue,
    /// Reap zombie processes
    ReapZombies,
    /// Exit the init system
    Exit,
}
