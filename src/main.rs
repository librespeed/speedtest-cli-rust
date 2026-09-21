//! librespeed-cli — test your Internet speed with LibreSpeed.

use clap::Parser;

use librespeed_cli::cli::{self, Cli};
use librespeed_cli::{output, speedtest, write_out};

#[tokio::main]
async fn main() {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            // The usual mistakes are reported the way the Go client reports
            // them: the usage line on stdout, then the reason as the last line
            // on stderr, which is the line a wrapping script shows.
            if let Some(message) = cli::go_usage_error(&e) {
                write_out!("Incorrect Usage: {message}\n\n");
                output::fatal(format!("Terminated due to error: {message}"));
            }
            let _ = e.print();
            // --help and --version are not failures. A real usage error exits
            // 1, where clap would exit 2: every failure in the Go client exits
            // 1, and an init script branching on the status has to see the
            // same number from both.
            std::process::exit(if e.use_stderr() { 1 } else { 0 });
        }
    };

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
