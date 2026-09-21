//! Differential tests against the Go client, librespeed/speedtest-cli.
//!
//! Both binaries are run with the same arguments against the same in-process
//! backend, and their standard output, standard error and exit status are
//! compared. `cases.rs` declares for every case what is expected: the same
//! output, or the exact lines that differ together with the entry of the
//! README's list of deliberate differences that explains them. A case that
//! differs where it should not fails, and so does one that stopped differing
//! or differs in another way. The two lists are held against each other as
//! well: every difference declared here names an entry of the README's list,
//! and every entry of that list has a case behind it or is named as one this
//! harness cannot reach -- so neither list can quietly go stale.
//!
//! The Go binary is taken from `LIBRESPEED_GO_BIN`; without it the comparison
//! is skipped, and only the harness's own tests run. Build it from the commit
//! the expectations were recorded against:
//!
//! ```sh
//! git clone https://github.com/librespeed/speedtest-cli go-client
//! git -C go-client checkout b660d1e6c24f14fc93624538d9e73163e7784335
//! (cd go-client && go build -o librespeed-go .)
//! LIBRESPEED_GO_BIN=$PWD/go-client/librespeed-go cargo test --test parity
//! ```
//!
//! `LIBRESPEED_PARITY_ONLY=name,name` runs just the named cases.

mod cases;
mod compare;
mod fixture;
mod normalize;

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use cases::{Case, Exit, Stream};
use compare::{difference, lines_match};
use fixture::{dead_port, Backend, Variant};
use normalize::{decode, normalize, Context};

const RUST_BIN: &str = env!("CARGO_BIN_EXE_librespeed-cli");
const GO_BIN_VAR: &str = "LIBRESPEED_GO_BIN";
const GO_COMMIT_VAR: &str = "LIBRESPEED_GO_COMMIT";
const ONLY_VAR: &str = "LIBRESPEED_PARITY_ONLY";

/// The longest a single run may take before it counts as hung.
const RUN_TIMEOUT: Duration = Duration::from_secs(120);

/// Cases run side by side. Every number a busy machine could change is
/// normalized away, so this only has to stay below what a CI runner can carry.
const WORKERS: usize = 4;

const SERVER_LISTS: [(&str, &str); 7] = [
    ("servers.json", include_str!("../data/parity/servers.json")),
    (
        "servers-special.json",
        include_str!("../data/parity/servers-special.json"),
    ),
    (
        "servers-schemeless.json",
        include_str!("../data/parity/servers-schemeless.json"),
    ),
    (
        "servers-schemeless-port.json",
        include_str!("../data/parity/servers-schemeless-port.json"),
    ),
    (
        "servers-hostile.json",
        include_str!("../data/parity/servers-hostile.json"),
    ),
    (
        "servers-garbled.json",
        include_str!("../data/parity/servers-garbled.json"),
    ),
    ("bad.json", include_str!("../data/parity/bad.json")),
];

/// The fixtures and the directory of server lists pointing at them.
struct World {
    dir: PathBuf,
    tokens: Vec<(&'static str, String)>,
    context: Context,
}

impl World {
    fn start() -> World {
        let live = Backend::start(Variant::Plain);
        let hostile = Backend::start(Variant::Hostile);
        let garbled = Backend::start(Variant::Garbled);
        let dead = dead_port();

        let dir = std::env::temp_dir().join(format!("librespeed-parity-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create the fixture directory");
        let fix = dir
            .to_str()
            .expect("a UTF-8 temporary directory")
            .to_string();

        let tokens = vec![
            ("@FIX@", fix.clone()),
            ("@LIVE@", live.port.to_string()),
            ("@DEAD@", dead.to_string()),
            ("@HOSTILE@", hostile.port.to_string()),
            ("@GARBLED@", garbled.port.to_string()),
        ];
        let world = World {
            dir,
            tokens,
            context: Context {
                fix,
                ports: vec![
                    (live.port, "LIVE"),
                    (dead, "DEAD"),
                    (hostile.port, "HOSTILE"),
                    (garbled.port, "GARBLED"),
                ],
            },
        };
        for (name, template) in SERVER_LISTS {
            std::fs::write(world.dir.join(name), world.fill(template)).expect("write server list");
        }
        world
    }

    fn fill(&self, template: &str) -> String {
        let mut text = template.to_string();
        for (token, value) in &self.tokens {
            text = text.replace(token, value);
        }
        text
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Run {
    stdout: Vec<String>,
    stderr: Vec<String>,
    exit: i32,
}

fn run(binary: &str, case: &Case, world: &World) -> Run {
    let args: Vec<String> = case.args.iter().map(|arg| world.fill(arg)).collect();
    let mut command = Command::new(binary);
    command
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Go's client honours these, and neither client should leave the machine.
    for proxy in ["http_proxy", "https_proxy", "all_proxy", "no_proxy"] {
        command.env_remove(proxy).env_remove(proxy.to_uppercase());
    }
    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("{binary} did not start: {error}"));

    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let (out, err, status) = std::thread::scope(|scope| {
        let out = scope.spawn(move || {
            let mut bytes = Vec::new();
            let _ = stdout.read_to_end(&mut bytes);
            bytes
        });
        let err = scope.spawn(move || {
            let mut bytes = Vec::new();
            let _ = stderr.read_to_end(&mut bytes);
            bytes
        });
        let started = Instant::now();
        let status = loop {
            match child.try_wait().expect("wait for the client") {
                Some(status) => break status,
                None if started.elapsed() > RUN_TIMEOUT => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("{}: {binary} {args:?} is still running", case.name);
                }
                None => std::thread::sleep(Duration::from_millis(5)),
            }
        };
        (out.join().unwrap(), err.join().unwrap(), status)
    });

    let view = |bytes: &[u8]| case.view.apply(normalize(&world.context, &decode(bytes)));
    Run {
        stdout: view(&out),
        stderr: view(&err),
        // No code: a signal ended it, which no case expects of either client.
        exit: status.code().unwrap_or(-1),
    }
}

/// What a case got wrong, ready to be read or pasted into `cases.rs`.
fn judge(case: &Case, go: &Run, rust: &Run) -> Vec<String> {
    let mut failures = Vec::new();
    for (name, expected, go_lines, rust_lines) in [
        ("stdout", &case.stdout, &go.stdout, &rust.stdout),
        ("stderr", &case.stderr, &go.stderr, &rust.stderr),
    ] {
        let actual = difference(go_lines, rust_lines);
        let as_expected = match expected {
            Stream::Same => actual.is_empty(),
            Stream::Differs { go, rust, .. } => {
                lines_match(go, &actual.go) && lines_match(rust, &actual.rust)
            }
        };
        if as_expected {
            continue;
        }
        let expected = match expected {
            Stream::Same => "expected the same output".to_string(),
            Stream::Differs { why, go, rust } => format!(
                "expected the difference explained by {why:?}:\n    only Go:   {go:#?}\n    only Rust: {rust:#?}"
            ),
        };
        let found = if actual.is_empty() {
            "the outputs are the same; if that is intended, declare it".to_string()
        } else {
            format!(
                "only Go:   {:#?}\n    only Rust: {:#?}",
                actual.go, actual.rust
            )
        };
        failures.push(format!("{name}: {expected}\n  found:\n    {found}"));
    }

    let as_expected = match case.exit {
        Exit::Same => go.exit == rust.exit,
        Exit::Differs {
            go: go_exit,
            rust: rust_exit,
            ..
        } => (go.exit, rust.exit) == (go_exit, rust_exit),
    };
    if !as_expected {
        failures.push(format!(
            "exit status: expected {:?}, found Go {} and Rust {}",
            case.exit, go.exit, rust.exit
        ));
    }
    failures
}

/// The Go client to compare with, or `None` with the reason on the terminal.
fn go_binary() -> Option<String> {
    let Some(path) = std::env::var_os(GO_BIN_VAR) else {
        // Written past the test harness's capture: a skipped comparison has to
        // be visible in the output of a plain `cargo test`.
        let _ = writeln!(
            std::io::stderr(),
            "parity: skipped: set {GO_BIN_VAR} to the Go client built from commit {} to compare against it",
            cases::GO_COMMIT
        );
        return None;
    };
    let path = path.into_string().expect("a UTF-8 path to the Go client");
    assert!(
        std::path::Path::new(&path).is_file(),
        "{GO_BIN_VAR} is set to {path:?}, which is not a file"
    );
    Some(path)
}

#[test]
fn outputs_match_the_go_client_except_where_declared() {
    let Some(go_bin) = go_binary() else {
        return;
    };
    let only = std::env::var(ONLY_VAR).unwrap_or_default();
    let only: Vec<&str> = only.split(',').filter(|name| !name.is_empty()).collect();
    let matrix: Vec<Case> = cases::matrix()
        .into_iter()
        .filter(|case| only.is_empty() || only.contains(&case.name))
        .collect();
    assert!(!matrix.is_empty(), "{ONLY_VAR} names no case");

    let world = World::start();
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<(usize, Vec<String>)>> = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..WORKERS {
            scope.spawn(|| loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                let Some(case) = matrix.get(index) else {
                    break;
                };
                let go = run(&go_bin, case, &world);
                let rust = run(RUST_BIN, case, &world);
                let failures = judge(case, &go, &rust);
                results.lock().unwrap().push((index, failures));
            });
        }
    });

    let mut results = results.into_inner().unwrap();
    results.sort_by_key(|(index, _)| *index);
    let mut report = String::new();
    let mut failed = 0;
    for (index, failures) in &results {
        let case = &matrix[*index];
        let verdict = if failures.is_empty() { "ok" } else { "FAILED" };
        report.push_str(&format!(
            "{:<28} {:<8} {:<8} {:<8} {verdict}\n",
            case.name,
            case.stdout.label(),
            case.stderr.label(),
            case.exit.label(),
        ));
        if !failures.is_empty() {
            failed += 1;
            report.push_str(&format!("  arguments: {:?}\n", case.args));
            for failure in failures {
                report.push_str(&format!("  {failure}\n"));
            }
        }
    }
    println!(
        "{:<28} {:<8} {:<8} {:<8}\n{report}",
        "case", "stdout", "stderr", "exit"
    );
    assert!(
        failed == 0,
        "{failed} of {} cases did not compare to the Go client as declared:\n{report}",
        matrix.len()
    );
}

/// Whoever builds the Go client says which commit they built, and the
/// expectations were recorded against one commit only.
#[test]
fn the_go_client_is_the_commit_the_expectations_name() {
    let Ok(commit) = std::env::var(GO_COMMIT_VAR) else {
        return;
    };
    assert_eq!(
        commit,
        cases::GO_COMMIT,
        "{GO_COMMIT_VAR} is another commit than the expectations were recorded against"
    );
}

#[test]
fn every_declared_difference_is_explained_in_the_readme() {
    let readme = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md"))
        .expect("read README.md");
    let readme = readme.split_whitespace().collect::<Vec<_>>().join(" ");
    for case in cases::matrix() {
        for why in case.reasons() {
            assert!(
                readme.contains(why.readme()),
                "{}: README.md no longer says {:?}, which explains {why:?}",
                case.name,
                why.readme()
            );
        }
    }
}

/// The other direction: an entry of the README's list that no case declares,
/// and that is not named as one this harness cannot reach. Without it the list
/// could keep describing behaviour that has since gone.
#[test]
fn every_readme_entry_is_declared_or_a_named_exception() {
    let readme = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md"))
        .expect("read README.md");
    let section = readme
        .split("## Differences from the Go implementation")
        .nth(1)
        .expect("the list of deliberate differences")
        .split("\n## ")
        .next()
        .expect("the end of the list");

    let declared: Vec<&str> = cases::matrix()
        .iter()
        .flat_map(|case| case.reasons())
        .map(|why| why.readme())
        .collect();

    let mut entries = 0;
    for line in section.lines() {
        let Some(rest) = line.strip_prefix("- **") else {
            continue;
        };
        let end = rest.find("**").expect("a bolded entry that ends");
        let entry = format!("**{}**", &rest[..end]);
        entries += 1;
        assert!(
            declared.contains(&entry.as_str()) || cases::UNEXERCISED.contains(&entry.as_str()),
            "README.md lists {entry:?}, which no case declares: give it one, or name it in UNEXERCISED"
        );
    }
    assert!(entries > 0, "the README's list of differences is empty");

    for entry in cases::UNEXERCISED {
        assert!(
            section.contains(entry),
            "UNEXERCISED names {entry:?}, which the README's list no longer has"
        );
    }
}

#[test]
fn the_matrix_is_well_formed() {
    let matrix = cases::matrix();
    let mut names: Vec<&str> = matrix.iter().map(|case| case.name).collect();
    names.sort_unstable();
    let before = names.len();
    names.dedup();
    assert_eq!(names.len(), before, "two cases share a name");

    for case in &matrix {
        for stream in [&case.stdout, &case.stderr] {
            if let Stream::Differs { go, rust, .. } = stream {
                assert!(
                    !go.is_empty() || !rust.is_empty(),
                    "{}: a difference with no lines is no difference",
                    case.name
                );
            }
        }
        if let Exit::Differs { go, rust, .. } = case.exit {
            assert_ne!(go, rust, "{}: the exit statuses do not differ", case.name);
        }
    }
}
