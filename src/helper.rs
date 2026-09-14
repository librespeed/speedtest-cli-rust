//! Runs the speed test against the selected servers and reports the results.

use std::net::IpAddr;
use std::time::Duration;

use anyhow::Context as _;
use bytes::Bytes;
use rand::Rng;
use url::Url;

use crate::cli::Cli;
use crate::defs::server::TransferOptions;
use crate::defs::{GetIPResult, Server, TelemetryExtra, TelemetryLog, TelemetryServer};
use crate::http::{HttpClient, IpFamily};
use crate::report::{self, CSVReport, Client, JSONReport, ReportServer};
use crate::spinner::Spinner;
use crate::util::round2;
use crate::{output, write_error, write_out, write_ui};

/// The number of pings used to measure latency and jitter during a test.
const PING_COUNT: usize = 10;

/// Everything the test needs that is not derived from the CLI struct directly.
pub struct TestContext<'a> {
    pub client: &'a HttpClient,
    pub telemetry: &'a TelemetryServer,
    pub family: IpFamily,
    pub source: Option<IpAddr>,
    pub no_icmp: bool,
    pub silent: bool,
}

/// Where the actual speed test happens.
pub async fn do_speed_test(
    cli: &Cli,
    servers: &[Server],
    ctx: &TestContext<'_>,
) -> anyhow::Result<()> {
    if servers.len() > 1 {
        write_ui!("Testing against {} servers\n", servers.len());
    }

    let delimiter = cli.csv_delimiter_byte()?;
    let mut reps_json: Vec<JSONReport> = Vec::new();
    let mut reps_csv: Vec<CSVReport> = Vec::new();

    for current_server in servers {
        let tlog = TelemetryLog::new();
        tlog.set_level(ctx.telemetry.get_level());

        let url = current_server
            .get_url()
            .context("Failed to get server URL")?;
        let hostname = url.host_str().unwrap_or_default().to_string();

        write_ui!(
            "Selected server: {} [{}]\n",
            output::sanitize(&current_server.name),
            output::sanitize(&hostname)
        );

        let sponsor_msg = current_server.sponsor();
        if !sponsor_msg.is_empty() {
            write_ui!("Sponsored by: {}\n", output::sanitize(&sponsor_msg));
        }

        if !current_server.is_up(ctx.client, &tlog).await {
            write_ui!(
                "Selected server {} ({}) is not responding at the moment, try again later\n",
                output::sanitize(&current_server.name),
                output::sanitize(&hostname)
            );
            if servers.len() > 1 && !ctx.silent {
                output::write_ui_blank();
            }
            continue;
        }

        let isp_info = match current_server
            .get_ip_info(ctx.client, &tlog, &cli.distance)
            .await
        {
            Ok(i) => i,
            Err(e) => return Err(e.context("Failed to get IP info")),
        };
        write_ui!(
            "You're testing from: {}\n",
            output::sanitize(&isp_info.processed_string)
        );

        // Latency and jitter.
        let spinner = if ctx.silent {
            None
        } else {
            Some(Spinner::start("Pinging server...  ", String::new))
        };

        output::stream_event(r#"{"event":"phase","phase":"ping"}"#);
        let ping_result = current_server
            .icmp_ping_and_jitter(
                ctx.client,
                &tlog,
                PING_COUNT,
                ctx.source,
                cli.interface.as_deref(),
                ctx.family,
                ctx.no_icmp,
            )
            .await;

        let (ping, jitter) = match ping_result {
            Ok(v) => v,
            Err(e) => {
                if let Some(s) = spinner {
                    s.stop("").await;
                }
                return Err(e.context("Failed to get ping and jitter"));
            }
        };

        if let Some(s) = spinner {
            s.stop(&format!("Ping: {ping:.2} ms\tJitter: {jitter:.2} ms\n"))
                .await;
        }

        let opts = TransferOptions {
            silent: ctx.silent,
            use_bytes: cli.bytes,
            use_mebi: cli.mebibytes,
            requests: cli.concurrent as usize,
            chunks: cli.chunks as usize,
            upload_size: cli.upload_size as usize,
            no_prealloc: cli.no_pre_allocate,
            duration: Duration::from_secs(cli.duration),
        };

        // Download.
        let (download_value, bytes_read) = if cli.no_download {
            write_ui!("Download test is disabled\n");
            (0.0, 0)
        } else {
            output::stream_event(r#"{"event":"phase","phase":"download"}"#);
            match current_server.download(ctx.client, &tlog, &opts).await {
                Ok(v) => v,
                Err(e) => return Err(e.context("Failed to get download speed")),
            }
        };

        // Upload.
        let (upload_value, bytes_written) = if cli.no_upload {
            write_ui!("Upload test is disabled\n");
            (0.0, 0)
        } else {
            output::stream_event(r#"{"event":"phase","phase":"upload"}"#);
            match current_server.upload(ctx.client, &tlog, &opts).await {
                Ok(v) => v,
                Err(e) => return Err(e.context("Failed to get upload speed")),
            }
        };

        if cli.simple {
            if cli.bytes {
                write_out!(
                    "Ping:\t{ping:.2} ms\tJitter:\t{jitter:.2} ms\nDownload rate:\t{}\nUpload rate:\t{}\n",
                    humanize_mbps(download_value, cli.mebibytes),
                    humanize_mbps(upload_value, cli.mebibytes)
                );
            } else {
                write_out!(
                    "Ping:\t{ping:.2} ms\tJitter:\t{jitter:.2} ms\nDownload rate:\t{download_value:.2} Mbps\nUpload rate:\t{upload_value:.2} Mbps\n"
                );
            }
        }

        // Telemetry and share link.
        let mut share_link = String::new();
        if ctx.telemetry.get_level() > 0 {
            let extra = TelemetryExtra {
                server_name: current_server.name.clone(),
                extra: cli.telemetry_extra.clone().unwrap_or_default(),
            };

            match send_telemetry(
                ctx.client,
                ctx.telemetry,
                &isp_info,
                download_value,
                upload_value,
                ping,
                jitter,
                &tlog.contents(),
                &extra,
            )
            .await
            {
                Ok(link) => {
                    share_link = link.clone();
                    // Only goes to stdout when neither --json nor --csv is used.
                    if !cli.json && !cli.csv {
                        if cli.simple {
                            write_out!("Share your result: {link}\n");
                        } else {
                            write_ui!("Share your result: {link}\n");
                        }
                    }
                }
                Err(e) => write_error!("Error when sending telemetry data: {e}\n"),
            }
        }

        // --csv takes precedence over --json, matching speedtest-cli.
        if cli.csv {
            reps_csv.push(CSVReport {
                timestamp: report::timestamp_now(),
                name: current_server.name.clone(),
                address: current_server.server.clone(),
                ping: round2(ping),
                jitter: round2(jitter),
                download: round2(download_value),
                upload: round2(upload_value),
                share: share_link,
                ip: isp_info.ip(),
            });
        } else if cli.json || cli.json_stream {
            let mut ip_info = isp_info.ip_info();
            ip_info.readme = String::new();
            ip_info.ip = isp_info.ip();

            reps_json.push(JSONReport {
                timestamp: report::timestamp_now(),
                server: ReportServer {
                    name: current_server.name.clone(),
                    url: current_server.server.clone(),
                },
                client: Client { ip_info },
                bytes_sent: bytes_written,
                bytes_received: bytes_read,
                ping: round2(ping),
                jitter: round2(jitter),
                upload: round2(upload_value),
                download: round2(download_value),
                share: share_link,
            });
        }

        // Add a blank line after each test when testing multiple servers.
        if servers.len() > 1 && !ctx.silent {
            output::write_ui_blank();
        }
    }

    if cli.csv {
        match report::csv_rows(&reps_csv, delimiter) {
            Ok(s) => write_out!("{s}"),
            Err(e) => write_error!("Error generating CSV report: {e}\n"),
        }
    } else if cli.json_stream {
        // The reports are the same array --json prints, wrapped as the final
        // event so a consumer needs one parser for the whole stream.
        match serde_json::to_string(&reps_json) {
            Ok(s) => output::stream_event(&format!(r#"{{"event":"result","reports":{s}}}"#)),
            Err(e) => write_error!("Error generating JSON report: {e}\n"),
        }
    } else if cli.json {
        match serde_json::to_string(&reps_json) {
            // serde_json does not terminate its output, and a document that
            // ends mid-line makes a shell prompt land on top of it and leaves
            // line-oriented tools with an unterminated last line.
            Ok(s) => write_out!("{s}\n"),
            Err(e) => write_error!("Error generating JSON report: {e}\n"),
        }
    }

    Ok(())
}

/// Sends the result to the telemetry server and returns the share link.
#[allow(clippy::too_many_arguments)]
async fn send_telemetry(
    client: &HttpClient,
    telemetry: &TelemetryServer,
    isp_info: &GetIPResult,
    download: f64,
    upload: f64,
    ping: f64,
    jitter: f64,
    logs: &str,
    extra: &TelemetryExtra,
) -> anyhow::Result<String> {
    let isp_json = serde_json::to_string(isp_info)?;
    let extra_json = serde_json::to_string(extra)?;

    let (content_type, body) = multipart_form(&[
        ("ispinfo", &isp_json),
        ("dl", &format!("{download:.2}")),
        ("ul", &format!("{upload:.2}")),
        ("ping", &format!("{ping:.2}")),
        ("jitter", &format!("{jitter:.2}")),
        ("log", logs),
        ("extra", &extra_json),
    ]);

    let telemetry_url = telemetry.get_path()?;
    let (status, response) = client
        .post_bytes(&telemetry_url, &content_type, body)
        .await?;
    if !status.is_success() {
        anyhow::bail!("telemetry server returned HTTP {status}");
    }

    let response = String::from_utf8_lossy(&response);
    let parts: Vec<&str> = response.split(' ').collect();
    if parts.len() != 2 {
        // The body is whatever the telemetry server chose to send, and this
        // error is printed unconditionally, so it is sanitized and capped
        // rather than echoed whole.
        let shown: String = output::sanitize(&response).chars().take(200).collect();
        anyhow::bail!("server returned invalid response: {shown}");
    }

    let mut result_url: Url = telemetry.get_share()?;
    result_url.query_pairs_mut().append_pair("id", parts[1]);
    Ok(result_url.to_string())
}

/// Builds a `multipart/form-data` body, returning the content type and bytes.
fn multipart_form(fields: &[(&str, &str)]) -> (String, Bytes) {
    let boundary: String = {
        let mut rng = rand::thread_rng();
        (0..60)
            .map(|_| char::from(b'a' + rng.gen_range(0..26)))
            .collect()
    };

    let mut body: Vec<u8> = Vec::new();
    for (name, value) in fields {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n")
                .as_bytes(),
        );
        body.extend_from_slice(value.as_bytes());
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

    (
        format!("multipart/form-data; boundary={boundary}"),
        Bytes::from(body),
    )
}

/// Renders a Mbps figure as a byte rate, for `--bytes --simple`.
pub fn humanize_mbps(mbps: f64, use_mebi: bool) -> String {
    let val = mbps / 8.0;
    let base: f64 = if use_mebi { 1024.0 } else { 1000.0 };

    if val < 1.0 {
        let kb = val * base;
        if kb < 1.0 {
            format!("{:.2} bytes/s", kb * base)
        } else {
            format!("{kb:.2} KB/s")
        }
    } else if val > base {
        format!("{:.2} GB/s", val / base)
    } else {
        format!("{val:.2} MB/s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanize_mbps_matches_go() {
        assert_eq!(humanize_mbps(80.0, false), "10.00 MB/s");
        assert_eq!(humanize_mbps(0.004, false), "500.00 bytes/s");
        assert_eq!(humanize_mbps(0.08, false), "10.00 KB/s");
        assert_eq!(humanize_mbps(80000.0, false), "10.00 GB/s");
    }

    #[test]
    fn multipart_form_is_well_formed() {
        let (ct, body) = multipart_form(&[("dl", "1.00")]);
        let boundary = ct.strip_prefix("multipart/form-data; boundary=").unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.starts_with(&format!("--{boundary}\r\n")));
        assert!(body.contains("Content-Disposition: form-data; name=\"dl\"\r\n\r\n1.00\r\n"));
        assert!(body.ends_with(&format!("--{boundary}--\r\n")));
    }
}
