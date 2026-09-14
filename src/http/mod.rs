//! HTTP client built directly on hyper so that socket binding options
//! (`--source`, `--interface`, `--fwmark`) can be honoured.

pub mod connector;
pub mod tls;

use std::io;
use std::time::Duration;

use anyhow::{bail, Context as _};
use bytes::{Bytes, BytesMut};
use http::header::{HeaderName, HeaderValue, ACCEPT_ENCODING, CONTENT_TYPE, USER_AGENT};
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

/// Go's `http.Client` follows at most 10 redirects by default.
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

pub type ReqBody = BoxBody<Bytes, io::Error>;

/// Whether two URLs share a scheme, host and effective port.
fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
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

/// Reads at most `limit` bytes of a body and discards the rest.
///
/// For deciding what a body *is* rather than reading it: emptiness, or the
/// opening of an error page worth showing.
async fn head_of_body(body: Incoming, limit: usize) -> anyhow::Result<Bytes> {
    let mut body = body;
    let mut out = BytesMut::new();

    while let Some(frame) = body.frame().await {
        if let Some(data) = frame?.data_ref() {
            if out.len() < limit {
                let take = (limit - out.len()).min(data.len());
                out.extend_from_slice(&data[..take]);
            }
        }
    }

    Ok(out.freeze())
}

/// The program's HTTP client.
#[derive(Clone)]
pub struct HttpClient {
    inner: Client<tls::Connector, ReqBody>,
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

        // Keep enough connections alive for every concurrent stream, matching the
        // Go version's MaxIdleConnsPerHost/MaxConnsPerHost tuning.
        let mut builder = Client::builder(TokioExecutor::new());
        builder.pool_max_idle_per_host(concurrent + 2);

        if tls_settings.http2 {
            builder
                .http2_initial_stream_window_size(H2_STREAM_WINDOW)
                .http2_initial_connection_window_size(H2_CONNECTION_WINDOW);
        }

        let inner = builder.build(https);

        Ok(Self {
            inner,
            timeout,
            user_agent: HeaderValue::from_str(user_agent)?,
        })
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

    /// Issues a request, following redirects the way Go's `http.Client` does.
    ///
    /// `mk_body` is called once per attempt so that a redirected request can be
    /// replayed with a fresh body.
    pub async fn request<F>(
        &self,
        method: Method,
        url: &Url,
        headers: &[(HeaderName, HeaderValue)],
        mk_body: F,
    ) -> anyhow::Result<Response<Incoming>>
    where
        F: Fn() -> ReqBody,
    {
        let mut url = url.clone();
        let mut method = method;

        for _ in 0..=MAX_REDIRECTS {
            let uri: Uri = url
                .as_str()
                .parse()
                .with_context(|| format!("invalid URL: {url}"))?;

            let mut builder = Request::builder().method(method.clone()).uri(uri);
            builder = builder.header(USER_AGENT, self.user_agent.clone());
            for (name, value) in headers {
                // A redirect that turned a POST into a GET leaves no body, so
                // the headers describing one would be describing nothing. Go
                // strips them for the same reason, and a strict server or a
                // WAF can refuse a GET that claims a content type.
                if method == Method::GET && name == CONTENT_TYPE {
                    continue;
                }
                builder = builder.header(name.clone(), value.clone());
            }

            let body = if method == Method::GET || method == Method::HEAD {
                empty_body()
            } else {
                mk_body()
            };

            let resp = self.inner.request(builder.body(body)?).await?;

            let status = resp.status();
            if !status.is_redirection() {
                return Ok(resp);
            }

            let Some(location) = resp.headers().get(http::header::LOCATION) else {
                return Ok(resp);
            };
            let location = location.to_str().context("invalid Location header")?;
            let next = url.join(location).context("invalid redirect target")?;

            // Never let a redirect drop TLS. Downgrading would expose a request
            // that was deliberately made over https.
            if url.scheme() == "https" && next.scheme() != "https" {
                bail!(
                    "refusing to follow a redirect from https to {}",
                    next.scheme()
                );
            }

            // 301/302/303 turn the request into a GET; 307/308 replay it as-is.
            let becomes_get = matches!(
                status,
                StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND | StatusCode::SEE_OTHER
            );

            // A replayed body must not be handed to a different origin: the
            // telemetry POST carries the measurement, the client's IP and the
            // ISP details, and 307/308 would resend all of it verbatim.
            //
            // Only requests that actually carry a body are affected. A 307/308
            // on a GET has nothing to replay, and refusing it broke the common
            // case of a scheme-less server list redirecting http to https.
            let carries_body = !matches!(method, Method::GET | Method::HEAD);
            if !becomes_get && carries_body && !same_origin(&url, &next) {
                bail!(
                    "refusing to replay a {method} body across origins ({} -> {})",
                    url.host_str().unwrap_or_default(),
                    next.host_str().unwrap_or_default()
                );
            }

            if becomes_get {
                method = Method::GET;
            }
            url = next;
        }

        bail!("stopped after {MAX_REDIRECTS} redirects")
    }

    /// Performs a GET request and reads the whole response body.
    pub async fn get_bytes(&self, url: &Url) -> anyhow::Result<(StatusCode, Bytes)> {
        let fut = async {
            let resp = self.request(Method::GET, url, &[], empty_body).await?;
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
            let resp = self.request(Method::GET, url, &[], empty_body).await?;
            let status = resp.status();
            let facts = ConnectionFacts::of(&resp);
            drain_body(resp.into_body()).await?;
            Ok::<_, anyhow::Error>((status, facts))
        };

        self.with_timeout(fut).await
    }

    /// Fetches a URL, keeping only the opening of the body.
    ///
    /// Enough to tell an empty body from a full one and to quote what came
    /// back, without letting its size decide how much memory that costs.
    pub async fn get_head(
        &self,
        url: &Url,
        limit: usize,
    ) -> anyhow::Result<(StatusCode, Bytes, ConnectionFacts)> {
        let fut = async {
            let resp = self.request(Method::GET, url, &[], empty_body).await?;
            let status = resp.status();
            let facts = ConnectionFacts::of(&resp);
            let body = head_of_body(resp.into_body(), limit).await?;
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
                .request(Method::POST, url, &headers, || full_body(body.clone()))
                .await?;
            let status = resp.status();
            let out = collect_limited(resp.into_body(), MAX_TELEMETRY_RESPONSE).await?;
            Ok::<_, anyhow::Error>((status, out))
        };

        self.with_timeout(fut).await
    }

    /// Sends a request without buffering the response body, for the transfer tests.
    pub async fn send_streaming<F>(
        &self,
        method: Method,
        url: &Url,
        mk_body: F,
    ) -> anyhow::Result<Response<Incoming>>
    where
        F: Fn() -> ReqBody,
    {
        // Speed tests must measure the wire, not a decompressed stream.
        let headers = vec![(ACCEPT_ENCODING, HeaderValue::from_static("identity"))];
        self.request(method, url, &headers, mk_body).await
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
