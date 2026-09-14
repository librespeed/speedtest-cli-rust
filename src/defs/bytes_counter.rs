//! Counts bytes transferred during the download and upload tests.

use std::sync::Mutex;
use std::time::Instant;

use rand::RngCore;

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
}

impl crate::http::connector::ByteSink for BytesCounter {
    fn add_written(&self, n: u64) {
        self.add(n);
    }
}

impl BytesCounter {
    pub fn new() -> Self {
        Self {
            total: Mutex::new(0),
            start: Mutex::new(None),
            mebi: false,
            upload_size: 0,
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

    /// Starts the clock used for the average.
    pub fn start(&self) {
        *self.start.lock().unwrap() = Some(Instant::now());
    }

    /// Records `n` transferred bytes.
    pub fn add(&self, n: u64) {
        *self.total.lock().unwrap() += n;
    }

    /// Total bytes read or written.
    pub fn total(&self) -> u64 {
        *self.total.lock().unwrap()
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
    rand::thread_rng().fill_bytes(&mut data);
    data
}
