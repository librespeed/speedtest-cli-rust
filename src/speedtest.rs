//! Program flow: option handling, server list loading and server selection.

use std::io::Read as _;
use std::net::IpAddr;
use std::str::FromStr;
use std::time::Duration;

use anyhow::Context as _;
use futures_util::stream::{self, StreamExt};

use crate::cli::Cli;
use crate::defs::{self, Server, TelemetryLog, TelemetryServer};
use crate::helper::{self, TestContext};
use crate::http::connector::resolve;
use crate::http::{BindOptions, HttpClient, IpFamily, TlsSettings};
use crate::report;
use crate::{output, write_debug, write_out, write_ui};

/// The default remote server JSON URL.
const SERVER_LIST_URL: &str = "https://librespeed.org/backend-servers/servers.php";

const DEFAULT_TELEMETRY_LEVEL: &str = "basic";
const DEFAULT_TELEMETRY_SERVER: &str = "https://librespeed.org";
const DEFAULT_TELEMETRY_PATH: &str = "/results/telemetry.php";
const DEFAULT_TELEMETRY_SHARE: &str = "/results/";

/// The number of servers pinged in parallel during selection.
const PING_WORKERS: usize = 10;

/// Cap on a server list read from a file or stdin, so a runaway pipe cannot
/// exhaust memory on a router.
const MAX_LOCAL_JSON: u64 = 8 * 1024 * 1024;

/// Which URL scheme the server list entries are forced to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ForceScheme {
    Nothing,
    Https,
    Http,
}

/// Handles the speed test(s).
pub async fn run(cli: &Cli) -> anyhow::Result<()> {
    if cli.silent() {
        output::set_quiet(true);
    }
    if cli.debug {
        output::set_debug(true);
    }
    if cli.json_stream {
        output::set_stream(true);
    }

    if cli.version {
        print_version();
        return Ok(());
    }

    if cli.source.is_some() && cli.interface.is_some() {
        anyhow::bail!("incompatible options 'source' and 'interface'");
    }

    let delimiter = cli.csv_delimiter_byte()?;

    // If --csv-header is given, print the header and exit (same behavior as speedtest-cli).
    if cli.csv_header {
        write_out!("{}", report::csv_header(delimiter)?);
        return Ok(());
    }

    let telemetry = resolve_telemetry(cli)?;

    let family = match (cli.ipv4, cli.ipv6) {
        (true, _) => IpFamily::V4,
        (_, true) => IpFamily::V6,
        _ => IpFamily::Any,
    };

    let source = match &cli.source {
        Some(s) => Some(parse_source(s, family).await?),
        None => None,
    };

    let mut no_icmp = cli.no_icmp;
    let bind = BindOptions {
        source,
        interface: cli.interface.clone(),
        fwmark: cli.fwmark,
        family,
    };
    if cli.interface.is_some() || cli.fwmark > 0 {
        bind.validate()?;
        // ICMP ping does not support interface binding.
        no_icmp = true;
    }

    let client = HttpClient::new(
        bind,
        &TlsSettings {
            ca_cert: cli.ca_cert.as_deref(),
            skip_verify: cli.skip_cert_verify,
            http2: cli.http2,
        },
        Duration::from_secs(cli.timeout),
        cli.concurrent as usize,
        &defs::user_agent(),
    )?;

    // No scheme is forced by default; --secure forces https, --insecure forces http.
    let force_scheme = if cli.secure {
        ForceScheme::Https
    } else if cli.insecure {
        ForceScheme::Http
    } else {
        ForceScheme::Nothing
    };

    let servers = load_servers(cli, &client, force_scheme).await?;
    write_debug!("Loaded {} server(s)\n", servers.len());

    // If --list is given, list all the servers fetched and exit.
    if cli.list {
        for svr in &servers {
            let sponsor = svr.sponsor();
            let sponsor_msg = if sponsor.is_empty() {
                String::new()
            } else {
                format!(" [Sponsor: {}]", output::sanitize(&sponsor))
            };
            // --list goes to stdout, so a newline smuggled into a server name
            // would forge an entry for anything parsing it.
            write_out!(
                "{}: {} ({}) {}\n",
                svr.id,
                output::sanitize(&svr.name),
                output::sanitize(&svr.server),
                sponsor_msg
            );
        }
        return Ok(());
    }

    let ctx = TestContext {
        client: &client,
        telemetry: &telemetry,
        family,
        source,
        no_icmp,
        silent: cli.silent(),
    };

    // If --server is given, test against all of them.
    if !cli.server.is_empty() {
        return helper::do_speed_test(cli, &servers, &ctx).await;
    }

    // Otherwise select the fastest server from the list.
    write_ui!("Selecting the fastest server based on ping\n");

    write_debug!(
        "Probing {} server(s), {PING_WORKERS} at a time\n",
        servers.len()
    );
    let ping_list = ping_all(&servers, &ctx).await;
    write_debug!("{} server(s) responded\n", ping_list.len());
    if ping_list.is_empty() {
        output::fatal("No server is currently available, please try again later.");
    }

    let (best_idx, _) = ping_list
        .iter()
        .filter(|(_, ping)| *ping > 0.0)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .or_else(|| ping_list.first())
        .copied()
        .expect("ping list is not empty");

    write_debug!(
        "Fastest: {} ({}) at {:.2} ms\n",
        output::sanitize(&servers[best_idx].name),
        servers[best_idx].id,
        ping_list
            .iter()
            .find(|(i, _)| *i == best_idx)
            .map(|(_, p)| *p)
            .unwrap_or_default()
    );
    helper::do_speed_test(cli, std::slice::from_ref(&servers[best_idx]), &ctx).await
}

fn print_version() {
    // The revision is carried when it is known, which is what tells two builds
    // of the same version apart. A release tarball has no git metadata and no
    // packager-supplied revision, and there the line reads exactly as the Go
    // client's does.
    if defs::REVISION == "unknown" {
        write_out!(
            "{} {} (built on {})\n",
            defs::PROG_NAME,
            defs::PROG_VERSION,
            defs::BUILD_DATE
        );
    } else {
        write_out!(
            "{} {} ({}, built on {})\n",
            defs::PROG_NAME,
            defs::PROG_VERSION,
            defs::REVISION,
            defs::BUILD_DATE
        );
    }
    write_out!("{}\n", env!("CARGO_PKG_REPOSITORY"));
    write_out!("Licensed under GNU Lesser General Public License v3.0\n");
    write_out!("LibreSpeed\tCopyright (C) 2016-2020 Federico Dossena\n");
    write_out!("librespeed-cli\tCopyright (C) 2020 Maddie Zhan\n");
}

/// Reads telemetry settings if --share or any --telemetry option is given.
fn resolve_telemetry(cli: &Cli) -> anyhow::Result<TelemetryServer> {
    let any_telemetry_option = cli.telemetry_json.is_some()
        || cli.telemetry_level.is_some()
        || cli.telemetry_server.is_some()
        || cli.telemetry_path.is_some()
        || cli.telemetry_share.is_some();

    let mut telemetry = TelemetryServer::default();
    if !cli.share && !any_telemetry_option {
        return Ok(telemetry);
    }

    if let Some(path) = &cli.telemetry_json {
        let b = std::fs::read(path).with_context(|| format!("Cannot read {path}"))?;
        telemetry = serde_json::from_slice(&b).with_context(|| format!("Error parsing {path}"))?;
    }

    match &cli.telemetry_level {
        Some(level) => {
            if !defs::telemetry::LEVELS.contains(&level.as_str()) {
                output::fatal(format!("Unsupported telemetry level: {level}"));
            }
            telemetry.level = level.clone();
        }
        None => {
            if telemetry.level.is_empty() {
                telemetry.level = DEFAULT_TELEMETRY_LEVEL.to_string();
            }
        }
    }

    apply_default(
        &mut telemetry.server,
        &cli.telemetry_server,
        DEFAULT_TELEMETRY_SERVER,
    );
    apply_default(
        &mut telemetry.path,
        &cli.telemetry_path,
        DEFAULT_TELEMETRY_PATH,
    );
    apply_default(
        &mut telemetry.share,
        &cli.telemetry_share,
        DEFAULT_TELEMETRY_SHARE,
    );

    Ok(telemetry)
}

fn apply_default(field: &mut String, flag: &Option<String>, default: &str) {
    match flag {
        Some(v) => *field = v.clone(),
        None => {
            if field.is_empty() {
                *field = default.to_string();
            }
        }
    }
}

/// Parses `--source` into an address of the requested family.
async fn parse_source(src: &str, family: IpFamily) -> anyhow::Result<IpAddr> {
    if let Ok(ip) = IpAddr::from_str(src) {
        let ok = match family {
            IpFamily::Any => true,
            IpFamily::V4 => ip.is_ipv4(),
            IpFamily::V6 => ip.is_ipv6(),
        };
        if !ok {
            let want = if family == IpFamily::V6 {
                "IPv6"
            } else {
                "IPv4"
            };
            anyhow::bail!("Address {src} is not a valid {want} address");
        }
        write_debug!("Using {src} as source IP\n");
        return Ok(ip);
    }

    match resolve(src, 0, family).await {
        Ok(addrs) => {
            let ip = addrs[0].ip();
            write_debug!("Using {ip} as source IP\n");
            Ok(ip)
        }
        Err(e) => Err(anyhow::Error::new(e).context("Error parsing source IP")),
    }
}

/// Loads the server list from the configured source.
async fn load_servers(
    cli: &Cli,
    client: &HttpClient,
    force_scheme: ForceScheme,
) -> anyhow::Result<Vec<Server>> {
    let filter = !cli.list;

    let raw = match &cli.local_json {
        Some(path) if path == "-" => {
            write_ui!("Using local JSON server list from stdin\n");
            let mut buf = Vec::new();
            std::io::Read::take(std::io::stdin(), MAX_LOCAL_JSON + 1)
                .read_to_end(&mut buf)
                .context("cannot read server list from stdin")?;
            if buf.len() as u64 > MAX_LOCAL_JSON {
                anyhow::bail!("server list from stdin exceeds {MAX_LOCAL_JSON} bytes");
            }
            bytes::Bytes::from(buf)
        }
        Some(path) => {
            write_ui!("Using local JSON server list: {path}\n");
            let file = std::fs::File::open(path).with_context(|| format!("cannot read {path}"))?;
            let mut buf = Vec::new();
            std::io::Read::take(file, MAX_LOCAL_JSON + 1)
                .read_to_end(&mut buf)
                .with_context(|| format!("cannot read {path}"))?;
            if buf.len() as u64 > MAX_LOCAL_JSON {
                anyhow::bail!("{path} exceeds {MAX_LOCAL_JSON} bytes");
            }
            bytes::Bytes::from(buf)
        }
        None => {
            let server_url = cli.server_json.as_deref().unwrap_or(SERVER_LIST_URL);
            write_ui!("Retrieving server list from {server_url}\n");

            match fetch_server_list(client, server_url).await {
                Ok(b) => b,
                Err(_) => {
                    // The discovery endpoint lives at the site root; appending
                    // it to the full list URL would ask for
                    // .../servers.php/.well-known/librespeed, which can never
                    // answer. Build the retry from the origin instead.
                    let retry = url::Url::parse(server_url)
                        .map(|u| {
                            format!(
                                "{}/.well-known/librespeed",
                                u.origin().ascii_serialization()
                            )
                        })
                        .unwrap_or_else(|_| format!("{server_url}/.well-known/librespeed"));

                    write_ui!("Retry with /.well-known/librespeed\n");
                    fetch_server_list(client, &retry)
                        .await
                        .context("Error when fetching server list")?
                }
            }
        }
    };

    let servers: Vec<Server> =
        serde_json::from_slice(&raw).context("Error when fetching server list")?;

    preprocess_servers(servers, force_scheme, &cli.exclude, &cli.server, filter)
}

async fn fetch_server_list(client: &HttpClient, url: &str) -> anyhow::Result<bytes::Bytes> {
    let url = url::Url::parse(url).with_context(|| format!("invalid server list URL: {url}"))?;
    let (status, body) = client.get_bytes(&url).await?;
    if !status.is_success() {
        anyhow::bail!("server list request returned HTTP {status}");
    }
    // Handed on as it came off the wire: serde parses from the borrowed
    // bytes, so a hostile list does not get to be held twice.
    Ok(body)
}

/// Rewrites a server URL's scheme, as Go's `url.URL.Scheme` assignment does.
pub fn apply_scheme(raw: &str, force: ForceScheme) -> String {
    let (scheme, rest) = match raw.find("://") {
        Some(i) => (Some(&raw[..i]), &raw[i + 3..]),
        None => (None, raw),
    };

    let new_scheme = match force {
        ForceScheme::Https => "https",
        ForceScheme::Http => "http",
        // If no scheme is defined and none is forced, default to http.
        ForceScheme::Nothing => scheme.unwrap_or("http"),
    };

    format!("{new_scheme}://{rest}")
}

/// Applies the scheme rules and the --server / --exclude filters.
pub fn preprocess_servers(
    mut servers: Vec<Server>,
    force_scheme: ForceScheme,
    excludes: &[i64],
    specific: &[i64],
    filter: bool,
) -> anyhow::Result<Vec<Server>> {
    if !excludes.is_empty() && !specific.is_empty() {
        anyhow::bail!("either --exclude or --server can be used");
    }

    for server in &mut servers {
        server.server = apply_scheme(&server.server, force_scheme);
    }

    if !filter {
        return Ok(servers);
    }

    if !excludes.is_empty() {
        return Ok(servers
            .into_iter()
            .filter(|s| !excludes.contains(&s.id))
            .collect());
    }

    // The special value -1 tests every server.
    if !specific.is_empty() && !specific.contains(&-1) {
        let ret: Vec<Server> = servers
            .into_iter()
            .filter(|s| specific.contains(&s.id))
            .collect();
        if ret.is_empty() {
            anyhow::bail!("specified server(s) not found: {specific:?}");
        }
        return Ok(ret);
    }

    Ok(servers)
}

/// Pings every server, returning `(index, ping)` for those that responded.
async fn ping_all(servers: &[Server], ctx: &TestContext<'_>) -> Vec<(usize, f64)> {
    stream::iter(servers.iter().enumerate())
        .map(|(idx, server)| async move {
            let tlog = TelemetryLog::new();

            let hostname = match server.get_url() {
                Ok(u) => u.host_str().unwrap_or_default().to_string(),
                Err(_) => {
                    write_debug!(
                        "Server URL is invalid for {} ({}), skipping\n",
                        output::sanitize(&server.name),
                        output::sanitize(&server.server)
                    );
                    return None;
                }
            };

            // Check the server is up before spending time on a ping.
            if !server.is_up(ctx.client, &tlog).await.up {
                write_debug!(
                    "Server {} ({}) doesn't seem to be up, skipping\n",
                    output::sanitize(&server.name),
                    output::sanitize(&hostname)
                );
                return None;
            }

            match server
                .icmp_ping_and_jitter(
                    ctx.client,
                    &tlog,
                    1,
                    ctx.source,
                    None,
                    ctx.family,
                    ctx.no_icmp,
                )
                .await
            {
                Ok((ping, _)) => {
                    write_debug!(
                        "  {} ({hostname}) {ping:.2} ms\n",
                        output::sanitize(&server.name)
                    );
                    Some((idx, ping))
                }
                Err(_) => {
                    write_debug!(
                        "Can't ping server {} ({}), skipping\n",
                        output::sanitize(&server.name),
                        output::sanitize(&hostname)
                    );
                    None
                }
            }
        })
        .buffer_unordered(PING_WORKERS)
        .filter_map(|r| async move { r })
        .collect()
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(id: i64, url: &str) -> Server {
        Server {
            id,
            server: url.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn scheme_is_forced_when_requested() {
        assert_eq!(
            apply_scheme("http://example.com", ForceScheme::Https),
            "https://example.com"
        );
        assert_eq!(
            apply_scheme("https://example.com", ForceScheme::Http),
            "http://example.com"
        );
    }

    #[test]
    fn scheme_defaults_to_http_when_absent() {
        assert_eq!(
            apply_scheme("example.com/backend", ForceScheme::Nothing),
            "http://example.com/backend"
        );
        assert_eq!(
            apply_scheme("https://example.com", ForceScheme::Nothing),
            "https://example.com"
        );
    }

    #[test]
    fn exclude_removes_listed_servers() {
        let servers = vec![server(1, "https://a"), server(2, "https://b")];
        let out = preprocess_servers(servers, ForceScheme::Nothing, &[1], &[], true).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, 2);
    }

    #[test]
    fn specific_keeps_only_listed_servers() {
        let servers = vec![server(1, "https://a"), server(2, "https://b")];
        let out = preprocess_servers(servers, ForceScheme::Nothing, &[], &[2], true).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, 2);
    }

    #[test]
    fn specific_minus_one_keeps_everything() {
        let servers = vec![server(1, "https://a"), server(2, "https://b")];
        let out = preprocess_servers(servers, ForceScheme::Nothing, &[], &[-1], true).unwrap();
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn unknown_specific_server_is_an_error() {
        let servers = vec![server(1, "https://a")];
        assert!(preprocess_servers(servers, ForceScheme::Nothing, &[], &[99], true).is_err());
    }

    #[test]
    fn exclude_and_server_are_mutually_exclusive() {
        let servers = vec![server(1, "https://a")];
        assert!(preprocess_servers(servers, ForceScheme::Nothing, &[1], &[2], true).is_err());
    }

    #[test]
    fn list_mode_skips_filtering() {
        let servers = vec![server(1, "https://a"), server(2, "https://b")];
        let out = preprocess_servers(servers, ForceScheme::Nothing, &[], &[2], false).unwrap();
        assert_eq!(out.len(), 2);
    }
}
