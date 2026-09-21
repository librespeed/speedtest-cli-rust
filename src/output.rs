//! Clean stdout/stderr separation for program output.
//!
//! Design:
//!   - stdout: machine-readable data for piping (JSON, CSV, --simple, --list, --version)
//!   - stderr: human-readable UI (progress, errors, debugging)
//!   - quiet mode: suppresses informational UI output (for --csv, --json, --simple)
//!   - debug mode: enables verbose debug output (for --debug)

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};

static DEBUG: AtomicBool = AtomicBool::new(false);
static QUIET: AtomicBool = AtomicBool::new(false);
static STREAM: AtomicBool = AtomicBool::new(false);

/// Enables or disables debug output. Called when --debug is set.
pub fn set_debug(v: bool) {
    DEBUG.store(v, Ordering::Relaxed);
}

/// Enables or disables quiet mode (suppresses `write_ui!`).
/// Called when --csv, --json, or --simple is set.
pub fn set_quiet(v: bool) {
    QUIET.store(v, Ordering::Relaxed);
}

/// Enables NDJSON progress events on stdout (--json-stream).
pub fn set_stream(v: bool) {
    STREAM.store(v, Ordering::Relaxed);
}

/// Reports whether NDJSON progress events are enabled.
pub fn is_stream() -> bool {
    STREAM.load(Ordering::Relaxed)
}

/// Emits one NDJSON event line on stdout, when --json-stream is active.
///
/// Goes through the same locked, flushed path as write_out!, so an event is
/// always a whole line: the consumer on the other side is a script reading
/// line by line, and a torn event would be worse than a missing one.
pub fn stream_event(json_line: &str) {
    if is_stream() {
        _out(format_args!("{json_line}\n"));
    }
}

/// Reports whether debug output is enabled.
pub fn is_debug() -> bool {
    DEBUG.load(Ordering::Relaxed)
}

/// Reports whether informational UI output is suppressed.
pub fn is_quiet() -> bool {
    QUIET.load(Ordering::Relaxed)
}

/// Reports whether the UI stream (stderr) is an interactive terminal.
/// Used to decide whether the spinner may animate.
pub fn ui_is_terminal() -> bool {
    std::io::stderr().is_terminal()
}

#[doc(hidden)]
pub fn _out(args: std::fmt::Arguments<'_>) {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let _ = lock.write_fmt(args);
    let _ = lock.flush();
}

#[doc(hidden)]
pub fn _ui(args: std::fmt::Arguments<'_>) {
    if is_quiet() {
        return;
    }
    _err(args);
}

#[doc(hidden)]
pub fn _err(args: std::fmt::Arguments<'_>) {
    let stderr = std::io::stderr();
    let mut lock = stderr.lock();
    let _ = lock.write_fmt(args);
    let _ = lock.flush();
}

#[doc(hidden)]
pub fn _dbg(args: std::fmt::Arguments<'_>) {
    if !is_debug() {
        return;
    }
    _err(args);
}

/// Writes formatted data to stdout.
/// Used for JSON, CSV, --simple results, --list, and --version.
/// Does NOT append a newline; the caller controls formatting.
#[macro_export]
macro_rules! write_out {
    ($($arg:tt)*) => { $crate::output::_out(format_args!($($arg)*)) };
}

/// Writes informational messages to stderr.
/// Suppressed in quiet mode (--csv, --json, --simple).
#[macro_export]
macro_rules! write_ui {
    ($($arg:tt)*) => { $crate::output::_ui(format_args!($($arg)*)) };
}

/// Writes debug messages to stderr. Only shown when --debug is set.
#[macro_export]
macro_rules! write_debug {
    ($($arg:tt)*) => { $crate::output::_dbg(format_args!($($arg)*)) };
}

/// Writes error messages to stderr. Always shown regardless of mode.
#[macro_export]
macro_rules! write_error {
    ($($arg:tt)*) => { $crate::output::_err(format_args!($($arg)*)) };
}

/// Writes a blank line to stderr.
///
/// Unlike `write_ui!`, this is NOT suppressed in quiet mode, because
/// multi-server results and --list mode need the spacing.
pub fn write_ui_blank() {
    _err(format_args!("\n"));
}

/// Renders an error chain as one line, made safe to print.
///
/// The chain can carry text that came off the wire -- a server name inside a
/// parse error, a response body -- and these lines are printed whether or not
/// --debug is set.
pub fn error_text(e: &anyhow::Error) -> String {
    sanitize(&format!("{e:#}"))
}

/// Strips control characters from a string so it can be printed without
/// letting a remote party drive the terminal.
///
/// Server names, sponsor strings and the getIP response all come off the wire
/// (over plain HTTP for schemeless servers), so they are attacker-influenced.
/// Left raw, an embedded ESC sequence can rewrite earlier lines, hide text or
/// recolour the output, and an embedded newline can forge an extra entry in
/// `--list` output that a script would then parse as real.
///
/// Dropped: C0 controls (including ESC, CR, LF and TAB), DEL, C1 controls
/// (0x80-0x9F, where 0x9B doubles as CSI on some terminals), and the Unicode
/// format characters and separators that reorder or hide text without being
/// visible themselves. Anything that actually renders is left alone.
///
/// Reports are handled separately: serde_json escapes control characters on its
/// own, and the CSV writer pairs quoting with `report::csv_text`.
pub fn sanitize(s: &str) -> String {
    s.chars().filter(|c| !is_hostile(*c)).collect()
}

/// Whether a character steers the terminal or the reader rather than rendering.
///
/// The bidi and zero-width ranges matter because the sanitized strings are what
/// the user makes a decision from: an RLO in a sponsor name renders the sponsor
/// URL next to it reversed, so a trusted-looking domain can stand in for an
/// attacker's, and a separator or zero-width joiner can make one `--list` entry
/// read as another server's.
fn is_hostile(c: char) -> bool {
    let c = c as u32;
    // C0 controls, DEL and C1 controls.
    c < 0x20 || c == 0x7f || (0x80..=0x9f).contains(&c)
        // Zero-width space/joiners and the bidi marks.
        || (0x200b..=0x200f).contains(&c)
        // Line and paragraph separators.
        || c == 0x2028 || c == 0x2029
        // Bidi embeddings, overrides and the pop directional formatter.
        || (0x202a..=0x202e).contains(&c)
        // Word joiner, invisible operators, bidi isolates, deprecated formatters.
        || (0x2060..=0x206f).contains(&c)
        // Interlinear annotation, which can hide text outright.
        || (0xfff9..=0xfffb).contains(&c)
        // Zero-width no-break space, also the BOM.
        || c == 0xfeff
}

/// Writes a message to stderr with a trailing newline and exits with code 1.
pub fn fatal(msg: impl std::fmt::Display) -> ! {
    _err(format_args!("{msg}\n"));
    std::process::exit(1)
}

#[cfg(test)]
mod tests {
    use super::sanitize;

    #[test]
    fn strips_ansi_escape_sequences() {
        assert_eq!(sanitize("\x1b[31mred\x1b[0m"), "[31mred[0m");
    }

    #[test]
    fn strips_newlines_that_could_forge_a_list_entry() {
        assert_eq!(
            sanitize("Real\n999: Fake (http://evil)"),
            "Real999: Fake (http://evil)"
        );
        assert_eq!(sanitize("a\rb\tc"), "abc");
    }

    #[test]
    fn strips_del_and_c1_controls() {
        assert_eq!(sanitize("a\x7fb\u{9b}c"), "abc");
    }

    #[test]
    fn leaves_printable_unicode_alone() {
        assert_eq!(
            sanitize("Praha, Česko (CESNET) — 100%"),
            "Praha, Česko (CESNET) — 100%"
        );
    }

    #[test]
    fn strips_bidi_overrides_that_could_reverse_a_sponsor_url() {
        // RLO would render the URL that follows the name right-to-left.
        assert_eq!(sanitize("Sponsor\u{202e}moc.live"), "Sponsormoc.live");
        assert_eq!(sanitize("a\u{202a}b\u{202c}c\u{2066}d\u{2069}e"), "abcde");
    }

    #[test]
    fn strips_zero_width_and_separator_characters() {
        assert_eq!(sanitize("ev\u{200b}il.com"), "evil.com");
        assert_eq!(sanitize("a\u{2028}b\u{2029}c"), "abc");
        assert_eq!(sanitize("\u{feff}CESNET"), "CESNET");
        assert_eq!(sanitize("a\u{fff9}b\u{fffb}c"), "abc");
    }
}
