//! librespeed-cli — test your Internet speed with LibreSpeed.

use clap::Parser;

use librespeed_cli::cli::Cli;
use librespeed_cli::{output, speedtest, write_error};

#[tokio::main]
async fn main() {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            // --help and --version are not failures. A real usage error exits
            // 1, where clap would exit 2: every failure in the Go client exits
            // 1, and an init script branching on the status has to see the
            // same number from both.
            std::process::exit(if e.use_stderr() { 1 } else { 0 });
        }
    };

    if let Err(e) = speedtest::run(&cli).await {
        // `{:#}` renders the whole error chain, so the underlying cause
        // (DNS failure, refused connection, ...) is visible.
        write_error!("{e:#}\n");
        output::fatal("Terminated due to error");
    }

    // Exit rather than returning, so the process does not depend on the tokio
    // runtime winding down. Returning from main drops the runtime, which joins
    // its worker threads, and on 32-bit PowerPC musl (Turris 1.x) that never
    // completes: every successful run hung after printing its output, while
    // error paths exited fine because they go through output::fatal, which
    // already calls process::exit. All output is flushed as it is written.
    std::process::exit(0);
}
