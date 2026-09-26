//! Counting the bytes a client's connections hand to the transport.
//!
//! The upload total is body bytes, counted as hyper takes each frame, and a
//! frame taken is not yet written: hyper queues what it cannot write at once.
//! Counting the queue as sent overstated a slow uplink by a quarter to a half,
//! and the first fix for it shrank the queue -- which bought the accounting at
//! the price of a write syscall every 16 KiB.
//!
//! This is the other half: the connections report what they write, and the
//! upload total is never allowed past it (see `BytesCounter::set_wire`). What
//! the meter counts is one layer below what hyper writes and one above the
//! socket -- plaintext, so it is comparable with body bytes, and returned by
//! the transport, so it excludes anything hyper is still holding. It counts
//! request heads and chunk framing as well as body bytes, which is why it is
//! a ceiling rather than the total itself: it can only ever leave the total a
//! few hundred bytes per request too high, where the queue left it hundreds of
//! kilobytes per connection too high.
//!
//! Over TLS the bytes are counted as the TLS session accepts them, so whatever
//! the session has buffered without writing it to the socket is counted too:
//! with rustls, its plaintext and outgoing-record buffers, 64 KiB each by
//! default. Capping hyper's own buffers never reached those, so this is no
//! looser there than what it replaces.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use http::Uri;
use hyper_util::client::legacy::connect::{Connected, Connection};

/// How many bytes a client's connections have handed to the transport.
#[derive(Debug, Default)]
pub struct WriteMeter {
    /// A mutex rather than an `AtomicU64` for the reason `BytesCounter` uses
    /// one: 32-bit targets such as the PowerPC in Turris 1.x routers have no
    /// 64-bit atomics. This is bumped once per write, not once per byte.
    written: Mutex<u64>,
}

impl WriteMeter {
    pub fn new() -> Self {
        Self::default()
    }

    fn add(&self, n: usize) {
        *self.written.lock().unwrap() += n as u64;
    }

    /// Bytes written since the client was built.
    pub fn written(&self) -> u64 {
        *self.written.lock().unwrap()
    }
}

/// A connector whose streams report what they write to `meter`.
#[derive(Clone, Debug)]
pub struct MeteredConnector<C> {
    inner: C,
    meter: Arc<WriteMeter>,
}

impl<C> MeteredConnector<C> {
    pub fn new(inner: C, meter: Arc<WriteMeter>) -> Self {
        Self { inner, meter }
    }
}

impl<C> tower_service::Service<Uri> for MeteredConnector<C>
where
    C: tower_service::Service<Uri>,
    C::Response: Send + 'static,
    C::Future: Send + 'static,
    C::Error: Send + 'static,
{
    type Response = MeteredStream<C::Response>;
    type Error = C::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, dst: Uri) -> Self::Future {
        let meter = self.meter.clone();
        let connect = self.inner.call(dst);
        Box::pin(async move {
            Ok(MeteredStream {
                inner: connect.await?,
                meter,
            })
        })
    }
}

/// A connected stream that counts the bytes it accepts for writing.
#[derive(Debug)]
pub struct MeteredStream<S> {
    inner: S,
    meter: Arc<WriteMeter>,
}

impl<S: Connection> Connection for MeteredStream<S> {
    fn connected(&self) -> Connected {
        self.inner.connected()
    }
}

impl<S: hyper::rt::Read + Unpin> hyper::rt::Read for MeteredStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: hyper::rt::Write + Unpin> hyper::rt::Write for MeteredStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let written = Pin::new(&mut self.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = &written {
            self.meter.add(*n);
        }
        written
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    /// Forwarded, and not merely for tidiness: a stream that says it cannot
    /// take a vector of buffers makes hyper copy every byte of every body into
    /// a buffer of its own before writing it, which on the upload path is the
    /// whole payload, over and over.
    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        let written = Pin::new(&mut self.inner).poll_write_vectored(cx, bufs);
        if let Poll::Ready(Ok(n)) = &written {
            self.meter.add(*n);
        }
        written
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::IoSlice;

    use hyper::rt::Write as _;

    /// A stream that takes at most `take` bytes of any write.
    struct Partial {
        take: usize,
        vectored: bool,
    }

    impl hyper::rt::Read for Partial {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: hyper::rt::ReadBufCursor<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Pending
        }
    }

    impl hyper::rt::Write for Partial {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Ok(buf.len().min(self.take)))
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn is_write_vectored(&self) -> bool {
            self.vectored
        }

        fn poll_write_vectored(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            bufs: &[IoSlice<'_>],
        ) -> Poll<std::io::Result<usize>> {
            let total: usize = bufs.iter().map(|b| b.len()).sum();
            Poll::Ready(Ok(total.min(self.take)))
        }
    }

    fn noop_context() -> Context<'static> {
        Context::from_waker(std::task::Waker::noop())
    }

    /// What the meter records is what the stream took, not what was offered:
    /// a short write leaves the rest for hyper to write later, and counting it
    /// now is counting a byte that is still in a buffer.
    #[test]
    fn a_short_write_counts_only_what_was_taken() {
        let meter = Arc::new(WriteMeter::new());
        let mut stream = MeteredStream {
            inner: Partial {
                take: 10,
                vectored: false,
            },
            meter: meter.clone(),
        };
        let mut cx = noop_context();

        let n = Pin::new(&mut stream).poll_write(&mut cx, &[0u8; 100]);
        assert!(matches!(n, Poll::Ready(Ok(10))));
        let n = Pin::new(&mut stream).poll_write_vectored(&mut cx, &[IoSlice::new(&[0u8; 100])]);
        assert!(matches!(n, Poll::Ready(Ok(10))));
        assert_eq!(meter.written(), 20, "the meter counted bytes not taken");
    }

    /// See `is_write_vectored` above: getting this wrong costs a copy of every
    /// uploaded byte and would not fail any other test.
    #[test]
    fn vectored_writes_are_reported_as_the_inner_stream_reports_them() {
        for vectored in [false, true] {
            let stream = MeteredStream {
                inner: Partial { take: 1, vectored },
                meter: Arc::new(WriteMeter::new()),
            };
            assert_eq!(
                hyper::rt::Write::is_write_vectored(&stream),
                vectored,
                "the metered stream does not report the inner stream's vectored writes"
            );
        }
    }
}
