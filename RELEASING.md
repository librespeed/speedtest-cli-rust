# Releasing

How to cut a release is in the [README](README.md#how-to-release).

## When to release

A release publishes to crates.io and cannot be taken back, so it stays a tag
pushed by a person. Merging a dependency update does not call for one.

**A normal release** goes out when master has a change a user can see, meaning
behaviour, options, output or exit status, and at most once a month. Without such
a change there is no release, however long that takes. A serious bug fix can go
out sooner.

**A security release** goes out within a week of Audit reporting an advisory that
reaches a build we ship. The Audit workflow checks master and the latest release
every week, and a red run needs a decision. If only master is red, nothing shipped
is affected, so fix it before the next release. For an advisory in the latest
release:

1. Is the crate in a build we ship? At that tag, run
   `cargo tree -e normal --target all -i <crate>` with the flags of each build.
   `did not match` or `nothing to print` means no, and `multiple packages` means
   try again with `<crate>@<version>`. `--target all` also lists dependencies of
   other platforms, so read what it prints.

   | Build | Flags |
   | --- | --- |
   | OpenWrt package | `--no-default-features --features native-tls` |
   | GitHub release tarballs and crates.io | none, the default is `rustls-tls` |

2. Does this program reach the affected code? Whatever the advisory requires, be
   it a crate feature, an API or a configuration, has to hold for how it uses the
   crate.

If either answer is no, add the advisory to `ignore` in `deny.toml` with a
`reason`. The fix arrives with a monthly update and goes out with the next
release, and the entry can go after that.

If both are yes, move to the fixed version with `cargo update -p <crate>` unless a
pull request already does, and cut a release. If there is no fixed version yet,
look for a way to avoid the affected code, tell users, and add the advisory to
`ignore` with that as the `reason`, so that Audit can still report newer ones.

Run Audit by hand after tagging, or it stays red until the next Monday. A yanked
crate has no advisory ID, so its entry is `{ crate = "<crate>@<version>", reason =
"..." }`.

## OpenWrt

A release does not go to OpenWrt by itself. A new version there means writes on
flash that often cannot be replaced, on every device that updates, so it needs a
reason of its own:

- master gets a release with a change OpenWrt users want.
- A stable branch gets only a fix that reaches the OpenWrt build, or a bug that
  hits its users.
- A fix that concerns only rustls does not go there at all, because that build
  does not link it. OpenSSL fixes reach OpenWrt through its own `libopenssl`,
  without touching this package.

## Dependency updates

Once a month Dependabot opens one pull request for `Cargo.lock` and one for GitHub
Actions. Merge them when CI is green, and a red advisories job is fine if it names
something the pull request does not touch. There is no auto-merge on purpose: what
a merge brings in runs at the next tag in the publish job, which holds `id-token:
write`, so a person looks first.

A security update comes as soon as GitHub has an alert, if the fix fits the lock
file. An alert without a pull request means the fix needs a `Cargo.toml` edit,
which is a job for a person. Before a release, `cargo update --dry-run --verbose`
shows which crates have such a version. Do not raise `rust-version` above the Rust
in OpenWrt's oldest stable branch, or a fix can no longer reach it.

Keep merging the monthly updates. Without activity GitHub disables the scheduled
Audit run after 60 days, and a Dependabot pull request left untouched for 90 days
pauses Dependabot, security updates included.
