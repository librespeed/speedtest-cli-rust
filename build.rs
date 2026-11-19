//! Records the build date so `--version` can report it.

use std::process::Command;

fn main() {
    // SOURCE_DATE_EPOCH keeps reproducible builds reproducible; OpenWrt and
    // most distro packaging set it. Computing the date here rather than
    // shelling out to `date` avoids depending on GNU vs BSD flag differences.
    let at = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|epoch| epoch.parse::<i64>().ok())
        .and_then(|epoch| time::OffsetDateTime::from_unix_timestamp(epoch).ok())
        .unwrap_or_else(time::OffsetDateTime::now_utc);
    let date = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute(),
        at.second()
    );

    // The build date alone cannot identify a build: distro packaging derives
    // SOURCE_DATE_EPOCH from the package, so two builds of different commits
    // share it. Packagers can pass the revision explicitly; a git checkout
    // finds it itself.
    let revision = std::env::var("LIBRESPEED_REVISION")
        .ok()
        .filter(|r| !r.is_empty())
        .or_else(|| {
            Command::new("git")
                .args(["rev-parse", "--short=12", "HEAD"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|r| !r.is_empty())
        })
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=LIBRESPEED_BUILD_DATE={date}");
    println!("cargo:rustc-env=LIBRESPEED_REVISION={revision}");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    println!("cargo:rerun-if-env-changed=LIBRESPEED_REVISION");
}
