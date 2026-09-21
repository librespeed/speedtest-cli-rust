//! A deterministic LibreSpeed-compatible backend on 127.0.0.1, so both clients
//! are measured against exactly the same answers.
//!
//! Every variant serves:
//!
//! - `GET`/`POST /empty.php`: 200 with an empty body, after the whole request
//!   body has been read (Content-Length or chunked).
//! - `GET /garbage.php?ckSize=N`: 200 with N MiB of zeros, 1 MiB by default.
//! - `GET /getIP.php`: a fixed JSON answer.
//! - `POST /results/telemetry.php`: 200 with a share ID.
//!
//! Anything else is a 404. Connections are kept alive, as a real backend's are.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};

const MIB: usize = 1024 * 1024;
static ZEROS: [u8; MIB] = [0; MIB];

const GETIP: &[u8] = br#"{"processedString":"127.0.0.1 - fixture","rawIspInfo":""}"#;

/// What a malicious or compromised backend could answer: the address and the
/// ISP fields carry ESC sequences, C1 controls, bidi overrides, invisible tag
/// characters and a spreadsheet formula.
const GETIP_HOSTILE: &[u8] = include_bytes!("../data/parity/getip-hostile.json");

/// The ID a well-behaved telemetry endpoint answers with.
const TELEMETRY_OK: &[u8] = b"id parity42";

/// A reply that is neither an empty probe answer nor JSON. It carries quotes,
/// a backslash, C0 controls, ESC, DEL, a C1 control, bytes that are not UTF-8,
/// format characters, a no-break space, an emoji and a character assigned
/// after Unicode 15.0, which is everything Go's %q treats specially.
const GARBLED: &[u8] = b"<html>\r\n\t\"quoted\" back\\slash \
ESC\x1b[31mred\x1b[0m BEL\x07 BS\x08 FF\x0c VT\x0b NUL\x00 DEL\x7f \
C1\xc2\x9b rawC1\x9b FF\xff C3\xc3( trunc\xe2\x82 surrogate\xed\xa0\x80 \
RLO\xe2\x80\xae SHY\xc2\xad TAG\xf3\xa0\x81\x81 NBSP\xc2\xa0 \
LS\xe2\x80\xa8 U378\xcd\xb8 PUA\xee\x80\x80 emoji\xf0\x9f\x98\x80 \
U1FAE9\xf0\x9f\xab\xa9 FFFD\xef\xbf\xbd\n</html>\n";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Variant {
    /// The endpoints above and nothing else.
    Plain,
    /// `/getIP.php` answers with hostile text.
    Hostile,
    /// Adds replies the clients cannot parse:
    ///
    /// - `GET /garbled-ping.php` and `GET /garbled-getip.php`: 200 with a
    ///   body that is neither empty nor JSON.
    /// - `POST /telemetry-500-id.php`: 500 with a body shaped like a share
    ///   reply.
    /// - `POST /telemetry-500-page.php`: 500 with an HTML error page.
    Garbled,
}

/// A running backend. Its threads live until the test process exits.
pub struct Backend {
    pub port: u16,
}

impl Backend {
    pub fn start(variant: Variant) -> Backend {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind fixture backend");
        let port = listener.local_addr().expect("fixture address").port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    // A client hanging up mid-transfer is how every download
                    // ends, so an I/O error here is not a failure.
                    let _ = serve(stream, variant);
                });
            }
        });
        Backend { port }
    }
}

/// A port on 127.0.0.1 that refuses connections.
///
/// It is looked for below the range any of the three operating systems hands
/// out on its own, so no listener of the fixtures and no outgoing connection
/// of a client can be given it while the tests run. (Keeping a socket bound to
/// it without listening would reserve it outright, but macOS answers such a
/// port with silence rather than a reset.)
pub fn dead_port() -> u16 {
    let first = 20000 + (std::process::id() % 5000) as u16;
    (first..30000)
        .find(|port| TcpListener::bind((Ipv4Addr::LOCALHOST, *port)).is_ok())
        .expect("a free port")
}

struct Request {
    method: String,
    path: String,
    query: String,
}

fn serve(stream: TcpStream, variant: Variant) -> io::Result<()> {
    stream.set_nodelay(true)?;
    let mut reader = BufReader::with_capacity(256 * 1024, stream.try_clone()?);
    let mut writer = stream;
    while let Some(request) = read_request(&mut reader)? {
        respond(&mut writer, variant, &request)?;
    }
    Ok(())
}

/// Reads one request and discards its body. `None` is a connection the client
/// closed between requests.
fn read_request(reader: &mut BufReader<TcpStream>) -> io::Result<Option<Request>> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default();
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let request = Request {
        method,
        path: path.to_string(),
        query: query.to_string(),
    };

    let mut content_length = 0u64;
    let mut chunked = false;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            return Ok(None);
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        let Some((name, value)) = header.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.trim().parse().unwrap_or(0);
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = value.to_ascii_lowercase().contains("chunked");
        }
    }

    if chunked {
        discard_chunked(reader)?;
    } else {
        discard(reader, content_length)?;
    }
    Ok(Some(request))
}

fn discard(reader: &mut BufReader<TcpStream>, len: u64) -> io::Result<()> {
    let copied = io::copy(&mut reader.by_ref().take(len), &mut io::sink())?;
    if copied < len {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(())
}

fn discard_chunked(reader: &mut BufReader<TcpStream>) -> io::Result<()> {
    loop {
        let mut size_line = String::new();
        if reader.read_line(&mut size_line)? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let size = size_line.split(';').next().unwrap_or_default().trim();
        let size = u64::from_str_radix(size, 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?;
        if size == 0 {
            // Trailers, up to the blank line that ends the body.
            loop {
                let mut trailer = String::new();
                if reader.read_line(&mut trailer)? == 0 || trailer.trim_end().is_empty() {
                    return Ok(());
                }
            }
        }
        discard(reader, size)?;
        let mut crlf = String::new();
        reader.read_line(&mut crlf)?;
    }
}

fn respond(writer: &mut TcpStream, variant: Variant, request: &Request) -> io::Result<()> {
    let get = request.method == "GET";
    let post = request.method == "POST";
    let garbled = variant == Variant::Garbled;
    match request.path.as_str() {
        "/empty.php" if get || post => send(writer, "200 OK", "text/plain", b""),
        "/garbage.php" if get => send_garbage(writer, &request.query),
        "/getIP.php" if get && variant == Variant::Hostile => {
            send(writer, "200 OK", "application/json", GETIP_HOSTILE)
        }
        "/getIP.php" if get => send(writer, "200 OK", "application/json", GETIP),
        "/results/telemetry.php" if post => send(writer, "200 OK", "text/plain", TELEMETRY_OK),
        "/garbled-ping.php" | "/garbled-getip.php" if get && garbled => {
            send(writer, "200 OK", "text/html", GARBLED)
        }
        "/telemetry-500-id.php" if post && garbled => send(
            writer,
            "500 Internal Server Error",
            "text/html",
            b"id garbled500",
        ),
        "/telemetry-500-page.php" if post && garbled => send(
            writer,
            "500 Internal Server Error",
            "text/html",
            b"<html><body>Internal Server Error</body></html>\n",
        ),
        _ => send(writer, "404 Not Found", "text/plain", b""),
    }
}

fn send(writer: &mut TcpStream, status: &str, content_type: &str, body: &[u8]) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    writer.write_all(head.as_bytes())?;
    writer.write_all(body)?;
    writer.flush()
}

fn send_garbage(writer: &mut TcpStream, query: &str) -> io::Result<()> {
    let mib = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("ckSize="))
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1)
        .max(1);
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\n\r\n",
        mib * MIB
    );
    writer.write_all(head.as_bytes())?;
    for _ in 0..mib {
        writer.write_all(&ZEROS)?;
    }
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exchange(port: u16, request: &str) -> Vec<u8> {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("connect");
        stream.write_all(request.as_bytes()).expect("send request");
        stream
            .shutdown(std::net::Shutdown::Write)
            .expect("half-close");
        let mut reply = Vec::new();
        stream.read_to_end(&mut reply).expect("read reply");
        reply
    }

    fn text(reply: &[u8]) -> String {
        String::from_utf8_lossy(reply).into_owned()
    }

    #[test]
    fn a_connection_serves_several_requests() {
        let backend = Backend::start(Variant::Plain);
        let reply = text(&exchange(
            backend.port,
            "POST /empty.php?r=0.1 HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\nhello\
             POST /empty.php HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n\
             3\r\nabc\r\n2;ext=1\r\nde\r\n0\r\nTrailer: x\r\n\r\n\
             GET /getIP.php?isp=true HTTP/1.1\r\nHost: x\r\n\r\n",
        ));
        assert_eq!(reply.matches("HTTP/1.1 200 OK").count(), 3, "{reply}");
        assert!(reply.ends_with(&text(GETIP)), "{reply}");
    }

    #[test]
    fn garbage_is_as_long_as_cksize_asks() {
        let backend = Backend::start(Variant::Plain);
        let reply = exchange(
            backend.port,
            "GET /garbage.php?r=0.5&ckSize=2 HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        let head_end = reply.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        assert!(text(&reply[..head_end]).contains("Content-Length: 2097152"));
        assert_eq!(reply.len() - head_end, 2 * MIB);
        assert!(reply[head_end..].iter().all(|&byte| byte == 0));
    }

    #[test]
    fn variants_differ_only_where_they_say() {
        let plain = Backend::start(Variant::Plain);
        let hostile = Backend::start(Variant::Hostile);
        let garbled = Backend::start(Variant::Garbled);
        let getip = "GET /getIP.php HTTP/1.1\r\nHost: x\r\n\r\n";
        assert!(exchange(plain.port, getip).ends_with(GETIP));
        assert!(exchange(hostile.port, getip).ends_with(GETIP_HOSTILE));
        assert!(exchange(garbled.port, getip).ends_with(GETIP));

        let ping = "GET /garbled-ping.php HTTP/1.1\r\nHost: x\r\n\r\n";
        assert!(text(&exchange(plain.port, ping)).starts_with("HTTP/1.1 404"));
        assert!(exchange(garbled.port, ping).ends_with(GARBLED));

        let failing =
            "POST /telemetry-500-id.php HTTP/1.1\r\nHost: x\r\nContent-Length: 1\r\n\r\nx";
        let reply = text(&exchange(garbled.port, failing));
        assert!(reply.starts_with("HTTP/1.1 500"), "{reply}");
        assert!(reply.ends_with("id garbled500"), "{reply}");

        let telemetry =
            "POST /results/telemetry.php HTTP/1.1\r\nHost: x\r\nContent-Length: 1\r\n\r\nx";
        assert!(exchange(plain.port, telemetry).ends_with(TELEMETRY_OK));
    }

    /// The payload was designed against Go's %q, byte by byte; a slip in the
    /// literal above would quietly test something else.
    #[test]
    fn the_garbled_payload_is_the_designed_one() {
        assert_eq!(GARBLED.len(), 199);
        assert_eq!(
            GARBLED.iter().map(|&byte| u32::from(byte)).sum::<u32>(),
            18606
        );
        assert!(String::from_utf8(GARBLED.to_vec()).is_err());
    }

    #[test]
    fn a_dead_port_refuses_connections() {
        let error = TcpStream::connect((Ipv4Addr::LOCALHOST, dead_port())).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused, "{error}");
    }
}
