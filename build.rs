//! Records the build date so `--version` can report it.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    println!("cargo:rerun-if-env-changed=LIBRESPEED_REVISION");

    // SOURCE_DATE_EPOCH keeps reproducible builds reproducible; OpenWrt and
    // most distro packaging set it. Computing the date here rather than
    // shelling out to `date` avoids depending on GNU vs BSD flag differences.
    // A value that is not the decimal digits of a time in seconds up to the
    // end of 9999, an empty or signed one included, fails the build, as the
    // specification asks and as clang does, rather than giving it the date of
    // today.
    let at = match std::env::var_os("SOURCE_DATE_EPOCH") {
        None => time::OffsetDateTime::now_utc(),
        Some(epoch) => match epoch
            .to_str()
            .filter(|epoch| epoch.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|epoch| epoch.parse::<i64>().ok())
            .and_then(|epoch| time::OffsetDateTime::from_unix_timestamp(epoch).ok())
        {
            Some(at) => at,
            None => {
                println!(
                    "cargo::error=SOURCE_DATE_EPOCH is {epoch:?}, which is not a non-negative decimal integer up to 253402300799"
                );
                return;
            }
        },
    };
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
}
