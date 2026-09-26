//! Counts bytes transferred during the download and upload tests.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::http::WriteMeter;

/// Tracks total bytes transferred and derives the average transfer rate.
///
/// The total is a mutex rather than an `AtomicU64` because 32-bit targets such
/// as the PowerPC in Turris 1.x routers have no 64-bit atomics. A counter
/// bumped once per received frame is nowhere near contended enough for the
/// difference to matter, and 32 bits would overflow within a single test.
#[derive(Debug)]
pub struct BytesCounter {
    total: Mutex<u64>,
    start: Mutex<Option<Instant>>,
    mebi: bool,
    upload_size: usize,
    /// The ceiling on the total, for the upload phase; see `set_wire`.
    wire: Option<Wire>,
}

/// What a client's connections had written when a phase started, and the meter
/// to ask again.
#[derive(Debug)]
struct Wire {
    meter: Arc<WriteMeter>,
    start: u64,
}

impl BytesCounter {
    pub fn new() -> Self {
        Self {
            total: Mutex::new(0),
            start: Mutex::new(None),
            mebi: false,
            upload_size: 0,
            wire: None,
        }
    }

    /// Uses 1024 rather than 1000 as the base for derived units.
    pub fn set_mebi(&mut self, mebi: bool) {
        self.mebi = mebi;
    }

    /// Sets the payload size per upload request, given in KiB.
    pub fn set_upload_size(&mut self, upload_size_kib: usize) {
        self.upload_size = upload_size_kib * 1024;
    }

    pub fn upload_size(&self) -> usize {
        self.upload_size
    }

    /// Caps the total at what `meter`'s connections write from now on.
    ///
    /// For the upload phase. Its total is body bytes counted as hyper takes
    /// each frame, and hyper takes frames ahead of writing them, so without a
    /// ceiling the total includes whatever is still queued -- hundreds of
    /// kilobytes per connection, which is a quarter to a half of what a 1 Mbit
    /// uplink carries in a whole test.
    ///
    /// The meter counts request heads and chunk framing along with body bytes,
    /// so as a measure of body bytes written it is high by the overhead: a
    /// head is 119 bytes for the request this client sends, and chunk framing
    /// is 8 bytes per 16 KiB frame. That is the whole error this leaves -- the
    /// total lands between what the connections wrote of the body and that
    /// plus the overhead -- and a request the peer took whole is counted
    /// exactly, its body bytes alone being fewer than head and body together.
    pub fn set_wire(&mut self, meter: Arc<WriteMeter>) {
        let start = meter.written();
        self.wire = Some(Wire { meter, start });
    }

    /// Starts the clock used for the average.
    pub fn start(&self) {
        *self.start.lock().unwrap() = Some(Instant::now());
    }

    /// Records `n` transferred bytes.
    pub fn add(&self, n: u64) {
        *self.total.lock().unwrap() += n;
    }

    /// Total bytes read or written, at most what the wire ceiling allows.
    pub fn total(&self) -> u64 {
        let counted = *self.total.lock().unwrap();
        match &self.wire {
            Some(wire) => counted.min(wire.meter.written().saturating_sub(wire.start)),
            None => counted,
        }
    }

    fn elapsed_secs(&self) -> f64 {
        match *self.start.lock().unwrap() {
            Some(start) => start.elapsed().as_secs_f64(),
            None => 0.0,
        }
    }

    /// Average bytes per second.
    pub fn avg_bytes(&self) -> f64 {
        let secs = self.elapsed_secs();
        if secs <= 0.0 {
            return 0.0;
        }
        self.total() as f64 / secs
    }

    /// Average megabits per second.
    pub fn avg_mbps(&self) -> f64 {
        let base = if self.mebi { 131072.0 } else { 125000.0 };
        self.avg_bytes() / base
    }

    /// Average rate rendered in bytes/kilobytes/megabytes/gigabytes per second
    /// (or the binary equivalents when `mebi` is set).
    pub fn avg_humanize(&self) -> String {
        let val = self.avg_bytes();
        let base: f64 = if self.mebi { 1024.0 } else { 1000.0 };

        if val < base {
            format!("{val:.2} bytes/s")
        } else if val / base < base {
            format!("{:.2} KB/s", val / base)
        } else if val / base / base < base {
            format!("{:.2} MB/s", val / base / base)
        } else {
            format!("{:.2} GB/s", val / base / base / base)
        }
    }
}

impl Default for BytesCounter {
    fn default() -> Self {
        Self::new()
    }
}

/// Returns `length` bytes of random data.
///
/// Uses the thread RNG rather than the OS entropy source: it is dramatically
/// faster for bulk data and the payload only needs to be incompressible.
pub fn random_data(length: usize) -> Vec<u8> {
    let mut data = vec![0u8; length];
    rand::fill(&mut data[..]);
    data
}
