//! scinit's own diagnostics: stderr only, in tracing's standard format
//! without timestamps (`LEVEL scinit::module: message`).
//!
//! stdout belongs to the child, so nothing scinit says goes there. Every line
//! names its `scinit` target, which tells scinit's lines apart from the
//! child's. The level comes from `SCINIT_LOG`, not `RUST_LOG`: the child
//! inherits the environment, and a `RUST_LOG` meant for a Rust child
//! shouldn't make scinit chatty (or the other way around).
//!
//! Everything scinit reports goes through `tracing` events, including fatal
//! errors (`main`) and panics (the hook below); `print!`-style macros are
//! denied crate-wide so nothing bypasses the format or `SCINIT_LOG`.

use std::io::IsTerminal;
use tracing_subscriber::{prelude::*, EnvFilter};

/// Environment variable holding scinit's log filter (`EnvFilter` syntax)
pub const LOG_ENV: &str = "SCINIT_LOG";

/// Filter used when `SCINIT_LOG` is unset or invalid
const DEFAULT_FILTER: &str = "error";

pub fn init() {
    let filter =
        EnvFilter::try_from_env(LOG_ENV).unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(use_color())
                // Container log drivers record a timestamp per line;
                // tini, dumb-init and catatonit leave it out too
                .without_time(),
        )
        .with(filter)
        .init();

    // Panics are reported like any other error, not by the default hook
    // writing straight to stderr
    std::panic::set_hook(Box::new(|info| {
        let location = info
            .location()
            .map(|l| format!(" at {}:{}", l.file(), l.line()))
            .unwrap_or_default();
        let message = info.payload_as_str().unwrap_or("non-string payload");
        tracing::error!("panic{}: {}", location, message);
    }));
}

/// Color only on a terminal: container logs and files would otherwise get
/// escape codes. `NO_COLOR` turns it off everywhere (https://no-color.org)
fn use_color() -> bool {
    std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}
