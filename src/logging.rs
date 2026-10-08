//! scinit's own diagnostics: stderr only, one line per event, with a status
//! tag right-aligned in a 6-character column and the message at column 8:
//!
//! ```text
//! [info]  scinit: Spawning process: server []
//!   [ok]  scinit: Process spawned with PID: 42
//! [fail]  scinit: Failed to spawn process 'x': No such file or directory (os error 2)
//!  [dbg]  scinit::reaper: reaped zombie process 43 with exit status 0
//! ```
//!
//! stdout belongs to the child, so nothing scinit says goes there. Every line
//! says `scinit` right after the tag, which tells scinit's lines apart from
//! the child's even without color. The level comes from `SCINIT_LOG`, not
//! `RUST_LOG`: the child inherits the environment, and a `RUST_LOG` meant for
//! a Rust child shouldn't make scinit chatty (or the other way around).
//!
//! Everything scinit reports goes through `tracing` events, including fatal
//! errors (`main`) and panics (the hook below); `print!`-style macros are
//! denied crate-wide so nothing bypasses the format or `SCINIT_LOG`.

use std::ffi::OsString;
use std::fmt;
use std::io::IsTerminal;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Metadata, Subscriber};
use tracing_subscriber::filter::{filter_fn, FilterExt, LevelFilter};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{prelude::*, EnvFilter};

/// Logs a success milestone (a spawn, ports bound) at info level, shown
/// with the `[ok]` tag instead of `[info]`. Takes `info!`'s arguments.
macro_rules! ok {
    ($($arg:tt)+) => {
        tracing::info!(ok = true, $($arg)+)
    };
}
pub(crate) use ok;

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
                .event_format(Format)
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
/// escape codes. `NO_COLOR` turns it off everywhere (https://no-color.org),
/// and `CLICOLOR_FORCE` other than `0` turns it on off a terminal
/// (https://bixense.com/clicolors), unless `NO_COLOR` is set too
fn use_color() -> bool {
    color_wanted(
        std::io::stderr().is_terminal(),
        std::env::var_os("NO_COLOR").is_some(),
        std::env::var_os("CLICOLOR_FORCE"),
    )
}

fn color_wanted(terminal: bool, no_color: bool, clicolor_force: Option<OsString>) -> bool {
    let forced = clicolor_force.is_some_and(|v| !v.is_empty() && v != "0");
    !no_color && (terminal || forced)
}

/// Width of the tag column, which tags are right-aligned in
const TAG_WIDTH: usize = 6;

/// Between the tag column and the message, which starts at column 8
const TAG_GAP: &str = "  ";

/// The line format, see the module docs. Spans aren't shown: scinit has none.
///
/// There is no timestamp: container log drivers record one per line, and
/// tini, dumb-init and catatonit leave it out too.
struct Format;

impl<S, N> FormatEvent<S, N> for Format
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let mut fields = Fields::default();
        event.record(&mut fields);
        let meta = event.metadata();
        let ansi = writer.has_ansi_escapes();
        write_event(&mut writer, ansi, *meta.level(), meta.target(), &fields)
    }
}

/// An event's fields: the message, the `ok` marker set by [`ok!`], and the
/// others as `name=value`
#[derive(Default)]
struct Fields {
    message: String,
    ok: bool,
    others: Vec<String>,
}

impl Visit for Fields {
    fn record_bool(&mut self, field: &Field, value: bool) {
        if field.name() == "ok" {
            self.ok = value;
        } else {
            self.record_debug(field, &value);
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{:?}", value);
        } else {
            self.others.push(format!("{}={:?}", field.name(), value));
        }
    }
}

/// The styles the format uses, from the 16 basic ANSI colors
#[derive(Clone, Copy, Debug, PartialEq)]
enum Style {
    Red,
    Green,
    Yellow,
    Cyan,
    Dimmed,
}

impl Style {
    fn code(self) -> &'static str {
        match self {
            Style::Red => "\x1b[31m",
            Style::Green => "\x1b[32m",
            Style::Yellow => "\x1b[33m",
            Style::Cyan => "\x1b[36m",
            Style::Dimmed => "\x1b[2m",
        }
    }
}

/// Writes `text` in `style`, or plain without `ansi`
fn paint(out: &mut impl fmt::Write, ansi: bool, style: Style, text: &str) -> fmt::Result {
    if ansi {
        write!(out, "{}{}\x1b[0m", style.code(), text)
    } else {
        out.write_str(text)
    }
}

/// Writes `text` with every non-empty line after the first indented by
/// `indent` spaces, painting each line on its own so the style ends before
/// the newline
fn write_indented(
    out: &mut impl fmt::Write,
    ansi: bool,
    style: Option<Style>,
    indent: usize,
    text: &str,
) -> fmt::Result {
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.write_char('\n')?;
        }
        if line.is_empty() {
            continue;
        }
        if i > 0 {
            write!(out, "{:indent$}", "")?;
        }
        match style {
            Some(style) => paint(out, ansi, style, line)?,
            None => out.write_str(line)?,
        }
    }
    Ok(())
}

/// The tag for an event at `level` and its style. `ok` (from [`ok!`]) only
/// changes info events.
fn tag(level: Level, ok: bool) -> (&'static str, Style) {
    match level {
        Level::ERROR => ("[fail]", Style::Red),
        Level::WARN => ("[warn]", Style::Yellow),
        Level::INFO if ok => ("[ok]", Style::Green),
        Level::INFO => ("[info]", Style::Cyan),
        Level::DEBUG => ("[dbg]", Style::Dimmed),
        Level::TRACE => ("[trc]", Style::Dimmed),
    }
}

/// Who the line is from, which always starts with `scinit`: the module at
/// debug and trace, where it helps to pick a `SCINIT_LOG` directive, and only
/// `scinit` above. Another crate's target is always added after `scinit`,
/// since its lines would otherwise read as scinit's own.
fn source(level: Level, target: &str) -> String {
    let ours = target == "scinit" || target.starts_with("scinit::");
    if !ours {
        format!("scinit: {}", target)
    } else if matches!(level, Level::DEBUG | Level::TRACE) {
        target.to_string()
    } else {
        "scinit".to_string()
    }
}

/// Writes one event: the tag, the source (dimmed), the message and the
/// other fields (dimmed), with every line after the first continuing at the
/// message column
fn write_event(
    out: &mut impl fmt::Write,
    ansi: bool,
    level: Level,
    target: &str,
    fields: &Fields,
) -> fmt::Result {
    let (tag, style) = tag(level, fields.ok);
    write!(out, "{:1$}", "", TAG_WIDTH - tag.len())?;
    paint(out, ansi, style, tag)?;
    out.write_str(TAG_GAP)?;
    let indent = TAG_WIDTH + TAG_GAP.len();

    paint(
        out,
        ansi,
        Style::Dimmed,
        &format!("{}:", source(level, target)),
    )?;
    out.write_char(' ')?;
    write_indented(out, ansi, None, indent, &fields.message)?;
    for (i, field) in fields.others.iter().enumerate() {
        if i > 0 || !fields.message.is_empty() {
            out.write_char(' ')?;
        }
        write_indented(out, ansi, Some(Style::Dimmed), indent, field)?;
    }
    out.write_char('\n')
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

    /// `write_event`'s output for an event with these fields
    fn line(ansi: bool, level: Level, target: &str, message: &str, others: &[&str]) -> String {
        let fields = Fields {
            message: message.to_string(),
            ok: false,
            others: others.iter().map(|s| s.to_string()).collect(),
        };
        let mut out = String::new();
        write_event(&mut out, ansi, level, target, &fields).unwrap();
        out
    }

    /// Writes into a shared buffer, to read back what a subscriber wrote
    #[derive(Clone, Default)]
    struct Buffer(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(buf)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// What the events `log` emits look like through the real layer
    fn logged(ansi: bool, log: impl FnOnce()) -> String {
        let buffer = Buffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(Level::TRACE)
            .with_ansi(ansi)
            .with_writer(move || writer.clone())
            .event_format(Format)
            .finish();
        tracing::subscriber::with_default(subscriber, log);
        let bytes = buffer.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn levels_map_to_tags_right_aligned_with_the_message_at_column_8() {
        let target = "scinit";
        assert_eq!(
            line(false, Level::ERROR, target, "m", &[]),
            "[fail]  scinit: m\n"
        );
        assert_eq!(
            line(false, Level::WARN, target, "m", &[]),
            "[warn]  scinit: m\n"
        );
        assert_eq!(
            line(false, Level::INFO, target, "m", &[]),
            "[info]  scinit: m\n"
        );
        assert_eq!(
            line(false, Level::DEBUG, target, "m", &[]),
            " [dbg]  scinit: m\n"
        );
        assert_eq!(
            line(false, Level::TRACE, target, "m", &[]),
            " [trc]  scinit: m\n"
        );
        assert_eq!(tag(Level::INFO, true), ("[ok]", Style::Green));
        // Only info events become `[ok]`
        assert_eq!(tag(Level::WARN, true), ("[warn]", Style::Yellow));
    }

    #[test]
    fn the_module_is_shown_at_debug_and_trace_only() {
        let target = "scinit::file_watcher";
        assert_eq!(
            line(false, Level::INFO, target, "m", &[]),
            "[info]  scinit: m\n"
        );
        assert_eq!(
            line(false, Level::WARN, target, "m", &[]),
            "[warn]  scinit: m\n"
        );
        assert_eq!(
            line(false, Level::DEBUG, target, "m", &[]),
            " [dbg]  scinit::file_watcher: m\n"
        );
        assert_eq!(
            line(false, Level::TRACE, target, "m", &[]),
            " [trc]  scinit::file_watcher: m\n"
        );
    }

    #[test]
    fn another_crates_target_follows_scinit() {
        assert_eq!(
            line(false, Level::INFO, "tokio::runtime", "m", &[]),
            "[info]  scinit: tokio::runtime: m\n"
        );
        // A crate whose name only starts with scinit isn't scinit
        assert_eq!(
            line(false, Level::DEBUG, "scinitx", "m", &[]),
            " [dbg]  scinit: scinitx: m\n"
        );
    }

    #[test]
    fn continuation_lines_start_at_the_message_column() {
        assert_eq!(
            line(
                false,
                Level::ERROR,
                "scinit",
                "first\nsecond\n\nfourth",
                &[]
            ),
            "[fail]  scinit: first\n        second\n\n        fourth\n"
        );
    }

    #[test]
    fn other_fields_follow_the_message() {
        assert_eq!(
            line(
                false,
                Level::INFO,
                "scinit",
                "m",
                &["pid=42", "path=\"/a\""]
            ),
            "[info]  scinit: m pid=42 path=\"/a\"\n"
        );
        assert_eq!(
            line(false, Level::INFO, "scinit", "", &["pid=42"]),
            "[info]  scinit: pid=42\n"
        );
    }

    #[test]
    fn with_color_only_the_tag_is_colored_and_the_source_and_fields_dimmed() {
        assert_eq!(
            line(true, Level::INFO, "scinit", "m", &["pid=42"]),
            "\x1b[36m[info]\x1b[0m  \x1b[2mscinit:\x1b[0m m \x1b[2mpid=42\x1b[0m\n"
        );
        assert_eq!(
            line(true, Level::DEBUG, "scinit", "m", &[]),
            " \x1b[2m[dbg]\x1b[0m  \x1b[2mscinit:\x1b[0m m\n"
        );
        for (level, color) in [
            (Level::ERROR, "\x1b[31m[fail]"),
            (Level::WARN, "\x1b[33m[warn]"),
        ] {
            assert!(line(true, level, "scinit", "m", &[]).starts_with(color));
        }
        // A dimmed field spanning lines is dimmed on each, not across the indent
        assert_eq!(
            line(true, Level::INFO, "scinit", "", &["a=x\ny"]),
            "\x1b[36m[info]\x1b[0m  \x1b[2mscinit:\x1b[0m \x1b[2ma=x\x1b[0m\n        \x1b[2my\x1b[0m\n"
        );
    }

    #[test]
    fn events_go_through_the_format() {
        let out = logged(false, || {
            ok!("spawned {}", 42);
            tracing::info!(port = 8080, "bound");
            tracing::warn!(ok = false, "careful");
        });
        assert_eq!(
            out,
            "  [ok]  scinit: spawned 42\n[info]  scinit: bound port=8080\n[warn]  scinit: careful\n"
        );
        let out = logged(true, || ok!("spawned"));
        assert_eq!(
            out,
            "  \x1b[32m[ok]\x1b[0m  \x1b[2mscinit:\x1b[0m spawned\n"
        );
    }

    #[test]
    fn color_needs_a_terminal_or_clicolor_force_and_no_no_color() {
        let force = |v: &str| Some(OsString::from(v));
        assert!(color_wanted(true, false, None));
        assert!(!color_wanted(false, false, None));
        assert!(!color_wanted(true, true, None));
        assert!(color_wanted(false, false, force("1")));
        assert!(!color_wanted(false, false, force("0")));
        assert!(!color_wanted(false, false, force("")));
        assert!(!color_wanted(false, true, force("1")));
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
