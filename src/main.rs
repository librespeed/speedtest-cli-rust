//! librespeed-cli — test your Internet speed with LibreSpeed.

use clap::Parser;

use librespeed_cli::cli::Cli;
use librespeed_cli::{output, speedtest};

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    if let Err(e) = speedtest::run(&cli).await {
        // The whole chain, so the underlying cause (DNS failure, refused
        // connection, ...) is visible, after the Go client's prefix.
        output::fatal(format!(
            "Terminated due to error: {}",
            output::error_text(&e)
        ));
    }

    // Exit rather than returning, so the process does not depend on the tokio
    // runtime winding down. Returning from main drops the runtime, which joins
    // its worker threads, and on 32-bit PowerPC musl (Turris 1.x) that never
    // completes: every successful run hung after printing its output, while
    // error paths exited fine because they go through output::fatal, which
    // already calls process::exit. All output is flushed as it is written.
    std::process::exit(0);
}
