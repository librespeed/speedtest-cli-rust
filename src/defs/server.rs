//! A speed test server and the measurements performed against it.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use bytes::Bytes;
use http::{Method, StatusCode};
use http_body::{Body, Frame, SizeHint};
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
use crate::util::{avg, stddev, url_join_path};
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
    pub async fn is_up(&self, client: &HttpClient, tlog: &TelemetryLog) -> bool {
        let t = Instant::now();

        let url = match self.get_url() {
            Ok(u) => url_join_path(&u, &self.ping_url),
            Err(e) => {
                write_debug!("Failed when creating HTTP request: {e}\n");
                return false;
            }
        };

        let result = client.get_bytes(&url).await;
        tlog.logf(format!(
            "Check backend is up took {}",
            go_duration(t.elapsed())
        ));

        match result {
            Ok((status, body)) => {
                if !body.is_empty() {
                    write_debug!(
                        "Failed when parsing get IP result: {}\n",
                        crate::output::sanitize(&String::from_utf8_lossy(&body))
                    );
                    return false;
                }
                status == StatusCode::OK
            }
            Err(e) => {
                write_debug!("Error checking for server status: {e:#}\n");
                false
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

        for _ in 0..count {
            let start = Instant::now();
            client.get_bytes(&url).await?;
            pings.push(start.elapsed().as_secs_f64() * 1000.0);
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

        let mut tasks: JoinSet<bool> = JoinSet::new();
        for _ in 0..opts.requests {
            tasks.spawn(download_once(client.clone(), url.clone(), counter.clone()));
            tokio::time::sleep(RAMP_UP_DELAY).await;
        }

        // Replace each stream as it finishes until the test duration elapses.
        // Dropping the JoinSet afterwards aborts everything still in flight.
        let refill = async {
            while let Some(res) = tasks.join_next().await {
                if matches!(res, Ok(true)) {
                    tasks.spawn(download_once(client.clone(), url.clone(), counter.clone()));
                }
            }
        };
        // One clock for the whole test window: the timeout and the ticker's
        // elapsed time both measure from here, after ramp-up. A ticker that
        // started earlier would run ahead of the test itself.
        let test_start = Instant::now();
        let ticker = Self::start_progress_ticker("download", &counter, opts.duration, test_start);
        let _ = tokio::time::timeout(opts.duration, refill).await;
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

        counter.start();
        let spinner = self.start_transfer_spinner("Uploading...  ", opts, &counter);

        let mut tasks: JoinSet<bool> = JoinSet::new();
        for _ in 0..opts.requests {
            tasks.spawn(upload_once(
                client.clone(),
                url.clone(),
                counter.clone(),
                payload.clone(),
            ));
            tokio::time::sleep(RAMP_UP_DELAY).await;
        }

        let refill = async {
            while let Some(res) = tasks.join_next().await {
                if matches!(res, Ok(true)) {
                    tasks.spawn(upload_once(
                        client.clone(),
                        url.clone(),
                        counter.clone(),
                        payload.clone(),
                    ));
                }
            }
        };
        // One clock for the whole test window: the timeout and the ticker's
        // elapsed time both measure from here, after ramp-up. A ticker that
        // started earlier would run ahead of the test itself.
        let test_start = Instant::now();
        let ticker = Self::start_progress_ticker("upload", &counter, opts.duration, test_start);
        let _ = tokio::time::timeout(opts.duration, refill).await;
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

/// Downloads once, counting every byte received. Returns whether it completed.
async fn download_once(client: HttpClient, url: Url, counter: Arc<BytesCounter>) -> bool {
    let fut = async {
        let resp = match client.send_streaming(Method::GET, &url, empty_body).await {
            Ok(r) => r,
            Err(e) => {
                write_debug!("Failed when making HTTP request: {e}\n");
                return false;
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
                    return false;
                }
            }
        }
        true
    };

    let timeout = client.timeout();
    tokio::time::timeout(timeout, fut).await.unwrap_or(false)
}

/// Uploads once, counting every byte sent. Returns whether it completed.
async fn upload_once(
    client: HttpClient,
    url: Url,
    counter: Arc<BytesCounter>,
    payload: Option<Bytes>,
) -> bool {
    let fut = async {
        let mk_body = || {
            BodyExt::boxed(UploadBody {
                payload: payload.clone(),
                pos: 0,
                counter: counter.clone(),
            })
        };

        let resp = match client.send_streaming(Method::POST, &url, mk_body).await {
            Ok(r) => r,
            Err(e) => {
                write_debug!("Failed when making HTTP request: {e}\n");
                return false;
            }
        };

        // Drain the response so the connection can be reused.
        if let Err(e) = resp.into_body().collect().await {
            write_debug!("Failed when reading HTTP response: {e}\n");
            return false;
        }
        true
    };

    let timeout = client.timeout();
    tokio::time::timeout(timeout, fut).await.unwrap_or(false)
}

/// The request body for the upload test.
///
/// With a payload it sends exactly that blob once; without one it produces
/// random data indefinitely, until the test duration cancels the request.
struct UploadBody {
    payload: Option<Bytes>,
    pos: usize,
    counter: Arc<BytesCounter>,
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
            None => Bytes::from(random_data(UPLOAD_CHUNK)),
        };

        this.counter.add(chunk.len() as u64);
        Poll::Ready(Some(Ok(Frame::data(chunk))))
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
