# Releasing

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

## How to release

A release is a tag. Bump `version` in `Cargo.toml`, run `cargo update --workspace`
so that `Cargo.lock` follows, commit both, then:

```sh
git tag -a v1.2.1 -m "librespeed-cli 1.2.1"
git push origin v1.2.1
```

The tag starts [release.yml](.github/workflows/release.yml), which runs in
one chain and stops at the first step that fails:

1. **Resolve the commit** turns the tag into a commit, once. Every step
   after it is handed that commit instead of resolving the tag again, so a
   tag that moves mid-release cannot split the run across two of them.
2. **Checks** run [ci.yml](.github/workflows/ci.yml) against that commit:
   tests on three platforms, both TLS backends, the minimum supported Rust
   version, lint, the fuzz targets, the differential test against the Go
   client and `cargo package`, and a tag with a known advisory in its
   `Cargo.lock` stops too. Nothing is built or published until they pass.
3. **Package the crate** verifies the tag against `version` in `Cargo.toml`
   and builds the `.crate` once, recording its SHA-256. A tag that disagrees
   stops the run here, before a release exists to leave behind.
4. **Build** cross-builds the musl binaries.
5. **Publish release** attaches the tarballs and `SHA256SUMS` to a GitHub
   release.
6. **Publish to crates.io** checks the tag again against the commit it is
   about to upload, rebuilds the crate and refuses to publish unless it comes
   out byte for byte the one step 3 validated. `cargo` cannot upload a
   `.crate` it did not just build, so this comparison is what ties the two
   together. A dry run goes first, so a crate that cannot be published is
   distinguishable from an upload that failed. Only then does the job ask
   crates.io for a publishing token, through Trusted Publishing.

Running the workflow by hand rebuilds the binaries for a tag that is already
out; that path never publishes.

The tag is checked again before the GitHub release is cut and once more
immediately before the upload, because a run takes long enough for a tag to
move. That is detection, not a lock: a tag can still move a moment after a
check returns. Protecting `v*` tags against force-pushes and deletion with a
repository ruleset is what prevents it, and these checks catch what slips past.

Releases are built with the compiler named in `RELEASE_TOOLCHAIN`, with
`cross` pinned to a version, and with every action pinned to a commit. CI
keeps using whatever `stable` is that day: the two answer different
questions, CI that the code has not fallen behind the compiler, a release
what it was actually built with. Moving a release to a newer compiler is
therefore a commit, not a Tuesday.

That makes a release repeatable in the ways this repository controls: the
same source, the same dependency versions from `Cargo.lock`, the same
compiler, the same `cross` and the same actions. It does not make the
binaries bit-for-bit reproducible — the `cross` container images and the
runner environment are outside it. `SOURCE_DATE_EPOCH` only fixes the build
date `--version` prints.

## crates.io credentials

There are none. The publish job authenticates with
[Trusted Publishing](https://crates.io/docs/trusted-publishing): it asks
GitHub for an OIDC token naming the repository and the workflow file, and
`rust-lang/crates-io-auth-action` trades that with crates.io for a registry
token that lasts 30 minutes and is revoked when the job ends. That is what
`id-token: write` on the job is for; it grants nothing in this repository.

The crate side is one-time setup, and only an owner of the crate can do it,
under the crate's Settings → Trusted Publishing → Add, publisher GitHub:

| Field | Value |
| --- | --- |
| Repository owner | `librespeed` |
| Repository name | `speedtest-cli-rust` |
| Workflow filename | `release.yml` |
| Environment name | leave empty |

The environment field is optional, and left empty it mints a token for any
run of that workflow on this repository. Naming one would narrow that
further, but only an environment with protection rules is worth the second
place to keep in step, and configuring those needs repository admin.

Trusted Publishing cannot be configured for a crate that does not exist yet,
so the first version went out with an API token from
<https://crates.io/settings/tokens>, stored as the repository secret
`CARGO_REGISTRY_TOKEN`. Nothing reads that secret any more and it can be
deleted.
