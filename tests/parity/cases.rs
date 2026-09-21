#![allow(dead_code)]
//! The matrix: what both clients are asked to do, and what is expected of the
//! comparison.
//!
//! In the arguments `@FIX@` stands for the directory of server lists, and
//! `@LIVE@`, `@DEAD@`, `@HOSTILE@` and `@GARBLED@` for the fixtures' ports. In
//! expected lines the same things read `<FIX>`, `<LIVE>` and so on, measured
//! values are the placeholders `normalize.rs` lists, and `*` is any text.

use crate::compare::View;

/// The commit of librespeed/speedtest-cli the expectations were recorded
/// against. CI builds exactly this one; keep `.github/workflows/ci.yml` in step.
pub const GO_COMMIT: &str = "b660d1e6c24f14fc93624538d9e73163e7784335";

/// Why a difference is there: the words of README.md that explain it, so a
/// difference nobody documented cannot be declared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    Sanitized,
    CsvFormulas,
    CsvQuoting,
    TelemetryStatus,
    SchemelessPort,
    SchemeCase,
    NumbersRefused,
    SingleDash,
    DecimalNumbers,
    Positional,
    RepeatedOrSplit,
    BooleanValue,
    CsvDelimiter,
    HelpFirst,
    RarerUsageErrors,
    ListValueCause,
    ErrorCauses,
    Http2Option,
    HelpTypo,
    VersionUrl,
}

impl Why {
    /// Text README.md has to contain, compared with white space collapsed.
    pub fn readme(self) -> &'static str {
        match self {
            Why::Sanitized => "**Everything a server says is sanitized.**",
            Why::CsvFormulas => "**CSV formulas are defused.**",
            Why::CsvQuoting => "**CSV quoting follows the csv crate.**",
            Why::TelemetryStatus => "**A telemetry reply other than 2xx fails the upload.**",
            Why::SchemelessPort => "**Scheme-less server URLs with a port work.**",
            Why::SchemeCase => "**A server URL's scheme keeps its case.**",
            Why::NumbersRefused => "**Numbers the Go client cannot act on are refused.**",
            Why::SingleDash => "`-json` is not `--json`: long options take two dashes.",
            Why::DecimalNumbers => "Numbers are decimal. Go reads `010` as 8",
            Why::Positional => "An argument that is not an option is refused.",
            Why::RepeatedOrSplit => "`--server 1,2` is not split into two servers",
            Why::BooleanValue => "A boolean option takes no value",
            Why::CsvDelimiter => "`--csv-delimiter` must be one ASCII character other than `\"`.",
            Why::HelpFirst => "`--help` is answered as soon as it is seen",
            Why::RarerUsageErrors => "Rarer usage errors read differently",
            Why::ListValueCause => "A malformed or out-of-range `--server` or `--exclude` value",
            Why::ErrorCauses => "**Error causes use this client's libraries' words.**",
            Why::Http2Option => "**HTTP/1.1 by default, HTTP/2 behind `--http2`.**",
            Why::HelpTypo => "a typo corrected in `--telemetry-json`",
            Why::VersionUrl => "`--version` names this repository instead of the Go one",
        }
    }
}

/// The entries of the README's list that no case here exercises: they need a
/// second origin, a certificate, another operating system or hardware a test
/// does not have -- and the one entry that only heads a group of entries that
/// are exercised. Naming them is what lets the check insist that every other
/// entry of the list has a case behind it.
pub const UNEXERCISED: &[&str] = &[
    "**Response sizes are capped.**",
    "**A request body is not sent on to another origin.**",
    // The fixture backend speaks plaintext HTTP only, so no case here can
    // make either client start a request over https, let alone be redirected
    // off it. `http::request_tests::a_redirect_from_https_to_http_is_refused`
    // covers it against a TLS server of its own.
    "**A redirect may not leave https for http.**",
    "**`--interface` works on macOS**",
    "**TLS verification is stricter.**",
    "**The process exits rather than returning from `main`.**",
    "**Go's command line quirks are not copied.**",
];

type Lines = &'static [&'static str];
/// Every entry of the README's list that accounts for one difference.
type Whys = &'static [Why];

#[derive(Debug)]
pub enum Stream {
    /// Byte for byte the same once measurements are normalized.
    Same,
    /// The lines only one client prints, all of them and in order, and every
    /// documented difference that accounts for them.
    Differs { why: Whys, go: Lines, rust: Lines },
}

impl Stream {
    pub fn label(&self) -> &'static str {
        match self {
            Stream::Same => "same",
            Stream::Differs { .. } => "differs",
        }
    }
}

#[derive(Debug)]
pub enum Exit {
    Same,
    Differs { why: Whys, go: i32, rust: i32 },
}

impl Exit {
    pub fn label(&self) -> &'static str {
        match self {
            Exit::Same => "same",
            Exit::Differs { .. } => "differs",
        }
    }
}

pub struct Case {
    pub name: &'static str,
    pub args: Vec<&'static str>,
    pub view: View,
    pub stdout: Stream,
    pub stderr: Stream,
    pub exit: Exit,
}

impl Case {
    fn view(mut self, view: View) -> Case {
        self.view = view;
        self
    }

    fn stdout(mut self, why: Whys, go: Lines, rust: Lines) -> Case {
        self.stdout = Stream::Differs { why, go, rust };
        self
    }

    fn stderr(mut self, why: Whys, go: Lines, rust: Lines) -> Case {
        self.stderr = Stream::Differs { why, go, rust };
        self
    }

    fn exit(mut self, why: Whys, go: i32, rust: i32) -> Case {
        self.exit = Exit::Differs { why, go, rust };
        self
    }

    pub fn reasons(&self) -> Vec<Why> {
        let mut reasons = Vec::new();
        for stream in [&self.stdout, &self.stderr] {
            if let Stream::Differs { why, .. } = stream {
                reasons.extend_from_slice(why);
            }
        }
        if let Exit::Differs { why, .. } = self.exit {
            reasons.extend_from_slice(why);
        }
        reasons
    }
}

/// A case expected to be the same in every respect until said otherwise.
fn case(name: &'static str, args: &[&'static str]) -> Case {
    Case {
        name,
        args: args.to_vec(),
        view: View::Whole,
        stdout: Stream::Same,
        stderr: Stream::Same,
        exit: Exit::Same,
    }
}

/// A case against a server list in the fixture directory, without ICMP: raw
/// sockets need privileges a test does not have.
fn listed(name: &'static str, list: &'static str, args: &[&'static str]) -> Case {
    let mut all = vec!["--local-json", list, "--no-icmp"];
    all.extend_from_slice(args);
    case(name, &all)
}

const SERVERS: &str = "@FIX@/servers.json";
const SPECIAL: &str = "@FIX@/servers-special.json";
const SCHEMELESS: &str = "@FIX@/servers-schemeless.json";
const SCHEMELESS_PORT: &str = "@FIX@/servers-schemeless-port.json";
const HOSTILE: &str = "@FIX@/servers-hostile.json";
const GARBLED: &str = "@FIX@/servers-garbled.json";

/// Reaches the ping and the report without moving any data.
const NO_TRANSFER: [&str; 6] = [
    "--duration",
    "0",
    "--server",
    "1",
    "--no-download",
    "--no-upload",
];

fn no_transfer(name: &'static str, list: &'static str, args: &[&'static str]) -> Case {
    let mut all = NO_TRANSFER.to_vec();
    all.extend_from_slice(args);
    listed(name, list, &all)
}

pub fn matrix() -> Vec<Case> {
    let mut matrix = Vec::new();
    matrix.extend(reports());
    matrix.extend(failures());
    matrix.extend(usage());
    matrix.extend(numbers());
    matrix.extend(files());
    matrix.extend(delimiters());
    matrix.extend(lists());
    matrix.extend(hostile());
    matrix.extend(garbled());
    matrix.extend(texts());
    matrix
}

/// A whole measurement in every output format.
fn reports() -> Vec<Case> {
    const MEASURE: [&str; 4] = ["--duration", "1", "--server", "1"];
    let measured = |name, args: &[&'static str]| {
        let mut all = MEASURE.to_vec();
        all.extend_from_slice(args);
        listed(name, SERVERS, &all)
    };
    vec![
        measured("simple", &["--simple"]),
        measured("csv", &["--csv"]),
        measured("csv_delim_semicolon", &["--csv", "--csv-delimiter", ";"]),
        measured("json", &["--json"]),
        measured("debug", &["--debug"]),
        measured("timeout_0", &["--json", "--timeout", "0"]),
        // Two seconds, so that at least one progress event is certain.
        listed(
            "json_stream",
            SERVERS,
            &["--duration", "2", "--server", "1", "--json-stream"],
        ),
        listed("csv_header", SERVERS, &["--csv-header"]),
        listed("list", SERVERS, &["--list"]),
        no_transfer("nodl_noul_json", SERVERS, &["--json"]),
        listed(
            "duration_0",
            SERVERS,
            &["--duration", "0", "--server", "1", "--json"],
        ),
    ]
}

/// A backend that is not there, and a telemetry server that is not either.
fn failures() -> Vec<Case> {
    let dead = |name, args: &[&'static str]| {
        let mut all = vec!["--duration", "1", "--server", "2"];
        all.extend_from_slice(args);
        listed(name, SERVERS, &all)
    };
    vec![
        dead("dead_json", &["--json"]),
        dead("dead_csv", &["--csv"]),
        listed("server_not_in_list", SERVERS, &["--duration", "1", "--server", "99"]),
        listed("server_not_found_two", SERVERS, &["--server", "99", "--server", "98"]),
        // No --server: every server is probed, and one of them is dead.
        listed(
            "debug_autoselect",
            SERVERS,
            &[
                "--duration",
                "0",
                "--no-download",
                "--no-upload",
                "--json",
                "--debug",
            ],
        )
        .stderr(
            &[Why::ErrorCauses],
            &[
                "Error checking for server status: Get \"http://127.0.0.1:<DEAD>/empty.php\": dial tcp 127.0.0.1:<DEAD>: connect: connection refused",
            ],
            &[
                "Error checking for server status: client error (Connect): Connection refused (os error <ERRNO>)",
            ],
        ),
        no_transfer(
            "telemetry_unreachable",
            SERVERS,
            &["--simple", "--share", "--telemetry-server", "http://127.0.0.1:@DEAD@"],
        )
        .stderr(
            &[Why::ErrorCauses],
            &[
                "Error when sending telemetry data: Post \"http://127.0.0.1:<DEAD>/results/telemetry.php\": dial tcp 127.0.0.1:<DEAD>: connect: connection refused",
            ],
            &[
                "Error when sending telemetry data: client error (Connect)",
            ],
        ),
    ]
}

/// What either client makes of a command line it cannot use.
fn usage() -> Vec<Case> {
    vec![
        listed("bogus_flag", SERVERS, &["--duration", "1", "--server", "1", "--bogus-flag"]),
        listed("bad_value", SERVERS, &["--concurrent", "abc", "--list"]),
        listed("missing_value", SERVERS, &["--list", "--duration"]),
        listed("bad_bool", SERVERS, &["--list", "--json=foo"])
        .stdout(
            &[Why::BooleanValue],
            &[
                "Incorrect Usage: invalid boolean value \"foo\" for -json: parse error",
                "",
            ],
            &[],
        )
        .stderr(
            &[Why::BooleanValue],
            &[
                "Terminated due to error: invalid boolean value \"foo\" for -json: parse error",
            ],
            &[
                "error: unexpected value 'foo' for '--json' found; no more were expected",
                "",
                "Usage: librespeed-cli <--help|--version|--ipv4|--ipv6|--no-download|--no-upload|--no-icmp|--concurrent <CONCURRENT>|--bytes|--mebibytes|--distance <DISTANCE>|--share|--simple|--csv|--csv-delimiter <CSV_DELIMITER>|--csv-header|--json|--json-stream|--list|--server <SERVER>|--exclude <EXCLUDE>|--server-json <SERVER_JSON>|--local-json <LOCAL_JSON>|--source <SOURCE>|--interface <INTERFACE>|--timeout <TIMEOUT>|--duration <DURATION>|--chunks <CHUNKS>|--upload-size <UPLOAD_SIZE>|--secure|--insecure|--ca-cert <CA_CERT>|--skip-cert-verify|--no-pre-allocate|--debug|--telemetry-json <TELEMETRY_JSON>|--telemetry-level <TELEMETRY_LEVEL>|--telemetry-server <TELEMETRY_SERVER>|--telemetry-path <TELEMETRY_PATH>|--telemetry-share <TELEMETRY_SHARE>|--telemetry-extra <TELEMETRY_EXTRA>|--fwmark <FWMARK>|--http2>",
                "",
                "For more information, try '--help'.",
            ],
        ),
        listed("bad_syntax", SERVERS, &["---list"])
        .stdout(
            &[Why::RarerUsageErrors],
            &[
                "Incorrect Usage: bad flag syntax: ---list",
            ],
            &[
                "Incorrect Usage: flag provided but not defined: -list",
            ],
        )
        .stderr(
            &[Why::RarerUsageErrors],
            &[
                "Terminated due to error: bad flag syntax: ---list",
            ],
            &[
                "Terminated due to error: flag provided but not defined: -list",
            ],
        ),
        listed("two_forms", SERVERS, &["--ipv4", "-4", "--list"])
        .stdout(
            &[Why::RarerUsageErrors],
            &[
                "Incorrect Usage: Cannot use two forms of the same flag: 4 ipv4",
                "",
            ],
            &[],
        )
        .stderr(
            &[Why::RarerUsageErrors],
            &[
                "Terminated due to error: Cannot use two forms of the same flag: 4 ipv4",
            ],
            &[
                "error: the argument '--ipv4' cannot be used multiple times",
                "",
                "Usage: librespeed-cli [OPTIONS]",
                "",
                "For more information, try '--help'.",
            ],
        ),
        listed("single_dash", SERVERS, &["-list"])
        .stdout(
            &[Why::SingleDash],
            &[
                "1: Fixture Live (http://127.0.0.1:<LIVE>/)  [Sponsor: Fixture Sponsor @ https://example.org]",
                "2: Fixture Dead (http://127.0.0.1:<DEAD>/) ",
            ],
            &[
                "Incorrect Usage: flag provided but not defined: -l",
                "",
            ],
        )
        .stderr(
            &[Why::SingleDash],
            &[
                "Using local JSON server list: <FIX>/servers.json",
            ],
            &[
                "Terminated due to error: flag provided but not defined: -l",
            ],
        )
        .exit(&[Why::SingleDash], 0, 1),
        listed("positional_stops", SERVERS, &["--list", "foo", "--bogus"])
        .stdout(
            &[Why::Positional],
            &[
                "1: Fixture Live (http://127.0.0.1:<LIVE>/)  [Sponsor: Fixture Sponsor @ https://example.org]",
                "2: Fixture Dead (http://127.0.0.1:<DEAD>/) ",
            ],
            &[
                "Incorrect Usage: unexpected argument: foo",
                "",
            ],
        )
        .stderr(
            &[Why::Positional],
            &[
                "Using local JSON server list: <FIX>/servers.json",
            ],
            &[
                "Terminated due to error: unexpected argument: foo",
            ],
        )
        .exit(&[Why::Positional], 0, 1),
        listed("comma_list", SERVERS, &["--server", "1,2", "--list"])
        .stdout(
            &[Why::RepeatedOrSplit],
            &[
                "1: Fixture Live (http://127.0.0.1:<LIVE>/)  [Sponsor: Fixture Sponsor @ https://example.org]",
                "2: Fixture Dead (http://127.0.0.1:<DEAD>/) ",
            ],
            &[
                "Incorrect Usage: invalid value \"1,2\" for flag -server: parse error",
                "",
            ],
        )
        .stderr(
            &[Why::RepeatedOrSplit],
            &[
                "Using local JSON server list: <FIX>/servers.json",
            ],
            &[
                "Terminated due to error: invalid value \"1,2\" for flag -server: parse error",
            ],
        )
        .exit(&[Why::RepeatedOrSplit], 0, 1),
        listed("bad_list_value", SERVERS, &["--server", "1,x", "--list"])
        .stdout(
            &[Why::ListValueCause],
            &[
                "Incorrect Usage: invalid value \"1,x\" for flag -server: strconv.ParseInt: parsing \"x\": invalid syntax",
            ],
            &[
                "Incorrect Usage: invalid value \"1,x\" for flag -server: parse error",
            ],
        )
        .stderr(
            &[Why::ListValueCause],
            &[
                "Terminated due to error: invalid value \"1,x\" for flag -server: strconv.ParseInt: parsing \"x\": invalid syntax",
            ],
            &[
                "Terminated due to error: invalid value \"1,x\" for flag -server: parse error",
            ],
        ),
        listed("exclude_and_server", SERVERS, &["--server", "1", "--exclude", "2", "--list"])
        .stderr(
            &[Why::RarerUsageErrors],
            &[
                "Using local JSON server list: <FIX>/servers.json",
                "Error when fetching server list: either --exclude or --specific can be used",
                "Terminated due to error: either --exclude or --specific can be used",
            ],
            &[
                "error: the argument '--server <SERVER>' cannot be used with '--exclude <EXCLUDE>'",
                "",
                "Usage: librespeed-cli --local-json <LOCAL_JSON> --no-icmp --server <SERVER> --list",
                "",
                "For more information, try '--help'.",
            ],
        ),
        listed("json_stream_json", SERVERS, &["--json-stream", "--json", "--list"])
        .stderr(
            &[Why::RarerUsageErrors],
            &[
                "Terminated due to error: incompatible options 'json-stream' and 'json'",
            ],
            &[
                "error: the argument '--json-stream' cannot be used with '--json'",
                "",
                "Usage: librespeed-cli --local-json <LOCAL_JSON> --no-icmp --json-stream --list",
                "",
                "For more information, try '--help'.",
            ],
        ),
        listed(
            "source_and_interface",
            SERVERS,
            &["--source", "1.2.3.4", "--interface", "lo0", "--list"],
        ),
        listed("telemetry_level_bad", SERVERS, &["--telemetry-level", "bogus", "--list"]),
        case("help_and_bogus", &["--help", "--bogus-flag"]).view(View::Help)
        .stdout(
            &[Why::HelpFirst],
            &[
                "Incorrect Usage: flag provided but not defined: -bogus-flag",
                "",
            ],
            &[
                "<HELP>",
            ],
        )
        .stderr(
            &[Why::HelpFirst],
            &[
                "Terminated due to error: flag provided but not defined: -bogus-flag",
            ],
            &[],
        )
        .exit(&[Why::HelpFirst], 1, 0),
    ]
}

/// Numbers at the edge of what either client accepts.
fn numbers() -> Vec<Case> {
    vec![
        listed(
            "concurrent_0",
            SERVERS,
            &["--duration", "1", "--server", "1", "--json", "--concurrent", "0"],
        ),
        listed("concurrent_neg", SERVERS, &["--concurrent", "-1", "--list"])
        .stdout(
            &[Why::NumbersRefused],
            &[],
            &[
                "Incorrect Usage: invalid value \"-1\" for flag -concurrent: value out of range",
                "",
            ],
        )
        .stderr(
            &[Why::NumbersRefused],
            &[
                "Concurrent requests cannot be lower than 1: -1 is given",
                "Terminated due to error: invalid concurrent requests setting",
            ],
            &[
                "Terminated due to error: invalid value \"-1\" for flag -concurrent: value out of range",
            ],
        ),
        listed("fwmark_neg", SERVERS, &["--fwmark", "-1", "--list"])
        .stdout(
            &[Why::NumbersRefused],
            &[
                "1: Fixture Live (http://127.0.0.1:<LIVE>/)  [Sponsor: Fixture Sponsor @ https://example.org]",
                "2: Fixture Dead (http://127.0.0.1:<DEAD>/) ",
            ],
            &[
                "Incorrect Usage: invalid value \"-1\" for flag -fwmark: value out of range",
                "",
            ],
        )
        .stderr(
            &[Why::NumbersRefused],
            &[
                "Using local JSON server list: <FIX>/servers.json",
            ],
            &[
                "Terminated due to error: invalid value \"-1\" for flag -fwmark: value out of range",
            ],
        )
        .exit(&[Why::NumbersRefused], 0, 1),
        listed(
            "chunks_0",
            SERVERS,
            &[
                "--duration", "1", "--server", "1", "--chunks", "0", "--no-upload", "--json",
                "--debug",
            ],
        ),
        listed(
            "upload_size_0",
            SERVERS,
            &[
                "--duration", "0", "--server", "1", "--upload-size", "0", "--no-download",
                "--json", "--debug",
            ],
        ),
        listed(
            "chunks_octal",
            SERVERS,
            &[
                "--duration", "0", "--server", "1", "--chunks", "010", "--no-upload", "--json",
                "--debug",
            ],
        )
        .stderr(
            &[Why::DecimalNumbers],
            &[
                "Download test starting: 3 stream(s), 8 chunk(s), up to 0s",
            ],
            &[
                "Download test starting: 3 stream(s), 10 chunk(s), up to 0s",
            ],
        ),
    ]
}

/// A file that is missing, a directory where a file belongs, and a file that
/// is not the JSON it should be.
fn files() -> Vec<Case> {
    const MISSING: &str = "@FIX@/nonexistent";
    const BAD: &str = "@FIX@/bad.json";
    vec![
        listed("telemetry_json_missing", SERVERS, &["--telemetry-json", MISSING, "--list"]),
        listed("telemetry_json_bad", SERVERS, &["--telemetry-json", BAD, "--list"])
        .stderr(
            &[Why::ErrorCauses],
            &[
                "Error parsing <FIX>/bad.json: invalid character 'o' in literal null (expecting 'u')",
                "Terminated due to error: invalid character 'o' in literal null (expecting 'u')",
            ],
            &[
                "Error parsing <FIX>/bad.json: expected ident at line 1 column 2",
                "Terminated due to error: expected ident at line 1 column 2",
            ],
        ),
        listed("ca_cert_missing", SERVERS, &["--ca-cert", MISSING, "--list"]),
        case("local_json_missing", &["--local-json", MISSING, "--no-icmp", "--list"]),
        case("local_json_dir", &["--local-json", "@FIX@", "--no-icmp", "--list"]),
        case("local_json_bad", &["--local-json", BAD, "--no-icmp", "--list"])
        .stderr(
            &[Why::ErrorCauses],
            &[
                "Error when fetching server list: invalid character 'o' in literal null (expecting 'u')",
                "Terminated due to error: invalid character 'o' in literal null (expecting 'u')",
            ],
            &[
                "Error when fetching server list: expected ident at line 1 column 2",
                "Terminated due to error: expected ident at line 1 column 2",
            ],
        ),
    ]
}

/// `--csv-delimiter` with values that are not one plain character.
fn delimiters() -> Vec<Case> {
    vec![
        listed("csv_delim_multi", SERVERS, &["--csv-delimiter", ";;", "--csv-header"])
        .stdout(
            &[Why::CsvDelimiter],
            &[
                "Timestamp;Server Name;Address;Ping;Jitter;Download;Upload;Share;IP",
            ],
            &[],
        )
        .stderr(
            &[Why::CsvDelimiter],
            &[],
            &[
                "Terminated due to error: --csv-delimiter must be a single ASCII character other than '\"', got \";;\"",
            ],
        )
        .exit(&[Why::CsvDelimiter], 0, 1),
        listed("csv_delim_wide", SERVERS, &["--csv-delimiter", "\u{e9}", "--csv-header"])
        .stdout(
            &[Why::CsvDelimiter],
            &[
                "TimestampéServer NameéAddresséPingéJitteréDownloadéUploadéShareéIP",
            ],
            &[],
        )
        .stderr(
            &[Why::CsvDelimiter],
            &[],
            &[
                "Terminated due to error: --csv-delimiter must be a single ASCII character other than '\"', got \"é\"",
            ],
        )
        .exit(&[Why::CsvDelimiter], 0, 1),
        listed("csv_delim_quote", SERVERS, &["--csv-delimiter", "\"", "--csv-header"])
        .stderr(
            &[Why::CsvDelimiter],
            &[],
            &[
                "Terminated due to error: --csv-delimiter must be a single ASCII character other than '\"', got \"\\\"\"",
            ],
        )
        .exit(&[Why::CsvDelimiter], 0, 1),
        no_transfer(
            "csv_delim_quote_rows",
            SERVERS,
            &["--csv", "--csv-delimiter", "\""],
        )
        .stderr(
            &[Why::CsvDelimiter],
            &[
                "Error generating CSV report: csv: invalid field or comment delimiter",
            ],
            &[
                "Terminated due to error: --csv-delimiter must be a single ASCII character other than '\"', got \"\\\"\"",
            ],
        )
        .exit(&[Why::CsvDelimiter], 0, 1),
    ]
}

/// Server lists whose names and URLs need care.
fn lists() -> Vec<Case> {
    vec![
        no_transfer("special_json", SPECIAL, &["--json"]),
        listed(
            "special_csv",
            SPECIAL,
            &[
                "--duration", "0", "--server", "1", "--server", "2", "--server", "3", "--server",
                "4", "--server", "5", "--no-download", "--no-upload", "--csv",
            ],
        )
        .stdout(
            &[Why::CsvQuoting, Why::CsvFormulas],
            &[
                "<TS>,\" Leading space\",http://127.0.0.1:<LIVE>/,<NUM>,<NUM>,<NUM>,<NUM>,,127.0.0.1",
                "<TS>,\"\\.\",http://127.0.0.1:<LIVE>/,<NUM>,<NUM>,<NUM>,<NUM>,,127.0.0.1",
                "<TS>,=2+5+cmd|' /C calc'!A0,http://127.0.0.1:<LIVE>/,<NUM>,<NUM>,<NUM>,<NUM>,,127.0.0.1",
            ],
            &[
                "<TS>, Leading space,http://127.0.0.1:<LIVE>/,<NUM>,<NUM>,<NUM>,<NUM>,,127.0.0.1",
                "<TS>,\\.,http://127.0.0.1:<LIVE>/,<NUM>,<NUM>,<NUM>,<NUM>,,127.0.0.1",
                "<TS>,'=2+5+cmd|' /C calc'!A0,http://127.0.0.1:<LIVE>/,<NUM>,<NUM>,<NUM>,<NUM>,,127.0.0.1",
            ],
        ),
        listed("schemeless_list", SCHEMELESS, &["--list"])
        .stdout(
            &[Why::SchemeCase],
            &[
                "3: Upper (http://Example.COM/x) ",
            ],
            &[
                "3: Upper (HTTP://Example.COM/x) ",
            ],
        ),
        listed("schemeless_port_list", SCHEMELESS_PORT, &["--list"])
        .stdout(
            &[Why::SchemelessPort],
            &[
                "1: S1 (localhost:8080/backend) ",
            ],
            &[
                "1: S1 (http://localhost:8080/backend) ",
            ],
        ),
    ]
}

/// A backend whose name, sponsor and getIP answer are hostile text.
fn hostile() -> Vec<Case> {
    vec![
        listed("hostile_list", HOSTILE, &["--list"])
        .stdout(
            &[Why::Sanitized],
            &[
                "1: Evil[31mRed1m \u{202e}TXEN\u{202c} \u{e0041}\u{e0042}tag \u{61c}\u{ad}\u{180e} & <b> \u{a0}nbsp (http://127.0.0.1:<HOSTILE>/)  [Sponsor: Spon\u{202e}sor]0;title @ https://example.org]",
            ],
            &[
                "1: Evil[31mRed1m TXEN tag  & <b> \u{a0}nbsp (http://127.0.0.1:<HOSTILE>/)  [Sponsor: Sponsor]0;title @ https://example.org]",
            ],
        ),
        no_transfer("hostile_json", HOSTILE, &["--json"])
        .stdout(
            &[Why::Sanitized],
            &[
                "[{\"timestamp\":\"<TS>\",\"server\":{\"name\":\"Evil\\u001b[31mRed\u{9b}1m \u{202e}TXEN\u{202c} \u{e0041}\u{e0042}tag \u{61c}\u{ad}\u{180e} \\u0026 \\u003cb\\u003e \u{a0}nbsp\",\"url\":\"http://127.0.0.1:<HOSTILE>/\"},\"client\":{\"ip\":\"203.0.113.9\\u001b[2J\",\"hostname\":\"h\u{9b}31m.example\",\"city\":\"C\u{200b}ity\",\"region\":\"R\u{61c}egion\",\"country\":\"X\u{ad}X\",\"loc\":\"0,0\u{180e}\",\"org\":\"=Evil\u{e0041}ISP\",\"postal\":\"0\u{7f}\",\"timezone\":\"UTC\\u2028\"},\"bytes_sent\":<NUM>,\"bytes_received\":<NUM>,\"ping\":<NUM>,\"jitter\":<NUM>,\"upload\":<NUM>,\"download\":<NUM>,\"share\":\"\"}]",
            ],
            &[
                "[{\"timestamp\":\"<TS>\",\"server\":{\"name\":\"Evil[31mRed1m TXEN tag  \\u0026 \\u003cb\\u003e \u{a0}nbsp\",\"url\":\"http://127.0.0.1:<HOSTILE>/\"},\"client\":{\"ip\":\"203.0.113.9[2J\",\"hostname\":\"h31m.example\",\"city\":\"City\",\"region\":\"Region\",\"country\":\"XX\",\"loc\":\"0,0\",\"org\":\"=EvilISP\",\"postal\":\"0\",\"timezone\":\"UTC\"},\"bytes_sent\":<NUM>,\"bytes_received\":<NUM>,\"ping\":<NUM>,\"jitter\":<NUM>,\"upload\":<NUM>,\"download\":<NUM>,\"share\":\"\"}]",
            ],
        ),
        no_transfer("hostile_csv", HOSTILE, &["--csv"])
        .stdout(
            &[Why::Sanitized],
            &[
                "<TS>,Evil\u{1b}[31mRed\u{9b}1m \u{202e}TXEN\u{202c} \u{e0041}\u{e0042}tag \u{61c}\u{ad}\u{180e} & <b> \u{a0}nbsp,http://127.0.0.1:<HOSTILE>/,<NUM>,<NUM>,<NUM>,<NUM>,,203.0.113.9\u{1b}[2J",
            ],
            &[
                "<TS>,Evil[31mRed1m TXEN tag  & <b> \u{a0}nbsp,http://127.0.0.1:<HOSTILE>/,<NUM>,<NUM>,<NUM>,<NUM>,,203.0.113.9[2J",
            ],
        ),
        no_transfer("hostile_ui", HOSTILE, &[])
        .stderr(
            &[Why::Sanitized],
            &[
                "Selected server: Evil[31mRed1m \u{202e}TXEN\u{202c} \u{e0041}\u{e0042}tag \u{61c}\u{ad}\u{180e} & <b> \u{a0}nbsp [127.0.0.1]",
                "Sponsored by: Spon\u{202e}sor]0;title @ https://example.org",
                "You're testing from: 203.0.113.9[2J - Evil\u{202e}ISP",
            ],
            &[
                "Selected server: Evil[31mRed1m TXEN tag  & <b> \u{a0}nbsp [127.0.0.1]",
                "Sponsored by: Sponsor]0;title @ https://example.org",
                "You're testing from: 203.0.113.9[2J - EvilISP",
            ],
        ),
        no_transfer("hostile_debug", HOSTILE, &["--debug", "--simple"])
        .stderr(
            &[Why::Sanitized],
            &[
                "Testing against Evil[31mRed1m \u{202e}TXEN\u{202c} \u{e0041}\u{e0042}tag \u{61c}\u{ad}\u{180e} & <b> \u{a0}nbsp (http://127.0.0.1:<HOSTILE>/)",
                "IP info: 203.0.113.9[2J - Evil\u{202e}ISP",
                "Skipping ICMP for server Evil[31mRed1m \u{202e}TXEN\u{202c} \u{e0041}\u{e0042}tag \u{61c}\u{ad}\u{180e} & <b> \u{a0}nbsp, will use HTTP ping",
            ],
            &[
                "Testing against Evil[31mRed1m TXEN tag  & <b> \u{a0}nbsp (http://127.0.0.1:<HOSTILE>/)",
                "IP info: 203.0.113.9[2J - EvilISP",
                "Skipping ICMP for server Evil[31mRed1m TXEN tag  & <b> \u{a0}nbsp, will use HTTP ping",
            ],
        ),
    ]
}

/// A backend whose probe, getIP and telemetry answers are not what either
/// client can parse.
fn garbled() -> Vec<Case> {
    let telemetry = |name, path: &'static str| {
        listed(
            name,
            GARBLED,
            &[
                "--duration",
                "0",
                "--server",
                "3",
                "--no-download",
                "--no-upload",
                "--simple",
                "--share",
                "--telemetry-server",
                "http://127.0.0.1:@GARBLED@",
                "--telemetry-path",
                path,
            ],
        )
    };
    vec![
        listed(
            "garbled_getip_debug",
            GARBLED,
            &[
                "--duration", "0", "--server", "1", "--no-download", "--no-upload", "--debug",
                "--simple",
            ],
        )
        .stderr(
            &[Why::ErrorCauses, Why::Sanitized],
            &[
                "Failed when parsing get IP result: invalid character '<' looking for beginning of value",
                "IP info: <html>\"quoted\" back\\slash ESC[31mred[0m BEL BS FF VT NUL DEL C1 rawC1� FF� C3�( trunc�� surrogate��� RLO\u{202e} SHY\u{ad} TAG\u{e0041} NBSP\u{a0} LS\u{2028} U378\u{378} PUA\u{e000} emoji😀 U1FAE9🫩 FFFD�</html>",
            ],
            &[
                "Failed when parsing get IP result: expected value at line 1 column 1",
                "IP info: <html>\"quoted\" back\\slash ESC[31mred[0m BEL BS FF VT NUL DEL C1 rawC1� FF� C3�( trunc� surrogate��� RLO SHY TAG NBSP\u{a0} LS U378\u{378} PUA\u{e000} emoji😀 U1FAE9🫩 FFFD�</html>",
            ],
        ),
        listed(
            "garbled_probe_debug",
            GARBLED,
            &[
                "--duration", "0", "--server", "2", "--no-download", "--no-upload", "--debug",
                "--simple",
            ],
        ),
        telemetry("telemetry_500_id", "/telemetry-500-id.php")
        .stdout(
            &[Why::TelemetryStatus],
            &[
                "Share your result: http://127.0.0.1:<GARBLED>/results/?id=garbled500",
            ],
            &[],
        )
        .stderr(
            &[Why::TelemetryStatus],
            &[],
            &[
                "Error when sending telemetry data: telemetry server returned HTTP 500 Internal Server Error",
            ],
        ),
        telemetry("telemetry_500_page", "/telemetry-500-page.php")
        .stderr(
            &[Why::TelemetryStatus],
            &[
                "Error when sending telemetry data: server returned invalid response: <html><body>Internal Server Error</body></html>",
                "",
            ],
            &[
                "Error when sending telemetry data: telemetry server returned HTTP 500 Internal Server Error",
            ],
        ),
    ]
}

/// The texts the binary carries itself.
fn texts() -> Vec<Case> {
    vec![
        case("help", &["--help"]).view(View::HelpOptions)
        .stdout(
            &[Why::HelpTypo, Why::Http2Option],
            &[
                "--telemetry-json: Load telemetry server settings from a JSON file. This options overrides --telemetry-level, --telemetry-server, --telemetry-path, and --telemetry-share. Implies --share",
            ],
            &[
                "--telemetry-json: Load telemetry server settings from a JSON file. This option overrides --telemetry-level, --telemetry-server, --telemetry-path, and --telemetry-share. Implies --share",
                "--http2: Allow HTTP/2 when the server offers it. Off by default: HTTP/2 carries every stream over one TCP connection, so --concurrent would no longer mean concurrent connections, which is what a speed test measures",
            ],
        ),
        case("version", &["--version"]).view(View::Version)
        .stdout(
            &[Why::VersionUrl],
            &[
                "https://github.com/librespeed/speedtest-cli",
            ],
            &[
                "https://github.com/librespeed/speedtest-cli-rust",
            ],
        ),
    ]
}
