//! What the client offers in ALPN and which protocol it then speaks, against a
//! TLS server in this process. The backend in integration.rs is plain HTTP/1.1,
//! so this is where `--http2` and both TLS backends meet a real handshake.

use std::io::Read;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

const BIN: &str = env!("CARGO_BIN_EXE_librespeed-cli");

/// What an HTTP/2 client sends first, before any frame.
const H2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// The self-signed certificate and key the unit tests in src/http/mod.rs use.
/// The client runs with `--skip-cert-verify`.
const CERT: &str = "-----BEGIN CERTIFICATE-----
MIIBmzCCAUGgAwIBAgIUYO6HUq+LfqLyuDs6KRY13lQEHOEwCgYIKoZIzj0EAwIw
FDESMBAGA1UEAwwJbG9jYWxob3N0MCAXDTI2MDkxNTA5MjYxOVoYDzIxMjYwODIy
MDkyNjE5WjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIBBggqhkjO
PQMBBwNCAASKZCFHvCS6BEkimY9mqjPZhDcULO3EZD/4gnUj9TooNmtCjXdP4GAK
xDoCb0FzQoHhDoi8BXY4DFLQYpbnX4Nuo28wbTAdBgNVHQ4EFgQUZgrzyhvx3Uvo
bB/Fanc9qOkc2q0wHwYDVR0jBBgwFoAUZgrzyhvx3UvobB/Fanc9qOkc2q0wDwYD
VR0TAQH/BAUwAwEB/zAaBgNVHREEEzARgglsb2NhbGhvc3SHBH8AAAEwCgYIKoZI
zj0EAwIDSAAwRQIhANiLitk3Rlnv12z50HG/whfDtlrRKDWvxniZExYKrm8HAiAy
4RRR3gvO9obcQwMncVNQLZsC6C6J00O0EcR4TET5zA==
-----END CERTIFICATE-----
";

const KEY: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgSiYX/YODAAF5OWIE
qQuLN6hmz3baz471Yn8bjYeNL0yhRANCAASKZCFHvCS6BEkimY9mqjPZhDcULO3E
ZD/4gnUj9TooNmtCjXdP4GAKxDoCb0FzQoHhDoi8BXY4DFLQYpbnX4Nu
-----END PRIVATE KEY-----
";

/// One connection as the server saw it.
#[derive(Debug)]
struct Seen {
    /// The ALPN protocols in the client's hello, in its order.
    offered: Vec<Vec<u8>>,
    /// The protocol the server agreed to, if any.
    chosen: Option<Vec<u8>>,
    /// The first bytes the client sent once the handshake was done.
    first: Vec<u8>,
}

type Log = Arc<Mutex<Vec<Seen>>>;

/// Accepts TLS connections, agreeing to the first of `alpn` the client also
/// offers (no ALPN at all when `alpn` is empty), and records each connection
/// that sends something. Nothing is answered: the first bytes are all a test
/// needs. Returns the server's URL and its log.
fn serve(alpn: &[&[u8]]) -> (String, Log) {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from_pem_slice(CERT.as_bytes()).unwrap()],
            PrivateKeyDer::from_pem_slice(KEY.as_bytes()).unwrap(),
        )
        .unwrap();
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    let config = Arc::new(config);

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("https://{}/", listener.local_addr().unwrap());
    let log = Log::default();
    let seen = log.clone();
    std::thread::spawn(move || {
        for tcp in listener.incoming() {
            let Ok(mut tcp) = tcp else { continue };
            let _ = tcp.set_read_timeout(Some(Duration::from_secs(10)));

            // The Acceptor shows the client's hello before the handshake goes on.
            let mut acceptor = rustls::server::Acceptor::default();
            let accepted = loop {
                if !matches!(acceptor.read_tls(&mut tcp), Ok(n) if n > 0) {
                    break None;
                }
                match acceptor.accept() {
                    Ok(Some(accepted)) => break Some(accepted),
                    Ok(None) => {}
                    Err(_) => break None,
                }
            };
            let Some(accepted) = accepted else { continue };
            let offered = accepted
                .client_hello()
                .alpn()
                .map(|protocols| protocols.map(<[u8]>::to_vec).collect())
                .unwrap_or_default();
            let Ok(conn) = accepted.into_connection(config.clone()) else {
                continue;
            };

            let mut tls = rustls::StreamOwned::new(conn, tcp);
            let mut first = vec![0; H2_PREFACE.len()];
            let mut filled = 0;
            while filled < first.len() {
                match tls.read(&mut first[filled..]) {
                    Ok(n) if n > 0 => filled += n,
                    _ => break,
                }
            }
            if filled == 0 {
                continue;
            }
            first.truncate(filled);
            let chosen = tls.conn.alpn_protocol().map(<[u8]>::to_vec);
            seen.lock().unwrap().push(Seen {
                offered,
                chosen,
                first,
            });
        }
    });
    (url, log)
}

/// Runs the client against `url` until the server has recorded a connection,
/// stops it, and returns what the server saw on that first connection. Later
/// ones may have been cut short by stopping the client.
fn first_connection(url: &str, log: &Log, extra: &[&str]) -> Seen {
    let port = url.rsplit(':').next().unwrap().trim_end_matches('/');
    let list: PathBuf = std::env::temp_dir().join(format!("librespeed-cli-alpn-{port}.json"));
    let json = format!(
        r#"[{{"name":"ALPN","server":"{url}","id":1,"dlURL":"garbage.php","ulURL":"empty.php","pingURL":"empty.php","getIpURL":"getIP.php"}}]"#
    );
    std::fs::write(&list, json).unwrap();

    let mut child = Command::new(BIN)
        .arg("--local-json")
        .arg(&list)
        .args(["--server", "1", "--no-icmp", "--skip-cert-verify"])
        .args(extra)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("run librespeed-cli");
    let deadline = Instant::now() + Duration::from_secs(30);
    while log.lock().unwrap().is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&list);

    let mut seen = std::mem::take(&mut *log.lock().unwrap());
    assert!(!seen.is_empty(), "the client never sent anything over TLS");
    seen.remove(0)
}

/// The connection carries an HTTP/1.1 request line, such as `GET /getIP.php`.
fn assert_http1(conn: &Seen) {
    assert!(
        [&b"GET /"[..], b"POST /"]
            .iter()
            .any(|method| conn.first.starts_with(method)),
        "not an HTTP/1.1 request: {conn:?}"
    );
}

#[test]
fn without_http2_the_client_offers_and_speaks_only_http1() {
    let (url, log) = serve(&[b"h2", b"http/1.1"]);
    let conn = first_connection(&url, &log, &[]);

    // Neither backend sends ALPN then, so the server cannot pick h2.
    assert!(conn.offered.is_empty(), "{conn:?}");
    assert_eq!(conn.chosen, None, "{conn:?}");
    assert_http1(&conn);
}

#[cfg(feature = "http2")]
#[test]
fn with_http2_the_client_offers_h2_first_and_speaks_it() {
    let (url, log) = serve(&[b"h2", b"http/1.1"]);
    let conn = first_connection(&url, &log, &["--http2"]);

    assert_eq!(
        conn.offered,
        [b"h2".to_vec(), b"http/1.1".to_vec()],
        "{conn:?}"
    );
    assert_eq!(conn.chosen.as_deref(), Some(&b"h2"[..]), "{conn:?}");
    assert_eq!(conn.first, H2_PREFACE, "{conn:?}");
}

#[cfg(feature = "http2")]
#[test]
fn with_http2_a_server_that_picks_http1_gets_http1() {
    let (url, log) = serve(&[b"http/1.1"]);
    let conn = first_connection(&url, &log, &["--http2"]);

    assert_eq!(conn.chosen.as_deref(), Some(&b"http/1.1"[..]), "{conn:?}");
    assert_http1(&conn);
}

#[cfg(feature = "http2")]
#[test]
fn with_http2_a_server_without_alpn_gets_http1() {
    let (url, log) = serve(&[]);
    let conn = first_connection(&url, &log, &["--http2"]);

    assert_eq!(conn.chosen, None, "{conn:?}");
    assert_http1(&conn);
}
