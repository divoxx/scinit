use super::Result;
use crate::process_manager::ProcessManager;

pub use nix::sys::signal::Signal;

use nix::sys::signal::{pthread_sigmask, SaFlags, SigAction, SigHandler, SigSet, SigmaskHow};
use eyre::eyre;
use std::time::Duration;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
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
/// Handled signals are blocked on every thread and consumed by one dedicated
/// `sigwait` thread, which forwards them over a channel. Signals that arrive
/// while the main loop is busy (e.g. during a restart) queue in the channel
/// instead of being lost.
///
/// SIGCHLD is deliberately not handled here: tokio's own SIGCHLD handler
/// drives `Child::wait()`, so the main loop observes it through a tokio
/// signal stream instead.
#[derive(Debug)]
pub(super) struct SignalHandler {
    /// Signals consumed by the sigwait thread (blocked on every thread)
    handled_signals: SigSet,
    receiver: UnboundedReceiver<Signal>,
}

impl SignalHandler {
    /// Blocks the handled signals and starts the sigwait thread.
    ///
    /// Must be called on the main thread before any other thread exists
    /// (in particular before the tokio runtime is built): threads inherit the
    /// signal mask, so this leaves no thread on which a handled signal could
    /// take its default action and kill scinit.
    ///
    /// Critical synchronous signals (SIGFPE, SIGILL, SIGSEGV, ...) are never
    /// blocked.
    pub fn install() -> Result<Self> {
        // Signals that init should handle synchronously:
        // - SIGTERM, SIGINT, SIGQUIT: Termination signals for graceful shutdown
        // - SIGUSR1, SIGUSR2: User-defined signals to forward
        // - SIGHUP: Hangup signal to forward
        let signals_to_handle = [
            Signal::SIGTERM,
            Signal::SIGINT,
            Signal::SIGQUIT,
            Signal::SIGUSR1,
            Signal::SIGUSR2,
            Signal::SIGHUP,
        ];

        let mut handled_signals = SigSet::empty();
        for &sig in &signals_to_handle {
            handled_signals.add(sig);
        }

        pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&handled_signals), None)?;

        // Ignore SIGTTIN and SIGTTOU to prevent blocking on terminal operations
        // This is critical for init systems running in containers
        let ignore_action = SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty());
        unsafe {
            nix::sys::signal::sigaction(Signal::SIGTTIN, &ignore_action)?;
            nix::sys::signal::sigaction(Signal::SIGTTOU, &ignore_action)?;
        }

        let (sender, receiver) = unbounded_channel();
        std::thread::Builder::new()
            .name("scinit-sigwait".into())
            .spawn(move || loop {
                match handled_signals.wait() {
                    Ok(signal) => {
                        debug!("Received signal: {:?} (init semantics)", signal);
                        if sender.send(signal).is_err() {
                            // Main loop is gone; nothing left to deliver to
                            break;
                        }
                    }
                    Err(e) => error!("sigwait failed: {}", e),
                }
            })?;

        Ok(SignalHandler {
            handled_signals,
            receiver,
        })
    }

    /// The signals consumed by the sigwait thread
    pub fn handled_signals(&self) -> SigSet {
        self.handled_signals
    }

    /// Waits for the next handled signal.
    ///
    /// Cancel-safe: dropping the future (e.g. when another `select!` branch
    /// wins) never loses a signal.
    pub async fn wait_for_signal(&mut self) -> Result<Signal> {
        self.receiver
            .recv()
            .await
            .ok_or_else(|| eyre!("signal thread exited"))
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
    /// Exit the init system
    Exit,
}
