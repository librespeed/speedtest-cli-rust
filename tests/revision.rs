//! The revision build.rs gives `--version`, checked by building a small crate
//! whose build script includes src/revision.rs as build.rs does, in temporary
//! git repositories.
//!
//! Not run on Windows, where it has not been tried.

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const REVISION_RS: &str = include_str!("../src/revision.rs");

const BUILD_RS: &str = r#"#[path = "revision.rs"]
mod revision;

fn main() {
    println!("cargo:rerun-if-env-changed=LIBRESPEED_REVISION");
    let revision = revision::git_revision().unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=REVISION={revision}");
}
"#;

const MAIN_RS: &str = r#"fn main() {
    println!("{}", env!("REVISION"));
}
"#;

// An include list like the crate's own, so Cargo finds the files without
// asking git, which refuses inside a bare repository.
const CARGO_TOML: &str = r#"[package]
name = "revision-fixture"
version = "0.0.0"
edition = "2021"
include = ["/build.rs", "/revision.rs", "/src"]

[workspace]
"#;

/// An empty directory of its own for each test.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("revision")
        .join(name);
    if dir.exists() {
        fs::remove_dir_all(&dir).expect("clear the scratch directory");
    }
    fs::create_dir_all(&dir).expect("create the scratch directory");
    dir
}

/// git in `dir`, without the user's configuration or the repository of a hook
/// the tests may run from.
fn git_command(dir: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE");
    command
}

/// Runs git in `dir` and returns its output.
fn git(dir: &Path, args: &[&str]) -> String {
    let output = git_command(dir).args(args).output().expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn commit(dir: &Path, message: &str) {
    git(dir, &["commit", "-q", "--allow-empty", "-m", message]);
}

fn head(dir: &Path) -> String {
    git(dir, &["rev-parse", "--short=12", "HEAD"])
}

fn write_fixture(dir: &Path) {
    fs::create_dir_all(dir.join("src")).expect("create the fixture");
    for (path, text) in [
        ("Cargo.toml", CARGO_TOML),
        ("build.rs", BUILD_RS),
        ("revision.rs", REVISION_RS),
        ("src/main.rs", MAIN_RS),
    ] {
        fs::write(dir.join(path), text).expect("write the fixture");
    }
}

/// A repository holding the fixture on the branch main, with one commit on top
/// of it.
fn repository(dir: &Path, init: &[&str]) {
    fs::create_dir_all(dir).expect("create the repository");
    git(dir, init);
    git(dir, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    write_fixture(dir);
    git(dir, &["add", "-A"]);
    commit(dir, "fixture");
    commit(dir, "base");
}

/// Builds and runs the fixture in `dir` and returns the revision it reports and
/// what its build script printed, nothing when the script did not run.
fn build(dir: &Path, target: &Path, env: &[(&str, &Path)]) -> (String, String) {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    // -vv shows what the build script prints, and only when it runs, on
    // stdout before the output of the fixture.
    let output = Command::new(cargo)
        .args(["run", "--offline", "-vv"])
        .current_dir(dir)
        .env("CARGO_TARGET_DIR", target)
        .env("CARGO_TERM_COLOR", "never")
        .envs(env.iter().copied())
        .output()
        .expect("run cargo");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "cargo run in {}: {stderr}",
        dir.display()
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let (mut fixture, mut script) = (Vec::new(), String::new());
    for line in stdout.lines() {
        match line.strip_prefix("[revision-fixture 0.0.0] ") {
            Some(printed) => script += &format!("{printed}\n"),
            None => fixture.push(line),
        }
    }
    (fixture.join("\n"), script)
}

#[test]
fn the_top_of_a_work_tree_reports_its_commit() {
    let root = scratch("top");
    let repo = root.join("repo");
    repository(&repo, &["init", "-q"]);
    assert_eq!(build(&repo, &root.join("target"), &[]).0, head(&repo));
    fs::remove_dir_all(root).expect("remove the scratch directory");
}

#[test]
fn no_revision_below_the_top_of_a_work_tree() {
    let root = scratch("below");
    let outer = root.join("outer");
    repository(&outer, &["init", "-q"]);

    // Sources unpacked inside another checkout.
    let sources = outer.join("sources");
    write_fixture(&sources);
    assert_eq!(
        build(&sources, &root.join("target-below"), &[]).0,
        "unknown"
    );

    // The same, with the GIT_DIR git gives a hook of that checkout.
    let git_dir = outer.join(".git");
    let env = [("GIT_DIR", git_dir.as_path())];
    assert_eq!(
        build(&sources, &root.join("target-git-dir"), &env).0,
        "unknown"
    );

    // Inside the git directory of a bare repository with a commit.
    let bare = root.join("bare.git");
    git(&root, &["init", "-q", "--bare", "bare.git"]);
    git(&bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    git(
        &outer,
        &["push", "-q", bare.to_str().unwrap(), "HEAD:refs/heads/main"],
    );
    let inside = bare.join("sources");
    write_fixture(&inside);
    assert_eq!(build(&inside, &root.join("target-bare"), &[]).0, "unknown");
    fs::remove_dir_all(root).expect("remove the scratch directory");
}
