//! A speed test server and the measurements performed against it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use bytes::Bytes;
use http::{Method, Response, StatusCode};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;
use url::Url;

use crate::defs::bytes_counter::{random_data, BytesCounter};
use crate::defs::telemetry::TelemetryLog;
use crate::defs::GetIPResult;
use crate::http::{HttpClient, IpFamily, RequestBody};
use crate::ping::{compute_jitter, icmp_rtts, resolve_host};
use crate::spinner::Spinner;
use crate::util::{avg, stddev, url_join_path};
use crate::{write_debug, write_ui};

/// The stagger between starting concurrent transfer streams.
const RAMP_UP_DELAY: Duration = Duration::from_millis(200);
/// The chunk size the upload body is fed to the connection in. hyper takes a
/// whole chunk whenever its write buffer has room, so this is also how much of
/// a connection's counted upload can sit past that buffer, unsent.
const UPLOAD_CHUNK: usize = 16 * 1024;

/// A speed test server, as described by the server list JSON.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Server {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub server: String,
    #[serde(rename = "dlURL", default)]
    pub download_url: String,
    #[serde(rename = "ulURL", default)]
    pub upload_url: String,
    #[serde(rename = "pingURL", default)]
    pub ping_url: String,
    #[serde(rename = "getIpURL", default)]
    pub get_ip_url: String,
    #[serde(rename = "sponsorName", default)]
    pub sponsor_name: String,
    #[serde(rename = "sponsorURL", default)]
    pub sponsor_url: String,
}

/// Settings shared by the download and upload tests.
#[derive(Debug, Clone)]
pub struct TransferOptions {
    pub silent: bool,
    pub use_bytes: bool,
    pub use_mebi: bool,
    pub requests: usize,
    pub chunks: usize,
    pub upload_size: usize,
    pub no_prealloc: bool,
    pub duration: Duration,
}

impl Server {
    /// Parses the server's base URL.
    pub fn get_url(&self) -> anyhow::Result<Url> {
        // Sanitized here rather than at the print sites: the context outlives
        // this call and is rendered by both `write_error!` in helper.rs and the
        // `{e:#}` chain in main.rs, neither of which is gated on --debug.
        Url::parse(&self.server).with_context(|| {
            format!(
                "invalid server URL: {}",
                crate::output::sanitize(&self.server)
            )
        })
    }

    /// Renders the sponsor line shown in `--list` and before a test.
    pub fn sponsor(&self) -> String {
        if self.sponsor_name.is_empty() {
            return String::new();
        }
        let mut msg = self.sponsor_name.clone();
        if !self.sponsor_url.is_empty() {
            // A scheme-less sponsor URL defaults to https, as in the Go version.
            let url = if self.sponsor_url.contains("://") {
                self.sponsor_url.clone()
            } else {
                format!("https://{}", self.sponsor_url)
            };
            // Render the URL as given rather than as re-serialised by the `url`
            // crate, which would append a slash to a bare authority.
            match Url::parse(&url) {
                Ok(_) => msg.push_str(&format!(" @ {url}")),
                Err(_) => write_debug!(
                    "Sponsor URL is invalid: {}\n",
                    crate::output::sanitize(&self.sponsor_url)
                ),
            }
        }
        msg
    }

    /// Checks the backend is up: the ping URL must return 200 and an empty body.
    ///
    /// Also reports what this probe's connection negotiated. The transfer
    /// phases open further connections and a reconnect can land on a different
    /// address, so this describes the probe, not necessarily every connection
    /// the run went on to use; the Go client reports the same thing from the
    /// same place.
    pub async fn is_up(&self, client: &HttpClient, tlog: &TelemetryLog) -> ServerStatus {
        let t = Instant::now();

        let url = match self.get_url() {
            Ok(u) => url_join_path(&u, &self.ping_url),
            Err(e) => {
                write_debug!("Failed when creating HTTP request: {e}\n");
                return ServerStatus::default();
            }
        };

        // Enough of the body to tell empty from not, and to quote what came
        // back; a backend answering the probe with megabytes used to make the
        // client hold all of them, ten servers at a time.
        let result = client.get_prefix(&url, PROBE_BODY_PEEK).await;
        tlog.logf(format!(
            "Check backend is up took {}",
            go_duration(t.elapsed())
        ));

        match result {
            Ok((status, body, facts)) => {
                // Say what the connection settled on. On hardware without AES
                // acceleration the cipher, not the link, is what bounds the
                // result, and under TLS 1.3 the server picks it from the set
                // offered, so two runs can differ several-fold for a reason
                // the numbers alone do not show.
                match (&facts.tls, url.scheme()) {
                    (Some(t), _) => {
                        write_debug!("Negotiated {} with {}\n", t.version, t.cipher)
                    }
                    (None, "https") => write_debug!(
                        "Connection is encrypted, but this TLS backend does not report with what\n"
                    ),
                    (None, _) => write_debug!("Connection is not encrypted\n"),
                }

                if !body.is_empty() {
                    write_debug!(
                        "Failed when parsing get IP result: {}\n",
                        crate::output::sanitize(&String::from_utf8_lossy(&body))
                    );
                    return ServerStatus::default();
                }
                ServerStatus {
                    up: status == StatusCode::OK,
                    tls: facts.tls,
                }
            }
            Err(e) => {
                write_debug!("Error checking for server status: {e:#}\n");
                ServerStatus::default()
            }
        }
    }

    /// Fetches the client's IP information from the backend's getIP endpoint.
    pub async fn get_ip_info(
        &self,
        client: &HttpClient,
        tlog: &TelemetryLog,
        distance_unit: &str,
    ) -> anyhow::Result<GetIPResult> {
        let t = Instant::now();

        let mut url = url_join_path(&self.get_url()?, &self.get_ip_url);
        url.query_pairs_mut()
            .append_pair("distance", distance_unit)
            .append_pair("isp", "true");

        let (_, body) = client.get_bytes(&url).await?;
        tlog.logf(format!("Get IP info took {}", go_duration(t.elapsed())));

        let mut info = GetIPResult::default();
        if body.is_empty() {
            return Ok(info);
        }

        match serde_json::from_slice::<GetIPResult>(&body) {
            Ok(v) => info = v,
            Err(e) => {
                write_debug!("Failed when parsing get IP result: {e}\n");
                write_debug!(
                    "Received payload: {}\n",
                    crate::output::sanitize(&String::from_utf8_lossy(&body))
                );

                // Reached when the body is not JSON at all, or is JSON that
                // does not fit the schema -- a non-string processedString,
                // say. Either way the raw body shown as the processed string
                // beats losing everything.
                info.processed_string = String::from_utf8_lossy(&body).into_owned();
            }
        }

        Ok(info)
    }

    /// Measures latency and jitter by repeatedly fetching the ping URL.
    pub async fn ping_and_jitter(
        &self,
        client: &HttpClient,
        tlog: &TelemetryLog,
        count: usize,
    ) -> anyhow::Result<(f64, f64)> {
        let t = Instant::now();

        let url = url_join_path(&self.get_url()?, &self.ping_url);
        let mut pings = Vec::with_capacity(count);

        // Collect every distinct peer, not just the first. The requests
        // usually share one connection, but a reconnect can land on a
        // different address -- a different family, even -- and reporting only
        // the first would describe a connection the later samples did not use.
        let mut remotes: Vec<std::net::SocketAddr> = Vec::new();
        for _ in 0..count {
            let start = Instant::now();
            // The reply is timing, not data: read it away rather than into
            // memory, as the Go client does.
            let (_, facts) = client.get_drained(&url).await?;
            pings.push(start.elapsed().as_secs_f64() * 1000.0);
            if let Some(peer) = facts.peer {
                if !remotes.contains(&peer) {
                    remotes.push(peer);
                }
            }
        }

        for addr in &remotes {
            write_debug!(
                "Pinging {addr} over TCP ({})\n",
                if addr.is_ipv4() { "IPv4" } else { "IPv6" }
            );
        }

        // Discard the first sample, which carries the handshake overhead.
        if pings.len() > 1 {
            pings.remove(0);
        }

        tlog.logf(format!("TCP ping took {}", go_duration(t.elapsed())));
        Ok((avg(&pings), compute_jitter(&pings)))
    }

    /// Measures latency and jitter with ICMP echos, falling back to HTTP pings
    /// whenever ICMP is unavailable.
    #[allow(clippy::too_many_arguments)]
    pub async fn icmp_ping_and_jitter(
        &self,
        client: &HttpClient,
        tlog: &TelemetryLog,
        count: usize,
        source: Option<std::net::IpAddr>,
        interface: Option<&str>,
        family: IpFamily,
        no_icmp: bool,
    ) -> anyhow::Result<(f64, f64)> {
        if no_icmp {
            write_debug!(
                "Skipping ICMP for server {}, will use HTTP ping\n",
                crate::output::sanitize(&self.name)
            );
            return self.ping_and_jitter(client, tlog, count + 2).await;
        }

        let t = Instant::now();
        let url = self.get_url()?;
        let host = url
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("server URL has no host"))?;

        let target = match resolve_host(host, family).await {
            Ok(t) => t,
            Err(e) => {
                write_debug!("Failed to resolve ping target: {e}\n");
                write_debug!("Will try TCP ping\n");
                return self.ping_and_jitter(client, tlog, count + 2).await;
            }
        };

        let rtts = match icmp_rtts(target, count, source, interface).await {
            Ok(r) => r,
            Err(e) => {
                write_debug!("Failed to ping target host: {e}\n");
                write_debug!("Will try TCP ping\n");
                return self.ping_and_jitter(client, tlog, count + 2).await;
            }
        };

        if rtts.is_empty() {
            write_debug!(
                "No ICMP pings returned for server {} ({}), trying TCP ping\n",
                crate::output::sanitize(&self.name),
                crate::output::sanitize(host)
            );
            return self.ping_and_jitter(client, tlog, count + 2).await;
        }

        // Say which address the test actually reached. IPv4 and IPv6 can take
        // different paths through the network, so a result is not fully
        // described by the hostname it was measured against, and --json carries
        // no client address to infer it from.
        write_debug!(
            "Pinging {} over ICMP ({})\n",
            target,
            if target.is_ipv4() { "IPv4" } else { "IPv6" }
        );

        // A single figure hides how the samples were spread, and the spread is
        // what says whether a link is steady or merely fast on average. Raw
        // counts rather than a loss percentage: a handful of probes is too few
        // for a rate, and ICMP is often policed independently of the data path,
        // so a percentage would say more about the server's ICMP handling than
        // about the network.
        write_debug!(
            "Ping over ICMP: min {:.2} ms, avg {:.2} ms, max {:.2} ms, stddev {:.2} ms, {}/{} replies\n",
            rtts.iter().copied().fold(f64::INFINITY, f64::min),
            avg(&rtts),
            rtts.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            stddev(&rtts),
            rtts.len(),
            count
        );

        tlog.logf(format!("ICMP ping took {}", go_duration(t.elapsed())));
        Ok((avg(&rtts), compute_jitter(&rtts)))
    }

    /// Runs the download test, returning the average rate in Mbps and the total
    /// number of bytes received.
    pub async fn download(
        &self,
        client: &HttpClient,
        tlog: &TelemetryLog,
        opts: &TransferOptions,
    ) -> anyhow::Result<(f64, u64)> {
        let t = Instant::now();

        let mut counter = BytesCounter::new();
        counter.set_mebi(opts.use_mebi);
        let counter = Arc::new(counter);

        let mut url = url_join_path(&self.get_url()?, &self.download_url);
        url.query_pairs_mut()
            .append_pair("ckSize", &opts.chunks.to_string());

        counter.start();
        let spinner = self.start_transfer_spinner("Downloading...  ", opts, &counter);

        let mut tasks: JoinSet<StreamEnd> = JoinSet::new();
        for _ in 0..opts.requests {
            tasks.spawn(download_once(client.clone(), url.clone(), counter.clone()));
            tokio::time::sleep(RAMP_UP_DELAY).await;
        }

        // The window is timed from here, after ramp-up: the deadline, and the
        // ticker's seconds and percent, so progress reaches 100 as the window
        // closes. The rate is not. The counter's clock started before ramp-up,
        // as the Go client's does, so every average it reports, the final one
        // included, divides by the ramp-up as well.
        let test_start = Instant::now();
        let ticker = Self::start_progress_ticker("download", &counter, opts.duration, test_start);
        // Only the deadline ends the phase. Draining the set used to end it
        // too, so a link that broke every connection finished the test in
        // under a second and divided its bytes by that, reporting a rate many
        // times too high; --duration was also silently cut to --timeout.
        // Dropping the JoinSet afterwards aborts everything still in flight.
        let deadline = tokio::time::Instant::now() + opts.duration;
        loop {
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(deadline) => break,
                Some(res) = tasks.join_next(), if !tasks.is_empty() => {
                    if matches!(res, Ok(end) if end.replaceable()) {
                        tasks.spawn(download_once(client.clone(), url.clone(), counter.clone()));
                    }
                }
            }
        }
        // Abort the in-flight transfers and wait for them to unwind before
        // reading the counter, so the reported total cannot change under us.
        tasks.shutdown().await;
        if let Some(ticker) = ticker {
            ticker.abort();
        }

        let (mbps, total) = (counter.avg_mbps(), counter.total());
        if let Some(spinner) = spinner {
            spinner
                .stop(&format_rate("Download rate", opts.use_bytes, &counter))
                .await;
        }

        tlog.logf(format!("Download took {}", go_duration(t.elapsed())));
        Ok((mbps, total))
    }

    /// Runs the upload test, returning the average rate in Mbps and the total
    /// number of bytes sent.
    pub async fn upload(
        &self,
        client: &HttpClient,
        tlog: &TelemetryLog,
        opts: &TransferOptions,
    ) -> anyhow::Result<(f64, u64)> {
        let t = Instant::now();

        let mut counter = BytesCounter::new();
        counter.set_mebi(opts.use_mebi);
        counter.set_upload_size(opts.upload_size);
        let counter = Arc::new(counter);

        // Pre-allocating one random blob and reusing it keeps the CPU out of the
        // measurement; --no-pre-allocate streams endless random data instead.
        let payload = if opts.no_prealloc {
            write_ui!("Pre-allocation is disabled, performance might be lower!\n");
            None
        } else {
            Some(Bytes::from(random_data(counter.upload_size())))
        };

        let url = url_join_path(&self.get_url()?, &self.upload_url);
        // Upload over the pool whose buffers are capped, so little is counted
        // but still unsent when the window closes.
        let client = client.for_uploads();

        counter.start();
        let spinner = self.start_transfer_spinner("Uploading...  ", opts, &counter);

        let mut tasks: JoinSet<StreamEnd> = JoinSet::new();
        for _ in 0..opts.requests {
            tasks.spawn(upload_once(
                client.clone(),
                url.clone(),
                payload.clone(),
                counter.clone(),
            ));
            tokio::time::sleep(RAMP_UP_DELAY).await;
        }

        // The window is timed from here, after ramp-up: the deadline, and the
        // ticker's seconds and percent, so progress reaches 100 as the window
        // closes. The rate is not. The counter's clock started before ramp-up,
        // as the Go client's does, so every average it reports, the final one
        // included, divides by the ramp-up as well.
        let test_start = Instant::now();
        let ticker = Self::start_progress_ticker("upload", &counter, opts.duration, test_start);
        // Only the deadline ends the phase; see the download loop.
        let deadline = tokio::time::Instant::now() + opts.duration;
        loop {
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(deadline) => break,
                Some(res) = tasks.join_next(), if !tasks.is_empty() => {
                    if matches!(res, Ok(end) if end.replaceable()) {
                        tasks.spawn(upload_once(
                            client.clone(),
                            url.clone(),
                            payload.clone(),
                            counter.clone(),
                        ));
                    }
                }
            }
        }
        // Abort the in-flight transfers and wait for them to unwind before
        // reading the counter, so the reported total cannot change under us.
        tasks.shutdown().await;
        if let Some(ticker) = ticker {
            ticker.abort();
        }

        let (mbps, total) = (counter.avg_mbps(), counter.total());
        if let Some(spinner) = spinner {
            spinner
                .stop(&format_rate("Upload rate", opts.use_bytes, &counter))
                .await;
        }

        tlog.logf(format!("Upload took {}", go_duration(t.elapsed())));
        Ok((mbps, total))
    }

    /// Emits one NDJSON progress event a second while a transfer runs.
    ///
    /// The machine-readable sibling of the spinner: the spinner narrates to a
    /// person on stderr, this reports to a script on stdout, and both read the
    /// same counter. Aborted -- not joined -- when the transfer ends, since a
    /// sleeping tick holds nothing worth waiting for.
    fn start_progress_ticker(
        phase: &'static str,
        counter: &Arc<BytesCounter>,
        duration: Duration,
        started: Instant,
    ) -> Option<tokio::task::JoinHandle<()>> {
        if !crate::output::is_stream() {
            return None;
        }
        let counter = counter.clone();
        Some(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let elapsed = started.elapsed().as_secs_f64();
                // A speed test is bounded by time, not by volume, so percent
                // done is elapsed over the configured duration -- exact, and
                // the only notion of "how much is left" the test has.
                let percent = (elapsed / duration.as_secs_f64() * 100.0).min(100.0);
                crate::output::stream_event(&format!(
                    r#"{{"event":"progress","phase":"{phase}","seconds":{elapsed:.1},"mbps":{:.2},"progress":{percent:.0}}}"#,
                    counter.avg_mbps()
                ));
            }
        }))
    }

    fn start_transfer_spinner(
        &self,
        prefix: &str,
        opts: &TransferOptions,
        counter: &Arc<BytesCounter>,
    ) -> Option<Spinner> {
        if opts.silent {
            return None;
        }
        let counter = counter.clone();
        let use_bytes = opts.use_bytes;
        Some(Spinner::start(prefix, move || {
            if use_bytes {
                format!("  {}", counter.avg_humanize())
            } else {
                format!("  {:.2} Mbps", counter.avg_mbps())
            }
        }))
    }
}

fn format_rate(label: &str, use_bytes: bool, counter: &BytesCounter) -> String {
    if use_bytes {
        format!("{label}:\t{}\n", counter.avg_humanize())
    } else {
        format!("{label}:\t{:.2} Mbps\n", counter.avg_mbps())
    }
}

/// How much of a probe response is worth keeping to quote in a diagnostic.
const PROBE_BODY_PEEK: usize = 8 * 1024;

/// Whether a backend answered its probe, and what that connection negotiated.
#[derive(Debug, Default)]
pub struct ServerStatus {
    pub up: bool,
    pub tls: Option<crate::http::TlsFacts>,
}

/// Why a transfer stream ended, which decides whether it is replaced.
///
/// A stream that carried data and then broke is replaced, so the phase keeps
/// the link busy for the whole window; one whose request never got off the
/// ground is not, because retrying it as fast as it fails would spin. This
/// mirrors the Go client, which respawns after a body error but declines to
/// after a failed request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum StreamEnd {
    Completed,
    TransferFailed,
    RequestFailed,
}

impl StreamEnd {
    fn replaceable(self) -> bool {
        !matches!(self, StreamEnd::RequestFailed)
    }
}

/// Downloads once, counting every byte received.
async fn download_once(client: HttpClient, url: Url, counter: Arc<BytesCounter>) -> StreamEnd {
    let deadline = stream_deadline(&client);
    let resp = match send_request(&client, deadline, Method::GET, &url, RequestBody::Empty).await {
        Ok(resp) => resp,
        Err(end) => return end,
    };

    let read = async {
        let mut body = resp.into_body();
        while let Some(frame) = body.frame().await {
            match frame {
                Ok(f) => {
                    if let Some(data) = f.data_ref() {
                        counter.add(data.len() as u64);
                    }
                }
                Err(e) => {
                    write_debug!("Failed when reading HTTP response: {e}\n");
                    return StreamEnd::TransferFailed;
                }
            }
        }
        StreamEnd::Completed
    };

    // A timeout here cut short a body that was arriving, and the stream is
    // replaced, as the Go client respawns after its client timeout kills a
    // body read.
    within(deadline, read)
        .await
        .unwrap_or(StreamEnd::TransferFailed)
}

/// Uploads once. The body counts itself as hyper takes it; see `UploadBody`.
async fn upload_once(
    client: HttpClient,
    url: Url,
    payload: Option<Bytes>,
    counter: Arc<BytesCounter>,
) -> StreamEnd {
    let deadline = stream_deadline(&client);
    let body = RequestBody::Stream(BodyExt::boxed(upload_body::UploadBody::new(
        payload, counter,
    )));
    let resp = match send_request(&client, deadline, Method::POST, &url, body).await {
        Ok(resp) => resp,
        Err(end) => return end,
    };

    // Discard the response frame by frame so the connection can be reused.
    // Collecting it instead retained whatever the server chose to send, per
    // stream, for the whole test -- the one body read here with no cap.
    let read = async {
        let mut body = resp.into_body();
        while let Some(frame) = body.frame().await {
            if let Err(e) = frame {
                write_debug!("Failed when reading HTTP response: {e}\n");
                return StreamEnd::TransferFailed;
            }
        }
        StreamEnd::Completed
    };

    within(deadline, read)
        .await
        .unwrap_or(StreamEnd::TransferFailed)
}

/// When `--timeout` runs out for a transfer stream starting now, or `None` when
/// it is zero, which means no timeout, the same as for every other request.
/// One deadline covers the request and the response body, as the Go client's
/// `http.Client.Timeout` does.
fn stream_deadline(client: &HttpClient) -> Option<tokio::time::Instant> {
    let timeout = client.timeout();
    (!timeout.is_zero()).then(|| tokio::time::Instant::now() + timeout)
}

/// Runs `fut` until `deadline`. Returns `None` if the deadline cut it short.
async fn within<T>(
    deadline: Option<tokio::time::Instant>,
    fut: impl std::future::Future<Output = T>,
) -> Option<T> {
    match deadline {
        Some(deadline) => tokio::time::timeout_at(deadline, fut).await.ok(),
        None => Some(fut.await),
    }
}

/// Sends a transfer stream's request and waits for the response head.
///
/// A request that fails, or that the deadline cuts short before the response
/// head arrives, ends the stream as `RequestFailed`, and it is not replaced.
/// For an upload that span normally covers sending the whole body. The Go
/// client draws the line in the same place: its client timeout firing inside
/// `Do` is a request error it does not respawn after.
async fn send_request(
    client: &HttpClient,
    deadline: Option<tokio::time::Instant>,
    method: Method,
    url: &Url,
    body: RequestBody,
) -> Result<Response<Incoming>, StreamEnd> {
    match within(deadline, client.send_streaming(method, url, body)).await {
        Some(Ok(resp)) => Ok(resp),
        Some(Err(e)) => {
            write_debug!("Failed when making HTTP request: {e}\n");
            Err(StreamEnd::RequestFailed)
        }
        None => Err(StreamEnd::RequestFailed),
    }
}

/// The upload request body, in a module of its own so that its fields are
/// private: `UploadBody::new` is the only way to build one, so every upload
/// gets its filler decided there.
mod upload_body {
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::{Context, Poll};

    use bytes::Bytes;
    use http_body::{Body, Frame, SizeHint};

    use super::UPLOAD_CHUNK;
    use crate::defs::bytes_counter::{random_data, BytesCounter};

    /// The request body for the upload test.
    ///
    /// With a payload it sends exactly that blob once; without one it produces
    /// random data indefinitely, until the test duration cancels the request.
    ///
    /// Each frame is added to the upload total as hyper takes it, so the total
    /// is request body bytes, which is what the Go client's TeeReader around
    /// its request body counts: no request heads, chunk framing or TLS
    /// overhead. A frame taken is not yet sent. hyper takes one whenever its
    /// write buffer has room, and that buffer is capped (see
    /// `http::client_builder`), so a connection holds at most the buffer and
    /// one frame counted but unsent when the window closes.
    pub(super) struct UploadBody {
        payload: Option<Bytes>,
        pos: usize,
        /// The block sent over and over when there is no pre-allocated payload,
        /// and empty when there is one.
        filler: Bytes,
        /// The phase's upload total.
        counter: Arc<BytesCounter>,
    }

    impl UploadBody {
        pub(super) fn new(payload: Option<Bytes>, counter: Arc<BytesCounter>) -> Self {
            // Only the endless stream ever sends the filler.
            let filler = match payload {
                Some(_) => Bytes::new(),
                None => Bytes::from(random_data(UPLOAD_CHUNK)),
            };
            Self {
                payload,
                pos: 0,
                filler,
                counter,
            }
        }
    }

    impl Body for UploadBody {
        type Data = Bytes;
        type Error = std::io::Error;

        /// Without an exact size hyper frames the POST as `Transfer-Encoding:
        /// chunked`, and a backend whose parser reads fixed blocks rather than
        /// chunk-decoding then waits for a block that never fills: the request
        /// never completes and one payload is all the test ever sends. The Go
        /// client sets Content-Length for the same reason. The endless
        /// `--no-pre-allocate` stream has no length to declare and stays chunked.
        fn size_hint(&self) -> SizeHint {
            match &self.payload {
                Some(payload) => SizeHint::with_exact((payload.len() - self.pos) as u64),
                None => SizeHint::default(),
            }
        }

        fn poll_frame(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
            let this = self.get_mut();

            let chunk = match &this.payload {
                Some(payload) => {
                    if this.pos >= payload.len() {
                        return Poll::Ready(None);
                    }
                    let end = (this.pos + UPLOAD_CHUNK).min(payload.len());
                    let out = payload.slice(this.pos..end);
                    this.pos = end;
                    out
                }
                // One block per request, resliced, rather than a fresh one per
                // frame. The point of --no-pre-allocate is not to hold the whole
                // payload, which one block does not; generating it again for
                // every frame cost about seventeen times the CPU of the
                // pre-allocated path and saved no memory at all.
                None => this.filler.clone(),
            };

            this.counter.add(chunk.len() as u64);
            Poll::Ready(Some(Ok(Frame::data(chunk))))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The filler is only ever sent without a pre-allocated payload, so a
        /// body that has one must not generate it.
        #[test]
        fn only_a_body_without_a_payload_generates_filler() {
            let counter = Arc::new(BytesCounter::new());
            let with_payload =
                UploadBody::new(Some(Bytes::from_static(b"payload")), counter.clone());
            assert!(
                with_payload.filler.is_empty(),
                "filler was generated for a pre-allocated payload"
            );
            assert_eq!(UploadBody::new(None, counter).filler.len(), UPLOAD_CHUNK);
        }
    }
}

/// Renders a duration the way Go's `time.Duration.String()` does, for telemetry logs.
fn go_duration(d: Duration) -> String {
    fn trim(s: String) -> String {
        if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            s
        }
    }

    let secs = d.as_secs_f64();
    if secs >= 1.0 {
        format!("{}s", trim(format!("{secs:.9}")))
    } else if secs >= 1e-3 {
        format!("{}ms", trim(format!("{:.6}", secs * 1e3)))
    } else if secs >= 1e-6 {
        format!("{}µs", trim(format!("{:.3}", secs * 1e6)))
    } else {
        format!("{}ns", d.as_nanos())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sponsor_renders_name_and_url() {
        let s = Server {
            sponsor_name: "Clouvider".into(),
            sponsor_url: "https://www.clouvider.co.uk/".into(),
            ..Default::default()
        };
        assert_eq!(s.sponsor(), "Clouvider @ https://www.clouvider.co.uk/");
    }

    #[test]
    fn sponsor_defaults_to_https() {
        let s = Server {
            sponsor_name: "Example".into(),
            sponsor_url: "example.com".into(),
            ..Default::default()
        };
        assert_eq!(s.sponsor(), "Example @ https://example.com");
    }

    #[test]
    fn sponsor_is_empty_without_a_name() {
        assert_eq!(Server::default().sponsor(), "");
    }

    #[test]
    fn go_duration_formats_like_go() {
        assert_eq!(go_duration(Duration::from_millis(1500)), "1.5s");
        assert_eq!(go_duration(Duration::from_micros(1500)), "1.5ms");
    }
}

#[cfg(test)]
mod transfer_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::http::{BindOptions, HttpClient, TlsSettings};

    /// Answers every request only after `delay`, so a stream completes only if
    /// nothing cancels it while it waits. Returns the address it listens on.
    async fn slow_server(delay: Duration) -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    let _ = stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndata")
                        .await;
                    // Read the request away, so closing does not reset it.
                    let mut buf = [0u8; 8192];
                    while matches!(stream.read(&mut buf).await, Ok(n) if n > 0) {}
                });
            }
        });
        addr
    }

    /// --timeout 0 means no timeout, so it must not cut a transfer stream
    /// short, however long the server takes to answer.
    #[tokio::test]
    async fn a_zero_timeout_does_not_cut_transfer_streams_short() {
        let addr = slow_server(Duration::from_millis(200)).await;
        let url = Url::parse(&format!("http://{addr}/")).unwrap();
        let client = client_with_timeout(Duration::ZERO);

        let counter = Arc::new(BytesCounter::new());
        let end = download_once(client.clone(), url.clone(), counter.clone()).await;
        assert!(
            matches!(end, StreamEnd::Completed),
            "the download stream was cut short"
        );
        assert_eq!(counter.total(), 4, "the whole response must be counted");

        let payload = Some(Bytes::from_static(&[0; 1024]));
        let end = upload_once(client, url, payload, counter).await;
        assert!(
            matches!(end, StreamEnd::Completed),
            "the upload stream was cut short"
        );
    }

    fn client_with_timeout(timeout: Duration) -> HttpClient {
        HttpClient::new(
            BindOptions::default(),
            &TlsSettings {
                ca_cert: None,
                skip_verify: false,
                http2: false,
            },
            timeout,
            3,
            "test",
        )
        .unwrap()
    }

    fn client() -> HttpClient {
        client_with_timeout(Duration::from_secs(5))
    }

    fn opts(duration: Duration) -> TransferOptions {
        TransferOptions {
            silent: true,
            use_bytes: false,
            use_mebi: false,
            requests: 3,
            chunks: 1,
            upload_size: 1,
            no_prealloc: false,
            duration,
        }
    }

    /// Accepts every connection and drops it at once, so every stream fails
    /// immediately. Returns the address it listens on.
    async fn hostile_server() -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => drop(stream),
                    Err(_) => return,
                }
            }
        });
        addr
    }

    fn server_at(addr: std::net::SocketAddr) -> Server {
        Server {
            server: format!("http://{addr}/"),
            download_url: "garbage.php".into(),
            upload_url: "empty.php".into(),
            ping_url: "empty.php".into(),
            ..Default::default()
        }
    }

    /// A phase must run for its whole window even when every stream dies at
    /// once. It used to end when the pool drained, so a link that broke every
    /// connection finished in milliseconds and divided its bytes by that.
    #[tokio::test]
    async fn a_download_phase_lasts_its_full_duration_even_if_every_stream_fails() {
        let addr = hostile_server().await;
        let server = server_at(addr);
        let window = Duration::from_secs(2);

        let started = Instant::now();
        let (mbps, total) = server
            .download(&client(), &TelemetryLog::new(), &opts(window))
            .await
            .expect("a failing link is still a completed measurement");
        let elapsed = started.elapsed();

        assert!(
            elapsed >= window,
            "phase ended after {elapsed:?}, before its {window:?} window"
        );
        assert_eq!(
            total, 0,
            "nothing was transferred, so nothing may be counted"
        );
        assert_eq!(mbps, 0.0, "no bytes over a full window is zero, not a rate");
    }

    /// The same for upload, which has its own copy of the loop.
    #[tokio::test]
    async fn an_upload_phase_lasts_its_full_duration_even_if_every_stream_fails() {
        let addr = hostile_server().await;
        let server = server_at(addr);
        let window = Duration::from_secs(2);

        let started = Instant::now();
        server
            .upload(&client(), &TelemetryLog::new(), &opts(window))
            .await
            .expect("a failing link is still a completed measurement");

        assert!(
            started.elapsed() >= window,
            "phase ended after {:?}, before its {window:?} window",
            started.elapsed()
        );
    }

    /// Reads everything sent to it and never answers, so an upload stream
    /// finishes sending and then sits idle. Returns the address and a receiver
    /// for the bytes read, sent once the client closes the connection.
    async fn silent_server() -> (
        std::net::SocketAddr,
        tokio::sync::oneshot::Receiver<Vec<u8>>,
    ) {
        use tokio::io::AsyncReadExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (report, arrived) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut arrived = Vec::new();
            let _ = stream.read_to_end(&mut arrived).await;
            let _ = report.send(arrived);
        });
        (addr, arrived)
    }

    /// The upload total is the request body, each byte counted once, as the Go
    /// client's TeeReader counts it. Counting on the socket also took in the
    /// request head.
    #[tokio::test]
    async fn the_upload_total_is_the_request_body_and_nothing_else() {
        let (addr, arrived) = silent_server().await;
        let mut opts = opts(Duration::from_secs(1));
        opts.requests = 1;
        // KiB: several frames and a short last one, so a frame counted twice
        // or not at all shows.
        opts.upload_size = 50;

        let (_, counted) = server_at(addr)
            .upload(&client(), &TelemetryLog::new(), &opts)
            .await
            .expect("an unanswered upload is still a completed measurement");
        let arrived = tokio::time::timeout(Duration::from_secs(10), arrived)
            .await
            .expect("the client closed its connection")
            .unwrap();

        let head = arrived
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("a request head arrived")
            + 4;
        let body = (arrived.len() - head) as u64;
        assert_eq!(body, 50 * 1024, "one whole payload should have arrived");
        assert_eq!(
            counted, body,
            "the upload total is not the request body ({head} head bytes arrived with it)"
        );
    }

    /// Accepts every connection and reads what arrives on it, but never
    /// answers. Returns the address and the number of connections accepted.
    async fn unanswering_server() -> (std::net::SocketAddr, Arc<AtomicUsize>) {
        use tokio::io::AsyncReadExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = Arc::new(AtomicUsize::new(0));
        let count = accepted.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                count.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    while matches!(stream.read(&mut buf).await, Ok(n) if n > 0) {}
                });
            }
        });
        (addr, accepted)
    }

    /// Answers every request with a head promising a body it never sends.
    async fn stalling_server() -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    if !matches!(stream.read(&mut buf).await, Ok(n) if n > 0) {
                        return;
                    }
                    let _ = stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1048576\r\n\r\n")
                        .await;
                    while matches!(stream.read(&mut buf).await, Ok(n) if n > 0) {}
                });
            }
        });
        addr
    }

    /// A stream whose timeout runs out before its response head arrives
    /// failed as a request, as it does in the Go client, where the timeout
    /// fires inside `Do`; for an upload that is while the body is being sent
    /// or its answer awaited.
    #[tokio::test]
    async fn a_timeout_before_the_response_head_fails_the_request() {
        let (addr, _) = unanswering_server().await;
        let url = Url::parse(&format!("http://{addr}/")).unwrap();
        let client = client_with_timeout(Duration::from_millis(200));
        let counter = Arc::new(BytesCounter::new());

        let end = download_once(client.clone(), url.clone(), counter.clone()).await;
        assert_eq!(end, StreamEnd::RequestFailed, "download");
        let payload = Some(Bytes::from_static(&[0; 1024]));
        let end = upload_once(client, url, payload, counter).await;
        assert_eq!(end, StreamEnd::RequestFailed, "upload");
    }

    /// A timeout while the response body is read cut short a transfer under
    /// way, and that stream is still replaced, as in the Go client.
    #[tokio::test]
    async fn a_timeout_while_reading_the_response_body_fails_the_transfer() {
        let addr = stalling_server().await;
        let url = Url::parse(&format!("http://{addr}/")).unwrap();
        let client = client_with_timeout(Duration::from_millis(200));
        let counter = Arc::new(BytesCounter::new());

        let end = download_once(client.clone(), url.clone(), counter.clone()).await;
        assert_eq!(end, StreamEnd::TransferFailed, "download");
        let payload = Some(Bytes::from_static(&[0; 1024]));
        let end = upload_once(client, url, payload, counter).await;
        assert_eq!(end, StreamEnd::TransferFailed, "upload");
    }

    /// An upload the server never answers is not started again each time the
    /// timeout runs out: one stream, one connection, for the whole window.
    #[tokio::test]
    async fn an_upload_that_times_out_unanswered_is_not_replaced() {
        let (addr, accepted) = unanswering_server().await;
        let mut opts = opts(Duration::from_secs(1));
        opts.requests = 1;

        server_at(addr)
            .upload(
                &client_with_timeout(Duration::from_millis(200)),
                &TelemetryLog::new(),
                &opts,
            )
            .await
            .expect("an unanswered upload is still a completed measurement");

        assert_eq!(
            accepted.load(Ordering::SeqCst),
            1,
            "the upload was started again after its timeout"
        );
    }

    /// hyper takes upload frames while its write buffer has room, and each
    /// frame is counted as it is taken, so the buffer decides how much a
    /// connection has counted but not sent when the window closes. hyper's
    /// default of ~400 KB overstated a slow uplink by a quarter to a half.
    #[tokio::test]
    async fn a_connection_holds_at_most_a_buffer_and_a_frame_of_counted_upload() {
        use hyper_util::client::legacy::connect::{Connected, Connection};
        use std::pin::Pin;
        use std::task::{Context, Poll};

        /// What the peer takes before it stops reading. Not a whole number of
        /// 16-frame batches: hyper also stops at 16 queued buffers, and a
        /// budget ending on that boundary would hide an uncapped buffer.
        const TAKES: usize = 300_000;

        /// A connection whose peer takes `TAKES` bytes and then nothing more,
        /// and never answers.
        struct Stalled(Arc<AtomicUsize>);

        impl hyper::rt::Read for Stalled {
            fn poll_read(
                self: Pin<&mut Self>,
                _: &mut Context<'_>,
                _: hyper::rt::ReadBufCursor<'_>,
            ) -> Poll<std::io::Result<()>> {
                Poll::Pending
            }
        }

        impl hyper::rt::Write for Stalled {
            fn poll_write(
                self: Pin<&mut Self>,
                _: &mut Context<'_>,
                buf: &[u8],
            ) -> Poll<std::io::Result<usize>> {
                let n = buf.len().min(TAKES - self.0.load(Ordering::SeqCst));
                if n == 0 {
                    return Poll::Pending;
                }
                self.0.fetch_add(n, Ordering::SeqCst);
                Poll::Ready(Ok(n))
            }

            fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
                Poll::Ready(Ok(()))
            }

            fn poll_shutdown(
                self: Pin<&mut Self>,
                _: &mut Context<'_>,
            ) -> Poll<std::io::Result<()>> {
                Poll::Ready(Ok(()))
            }

            // As a TCP stream does, so hyper buffers the way it would for one.
            fn is_write_vectored(&self) -> bool {
                true
            }
        }

        impl Connection for Stalled {
            fn connected(&self) -> Connected {
                Connected::new()
            }
        }

        #[derive(Clone)]
        struct Connect(Arc<AtomicUsize>);

        impl tower_service::Service<http::Uri> for Connect {
            type Response = Stalled;
            type Error = std::io::Error;
            type Future = std::future::Ready<std::io::Result<Stalled>>;

            fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
                Poll::Ready(Ok(()))
            }

            fn call(&mut self, _: http::Uri) -> Self::Future {
                std::future::ready(Ok(Stalled(self.0.clone())))
            }
        }

        let taken = Arc::new(AtomicUsize::new(0));
        let client = crate::http::client_builder(1, false, true)
            .build::<_, crate::http::ReqBody>(Connect(taken.clone()));
        let counter = Arc::new(BytesCounter::new());
        let body = BodyExt::boxed(upload_body::UploadBody::new(None, counter.clone()));
        let request = http::Request::post("http://stalled.invalid/")
            .body(body)
            .unwrap();
        // Nothing ever answers, so this only ends at the timeout.
        let _ = tokio::time::timeout(Duration::from_millis(500), client.request(request)).await;

        let taken = taken.load(Ordering::SeqCst) as u64;
        assert_eq!(
            taken, TAKES as u64,
            "the connection took less than it could"
        );
        // What the peer took includes the request head and chunk framing, so
        // this slightly understates what is held; the bound has room to spare.
        let held = counter.total().saturating_sub(taken);
        let bound = (crate::http::H1_MAX_BUF + UPLOAD_CHUNK) as u64;
        assert!(
            held <= bound,
            "{held} bytes were counted but not sent, more than the {bound} a buffer and a frame hold"
        );
    }

    /// The upload phase sends over the capped pool: a response head bigger
    /// than the cap fails its request there, and a failed request is not
    /// started again. Over the default pool it would be read and repeated.
    #[tokio::test]
    async fn the_upload_phase_sends_over_the_capped_pool() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let seen = requests.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let seen = seen.clone();
                tokio::spawn(async move {
                    // Answer only once the head and the 1 KiB body are in.
                    let mut request = Vec::new();
                    let mut buf = [0u8; 8192];
                    while !request
                        .windows(4)
                        .position(|w| w == b"\r\n\r\n")
                        .is_some_and(|head| request.len() >= head + 4 + 1024)
                    {
                        match stream.read(&mut buf).await {
                            Ok(n) if n > 0 => request.extend_from_slice(&buf[..n]),
                            _ => return,
                        }
                    }
                    seen.fetch_add(1, Ordering::SeqCst);
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\nX-Pad: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        "a".repeat(2 * crate::http::H1_MAX_BUF)
                    );
                    let _ = stream.write_all(reply.as_bytes()).await;
                    while matches!(stream.read(&mut buf).await, Ok(n) if n > 0) {}
                });
            }
        });
        let mut opts = opts(Duration::from_millis(500));
        opts.requests = 1;

        server_at(addr)
            .upload(&client(), &TelemetryLog::new(), &opts)
            .await
            .unwrap();

        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "the upload read a response head bigger than the cap, so its pool is not capped"
        );
    }
}
