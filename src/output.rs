//! Clean stdout/stderr separation for program output.
//!
//! Design:
//!   - stdout: machine-readable data for piping (JSON, CSV, --simple, --list, --version)
//!   - stderr: human-readable UI (progress, errors, debugging)
//!   - quiet mode: suppresses informational UI output (for --csv, --json, --simple)
//!   - debug mode: enables verbose debug output (for --debug)
//!
//! Which of these a run wants is an `Output` value the caller passes down,
//! not process-wide state.

use std::io::{IsTerminal, Write};

/// What a run prints: which of its messages are written, and where.
///
/// Carried by value from the caller that decided it down to every line that
/// prints, rather than kept in statics. This crate is also a library, so a
/// second run in the same process used to inherit whatever the first one set
/// and never cleared.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Output {
    /// Verbose diagnostics on stderr (--debug).
    pub debug: bool,
    /// Informational UI suppressed (--csv, --json, --json-stream, --simple).
    pub quiet: bool,
    /// NDJSON progress events on stdout (--json-stream).
    pub stream: bool,
}

impl Output {
    /// Emits one NDJSON event line on stdout, when --json-stream is active.
    ///
    /// Goes through the same locked, flushed path as write_out!, so an event is
    /// always a whole line: the consumer on the other side is a script reading
    /// line by line, and a torn event would be worse than a missing one.
    pub fn stream_event(&self, json_line: &str) {
        if self.stream {
            _out(format_args!("{json_line}\n"));
        }
    }

    #[doc(hidden)]
    pub fn _ui(&self, args: std::fmt::Arguments<'_>) {
        if self.quiet {
            return;
        }
        _err(args);
    }

    #[doc(hidden)]
    pub fn _dbg(&self, args: std::fmt::Arguments<'_>) {
        if !self.debug {
            return;
        }
        _err(args);
    }
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
pub fn _err(args: std::fmt::Arguments<'_>) {
    let stderr = std::io::stderr();
    let mut lock = stderr.lock();
    let _ = lock.write_fmt(args);
    let _ = lock.flush();
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
    ($out:expr, $($arg:tt)*) => {
        $crate::output::Output::_ui(&$out, format_args!($($arg)*))
    };
}

/// Writes debug messages to stderr. Only shown when --debug is set.
#[macro_export]
macro_rules! write_debug {
    ($out:expr, $($arg:tt)*) => {
        $crate::output::Output::_dbg(&$out, format_args!($($arg)*))
    };
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

/// Strips the characters that let a remote party drive the terminal or forge a
/// line, so server-supplied text can be printed as text.
///
/// Server names, sponsor strings and the getIP response all come off the wire
/// (over plain HTTP for schemeless servers), so they are attacker-influenced.
/// Left raw, an embedded ESC sequence can rewrite earlier lines, hide text or
/// recolour the output, and an embedded newline can forge an extra entry in
/// `--list` output that a script would then parse as real.
///
/// Dropped: C0 controls (including ESC, CR, LF and TAB), DEL, C1 controls
/// (0x80-0x9F, where 0x9B doubles as CSI on some terminals), the line and
/// paragraph separators, every Unicode format character (general category Cf:
/// the bidi controls, the zero-width joiners, the byte order mark, the Arabic
/// letter mark, the soft hyphen and the rest), and the whole tag block
/// (U+E0000-U+E007F), whose characters are invisible and can carry a hidden
/// copy of ASCII text through anything that displays the result.
///
/// Kept: everything that renders, including no-break space and the other
/// spacing characters. A space renders as a space: it cannot reorder, hide or
/// forge anything, it occurs in real server and ISP names, and dropping it
/// would run words together -- turning a display problem into a wrong name.
///
/// This is the filter for every output this client produces, terminal, CSV and
/// JSON alike; the Go client applies its own, narrower one (C0, DEL and C1) to
/// terminal output only.
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
        // Line and paragraph separators (Zl, Zp).
        || c == 0x2028 || c == 0x2029
        // The tag block, whole: a full invisible ASCII alphabet, and what is
        // still unassigned in it is reserved as ignorable.
        || (0xe0000..=0xe007f).contains(&c)
        || is_format_char(c)
}

/// Whether a code point is a Unicode format character (general category Cf).
///
/// Held as a table rather than pulled from a Unicode crate. It is the whole of
/// Cf as of Unicode 17, and one code point more: U+2065, the unassigned gap in
/// the run of invisible operators, which Unicode reserves as ignorable, so
/// whatever is assigned there is covered rather than missed.
///
/// Cf does grow: Unicode 11, 12, 14 and 15 each added to it. The tests walk
/// every scalar value and compare this table with the Unicode data in
/// icu_properties, so an update of that crate to a Unicode version with a new
/// format character, or with U+2065 assigned, fails there and says which.
fn is_format_char(c: u32) -> bool {
    const FORMAT: &[(u32, u32)] = &[
        (0x00ad, 0x00ad), // soft hyphen
        (0x0600, 0x0605), // Arabic number signs
        (0x061c, 0x061c), // Arabic letter mark
        (0x06dd, 0x06dd),
        (0x070f, 0x070f),
        (0x0890, 0x0891),
        (0x08e2, 0x08e2),
        (0x180e, 0x180e), // Mongolian vowel separator
        (0x200b, 0x200f), // zero width space/joiners, LRM, RLM
        (0x202a, 0x202e), // bidi embeddings and overrides
        (0x2060, 0x206f), // word joiner, invisible operators, bidi isolates
        (0xfeff, 0xfeff), // zero width no-break space, also the BOM
        (0xfff9, 0xfffb), // interlinear annotation
        (0x110bd, 0x110bd),
        (0x110cd, 0x110cd),
        (0x13430, 0x1343f), // Egyptian hieroglyph format controls
        (0x1bca0, 0x1bca3),
        (0x1d173, 0x1d17a), // musical formatting
        (0xe0001, 0xe0001), // language tag (also inside the tag block above)
        (0xe0020, 0xe007f), // tag characters (likewise)
    ];
    FORMAT
        .binary_search_by(|&(lo, hi)| {
            if c < lo {
                std::cmp::Ordering::Greater
            } else if c > hi {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// Writes a message to stderr with a trailing newline and exits with code 1.
pub fn fatal(msg: impl std::fmt::Display) -> ! {
    _err(format_args!("{msg}\n"));
    std::process::exit(1)
}

#[cfg(test)]
mod tests {
    use super::{is_format_char, is_hostile, sanitize};
    use icu_properties::props::{DefaultIgnorableCodePoint, GeneralCategory as Gc};
    use icu_properties::{CodePointMapData, CodePointSetData};

    /// Every Unicode scalar value with its General_Category, taken from the
    /// Unicode Character Database compiled into icu_properties and from
    /// nothing in this file.
    fn scalars() -> impl Iterator<Item = (char, Gc)> {
        let categories = CodePointMapData::<Gc>::new();
        ('\0'..=char::MAX).map(move |c| (c, categories.get(c)))
    }

    /// The categories sanitize() promises to remove: Cc, Cf, Zl and Zp.
    fn must_go(gc: Gc) -> bool {
        matches!(
            gc,
            Gc::Control | Gc::Format | Gc::LineSeparator | Gc::ParagraphSeparator
        )
    }

    fn code_points(chars: &[char]) -> String {
        let list: Vec<String> = chars
            .iter()
            .map(|c| format!("U+{:04X}", *c as u32))
            .collect();
        list.join(" ")
    }

    // The table in is_format_char() is written by hand, so it is held here to
    // Unicode data over every scalar value, not to a second hand-written list.
    #[test]
    fn every_control_format_and_separator_character_is_hostile() {
        let missed: Vec<char> = scalars()
            .filter(|&(c, gc)| must_go(gc) && !is_hostile(c))
            .map(|(c, _)| c)
            .collect();
        assert!(
            missed.is_empty(),
            "in Cc, Cf, Zl or Zp but not stripped: {}",
            code_points(&missed)
        );
    }

    // The other direction: what is stripped without being in one of the four
    // categories. It is the gap inside the invisible operators and the unused
    // part of the tag block, all of them unassigned and reserved for characters
    // that do not render. The set is pinned, so Unicode data that assigns one
    // of them, and anything added to is_hostile() that renders (a space, say,
    // which is category Zs), fails here.
    #[test]
    fn the_only_other_hostile_code_points_are_reserved_and_invisible() {
        let mut reserved = vec!['\u{2065}', '\u{e0000}'];
        reserved.extend('\u{e0002}'..='\u{e001f}');

        let extra: Vec<(char, Gc)> = scalars()
            .filter(|&(c, gc)| is_hostile(c) && !must_go(gc))
            .collect();
        let stripped: Vec<char> = extra.iter().map(|&(c, _)| c).collect();
        assert_eq!(
            stripped,
            reserved,
            "stripped outside Cc, Cf, Zl and Zp: {}",
            code_points(&stripped)
        );

        let ignorable = CodePointSetData::new::<DefaultIgnorableCodePoint>();
        for (c, gc) in extra {
            assert_eq!(gc, Gc::Unassigned, "U+{:04X}", c as u32);
            assert!(ignorable.contains(c), "U+{:04X}", c as u32);
        }
    }

    // is_format_char() says it is category Cf, so it is held to that on its
    // own as well, apart from the one reserved code point it adds on purpose.
    #[test]
    fn the_format_table_is_general_category_cf_and_the_gap_in_it() {
        let wrong: Vec<char> = scalars()
            .filter(|&(c, gc)| is_format_char(c as u32) != (gc == Gc::Format || c == '\u{2065}'))
            .map(|(c, _)| c)
            .collect();
        assert!(
            wrong.is_empty(),
            "table and Unicode data disagree on: {}",
            code_points(&wrong)
        );
    }

    // The tests above prove the table against whatever Unicode version
    // icu_properties carries, and the comment on is_format_char() names one.
    // U+20C1 SAUDI RIYAL SIGN was first assigned in Unicode 17.0.
    #[test]
    fn the_unicode_data_is_at_least_version_17() {
        let categories = CodePointMapData::<Gc>::new();
        assert_eq!(categories.get('\u{20c1}'), Gc::CurrencySymbol);
    }

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
        // The Arabic letter mark does the same job as RLM.
        assert_eq!(sanitize("a\u{61c}b"), "ab");
    }

    #[test]
    fn strips_zero_width_and_separator_characters() {
        assert_eq!(sanitize("ev\u{200b}il.com"), "evil.com");
        assert_eq!(sanitize("a\u{2028}b\u{2029}c"), "abc");
        assert_eq!(sanitize("\u{feff}CESNET"), "CESNET");
        assert_eq!(sanitize("a\u{fff9}b\u{fffb}c"), "abc");
    }

    // Characters that render as nothing but are not controls: a soft hyphen
    // can hide a word boundary, the Mongolian vowel separator is invisible,
    // and a tag sequence spells out ASCII that no reader can see.
    #[test]
    fn strips_invisible_formatting_and_tag_characters() {
        assert_eq!(sanitize("ev\u{ad}il.com"), "evil.com");
        assert_eq!(sanitize("a\u{180e}b"), "ab");
        assert_eq!(sanitize("CESNET\u{e0041}\u{e0042}\u{e007f}"), "CESNET");
        assert_eq!(sanitize("a\u{e0000}b\u{e0020}c"), "abc");
        assert_eq!(sanitize("\u{600}1\u{6dd}"), "1");
    }

    // A no-break space renders; removing it would join two words into one and
    // misreport a name that legitimately contains it.
    #[test]
    fn keeps_spacing_characters_that_render() {
        assert_eq!(sanitize("Paris\u{a0}: CESNET"), "Paris\u{a0}: CESNET");
        assert_eq!(sanitize("a\u{2009}b\u{3000}c"), "a\u{2009}b\u{3000}c");
    }

    #[test]
    fn sanitizing_twice_changes_nothing_more() {
        let once = sanitize("a\x1b[0m\u{202e}b\u{e0041}\u{ad}");
        assert_eq!(sanitize(&once), once);
    }
}
