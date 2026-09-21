//! Replaces what legitimately varies between two runs with placeholders, and
//! nothing else.
//!
//! A value is only replaced when it has the shape the Go client prints, so a
//! rate written as `1e+06` or `0.10`, or a byte count with a decimal point,
//! stays as it is and shows up as a difference.
//!
//! | Placeholder | Stands for |
//! | --- | --- |
//! | `<FIX>` | the directory holding the server lists |
//! | `<LIVE>`, `<DEAD>`, `<HOSTILE>`, `<GARBLED>` | a fixture's port |
//! | `<TS>` | an RFC 3339 timestamp |
//! | `<NUM>` | a measured number in JSON or CSV |
//! | `<F2>` | a measured number with two decimals, before `ms` or `Mbps` |
//! | `<DUR>` | how long a phase took, in Go's duration notation |
//! | `<N>` | a byte count in a `--debug` line |
//! | `<ERRNO>` | the number in `(os error N)` |
//!
//! Progress events that are identical once normalized are collapsed into
//! one, because how many a run emits depends on how long it took.

/// What is known about one run of the harness.
pub struct Context {
    /// The directory the server lists were written to, as passed to the clients.
    pub fix: String,
    /// Each fixture's port and the placeholder it is shown as.
    pub ports: Vec<(u16, &'static str)>,
}

/// Decodes a client's output without losing anything: a byte that is not
/// UTF-8 becomes `<XX>`, so two different invalid bytes stay different.
pub fn decode(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.utf8_chunks() {
        out.push_str(chunk.valid());
        for byte in chunk.invalid() {
            out.push_str(&format!("<{byte:02X}>"));
        }
    }
    out
}

/// Normalizes a whole stream and splits it into lines. A last line with no
/// line break is marked, so a missing final newline is a difference.
pub fn normalize(context: &Context, raw: &str) -> Vec<String> {
    let mut text = raw.replace(&context.fix, "<FIX>");
    text = text.replace("<FIX>\\", "<FIX>/");
    for (port, name) in &context.ports {
        text = replace_port(&text, *port, name);
    }

    let mut lines: Vec<String> = Vec::new();
    let mut rest = text.as_str();
    while !rest.is_empty() {
        let (line, tail, terminated) = match rest.split_once('\n') {
            Some((line, tail)) => (line, tail, true),
            None => (rest, "", false),
        };
        let mut line = normalize_line(line);
        if !terminated {
            line.push_str("<NO NEWLINE>");
        }
        let repeated_progress =
            line.starts_with(r#"{"event":"progress""#) && lines.last() == Some(&line);
        if !repeated_progress {
            lines.push(line);
        }
        rest = tail;
    }
    lines
}

fn replace_port(text: &str, port: u16, name: &str) -> String {
    let needle = format!(":{port}");
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find(&needle) {
        let after = &rest[at + needle.len()..];
        out.push_str(&rest[..at]);
        if after.starts_with(|c: char| c.is_ascii_digit()) {
            out.push_str(&needle);
        } else {
            out.push_str(&format!(":<{name}>"));
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

fn normalize_line(line: &str) -> String {
    let line = replace_timestamps(line);
    let line = csv_row(&line).unwrap_or(line);
    let line = json_numbers(&line);
    let line = two_decimals(&line);
    let line = between(&line, "finished in ", ":", is_go_duration, "<DUR>");
    let line = before(&line, " byte(s)", "<N>");
    between(&line, "(os error ", ")", is_integer, "<ERRNO>")
}

fn digits(text: &str) -> usize {
    text.bytes().take_while(u8::is_ascii_digit).count()
}

/// The length of an RFC 3339 timestamp at the start of `text`, if there is one.
fn timestamp_len(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut at = 0;
    for (count, separator) in [(4, "-"), (2, "-"), (2, "T"), (2, ":"), (2, ":"), (2, "")] {
        if digits(&text[at..]) < count || !text[at + count..].starts_with(separator) {
            return None;
        }
        at += count + separator.len();
    }
    if bytes.get(at) == Some(&b'.') {
        let fraction = digits(&text[at + 1..]);
        if fraction == 0 || fraction > 9 {
            return None;
        }
        at += 1 + fraction;
    }
    match bytes.get(at) {
        Some(b'Z') => Some(at + 1),
        Some(b'+' | b'-') => {
            let zone = &text[at + 1..];
            let shaped = digits(zone) == 2 && zone[2..].starts_with(':') && digits(&zone[3..]) == 2;
            shaped.then_some(at + 6)
        }
        _ => None,
    }
}

fn replace_timestamps(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(first) = rest.chars().next() {
        match timestamp_len(rest) {
            Some(len) if first.is_ascii_digit() => {
                out.push_str("<TS>");
                rest = &rest[len..];
            }
            _ => {
                out.push(first);
                rest = &rest[first.len_utf8()..];
            }
        }
    }
    out
}

/// Whether `text` is a number as Go prints one that was rounded to two
/// decimals: no exponent, no leading zeros, no trailing zeros in the fraction.
fn is_go_rounded(text: &str) -> bool {
    let unsigned = text.strip_prefix('-').unwrap_or(text);
    let (whole, fraction) = match unsigned.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (unsigned, None),
    };
    is_integer(whole)
        && fraction
            .is_none_or(|f| (1..=2).contains(&f.len()) && digits(f) == f.len() && !f.ends_with('0'))
}

fn is_integer(text: &str) -> bool {
    !text.is_empty() && digits(text) == text.len() && (text == "0" || !text.starts_with('0'))
}

/// A CSV row starts with its timestamp and ends with ping, jitter, download,
/// upload, share link and address. The name and the URL before them may hold
/// the delimiter, so the fields are counted from the right.
fn csv_row(line: &str) -> Option<String> {
    let after = line.strip_prefix("<TS>")?;
    let delimiter = after.chars().next()?;
    if delimiter.is_alphanumeric() || delimiter == '"' {
        return None;
    }
    let mut fields: Vec<&str> = line.rsplitn(7, delimiter).collect();
    if fields.len() != 7 {
        return None;
    }
    fields.reverse();
    let mut row = vec![fields[0].to_string()];
    for measured in &fields[1..5] {
        row.push(if is_go_rounded(measured) {
            "<NUM>".to_string()
        } else {
            measured.to_string()
        });
    }
    row.push(fields[5].to_string());
    row.push(fields[6].to_string());
    Some(row.join(&delimiter.to_string()))
}

/// The measured members of a report and of a progress event. `seconds` is
/// measured, not whole: a tick a tenth of a second late prints 1.1 where an
/// idle machine prints 1.
fn json_numbers(line: &str) -> String {
    const WHOLE: [&str; 3] = ["bytes_sent", "bytes_received", "progress"];
    const ROUNDED: [&str; 6] = ["ping", "jitter", "upload", "download", "mbps", "seconds"];
    let mut line = line.to_string();
    for key in WHOLE {
        line = json_member(&line, key, is_integer);
    }
    for key in ROUNDED {
        line = json_member(&line, key, is_go_rounded);
    }
    line
}

fn json_member(line: &str, key: &str, shaped: fn(&str) -> bool) -> String {
    let needle = format!("\"{key}\":");
    let mut out = String::new();
    let mut rest = line;
    while let Some(at) = rest.find(&needle) {
        let value_at = at + needle.len();
        let value = &rest[value_at..];
        let len = value.find([',', '}']).unwrap_or(value.len());
        out.push_str(&rest[..value_at]);
        if shaped(&value[..len]) {
            out.push_str("<NUM>");
        } else {
            out.push_str(&value[..len]);
        }
        rest = &value[len..];
    }
    out.push_str(rest);
    out
}

/// `0.61 ms` and `41224.32 Mbps`: exactly two decimals, as `%.2f` prints.
fn two_decimals(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    let mut in_number = false;
    while let Some(first) = rest.chars().next() {
        let whole = digits(rest);
        let shaped = !in_number
            && whole > 0
            && rest[whole..].starts_with('.')
            && digits(&rest[whole + 1..]) == 2
            && [" ms", " Mbps"].iter().any(|unit| {
                rest[whole + 3..]
                    .strip_prefix(unit)
                    .is_some_and(|after| !after.starts_with(|c: char| c.is_alphanumeric()))
            });
        if shaped {
            out.push_str("<F2>");
            rest = &rest[whole + 3..];
            in_number = false;
        } else {
            out.push(first);
            in_number = first.is_ascii_digit() || first == '.';
            rest = &rest[first.len_utf8()..];
        }
    }
    out
}

/// Go's `time.Duration` notation: `603ms`, `2.603s`, `1m2.5s`, `0s`.
fn is_go_duration(text: &str) -> bool {
    let mut rest = text;
    if rest.is_empty() {
        return false;
    }
    while !rest.is_empty() {
        let whole = digits(rest);
        if whole == 0 {
            return false;
        }
        rest = &rest[whole..];
        if let Some(fraction) = rest.strip_prefix('.') {
            let len = digits(fraction);
            if len == 0 {
                return false;
            }
            rest = &fraction[len..];
        }
        let Some(unit) = ["ns", "us", "µs", "ms", "h", "m", "s"]
            .iter()
            .find(|unit| rest.starts_with(**unit))
        else {
            return false;
        };
        rest = &rest[unit.len()..];
    }
    true
}

/// Replaces what stands between each `open` and the next `close`, if shaped.
fn between(
    line: &str,
    open: &str,
    close: &str,
    shaped: fn(&str) -> bool,
    placeholder: &str,
) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(at) = rest.find(open) {
        let value_at = at + open.len();
        out.push_str(&rest[..value_at]);
        rest = &rest[value_at..];
        if let Some(len) = rest.find(close) {
            if shaped(&rest[..len]) {
                out.push_str(placeholder);
                rest = &rest[len..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Replaces the integer, a word of its own, that ends right before each `suffix`.
fn before(line: &str, suffix: &str, placeholder: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(at) = rest.find(suffix) {
        let head = &rest[..at];
        let number_at = head.len() - head.bytes().rev().take_while(u8::is_ascii_digit).count();
        let starts_word = head[..number_at].ends_with(' ') || number_at == 0;
        if starts_word && is_integer(&head[number_at..]) {
            out.push_str(&head[..number_at]);
            out.push_str(placeholder);
        } else {
            out.push_str(head);
        }
        out.push_str(suffix);
        rest = &rest[at + suffix.len()..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> Context {
        Context {
            fix: "/tmp/parity-1".to_string(),
            ports: vec![(18080, "LIVE"), (1808, "DEAD")],
        }
    }

    fn one(line: &str) -> String {
        let lines = normalize(&context(), &format!("{line}\n"));
        assert_eq!(lines.len(), 1, "{lines:?}");
        lines.into_iter().next().unwrap()
    }

    #[test]
    fn a_json_report_keeps_everything_but_the_measurements() {
        assert_eq!(
            one(
                r#"[{"timestamp":"2026-09-20T09:07:09.090905+02:00","server":{"name":"Fixture Live","url":"http://127.0.0.1:18080/"},"client":{"ip":"127.0.0.1"},"bytes_sent":14484701184,"bytes_received":0,"ping":0.06,"jitter":0,"upload":44536.7,"download":68285.34,"share":""}]"#
            ),
            r#"[{"timestamp":"<TS>","server":{"name":"Fixture Live","url":"http://127.0.0.1:<LIVE>/"},"client":{"ip":"127.0.0.1"},"bytes_sent":<NUM>,"bytes_received":<NUM>,"ping":<NUM>,"jitter":<NUM>,"upload":<NUM>,"download":<NUM>,"share":""}]"#
        );
    }

    #[test]
    fn a_number_go_would_not_print_is_left_alone() {
        for odd in [
            "1e+06", "0.10", "1.0", "01", "0.123", ".5", "1.", "-", "NaN", "null",
        ] {
            let line = format!(r#"{{"ping":{odd},"bytes_sent":{odd}}}"#);
            assert_eq!(one(&line), line, "{odd}");
            let row = format!("2026-09-20T09:06:47Z,Name,http://x/,{odd},0.01,5.5,7,,127.0.0.1");
            assert_eq!(
                one(&row),
                format!("<TS>,Name,http://x/,{odd},<NUM>,<NUM>,<NUM>,,127.0.0.1")
            );
        }
        // A byte count is whole.
        assert_eq!(one(r#"{"bytes_sent":1.5}"#), r#"{"bytes_sent":1.5}"#);
        assert_eq!(one(r#"{"ping":-0.5}"#), r#"{"ping":<NUM>}"#);
    }

    #[test]
    fn a_csv_row_is_read_from_the_right() {
        assert_eq!(
            one(
                r#"2026-09-20T09:06:47.603408+02:00;"Quote""d;semi,comma";http://127.0.0.1:18080;0.07;0.01;53406.57;7398.33;;127.0.0.1"#
            ),
            r#"<TS>;"Quote""d;semi,comma";http://127.0.0.1:<LIVE>;<NUM>;<NUM>;<NUM>;<NUM>;;127.0.0.1"#
        );
        // Not a row: too few fields after the timestamp.
        assert_eq!(one("2026-09-20T09:06:47Z,1,2"), "<TS>,1,2");
    }

    #[test]
    fn text_output_loses_only_its_measurements() {
        assert_eq!(
            one("Ping:\t0.61 ms\tJitter:\t0.34 ms"),
            "Ping:\t<F2> ms\tJitter:\t<F2> ms"
        );
        assert_eq!(
            one("Download rate:\t41224.32 Mbps"),
            "Download rate:\t<F2> Mbps"
        );
        assert_eq!(
            one("Download test finished in 2.603s: 74682.92 Mbps, 24296659172 byte(s) received"),
            "Download test finished in <DUR>: <F2> Mbps, <N> byte(s) received"
        );
        assert_eq!(
            one("Ping test finished in 1ms: ping 0.07 ms, jitter 0.01 ms"),
            "Ping test finished in <DUR>: ping <F2> ms, jitter <F2> ms"
        );
        // What the options decided is not a measurement.
        let fixed = "Download test starting: 3 stream(s), 100 chunk(s), up to 2s";
        assert_eq!(one(fixed), fixed);
        let fixed = "Upload test starting: 3 stream(s), 1024 KiB per request, up to 0s";
        assert_eq!(one(fixed), fixed);
    }

    #[test]
    fn text_output_in_another_shape_is_left_alone() {
        for odd in [
            "Ping:\t0.6 ms",
            "Ping:\t0.610 ms",
            "Ping:\t1e3 ms",
            "Download rate:\t1.00 Mbpsx",
            "Download rate:\t1.00 MB/s",
            "Download test finished in 2.6 seconds: x",
            "Download test finished in fast: x",
            "sent 1.5 byte(s)",
            "sent 007 byte(s)",
        ] {
            let kept = one(odd);
            assert!(
                !kept.contains("<F2>") && !kept.contains("<DUR>") && !kept.contains("<N>"),
                "{odd:?} became {kept:?}"
            );
        }
    }

    #[test]
    fn durations_are_gos() {
        for shaped in [
            "0s", "1ms", "603ms", "2.603s", "1m2.5s", "1h0m0s", "15µs", "200ns",
        ] {
            assert!(is_go_duration(shaped), "{shaped}");
        }
        for odd in ["", "1", "s", "1.s", "1 s", "2.6 seconds", "1sec", "-1s"] {
            assert!(!is_go_duration(odd), "{odd}");
        }
    }

    #[test]
    fn timestamps_are_rfc_3339() {
        for shaped in [
            "2026-09-20T09:07:09Z",
            "2026-09-20T09:07:09.5Z",
            "2026-09-20T09:07:09.090905+02:00",
            "2026-09-20T09:07:09.123456789-11:30",
        ] {
            assert_eq!(one(&format!("[{shaped}]")), "[<TS>]", "{shaped}");
        }
        for odd in [
            "2026-09-20 09:07:09Z",
            "2026-09-20T09:07:09",
            "2026-09-20T09:07:09.Z",
            "2026-09-20T09:07:09+0200",
            "2026-9-20T09:07:09Z",
        ] {
            assert_eq!(one(&format!("[{odd}]")), format!("[{odd}]"));
        }
    }

    #[test]
    fn paths_and_ports_become_names() {
        assert_eq!(
            one("Using local JSON server list: /tmp/parity-1/servers.json"),
            "Using local JSON server list: <FIX>/servers.json"
        );
        assert_eq!(
            one("dial tcp 127.0.0.1:1808: connect: connection refused"),
            "dial tcp 127.0.0.1:<DEAD>: connect: connection refused"
        );
        // 1808 is not the port in 18080 or 18081.
        assert_eq!(
            one("http://127.0.0.1:18080/ http://127.0.0.1:18081/"),
            "http://127.0.0.1:<LIVE>/ http://127.0.0.1:18081/"
        );
        assert_eq!(
            one("Connection refused (os error 61)"),
            "Connection refused (os error <ERRNO>)"
        );
    }

    #[test]
    fn repeated_progress_events_collapse_and_nothing_else_does() {
        let raw = concat!(
            "{\"event\":\"phase\",\"phase\":\"download\"}\n",
            "{\"event\":\"progress\",\"phase\":\"download\",\"seconds\":1,\"mbps\":5.5,\"progress\":50}\n",
            // A tick that came late, as it does on a loaded machine.
            "{\"event\":\"progress\",\"phase\":\"download\",\"seconds\":1.1,\"mbps\":6,\"progress\":55}\n",
            "{\"event\":\"progress\",\"phase\":\"download\",\"seconds\":2,\"mbps\":6,\"progress\":100}\n",
            "{\"event\":\"phase\",\"phase\":\"upload\"}\n",
            "{\"event\":\"progress\",\"phase\":\"upload\",\"seconds\":1,\"mbps\":5.5,\"progress\":50}\n",
            "same\n",
            "same\n",
        );
        assert_eq!(
            normalize(&context(), raw),
            [
                r#"{"event":"phase","phase":"download"}"#,
                r#"{"event":"progress","phase":"download","seconds":<NUM>,"mbps":<NUM>,"progress":<NUM>}"#,
                r#"{"event":"phase","phase":"upload"}"#,
                r#"{"event":"progress","phase":"upload","seconds":<NUM>,"mbps":<NUM>,"progress":<NUM>}"#,
                "same",
                "same",
            ]
        );
    }

    #[test]
    fn line_endings_are_part_of_the_output() {
        assert!(normalize(&context(), "").is_empty());
        assert_eq!(normalize(&context(), "\n"), [""]);
        assert_eq!(normalize(&context(), "a\n\n"), ["a", ""]);
        assert_eq!(normalize(&context(), "a"), ["a<NO NEWLINE>"]);
        assert_eq!(normalize(&context(), "a\r\n"), ["a\r"]);
    }

    #[test]
    fn bytes_that_are_not_text_stay_distinct() {
        assert_eq!(decode(b"ok \xff\x9b \xe2\x82"), "ok <FF><9B> <E2><82>");
        assert_eq!(decode("emoji \u{1f600}".as_bytes()), "emoji \u{1f600}");
    }
}
