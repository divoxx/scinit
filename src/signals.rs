use super::Result;

use std::pin::Pin;

use futures::stream::{select_all, Stream, StreamExt};
use tokio::signal::unix::{signal, SignalKind};
use tokio_stream::wrappers::SignalStream;

static SIGNALS: [SignalKind; 1] = [SignalKind::from_raw(2)];

pub struct Signals {
    stream: Pin<Box<dyn Stream<Item = ()>>>,
}

impl Signals {
    pub(super) fn new() -> Result<Self> {
        let signal_streams = SIGNALS
            .iter()
            .map(|s| SignalStream::new(signal(*s).unwrap()));

        Ok(Signals {
            stream: Box::pin(select_all(signal_streams)),
        })
    }

    pub(super) async fn next(&mut self) -> Option<()> {
        self.stream.next().await
    }
}
