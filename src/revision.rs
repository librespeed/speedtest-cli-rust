//! The git revision build.rs gives `--version`. build.rs and
//! tests/revision.rs include this file by path, it is not a module of the
//! library.

use std::process::Command;

/// The commit checked out in the current directory, or `None` unless that
/// directory is the top of a git work tree.
///
/// Git cannot tell whether the work tree is this project's, so sources copied
/// to the top of another repository still get its commit.
pub fn git_revision() -> Option<String> {
    // Below the top, as for sources unpacked inside another checkout, or
    // inside a git directory, the commit would be that repository's.
    if git(&["rev-parse", "--is-inside-work-tree"])? != "true"
        || !git(&["rev-parse", "--show-prefix"])?.is_empty()
    {
        return None;
    }
    git(&["rev-parse", "--short=12", "HEAD"]).filter(|r| !r.is_empty())
}

/// What git printed, without its last line break, or `None` when it failed.
///
/// `GIT_DIR` and `GIT_WORK_TREE` are cleared, because they name a repository
/// wherever the build runs, and git exports `GIT_DIR` to its hooks. The rest
/// of the environment is respected, `GIT_CEILING_DIRECTORIES` included, which
/// OpenWrt sets so git stays inside its build directory.
fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .ok()?;
    output.status.success().then(|| {
        String::from_utf8_lossy(&output.stdout)
            .trim_end_matches(['\r', '\n'])
            .to_string()
    })
}
