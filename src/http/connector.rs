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
use std::sync::{Arc, Mutex};
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
async fn connect_one(
    addr: SocketAddr,
    opts: &BindOptions,
    attempt_deadline: tokio::time::Instant,
) -> io::Result<TcpStream> {
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

    tokio::time::timeout_at(attempt_deadline, socket.connect(addr))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "connection timed out"))?
}

/// Tries each address in turn, splitting what is left of the budget between
/// the addresses still to try, as Go's `dialSerial` does.
async fn dial_serial(
    addrs: Vec<SocketAddr>,
    opts: BindOptions,
    deadline: tokio::time::Instant,
) -> io::Result<TcpStream> {
    let mut last_err = None;
    let total = addrs.len();

    for (i, addr) in addrs.into_iter().enumerate() {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            break;
        }
        // An address that hangs must not eat the whole budget and leave the
        // others untried, but a share too small to complete a handshake is no
        // use either.
        let remaining = deadline - now;
        let share = remaining / (total - i) as u32;
        let attempt = now + share.max(MIN_ATTEMPT).min(remaining);

        match connect_one(addr, &opts, attempt).await {
            Ok(stream) => return Ok(stream),
            Err(e) => last_err = Some(e),
        }
    }

    Err(last_err.unwrap_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "connection timed out")))
}

/// Connects to the first address that answers, trying both families.
///
/// A name commonly resolves to an AAAA record first, and on a network where
/// IPv6 resolves but blackholes -- a stale delegated prefix, filtered ICMPv6
/// -- dialling strictly in order means the per-request timeout kills the
/// request before any IPv4 address is reached, and the client cannot run at
/// all. So the second family starts alongside the first after a short delay
/// and the first connection to answer wins, as Go's `dialParallel` does.
async fn dial(addrs: Vec<SocketAddr>, opts: BindOptions) -> io::Result<TcpStream> {
    let deadline = tokio::time::Instant::now() + CONNECT_TIMEOUT;

    let Some(first) = addrs.first().copied() else {
        return Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "no address to connect to",
        ));
    };
    let (primary, fallback): (Vec<_>, Vec<_>) = addrs
        .into_iter()
        .partition(|a| a.is_ipv4() == first.is_ipv4());

    if fallback.is_empty() {
        return dial_serial(primary, opts, deadline).await;
    }

    // Dropping the set at the end of this function cancels whichever attempt
    // did not win.
    let mut racers: tokio::task::JoinSet<(bool, io::Result<TcpStream>)> =
        tokio::task::JoinSet::new();
    let fallback_opts = opts.clone();
    racers.spawn(async move { (true, dial_serial(primary, opts, deadline).await) });
    racers.spawn(async move {
        tokio::time::sleep(FALLBACK_DELAY).await;
        (false, dial_serial(fallback, fallback_opts, deadline).await)
    });

    let mut primary_err = None;
    let mut other_err = None;
    while let Some(joined) = racers.join_next().await {
        match joined {
            Ok((_, Ok(stream))) => return Ok(stream),
            // Report the first family's failure: it is the one the resolver
            // put first, and the one the user is most likely asking about.
            Ok((true, Err(e))) => primary_err = Some(e),
            Ok((false, Err(e))) => other_err = Some(e),
            Err(e) => other_err = Some(io::Error::other(e)),
        }
    }

    Err(primary_err.or(other_err).unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::AddrNotAvailable, "no address to connect to")
    }))
}

/// A `tower` connector that produces socket-bound TCP streams.
#[derive(Clone, Debug)]
pub struct BoundConnector {
    opts: BindOptions,
    meter: WriteMeter,
}

impl BoundConnector {
    pub fn new(opts: BindOptions, meter: WriteMeter) -> Self {
        Self { opts, meter }
    }
}

/// Something socket writes are reported to.
pub trait ByteSink: Send + Sync + std::fmt::Debug {
    fn add_written(&self, n: u64);
}

/// The counter that socket writes are added to, while one is installed.
///
/// The upload figure has to be measured where the kernel takes the bytes, not
/// where hyper asks the body for them: hyper reads ahead to fill its write
/// queue, several hundred kilobytes per connection, and the end of the test
/// discards whatever is still queued after it has already been counted. On a
/// slow uplink that queue is a large fraction of everything the test moved.
///
/// What this counts is bytes the kernel accepted, not bytes the peer
/// acknowledged: a socket buffer's worth can still be in flight when the
/// window closes, and over HTTPS these are ciphertext, so TLS record overhead
/// and request heads are included. That is the same thing the Go client's
/// TeeReader counts, and it is what client-side throughput means; a figure
/// bounded by acknowledgements would need the peer to report back.
///
/// A connection outlives any one phase and is shared through hyper's pool, so
/// the counter cannot be captured when the connection is made; it is installed
/// for the duration of the upload phase and taken away afterwards.
#[derive(Clone, Default, Debug)]
pub struct WriteMeter(Arc<Mutex<Option<Arc<dyn ByteSink>>>>);

impl WriteMeter {
    /// Starts counting socket writes into `sink`.
    pub fn install(&self, sink: Arc<dyn ByteSink>) {
        *self.0.lock().unwrap() = Some(sink);
    }

    /// Stops counting.
    pub fn clear(&self) {
        *self.0.lock().unwrap() = None;
    }

    fn add(&self, n: usize) {
        if let Some(sink) = self.0.lock().unwrap().as_ref() {
            sink.add_written(n as u64);
        }
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
    meter: WriteMeter,
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
        let written = Pin::new(&mut self.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = &written {
            self.meter.add(*n);
        }
        written
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
        let written = Pin::new(&mut self.inner).poll_write_vectored(cx, bufs);
        if let Poll::Ready(Ok(n)) = &written {
            self.meter.add(*n);
        }
        written
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
        let meter = self.meter.clone();
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
                meter,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A name that resolves to an unreachable address of one family and a
    /// working address of the other must still connect, and quickly. Dialling
    /// strictly in order left the working address untried until the first had
    /// used up its own 30 s, by which point the per-request timeout had
    /// already failed the request.
    #[tokio::test]
    async fn the_other_family_is_tried_alongside_the_first() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let working = listener.local_addr().unwrap();
        // Reserved for documentation, so it is routed nowhere.
        let dead: SocketAddr = "[2001:db8::1]:80".parse().unwrap();

        let started = std::time::Instant::now();
        let stream = dial(vec![dead, working], BindOptions::default())
            .await
            .expect("the reachable address should have been used");

        assert_eq!(stream.peer_addr().unwrap(), working);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?}, so the families were not raced",
            started.elapsed()
        );
    }

    /// The connect timeout is a budget for reaching the host, not a grant to
    /// each address in turn. Dialling strictly in order gave every address its
    /// own full timeout, so a name with two dead addresses took twice as long
    /// as the budget allows.
    #[tokio::test]
    async fn addresses_share_one_budget_rather_than_each_getting_their_own() {
        // Reserved for documentation, so both are routed nowhere and hang.
        let dead: Vec<SocketAddr> = vec![
            "192.0.2.1:80".parse().unwrap(),
            "192.0.2.2:80".parse().unwrap(),
        ];
        let budget = Duration::from_secs(3);
        let deadline = tokio::time::Instant::now() + budget;

        let started = std::time::Instant::now();
        dial_serial(dead, BindOptions::default(), deadline)
            .await
            .expect_err("nothing is listening on either address");
        let elapsed = started.elapsed();

        assert!(
            elapsed < budget + Duration::from_secs(1),
            "took {elapsed:?} for a {budget:?} budget, so each address took its own"
        );
    }

    /// Socket writes are only counted while a phase has installed a counter,
    /// so the ping and download phases cannot leak into the upload total.
    #[test]
    fn the_write_meter_counts_only_while_installed() {
        #[derive(Debug, Default)]
        struct Sink(Mutex<u64>);
        impl ByteSink for Sink {
            fn add_written(&self, n: u64) {
                *self.0.lock().unwrap() += n;
            }
        }

        let meter = WriteMeter::default();
        meter.add(100);

        let sink = Arc::new(Sink::default());
        meter.install(sink.clone());
        meter.add(64);
        meter.add(36);
        meter.clear();
        meter.add(1000);

        assert_eq!(
            *sink.0.lock().unwrap(),
            100,
            "only the installed window counts"
        );
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
}
