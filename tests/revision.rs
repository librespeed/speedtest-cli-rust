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

/// A checkout with the fixture and its own target directory.
struct Tree {
    dir: PathBuf,
    target: PathBuf,
    reported: String,
}

impl Tree {
    fn new(dir: PathBuf, target: PathBuf) -> Self {
        Tree {
            dir,
            target,
            reported: String::new(),
        }
    }

    /// Builds after HEAD moved and checks the new commit is reported.
    fn follows(&mut self, step: &str) {
        let head = head(&self.dir);
        assert_ne!(head, self.reported, "{step}: HEAD did not move");
        let (revision, _) = build(&self.dir, &self.target, &[]);
        assert_eq!(revision, head, "{step}");
        self.reported = revision;
    }

    /// Builds with nothing changed and checks the build script did not run.
    fn stays(&self, step: &str) {
        let (revision, script) = build(&self.dir, &self.target, &[]);
        assert!(script.is_empty(), "{step}: the build script ran again");
        assert_eq!(revision, self.reported, "{step}");
    }

    /// Moves HEAD in each way git does, building after each move.
    fn moves(&mut self, branch: &str) {
        let dir = self.dir.clone();
        self.follows("first build");
        self.stays("nothing changed");
        commit(&dir, "next");
        self.follows("commit");
        git(
            &dir,
            &["commit", "-q", "--allow-empty", "--amend", "-m", "amended"],
        );
        self.follows("amend");
        git(
            &dir,
            &["checkout", "-q", "-b", &format!("{branch}-side"), "HEAD~1"],
        );
        self.follows("switch to another branch");
        git(&dir, &["checkout", "-q", "--detach", "HEAD~1"]);
        self.follows("detached checkout");
        commit(&dir, "detached");
        self.follows("commit on a detached HEAD");
        git(&dir, &["checkout", "-q", branch]);
        self.follows("switch back");
        git(&dir, &["reset", "-q", "--hard", "HEAD~1"]);
        self.follows("reset");
        git(&dir, &["pack-refs", "--all"]);
        commit(&dir, "after pack-refs");
        self.follows("commit after pack-refs");
        self.stays("nothing changed again");
    }
}

/// HEAD moves in a repository and in a linked worktree of it, and the branch
/// of the worktree moves from the main checkout.
fn follows_head(name: &str, init: &[&str]) {
    let root = scratch(name);
    let main = root.join("main");
    repository(&main, init);
    let mut tree = Tree::new(main.clone(), root.join("target-main"));
    tree.moves("main");

    git(
        &main,
        &["worktree", "add", "-q", "-b", "linked", "../linked"],
    );
    let mut linked = Tree::new(root.join("linked"), root.join("target-linked"));
    linked.moves("linked");
    git(
        &main,
        &["update-ref", "refs/heads/linked", "refs/heads/main"],
    );
    linked.follows("branch moved from the main checkout");
    linked.stays("nothing changed after the move");
    fs::remove_dir_all(root).expect("remove the scratch directory");
}

#[test]
fn the_revision_follows_head_with_refs_in_files() {
    follows_head("files", &["init", "-q"]);
}

#[test]
fn the_revision_follows_head_with_refs_in_a_reftable() {
    let probe = scratch("reftable-probe");
    let supported = git_command(&probe)
        .args(["init", "-q", "--ref-format=reftable"])
        .output()
        .is_ok_and(|output| output.status.success());
    fs::remove_dir_all(probe).expect("remove the scratch directory");
    if !supported {
        eprintln!("skipped: this git cannot create a reftable repository");
        return;
    }
    follows_head("reftable", &["init", "-q", "--ref-format=reftable"]);
}

/// Git before 2.31, which does not know `--path-format` and prints it back as a
/// line of its own, as builtin/rev-parse.c in v2.30.0 does.
const OLD_GIT: &str = r#"#!/bin/sh
if [ "$1" = rev-parse ]; then
  for arg in "$@"; do
    shift
    case "$arg" in
      --path-format=*) printf '%s\n' "$arg" ;;
      *) set -- "$@" "$arg" ;;
    esac
  done
fi
exec "$REAL_GIT" "$@"
"#;

#[test]
fn the_revision_follows_head_with_git_before_2_31() {
    use std::os::unix::fs::PermissionsExt;

    let root = scratch("old-git");
    let repo = root.join("repo");
    repository(&repo, &["init", "-q"]);
    let bin = root.join("bin");
    fs::create_dir(&bin).expect("create the bin directory");
    fs::write(bin.join("git"), OLD_GIT).expect("write the old git");
    fs::set_permissions(bin.join("git"), fs::Permissions::from_mode(0o755))
        .expect("make the old git executable");
    let real = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .expect("find git");
    let real = PathBuf::from(String::from_utf8_lossy(&real.stdout).trim());
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .expect("join PATH");
    let env = [("PATH", Path::new(&path)), ("REAL_GIT", real.as_path())];
    let target = root.join("target");

    let (revision, script) = build(&repo, &target, &env);
    assert_eq!(revision, head(&repo), "first build");
    // The paths the build script named are relative, so the stand-in was used.
    assert!(
        script.contains("cargo:rerun-if-changed=.git/HEAD\n"),
        "{script}"
    );
    assert!(!script.contains("--path-format"), "{script}");
    commit(&repo, "next");
    assert_eq!(build(&repo, &target, &env).0, head(&repo), "commit");
    let (revision, script) = build(&repo, &target, &env);
    assert!(script.is_empty(), "the build script ran again");
    assert_eq!(revision, head(&repo), "nothing changed");
    fs::remove_dir_all(root).expect("remove the scratch directory");
}
