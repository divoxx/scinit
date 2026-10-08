//! Receiving the signals scinit handles; main.rs decides what to do with them.

use crate::Result;

pub use nix::sys::signal::Signal;

use eyre::eyre;
use nix::sys::signal::{pthread_sigmask, SaFlags, SigAction, SigHandler, SigSet, SigmaskHow};
use std::collections::VecDeque;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tracing::{debug, error};

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
    /// Signals set aside by `wait_for_termination_signal`, delivered first
    /// by the next `wait_for_signal`
    deferred: VecDeque<Signal>,
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
        let handled_signals = handled_signals();
        pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&handled_signals), None)?;
        // After blocking, so none of them can take its default action on
        // scinit: an ignored signal would be discarded instead of reaching
        // sigwait (e.g. SIGINT for a `scinit ... &` from a script)
        reset_dispositions(handled_signals)?;
        ignore_tty_signals()?;

        let (sender, receiver) = unbounded_channel();
        spawn_sigwait_thread(handled_signals, sender)?;

        Ok(SignalHandler {
            handled_signals,
            receiver,
            deferred: VecDeque::new(),
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
        if let Some(signal) = self.deferred.pop_front() {
            return Ok(signal);
        }
        self.recv().await
    }

    /// Waits for the next termination signal (SIGTERM, SIGINT, SIGQUIT).
    /// Other signals received meanwhile are kept, in order, for
    /// `wait_for_signal`.
    ///
    /// Cancel-safe, like `wait_for_signal`.
    pub async fn wait_for_termination_signal(&mut self) -> Result<Signal> {
        loop {
            let signal = self.recv().await?;
            if is_termination(signal) {
                return Ok(signal);
            }
            debug!("deferring {:?} until the restart is done", signal);
            self.deferred.push_back(signal);
        }
    }

    async fn recv(&mut self) -> Result<Signal> {
        self.receiver
            .recv()
            .await
            .ok_or_else(|| eyre!("signal thread exited"))
    }
}

/// Whether `signal` makes scinit stop the child and exit
pub fn is_termination(signal: Signal) -> bool {
    matches!(signal, Signal::SIGTERM | Signal::SIGINT | Signal::SIGQUIT)
}

/// Signals that init should handle synchronously:
/// - SIGTERM, SIGINT, SIGQUIT: Termination signals for graceful shutdown
/// - SIGUSR1, SIGUSR2: User-defined signals to forward
/// - SIGHUP: Hangup signal to forward
fn handled_signals() -> SigSet {
    [
        Signal::SIGTERM,
        Signal::SIGINT,
        Signal::SIGQUIT,
        Signal::SIGUSR1,
        Signal::SIGUSR2,
        Signal::SIGHUP,
    ]
    .into_iter()
    .collect()
}

/// Resets the dispositions of `set` to the default, undoing any `SIG_IGN`
/// scinit inherited
fn reset_dispositions(set: SigSet) -> Result<()> {
    let default_action = SigAction::new(SigHandler::SigDfl, SaFlags::empty(), SigSet::empty());
    for signal in set.iter() {
        unsafe { nix::sys::signal::sigaction(signal, &default_action)? };
    }
    Ok(())
}

/// Ignores SIGTTIN and SIGTTOU, so terminal operations can't stop scinit.
/// This is critical for init systems running in containers.
fn ignore_tty_signals() -> Result<()> {
    let ignore_action = SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty());
    unsafe {
        nix::sys::signal::sigaction(Signal::SIGTTIN, &ignore_action)?;
        nix::sys::signal::sigaction(Signal::SIGTTOU, &ignore_action)?;
    }
    Ok(())
}

/// Starts the thread that consumes `set` with `sigwait` and sends each
/// signal to `sender`, until the receiver is gone
fn spawn_sigwait_thread(set: SigSet, sender: UnboundedSender<Signal>) -> Result<()> {
    std::thread::Builder::new()
        .name("scinit-sigwait".into())
        .spawn(move || loop {
            match set.wait() {
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
    Ok(())
}
