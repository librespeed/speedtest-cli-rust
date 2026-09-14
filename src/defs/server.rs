//! A speed test server and the measurements performed against it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use bytes::Bytes;
use http::{Method, StatusCode};
use http_body_util::BodyExt;
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;
use url::Url;

use crate::defs::bytes_counter::{random_data, BytesCounter};
use crate::defs::telemetry::TelemetryLog;
use crate::defs::GetIPResult;
use crate::http::{empty_body, HttpClient, IpFamily};
use crate::ping::{compute_jitter, icmp_rtts, resolve_host};
use crate::spinner::Spinner;
use crate::util::{avg, go_duration, stddev, url_join_path};
use crate::{write_debug, write_ui};

/// The stagger between starting concurrent transfer streams.
const RAMP_UP_DELAY: Duration = Duration::from_millis(200);
/// The chunk size the upload body is fed to the connection in.
const UPLOAD_CHUNK: usize = 64 * 1024;

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
        let result = client.get_head(&url, PROBE_BODY_PEEK).await;
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

        // One clock for the whole test window: the deadline and the ticker's
        // elapsed time both measure from here, after ramp-up. A ticker that
        // started earlier would run ahead of the test itself.
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
            ticker.stop().await;
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

        // Count what the kernel takes, not what hyper asks for. hyper reads
        // several hundred kilobytes ahead per connection to fill its write
        // queue, and the end of the window throws that away again -- after it
        // had already been added to the total. On a slow uplink the queue is a
        // large share of everything the test moved.
        counter.start();
        client.write_meter().install(counter.clone());
        let spinner = self.start_transfer_spinner("Uploading...  ", opts, &counter);

        let mut tasks: JoinSet<StreamEnd> = JoinSet::new();
        for _ in 0..opts.requests {
            tasks.spawn(upload_once(client.clone(), url.clone(), payload.clone()));
            tokio::time::sleep(RAMP_UP_DELAY).await;
        }

        // One clock for the whole test window: the deadline and the ticker's
        // elapsed time both measure from here, after ramp-up. A ticker that
        // started earlier would run ahead of the test itself.
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
                        tasks.spawn(upload_once(client.clone(), url.clone(), payload.clone()));
                    }
                }
            }
        }
        // Abort the in-flight transfers and wait for them to unwind before
        // reading the counter, so the reported total cannot change under us.
        tasks.shutdown().await;
        client.write_meter().clear();
        if let Some(ticker) = ticker {
            ticker.stop().await;
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
    /// same counter.
    fn start_progress_ticker(
        phase: &'static str,
        counter: &Arc<BytesCounter>,
        duration: Duration,
        started: Instant,
    ) -> Option<ProgressTicker> {
        if !crate::output::is_stream() {
            return None;
        }
        let counter = counter.clone();
        let stop = Arc::new(tokio::sync::Notify::new());
        let wait_for_stop = stop.clone();
        let handle = tokio::spawn(async move {
            // A ticker, not a sleep loop: sleeping a second between events
            // adds each event's own cost to the next interval, so the events
            // drift away from the seconds they claim to report.
            let period = Duration::from_secs(1);
            let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
            // A tick the task was too busy to take is dropped, and the next
            // one lands on the original schedule, rather than firing a burst
            // to catch up.
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    _ = wait_for_stop.notified() => return,
                    _ = ticker.tick() => {}
                }
                let elapsed = started.elapsed().as_secs_f64();
                // A speed test is bounded by time, not by volume, so percent
                // done is elapsed over the configured duration -- exact, and
                // the only notion of "how much is left" the test has. It is
                // truncated, so the run reads 99 until the window is over.
                let percent = (elapsed / duration.as_secs_f64() * 100.0).min(100.0) as u32;
                let event = crate::report::ProgressEvent {
                    event: "progress",
                    phase,
                    seconds: (elapsed * 10.0).round() / 10.0,
                    mbps: (counter.avg_mbps() * 100.0).round() / 100.0,
                    progress: percent,
                };
                match serde_json::to_string(&event) {
                    Ok(line) => crate::output::stream_event(&line),
                    Err(e) => crate::write_error!("Error generating stream event: {e}\n"),
                }
            }
        });
        Some(ProgressTicker { stop, handle })
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

/// A running `--json-stream` progress ticker.
struct ProgressTicker {
    stop: Arc<tokio::sync::Notify>,
    handle: tokio::task::JoinHandle<()>,
}

impl ProgressTicker {
    /// Ends the ticker and waits for it, so a late progress event can never
    /// land after the phase event that follows it.
    async fn stop(self) {
        self.stop.notify_one();
        let _ = self.handle.await;
    }
}

/// Why a transfer stream ended, which decides whether it is replaced.
///
/// A stream that carried data and then broke is replaced, so the phase keeps
/// the link busy for the whole window; one whose request never got off the
/// ground is not, because retrying it as fast as it fails would spin. This
/// mirrors the Go client, which respawns after a body error but declines to
/// after a failed request.
#[derive(Clone, Copy, PartialEq, Eq)]
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
    let fut = async {
        let resp = match client.send_streaming(Method::GET, &url, empty_body).await {
            Ok(r) => r,
            Err(e) => {
                write_debug!("Failed when making HTTP request: {e}\n");
                return StreamEnd::RequestFailed;
            }
        };

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

    // A stream cut short by our own timeout carried data and is replaced, the
    // same way the Go client treats a body read killed by its client timeout.
    within_timeout(&client, fut)
        .await
        .unwrap_or(StreamEnd::TransferFailed)
}

/// Uploads once. The bytes are counted by the client's write meter, on the
/// socket.
async fn upload_once(client: HttpClient, url: Url, payload: Option<Bytes>) -> StreamEnd {
    let fut = async {
        let mk_body = || BodyExt::boxed(upload_body::UploadBody::new(payload.clone()));

        let resp = match client.send_streaming(Method::POST, &url, mk_body).await {
            Ok(r) => r,
            Err(e) => {
                write_debug!("Failed when making HTTP request: {e}\n");
                return StreamEnd::RequestFailed;
            }
        };

        // Discard the response frame by frame so the connection can be reused.
        // Collecting it instead retained whatever the server chose to send, per
        // stream, for the whole test -- the one body read here with no cap.
        let mut body = resp.into_body();
        while let Some(frame) = body.frame().await {
            if let Err(e) = frame {
                write_debug!("Failed when reading HTTP response: {e}\n");
                return StreamEnd::TransferFailed;
            }
        }
        StreamEnd::Completed
    };

    within_timeout(&client, fut)
        .await
        .unwrap_or(StreamEnd::TransferFailed)
}

/// Runs a transfer stream under `--timeout`, or without one when it is zero,
/// the same as every other request. Returns `None` if the timeout cut it short.
async fn within_timeout<T>(
    client: &HttpClient,
    fut: impl std::future::Future<Output = T>,
) -> Option<T> {
    let timeout = client.timeout();
    if timeout.is_zero() {
        return Some(fut.await);
    }
    tokio::time::timeout(timeout, fut).await.ok()
}

/// The upload request body, in a module of its own so that its fields are
/// private: `UploadBody::new` is the only way to build one, so every upload
/// gets its filler decided there.
mod upload_body {
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use bytes::Bytes;
    use http_body::{Body, Frame, SizeHint};

    use super::UPLOAD_CHUNK;
    use crate::defs::bytes_counter::random_data;

    /// The request body for the upload test.
    ///
    /// With a payload it sends exactly that blob once; without one it produces
    /// random data indefinitely, until the test duration cancels the request.
    pub(super) struct UploadBody {
        payload: Option<Bytes>,
        pos: usize,
        /// The block sent over and over when there is no pre-allocated payload,
        /// and empty when there is one.
        filler: Bytes,
    }

    impl UploadBody {
        pub(super) fn new(payload: Option<Bytes>) -> Self {
            // Only the endless stream ever sends the filler.
            let filler = match payload {
                Some(_) => Bytes::new(),
                None => Bytes::from(random_data(UPLOAD_CHUNK)),
            };
            Self {
                payload,
                pos: 0,
                filler,
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
                // payload, which 64 KiB does not; generating it again for every
                // frame cost about seventeen times the CPU of the pre-allocated
                // path and saved no memory at all.
                None => this.filler.clone(),
            };

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
            let with_payload = UploadBody::new(Some(Bytes::from_static(b"payload")));
            assert!(
                with_payload.filler.is_empty(),
                "filler was generated for a pre-allocated payload"
            );
            assert_eq!(UploadBody::new(None).filler.len(), UPLOAD_CHUNK);
        }
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
}

#[cfg(test)]
mod transfer_tests {
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
        let client = HttpClient::new(
            BindOptions::default(),
            &TlsSettings {
                ca_cert: None,
                skip_verify: false,
                http2: false,
            },
            Duration::ZERO,
            3,
            "test",
        )
        .unwrap();

        let counter = Arc::new(BytesCounter::new());
        let end = download_once(client.clone(), url.clone(), counter.clone()).await;
        assert!(
            matches!(end, StreamEnd::Completed),
            "the download stream was cut short"
        );
        assert_eq!(counter.total(), 4, "the whole response must be counted");

        let payload = Some(Bytes::from_static(&[0; 1024]));
        let end = upload_once(client, url, payload).await;
        assert!(
            matches!(end, StreamEnd::Completed),
            "the upload stream was cut short"
        );
    }

    fn client() -> HttpClient {
        HttpClient::new(
            BindOptions::default(),
            &TlsSettings {
                ca_cert: None,
                skip_verify: false,
                http2: false,
            },
            Duration::from_secs(5),
            3,
            "test",
        )
        .unwrap()
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
    /// for the number of bytes read, sent once the client closes the connection.
    async fn silent_server() -> (std::net::SocketAddr, tokio::sync::oneshot::Receiver<u64>) {
        use tokio::io::AsyncReadExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (report, arrived) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut total = 0u64;
            let mut buf = [0u8; 8192];
            while let Ok(n) = stream.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                total += n as u64;
            }
            let _ = report.send(total);
        });
        (addr, arrived)
    }

    /// The upload total is counted where the socket takes the bytes, so it is
    /// exactly what went over the connection, request head included. Counting
    /// in the body instead also took in data hyper had queued but not yet sent.
    #[tokio::test]
    async fn the_upload_total_is_what_went_over_the_socket() {
        let (addr, arrived) = silent_server().await;
        let mut opts = opts(Duration::from_secs(1));
        opts.requests = 1;

        let (_, counted) = server_at(addr)
            .upload(&client(), &TelemetryLog::new(), &opts)
            .await
            .expect("an unanswered upload is still a completed measurement");
        let arrived = tokio::time::timeout(Duration::from_secs(10), arrived)
            .await
            .expect("the client closed its connection")
            .unwrap();

        assert!(counted > 0, "nothing was counted");
        assert_eq!(
            counted, arrived,
            "the upload total is not what went over the socket"
        );
    }
}
