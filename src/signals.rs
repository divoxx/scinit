use super::Result;
use crate::process_manager::ProcessManager;

pub use nix::sys::signal::Signal;

use nix::libc::{sigaction, sigemptyset, SIG_IGN, sigtimedwait, timespec};
use nix::sys::signal::{pthread_sigmask, SigmaskHow, SigSet};
use std::mem::MaybeUninit;
use std::ptr;
use std::time::Duration;
use tokio::time;
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

/// Signal handler for the init system that manages signal blocking and forwarding.
/// 
/// This handler properly blocks signals that should be handled by the init system
/// and uses sigtimedwait for synchronous signal handling, which is more appropriate
/// for init systems than async signal handling.
pub(super) struct SignalHandler {
    /// Set of signals we handle (blocked for synchronous handling)
    handled_signals: SigSet,
}

impl SignalHandler {
    /// Creates a new signal handler with proper init system signal masking.
    /// 
    /// This function:
    /// - Blocks only the signals that should be handled synchronously by init
    /// - Leaves synchronous/critical signals (SIGFPE, SIGILL, etc.) unblocked 
    /// - Ignores terminal signals that could block the init process
    /// - Sets up proper signal mask inheritance for child processes
    pub fn new() -> Result<Self> {
        // Create signal set with the signals we want to handle
        let mut handled_signals = SigSet::empty();
        
        // Signals that init should handle and forward to children:
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

        // Set the signal mask for this process (replace existing mask)
        // Use pthread_sigmask instead of sigprocmask for thread safety
        if let Err(e) = pthread_sigmask(SigmaskHow::SIG_SETMASK, Some(&handled_signals), None) {
            warn!("Failed to set signal mask: {}", e);
            return Err(e.into());
        } else {
            debug!("Successfully set signal mask for synchronous handling");
        }

        // Ignore SIGTTIN and SIGTTOU to prevent blocking on terminal operations
        // This is critical for init systems running in containers
        let mut ign_action: sigaction = unsafe { MaybeUninit::zeroed().assume_init() };
        unsafe {
            ign_action.sa_sigaction = SIG_IGN as usize;
            sigemptyset(&mut ign_action.sa_mask);
        }

        unsafe {
            if sigaction(nix::libc::SIGTTIN, &ign_action, ptr::null_mut()) != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            if sigaction(nix::libc::SIGTTOU, &ign_action, ptr::null_mut()) != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }

        debug!("Signal handler initialized with proper init signal masking");

        Ok(SignalHandler { 
            handled_signals,
        })
    }


    /// Waits for a signal with timeout.
    /// 
    /// This function uses proper synchronous signal handling via sigtimedwait,
    /// which is the correct approach for init systems. We run it in a blocking
    /// task to maintain async compatibility with the rest of the system.
    /// 
    /// # Arguments
    /// * `timeout_duration` - The maximum time to wait for a signal
    pub async fn wait_for_signal(&self, timeout_duration: Duration) -> Result<Option<Signal>> {
        // Copy the signal set for use in the blocking task
        let handled_signals = *self.handled_signals.as_ref();
        
        // Run sigtimedwait in a blocking task since it's a blocking syscall
        let result = tokio::task::spawn_blocking(move || {
            let timeout_spec = timespec {
                tv_sec: timeout_duration.as_secs() as i64,
                tv_nsec: timeout_duration.subsec_nanos() as i64,
            };

            let signal_num = unsafe {
                sigtimedwait(&handled_signals, ptr::null_mut(), &timeout_spec)
            };

            if signal_num > 0 {
                // Convert signal number to Signal enum
                let signal = match signal_num {
                    nix::libc::SIGTERM => Some(Signal::SIGTERM),
                    nix::libc::SIGINT => Some(Signal::SIGINT),
                    nix::libc::SIGQUIT => Some(Signal::SIGQUIT),
                    nix::libc::SIGUSR1 => Some(Signal::SIGUSR1),
                    nix::libc::SIGUSR2 => Some(Signal::SIGUSR2),
                    nix::libc::SIGHUP => Some(Signal::SIGHUP),
                    nix::libc::SIGCHLD => Some(Signal::SIGCHLD),
                    _ => {
                        warn!("Received unexpected signal: {}", signal_num);
                        None
                    }
                };
                
                if let Some(sig) = signal {
                    debug!("Received signal: {:?}", sig);
                    Ok(Some(sig))
                } else {
                    Ok(None)
                }
            } else {
                let errno = std::io::Error::last_os_error();
                match errno.raw_os_error() {
                    Some(nix::libc::EAGAIN) | Some(nix::libc::ETIMEDOUT) => {
                        // Timeout - no signal received
                        Ok(None)
                    }
                    Some(nix::libc::EINTR) => {
                        // Interrupted by another signal - try again
                        Ok(None)
                    }
                    _ => {
                        warn!("sigtimedwait error: {}", errno);
                        Err(errno)
                    }
                }
            }
        }).await?;
        
        result.map_err(|e| e.into())
    }
}

impl Drop for SignalHandler {
    /// Restores the original signal mask when the handler is dropped.
    /// 
    /// This ensures that signal handling is properly cleaned up even if
    /// the handler is dropped due to an error or panic.
    fn drop(&mut self) {
        // Unblock the signals we were handling (restore to previous state)
        if let Err(e) = pthread_sigmask(SigmaskHow::SIG_UNBLOCK, Some(&self.handled_signals), None) {
            // Log error but don't panic in drop
            eprintln!("Failed to restore signal mask: {}", e);
        }
        
        debug!("Signal handler cleaned up, signal mask restored");
    }
}

impl SignalHandler {
    /// Processes a specific signal according to init system semantics
    pub async fn process_signal(&self, signal: Signal, process_manager: &mut ProcessManager, graceful_timeout_secs: u64) -> Result<SignalAction> {
        match signal {
            Signal::SIGCHLD => {
                // Reap zombie processes asynchronously - this is always handled by init
                debug!("received SIGCHLD, reaping zombie processes");
                Ok(SignalAction::ReapZombies)
            }
            Signal::SIGTERM | Signal::SIGINT | Signal::SIGQUIT => {
                // Scenario B: Signal forwarding with graceful shutdown and timeout
                info!("received termination signal {:?}, initiating graceful shutdown", signal);
                self.handle_termination_signal(signal, process_manager, graceful_timeout_secs).await?;
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
    async fn handle_termination_signal(&self, signal: Signal, process_manager: &mut ProcessManager, graceful_timeout_secs: u64) -> Result<()> {
        info!("Termination signal {:?} received, forwarding to child process", signal);
        
        // Forward the signal to child process
        if let Err(e) = process_manager.forward_signal(signal) {
            warn!("Failed to forward signal {:?} to child: {}", signal, e);
        }
        
        match signal {
            Signal::SIGTERM => {
                // SIGTERM gets graceful shutdown with timeout
                info!("Waiting for child process to exit gracefully (timeout: {}s)", graceful_timeout_secs);
                
                if let Err(_) = process_manager.graceful_shutdown().await {
                    warn!("Graceful shutdown timed out, child process may have been force-killed");
                }
            }
            Signal::SIGINT | Signal::SIGQUIT => {
                // SIGINT/SIGQUIT get shorter timeout or immediate cleanup
                info!("Waiting for child process to exit (signal: {:?})", signal);
                
                // Wait a bit for child to exit, but don't use full graceful timeout
                time::sleep(Duration::from_secs(2)).await;
                
                // Force kill if still running
                if process_manager.is_running() {
                    warn!("Child process didn't exit after {:?}, forcing termination", signal);
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
