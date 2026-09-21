//! HTTP client built directly on hyper so that socket binding options
//! (`--source`, `--interface`, `--fwmark`) can be honoured.

pub mod connector;
pub mod tls;

use std::io;
use std::time::Duration;

use anyhow::{bail, Context as _};
use bytes::{Bytes, BytesMut};
use http::header::{HeaderName, HeaderValue, ACCEPT_ENCODING, CONTENT_TYPE, LOCATION, USER_AGENT};
use http::{Method, Request, Response, StatusCode, Uri};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use url::Url;

pub use connector::{BindOptions, IpFamily};
pub use tls::{TlsFacts, TlsSettings};

/// What the transport negotiated for a request, read back off its response.
///
/// hyper copies a connection's extras onto every response served over it, so
/// these describe the connection the request actually used -- not a throwaway
/// one opened afterwards to ask, which can differ in address and cipher both.
#[derive(Clone, Debug, Default)]
pub struct ConnectionFacts {
    /// The address the request reached.
    pub peer: Option<std::net::SocketAddr>,
    /// What the TLS handshake settled on, absent over plain HTTP and on a
    /// TLS backend that does not report it.
    pub tls: Option<TlsFacts>,
}

impl ConnectionFacts {
    fn of<B>(resp: &Response<B>) -> Self {
        Self {
            peer: resp.extensions().get::<connector::PeerAddr>().map(|p| p.0),
            tls: resp.extensions().get::<TlsFacts>().cloned(),
        }
    }
}

/// The redirect that is an error instead of being followed, as in Go's
/// `http.Client`: the tenth, so at most ten requests go out.
const MAX_REDIRECTS: usize = 10;

/// Cap on a buffered response body. The server list URL is user-supplied and
/// the entries in it name further hosts, so response sizes are attacker-chosen;
/// without a cap the client will buffer whatever it is fed until it runs out of
/// memory. Transfer test bodies are streamed and never buffered, so this only
/// bounds control-plane responses.
pub const MAX_BUFFERED_RESPONSE: usize = 8 * 1024 * 1024;
/// Telemetry replies are a short `id <n>` string.
pub const MAX_TELEMETRY_RESPONSE: usize = 64 * 1024;

/// HTTP/2 flow-control windows, matching what Go's transport uses
/// (`transportDefaultStreamFlow` / `transportDefaultConnFlow`).
///
/// The protocol default is 64 KiB, which caps a stream at window/RTT: about
/// 105 Mbps at 5 ms and 26 Mbps at 20 ms. Leaving it there would silently
/// understate any link faster than that.
const H2_STREAM_WINDOW: u32 = 4 * 1024 * 1024;
const H2_CONNECTION_WINDOW: u32 = 1024 * 1024 * 1024;

/// The cap on each HTTP/1 connection's buffers: hyper's minimum.
pub(crate) const H1_MAX_BUF: usize = 8192;

pub type ReqBody = BoxBody<Bytes, io::Error>;

/// What a request sends, which decides what a 307 or 308 can send again.
pub enum RequestBody {
    /// No body.
    Empty,
    /// A body held in memory, sent again when a 307 or 308 asks for it.
    Bytes(Bytes),
    /// A body that can be sent only once, such as the upload stream. A 307 or
    /// 308 is returned instead of followed, as Go does for a body it has no
    /// `GetBody` for.
    Stream(ReqBody),
}

/// Whether two URLs share a scheme, host and effective port.
fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

/// A URL's origin as an error shows it: no user information, and the port only
/// where it is not the scheme's default.
fn origin_of(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default();
    match url.port() {
        Some(port) => format!("{}://{host}:{port}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    }
}

/// An empty request body.
pub fn empty_body() -> ReqBody {
    Empty::<Bytes>::new().map_err(|e| match e {}).boxed()
}

/// A request body holding the given bytes.
pub fn full_body(b: Bytes) -> ReqBody {
    Full::new(b).map_err(|e| match e {}).boxed()
}

/// Reads a response body, refusing to buffer more than `limit` bytes.
///
/// The cap bounds memory, not time: a peer that stays under it by sending
/// slowly is stopped by the request timeout alone, and `--timeout 0` leaves
/// no time bound at all.
async fn collect_limited(body: Incoming, limit: usize) -> anyhow::Result<Bytes> {
    let mut body = body;
    let mut out = BytesMut::new();

    while let Some(frame) = body.frame().await {
        let frame = frame?;
        if let Some(data) = frame.data_ref() {
            if out.len() + data.len() > limit {
                bail!("response body exceeds the {limit} byte limit");
            }
            out.extend_from_slice(data);
        }
    }

    Ok(out.freeze())
}

/// Reads a response body without keeping it, returning how many bytes it held.
///
/// Bodies the program does not read still have to be consumed for the
/// connection to be reusable, and buffering them hands a server control of
/// this process's memory.
async fn drain_body(body: Incoming) -> anyhow::Result<u64> {
    let mut body = body;
    let mut n = 0u64;

    while let Some(frame) = body.frame().await {
        if let Some(data) = frame?.data_ref() {
            n += data.len() as u64;
        }
    }

    Ok(n)
}

/// Keeps the first `limit` bytes of a body and drops the rest unread.
///
/// For deciding what a body *is* rather than reading it: emptiness, or the
/// opening of an error page worth showing. Reading stops once `limit` bytes
/// are kept, so a body that never ends cannot keep the caller waiting.
async fn body_prefix(body: Incoming, limit: usize) -> anyhow::Result<Bytes> {
    let mut body = body;
    let mut out = BytesMut::new();

    while out.len() < limit {
        let Some(frame) = body.frame().await else {
            break;
        };
        if let Some(data) = frame?.data_ref() {
            let take = (limit - out.len()).min(data.len());
            out.extend_from_slice(&data[..take]);
        }
    }

    Ok(out.freeze())
}

/// The hyper client configuration `HttpClient` is built from.
///
/// `bounded` caps a connection's HTTP/1 buffers, and only the upload pool asks
/// for it. The upload total counts body frames as hyper takes them, so what
/// hyper holds when the window closes was counted but never sent: with its
/// default buffer, ~400 KB per connection, that overstated a 1 Mbit uplink by
/// a quarter to a half. The cap is hyper's minimum, leaving 8 KiB and one
/// frame. It binds the read buffer as well, which cost a 65 Gbit loopback
/// download a fifth of its rate and would limit every response head to 8 KiB,
/// so downloads, pings and the control-plane requests keep hyper's defaults.
pub(crate) fn client_builder(
    concurrent: usize,
    http2: bool,
    bounded: bool,
) -> hyper_util::client::legacy::Builder {
    // Keep enough connections alive for every concurrent stream, matching the
    // Go version's MaxIdleConnsPerHost/MaxConnsPerHost tuning.
    let mut builder = Client::builder(TokioExecutor::new());
    builder.pool_max_idle_per_host(concurrent + 2);

    if bounded {
        builder.http1_max_buf_size(H1_MAX_BUF);
    }

    if http2 {
        builder
            .http2_initial_stream_window_size(H2_STREAM_WINDOW)
            .http2_initial_connection_window_size(H2_CONNECTION_WINDOW);
    }
    builder
}

/// The program's HTTP client.
#[derive(Clone)]
pub struct HttpClient {
    inner: Client<tls::Connector, ReqBody>,
    /// The pool whose buffers are capped, for the upload test only.
    upload: Client<tls::Connector, ReqBody>,
    timeout: Duration,
    user_agent: HeaderValue,
}

impl HttpClient {
    pub fn new(
        bind: BindOptions,
        tls_settings: &TlsSettings<'_>,
        timeout: Duration,
        concurrent: usize,
        user_agent: &str,
    ) -> anyhow::Result<Self> {
        let https = tls::build(bind, tls_settings)?;
        let inner = client_builder(concurrent, tls_settings.http2, false).build(https.clone());
        let upload = client_builder(concurrent, tls_settings.http2, true).build(https);

        Ok(Self {
            inner,
            upload,
            timeout,
            user_agent: HeaderValue::from_str(user_agent)?,
        })
    }

    /// The same client, sending over the pool whose buffers are capped.
    ///
    /// Only the upload test wants that cap: it keeps what hyper has counted
    /// but not sent small, and it costs a download speed.
    pub fn for_uploads(&self) -> Self {
        Self {
            inner: self.upload.clone(),
            ..self.clone()
        }
    }

    /// The configured per-request timeout (`--timeout`).
    ///
    /// Zero means none, as it does in the Go client, which is what a slow link
    /// needs when the transfer legitimately outlasts any sensible limit.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Runs a request under the configured timeout, or without one when it is
    /// zero.
    async fn with_timeout<T>(
        &self,
        fut: impl std::future::Future<Output = anyhow::Result<T>>,
    ) -> anyhow::Result<T> {
        if self.timeout.is_zero() {
            return fut.await;
        }

        tokio::time::timeout(self.timeout, fut)
            .await
            .map_err(|_| anyhow::anyhow!("request timed out after {:?}", self.timeout))?
    }

    /// Sends a request and follows its redirects.
    ///
    /// - 301, 302 and 303 are followed with a GET that has no body and no
    ///   Content-Type.
    /// - 307 and 308 are followed with the same method and body, unless the
    ///   request was made with a [`RequestBody::Stream`]: that cannot be sent
    ///   twice, so the 307 or 308 is returned, even when a 303 has already
    ///   dropped the body.
    /// - Any other status, and a redirect with no Location, is returned.
    /// - The tenth redirect is an error, so at most ten requests go out.
    /// - A URL that is neither http nor https is an error, whether the request
    ///   starts at it or a redirect leads to it.
    ///
    /// All of that is what Go's `http.Client` does. Where this differs: a
    /// redirect from https to http is an error, where Go follows it and only
    /// leaves the Referer out; a 307 or 308 that would send a
    /// [`RequestBody::Bytes`] to another origin (scheme, host or port) is an
    /// error, where Go sends the body on; and no Referer is added to the
    /// redirected request.
    pub async fn request(
        &self,
        method: Method,
        url: &Url,
        headers: &[(HeaderName, HeaderValue)],
        body: RequestBody,
    ) -> anyhow::Result<Response<Incoming>> {
        // Go decides this on the original request, not on each hop.
        let replayable = !matches!(body, RequestBody::Stream(_));
        // None once a 301, 302 or 303 has dropped the body.
        let mut body = Some(body);
        let mut url = url.clone();
        let mut method = method;
        let mut sent = 0;

        loop {
            // Go's error. The connectors cannot be left to it: hyper-tls
            // sends every scheme but https as plain HTTP.
            if !matches!(url.scheme(), "http" | "https") {
                bail!("unsupported protocol scheme {:?}", url.scheme());
            }
            let uri: Uri = url
                .as_str()
                .parse()
                .with_context(|| format!("invalid URL: {url}"))?;

            let mut builder = Request::builder().method(method.clone()).uri(uri);
            builder = builder.header(USER_AGENT, self.user_agent.clone());
            for (name, value) in headers {
                // With the body dropped, a Content-Type would describe nothing.
                // Go strips it too, and a strict server or a WAF can refuse a
                // GET that claims one.
                if body.is_none() && name == CONTENT_TYPE {
                    continue;
                }
                builder = builder.header(name.clone(), value.clone());
            }

            let outgoing = match &mut body {
                Some(RequestBody::Bytes(bytes)) => full_body(bytes.clone()),
                // What stays behind is never sent: a 307 or 308 answering a
                // stream is returned, and a 301, 302 or 303 drops the body.
                Some(RequestBody::Stream(stream)) => std::mem::replace(stream, empty_body()),
                Some(RequestBody::Empty) | None => empty_body(),
            };
            let resp = self.inner.request(builder.body(outgoing)?).await?;
            sent += 1;

            let keeps_body = match resp.status() {
                StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND | StatusCode::SEE_OTHER => false,
                StatusCode::TEMPORARY_REDIRECT | StatusCode::PERMANENT_REDIRECT if replayable => {
                    true
                }
                _ => return Ok(resp),
            };

            let Some(location) = resp.headers().get(LOCATION).filter(|l| !l.is_empty()) else {
                return Ok(resp);
            };
            let location = location.to_str().context("invalid Location header")?;
            let next = url.join(location).context("invalid redirect target")?;

            // Never let a redirect drop TLS. Whoever asked for https asked for
            // it for the whole exchange, and a server that answers one with a
            // Location on http would put the rest of it on the wire in clear.
            // Go follows the downgrade, leaving out only the Referer.
            if url.scheme() == "https" && next.scheme() == "http" {
                bail!("refusing to follow a redirect from https to http");
            }

            if sent == MAX_REDIRECTS {
                bail!("stopped after {MAX_REDIRECTS} redirects");
            }

            if !keeps_body {
                method = Method::GET;
                body = None;
            } else if matches!(body, Some(RequestBody::Bytes(_))) && !same_origin(&url, &next) {
                // Stricter than Go, which would send the body on: the
                // telemetry POST carries the results, the client's IP address
                // and its ISP, and must not go wherever a redirect points. A
                // request without a body has nothing to give away and is
                // followed, as a server list redirected to https needs.
                bail!(
                    "refusing to send a {method} body to another origin ({} -> {})",
                    origin_of(&url),
                    origin_of(&next)
                );
            }
            url = next;
        }
    }

    /// Performs a GET request and reads the whole response body.
    pub async fn get_bytes(&self, url: &Url) -> anyhow::Result<(StatusCode, Bytes)> {
        let fut = async {
            let resp = self
                .request(Method::GET, url, &[], RequestBody::Empty)
                .await?;
            let status = resp.status();
            let body = collect_limited(resp.into_body(), MAX_BUFFERED_RESPONSE).await?;
            Ok::<_, anyhow::Error>((status, body))
        };

        self.with_timeout(fut).await
    }

    /// Fetches a URL and discards the body, returning the status.
    ///
    /// For requests whose body is never read -- the latency probe fires one of
    /// these `count` times per server -- so a server cannot make the client
    /// hold what it sends.
    pub async fn get_drained(&self, url: &Url) -> anyhow::Result<(StatusCode, ConnectionFacts)> {
        let fut = async {
            let resp = self
                .request(Method::GET, url, &[], RequestBody::Empty)
                .await?;
            let status = resp.status();
            let facts = ConnectionFacts::of(&resp);
            drain_body(resp.into_body()).await?;
            Ok::<_, anyhow::Error>((status, facts))
        };

        self.with_timeout(fut).await
    }

    /// Fetches a URL, keeping at most the first `limit` bytes of the body.
    ///
    /// Enough to tell an empty body from a full one and to quote what came
    /// back, without letting its size decide what that costs: reading stops
    /// once `limit` bytes are kept, so an endless body returns too, with no
    /// timeout set. The rest is dropped unread, which closes an HTTP/1.1
    /// connection instead of returning it to the pool. The backend probe can
    /// afford that, because a body at all already marks the backend down.
    pub async fn get_prefix(
        &self,
        url: &Url,
        limit: usize,
    ) -> anyhow::Result<(StatusCode, Bytes, ConnectionFacts)> {
        let fut = async {
            let resp = self
                .request(Method::GET, url, &[], RequestBody::Empty)
                .await?;
            let status = resp.status();
            let facts = ConnectionFacts::of(&resp);
            let body = body_prefix(resp.into_body(), limit).await?;
            Ok::<_, anyhow::Error>((status, body, facts))
        };

        self.with_timeout(fut).await
    }

    /// Posts a body and reads the whole response, used for telemetry.
    pub async fn post_bytes(
        &self,
        url: &Url,
        content_type: &str,
        body: Bytes,
    ) -> anyhow::Result<(StatusCode, Bytes)> {
        let headers = vec![(CONTENT_TYPE, HeaderValue::from_str(content_type)?)];
        let fut = async {
            let resp = self
                .request(Method::POST, url, &headers, RequestBody::Bytes(body))
                .await?;
            let status = resp.status();
            let out = collect_limited(resp.into_body(), MAX_TELEMETRY_RESPONSE).await?;
            Ok::<_, anyhow::Error>((status, out))
        };

        self.with_timeout(fut).await
    }

    /// Sends a request without buffering the response body, for the transfer tests.
    pub async fn send_streaming(
        &self,
        method: Method,
        url: &Url,
        body: RequestBody,
    ) -> anyhow::Result<Response<Incoming>> {
        // Speed tests must measure the wire, not a decompressed stream.
        let headers = vec![(ACCEPT_ENCODING, HeaderValue::from_static("identity"))];
        self.request(method, url, &headers, body).await
    }
}

#[cfg(test)]
mod facts_tests {
    use super::*;

    /// The facts must come off the response the request was served on, which
    /// is how they end up describing that connection rather than another.
    #[test]
    fn facts_are_read_from_the_response_a_request_was_served_on() {
        let peer: std::net::SocketAddr = "192.0.2.7:443".parse().unwrap();
        let mut resp = Response::new(());
        resp.extensions_mut().insert(connector::PeerAddr(peer));
        resp.extensions_mut().insert(TlsFacts {
            version: "TLS 1.3".into(),
            cipher: "TLS_AES_128_GCM_SHA256".into(),
        });

        let facts = ConnectionFacts::of(&resp);
        assert_eq!(facts.peer, Some(peer));
        let tls = facts
            .tls
            .expect("the TLS pair travelled with the connection");
        assert_eq!(tls.version, "TLS 1.3");
        assert_eq!(tls.cipher, "TLS_AES_128_GCM_SHA256");
    }

    /// A plain HTTP response carries no TLS pair, and the report must then
    /// leave the field out rather than invent one.
    #[test]
    fn a_response_without_tls_reports_none() {
        let resp = Response::new(());
        let facts = ConnectionFacts::of(&resp);
        assert!(facts.tls.is_none());
        assert!(facts.peer.is_none());
    }
}

#[cfg(test)]
mod origin_tests {
    use super::*;

    /// Resolves `location` against `from` as `request` does, and says whether
    /// a body may follow it there.
    fn body_may_follow(from: &str, location: &str) -> bool {
        let from = Url::parse(from).unwrap();
        let next = from.join(location).unwrap();
        let same = same_origin(&from, &next);
        assert_eq!(
            same,
            same_origin(&next, &from),
            "{from} and {next} compare differently the other way round"
        );
        same
    }

    #[test]
    fn locations_on_the_same_origin() {
        for (from, location) in [
            // Relative references keep the scheme, host and port.
            ("http://example.com/a/b", "/next"),
            ("http://example.com/a/b", "next"),
            ("http://example.com/a/b", "../next"),
            ("http://example.com/a/b", "?again=1"),
            ("http://example.com:8080/a/b", "//example.com:8080/next"),
            ("http://example.com/a/b", "http:next"),
            // The default port, written out or not, on either side.
            ("http://example.com/", "http://example.com:80/next"),
            ("http://example.com:80/", "http://example.com/next"),
            ("https://example.com/", "https://example.com:443/next"),
            ("https://example.com:443/", "https://example.com/next"),
            ("http://example.com:8080/", "http://example.com:8080/next"),
            // Scheme and host are lowercased when a URL is parsed.
            ("http://example.com/", "http://EXAMPLE.com/next"),
            ("http://Example.COM/", "http://example.com/next"),
            ("http://example.com/", "HTTP://example.com/next"),
            // So are the other spellings of one host made one.
            ("http://example.com/", "http://ex%61mple.com/next"),
            (
                "http://xn--bcher-kva.example/",
                "http://b\u{fc}cher.example/next",
            ),
            ("http://127.0.0.1/", "http://127.1/next"),
            ("http://127.0.0.1/", "http://0x7f.0.0.1/next"),
            ("http://127.0.0.1/", "http://2130706433/next"),
            ("http://[::1]:8080/", "http://[::1]:8080/next"),
            ("http://[::1]:8080/", "http://[0:0:0:0:0:0:0:1]:8080/next"),
            ("http://[2001:db8::a]/", "http://[2001:DB8:0::A]:80/next"),
            // User information is no part of an origin: the host is the same.
            ("http://example.com/", "http://user:secret@example.com/next"),
            ("http://user@example.com/", "http://example.com/next"),
            // The host ends where the path, query or fragment begins, and a
            // backslash begins the path as a slash does.
            ("http://example.com/", "http://example.com/@example.org/"),
            ("http://example.com/", "http://example.com?@example.org/"),
            ("http://example.com/", "http://example.com#@example.org/"),
            ("http://example.com/", "http://example.com\\@example.org/"),
        ] {
            assert!(
                body_may_follow(from, location),
                "{from} -> {location} left the origin"
            );
        }
    }

    #[test]
    fn locations_on_another_origin() {
        for (from, location) in [
            // Another scheme, with the port written out or not.
            ("http://example.com/", "https://example.com/next"),
            ("https://example.com/", "http://example.com/next"),
            ("http://example.com/", "https://example.com:80/next"),
            ("https://example.com/", "http://example.com:443/next"),
            ("http://example.com/", "https:/next"),
            ("http://example.com/", "ftp://example.com/next"),
            ("http://example.com/", "file:///etc/passwd"),
            ("http://example.com/", "data:text/plain,next"),
            // Another port.
            ("http://example.com/", "http://example.com:8080/next"),
            ("http://example.com:8080/", "http://example.com/next"),
            ("http://example.com:8080/", "http://example.com:8081/next"),
            ("http://example.com/", "http://example.com:443/next"),
            ("https://example.com/", "https://example.com:80/next"),
            ("http://[::1]:8080/", "http://[::1]:8081/next"),
            // Another host.
            ("http://example.com/", "http://example.org/next"),
            ("http://example.com/", "http://www.example.com/next"),
            ("http://example.com/", "//example.org/next"),
            ("http://[::1]/", "http://[::2]/next"),
            // A trailing dot is kept, a name is not the address it resolves
            // to, and an IPv4-mapped address is not the IPv4 address.
            ("http://example.com/", "http://example.com./next"),
            ("http://127.0.0.1/", "http://localhost/next"),
            ("http://127.0.0.1/", "http://[::ffff:127.0.0.1]/next"),
            ("http://127.0.0.1/", "http://[::1]/next"),
            // The host is what follows the user information, and backslashes
            // count as slashes.
            ("http://example.com/", "http://example.com@example.org/next"),
            (
                "http://example.com/",
                "http://example.com:80@example.org/next",
            ),
            ("http://example.com/", "/\\example.org/next"),
            ("http://example.com/", "\\\\example.org/next"),
        ] {
            assert!(
                !body_may_follow(from, location),
                "{from} -> {location} stayed on the origin"
            );
        }
    }

    /// `request` turns these into an error before anything is sent on.
    #[test]
    fn locations_that_are_no_url() {
        let from = Url::parse("http://example.com/").unwrap();
        for location in [
            "http://",
            "http://[::1",
            "http://[::1]:port/",
            "http://exa mple.com/",
            "http://example.com:65536/",
            "http://user@/next",
        ] {
            assert!(from.join(location).is_err(), "{location} parsed");
        }
    }
}

#[cfg(test)]
mod request_tests {
    use super::*;

    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    const TIMEOUT: Duration = Duration::from_secs(10);

    /// A request as a test server received it.
    #[derive(Clone, Debug, Default)]
    struct Seen {
        method: String,
        path: String,
        content_type: Option<String>,
        authorization: Option<String>,
        body: Vec<u8>,
    }

    type Log = Arc<Mutex<Vec<Seen>>>;
    type Answer = Arc<dyn Fn(&Seen) -> String + Send + Sync>;

    fn client(timeout: Duration) -> HttpClient {
        HttpClient::new(
            BindOptions::default(),
            &TlsSettings {
                ca_cert: None,
                skip_verify: true,
                http2: false,
            },
            timeout,
            1,
            "test",
        )
        .unwrap()
    }

    /// A response head with an empty body.
    fn reply(status: u16, location: Option<&str>) -> String {
        let location = location
            .map(|l| format!("Location: {l}\r\n"))
            .unwrap_or_default();
        format!(
            "HTTP/1.1 {status} Test\r\n{location}Content-Length: 0\r\nConnection: close\r\n\r\n"
        )
    }

    /// What a request head says: the request without its body, and how long
    /// that body is.
    fn parse_head(head: &[u8]) -> (Seen, usize) {
        let head = String::from_utf8_lossy(head);
        let mut lines = head.lines();
        let mut start = lines.next().unwrap_or_default().split_whitespace();
        let mut seen = Seen {
            method: start.next().unwrap_or_default().to_string(),
            path: start.next().unwrap_or_default().to_string(),
            ..Seen::default()
        };
        let mut length = 0;
        for (name, value) in lines.filter_map(|l| l.split_once(':')) {
            let value = value.trim();
            if name.eq_ignore_ascii_case("content-type") {
                seen.content_type = Some(value.to_string());
            } else if name.eq_ignore_ascii_case("authorization") {
                seen.authorization = Some(value.to_string());
            } else if name.eq_ignore_ascii_case("content-length") {
                length = value.parse().unwrap_or(0);
            }
        }
        (seen, length)
    }

    /// Reads one request: its head and a body of the declared Content-Length.
    async fn read_request(stream: &mut TcpStream) -> io::Result<Seen> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let head_len = loop {
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            buf.extend_from_slice(&chunk[..n]);
        };

        let (mut seen, length) = parse_head(&buf[..head_len]);
        seen.body = buf.split_off(head_len);
        while seen.body.len() < length {
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            seen.body.extend_from_slice(&chunk[..n]);
        }
        Ok(seen)
    }

    /// A listener on `addr` and the URL it is reached at.
    async fn listen(addr: &str) -> io::Result<(TcpListener, Url)> {
        let listener = TcpListener::bind(addr).await?;
        let url = Url::parse(&format!("http://{}/", listener.local_addr()?)).unwrap();
        Ok((listener, url))
    }

    /// Serves one request per connection on `listener`, logging each before
    /// answering it with `answer`.
    fn serve_on(listener: TcpListener, answer: Answer, log: Log) {
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let (answer, log) = (answer.clone(), log.clone());
                tokio::spawn(async move {
                    let Ok(req) = read_request(&mut stream).await else {
                        return;
                    };
                    let response = answer(&req);
                    log.lock().unwrap().push(req);
                    let _ = stream.write_all(response.as_bytes()).await;
                    // Let the client hang up first, so closing cannot reset
                    // the response away.
                    let mut rest = [0u8; 1024];
                    while matches!(stream.read(&mut rest).await, Ok(n) if n > 0) {}
                });
            }
        });
    }

    /// Serves on a local port as `serve_on` does. Returns the server's URL
    /// and its log.
    async fn serve(answer: impl Fn(&Seen) -> String + Send + Sync + 'static) -> (Url, Log) {
        let (listener, url) = listen("127.0.0.1:0").await.unwrap();
        let log = Log::default();
        serve_on(listener, Arc::new(answer), log.clone());
        (url, log)
    }

    fn requests(log: &Log) -> Vec<Seen> {
        log.lock().unwrap().clone()
    }

    const BODY: &[u8] = b"results";

    /// Posts `BODY` the way the telemetry upload does.
    async fn post(url: &Url) -> anyhow::Result<(StatusCode, Bytes)> {
        client(TIMEOUT)
            .post_bytes(url, "text/plain", Bytes::from_static(BODY))
            .await
    }

    /// The host and port of a test server's URL, as a Location spells them.
    fn authority(url: &Url) -> String {
        format!("{}:{}", url.host_str().unwrap(), url.port().unwrap())
    }

    /// The method and path of each logged request, and whether it had a body.
    fn summary(log: &Log) -> Vec<(String, String, bool)> {
        requests(log)
            .into_iter()
            .map(|r| (r.method, r.path, !r.body.is_empty()))
            .collect()
    }

    fn posted(path: &str) -> (String, String, bool) {
        ("POST".into(), path.into(), true)
    }

    fn got(path: &str) -> (String, String, bool) {
        ("GET".into(), path.into(), false)
    }

    /// Asserts that `result` is the refusal to send a body to another origin.
    fn assert_refused(result: anyhow::Result<(StatusCode, Bytes)>, what: &str) {
        let err = match result {
            Ok((status, _)) => panic!("{what}: the body was sent on, ending in {status}"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("another origin"),
            "{what}: {err:#}"
        );
    }

    #[tokio::test]
    async fn a_301_302_or_303_turns_a_post_into_a_get_without_body_or_content_type() {
        for status in [301, 302, 303] {
            let (url, log) = serve(move |req| match req.path.as_str() {
                "/start" => reply(status, Some("/next")),
                _ => reply(200, None),
            })
            .await;

            let (got, _) = client(TIMEOUT)
                .post_bytes(
                    &url.join("start").unwrap(),
                    "text/plain",
                    Bytes::from_static(b"results"),
                )
                .await
                .unwrap();

            assert_eq!(got, StatusCode::OK, "{status} was not followed");
            let seen = requests(&log);
            assert_eq!(seen.len(), 2, "{status}: {seen:?}");
            assert_eq!(seen[0].method, "POST");
            assert_eq!(seen[0].body, b"results");
            assert_eq!(seen[0].content_type.as_deref(), Some("text/plain"));
            assert_eq!(
                (seen[1].method.as_str(), seen[1].path.as_str()),
                ("GET", "/next")
            );
            assert!(seen[1].body.is_empty(), "{status} kept the body");
            assert_eq!(seen[1].content_type, None, "{status} kept the Content-Type");
        }
    }

    #[tokio::test]
    async fn a_307_or_308_sends_a_bytes_body_again_to_the_same_origin() {
        for status in [307, 308] {
            let (url, log) = serve(move |req| match req.path.as_str() {
                "/start" => reply(status, Some("/next")),
                _ => reply(200, None),
            })
            .await;

            let (got, _) = client(TIMEOUT)
                .post_bytes(
                    &url.join("start").unwrap(),
                    "text/plain",
                    Bytes::from_static(b"results"),
                )
                .await
                .unwrap();

            assert_eq!(got, StatusCode::OK, "{status} was not followed");
            let seen = requests(&log);
            assert_eq!(seen.len(), 2, "{status}: {seen:?}");
            assert_eq!(
                (seen[1].method.as_str(), seen[1].path.as_str()),
                ("POST", "/next")
            );
            assert_eq!(seen[1].body, b"results", "{status} lost the body");
            assert_eq!(seen[1].content_type.as_deref(), Some("text/plain"));
        }
    }

    /// Stricter than Go on purpose: a body is not sent to another origin. A
    /// request without one is still followed there.
    #[tokio::test]
    async fn a_307_or_308_does_not_send_a_body_to_another_origin() {
        for status in [307, 308] {
            let (other, other_log) = serve(|_| reply(200, None)).await;
            let target = other.join("next").unwrap().to_string();
            let (url, _) = serve(move |_| reply(status, Some(&target))).await;
            let client = client(TIMEOUT);

            let err = client
                .post_bytes(&url, "text/plain", Bytes::from_static(b"results"))
                .await
                .expect_err("the body was allowed to another origin");
            assert!(
                err.to_string().contains("another origin"),
                "{status}: {err:#}"
            );
            assert!(
                requests(&other_log).is_empty(),
                "{status} reached the other origin"
            );

            let (got, _) = client.get_bytes(&url).await.unwrap();
            assert_eq!(
                got,
                StatusCode::OK,
                "{status} without a body was not followed"
            );
            assert_eq!(requests(&other_log)[0].method, "GET");
        }
    }

    /// An absolute Location naming the origin the request went to is as good
    /// as a relative one, however it spells that origin. User information is
    /// no part of an origin and is sent nowhere: no Authorization header is
    /// made of it, where Go would make one.
    #[tokio::test]
    async fn a_307_or_308_sends_a_body_to_any_spelling_of_the_same_origin() {
        let (listener, url) = listen("127.0.0.1:0").await.unwrap();
        let host = authority(&url);
        let cases = [
            ("/next".to_string(), "/next"),
            ("next".to_string(), "/a/b/next"),
            ("../next".to_string(), "/a/next"),
            ("?again=1".to_string(), "/a/b/start?again=1"),
            (format!("http://{host}/next"), "/next"),
            (format!("HTTP://{host}/next"), "/next"),
            (format!("//{host}/next"), "/next"),
            (format!("http://user:secret@{host}/next"), "/next"),
        ];
        let locations: Vec<String> = cases.iter().map(|(l, _)| l.clone()).collect();
        let log = Log::default();
        let answer = move |req: &Seen| match req.path.strip_prefix("/a/b/start?case=") {
            Some(case) => {
                let (status, i) = case.split_once('-').unwrap();
                let location = &locations[i.parse::<usize>().unwrap()];
                reply(status.parse().unwrap(), Some(location))
            }
            None => reply(200, None),
        };
        serve_on(listener, Arc::new(answer), log.clone());

        for status in [307, 308] {
            for (i, (location, path)) in cases.iter().enumerate() {
                log.lock().unwrap().clear();
                let start = format!("/a/b/start?case={status}-{i}");

                let (got, _) = post(&url.join(&start).unwrap())
                    .await
                    .unwrap_or_else(|e| panic!("{status} to {location}: {e:#}"));

                assert_eq!(got, StatusCode::OK, "{status} to {location}");
                assert_eq!(
                    summary(&log),
                    [posted(&start), posted(path)],
                    "{status} to {location}"
                );
                let seen = requests(&log);
                assert_eq!(seen[1].body, BODY, "{status} to {location}");
                assert_eq!(seen[1].content_type.as_deref(), Some("text/plain"));
                assert_eq!(seen[1].authorization, None, "{status} to {location}");
            }
        }
    }

    /// Go turns the user information of a URL into an Authorization header.
    /// Here it is dropped, from the URL a request starts at as well, so a
    /// Location cannot make the client send credentials either.
    #[tokio::test]
    async fn user_information_in_a_url_is_not_sent() {
        let (url, log) = serve(|_| reply(200, None)).await;
        let mut with_user = url.clone();
        with_user.set_username("user").unwrap();
        with_user.set_password(Some("secret")).unwrap();

        let (got, _) = client(TIMEOUT).get_bytes(&with_user).await.unwrap();

        assert_eq!(got, StatusCode::OK);
        assert_eq!(requests(&log)[0].authorization, None);
    }

    /// Every way of naming another origin is refused before anything is sent
    /// there, including the ones that only look like the first origin.
    #[tokio::test]
    async fn a_307_or_308_does_not_send_a_body_to_any_spelling_of_another_origin() {
        let (other, other_log) = serve(|_| reply(200, None)).await;
        let (listener, url) = listen("127.0.0.1:0").await.unwrap();
        let (host, port) = (authority(&url), url.port().unwrap());
        let elsewhere = authority(&other);
        let locations = [
            // Another port, spelled in full and relative to the scheme.
            format!("http://{elsewhere}/next"),
            format!("//{elsewhere}/next"),
            // Backslashes are read as slashes, so these name a host too.
            format!("/\\{elsewhere}/next"),
            format!("\\\\{elsewhere}/next"),
            // The first origin as the user information of another.
            format!("http://{host}@{elsewhere}/next"),
            // The same host and port under another scheme.
            format!("https://{host}/next"),
            format!("ftp://{host}/next"),
            // The same port of the same machine under another name.
            format!("http://localhost:{port}/next"),
            format!("http://[::ffff:127.0.0.1]:{port}/next"),
        ];
        let sent = locations.clone();
        let log = Log::default();
        let answer = move |req: &Seen| match req.path.strip_prefix("/start?case=") {
            Some(case) => {
                let (status, i) = case.split_once('-').unwrap();
                let location = &sent[i.parse::<usize>().unwrap()];
                reply(status.parse().unwrap(), Some(location))
            }
            None => reply(200, None),
        };
        serve_on(listener, Arc::new(answer), log.clone());

        for status in [307, 308] {
            for (i, location) in locations.iter().enumerate() {
                log.lock().unwrap().clear();
                let start = format!("/start?case={status}-{i}");

                let result = post(&url.join(&start).unwrap()).await;

                assert_refused(result, &format!("{status} to {location}"));
                assert_eq!(summary(&log), [posted(&start)], "{status} to {location}");
                assert_eq!(summary(&other_log), [], "{status} to {location}");
            }
        }
    }

    /// The scheme or the port alone can make another origin, so the error
    /// names both origins in full. It was the two hosts, which then read the
    /// same. Nothing else of the Location is repeated.
    #[tokio::test]
    async fn a_refused_redirect_names_both_origins() {
        let (listener, url) = listen("127.0.0.1:0").await.unwrap();
        let host = authority(&url);
        let cases = [
            (format!("https://{host}/next"), format!("https://{host}")),
            (
                "https://user:secret@LOCALHOST:443/next?secret".to_string(),
                "https://localhost".to_string(),
            ),
            ("//[::1]:81/next".to_string(), "http://[::1]:81".to_string()),
        ];
        let locations: Vec<String> = cases.iter().map(|(l, _)| l.clone()).collect();
        let answer = move |req: &Seen| {
            let i: usize = req.path[1..].parse().unwrap();
            reply(307, Some(&locations[i]))
        };
        serve_on(listener, Arc::new(answer), Log::default());

        for (i, (location, origin)) in cases.iter().enumerate() {
            let err = post(&url.join(&i.to_string()).unwrap())
                .await
                .expect_err(location);
            assert_eq!(
                err.to_string(),
                format!(
                    "refusing to send a POST body to another origin (http://{host} -> {origin})"
                )
            );
        }
    }

    /// The same over IPv6, where one address has several spellings.
    #[tokio::test]
    async fn a_307_or_308_compares_ipv6_origins_by_address_and_port() {
        let (Ok((listener, url)), Ok((other_listener, other))) =
            (listen("[::1]:0").await, listen("[::1]:0").await)
        else {
            eprintln!("skipped: cannot listen on [::1]");
            return;
        };
        let (port, other_port) = (url.port().unwrap(), other.port().unwrap());
        let other_log = Log::default();
        serve_on(
            other_listener,
            Arc::new(|_: &Seen| reply(200, None)),
            other_log.clone(),
        );
        let same = [
            format!("http://[::1]:{port}/next"),
            format!("http://[0:0:0:0:0:0:0:1]:{port}/next"),
            format!("http://[0000::0001]:{port}/next"),
        ];
        let different = [
            format!("http://[::1]:{other_port}/next"),
            format!("http://[0:0:0:0:0:0:0:1]:{other_port}/next"),
            format!("http://127.0.0.1:{port}/next"),
        ];
        let locations: Vec<String> = same.iter().chain(&different).cloned().collect();
        let log = Log::default();
        let answer = move |req: &Seen| match req.path.strip_prefix("/start?case=") {
            Some(case) => {
                let (status, i) = case.split_once('-').unwrap();
                let location = &locations[i.parse::<usize>().unwrap()];
                reply(status.parse().unwrap(), Some(location))
            }
            None => reply(200, None),
        };
        serve_on(listener, Arc::new(answer), log.clone());

        for status in [307, 308] {
            for (i, location) in same.iter().chain(&different).enumerate() {
                log.lock().unwrap().clear();
                let start = format!("/start?case={status}-{i}");

                let result = post(&url.join(&start).unwrap()).await;

                if i < same.len() {
                    let (got, _) =
                        result.unwrap_or_else(|e| panic!("{status} to {location}: {e:#}"));
                    assert_eq!(got, StatusCode::OK, "{status} to {location}");
                    assert_eq!(
                        summary(&log),
                        [posted(&start), posted("/next")],
                        "{status} to {location}"
                    );
                } else {
                    assert_refused(result, &format!("{status} to {location}"));
                    assert_eq!(summary(&log), [posted(&start)], "{status} to {location}");
                }
                assert_eq!(summary(&other_log), [], "{status} to {location}");
            }
        }
    }

    /// Listeners on one port of both loopback addresses, or of 127.0.0.1 alone
    /// where ::1 is unavailable, so that `localhost` leads here whichever of
    /// its addresses is dialled.
    async fn listen_on_localhost() -> (Vec<TcpListener>, u16) {
        loop {
            let (v4, url) = listen("127.0.0.1:0").await.unwrap();
            let port = url.port().unwrap();
            match TcpListener::bind(("::1", port)).await {
                Ok(v6) => return (vec![v4, v6], port),
                // Someone else has that port of ::1: take another.
                Err(e) if e.kind() == io::ErrorKind::AddrInUse => continue,
                Err(_) => return (vec![v4], port),
            }
        }
    }

    /// A host name is lowercased when a URL is parsed, so its case cannot make
    /// another origin of it.
    #[tokio::test]
    async fn a_307_or_308_ignores_the_case_of_a_host_name() {
        let (listeners, port) = listen_on_localhost().await;
        let ours: Vec<std::net::IpAddr> = listeners
            .iter()
            .map(|l| l.local_addr().unwrap().ip())
            .collect();
        let resolved: Vec<std::net::IpAddr> = tokio::net::lookup_host(("localhost", port))
            .await
            .map(|found| found.map(|a| a.ip()).collect())
            .unwrap_or_default();
        if resolved.is_empty() || !resolved.iter().all(|ip| ours.contains(ip)) {
            eprintln!("skipped: localhost resolves to {resolved:?}, listening on {ours:?}");
            return;
        }

        let log = Log::default();
        let answer: Answer = Arc::new(move |req: &Seen| match req.path.as_str() {
            "/307" => reply(307, Some(&format!("http://LOCALHOST:{port}/next"))),
            "/308" => reply(308, Some(&format!("http://LocalHost:{port}/next"))),
            _ => reply(200, None),
        });
        for listener in listeners {
            serve_on(listener, answer.clone(), log.clone());
        }

        for start in ["/307", "/308"] {
            log.lock().unwrap().clear();
            let url = Url::parse(&format!("http://localhost:{port}{start}")).unwrap();

            let (got, _) = post(&url).await.unwrap();

            assert_eq!(got, StatusCode::OK, "from {start}");
            assert_eq!(summary(&log), [posted(start), posted("/next")]);
        }
    }

    /// With a body to send again, a Location that is missing or empty makes
    /// the redirect the response, and one that is no URL makes it an error.
    /// Nothing is sent on in either case.
    #[tokio::test]
    async fn a_307_or_308_with_a_body_and_no_usable_location_goes_nowhere() {
        const BROKEN: [&str; 5] = [
            "http://[::1",
            "http://exa mple.com/",
            "http://127.0.0.1:65536/",
            "http://user@/next",
            "http://",
        ];
        let (url, log) = serve(|req| {
            let (status, case) = req.path[1..].split_once('-').unwrap();
            let status = status.parse().unwrap();
            match case {
                "missing" => reply(status, None),
                "empty" => reply(status, Some("")),
                // Not the visible ASCII a header value is read as text from.
                "unreadable" => reply(status, Some("/n\u{e9}xt")),
                i => reply(status, Some(BROKEN[i.parse::<usize>().unwrap()])),
            }
        })
        .await;

        for status in [307, 308] {
            for case in ["missing", "empty"] {
                let (got, _) = post(&url.join(&format!("{status}-{case}")).unwrap())
                    .await
                    .unwrap();
                assert_eq!(got.as_u16(), status, "{case} Location");
            }

            let err = post(&url.join(&format!("{status}-unreadable")).unwrap())
                .await
                .expect_err("a Location that is not ASCII was followed");
            assert_eq!(err.to_string(), "invalid Location header");

            for (i, location) in BROKEN.iter().enumerate() {
                let err = post(&url.join(&format!("{status}-{i}")).unwrap())
                    .await
                    .expect_err(location);
                assert_eq!(err.to_string(), "invalid redirect target", "{location}");
            }
        }
        let seen = summary(&log);
        assert_eq!(seen.len(), 2 * (3 + BROKEN.len()), "{seen:?}");
        assert!(seen.iter().all(|(_, path, _)| path != "/next"), "{seen:?}");
    }

    /// Each hop is judged against the one before it, so a redirect that stays
    /// on the origin first does not open the way to another origin after it.
    #[tokio::test]
    async fn a_307_or_308_chain_stops_where_it_leaves_the_origin() {
        for status in [307, 308] {
            let (third, third_log) = serve(|_| reply(200, None)).await;
            let (second, second_log) = {
                let target = third.join("third").unwrap().to_string();
                serve(move |_| reply(status, Some(&target))).await
            };
            let target = second.join("second").unwrap().to_string();
            let (first, first_log) = serve(move |req| match req.path.as_str() {
                "/start" => reply(status, Some("/hop")),
                "/hop" => reply(status, Some("/again")),
                _ => reply(status, Some(&target)),
            })
            .await;

            let result = post(&first.join("start").unwrap()).await;

            assert_refused(result, &format!("a {status} chain"));
            assert_eq!(
                summary(&first_log),
                [posted("/start"), posted("/hop"), posted("/again")],
                "{status}"
            );
            assert_eq!(summary(&second_log), [], "{status}");
            assert_eq!(summary(&third_log), [], "{status}");
        }
    }

    /// As in Go: a 301, 302 or 303 may lead anywhere, because what follows it
    /// is a GET with nothing of the POST in it, and that stays so when a 307
    /// or 308 comes next. The body is gone, not held back.
    #[tokio::test]
    async fn a_301_302_or_303_drops_the_body_for_the_rest_of_the_chain() {
        for status in [301, 302, 303] {
            for then in [307, 308] {
                let (third, third_log) = serve(|_| reply(200, None)).await;
                let (second, second_log) = {
                    let target = third.join("third").unwrap().to_string();
                    serve(move |_| reply(then, Some(&target))).await
                };
                let target = second.join("second").unwrap().to_string();
                let (first, first_log) = serve(move |_| reply(status, Some(&target))).await;

                let (end, _) = post(&first.join("start").unwrap())
                    .await
                    .unwrap_or_else(|e| panic!("{status} then {then}: {e:#}"));

                assert_eq!(end, StatusCode::OK, "{status} then {then}");
                assert_eq!(summary(&first_log), [posted("/start")]);
                assert_eq!(summary(&second_log), [got("/second")], "{status}");
                assert_eq!(summary(&third_log), [got("/third")], "{then}");
                for seen in requests(&second_log).iter().chain(&requests(&third_log)) {
                    assert_eq!(seen.content_type, None, "{status} then {then}");
                }
            }
        }
    }

    /// Only http and https are requested, wherever the URL comes from. Left
    /// to the connector, the native-tls build sent these as plain HTTP.
    #[tokio::test]
    async fn a_url_of_another_scheme_is_an_error() {
        let (other, other_log) = serve(|_| reply(200, None)).await;
        let client = client(TIMEOUT);

        for scheme in ["ftp", "ws", "gopher"] {
            let target = format!("{scheme}://{}/next", authority(&other));
            let expected = format!("unsupported protocol scheme {scheme:?}");

            let (url, log) = serve({
                let target = target.clone();
                move |_| reply(302, Some(&target))
            })
            .await;
            let err = client
                .get_bytes(&url)
                .await
                .expect_err("the redirect was followed");
            assert_eq!(err.to_string(), expected);
            assert_eq!(summary(&log), [got("/")]);

            let err = client
                .get_bytes(&Url::parse(&target).unwrap())
                .await
                .expect_err("the URL was requested");
            assert_eq!(err.to_string(), expected);

            assert_eq!(summary(&other_log), [], "{scheme}");
        }
    }

    /// A stream cannot be sent twice, so its 307 or 308 is the response. Go
    /// judges the original request, so that holds after a 303 dropped it too.
    #[tokio::test]
    async fn a_307_or_308_answering_a_stream_body_is_returned() {
        for status in [307, 308] {
            let (url, log) = serve(move |req| match req.path.as_str() {
                "/upload" => reply(status, Some("/elsewhere")),
                "/see-other" => reply(303, Some("/upload")),
                _ => reply(200, None),
            })
            .await;
            let client = client(TIMEOUT);
            let stream = || RequestBody::Stream(full_body(Bytes::from_static(b"upload")));

            for (start, sent) in [("upload", 1), ("see-other", 2)] {
                log.lock().unwrap().clear();
                let resp = client
                    .send_streaming(Method::POST, &url.join(start).unwrap(), stream())
                    .await
                    .unwrap();

                assert_eq!(resp.status().as_u16(), status, "from /{start}");
                let seen = requests(&log);
                assert_eq!(seen.len(), sent, "from /{start}: {seen:?}");
                assert_eq!(seen[0].body, b"upload");
            }
        }
    }

    #[tokio::test]
    async fn the_tenth_redirect_is_an_error() {
        let (url, log) = serve(|req| reply(302, Some(&format!("{}x", req.path)))).await;

        let err = client(TIMEOUT)
            .get_bytes(&url)
            .await
            .expect_err("an endless redirect chain ended");

        assert_eq!(err.to_string(), "stopped after 10 redirects");
        assert_eq!(requests(&log).len(), 10);
    }

    #[tokio::test]
    async fn other_3xx_and_redirects_without_a_location_are_returned() {
        let (url, log) = serve(|req| match req.path.as_str() {
            "/300" => reply(300, Some("/next")),
            "/304" => reply(304, Some("/next")),
            "/empty-location" => reply(302, Some("")),
            "/no-location" => reply(302, None),
            _ => reply(200, None),
        })
        .await;
        let client = client(TIMEOUT);

        for (path, status) in [
            ("300", 300),
            ("304", 304),
            ("empty-location", 302),
            ("no-location", 302),
        ] {
            let (got, _) = client.get_bytes(&url.join(path).unwrap()).await.unwrap();
            assert_eq!(got.as_u16(), status, "/{path} was followed");
        }
        assert_eq!(requests(&log).len(), 4, "{:?}", requests(&log));
    }

    /// A request made over https stays there: the redirect to http is an
    /// error and the plaintext server is never reached, whether the downgrade
    /// is the first hop or comes after one that kept the scheme. Go follows
    /// it, leaving out only the Referer.
    #[cfg(feature = "rustls-tls")]
    #[tokio::test]
    async fn a_redirect_from_https_to_http_is_refused() {
        let (plain, plain_log) = serve(|_| reply(200, None)).await;
        let target = plain.join("next").unwrap().to_string();
        let (listener, url) = tls_listen();
        let secure_log = tls_serve_on(listener, move |req| match req.path.as_str() {
            "/hop" => reply(302, Some("/down")),
            _ => reply(302, Some(&target)),
        });

        for (start, hops) in [
            ("start", vec![got("/start")]),
            ("hop", vec![got("/hop"), got("/down")]),
        ] {
            secure_log.lock().unwrap().clear();

            let err = client(TIMEOUT)
                .get_bytes(&url.join(start).unwrap())
                .await
                .expect_err("the downgrade was followed");

            assert_eq!(
                err.to_string(),
                "refusing to follow a redirect from https to http",
                "from /{start}"
            );
            assert_eq!(summary(&secure_log), hops, "from /{start}");
            assert_eq!(summary(&plain_log), [], "from /{start}");
        }
    }

    /// Only the downgrade is refused: a redirect that stays on http is
    /// followed as before, which is every hop of a plaintext backend list.
    #[tokio::test]
    async fn a_redirect_from_http_to_http_is_followed() {
        let (url, log) = serve(|req| match req.path.as_str() {
            "/start" => reply(302, Some("/next")),
            _ => reply(200, None),
        })
        .await;

        let (status, _) = client(TIMEOUT)
            .get_bytes(&url.join("start").unwrap())
            .await
            .unwrap();

        assert_eq!(status, StatusCode::OK);
        assert_eq!(summary(&log), [got("/start"), got("/next")]);
    }

    /// And a redirect that stays on https is followed too.
    #[cfg(feature = "rustls-tls")]
    #[tokio::test]
    async fn a_redirect_from_https_to_https_is_followed() {
        let (listener, url) = tls_listen();
        let absolute = url.join("absolute").unwrap().to_string();
        let log = tls_serve_on(listener, move |req| match req.path.as_str() {
            "/start" => reply(302, Some("/relative")),
            "/relative" => reply(302, Some(&absolute)),
            _ => reply(200, None),
        });

        let (status, _) = client(TIMEOUT)
            .get_bytes(&url.join("start").unwrap())
            .await
            .unwrap();

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            summary(&log),
            [got("/start"), got("/relative"), got("/absolute")]
        );
    }

    /// A self-signed certificate for 127.0.0.1, only for the test server; the
    /// test client skips verification.
    #[cfg(feature = "rustls-tls")]
    const TEST_CERT: &str = "-----BEGIN CERTIFICATE-----
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

    #[cfg(feature = "rustls-tls")]
    const TEST_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgSiYX/YODAAF5OWIE
qQuLN6hmz3baz471Yn8bjYeNL0yhRANCAASKZCFHvCS6BEkimY9mqjPZhDcULO3E
ZD/4gnUj9TooNmtCjXdP4GAKxDoCb0FzQoHhDoi8BXY4DFLQYpbnX4Nu
-----END PRIVATE KEY-----
";

    /// A listener for `tls_serve_on` and the URL it is reached at.
    #[cfg(feature = "rustls-tls")]
    fn tls_listen() -> (std::net::TcpListener, Url) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = Url::parse(&format!("https://{}/", listener.local_addr().unwrap())).unwrap();
        (listener, url)
    }

    /// Serves over TLS on a local port as `tls_serve_on` does. Returns the
    /// server's URL and its log.
    #[cfg(feature = "rustls-tls")]
    fn tls_serve(answer: impl Fn(&Seen) -> String + Send + 'static) -> (Url, Log) {
        let (listener, url) = tls_listen();
        (url, tls_serve_on(listener, answer))
    }

    /// Serves one request per connection over TLS, as `serve_on` does over
    /// TCP. Returns the log.
    #[cfg(feature = "rustls-tls")]
    fn tls_serve_on(
        listener: std::net::TcpListener,
        answer: impl Fn(&Seen) -> String + Send + 'static,
    ) -> Log {
        use std::io::{Read, Write};

        use rustls_pki_types::pem::PemObject as _;
        use rustls_pki_types::{CertificateDer, PrivateKeyDer};

        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from_pem_slice(TEST_CERT.as_bytes()).unwrap()],
                PrivateKeyDer::from_pem_slice(TEST_KEY.as_bytes()).unwrap(),
            )
            .unwrap();
        let config = Arc::new(config);

        let log = Log::default();
        let seen = log.clone();
        std::thread::spawn(move || {
            for tcp in listener.incoming() {
                let Ok(tcp) = tcp else { continue };
                let _ = tcp.set_read_timeout(Some(TIMEOUT));
                let conn = rustls::ServerConnection::new(config.clone()).unwrap();
                let mut tls = rustls::StreamOwned::new(conn, tcp);

                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    match tls.read(&mut byte) {
                        Ok(1) => head.push(byte[0]),
                        _ => break,
                    }
                }
                // Not a request: the handshake failed or the client hung up.
                if !head.ends_with(b"\r\n\r\n") {
                    continue;
                }
                let (mut req, length) = parse_head(&head);
                req.body = vec![0; length];
                if tls.read_exact(&mut req.body).is_err() {
                    continue;
                }

                let response = answer(&req);
                seen.lock().unwrap().push(req);
                let _ = tls.write_all(response.as_bytes());
                tls.conn.send_close_notify();
                let _ = tls.flush();
                // Let the client hang up first, as in `serve`.
                let mut rest = [0u8; 1024];
                while matches!(tls.sock.read(&mut rest), Ok(n) if n > 0) {}
            }
        });
        log
    }

    /// The scheme is part of the origin: a body does not follow a redirect
    /// from http to https, nor to another https port, and the server it was
    /// kept from receives nothing at all. The other way the request never
    /// gets as far as the origins, because leaving https is refused first.
    #[cfg(feature = "rustls-tls")]
    #[tokio::test]
    async fn a_307_or_308_does_not_send_a_body_across_schemes_or_https_ports() {
        for status in [307, 308] {
            let (secure, secure_log) = tls_serve(|_| reply(200, None));
            let target = secure.join("next").unwrap().to_string();
            let (plain, plain_log) = serve(move |_| reply(status, Some(&target))).await;
            let result = post(&plain.join("start").unwrap()).await;
            assert_refused(result, &format!("{status} from http to https"));
            assert_eq!(summary(&plain_log), [posted("/start")]);
            assert_eq!(summary(&secure_log), [], "{status} from http to https");

            let (plain, plain_log) = serve(|_| reply(200, None)).await;
            let target = plain.join("next").unwrap().to_string();
            let (secure, secure_log) = tls_serve(move |_| reply(status, Some(&target)));
            let err = post(&secure.join("start").unwrap())
                .await
                .expect_err("the body was sent on");
            assert_eq!(
                err.to_string(),
                "refusing to follow a redirect from https to http",
                "{status} from https to http"
            );
            assert_eq!(summary(&secure_log), [posted("/start")]);
            assert_eq!(summary(&plain_log), [], "{status} from https to http");

            let (other, other_log) = tls_serve(|_| reply(200, None));
            let target = other.join("next").unwrap().to_string();
            let (secure, secure_log) = tls_serve(move |_| reply(status, Some(&target)));
            let result = post(&secure.join("start").unwrap()).await;
            assert_refused(result, &format!("{status} to another https port"));
            assert_eq!(summary(&secure_log), [posted("/start")]);
            assert_eq!(summary(&other_log), [], "{status} to another https port");
        }
    }

    /// Over https the same origin receives the body again, as over http.
    #[cfg(feature = "rustls-tls")]
    #[tokio::test]
    async fn a_307_or_308_sends_a_body_again_to_the_same_https_origin() {
        for status in [307, 308] {
            let (listener, url) = tls_listen();
            let absolute = url.join("absolute").unwrap().to_string();
            let log = tls_serve_on(listener, move |req| match req.path.as_str() {
                "/start" => reply(status, Some("/relative")),
                "/relative" => reply(status, Some(&absolute)),
                _ => reply(200, None),
            });

            let (got, _) = post(&url.join("start").unwrap()).await.unwrap();

            assert_eq!(got, StatusCode::OK, "{status}");
            assert_eq!(
                summary(&log),
                [posted("/start"), posted("/relative"), posted("/absolute")],
                "{status}"
            );
            assert!(requests(&log).iter().all(|seen| seen.body == BODY));
        }
    }

    /// A 301, 302 or 303 crosses from http to https, as it does in Go, and
    /// what arrives is a GET with nothing of the POST in it. Downwards it is
    /// refused even with the body gone: the request was still made over TLS.
    #[cfg(feature = "rustls-tls")]
    #[tokio::test]
    async fn a_301_302_or_303_is_followed_up_to_https_but_not_down_to_http() {
        for status in [301, 302, 303] {
            let (secure, secure_log) = tls_serve(|_| reply(200, None));
            let target = secure.join("next").unwrap().to_string();
            let (plain, _) = serve(move |_| reply(status, Some(&target))).await;
            let (end, _) = post(&plain.join("start").unwrap()).await.unwrap();
            assert_eq!(end, StatusCode::OK, "{status} from http to https");
            assert_eq!(summary(&secure_log), [got("/next")], "{status}");
            assert_eq!(requests(&secure_log)[0].content_type, None, "{status}");

            let (plain, plain_log) = serve(|_| reply(200, None)).await;
            let target = plain.join("next").unwrap().to_string();
            let (secure, _) = tls_serve(move |_| reply(status, Some(&target)));
            let err = post(&secure.join("start").unwrap())
                .await
                .expect_err("the downgrade was followed");
            assert_eq!(
                err.to_string(),
                "refusing to follow a redirect from https to http",
                "{status} from https to http"
            );
            assert_eq!(summary(&plain_log), [], "{status} from https to http");
        }
    }

    /// With no timeout, only the limit can end the read of an endless body,
    /// and dropping the rest must hang up rather than leave it unread.
    #[tokio::test]
    async fn a_prefix_of_an_endless_body_returns_once_it_has_its_bytes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let (hung_up, closed) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut stream).await;
            let head = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
            let chunk = format!("1000\r\n{}\r\n", "x".repeat(0x1000));
            let _ = stream.write_all(head.as_bytes()).await;
            while stream.write_all(chunk.as_bytes()).await.is_ok() {}
            let _ = hung_up.send(());
        });

        let client = client(Duration::ZERO);
        let (status, prefix, _) = tokio::time::timeout(TIMEOUT, client.get_prefix(&url, 10_000))
            .await
            .expect("reading a prefix of an endless body did not return")
            .unwrap();

        assert_eq!(status, StatusCode::OK);
        assert_eq!(prefix.len(), 10_000);
        tokio::time::timeout(TIMEOUT, closed)
            .await
            .expect("the connection was not closed")
            .unwrap();
    }
}
