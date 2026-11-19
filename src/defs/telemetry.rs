//! Telemetry configuration and the log shipped in the `log` telemetry field.

use std::sync::Mutex;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::util::path_join;

pub const LEVEL_DISABLED: &str = "disabled";
pub const LEVEL_BASIC: &str = "basic";
pub const LEVEL_FULL: &str = "full";
pub const LEVEL_DEBUG: &str = "debug";

pub const LEVELS: [&str; 4] = [LEVEL_DISABLED, LEVEL_BASIC, LEVEL_FULL, LEVEL_DEBUG];

/// Collects the log lines sent as the `log` telemetry field.
#[derive(Debug, Default)]
pub struct TelemetryLog {
    level: Mutex<i32>,
    content: Mutex<Vec<String>>,
}

impl TelemetryLog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_level(&self, level: i32) {
        *self.level.lock().unwrap() = level;
    }

    fn level(&self) -> i32 {
        *self.level.lock().unwrap()
    }

    fn push(&self, prefix: &str, msg: &str) {
        let stamp = log_stamp(crate::report::local_now());
        self.content
            .lock()
            .unwrap()
            .push(format!("{stamp}: {prefix}{msg}"));
    }

    /// Logs when the level is at least "full".
    pub fn logf(&self, msg: impl std::fmt::Display) {
        if self.level() >= 2 {
            self.push("", &msg.to_string());
        }
    }

    /// Logs with a WARN prefix when the level is at least "full".
    ///
    /// Part of the telemetry log API for parity with the Go implementation,
    /// which likewise has no current call site.
    #[allow(dead_code)]
    pub fn warnf(&self, msg: impl std::fmt::Display) {
        if self.level() >= 2 {
            self.push("WARN: ", &msg.to_string());
        }
    }

    /// Logs when the level is at least "debug".
    ///
    /// Part of the telemetry log API for parity with the Go implementation,
    /// which likewise has no current call site.
    #[allow(dead_code)]
    pub fn verbosef(&self, msg: impl std::fmt::Display) {
        if self.level() >= 3 {
            self.push("", &msg.to_string());
        }
    }

    /// The accumulated log as a single string.
    pub fn contents(&self) -> String {
        self.content.lock().unwrap().join("\n")
    }
}

/// When a log line was written: close to Go's `time.Time.String()` layout,
/// without the zone abbreviation and the monotonic clock reading Go adds. The
/// fraction keeps 3, 6 or 9 digits and the offset is rounded to the minute, as
/// earlier releases wrote them.
fn log_stamp(t: time::OffsetDateTime) -> String {
    let nanos = t.nanosecond();
    let fraction = if nanos == 0 {
        String::new()
    } else if nanos.is_multiple_of(1_000_000) {
        format!(".{:03}", nanos / 1_000_000)
    } else if nanos.is_multiple_of(1_000) {
        format!(".{:06}", nanos / 1_000)
    } else {
        format!(".{nanos:09}")
    };
    let offset = t.offset().whole_seconds();
    let (sign, offset) = if offset < 0 {
        ('-', -offset)
    } else {
        ('+', offset)
    };
    let minutes = (offset + 30) / 60;
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}{fraction} {sign}{:02}{:02}",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        minutes / 60,
        minutes % 60
    )
}

/// The `extra` field of the telemetry payload.
#[derive(Debug, Default, Serialize)]
pub struct TelemetryExtra {
    #[serde(rename = "server")]
    pub server_name: String,
    #[serde(rename = "extra", skip_serializing_if = "String::is_empty")]
    pub extra: String,
}

/// Telemetry server configuration, also the shape of `--telemetry-json`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct TelemetryServer {
    #[serde(rename = "telemetryLevel", default)]
    pub level: String,
    #[serde(default)]
    pub server: String,
    #[serde(default)]
    pub path: String,
    #[serde(rename = "shareURL", default)]
    pub share: String,
}

impl TelemetryServer {
    /// Translates the level string to its numeric value.
    pub fn get_level(&self) -> i32 {
        match self.level.as_str() {
            LEVEL_BASIC => 1,
            LEVEL_FULL => 2,
            LEVEL_DEBUG => 3,
            _ => 0,
        }
    }

    /// The telemetry upload URL.
    pub fn get_path(&self) -> anyhow::Result<Url> {
        let base = Url::parse(&self.server)
            .with_context(|| format!("invalid telemetry server URL: {}", self.server))?;
        Ok(crate::util::url_join_path(&base, &self.path))
    }

    /// The share link URL, which always carries a trailing slash.
    pub fn get_share(&self) -> anyhow::Result<Url> {
        let base = Url::parse(&self.server)
            .with_context(|| format!("invalid telemetry server URL: {}", self.server))?;
        let joined = path_join(base.path(), &self.share);
        let joined = if joined.starts_with('/') {
            format!("{joined}/")
        } else {
            format!("/{joined}/")
        };
        let mut u = base;
        u.set_path(&joined);
        Ok(u)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The log stamp of 2026-08-06 15:41:36 and `nanos` on a clock
    /// `offset_seconds` east of UTC.
    fn at(nanos: u32, offset_seconds: i32) -> String {
        let offset = time::UtcOffset::from_whole_seconds(offset_seconds).unwrap();
        let t = time::Date::from_calendar_date(2026, time::Month::August, 6)
            .unwrap()
            .with_hms_nano(15, 41, 36, nanos)
            .unwrap()
            .assume_offset(offset);
        log_stamp(t)
    }

    #[test]
    fn log_stamps_keep_three_six_or_nine_digits_of_the_second() {
        assert_eq!(at(0, 7200), "2026-08-06 15:41:36 +0200");
        assert_eq!(at(120_000_000, 7200), "2026-08-06 15:41:36.120 +0200");
        assert_eq!(at(120_500_000, 7200), "2026-08-06 15:41:36.120500 +0200");
        assert_eq!(at(1, -19_800), "2026-08-06 15:41:36.000000001 -0530");
        assert_eq!(at(999_999_999, 0), "2026-08-06 15:41:36.999999999 +0000");
    }

    #[test]
    fn log_stamps_round_the_offset_to_the_minute() {
        assert_eq!(at(0, 19_829), "2026-08-06 15:41:36 +0530");
        assert_eq!(at(0, 19_830), "2026-08-06 15:41:36 +0531");
        assert_eq!(at(0, -19_830), "2026-08-06 15:41:36 -0531");
        assert_eq!(at(0, -29), "2026-08-06 15:41:36 -0000");
        assert_eq!(at(0, -30), "2026-08-06 15:41:36 -0001");
        assert_eq!(at(0, -31), "2026-08-06 15:41:36 -0001");
        assert_eq!(at(0, 29), "2026-08-06 15:41:36 +0000");
        assert_eq!(at(0, 30), "2026-08-06 15:41:36 +0001");
    }
}
