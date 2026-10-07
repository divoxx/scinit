//! scinit's own diagnostics: stderr only, as `[scinit] LEVEL message`.
//!
//! stdout belongs to the child, so nothing scinit says goes there. The level
//! comes from `SCINIT_LOG`, not `RUST_LOG`: the child inherits the
//! environment, and a `RUST_LOG` meant for a Rust child shouldn't make scinit
//! chatty (or the other way around).

use std::fmt;
use std::io::IsTerminal;
use tracing::{Event, Subscriber};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;
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
                .event_format(Prefixed),
        )
        .with(filter)
        .init();
}

/// Color only on a terminal: container logs and files would otherwise get
/// escape codes. `NO_COLOR` turns it off everywhere (https://no-color.org)
fn use_color() -> bool {
    std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

/// `[scinit] LEVEL message fields`, with no timestamp: container runtimes
/// already timestamp every line
struct Prefixed;

impl<S, N> FormatEvent<S, N> for Prefixed
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        write!(writer, "[scinit] {} ", event.metadata().level())?;
        ctx.field_format().format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}
