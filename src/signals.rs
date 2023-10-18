use super::Result;

pub use nix::sys::signal::Signal;

use std::pin::Pin;

use async_stream::stream;
use futures::stream::{Stream, StreamExt};
use nix::libc::{sigfillset, sigprocmask, sigset_t, sigwait};
use std::mem::MaybeUninit;
use std::ptr;

pub(super) struct Signals {
    stream: Pin<Box<dyn Stream<Item = Signal>>>,
}

impl Signals {
    pub(super) fn new() -> Result<Self> {
        Ok(Signals {
            stream: Box::pin(stream! {
                let sigmask: sigset_t = unsafe {
                    let mut masku = MaybeUninit::<sigset_t>::uninit();
                    sigfillset(masku.as_mut_ptr());
                    masku.assume_init()
                };

                unsafe { sigprocmask(0, &sigmask, ptr::null_mut()) };

                loop {
                    let mut s: i32 = 0; 
                    unsafe { sigwait(&sigmask, &mut s) };
                    yield Signal::try_from(s).unwrap();
                }
            }),
        })
    }

    pub(super) async fn next(&mut self) -> Option<Signal> {
        self.stream.next().await
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn it_captures_a_single_signal() {}
}
