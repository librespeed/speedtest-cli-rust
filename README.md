# librespeed-cli (Rust)

[![CI](https://github.com/librespeed/speedtest-cli-rust/actions/workflows/ci.yml/badge.svg)](https://github.com/librespeed/speedtest-cli-rust/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/librespeed-cli.svg)](https://crates.io/crates/librespeed-cli)
[![License](https://img.shields.io/crates/l/librespeed-cli.svg)](LICENSE)

Command line client for [LibreSpeed](https://github.com/librespeed/speedtest),
a Rust port of the Go [librespeed-cli](https://github.com/librespeed/speedtest-cli)
([issue #105](https://github.com/librespeed/speedtest-cli/issues/105)).

- Same protocol, options and output formats as the Go client.
- Works against any LibreSpeed backend, [Go](https://github.com/librespeed/speedtest-go)
  or [Rust](https://github.com/librespeed/speedtest-rust).
- Runs where Go does not, such as the 32-bit PowerPC in Turris 1.x routers.

## Install

| Where | How |
| --- | --- |
| OpenWrt | `apk add librespeed-cli-rust`, on 24.10 `opkg install librespeed-cli-rust` |
| Cargo | `cargo install --locked librespeed-cli` |
| Linux binaries | [Releases](https://github.com/librespeed/speedtest-cli-rust/releases) for x86_64, aarch64, armv7 and arm (musl) |
| From source | `cargo build --release`, the binary is `target/release/librespeed-cli` |

Set `SOURCE_DATE_EPOCH` for a reproducible build date in `--version`. It
fixes only the date and does not make the binary bit-for-bit reproducible, see
[RELEASING.md](RELEASING.md#how-to-release).

## Usage

```sh
# Test against the automatically selected fastest server
librespeed-cli

# Machine-readable output
librespeed-cli --json
librespeed-cli --csv --csv-header

# Pick servers explicitly
librespeed-cli --list
librespeed-cli --server 51 --server 94

# Use your own backend
librespeed-cli --server-json https://example.com/servers.json
librespeed-cli --local-json ./servers.json
cat servers.json | librespeed-cli --local-json -

# Bind the test to a specific egress path
librespeed-cli --source 192.0.2.10
librespeed-cli --interface eth1
librespeed-cli --fwmark 42          # Linux only
```

> [!TIP]
> `librespeed-cli --help` lists every option.

## TLS backends

| Feature | Crypto | Links against | Architectures |
| --- | --- | --- | --- |
| `rustls-tls` (default) | rustls + ring | nothing, static | anything, with assembly on x86, x86_64, aarch64 and arm |
| `native-tls` | system OpenSSL | libopenssl | anything OpenSSL builds for |

```sh
cargo build --release --no-default-features --features native-tls
```

> [!IMPORTANT]
> `ring` has assembly only for x86, x86_64, aarch64 and arm. Elsewhere it runs
> portable C. On a slow core without crypto instructions that can cost enough
> throughput for a speed test to measure the CPU instead of the link, and the
> OpenSSL backend is the better choice there.
>
> One measurement, on the e500v2 of a Turris 1.x: an HTTPS run against a
> backend negotiating AES-256-GCM reached 138 Mbps with OpenSSL and 37 Mbps
> with rustls. AES-128-GCM alone, with 8 KiB buffers, does 18.8 MB/s with
> OpenSSL and 8.5 MB/s with ring. Other CPUs will give other numbers.

## OpenWrt and Turris

The Go client cannot run on 32-bit PowerPC, because Go targets only `ppc64` and
`ppc64le`. Rust reaches that hardware through the Tier 3 target
`powerpc-unknown-linux-muslspe`, so this port also covers Turris 1.x.

The OpenWrt package `librespeed-cli-rust` builds the `native-tls` backend and
depends on `libopenssl`.

To build for Turris 1.x by hand:

```sh
cargo build --release --no-default-features --features native-tls \
  --target powerpc-unknown-linux-muslspe
```

A Tier 3 target has no prebuilt `std`, so this needs a nightly toolchain with
`-Z build-std`, which OpenWrt's Rust packaging arranges. Point the `openssl`
crate at the SDK's OpenSSL with `OPENSSL_DIR`, or let `pkg-config` find it
through the SDK environment.

## Parity with the Go client

Every option of the Go client is implemented, including telemetry and `--share`,
JSON and CSV reports, ICMP and HTTP ping, IPv4 and IPv6 forcing, source address,
interface and firewall mark binding, custom CA bundles and the server list
filters.

> [!NOTE]
> Agreement is a test, not a claim. `tests/parity` runs both binaries against
> the same backend and compares their output, error output and exit status.
> A difference that is not in the list below fails CI, and so does a listed one
> that went away.

| Compared | Result |
| --- | --- |
| Reports and errors | Same: `--list`, `--csv-header`, `--simple`, `--csv`, `--json`, `--json-stream`, the `--debug` lines, usage errors, error lines and the exit status of a failed run |
| `--help` | Every shared option reads the same apart from a typo corrected in `--telemetry-json` |
| Only here | `--http2`, and `--version` names this repository instead of the Go one |

## Differences from the Go implementation

The protocol, the options and the output formats match. These behaviours are
deliberately different.

### Security

- **Everything a server says is sanitized.** Server names, sponsors, addresses
  and the getIP answer lose their control characters, Unicode format characters
  such as bidi overrides and zero-width joiners, and the invisible tag block,
  in terminal, CSV and JSON output alike. The characters are removed, not
  escaped. Go cleans only C0, DEL and C1, and only in terminal output.
- **CSV formulas are defused.** A field starting with `=`, `+`, `-` or `@` gets
  a leading single quote, because spreadsheets evaluate what is left after
  unquoting.
- **Response sizes are capped.** 8 MiB for a server list or a getIP answer,
  64 KiB for a telemetry reply and 8 KiB for the check that a backend is up.
  The cap bounds memory, not time, and `--timeout 0` leaves no time bound.
- **A redirect may not leave https for http.** It is an error here. Go follows
  it and only leaves out the Referer.
- **A request body is not sent on to another origin.** A 307 or 308 that would
  move the telemetry POST to another scheme, host or port is refused, because
  the POST carries the measurement, the client's address and its ISP. Go sends
  it on. A redirected request also carries no Referer.
- **A telemetry reply other than 2xx fails the upload.** The error names the
  HTTP status and no share link is printed. Go takes a share ID from any body
  with exactly one space in it, so a 500 reply can still yield a link.
- **TLS verification is stricter.** rustls rejects a self-signed certificate
  that is both the leaf and its own trust anchor, even with `--ca-cert`.
  A normal private CA works, and `--skip-cert-verify` covers the other case.

### Behaviour

- **HTTP/1.1 by default, HTTP/2 behind `--http2`.** HTTP/2 carries every stream
  over one TCP connection, so `--concurrent` would stop meaning connections.
  Go negotiates h2 whenever a server offers it. With `--http2` the flow-control
  windows are raised to Go's values, 4 MiB per stream and 1 GiB per connection.
- **Scheme-less server URLs with a port work.** `example.com:8080/backend`
  becomes an `http://` URL. Go takes `example.com` for the scheme, and
  `127.0.0.1:8080/` makes it reject the whole list.
- **`--interface` works on macOS** through `IP_BOUND_IF`. Go binds interfaces
  on Linux only. `--fwmark` stays Linux-only (`SO_MARK`).
- **Numbers the Go client cannot act on are refused.** A negative value,
  a duration or timeout past the `time.Duration` range, and a count, size or
  mark past 32 bits are out of range. Go panics on `--upload-size -1`.
  A negative `--server` or `--exclude` is still taken, so `--server -1` tests
  every server as it does in Go.
- **CSV quoting follows the csv crate.** Only the delimiter, a quote or a line
  break causes quoting. Go also quotes a field that starts with a space or is
  exactly `\.`.
- **Error causes use this client's libraries' words.** The prefixes match Go,
  such as `Error when fetching server list:`. The cause after them, a refused
  connection or a JSON syntax error, reads differently.
- **A server URL's scheme keeps its case.** Go lowercases `HTTP://` when it
  parses the list.
- **The process exits rather than returning from `main`.** On 32-bit PowerPC
  musl, dropping the tokio runtime never finished and every run hung after its
  output. `main` ends with `process::exit(0)` as a workaround. Output is flushed
  as it is written, so nothing is lost.

### Command line

- **Go's command line quirks are not copied.** The command line is parsed by
  clap, so:
  - `-json` is not `--json`: long options take two dashes.
  - Numbers are decimal. Go reads `010` as 8 and accepts `0x10` and `1_000`.
  - An argument that is not an option is refused. Go ignores it, and every
    option after it.
  - An option other than `--server` and `--exclude` given twice is refused,
    and `--server 1,2` is not split into two servers. Go keeps the last value
    and splits the list.
  - A boolean option takes no value, so `--json=false` is refused.
  - `--csv-delimiter` must be one ASCII character other than `"`. Go uses the
    first character of a longer value.
  - `--help` is answered as soon as it is seen, even if an unknown option
    follows. Go reports the unknown option.
  - Rarer usage errors read differently: a repeated option, `--ipv4 -4`,
    `---list`, `--json-stream` with `--json` or `--csv`, and `--server` with
    `--exclude`.
  - A malformed or out-of-range `--server` or `--exclude` value ends in
    `parse error` or `value out of range`. Go prints strconv's message.

## Testing

```sh
cargo test
cargo clippy --all-targets
cargo fmt --check
```

| Suite | What it covers |
| --- | --- |
| `tests/integration.rs` | The whole flow against an in-process backend, with no network: server list, ping, download, upload, telemetry and every output mode |
| `tests/golden.rs` | Report rendering against data the Go standard library wrote |
| `tests/parity` | The differential test against the Go client |
| unit tests | Jitter, path joining, rounding, timestamps, CSV and JSON rendering, server list filters and URL schemes |

<details>
<summary>Running the parity test by hand</summary>

Without `LIBRESPEED_GO_BIN` the comparison is skipped and says why, so a plain
`cargo test` passes. CI sets it.

```sh
# GO_COMMIT is the commit tests/parity/cases.rs names.
git clone https://github.com/librespeed/speedtest-cli go-client
git -C go-client checkout "$GO_COMMIT"
(cd go-client && go build -o librespeed-go .)
LIBRESPEED_GO_BIN=$PWD/go-client/librespeed-go cargo test --test parity
```

</details>

<details>
<summary>Regenerating the golden data</summary>

Only the recorded data is needed to run the test. Regenerating it needs Go:

```sh
go run tools/golden/main.go tests/data
```

</details>

## Releasing

[RELEASING.md](RELEASING.md) says when a release goes out, when one goes to
OpenWrt, how dependencies are kept up to date and how a tag becomes a release.

## License

GNU Lesser General Public License v3.0, the same as the Go implementation. See
[LICENSE](LICENSE).

- LibreSpeed, Copyright (C) 2016-2020 Federico Dossena
- librespeed-cli, Copyright (C) 2020 Maddie Zhan
