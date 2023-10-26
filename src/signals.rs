use super::Result;

pub use nix::sys::signal::Signal;

use async_stream::stream;
use futures::stream::{Stream, StreamExt};
use nix::libc::{pthread_sigmask, sigemptyset, sigfillset, sigpending, sigset_t, sigwait};
use nix::sys::signal::SigmaskHow::SIG_BLOCK;
use std::future::Future;
use std::mem::MaybeUninit;
use std::pin::Pin;
use std::ptr;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use tracing::{debug, info, instrument};

pub(super) struct Monitor {
    sigmask: Arc<sigset_t>,
    thread_handler: Option<std::thread::JoinHandle<()>>,
}

impl Monitor {
    pub fn new() -> Self {
        let sigmask: sigset_t = unsafe {
            let mut u = MaybeUninit::<sigset_t>::uninit();
            sigfillset(u.as_mut_ptr());
            u.assume_init()
        };

        Self {
            sigmask: Arc::new(sigmask),
            thread_handler: None,
        }
    }

    pub fn monitor(&mut self) -> Result<mpsc::UnboundedReceiver<Signal>> {
        let pthread_sigmask_ret =
            unsafe { pthread_sigmask(SIG_BLOCK as i32, &*self.sigmask, ptr::null_mut()) };
        if pthread_sigmask_ret != 0 {
            panic!("pthread_sigmask returned error: {}", pthread_sigmask_ret);
        }

        let sigmask = Arc::clone(&self.sigmask);
        let (sig_sender, sig_receiver) = mpsc::unbounded_channel();

        self.thread_handler = Some(std::thread::spawn(move || {
            let mut s: i32 = 0;

            debug!("Calling sigwait");
            let sigwait_ret = unsafe { sigwait(&*sigmask, &mut s) };
            if sigwait_ret != 0 {
                panic!("sigwait returned an error: {}", sigwait_ret);
            }
            debug!("Received signal from sigwait: {}", s);

            if let Err(err) = sig_sender.send(Signal::try_from(s).unwrap()) {
                panic!("{}", err);
            }
        }));

        Ok(sig_receiver)
    }
}
