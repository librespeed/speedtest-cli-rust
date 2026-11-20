//! The git revision build.rs gives `--version`. build.rs and
//! tests/revision.rs include this file by path, it is not a module of the
//! library.

use std::path::PathBuf;
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
    watch_refs();
    git(&["rev-parse", "--short=12", "HEAD"]).filter(|r| !r.is_empty())
}

/// Tells Cargo to run the build script again when the checked-out commit
/// changes.
///
/// Cargo compares the mtime of each named path, of the newest file in it for
/// a directory, with the time the script last started. A commit, a checkout,
/// a reset or a branch update rewrites one of these: the HEAD file of this
/// worktree, a file in the shared `refs/heads`, which a commit after
/// `git pack-refs` adds back, or the `tables.list` of a reftable, the
/// worktree's own for its HEAD and the shared one for the branches. A path
/// that does not exist would make Cargo rerun the script on every build, so
/// only the existing ones are named. The price is a rebuild now and then while
/// HEAD stays: after a change to any branch, in another worktree too, after
/// `git pack-refs`, and in a reftable after any ref update, a fetch included.
fn watch_refs() {
    let Some(answer) = git(&[
        "rev-parse",
        "--path-format=absolute",
        "--git-path",
        "HEAD",
        "--git-path",
        "refs/heads",
        "--git-path",
        "reftable/tables.list",
        "--git-common-dir",
    ]) else {
        return;
    };
    let mut paths: Vec<PathBuf> = answer.lines().map(PathBuf::from).collect();
    // The last answer is the common directory. Git before 2.31 does not know
    // --path-format and prints it back as a line of its own, which does not
    // exist and is skipped, and the paths after it are relative to this
    // directory, the package root.
    if let Some(common) = paths.pop() {
        paths.push(common.join("reftable").join("tables.list"));
    }
    // In the main worktree both tables.list are the same file.
    paths.dedup();
    for path in paths.iter().filter(|path| path.exists()) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
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
