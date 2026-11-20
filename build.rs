//! Records the build date and the revision so `--version` can report them.

#[path = "src/revision.rs"]
mod revision;

fn main() {
    // SOURCE_DATE_EPOCH keeps reproducible builds reproducible; OpenWrt and
    // most distro packaging set it. Computing the date here rather than
    // shelling out to `date` avoids depending on GNU vs BSD flag differences.
    let date = match std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|epoch| epoch.parse::<i64>().ok())
        .and_then(|epoch| chrono::DateTime::from_timestamp(epoch, 0))
    {
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        None => chrono::Utc::now()
            .format("%Y-%m-%d %H:%M:%S UTC")
            .to_string(),
    };

    // The build date alone cannot identify a build: distro packaging derives
    // SOURCE_DATE_EPOCH from the package, so two builds of different commits
    // share it. Packagers can pass the revision explicitly, and the top of a
    // git checkout finds it itself.
    let revision = std::env::var("LIBRESPEED_REVISION")
        .ok()
        .filter(|r| !r.is_empty())
        .or_else(revision::git_revision)
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=LIBRESPEED_BUILD_DATE={date}");
    println!("cargo:rustc-env=LIBRESPEED_REVISION={revision}");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    println!("cargo:rerun-if-env-changed=LIBRESPEED_REVISION");
}
