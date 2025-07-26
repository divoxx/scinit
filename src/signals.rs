use super::Result;

pub use nix::sys::signal::Signal;

use nix::libc::{sigaction, sigemptyset, SIG_IGN, sigtimedwait, timespec};
use nix::sys::signal::{pthread_sigmask, SigmaskHow, SigSet};
use std::mem::MaybeUninit;
use std::ptr;
use tokio::time::Duration;
use tracing::{debug, warn};

/// Sets up signal masking for the init system before starting tokio runtime
/// This ensures all threads inherit the correct signal mask
pub fn setup_signal_masking() -> Result<()> {
    use std::mem::MaybeUninit;
    use std::ptr;

    // Create signal set with the signals we want to handle
    let mut sigset = SigSet::empty();
    
    // Signals that init should handle and forward to children
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
        sigset.add(sig);
    }

    // Block the handled signals for this process (add to existing mask)
    // Use pthread_sigmask instead of sigprocmask for thread safety
    if let Err(e) = pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&sigset), None) {
        warn!("Failed to block signals: {}", e);
        return Err(e.into());
    } else {
        debug!("Successfully blocked signals for synchronous handling");
    }

    // Ignore SIGTTIN and SIGTTOU to prevent blocking on terminal operations
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

    debug!("Signal masking configured before tokio runtime startup");
    Ok(())
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

        // Block the handled signals for this process (add to existing mask)
        // Use pthread_sigmask instead of sigprocmask for thread safety
        if let Err(e) = pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&handled_signals), None) {
            warn!("Failed to block signals: {}", e);
            return Err(e.into());
        } else {
            debug!("Successfully blocked signals for synchronous handling");
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

            debug!("Waiting for signals with sigtimedwait (timeout: {}ms)", timeout_duration.as_millis());
            let signal_num = unsafe {
                sigtimedwait(&handled_signals, ptr::null_mut(), &timeout_spec)
            };
            debug!("sigtimedwait returned: {}", signal_num);

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
                }
                Ok(signal)
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
