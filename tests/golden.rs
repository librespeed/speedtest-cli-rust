//! The report rendering, checked against data Go itself wrote.
//!
//! `tools/golden/main.go` writes the files under `tests/data` using the very
//! packages the Go client renders its reports with -- `encoding/json`,
//! `strconv` and `time` -- and every expected value here comes from there.
//! Feeding the same inputs to this client pins the two renderings together
//! without a Go toolchain at hand, so this runs in every `cargo test`.

use chrono::{DateTime, FixedOffset};
use librespeed_cli::defs::IPInfoResponse;
use librespeed_cli::report::{
    csv_rows, format_timestamp, to_go_json, CSVReport, Client, JSONReport, ProgressEvent,
    ReportServer, TLSReport,
};
use librespeed_cli::util::round2;
use serde::Deserialize;

const FLOATS: &str = include_str!("data/go_floats.tsv");
const ROUNDING: &str = include_str!("data/go_rounding.tsv");
const TIMESTAMPS: &str = include_str!("data/go_timestamps.tsv");
const JSON: &str = include_str!("data/go_json.tsv");

/// The rows of a data file, without its comment header.
fn rows(text: &'static str) -> impl Iterator<Item = Vec<&'static str>> {
    text.lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| line.split('\t').collect())
}

/// A float from the bits the data carries it as, so that no decimal parser
/// stands between the two languages.
fn unbits(hex: &str) -> f64 {
    f64::from_bits(u64::from_str_radix(hex, 16).expect("hexadecimal float bits"))
}

/// Collects mismatches and fails once with all of them, so a rendering that
/// slipped is read off in a single run rather than one row at a time.
struct Mismatches {
    what: &'static str,
    found: Vec<String>,
    checked: usize,
}

impl Mismatches {
    fn new(what: &'static str) -> Self {
        Mismatches {
            what,
            found: Vec::new(),
            checked: 0,
        }
    }

    fn check(&mut self, input: &str, got: &str, want: &str) {
        self.checked += 1;
        if got != want {
            self.found
                .push(format!("{input}: this client {got:?}, Go {want:?}"));
        }
    }

    fn verdict(self) {
        assert!(self.checked > 0, "{}: no rows were checked", self.what);
        let shown: Vec<&String> = self.found.iter().take(20).collect();
        assert!(
            self.found.is_empty(),
            "{}: {} of {} rows render differently from Go:\n{}",
            self.what,
            self.found.len(),
            self.checked,
            shown
                .iter()
                .map(|line| format!("  {line}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}

/// The number as this client writes it into a JSON document.
fn json_number(v: f64) -> String {
    let event = ProgressEvent {
        event: "progress",
        phase: "download",
        seconds: 0.0,
        mbps: v,
        progress: 0,
    };
    let out = to_go_json(&event).expect("render a progress event");
    let tail = out.split("\"mbps\":").nth(1).expect("an mbps member");
    tail.split(",\"progress\"")
        .next()
        .expect("a progress member after it")
        .to_string()
}

/// The number as this client writes it into a CSV row.
fn csv_number(v: f64) -> String {
    let report = CSVReport {
        ping: v,
        ..Default::default()
    };
    let row = csv_rows(&[report], b',').expect("render a CSV row");
    row.trim_end_matches('\n')
        .split(',')
        .nth(3)
        .expect("the Ping column")
        .to_string()
}

#[test]
fn floats_render_as_encoding_json_renders_them() {
    let mut bad = Mismatches::new("go_floats.tsv, the JSON rendering");
    for row in rows(FLOATS) {
        let [bits, want, _] = row[..] else {
            panic!("a row of bits, JSON and CSV: {row:?}");
        };
        bad.check(bits, &json_number(unbits(bits)), want);
    }
    bad.verdict();
}

#[test]
fn floats_render_in_csv_as_gocsv_renders_them() {
    let mut bad = Mismatches::new("go_floats.tsv, the CSV rendering");
    for row in rows(FLOATS) {
        let [bits, _, want] = row[..] else {
            panic!("a row of bits, JSON and CSV: {row:?}");
        };
        bad.check(bits, &csv_number(unbits(bits)), want);
    }
    bad.verdict();
}

#[test]
fn rounding_lands_where_go_rounds() {
    let mut bad = Mismatches::new("go_rounding.tsv");
    for row in rows(ROUNDING) {
        let [bits, want] = row[..] else {
            panic!("a row of input and rounded bits: {row:?}");
        };
        let got = format!("{:016x}", round2(unbits(bits)).to_bits());
        bad.check(bits, &got, want);
    }
    bad.verdict();
}

#[test]
fn timestamps_render_as_time_marshals_them() {
    let mut bad = Mismatches::new("go_timestamps.tsv");
    for row in rows(TIMESTAMPS) {
        let [secs, nanos, offset, want] = row[..] else {
            panic!("a row of seconds, nanoseconds, offset and text: {row:?}");
        };
        let at = instant(
            secs.parse().expect("unix seconds"),
            nanos.parse().expect("nanoseconds"),
            offset.parse().expect("an offset in seconds"),
        );
        bad.check(
            &format!("{secs}.{nanos} at {offset}"),
            &format_timestamp(at),
            want,
        );
    }
    bad.verdict();
}

/// The instant Go's `time.Unix(sec, nsec).In(time.FixedZone("", offset))` is.
fn instant(secs: i64, nanos: u32, offset: i32) -> DateTime<FixedOffset> {
    DateTime::from_timestamp(secs, nanos)
        .expect("an instant chrono can hold")
        .with_timezone(&FixedOffset::east_opt(offset).expect("an offset chrono can hold"))
}

/// A timestamp in the data, built the same way on both sides.
#[derive(Deserialize)]
struct Instant {
    sec: i64,
    nsec: u32,
    offset: i32,
}

/// One report to render. The floats travel as bits, the client block as the
/// ten strings it is made of, and the TLS block as its two.
#[derive(Deserialize)]
struct ReportIn {
    at: Instant,
    name: String,
    url: String,
    client: [String; 10],
    bytes_sent: u64,
    bytes_received: u64,
    ping: String,
    jitter: String,
    upload: String,
    download: String,
    share: String,
    tls: Option<[String; 2]>,
}

#[derive(Deserialize)]
struct ProgressIn {
    phase: String,
    seconds: String,
    mbps: String,
    progress: u32,
}

/// One row's input: a list of reports, or a single progress event.
#[derive(Deserialize)]
struct CaseIn {
    reports: Option<Vec<ReportIn>>,
    #[serde(default)]
    progress: Option<ProgressIn>,
}

impl From<ReportIn> for JSONReport {
    fn from(r: ReportIn) -> JSONReport {
        let [ip, hostname, city, region, country, location, organization, postal, timezone, readme] =
            r.client;
        JSONReport {
            timestamp: format_timestamp(instant(r.at.sec, r.at.nsec, r.at.offset)),
            server: ReportServer {
                name: r.name,
                url: r.url,
            },
            client: Client::new(IPInfoResponse {
                ip,
                hostname,
                city,
                region,
                country,
                location,
                organization,
                postal,
                timezone,
                readme,
            }),
            bytes_sent: r.bytes_sent,
            bytes_received: r.bytes_received,
            ping: unbits(&r.ping),
            jitter: unbits(&r.jitter),
            upload: unbits(&r.upload),
            download: unbits(&r.download),
            share: r.share,
            tls: r.tls.map(|[version, cipher]| TLSReport { version, cipher }),
        }
    }
}

#[test]
fn whole_documents_render_as_encoding_json_renders_them() {
    let mut bad = Mismatches::new("go_json.tsv");
    for row in rows(JSON) {
        let [input, want] = row[..] else {
            panic!("a row of input and expected document: {row:?}");
        };
        let case: CaseIn = serde_json::from_str(input).expect("a case the test understands");
        let got = match case.progress {
            Some(p) => to_go_json(&ProgressEvent {
                event: "progress",
                // The phase outlives the document it is rendered into.
                phase: Box::leak(p.phase.into_boxed_str()),
                seconds: unbits(&p.seconds),
                mbps: unbits(&p.mbps),
                progress: p.progress,
            }),
            None => {
                let reports: Vec<JSONReport> = case
                    .reports
                    .unwrap_or_default()
                    .into_iter()
                    .map(JSONReport::from)
                    .collect();
                // No report is `null`, as Go's nil slice marshals, not `[]`.
                to_go_json(&(!reports.is_empty()).then_some(&reports[..]))
            }
        }
        .expect("render the document");
        bad.check(input, &got, want);
    }
    bad.verdict();
}
