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

/// Filter used when `SCINIT_LOG` is unset, empty or ignored
const DEFAULT_FILTER: &str = "warn";

/// The level names, for the warning about a bare word that isn't one
const LEVELS: [&str; 6] = ["error", "warn", "info", "debug", "trace", "off"];

pub fn init() {
    let (directives, ignored) = filter_directives(std::env::var(LOG_ENV).ok().as_deref());
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
                .with_filter(filter.or(filter_fn(is_fatal))),
        )
        .init();

    if let Some(reason) = ignored {
        tracing::warn!("ignoring {}: {}; using {}", LOG_ENV, reason, DEFAULT_FILTER);
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

/// Errors that end scinit, which are shown whatever `SCINIT_LOG` says, so
/// exit code 1 always comes with a reason: the fatal error `main` logs
/// (the crate root's only errors) and panics (the hook above)
fn is_fatal(meta: &Metadata<'_>) -> bool {
    *meta.level() == Level::ERROR && matches!(meta.target(), "scinit" | "scinit::logging")
}

/// The filter directives to use for the `SCINIT_LOG` value `value`, and why
/// the value was ignored if it was.
///
/// Unset or empty means the default. So does a value that doesn't parse, or
/// one with a bare word that isn't a level name: `EnvFilter` reads `inf` as
/// "everything from a target named inf", which matches nothing and would
/// hide even fatal errors. A bare `scinit` is a target, not a typo.
fn filter_directives(value: Option<&str>) -> (String, Option<String>) {
    let default = || DEFAULT_FILTER.to_string();
    let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return (default(), None);
    };
    let typo = value.split(',').map(str::trim).find(|directive| {
        !directive.contains(['=', '[', ':'])
            && *directive != "scinit"
            && directive.parse::<LevelFilter>().is_err()
    });
    if let Some(word) = typo {
        let reason = format!("{:?} is not a level ({})", word, LEVELS.join(", "));
        return (default(), Some(reason));
    }
    match EnvFilter::try_new(value) {
        Ok(_) => (value.to_string(), None),
        Err(e) => (
            default(),
            Some(format!("{:?} is not a valid filter: {}", value, e)),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directives(value: Option<&str>) -> String {
        filter_directives(value).0
    }

    fn ignored(value: &str) -> bool {
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
    fn levels_and_module_directives_are_kept() {
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
            assert!(!ignored(value), "{}", value);
        }
    }

    #[test]
    fn a_bare_word_that_is_not_a_level_is_ignored() {
        for value in ["inf", "deubg", "info,deubg", "warning"] {
            assert_eq!(directives(Some(value)), "warn", "{}", value);
            assert!(ignored(value), "{}", value);
        }
    }

    #[test]
    fn an_unparseable_value_is_ignored() {
        assert!(ignored("scinit=notalevel"));
        assert_eq!(directives(Some("scinit=notalevel")), "warn");
    }
}
