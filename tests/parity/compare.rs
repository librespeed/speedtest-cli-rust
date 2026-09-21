//! Line differences between the two clients' outputs, and the views that make
//! `--help` and `--version` comparable at all.

/// The lines only one of the clients printed, in the order they came.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Difference {
    pub go: Vec<String>,
    pub rust: Vec<String>,
}

impl Difference {
    pub fn is_empty(&self) -> bool {
        self.go.is_empty() && self.rust.is_empty()
    }
}

/// What is left of each side once their longest common subsequence of lines is
/// taken away, which is what `diff` would mark with `-` and `+`.
pub fn difference(go: &[String], rust: &[String]) -> Difference {
    // common[i][j]: the length of the longest common subsequence of go[i..]
    // and rust[j..]. The outputs are a hundred lines at most.
    let mut common = vec![vec![0usize; rust.len() + 1]; go.len() + 1];
    for i in (0..go.len()).rev() {
        for j in (0..rust.len()).rev() {
            common[i][j] = if go[i] == rust[j] {
                common[i + 1][j + 1] + 1
            } else {
                common[i + 1][j].max(common[i][j + 1])
            };
        }
    }

    let mut difference = Difference::default();
    let (mut i, mut j) = (0, 0);
    while i < go.len() && j < rust.len() {
        if go[i] == rust[j] {
            i += 1;
            j += 1;
        } else if common[i + 1][j] >= common[i][j + 1] {
            difference.go.push(go[i].clone());
            i += 1;
        } else {
            difference.rust.push(rust[j].clone());
            j += 1;
        }
    }
    difference.go.extend_from_slice(&go[i..]);
    difference.rust.extend_from_slice(&rust[j..]);
    difference
}

/// Whether the lines are the expected ones. A `*` in an expected line stands
/// for any text, for the words an operating system puts into an error.
pub fn lines_match(expected: &[&str], actual: &[String]) -> bool {
    expected.len() == actual.len()
        && expected
            .iter()
            .zip(actual)
            .all(|(pattern, line)| glob(pattern, line))
}

fn glob(pattern: &str, text: &str) -> bool {
    let Some((head, tail)) = pattern.split_once('*') else {
        return pattern == text;
    };
    let Some(mut rest) = text.strip_prefix(head) else {
        return false;
    };
    let mut parts: Vec<&str> = tail.split('*').collect();
    let last = parts.pop().unwrap_or_default();
    for part in parts {
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    rest.ends_with(last)
}

/// How a stream is looked at before it is compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    /// As printed.
    Whole,
    /// `--version`: the first line carries the name, version and build date
    /// the build was given, which says nothing about the code.
    Version,
    /// `--help`: one line per option with its aliases, default and text. The
    /// layout belongs to the argument parser, urfave/cli there and clap here;
    /// the words are the client's.
    HelpOptions,
    /// A whole help text becomes one `<HELP>` line, for the cases where one
    /// client answers with the help and the other with a usage error.
    Help,
}

impl View {
    pub fn apply(self, lines: Vec<String>) -> Vec<String> {
        match self {
            View::Whole => lines,
            View::Version => {
                let mut lines = lines;
                if let Some(first) = lines.first_mut() {
                    *first = "<NAME AND VERSION>".to_string();
                }
                lines
            }
            View::HelpOptions => help_options(&lines),
            View::Help => {
                if lines.iter().any(|line| is_options_heading(line)) {
                    vec!["<HELP>".to_string()]
                } else {
                    lines
                }
            }
        }
    }
}

/// The line either argument parser puts above the list of options.
fn is_options_heading(line: &str) -> bool {
    matches!(line.trim(), "GLOBAL OPTIONS:" | "Options:")
}

struct HelpOption {
    names: Vec<String>,
    text: String,
}

/// Reads either client's `--help`.
///
/// urfave/cli puts the option and the start of its text on one line:
///
/// ```text
///    --ipv4, -4               Force IPv4 only (default: false)
/// ```
///
/// clap, with texts as long as these, puts the text on the following lines:
///
/// ```text
///   -4, --ipv4
///           Force IPv4 only
/// ```
///
/// Either way a text that wraps may continue with an option's name, so it is
/// the indentation that tells an option from the text of the one before.
fn help_options(lines: &[String]) -> Vec<String> {
    let mut about = None;
    let mut options: Vec<HelpOption> = Vec::new();
    let mut in_options = false;
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if is_options_heading(line) {
            in_options = true;
        } else if !in_options {
            // Go: "   librespeed-cli - <about>" under NAME:. clap: the first line.
            if let Some((_, text)) = trimmed.split_once(" - ") {
                about.get_or_insert(text.to_string());
            } else if index == 0 && !trimmed.ends_with(':') {
                about = Some(trimmed.to_string());
            }
        } else if trimmed.starts_with('-') && line.len() - line.trim_start().len() < 10 {
            // The option's names end where a value name or the text begins.
            let (spec, text) = trimmed.split_once("  ").unwrap_or((trimmed, ""));
            let mut names: Vec<String> = spec
                .split([' ', ','])
                .take_while(|word| word.is_empty() || word.starts_with('-'))
                .filter(|word| !word.is_empty())
                .map(str::to_string)
                .collect();
            names.sort_by_key(|name| std::cmp::Reverse(name.len()));
            options.push(HelpOption {
                names,
                text: text.trim().to_string(),
            });
        } else if let Some(option) = options.last_mut() {
            if !trimmed.is_empty() {
                if !option.text.is_empty() {
                    option.text.push(' ');
                }
                option.text.push_str(trimmed);
            }
        }
    }

    let mut view = vec![format!("about: {}", about.unwrap_or_default())];
    for option in options {
        let text = option.text.split_whitespace().collect::<Vec<_>>().join(" ");
        let (text, default) = split_default(&text);
        let default = match default {
            Some(default) if default != "false" => format!(" (default {default})"),
            _ => String::new(),
        };
        view.push(format!("{}{default}: {text}", option.names.join(", ")));
    }
    view
}

/// Splits a trailing `(default: x)` or `[default: x]` off an option's text.
fn split_default(text: &str) -> (String, Option<String>) {
    for (open, close) in [("(default: ", ')'), ("[default: ", ']')] {
        if let Some(at) = text.rfind(open) {
            if let Some(value) = text[at + open.len()..].strip_suffix(close) {
                let value = value
                    .strip_prefix('"')
                    .and_then(|v| v.strip_suffix('"'))
                    .unwrap_or(value);
                return (text[..at].trim_end().to_string(), Some(value.to_string()));
            }
        }
    }
    (text.to_string(), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter().map(|line| line.to_string()).collect()
    }

    #[test]
    fn a_difference_is_what_diff_would_mark() {
        let go = lines(&["a", "b", "c", "d"]);
        let rust = lines(&["a", "x", "c", "y", "d", "z"]);
        assert_eq!(
            difference(&go, &rust),
            Difference {
                go: lines(&["b"]),
                rust: lines(&["x", "y", "z"]),
            }
        );
        assert!(difference(&go, &go).is_empty());
        assert_eq!(difference(&[], &go).rust, go);
        assert_eq!(difference(&go, &[]).go, go);
    }

    #[test]
    fn a_line_printed_twice_is_not_the_same_as_once() {
        let once = lines(&["error"]);
        let twice = lines(&["error", "error"]);
        assert_eq!(difference(&once, &twice).rust, lines(&["error"]));
    }

    #[test]
    fn a_star_stands_for_any_text() {
        assert!(glob("plain", "plain"));
        assert!(!glob("plain", "plain "));
        assert!(glob(
            "error: * (os error <ERRNO>)",
            "error: refused (os error <ERRNO>)"
        ));
        assert!(glob("a*b*c", "a--b--c"));
        assert!(glob("a*", "a"));
        assert!(!glob("a*b*c", "a--c--b"));
        assert!(!glob("error: *", "warning: x"));
        assert!(lines_match(&["a*", "b"], &lines(&["ax", "b"])));
        assert!(!lines_match(&["a*"], &lines(&["ax", "b"])));
        assert!(!lines_match(&["a*", "b"], &lines(&["ax"])));
    }

    #[test]
    fn both_help_layouts_read_the_same() {
        let go = lines(&[
            "NAME:",
            "   librespeed-cli - Test your Internet speed with LibreSpeed",
            "",
            "USAGE:",
            "   librespeed-cli [global options]",
            "",
            "GLOBAL OPTIONS:",
            "   --help, -h                               show help",
            "   --ipv4, -4                               Force IPv4 only (default: false)",
            "   --no-icmp                                Do not use ICMP ping. ICMP doesn't work well under Linux",
            "                                            at this moment, so you might want to disable it (default: false)",
            "   --distance value                         Change distance unit shown in ISP info (default: \"km\")",
            "   --simple                                 Suppress verbose output, only show basic information",
            "                                             (default: false)",
            "   --bytes                                  Display values in bytes instead of bits. Does not affect",
            "                                            the image generated by --share, nor output from",
            "                                            --json or --csv (default: false)",
            "   --server SERVER [ --server SERVER ]      Specify a SERVER ID to test against. Can be supplied",
            "                                            multiple times. Cannot be used with --exclude",
            "   --timeout TIMEOUT                        HTTP TIMEOUT in seconds. (default: 15)",
        ]);
        let rust = lines(&[
            "Test your Internet speed with LibreSpeed",
            "",
            "Usage: librespeed-cli [OPTIONS]",
            "",
            "Options:",
            "  -h, --help",
            "          show help",
            "  -4, --ipv4",
            "          Force IPv4 only",
            "      --no-icmp",
            "          Do not use ICMP ping. ICMP doesn't work well under Linux at this moment, so you might want",
            "          to disable it",
            "      --distance <DISTANCE>",
            "          Change distance unit shown in ISP info [default: km]",
            "      --simple",
            "          Suppress verbose output, only show basic information",
            "      --bytes",
            "          Display values in bytes instead of bits. Does not affect the image generated by --share, nor output from",
            "          --json or --csv",
            "      --server <SERVER>",
            "          Specify a SERVER ID to test against. Can be supplied multiple times. Cannot be used with",
            "          --exclude",
            "      --timeout <TIMEOUT>",
            "          HTTP TIMEOUT in seconds. [default: 15]",
        ]);
        let view = View::HelpOptions.apply(go);
        assert_eq!(
            view,
            [
                "about: Test your Internet speed with LibreSpeed",
                "--help, -h: show help",
                "--ipv4, -4: Force IPv4 only",
                "--no-icmp: Do not use ICMP ping. ICMP doesn't work well under Linux at this moment, so you might want to disable it",
                "--distance (default km): Change distance unit shown in ISP info",
                "--simple: Suppress verbose output, only show basic information",
                "--bytes: Display values in bytes instead of bits. Does not affect the image generated by --share, nor output from --json or --csv",
                "--server: Specify a SERVER ID to test against. Can be supplied multiple times. Cannot be used with --exclude",
                "--timeout (default 15): HTTP TIMEOUT in seconds.",
            ]
        );
        assert_eq!(View::HelpOptions.apply(rust), view);
    }

    #[test]
    fn the_help_view_keeps_a_usage_error_and_folds_the_help() {
        let usage = lines(&["Incorrect Usage: flag provided but not defined: -x", ""]);
        assert_eq!(View::Help.apply(usage.clone()), usage);
        let help = lines(&[
            "About",
            "",
            "Options:",
            "  -h, --help",
            "          show help",
        ]);
        assert_eq!(View::Help.apply(help), ["<HELP>"]);
        let go = lines(&[
            "NAME:",
            "   x - y",
            "",
            "GLOBAL OPTIONS:",
            "   --help, -h  show help",
        ]);
        assert_eq!(View::Help.apply(go), ["<HELP>"]);
    }

    #[test]
    fn the_version_view_hides_only_the_first_line() {
        assert_eq!(
            View::Version.apply(lines(&["librespeed-cli v1 (built on x)", "https://x"])),
            ["<NAME AND VERSION>", "https://x"]
        );
    }
}
