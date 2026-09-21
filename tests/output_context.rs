//! Two runs of the library in one process must not inherit each other's
//! output settings.
//!
//! The settings used to live in statics that `run` only ever set, so a run
//! with `--json-stream` left every later run in the same process emitting
//! NDJSON events onto its stdout. Only a library user could reach that -- the
//! binary runs once and exits -- and the crate has a `[lib]` target.
//!
//! Unix only: the events go to file descriptor 1, so reading them back means
//! replacing it, and this file holds the one test that does, so nothing else
//! is writing there at the time.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use clap::Parser;
use librespeed_cli::cli::Cli;
use librespeed_cli::speedtest;

/// A backend that answers the two endpoints a test without transfers needs:
/// the ping URL, and getIP.
fn backend() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind backend");
    let addr = listener.local_addr().expect("backend address");
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || answer(stream));
        }
    });
    addr
}

fn answer(mut stream: TcpStream) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone connection"));
    let mut request = String::new();
    if reader.read_line(&mut request).unwrap_or(0) == 0 {
        return;
    }
    loop {
        let mut header = String::new();
        match reader.read_line(&mut header) {
            Ok(0) => break,
            Ok(_) if header == "\r\n" => break,
            Ok(_) => {}
            Err(_) => return,
        }
    }

    let path = request.split(' ').nth(1).unwrap_or("/");
    let body = if path.starts_with("/getIP.php") {
        r#"{"processedString":"127.0.0.1 - localhost","rawIspInfo":""}"#
    } else {
        ""
    };
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

fn server_list(addr: SocketAddr) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "librespeed-cli-output-context-{}.json",
        addr.port()
    ));
    let json = format!(
        r#"[{{"id":1,"name":"Mock","server":"http://{addr}","pingURL":"empty.php","getIpURL":"getIP.php","dlURL":"garbage.php","ulURL":"empty.php"}}]"#
    );
    std::fs::write(&path, json).expect("write server list");
    path
}

/// Runs the client with `args`, with file descriptor 1 pointed at a file, and
/// returns what it wrote there.
async fn run_capturing_stdout(dir: &Path, name: &str, args: &[&str]) -> String {
    let path = dir.join(name);
    let file = std::fs::File::create(&path).expect("create capture file");

    // SAFETY: plain descriptor calls, and this test binary holds one test, so
    // nothing else writes to descriptor 1 while it is redirected.
    let saved = unsafe {
        let saved = libc::dup(1);
        assert!(saved >= 0, "dup stdout");
        assert!(libc::dup2(file.as_raw_fd(), 1) >= 0, "redirect stdout");
        saved
    };

    let cli = Cli::try_parse_from(args).expect("parse arguments");
    let result = speedtest::run(&cli).await;

    // SAFETY: as above; `saved` is the descriptor dup returned.
    unsafe {
        assert!(libc::dup2(saved, 1) >= 0, "restore stdout");
        libc::close(saved);
    }
    result.expect("the run should succeed");

    let mut written = String::new();
    std::fs::File::open(&path)
        .expect("open capture file")
        .read_to_string(&mut written)
        .expect("read capture file");
    written
}

#[tokio::test]
async fn a_streaming_run_does_not_leave_the_next_one_streaming() {
    let addr = backend();
    let list = server_list(addr);
    let list = list.to_str().expect("a printable path");
    let dir = std::env::temp_dir();
    let common = [
        "librespeed-cli",
        "--local-json",
        list,
        "--server",
        "1",
        "--no-icmp",
        "--no-download",
        "--no-upload",
    ];

    let mut streaming = common.to_vec();
    streaming.push("--json-stream");
    let name = format!("librespeed-cli-output-context-{}-stream.out", addr.port());
    let first = run_capturing_stdout(&dir, &name, &streaming).await;
    assert!(
        first.contains(r#"{"event":"phase","phase":"ping"}"#),
        "the streaming run should emit progress events, got: {first}"
    );

    let mut plain = common.to_vec();
    plain.push("--simple");
    let name = format!("librespeed-cli-output-context-{}-plain.out", addr.port());
    let second = run_capturing_stdout(&dir, &name, &plain).await;
    assert!(
        second.starts_with("Ping:"),
        "the second run should print its result, got: {second}"
    );
    assert!(
        !second.contains(r#""event""#),
        "the second run did not ask for progress events but got: {second}"
    );
}
