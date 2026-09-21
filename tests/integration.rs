//! End-to-end tests driving the built binary against an in-process LibreSpeed
//! backend, so the full flow — server list, ping, download, upload, telemetry,
//! report rendering — is exercised without touching the network.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

const BIN: &str = env!("CARGO_BIN_EXE_librespeed-cli");
const GARBAGE_LEN: usize = 2 * 1024 * 1024;

/// A mock LibreSpeed backend. Dropping it leaves the thread running; tests are
/// short-lived processes, so that is fine.
struct MockBackend {
    addr: SocketAddr,
    telemetry_hits: Arc<AtomicUsize>,
}

impl MockBackend {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock backend");
        let addr = listener.local_addr().expect("mock backend address");
        let telemetry_hits = Arc::new(AtomicUsize::new(0));

        let hits = telemetry_hits.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let hits = hits.clone();
                std::thread::spawn(move || {
                    let _ = handle(stream, hits);
                });
            }
        });

        Self {
            addr,
            telemetry_hits,
        }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Writes a server list pointing at this backend and returns its path.
    fn server_list(&self, name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "librespeed-cli-test-{name}-{}.json",
            self.addr.port()
        ));
        let json = format!(
            r#"[{{"name":"Mock {name}","server":"{}","id":1,"dlURL":"garbage.php","ulURL":"empty.php","pingURL":"empty.php","getIpURL":"getIP.php","sponsorName":"Mock Sponsor","sponsorURL":"https://example.invalid"}}]"#,
            self.url()
        );
        std::fs::write(&path, json).expect("write server list");
        path
    }
    /// Writes a server list whose text is as hostile as a server list can be,
    /// pointing at the backend's hostile getIP answer.
    fn hostile_server_list(&self, name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "librespeed-cli-test-{name}-hostile-{}.json",
            self.addr.port()
        ));
        // Written with JSON escapes so the file itself stays plain ASCII: an
        // ESC, a C1 control, a bidi override, tag characters, an Arabic letter
        // mark, a soft hyphen, and the characters Go's JSON encoder escapes.
        let json = format!(
            r#"[{{"name":"Evil\u001b[31mRed\u009b1m \u202eTXEN\u202c \udb40\udc41\udb40\udc42tag \u061c\u00ad\u180e & <b>","server":"{}","id":1,"dlURL":"garbage.php","ulURL":"empty.php","pingURL":"empty.php","getIpURL":"getIP-hostile.php","sponsorName":"Spon\u202esor\u001b]0;title\u0007","sponsorURL":"example.invalid"}}]"#,
            self.url()
        );
        std::fs::write(&path, json).expect("write hostile server list");
        path
    }
}

fn handle(mut stream: TcpStream, telemetry_hits: Arc<AtomicUsize>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);

    // Request line.
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(());
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let path = target.split('?').next().unwrap_or_default().to_string();

    // Headers.
    let mut content_length = 0usize;
    let mut chunked = false;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            break;
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        let lower = header.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        } else if lower.starts_with("transfer-encoding:") && lower.contains("chunked") {
            chunked = true;
        }
    }

    // Body.
    let mut body = Vec::new();
    if chunked {
        loop {
            let mut size_line = String::new();
            if reader.read_line(&mut size_line)? == 0 {
                break;
            }
            let size = usize::from_str_radix(size_line.trim(), 16).unwrap_or(0);
            if size == 0 {
                break;
            }
            let mut chunk = vec![0u8; size];
            if reader.read_exact(&mut chunk).is_err() {
                break;
            }
            body.extend_from_slice(&chunk);
            let mut crlf = [0u8; 2];
            let _ = reader.read_exact(&mut crlf);
        }
    } else if content_length > 0 {
        let mut buf = vec![0u8; content_length];
        reader.read_exact(&mut buf)?;
        body = buf;
    }

    let respond = |stream: &mut TcpStream,
                   body: &[u8],
                   content_type: &str|
     -> std::io::Result<()> {
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes())?;
        stream.write_all(body)?;
        stream.flush()
    };

    match (method.as_str(), path.as_str()) {
        (_, "/empty.php") => respond(&mut stream, b"", "text/plain")?,
        ("GET", "/garbage.php") => respond(
            &mut stream,
            &vec![0u8; GARBAGE_LEN],
            "application/octet-stream",
        )?,
        ("GET", "/getIP.php") => {
            let json = br#"{"processedString":"203.0.113.7 - Example ISP","rawIspInfo":{"ip":"203.0.113.7","hostname":"host.example","city":"Testville","region":"Testshire","country":"XX","loc":"0,0","org":"AS64496 Example ISP","postal":"00000","timezone":"UTC","readme":"https://ipinfo.io/missingauth"}}"#;
            respond(&mut stream, json, "application/json")?
        }
        // What a malicious or compromised backend could answer: escape
        // sequences, C1 controls, bidi overrides, invisible tag characters and
        // the HTML delimiters Go's JSON encoder escapes.
        ("GET", "/getIP-hostile.php") => {
            let json = br#"{"processedString":"203.0.113.9\u001b[2J - Evil\u202eISP","rawIspInfo":{"ip":"203.0.113.9\u001b[2J","hostname":"h\u009b31m.example","city":"C\u200bity","region":"R\u061cegion","country":"X\u00adX","loc":"0,0\u180e","org":"=Evil\udb40\udc41ISP & <b>","postal":"0\u007f","timezone":"UTC\u2028","readme":"x"}}"#;
            respond(&mut stream, json, "application/json")?
        }
        ("POST", "/results/telemetry.php") => {
            assert!(
                String::from_utf8_lossy(&body).contains(r#"name="ispinfo""#),
                "telemetry payload must be multipart with an ispinfo field"
            );
            telemetry_hits.fetch_add(1, Ordering::Relaxed);
            respond(&mut stream, b"id 4815162342", "text/plain")?
        }
        _ => {
            stream.write_all(
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )?;
        }
    }

    let _ = stream.shutdown(Shutdown::Write);
    Ok(())
}

fn run(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .output()
        .expect("run librespeed-cli")
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn version_reports_program_name_and_license() {
    let out = run(&["--version"]);
    assert!(out.status.success());
    let stdout = stdout_of(&out);
    assert!(stdout.starts_with("librespeed-cli v"));
    assert!(stdout.contains("GNU Lesser General Public License v3.0"));

    // The upstream Go client carried a `librespeed.org  Copyright (C)` line
    // that named neither a holder nor a year, and dropped it. A copyright
    // notice with no holder claims nothing, so every line here must have one.
    for line in stdout.lines().filter(|l| l.contains("Copyright (C)")) {
        assert!(
            line.trim_end().len() > line.find("Copyright (C)").unwrap() + "Copyright (C)".len(),
            "copyright line names nobody: {line:?}"
        );
    }
}

#[test]
fn csv_header_matches_the_go_implementation() {
    let out = run(&["--csv-header"]);
    assert!(out.status.success());
    assert_eq!(
        stdout_of(&out).trim_end(),
        "Timestamp,Server Name,Address,Ping,Jitter,Download,Upload,Share,IP"
    );
}

#[test]
fn list_renders_id_name_url_and_sponsor() {
    let backend = MockBackend::start();
    let list = backend.server_list("list");

    let out = run(&["--local-json", list.to_str().unwrap(), "--list"]);
    assert!(out.status.success());
    assert_eq!(
        stdout_of(&out).trim_end(),
        format!(
            "1: Mock list ({})  [Sponsor: Mock Sponsor @ https://example.invalid]",
            backend.url()
        )
    );
}

#[test]
fn simple_output_reports_all_three_measurements() {
    let backend = MockBackend::start();
    let list = backend.server_list("simple");

    let out = run(&[
        "--local-json",
        list.to_str().unwrap(),
        "--server",
        "1",
        "--duration",
        "1",
        "--no-icmp",
        "--simple",
    ]);
    assert!(
        out.status.success(),
        "stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = stdout_of(&out);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 3, "unexpected output: {stdout:?}");
    assert!(lines[0].starts_with("Ping:\t") && lines[0].contains("\tJitter:\t"));
    assert!(lines[1].starts_with("Download rate:\t") && lines[1].ends_with(" Mbps"));
    assert!(lines[2].starts_with("Upload rate:\t") && lines[2].ends_with(" Mbps"));

    // Quiet modes keep stderr free of the interactive UI.
    assert!(
        out.stderr.is_empty(),
        "stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn json_report_has_the_expected_shape() {
    let backend = MockBackend::start();
    let list = backend.server_list("json");

    let out = run(&[
        "--local-json",
        list.to_str().unwrap(),
        "--server",
        "1",
        "--duration",
        "1",
        "--no-icmp",
        "--json",
    ]);
    assert!(
        out.status.success(),
        "stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = stdout_of(&out);

    // serde_json does not terminate its output. A document that ends mid-line
    // puts the shell prompt on top of it and leaves line-oriented tools with an
    // unterminated last line, so the trailing newline is part of what this
    // command promises -- and exactly one of them, so the output stays a single
    // line that `read` and friends can consume.
    assert!(stdout.ends_with('\n'), "unterminated output: {stdout:?}");
    assert!(!stdout.ends_with("\n\n"), "extra blank line: {stdout:?}");
    assert!(
        !stdout.trim_end_matches('\n').contains('\n'),
        "report is not on one line: {stdout:?}"
    );

    let reports: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let report = &reports[0];

    assert_eq!(report["server"]["name"], "Mock json");
    assert_eq!(report["server"]["url"], backend.url());
    assert_eq!(report["client"]["ip"], "203.0.113.7");
    assert_eq!(report["client"]["org"], "AS64496 Example ISP");
    // `readme` is deliberately stripped from the report.
    assert!(report["client"].get("readme").is_none());

    for key in ["timestamp", "ping", "jitter", "upload", "download", "share"] {
        assert!(report.get(key).is_some(), "missing key {key}");
    }
    assert!(report["bytes_received"].as_u64().unwrap() > 0);
    assert!(report["bytes_sent"].as_u64().unwrap() > 0);
    assert!(report["download"].as_f64().unwrap() > 0.0);
    assert!(report["upload"].as_f64().unwrap() > 0.0);
}

#[test]
fn csv_report_honours_a_custom_delimiter() {
    let backend = MockBackend::start();
    let list = backend.server_list("csv");

    let out = run(&[
        "--local-json",
        list.to_str().unwrap(),
        "--server",
        "1",
        "--duration",
        "1",
        "--no-icmp",
        "--csv",
        "--csv-delimiter",
        ";",
    ]);
    assert!(
        out.status.success(),
        "stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = stdout_of(&out);
    // Exactly one trailing newline. `--csv-header` ends with one as well, so
    // without this the two modes could not be concatenated into a file a CSV
    // reader will accept -- and the shell prompt would land on the row.
    assert!(stdout.ends_with('\n'), "unterminated row: {stdout:?}");
    assert!(!stdout.ends_with("\n\n"), "extra blank line: {stdout:?}");

    // The row splits into the nine documented columns.
    let fields: Vec<&str> = stdout.trim_end_matches('\n').split(';').collect();
    assert_eq!(fields.len(), 9, "unexpected row: {stdout:?}");
    assert_eq!(fields[1], "Mock csv");
    assert_eq!(fields[2], backend.url());
    assert_eq!(fields[8], "203.0.113.7");
}

// A header and a body written by consecutive runs have to make one valid file;
// that is the point of --csv-header existing separately.
#[test]
fn csv_header_and_rows_concatenate_into_a_valid_file() {
    let backend = MockBackend::start();
    let list = backend.server_list("csvcat");

    let header = run(&["--csv-header"]);
    let body = run(&[
        "--local-json",
        list.to_str().unwrap(),
        "--server",
        "1",
        "--duration",
        "1",
        "--no-icmp",
        "--csv",
    ]);
    assert!(header.status.success() && body.status.success());

    let file = format!("{}{}", stdout_of(&header), stdout_of(&body));
    let lines: Vec<&str> = file.lines().collect();
    assert_eq!(lines.len(), 2, "not two lines: {file:?}");
    assert_eq!(lines[0].split(',').count(), lines[1].split(',').count());
    assert!(file.ends_with('\n'), "unterminated file: {file:?}");
}

#[test]
fn telemetry_produces_a_share_link() {
    let backend = MockBackend::start();
    let list = backend.server_list("telemetry");

    let out = run(&[
        "--local-json",
        list.to_str().unwrap(),
        "--server",
        "1",
        "--duration",
        "1",
        "--no-icmp",
        "--simple",
        "--telemetry-server",
        &backend.url(),
        "--telemetry-level",
        "full",
    ]);
    assert!(
        out.status.success(),
        "stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = stdout_of(&out);
    assert!(
        stdout.contains(&format!(
            "Share your result: {}/results/?id=4815162342",
            backend.url()
        )),
        "unexpected output: {stdout:?}"
    );
    assert_eq!(backend.telemetry_hits.load(Ordering::Relaxed), 1);
}

#[test]
fn no_download_and_no_upload_skip_their_tests() {
    let backend = MockBackend::start();
    let list = backend.server_list("skip");

    let out = run(&[
        "--local-json",
        list.to_str().unwrap(),
        "--server",
        "1",
        "--duration",
        "1",
        "--no-icmp",
        "--json",
        "--no-download",
        "--no-upload",
    ]);
    assert!(
        out.status.success(),
        "stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );

    let reports: serde_json::Value = serde_json::from_str(&stdout_of(&out)).expect("valid JSON");
    assert_eq!(reports[0]["download"], 0.0);
    assert_eq!(reports[0]["upload"], 0.0);
    assert_eq!(reports[0]["bytes_received"], 0);
    assert_eq!(reports[0]["bytes_sent"], 0);
}

// The stream is NDJSON on stdout: phase and progress events while the test
// runs, and one final event carrying the same reports --json prints. One
// parser handles the whole stream, which is the point of the format.
#[test]
fn json_stream_emits_phases_progress_and_one_result() {
    let backend = MockBackend::start();
    let list = backend.server_list("stream");

    let out = run(&[
        "--local-json",
        list.to_str().unwrap(),
        "--server",
        "1",
        "--duration",
        "2",
        "--no-icmp",
        "--json-stream",
    ]);
    assert!(
        out.status.success(),
        "stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = stdout_of(&out);
    let events: Vec<serde_json::Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).expect("every line is valid JSON"))
        .collect();

    let names: Vec<&str> = events
        .iter()
        .map(|e| e["event"].as_str().expect("event field"))
        .collect();

    let phases: Vec<&str> = events
        .iter()
        .filter(|e| e["event"] == "phase")
        .map(|e| e["phase"].as_str().unwrap())
        .collect();
    assert_eq!(phases, ["ping", "download", "upload"], "phase order");

    let progress: Vec<&serde_json::Value> =
        events.iter().filter(|e| e["event"] == "progress").collect();
    assert!(!progress.is_empty(), "no progress events in: {stdout}");
    for p in &progress {
        assert!(p["mbps"].as_f64().is_some(), "mbps missing: {p}");
        assert!(p["seconds"].as_f64().unwrap() > 0.0);
        let pct = p["progress"].as_f64().expect("progress missing");
        assert!((0.0..=100.0).contains(&pct), "progress out of range: {pct}");
        assert!(matches!(
            p["phase"].as_str().unwrap(),
            "download" | "upload"
        ));
    }

    assert_eq!(
        names.iter().filter(|n| **n == "result").count(),
        1,
        "exactly one result event"
    );
    assert_eq!(names.last(), Some(&"result"), "result is the last event");

    let report = &events.last().unwrap()["reports"][0];
    assert_eq!(report["server"]["name"], "Mock stream");
    assert!(report["download"].as_f64().unwrap() > 0.0);
}

#[test]
fn json_stream_conflicts_with_json_and_csv() {
    for other in ["--json", "--csv"] {
        let out = run(&["--json-stream", other]);
        assert!(
            !out.status.success(),
            "--json-stream with {other} must be rejected"
        );
    }
}

#[test]
fn unknown_server_id_fails_cleanly() {
    let backend = MockBackend::start();
    let list = backend.server_list("unknown");

    let out = run(&["--local-json", list.to_str().unwrap(), "--server", "999"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("specified server(s) not found"));
}

#[test]
fn out_of_range_numeric_options_are_rejected() {
    // Before the bounds existed a negative value wrapped into a huge unsigned
    // one, so --upload-size=-1 aborted the process with a capacity overflow and
    // --duration=-1 ran effectively forever. The Go client takes both, and
    // panics on the first. They are refused, in Go's words for a bad number.
    for (option, value) in [
        ("duration", "-1"),
        ("chunks", "-1"),
        ("upload-size", "-1"),
        ("timeout", "-1"),
        ("duration", "9223372037"),
    ] {
        let out = run(&[&format!("--{option}"), value, "--list"]);
        assert_eq!(
            out.status.code(),
            Some(1),
            "--{option} {value} was accepted"
        );
        assert_eq!(
            stdout_of(&out),
            format!("Incorrect Usage: invalid value \"{value}\" for flag -{option}: value out of range\n\n")
        );
    }
}

// Zero is not a number the Go client refuses, apart from --concurrent, which it
// refuses in words of its own.
#[test]
fn concurrent_zero_is_refused_with_the_go_clients_message() {
    let out = run(&["--concurrent", "0", "--list"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "Concurrent requests cannot be lower than 1: 0 is given\nTerminated due to error: invalid concurrent requests setting\n"
    );
    assert!(stdout_of(&out).is_empty());
}

// --duration 0, --chunks 0 and --upload-size 0 run, as they do in the Go
// client: each phase runs only its ramp-up, and the report says what that
// moved.
#[test]
fn zero_duration_chunks_and_upload_size_still_produce_a_report() {
    let backend = MockBackend::start();
    let list = backend.server_list("zero");

    let out = run(&[
        "--local-json",
        list.to_str().unwrap(),
        "--server",
        "1",
        "--no-icmp",
        "--duration",
        "0",
        "--chunks",
        "0",
        "--upload-size",
        "0",
        "--json",
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let reports: serde_json::Value = serde_json::from_str(&stdout_of(&out)).expect("valid JSON");
    assert!(reports[0]["ping"].as_f64().is_some(), "{reports}");
}

#[test]
fn command_lines_the_go_client_accepts_are_not_rejected() {
    // The upper bounds were this port's own invention: a high --concurrent is
    // how a high bandwidth-delay link gets filled, and --timeout 0 means no
    // timeout, which is what a slow link needs.
    //
    // --list, not --help: clap answers --help before it checks for conflicts.
    let backend = MockBackend::start();
    let list = backend.server_list("accepted");
    for args in [
        vec!["--concurrent=100"],
        vec!["--timeout=0"],
        vec!["--ipv4", "--ipv6"],
        vec!["--secure", "--insecure"],
        vec!["--upload-size=70000"],
        vec!["--duration=4000"],
        vec!["--chunks=200000"],
    ] {
        let mut argv = args.clone();
        argv.extend(["--local-json", list.to_str().unwrap(), "--list"]);
        let out = run(&argv);
        assert_eq!(out.status.code(), Some(0), "{args:?} was rejected");
    }
}

#[test]
fn mutually_exclusive_options_are_rejected() {
    for args in [
        vec!["--json", "--json-stream"],
        vec!["--server", "1", "--exclude", "2"],
    ] {
        let out = run(&args);
        assert_eq!(out.status.code(), Some(1), "{args:?} was accepted");
        assert!(String::from_utf8_lossy(&out.stderr).contains("cannot be used with"));
    }
}

// Hostile text from a backend reaches no output at all: not the terminal, not
// the CSV file, and not the JSON report -- which is where the Go client passes
// it through.
#[test]
fn hostile_backend_text_is_stripped_from_every_output() {
    let backend = MockBackend::start();
    let list = backend.hostile_server_list("evil");
    let path = list.to_str().unwrap();

    for mode in [
        vec!["--json"],
        vec!["--csv"],
        vec!["--simple"],
        vec!["--list"],
        vec![],
    ] {
        let mut argv = vec![
            "--local-json",
            path,
            "--server",
            "1",
            "--no-icmp",
            "--duration",
            "1",
            "--no-download",
            "--no-upload",
        ];
        argv.extend(mode.iter().copied());
        let out = run(&argv);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{mode:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let both = format!(
            "{}{}",
            stdout_of(&out),
            String::from_utf8_lossy(&out.stderr)
        );
        for bad in [
            '\u{1b}',
            '\u{7}',
            '\u{9b}',
            '\u{7f}',
            '\u{202e}',
            '\u{202c}',
            '\u{200b}',
            '\u{61c}',
            '\u{ad}',
            '\u{180e}',
            '\u{e0041}',
            '\u{2028}',
        ] {
            assert!(
                !both.contains(bad),
                "{:#x} survived in {mode:?} output: {both:?}",
                bad as u32
            );
        }
    }

    // The fields are still reported, cleaned, and what Go escapes is escaped.
    let out = run(&[
        "--local-json",
        path,
        "--server",
        "1",
        "--no-icmp",
        "--duration",
        "1",
        "--no-download",
        "--no-upload",
        "--json",
    ]);
    let raw = stdout_of(&out);
    assert!(
        raw.contains(r#""name":"Evil[31mRed1m TXEN tag  \u0026 \u003cb\u003e""#),
        "{raw}"
    );
    assert!(raw.contains(r#""ip":"203.0.113.9[2J""#), "{raw}");
    assert!(
        raw.contains(r#""org":"=EvilISP \u0026 \u003cb\u003e""#),
        "{raw}"
    );
    assert!(raw.contains(r#""hostname":"h31m.example""#), "{raw}");
    assert!(raw.contains(r#""timezone":"UTC""#), "{raw}");

    // The CSV row carries the same cleaned text, including the IP column.
    let out = run(&[
        "--local-json",
        path,
        "--server",
        "1",
        "--no-icmp",
        "--duration",
        "1",
        "--no-download",
        "--no-upload",
        "--csv",
    ]);
    let row = stdout_of(&out);
    assert!(row.contains(",Evil[31mRed1m TXEN tag  & <b>,"), "{row}");
    assert!(row.ends_with(",203.0.113.9[2J\n"), "{row}");
}

// A usage error is reported on both streams, the way the Go client's CLI
// library and its main do between them, and exits 1.
#[test]
fn common_usage_errors_are_reported_the_way_the_go_client_reports_them() {
    for (args, message) in [
        (
            vec!["--bogus-flag"],
            "flag provided but not defined: -bogus-flag",
        ),
        (
            vec!["--concurrent", "abc"],
            "invalid value \"abc\" for flag -concurrent: parse error",
        ),
        (vec!["--duration"], "flag needs an argument: -duration"),
        (vec!["--list", "foo"], "unexpected argument: foo"),
    ] {
        let out = run(&args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert_eq!(stdout_of(&out), format!("Incorrect Usage: {message}\n\n"));
        assert_eq!(
            String::from_utf8_lossy(&out.stderr),
            format!("Terminated due to error: {message}\n")
        );
    }
}

#[test]
fn an_unknown_server_id_names_every_id_asked_for() {
    let backend = MockBackend::start();
    let list = backend.server_list("missing");

    let out = run(&[
        "--local-json",
        list.to_str().unwrap(),
        "--server",
        "99",
        "--server",
        "98",
    ]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(
            "\nError when fetching server list: specified server(s) not found: [99 98]\n"
        ),
        "{stderr}"
    );
    assert!(
        stderr.ends_with("\nTerminated due to error: specified server(s) not found: [99 98]\n"),
        "{stderr}"
    );
}

// A file that cannot be read is named the way Go's os package names it, inside
// the frame the Go client puts around each of these options.
#[test]
fn unreadable_files_are_reported_the_way_the_go_client_reports_them() {
    let missing = std::env::temp_dir().join("librespeed-cli-test-does-not-exist.json");
    let missing = missing.to_str().unwrap();

    let out = run(&["--local-json", missing, "--list"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&format!(
            "\nError when fetching server list: open {missing}: "
        )),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!("\nTerminated due to error: open {missing}: ")),
        "{stderr}"
    );

    let out = run(&["--telemetry-json", missing, "--list"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.starts_with(&format!("Cannot read {missing}: open {missing}: ")),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!("\nTerminated due to error: open {missing}: ")),
        "{stderr}"
    );

    // An unreadable CA bundle ends the run with the error on its own.
    let out = run(&["--ca-cert", missing, "--list"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.starts_with(&format!("open {missing}: ")), "{stderr}");
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    #[cfg(unix)]
    assert!(
        stderr.ends_with(": no such file or directory\n"),
        "{stderr}"
    );
}

// clap's layout, the Go client's wording for every option.
#[test]
fn help_carries_the_go_clients_option_texts() {
    let out = run(&["--help"]);
    assert_eq!(out.status.code(), Some(0));
    let flat: String = stdout_of(&out)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    for text in [
        "-h, --help show help",
        "Force HTTPS for every test server, whichever scheme the server list gives. Does not affect how the server list itself is fetched",
        "Force HTTP for every test server, whichever scheme the server list gives. Does not affect how the server list itself is fetched; use --server-json with an http:// URL for that",
        "emit newline-delimited JSON events on stdout while the test runs",
        "or read from stdin with \"--local-json -\".",
        "network INTERFACE to bind to",
        "firewall mark to set on socket.",
        "Single character delimiter (CSV_DELIMITER) to use in CSV output.",
        "HTTP TIMEOUT in seconds.",
        "This option overrides --telemetry-level",
    ] {
        assert!(flat.contains(text), "missing from --help: {text}\n{flat}");
    }
    // The one option this port adds is documented too.
    assert!(flat.contains("--http2"), "{flat}");
}

// The debug log carries the Go client's lines and no others, and names the
// address the server list gave rather than a re-serialised URL.
#[test]
fn debug_output_matches_the_go_clients_lines() {
    let backend = MockBackend::start();
    let list = backend.server_list("debug");

    let out = run(&[
        "--local-json",
        list.to_str().unwrap(),
        "--server",
        "1",
        "--no-icmp",
        "--duration",
        "0",
        "--json",
        "--debug",
    ]);
    assert_eq!(out.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        stderr.contains(&format!("Testing against Mock debug ({})\n", backend.url())),
        "{stderr}"
    );
    for absent in ["Loaded ", "server(s) responded", "Fastest:", "Probing "] {
        assert!(
            !stderr.contains(absent),
            "{absent:?} is not a Go line: {stderr}"
        );
    }
}

// Picking the fastest server logs nothing of its own in the Go client: only
// the probe of each server, then the test itself.
#[test]
fn server_selection_logs_only_the_go_clients_lines() {
    let backend = MockBackend::start();
    let list = backend.server_list("select");

    let out = run(&[
        "--local-json",
        list.to_str().unwrap(),
        "--no-icmp",
        "--duration",
        "0",
        "--no-download",
        "--no-upload",
        "--json",
        "--debug",
    ]);
    assert_eq!(out.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&out.stderr);
    let expected = format!(
        "Connection is not encrypted\nSkipping ICMP for server Mock select, will use HTTP ping\nPinging {} over TCP (IPv4)\nTesting against Mock select ({})\n",
        backend.addr,
        backend.url()
    );
    assert!(stderr.starts_with(&expected), "{stderr}");
}

// A list that arrives but does not parse sends the Go client to the discovery
// endpoint, just as a request that fails outright does.
#[test]
fn an_unparseable_remote_list_is_retried_at_the_discovery_endpoint() {
    let backend = MockBackend::start();
    let out = run(&[
        "--server-json",
        &format!("{}/garbage.php", backend.url()),
        "--list",
    ]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Retry with /.well-known/librespeed\n"),
        "{stderr}"
    );
    assert!(
        stderr.contains("\nError when fetching server list: "),
        "{stderr}"
    );
}

// A step that fails mid-run is reported as the Go client reports it: the step
// and its cause, then the cause alone as the reason the run ended.
#[test]
fn a_failed_step_is_reported_with_its_cause() {
    let path = std::env::temp_dir().join("librespeed-cli-test-bad-url.json");
    std::fs::write(
        &path,
        r#"[{"name":"Bad","server":"http://[not-an-address","id":1,"dlURL":"garbage.php","ulURL":"empty.php","pingURL":"empty.php","getIpURL":"getIP.php","sponsorName":"","sponsorURL":""}]"#,
    )
    .expect("write server list");

    let out = run(&["--local-json", path.to_str().unwrap(), "--server", "1"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    let lines: Vec<&str> = stderr.lines().collect();
    let cause = lines
        .iter()
        .find_map(|l| l.strip_prefix("Failed to get server URL: "))
        .unwrap_or_else(|| panic!("no step line in {stderr}"));
    assert_eq!(
        lines.last().copied(),
        Some(format!("Terminated due to error: {cause}").as_str()),
        "{stderr}"
    );
}
