//! A minimal progress spinner for the interactive UI.
//!
//! Unlike the Go implementation — which drives the spinner on stdout and drops
//! the final result line entirely when stdout is not a terminal — this writes to
//! stderr and always emits the final message, so results survive being piped.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::output;

/// The braille frames used by the Go version (`spinner.CharSets[11]`).
const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const TICK: Duration = Duration::from_millis(100);

pub struct Spinner {
    running: Arc<AtomicBool>,
    /// Wakes the animation the moment it is stopped, rather than leaving the
    /// caller to wait out the tick it is asleep in. Three spinners run per
    /// server tested, so on a whole server list that wait adds up.
    stopped: Arc<Notify>,
    handle: Option<JoinHandle<()>>,
    animated: bool,
}

impl Spinner {
    /// Starts a spinner that renders `prefix`, a frame, and the current value of
    /// `suffix`. When stderr is not a terminal nothing is drawn, but `stop` still
    /// prints the final message.
    pub fn start<F>(prefix: impl Into<String>, suffix: F) -> Self
    where
        F: Fn() -> String + Send + 'static,
    {
        let animated = output::ui_is_terminal() && !output::is_quiet();
        let running = Arc::new(AtomicBool::new(true));
        let stopped = Arc::new(Notify::new());

        let handle = if animated {
            let prefix = prefix.into();
            let running = running.clone();
            let stopped = stopped.clone();
            Some(tokio::spawn(async move {
                let mut frame = 0usize;
                while running.load(Ordering::Relaxed) {
                    let line = format!("{}{}{}", prefix, FRAMES[frame % FRAMES.len()], suffix());
                    // \x1b[2K clears the whole line so shorter frames don't leave residue.
                    output::_err(format_args!("\r\x1b[2K{line}"));
                    frame = frame.wrapping_add(1);
                    tokio::select! {
                        _ = stopped.notified() => return,
                        _ = tokio::time::sleep(TICK) => {}
                    }
                }
            }))
        } else {
            None
        };

        Self {
            running,
            stopped,
            handle,
            animated,
        }
    }

    /// Stops the animation and prints `final_msg`, which should end with a newline.
    pub async fn stop(mut self, final_msg: &str) {
        self.running.store(false, Ordering::Relaxed);
        self.stopped.notify_one();
        if let Some(handle) = self.handle.take() {
            let _ = handle.await;
        }
        if self.animated {
            output::_err(format_args!("\r\x1b[2K"));
        }
        output::_err(format_args!("{final_msg}"));
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        self.stopped.notify_one();
    }
}
