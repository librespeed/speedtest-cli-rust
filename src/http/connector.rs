//! A TCP connector with full socket-level control.
//!
//! This exists instead of an off-the-shelf HTTP client connector because the CLI
//! needs to bind sockets to a source address, a network interface
//! (`SO_BINDTODEVICE` / `IP_BOUND_IF`) and a firewall mark (`SO_MARK`) — the last
//! of which no high-level Rust HTTP client exposes.

use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use http::Uri;
use hyper_util::client::legacy::connect::{Connected, Connection};
use hyper_util::rt::TokioIo;
use socket2::{SockRef, TcpKeepalive};
use tokio::net::{TcpSocket, TcpStream};

/// Matches the Go implementation's `net.Dialer{Timeout: 30s, KeepAlive: 30s}`.
/// It is the budget for reaching the host, not for each address in turn.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the first address family gets to itself before the other one is
/// tried alongside it, as in Go's `net.Dialer.FallbackDelay`.
const FALLBACK_DELAY: Duration = Duration::from_millis(300);

/// The smallest slice of the budget a single address is given, matching Go's
/// `saneMinimum` in `dialSerial`.
const MIN_ATTEMPT: Duration = Duration::from_secs(2);
const KEEPALIVE: Duration = Duration::from_secs(30);

/// Which IP address family connections are restricted to.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum IpFamily {
    #[default]
    Any,
    V4,
    V6,
}

impl IpFamily {
    pub fn accepts(&self, addr: &SocketAddr) -> bool {
        match self {
            IpFamily::Any => true,
            IpFamily::V4 => addr.is_ipv4(),
            IpFamily::V6 => addr.is_ipv6(),
        }
    }

    /// The equivalent of Go's `ip` / `ip4` / `ip6` network strings.
    pub fn network(&self) -> &'static str {
        match self {
            IpFamily::Any => "ip",
            IpFamily::V4 => "ip4",
            IpFamily::V6 => "ip6",
        }
    }
}

/// Socket binding options applied to every outgoing connection.
#[derive(Clone, Debug, Default)]
pub struct BindOptions {
    /// Local source address to bind to (`--source`).
    pub source: Option<IpAddr>,
    /// Network interface to bind to (`--interface`).
    pub interface: Option<String>,
    /// Firewall mark to set on the socket (`--fwmark`), 0 means unset.
    pub fwmark: u32,
    /// Restrict connections to one address family (`--ipv4` / `--ipv6`).
    pub family: IpFamily,
}

impl BindOptions {
    /// Fails early, with the same wording as the Go implementation, when the
    /// platform cannot honour the requested interface or fwmark binding.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.interface.is_none() && self.fwmark == 0 {
            return Ok(());
        }
        #[cfg(any(target_os = "linux", target_os = "android", target_os = "fuchsia"))]
        {
            Ok(())
        }
        #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "fuchsia")))]
        {
            // IP_BOUND_IF gives us interface binding on Apple platforms; there is
            // no portable equivalent of SO_MARK anywhere but Linux.
            if self.fwmark > 0 {
                anyhow::bail!("cannot set a firewall mark on this platform");
            }
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            {
                Ok(())
            }
            #[cfg(not(any(target_os = "macos", target_os = "ios")))]
            {
                anyhow::bail!("cannot bound to interface on this platform")
            }
        }
    }
}

/// Resolves `host:port`, keeping only addresses of the configured family.
pub async fn resolve(host: &str, port: u16, family: IpFamily) -> io::Result<Vec<SocketAddr>> {
    // Strip brackets from IPv6 literals such as `[::1]`.
    let host = host.trim_start_matches('[').trim_end_matches(']');

    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await?
        .filter(|a| family.accepts(a))
        .collect();

    if addrs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            format!("no {} address found for {host}", family.network()),
        ));
    }
    Ok(addrs)
}

/// Applies the interface and firewall mark options to a socket.
fn apply_socket_options(
    socket: &TcpSocket,
    opts: &BindOptions,
    addr: &SocketAddr,
) -> io::Result<()> {
    let sock = SockRef::from(socket);

    sock.set_tcp_keepalive(&TcpKeepalive::new().with_time(KEEPALIVE))?;

    if let Some(iface) = &opts.interface {
        bind_to_interface(&sock, iface, addr)?;
    }

    if opts.fwmark > 0 {
        set_fwmark(&sock, opts.fwmark)?;
    }

    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "android", target_os = "fuchsia"))]
fn bind_to_interface(sock: &SockRef<'_>, iface: &str, _addr: &SocketAddr) -> io::Result<()> {
    // On Linux SO_BINDTODEVICE really binds the socket to the device, instead of
    // binding to an address that would still be subject to the default routes.
    sock.bind_device(Some(iface.as_bytes()))
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn bind_to_interface(sock: &SockRef<'_>, iface: &str, addr: &SocketAddr) -> io::Result<()> {
    let name = std::ffi::CString::new(iface)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "interface name contains NUL"))?;
    // SAFETY: `name` is a valid NUL-terminated C string for the duration of the call.
    let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
    let index = std::num::NonZeroU32::new(index).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no such interface: {iface}"),
        )
    })?;
    if addr.is_ipv4() {
        sock.bind_device_by_index_v4(Some(index))
    } else {
        sock.bind_device_by_index_v6(Some(index))
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "fuchsia",
    target_os = "macos",
    target_os = "ios"
)))]
fn bind_to_interface(_sock: &SockRef<'_>, _iface: &str, _addr: &SocketAddr) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "cannot bound to interface on this platform",
    ))
}

#[cfg(any(target_os = "linux", target_os = "android", target_os = "fuchsia"))]
fn set_fwmark(sock: &SockRef<'_>, fwmark: u32) -> io::Result<()> {
    sock.set_mark(fwmark)
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "fuchsia")))]
fn set_fwmark(_sock: &SockRef<'_>, _fwmark: u32) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "cannot set a firewall mark on this platform",
    ))
}

/// Opens a single TCP connection to `addr` honouring all bind options.
async fn connect_one(addr: SocketAddr, opts: &BindOptions) -> io::Result<TcpStream> {
    let socket = if addr.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };

    socket.set_nodelay(true)?;
    apply_socket_options(&socket, opts, &addr)?;

    if let Some(src) = opts.source {
        // A source address can only be bound to a socket of the same family.
        if src.is_ipv4() == addr.is_ipv4() {
            socket.bind(SocketAddr::new(src, 0))?;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "source address family does not match destination",
            ));
        }
    }

    socket.connect(addr).await
}

fn timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "connection timed out")
}

/// Tries each address in turn, giving each an equal share of what is left of
/// the budget but no less than `MIN_ATTEMPT`, as Go's `dialSerial` does.
async fn dial_serial<S, F, Fut>(
    addrs: Vec<SocketAddr>,
    connect: F,
    deadline: tokio::time::Instant,
) -> io::Result<S>
where
    F: Fn(SocketAddr) -> Fut,
    Fut: Future<Output = io::Result<S>>,
{
    let mut last_err = None;
    let total = addrs.len();

    for (i, addr) in addrs.into_iter().enumerate() {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            break;
        }
        let remaining = deadline - now;
        let share = remaining / (total - i) as u32;
        let attempt = now + share.max(MIN_ATTEMPT).min(remaining);

        match tokio::time::timeout_at(attempt, connect(addr)).await {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(e)) => last_err = Some(e),
            Err(_) => last_err = Some(timed_out()),
        }
    }

    Err(last_err.unwrap_or_else(timed_out))
}

/// Connects to the first of `addrs` that answers, honouring all bind options.
async fn dial(addrs: Vec<SocketAddr>, opts: BindOptions) -> io::Result<TcpStream> {
    dial_with(addrs, move |addr| {
        let opts = opts.clone();
        async move { connect_one(addr, &opts).await }
    })
    .await
}

/// Why the loop racing the two address families woke up.
enum Wake<S> {
    /// `FALLBACK_DELAY` ran out with the first family still trying.
    DelayElapsed,
    /// The first family finished: it connected, or ran out of addresses.
    Primary(io::Result<S>),
    /// The other family finished likewise.
    Fallback(io::Result<S>),
}

impl<S> Wake<S> {
    /// Whether this is a reason to start the other family: as in Go, only
    /// the delay running out or the first family failing is.
    fn starts_fallback(&self) -> bool {
        matches!(self, Wake::DelayElapsed | Wake::Primary(Err(_)))
    }
}

/// Waits for the racer in `slot`, and forever while there is none.
async fn running<T>(slot: Pin<&mut Option<impl Future<Output = T>>>) -> T {
    match slot.as_pin_mut() {
        Some(racer) => racer.await,
        None => std::future::pending().await,
    }
}

/// Connects to the first address that answers, racing the two families.
///
/// The other family starts after `FALLBACK_DELAY`, or as soon as the first
/// fails, so a family that resolves but does not answer cannot use up the
/// whole budget. This follows Go's `dialParallel`.
async fn dial_with<S, F, Fut>(addrs: Vec<SocketAddr>, connect: F) -> io::Result<S>
where
    F: Fn(SocketAddr) -> Fut + Clone,
    Fut: Future<Output = io::Result<S>>,
{
    let deadline = tokio::time::Instant::now() + CONNECT_TIMEOUT;

    let Some(first) = addrs.first().copied() else {
        return Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "no address to connect to",
        ));
    };
    let (primaries, fallbacks): (Vec<_>, Vec<_>) = addrs
        .into_iter()
        .partition(|a| a.is_ipv4() == first.is_ipv4());

    if fallbacks.is_empty() {
        return dial_serial(primaries, connect, deadline).await;
    }

    // Both racers live in this function and run only while it is polled, so
    // returning cancels whichever did not win, and one that was never polled
    // has not touched the network.
    let mut primary = std::pin::pin!(dial_serial(primaries, connect.clone(), deadline));
    let mut primary_err = None;
    // The other family: its addresses wait in `fallbacks` until it starts,
    // and `fallback` holds its racer for as long as that runs.
    let mut fallbacks = Some(fallbacks);
    let mut fallback = std::pin::pin!(None);
    let mut fallback_delay = std::pin::pin!(tokio::time::sleep(FALLBACK_DELAY));

    loop {
        let wake = tokio::select! {
            // Polled in this order, so a first family that connects in the
            // same instant as the delay runs out wins without the other
            // family being dialled.
            biased;
            result = primary.as_mut(), if primary_err.is_none() => Wake::Primary(result),
            result = running(fallback.as_mut()), if fallback.is_some() => Wake::Fallback(result),
            () = fallback_delay.as_mut(), if fallbacks.is_some() => Wake::DelayElapsed,
            // Nothing is left to wait for: both families have failed.
            else => break,
        };

        let start_fallback = wake.starts_fallback();
        match wake {
            Wake::Primary(Ok(stream)) | Wake::Fallback(Ok(stream)) => return Ok(stream),
            Wake::Primary(Err(e)) => primary_err = Some(e),
            Wake::Fallback(Err(_)) => fallback.set(None),
            Wake::DelayElapsed => {}
        }
        if start_fallback {
            if let Some(addrs) = fallbacks.take() {
                fallback.set(Some(dial_serial(addrs, connect.clone(), deadline)));
            }
        }
    }

    // As in Go, the first family's error is the one reported.
    Err(primary_err.unwrap_or_else(timed_out))
}

/// A `tower` connector that produces socket-bound TCP streams.
#[derive(Clone, Debug)]
pub struct BoundConnector {
    opts: BindOptions,
}

impl BoundConnector {
    pub fn new(opts: BindOptions) -> Self {
        Self { opts }
    }
}

/// The address a request's connection actually reached.
///
/// Attached to the connection so it reaches the response through hyper's
/// connection extras: a hostname can resolve to several addresses, in either
/// family, and a reconnect can land on a different one, so which address a
/// measurement ran against is not derivable from the URL.
#[derive(Clone, Copy, Debug)]
pub struct PeerAddr(pub SocketAddr);

/// A connected stream that remembers which address it reached.
#[derive(Debug)]
pub struct TrackedStream {
    inner: TokioIo<TcpStream>,
    peer: SocketAddr,
}

impl Connection for TrackedStream {
    fn connected(&self) -> Connected {
        Connected::new().extra(PeerAddr(self.peer))
    }
}

impl hyper::rt::Read for TrackedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl hyper::rt::Write for TrackedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }
}

impl tower_service::Service<Uri> for BoundConnector {
    type Response = TrackedStream;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, dst: Uri) -> Self::Future {
        let opts = self.opts.clone();
        Box::pin(async move {
            let host = dst
                .host()
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "URI has no host"))?;
            let port = dst.port_u16().unwrap_or(match dst.scheme_str() {
                Some("https") => 443,
                _ => 80,
            });

            let addrs = resolve(host, port, opts.family).await?;
            let stream = dial(addrs, opts).await?;
            // Ask the socket rather than trusting the address list: a name
            // resolving to several addresses leaves only the socket knowing
            // which one answered.
            let peer = stream.peer_addr()?;
            Ok(TrackedStream {
                inner: TokioIo::new(stream),
                peer,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;
    use tokio::time::Instant;

    /// How an address on the fake network answers a connection attempt.
    #[derive(Clone, Copy, Debug)]
    enum Peer {
        Refuses,
        Accepts,
        AcceptsAfter(Duration),
        Hangs,
    }

    /// Stands in for the network in the racing tests, which run on a paused
    /// clock: records which address each attempt went to and when it started.
    #[derive(Clone)]
    struct FakeNet {
        peers: Arc<Vec<(SocketAddr, Peer)>>,
        attempts: Arc<Mutex<Vec<(SocketAddr, Duration)>>>,
        start: Instant,
    }

    impl FakeNet {
        fn new(peers: &[(&str, Peer)]) -> Self {
            Self {
                peers: Arc::new(peers.iter().map(|&(a, p)| (addr(a), p)).collect()),
                attempts: Arc::default(),
                start: Instant::now(),
            }
        }

        fn addrs(&self) -> Vec<SocketAddr> {
            self.peers.iter().map(|&(a, _)| a).collect()
        }

        fn connect(
            &self,
            addr: SocketAddr,
        ) -> impl Future<Output = io::Result<SocketAddr>> + Send + 'static {
            self.attempts
                .lock()
                .unwrap()
                .push((addr, self.start.elapsed()));
            let (_, peer) = *self
                .peers
                .iter()
                .find(|&&(a, _)| a == addr)
                .expect("address is not on the fake network");
            async move {
                match peer {
                    Peer::Refuses => Err(io::Error::new(
                        io::ErrorKind::ConnectionRefused,
                        format!("{addr} refused"),
                    )),
                    Peer::Accepts => Ok(addr),
                    Peer::AcceptsAfter(delay) => {
                        tokio::time::sleep(delay).await;
                        Ok(addr)
                    }
                    Peer::Hangs => std::future::pending().await,
                }
            }
        }

        /// Dials all of the network's addresses in order, returning the
        /// result and how long it took.
        async fn dial(&self) -> (io::Result<SocketAddr>, Duration) {
            let net = self.clone();
            let result = dial_with(self.addrs(), move |a| net.connect(a)).await;
            (result, self.start.elapsed())
        }

        fn attempts(&self) -> Vec<(SocketAddr, Duration)> {
            self.attempts.lock().unwrap().clone()
        }
    }

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    const V6: &str = "[2001:db8::1]:80";
    const V4: &str = "192.0.2.1:80";

    /// A first family that does not answer holds the other back only for
    /// `FALLBACK_DELAY`, not for its whole share of the budget.
    #[tokio::test(start_paused = true)]
    async fn a_hanging_first_family_starts_the_other_after_the_fallback_delay() {
        let net = FakeNet::new(&[(V6, Peer::Hangs), (V4, Peer::Accepts)]);

        let (result, elapsed) = net.dial().await;

        assert_eq!(result.unwrap(), addr(V4));
        assert_eq!(
            net.attempts(),
            [(addr(V6), Duration::ZERO), (addr(V4), FALLBACK_DELAY)]
        );
        assert_eq!(elapsed, FALLBACK_DELAY);
    }

    /// A first family that fails gives way to the other at once, rather than
    /// after the rest of `FALLBACK_DELAY`.
    #[tokio::test(start_paused = true)]
    async fn a_refused_first_family_starts_the_other_at_once() {
        let net = FakeNet::new(&[(V6, Peer::Refuses), (V4, Peer::Accepts)]);

        let (result, elapsed) = net.dial().await;

        assert_eq!(result.unwrap(), addr(V4));
        assert_eq!(
            net.attempts(),
            [(addr(V6), Duration::ZERO), (addr(V4), Duration::ZERO)]
        );
        assert_eq!(elapsed, Duration::ZERO);
    }

    /// A first family that connects within `FALLBACK_DELAY` is used alone.
    #[tokio::test(start_paused = true)]
    async fn a_first_family_that_answers_in_time_never_starts_the_other() {
        let delay = Duration::from_millis(100);
        let net = FakeNet::new(&[(V6, Peer::AcceptsAfter(delay)), (V4, Peer::Accepts)]);

        let (result, elapsed) = net.dial().await;
        assert_eq!(result.unwrap(), addr(V6));
        assert_eq!(elapsed, delay);

        // Well past the point where the other family would have started.
        tokio::time::sleep(FALLBACK_DELAY * 2).await;
        assert_eq!(net.attempts(), [(addr(V6), Duration::ZERO)]);
    }

    /// A first family that connects in the very instant `FALLBACK_DELAY` runs
    /// out is used alone too: the delay does not get to start the other.
    #[tokio::test(start_paused = true)]
    async fn a_first_family_that_answers_as_the_delay_ends_never_starts_the_other() {
        let net = FakeNet::new(&[
            (V6, Peer::AcceptsAfter(FALLBACK_DELAY)),
            (V4, Peer::Accepts),
        ]);

        let (result, elapsed) = net.dial().await;
        assert_eq!(result.unwrap(), addr(V6));
        assert_eq!(elapsed, FALLBACK_DELAY);

        tokio::time::sleep(FALLBACK_DELAY * 2).await;
        assert_eq!(net.attempts(), [(addr(V6), Duration::ZERO)]);
    }

    /// Only the delay running out and the first family failing start the
    /// other family; no other wake-up of the racing loop does.
    #[test]
    fn only_the_delay_and_a_failed_first_family_start_the_other() {
        let failed = || Err(timed_out());

        assert!(Wake::<()>::DelayElapsed.starts_fallback());
        assert!(Wake::<()>::Primary(failed()).starts_fallback());
        assert!(!Wake::Primary(Ok(())).starts_fallback());
        assert!(!Wake::Fallback(Ok(())).starts_fallback());
        assert!(!Wake::<()>::Fallback(failed()).starts_fallback());
    }

    /// When nothing answers, both families give up together at the one
    /// `CONNECT_TIMEOUT` deadline, each address having had its share of it.
    #[tokio::test(start_paused = true)]
    async fn unanswered_families_fail_together_at_the_shared_deadline() {
        let net = FakeNet::new(&[
            ("[2001:db8::1]:80", Peer::Hangs),
            ("192.0.2.1:80", Peer::Hangs),
            ("[2001:db8::2]:80", Peer::Hangs),
            ("192.0.2.2:80", Peer::Hangs),
        ]);

        let (result, elapsed) = net.dial().await;

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert_eq!(
            net.attempts(),
            [
                (addr("[2001:db8::1]:80"), Duration::ZERO),
                (addr("192.0.2.1:80"), FALLBACK_DELAY),
                (addr("[2001:db8::2]:80"), CONNECT_TIMEOUT / 2),
                (
                    addr("192.0.2.2:80"),
                    FALLBACK_DELAY + (CONNECT_TIMEOUT - FALLBACK_DELAY) / 2
                ),
            ]
        );
        assert_eq!(elapsed, CONNECT_TIMEOUT);
    }

    /// Within one family the addresses are tried one after another: past a
    /// refusal at once, past one that does not answer when its share ends.
    #[tokio::test(start_paused = true)]
    async fn one_family_is_tried_address_by_address() {
        let net = FakeNet::new(&[
            ("192.0.2.1:80", Peer::Refuses),
            ("192.0.2.2:80", Peer::Hangs),
            ("192.0.2.3:80", Peer::Accepts),
        ]);

        let (result, elapsed) = net.dial().await;

        assert_eq!(result.unwrap(), addr("192.0.2.3:80"));
        // The second address is one of two left, so it gets half the budget.
        assert_eq!(
            net.attempts(),
            [
                (addr("192.0.2.1:80"), Duration::ZERO),
                (addr("192.0.2.2:80"), Duration::ZERO),
                (addr("192.0.2.3:80"), CONNECT_TIMEOUT / 2),
            ]
        );
        assert_eq!(elapsed, CONNECT_TIMEOUT / 2);
    }

    /// The connect timeout is a budget for reaching the host, not a grant to
    /// each address in turn, and no address gets less than `MIN_ATTEMPT`
    /// unless the deadline is nearer than that.
    #[tokio::test(start_paused = true)]
    async fn addresses_share_one_budget_rather_than_each_getting_their_own() {
        let net = FakeNet::new(&[("192.0.2.1:80", Peer::Hangs), ("192.0.2.2:80", Peer::Hangs)]);
        let budget = Duration::from_secs(3);

        let n = net.clone();
        let result = dial_serial(net.addrs(), move |a| n.connect(a), net.start + budget).await;

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
        // Half of 3 s is below MIN_ATTEMPT, so the first address gets 2 s.
        assert_eq!(
            net.attempts(),
            [
                (addr("192.0.2.1:80"), Duration::ZERO),
                (addr("192.0.2.2:80"), MIN_ATTEMPT),
            ]
        );
        assert_eq!(net.start.elapsed(), budget);
    }

    /// The first family's error is reported even when the other fails sooner.
    #[tokio::test(start_paused = true)]
    async fn the_first_familys_error_is_reported_even_if_the_other_fails_sooner() {
        let net = FakeNet::new(&[(V6, Peer::Hangs), (V4, Peer::Refuses)]);

        let (result, elapsed) = net.dial().await;

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert_eq!(elapsed, CONNECT_TIMEOUT);
    }

    /// With one family only there is nothing to race, and every address still
    /// gets a share of the budget rather than one address taking all of it.
    #[tokio::test]
    async fn a_single_family_falls_through_to_the_next_address() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let working = listener.local_addr().unwrap();
        // Nothing listens here: the connection is refused at once.
        let refused: SocketAddr = "127.0.0.1:1".parse().unwrap();

        let stream = dial(vec![refused, working], BindOptions::default())
            .await
            .expect("the second address should have been tried");
        assert_eq!(stream.peer_addr().unwrap(), working);
    }

    /// A listener on `::1`, or `None` where IPv6 loopback is unavailable (such
    /// as a container with IPv6 disabled), for tests that need both families.
    async fn ipv6_loopback_listener() -> Option<TcpListener> {
        match TcpListener::bind("[::1]:0").await {
            Ok(listener) => Some(listener),
            Err(e) => {
                eprintln!("skipped: cannot listen on [::1]: {e}");
                None
            }
        }
    }

    // The family filter tests use address literals, which resolve without a
    // DNS lookup or a route of either family.

    /// `--ipv4` keeps IPv4 addresses and leaves nothing to dial for IPv6 ones.
    #[tokio::test]
    async fn ipv4_only_drops_ipv6_addresses() {
        let found = resolve("127.0.0.1", 80, IpFamily::V4).await.unwrap();
        assert_eq!(found, [addr("127.0.0.1:80")]);

        let err = resolve("[::1]", 80, IpFamily::V4).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AddrNotAvailable);
        assert_eq!(err.to_string(), "no ip4 address found for ::1");
    }

    /// `--ipv6` keeps IPv6 addresses and leaves nothing to dial for IPv4 ones.
    #[tokio::test]
    async fn ipv6_only_drops_ipv4_addresses() {
        let found = resolve("[::1]", 80, IpFamily::V6).await.unwrap();
        assert_eq!(found, [addr("[::1]:80")]);

        let err = resolve("127.0.0.1", 80, IpFamily::V6).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AddrNotAvailable);
        assert_eq!(err.to_string(), "no ip6 address found for 127.0.0.1");
    }

    /// `--source` binds the socket to that address. Linux routes all of
    /// 127.0.0.0/8 to loopback, so there 127.0.0.2 shows the bind took effect
    /// rather than matching the address the kernel would pick anyway.
    #[tokio::test]
    async fn the_socket_is_bound_to_the_source_address() {
        let source: IpAddr = if cfg!(target_os = "linux") {
            "127.0.0.2"
        } else {
            "127.0.0.1"
        }
        .parse()
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let opts = BindOptions {
            source: Some(source),
            ..Default::default()
        };

        let stream = connect_one(listener.local_addr().unwrap(), &opts)
            .await
            .unwrap();

        assert_eq!(stream.local_addr().unwrap().ip(), source);
        let (_, peer) = listener.accept().await.unwrap();
        assert_eq!(peer.ip(), source);
    }

    /// A source address cannot be bound to a socket of the other family.
    #[tokio::test]
    async fn a_source_of_the_other_family_is_refused() {
        let opts = BindOptions {
            source: Some("::1".parse().unwrap()),
            ..Default::default()
        };

        let err = connect_one(addr("127.0.0.1:1"), &opts)
            .await
            .expect_err("an IPv6 source cannot reach an IPv4 address");

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(
            err.to_string(),
            "source address family does not match destination"
        );
    }

    /// The same on real sockets: a closed IPv4 port gives way to `::1` at once.
    #[tokio::test]
    #[cfg_attr(
        windows,
        ignore = "Windows retries a refused connection for about two seconds, so it is not refused at once"
    )]
    async fn a_refused_first_family_starts_the_other_at_once_on_real_sockets() {
        let Some(v6) = ipv6_loopback_listener().await else {
            return;
        };
        let v6_addr = v6.local_addr().unwrap();
        // The listener is dropped straight away, so connections are refused.
        let refused = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap();

        let started = std::time::Instant::now();
        let stream = dial(vec![refused, v6_addr], BindOptions::default())
            .await
            .unwrap();
        let elapsed = started.elapsed();

        assert_eq!(stream.peer_addr().unwrap(), v6_addr);
        assert!(
            elapsed < FALLBACK_DELAY,
            "took {elapsed:?}, so the other family waited out the fallback delay"
        );
    }

    /// Once the first family connects, the other family is never dialled.
    #[tokio::test]
    async fn a_first_family_that_connects_leaves_the_other_undialled() {
        let Some(v6) = ipv6_loopback_listener().await else {
            return;
        };
        let v4 = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let v4_addr = v4.local_addr().unwrap();

        let stream = dial(
            vec![v4_addr, v6.local_addr().unwrap()],
            BindOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(stream.peer_addr().unwrap(), v4_addr);

        let reached = tokio::time::timeout(FALLBACK_DELAY * 2, v6.accept()).await;
        assert!(
            reached.is_err(),
            "the other family was dialled: {reached:?}"
        );
    }
}
