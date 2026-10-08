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
use tracing::{Level, Metadata};
use tracing_subscriber::filter::{filter_fn, FilterExt, LevelFilter};
use tracing_subscriber::{prelude::*, EnvFilter};

/// Environment variable holding scinit's log filter (`EnvFilter` syntax)
pub const LOG_ENV: &str = "SCINIT_LOG";

/// Filter used when `SCINIT_LOG` is unset or can't be parsed
const DEFAULT_FILTER: &str = "warn";

/// The level names, for the warning about a bare word that isn't one
const LEVELS: [&str; 6] = ["error", "warn", "info", "debug", "trace", "off"];

pub fn init() {
    let (directives, warning) = filter_directives(std::env::var(LOG_ENV).ok().as_deref());
    // `filter_directives` only passes on values that parse
    let filter = EnvFilter::try_new(&directives).unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(use_color())
                // Container log drivers record a timestamp per line;
                // tini, dumb-init and catatonit leave it out too
                .without_time()
                .with_filter(filter.or(filter_fn(always_shown))),
        )
        .init();

    if let Some(warning) = warning {
        tracing::warn!("{}", warning);
    }

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

/// Events shown whatever `SCINIT_LOG` says: the fatal error `main` logs (the
/// crate root's only errors), so exit code 1 always comes with a reason, and
/// everything from this module, which is panics (the hook above) and the
/// warning about `SCINIT_LOG` itself, which its own filter could hide
fn always_shown(meta: &Metadata<'_>) -> bool {
    (meta.target() == "scinit" && *meta.level() == Level::ERROR) || meta.target() == module_path!()
}

/// The filter directives to use for the `SCINIT_LOG` value `value`, and a
/// warning about the value if it needs one.
///
/// Unset means the default, and so does a value that doesn't parse. A bare
/// word that isn't a level name is kept, since it may name a target on
/// purpose, but gets a warning: `EnvFilter` reads a typo like `inf` as
/// "everything from a target named inf", which matches nothing. A bare
/// `scinit` is clearly a target and gets none.
fn filter_directives(value: Option<&str>) -> (String, Option<String>) {
    let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return (DEFAULT_FILTER.to_string(), None);
    };
    if let Err(e) = EnvFilter::try_new(value) {
        let warning = format!(
            "ignoring {}={:?}, which is not a valid filter ({}); using {}",
            LOG_ENV, value, e, DEFAULT_FILTER
        );
        return (DEFAULT_FILTER.to_string(), Some(warning));
    }
    let not_a_level = value.split(',').map(str::trim).find(|directive| {
        !directive.contains(['=', '[', ':'])
            && *directive != "scinit"
            && directive.parse::<LevelFilter>().is_err()
    });
    let warning = not_a_level.map(|word| {
        format!(
            "{}: {:?} is not a level ({}), so it selects the target named {:?}",
            LOG_ENV,
            word,
            LEVELS.join(", "),
            word
        )
    });
    (value.to_string(), warning)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directives(value: Option<&str>) -> String {
        filter_directives(value).0
    }

    fn warns(value: &str) -> bool {
        filter_directives(Some(value)).1.is_some()
    }

    #[test]
    fn unset_or_empty_is_the_default_without_a_warning() {
        for value in [None, Some(""), Some("  ")] {
            assert_eq!(
                filter_directives(value),
                ("warn".to_string(), None),
                "{:?}",
                value
            );
        }
    }

    #[test]
    fn levels_and_module_directives_are_kept_without_a_warning() {
        for value in [
            "info",
            "DEBUG",
            "off",
            "info,scinit::file_watcher=debug",
            "scinit::reaper=debug",
            "scinit=trace",
            "scinit",
            "scinit::reaper",
        ] {
            assert_eq!(directives(Some(value)), value);
            assert!(!warns(value), "{}", value);
        }
    }

    #[test]
    fn a_bare_word_that_is_not_a_level_is_kept_with_a_warning() {
        for value in ["inf", "deubg", "info,deubg", "tokio"] {
            assert_eq!(directives(Some(value)), value);
            assert!(warns(value), "{}", value);
        }
    }

    #[test]
    fn an_unparseable_value_is_replaced_by_the_default_with_a_warning() {
        assert_eq!(directives(Some("scinit=notalevel")), "warn");
        assert!(warns("scinit=notalevel"));
    }
}
