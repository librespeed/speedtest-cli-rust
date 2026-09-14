//! ICMP echo based latency measurement.

use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use surge_ping::{Client, Config, PingIdentifier, PingSequence, ICMP};

use crate::http::IpFamily;

/// Per-echo timeout. The Go version budgets `count` seconds for the whole run.
const PING_TIMEOUT: Duration = Duration::from_secs(1);
/// One echo a second, the interval pro-bing gives the Go client. Firing them
/// back to back finished the phase in under a second and made jitter -- an
/// average over deltas between consecutive samples -- measure a different
/// property of the link.
const PING_INTERVAL: Duration = Duration::from_secs(1);
/// pro-bing's payload size, so both clients put the same thing on the wire.
const PAYLOAD: [u8; 24] = [0; 24];

/// Resolves a hostname to a single address of the requested family.
pub async fn resolve_host(host: &str, family: IpFamily) -> anyhow::Result<IpAddr> {
    let addrs = crate::http::connector::resolve(host, 0, family).await?;
    addrs
        .first()
        .map(|a| a.ip())
        .ok_or_else(|| anyhow::anyhow!("no address found for {host}"))
}

/// Sends `count` ICMP echos and returns the successful round-trip times, in
/// milliseconds. Unlike the Go version, sub-millisecond precision is preserved.
pub async fn icmp_rtts(
    target: IpAddr,
    count: usize,
    source: Option<IpAddr>,
    interface: Option<&str>,
) -> anyhow::Result<Vec<f64>> {
    let kind = if target.is_ipv4() { ICMP::V4 } else { ICMP::V6 };

    let mut builder = Config::builder().kind(kind);
    if let Some(src) = source {
        // Binding only works when the source matches the target's family.
        if src.is_ipv4() == target.is_ipv4() {
            builder = builder.bind(SocketAddr::new(src, 0));
        }
    }
    if let Some(iface) = interface {
        builder = builder.interface(iface);
    }

    // Defaults to a SOCK_DGRAM socket, which does not need elevated privileges,
    // matching the Go implementation's unprivileged pings.
    let client = Client::new(&builder.build())?;
    let mut pinger = client.pinger(target, PingIdentifier(rand::random())).await;
    pinger.timeout(PING_TIMEOUT);

    let deadline = Instant::now() + Duration::from_secs(count.max(1) as u64);
    let mut rtts = Vec::with_capacity(count);

    for seq in 0..count {
        if Instant::now() >= deadline {
            break;
        }
        let sent = Instant::now();
        if let Ok((_, rtt)) = pinger.ping(PingSequence(seq as u16), &PAYLOAD).await {
            rtts.push(rtt.as_secs_f64() * 1000.0);
        }
        // Space the echos a second apart, measured from when this one went out
        // so a slow reply does not push the next one further away.
        if seq + 1 < count {
            tokio::time::sleep_until((sent + PING_INTERVAL).into()).await;
        }
    }

    Ok(rtts)
}

/// The jitter estimator used by LibreSpeed: an asymmetric moving average over
/// consecutive latency deltas, weighted so that increases move it faster than
/// decreases.
pub fn compute_jitter(pings: &[f64]) -> f64 {
    let mut jitter = 0.0f64;
    let mut last_ping = 0.0f64;

    for (idx, &p) in pings.iter().enumerate() {
        if idx != 0 {
            let inst_jitter = (last_ping - p).abs();
            if idx > 1 {
                if jitter > inst_jitter {
                    jitter = jitter * 0.7 + inst_jitter * 0.3;
                } else {
                    jitter = inst_jitter * 0.2 + jitter * 0.8;
                }
            }
        }
        last_ping = p;
    }

    jitter
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_is_zero_for_short_series() {
        assert_eq!(compute_jitter(&[]), 0.0);
        assert_eq!(compute_jitter(&[5.0]), 0.0);
        // The first delta is deliberately not folded in, matching upstream.
        assert_eq!(compute_jitter(&[5.0, 9.0]), 0.0);
    }

    #[test]
    fn jitter_tracks_deltas() {
        let j = compute_jitter(&[10.0, 10.0, 14.0]);
        // inst = 4.0, jitter starts at 0 so the "increase" branch applies.
        assert!((j - 0.8).abs() < 1e-9);
    }

    #[test]
    fn jitter_is_zero_for_constant_series() {
        assert_eq!(compute_jitter(&[7.0; 10]), 0.0);
    }
}
