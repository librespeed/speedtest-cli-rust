//! Small helpers shared across modules.

use std::time::Duration;

use url::Url;

/// Port of Go's `path.Clean`.
fn path_clean(p: &str) -> String {
    if p.is_empty() {
        return ".".to_string();
    }
    let rooted = p.starts_with('/');
    let mut out: Vec<&str> = Vec::new();

    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => match out.last() {
                Some(&last) if last != ".." => {
                    out.pop();
                }
                _ => {
                    if !rooted {
                        out.push("..");
                    }
                }
            },
            s => out.push(s),
        }
    }

    let joined = out.join("/");
    match (rooted, joined.is_empty()) {
        (true, true) => "/".to_string(),
        (true, false) => format!("/{joined}"),
        (false, true) => ".".to_string(),
        (false, false) => joined,
    }
}

/// Port of Go's `path.Join`.
pub fn path_join(a: &str, b: &str) -> String {
    let joined = match (a.is_empty(), b.is_empty()) {
        (true, true) => return String::new(),
        (true, false) => b.to_string(),
        (false, true) => a.to_string(),
        (false, false) => format!("{a}/{b}"),
    };
    path_clean(&joined)
}

/// Appends `sub` to the URL's path using Go's `path.Join` semantics.
///
/// Go's `url.URL.String()` inserts a leading slash when the path is relative and
/// a host is present, so a server URL without a path still yields `/empty.php`.
pub fn url_join_path(base: &Url, sub: &str) -> Url {
    let mut u = base.clone();
    let joined = path_join(base.path(), sub);
    let joined = if joined.is_empty() {
        String::new()
    } else if joined.starts_with('/') {
        joined
    } else {
        format!("/{joined}")
    };
    u.set_path(&joined);
    u
}

/// Returns the average of a slice, or 0.0 when empty.
pub fn avg(vals: &[f64]) -> f64 {
    if vals.is_empty() {
        return 0.0;
    }
    vals.iter().sum::<f64>() / vals.len() as f64
}

/// Returns the population standard deviation of a slice, or 0.0 when empty.
///
/// Population rather than sample: these are all the probes that came back, not
/// a sample drawn from a larger set, which is also what pro-bing reports on the
/// Go side so the two clients agree.
pub fn stddev(vals: &[f64]) -> f64 {
    if vals.is_empty() {
        return 0.0;
    }
    let mean = avg(vals);
    let variance = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / vals.len() as f64;
    variance.sqrt()
}

/// Rounds to two decimal places, matching the Go version's `math.Round(x*100)/100`.
pub fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// Renders a duration the way Go's `time.Duration.String()` does, for the
/// telemetry log and the --debug phase timings.
pub fn go_duration(d: Duration) -> String {
    fn trim(s: String) -> String {
        if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            s
        }
    }

    let secs = d.as_secs_f64();
    if secs >= 1.0 {
        format!("{}s", trim(format!("{secs:.9}")))
    } else if secs >= 1e-3 {
        format!("{}ms", trim(format!("{:.6}", secs * 1e3)))
    } else if secs >= 1e-6 {
        format!("{}µs", trim(format!("{:.3}", secs * 1e6)))
    } else {
        format!("{}ns", d.as_nanos())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stddev_of_an_empty_slice_is_zero() {
        assert_eq!(stddev(&[]), 0.0);
    }

    #[test]
    fn stddev_of_identical_values_is_zero() {
        assert_eq!(stddev(&[5.0, 5.0, 5.0]), 0.0);
    }

    #[test]
    fn stddev_is_the_population_figure() {
        // Population sd of 2,4,4,4,5,5,7,9 is exactly 2; the sample sd would be
        // about 2.138, so this pins which of the two is computed.
        let vals = [2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0];
        assert!((stddev(&vals) - 2.0).abs() < 1e-12, "got {}", stddev(&vals));
    }

    #[test]
    fn stddev_of_one_value_is_zero() {
        assert_eq!(stddev(&[42.0]), 0.0);
    }

    #[test]
    fn path_join_matches_go() {
        assert_eq!(path_join("", "empty.php"), "empty.php");
        assert_eq!(path_join("/backend", "empty.php"), "/backend/empty.php");
        assert_eq!(path_join("/backend/", "/empty.php"), "/backend/empty.php");
        assert_eq!(path_join("/a/b", "../c"), "/a/c");
        assert_eq!(path_join("", ""), "");
        assert_eq!(path_join("/", ""), "/");
    }

    #[test]
    fn url_join_adds_leading_slash() {
        let base = Url::parse("https://example.com").unwrap();
        assert_eq!(
            url_join_path(&base, "empty.php").as_str(),
            "https://example.com/empty.php"
        );
        let base = Url::parse("https://example.com/backend").unwrap();
        assert_eq!(
            url_join_path(&base, "garbage.php").as_str(),
            "https://example.com/backend/garbage.php"
        );
    }

    #[test]
    fn round2_matches_go() {
        assert_eq!(round2(199.804), 199.8);
        assert_eq!(round2(5.455), 5.46);
    }

    #[test]
    fn go_duration_formats_like_go() {
        assert_eq!(go_duration(Duration::from_millis(1500)), "1.5s");
        assert_eq!(go_duration(Duration::from_micros(1500)), "1.5ms");
    }
}
