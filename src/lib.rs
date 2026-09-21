//! librespeed-cli — test your Internet speed with LibreSpeed.
//!
//! The binary is a thin wrapper around this library, so the parsing and
//! measurement logic can be exercised by tests and fuzz targets without
//! spawning a process.

pub mod cli;
pub mod defs;
pub mod helper;
pub mod http;
pub mod output;
pub mod ping;
pub mod quote;
pub mod report;
pub mod speedtest;
pub mod spinner;
pub mod util;
