# librespeed-cli (Rust)

A command line interface for [LibreSpeed](https://github.com/librespeed/speedtest), written in Rust.

LibreSpeed ships two backend implementations — one in Go
([speedtest-go](https://github.com/librespeed/speedtest-go)) and one in Rust
([speedtest-rust](https://github.com/librespeed/speedtest-rust)) — but the CLI
existed only in Go. This is a port of
[librespeed/speedtest-cli](https://github.com/librespeed/speedtest-cli) to Rust,
addressing [issue #105](https://github.com/librespeed/speedtest-cli/issues/105).

It speaks the same protocol as the Go CLI and works against any LibreSpeed
backend, Go or Rust.

## Status

Feature-complete with the Go CLI: every command line flag is implemented,
including telemetry/`--share`, JSON and CSV reports, ICMP and HTTP ping,
IPv4/IPv6 forcing, source-address, interface and firewall-mark socket binding,
custom CA bundles, and the server list filters.

Agreement with the Go client is a test rather than a claim. `tests/parity`
runs both binaries against the same in-process backend and compares their
output, their error output and their exit status. For every case it declares
either that the two agree or exactly which lines differ and which entry of the
list below accounts for them, so an undocumented difference fails the run and
so does one that quietly went away. CI builds the Go client from the commit
`tests/parity/cases.rs` names and runs the comparison against it.

What it shows, once the measured numbers are set aside -- those differ between
any two runs: `--list`, `--csv-header`, `--simple`, `--csv`, `--json`,
`--json-stream` and the `--debug` lines agree, and so do the usage errors, the
error lines and the exit status of a failed run. The text columns are the
exception, because what a server says about itself is rewritten on its way
into a report; the first entry below says how. In `--help` every option the Go
client has reads the same apart from a typo corrected in `--telemetry-json`,
`--http2` is an option it does not have, and `--version` names this repository
instead of the Go one.

## Building

```sh
cargo build --release
```

The binary lands in `target/release/librespeed-cli`. Set `SOURCE_DATE_EPOCH` for
a reproducible build date in `--version`.

### TLS backends

| Feature | Crypto | System dependency | Architectures |
| --- | --- | --- | --- |
| `rustls-tls` (default) | rustls + ring | none, statically linked | anything |
| `native-tls` | system OpenSSL | libopenssl | anything OpenSSL builds for |
| `vendored-openssl` | OpenSSL built from source | none, statically linked | anything OpenSSL builds for |

`ring` builds everywhere, falling back to portable C where it has no
hand-written assembly, which is everything outside x86, x86_64, aarch64 and
arm. On a core with no crypto instructions that costs throughput, so the
OpenSSL backend is the better choice there. Measured on the e500v2 in a
Turris 1.x, 8 KiB buffers:

| | ring | OpenSSL |
| --- | --- | --- |
| ChaCha20-Poly1305 | 48.6 MB/s | 46.9 MB/s |
| AES-128-GCM | 8.5 MB/s | 18.8 MB/s |

ChaCha20 in portable C already matches the assembly; AES does not, and AES is
what servers pick. An HTTPS run against a backend negotiating AES-256-GCM
measured 138 Mbps with OpenSSL against 37 Mbps with rustls. Build the OpenSSL
backend for such targets:

```sh
cargo build --release --no-default-features --features native-tls
```

### OpenWrt and Turris

The Go CLI cannot run on 32-bit PowerPC at all — Go's toolchain only targets
`ppc64` and `ppc64le`, which is why LibreSpeed's CLI is packaged for Turris
Omnia and MOX but not for Turris 1.x. Rust does reach that hardware through the
Tier 3 `powerpc-unknown-linux-muslspe` target that the OpenWrt build system
supports, so this port can go where the Go one cannot.

Build it against the OpenWrt SDK with the OpenSSL backend, for the throughput
reason above and because it links against the libopenssl already in the image
rather than carrying its own:

```sh
cargo build --release --no-default-features --features native-tls \
  --target powerpc-unknown-linux-muslspe
```

Being a Tier 3 target, `powerpc-unknown-linux-muslspe` has no prebuilt `std`, so
it needs a nightly toolchain with `-Z build-std` — which is what OpenWrt's Rust
packaging already arranges. Point the `openssl` crate at the SDK's OpenSSL via
`OPENSSL_DIR`, or let `pkg-config` find it through the SDK environment.

## Usage

```
librespeed-cli [OPTIONS]
```

Run `librespeed-cli --help` for the full list. The common ones:

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

## Differences from the Go implementation

The protocol, the flags and the output formats match. These behaviours are
deliberately different:

- **Everything a server says is sanitized.** Server names, sponsor strings,
  addresses and the backend's getIP answer are attacker-influenced text. The
  control characters (C0, DEL, C1), the line and paragraph separators, every
  Unicode format character — bidi overrides, zero-width joiners, the soft
  hyphen, the Arabic letter mark, the byte order mark — and the invisible tag
  block are dropped before any of it is printed, in terminal output, in CSV and
  in JSON alike. Spacing characters such as no-break space are kept: they
  render, and dropping them would run words together and misreport a name. The
  Go client cleans C0, DEL and C1 from its terminal output only, so its CSV and
  JSON carry escape sequences and bidi overrides straight through. A text
  column here is therefore a transformation of what the server sent rather than
  an escaping of it: the characters are removed, not encoded, and in CSV a
  field that would start a formula gains a leading apostrophe as well. Neither
  is reversible, which is the point -- nothing downstream can be steered by
  what a server called itself.
- **CSV formulas are defused.** A field starting with `=`, `+`, `-` or `@` is
  prefixed with a single quote, because spreadsheets strip the CSV quoting and
  then evaluate what is left.
- **CSV quoting follows the csv crate.** Go also quotes a field that starts
  with a space or is exactly `\.`; here only the delimiter, a quote or a line
  break causes quoting.
- **Response sizes are capped.** A server list and a getIP answer are read to
  8 MiB and a telemetry reply to 64 KiB, rather than to whatever the peer
  decides to send, and the check that a backend is up reads no more than 8 KiB
  of its answer. The cap bounds memory and not time: a peer that stays under it
  while sending slowly is left to the request timeout, and `--timeout 0` leaves
  no time bound at all.
- **A redirect may not leave https for http.** A server reached over https
  that answers with a `Location` on http is an error: the request was made
  over TLS deliberately, and following the downgrade would put the rest of
  the exchange on the wire in clear. Go follows it, leaving out only the
  Referer. A redirect that stays on http, and one that stays on https, is
  followed as it is in Go.
- **A request body is not sent on to another origin.** A 307 or 308 that would
  send the telemetry POST to another scheme, host or port is refused, because
  the POST carries the measurement, the client's address and its ISP; Go sends
  the body on. A redirected request also carries no Referer.
- **A telemetry reply other than 2xx fails the upload.** The error names the
  HTTP status, and no share link is printed. Go does not check the status: it
  takes a share ID from any body with exactly one space in it, so a 500 reply
  can still yield a share link.
- **HTTP/1.1 by default, HTTP/2 behind `--http2`.** HTTP/2 carries every stream
  over one TCP connection, so `--concurrent` would stop meaning concurrent
  connections — and multiple connections is the standard way a speed test
  saturates a link. Go's client negotiates h2 whenever a server offers it.
  `--http2` enables it here too, with the flow-control windows raised to what
  Go's transport uses (4 MiB per stream, 1 GiB per connection); the protocol
  default of 64 KiB caps a stream at window/RTT, about 105 Mbps at 5 ms.
- **Scheme-less server URLs with a port work.** `example.com/backend` becomes
  `http://example.com/backend` in both clients, but Go's URL parser takes
  `example.com:8080/backend` for a URL whose scheme is `example.com`, and
  `127.0.0.1:8080/` makes it reject the whole list. Here both become `http://`
  URLs.
- **`--interface` works on macOS** via `IP_BOUND_IF`; the Go version supports
  interface binding on Linux only. `--fwmark` remains Linux-only (`SO_MARK`).
- **Numbers the Go client cannot act on are refused.** It accepts any int64 and
  then panics on `--upload-size -1` or wraps a `--duration` past the
  `time.Duration` range. Here a negative value, a duration or timeout past that
  range, and a count, size or mark past 32 bits are refused as out of range;
  Go instead gives a negative `--concurrent` its own message and ignores a
  negative `--fwmark`. A negative `--server` or `--exclude` is still taken, so
  `--server -1` tests every server as it does in Go. Zero `--duration`,
  `--chunks` and `--upload-size` run as they do in Go.
- **TLS verification is stricter.** rustls rejects a self-signed certificate
  presented as both the leaf and its own trust anchor even when passed via
  `--ca-cert`. A normal private CA works; use `--skip-cert-verify` for the
  degenerate case.
- **The process exits rather than returning from `main`.** Dropping the tokio
  runtime joins its worker threads, and on 32-bit PowerPC musl (Turris 1.x)
  that never completed: every successful run hung after printing its output.
  `process::exit(0)` at the end of `main` is a workaround, still under
  investigation; all output is flushed as it is written, so nothing is lost.
- **Go's command line quirks are not copied.** The command line is parsed by
  clap, so:
  - `-json` is not `--json`: long options take two dashes.
  - Numbers are decimal. Go reads `010` as 8 and accepts `0x10` and `1_000`.
  - An argument that is not an option is refused. Go ignores it, and every
    option after it.
  - An option other than `--server` and `--exclude` given twice is refused,
    and `--server 1,2` is not split into two servers; Go keeps the last value
    and splits the list.
  - A boolean option takes no value, so `--json=false` is refused.
  - `--csv-delimiter` must be one ASCII character other than `"`. Go uses the
    first character of a longer value, and with `"` prints no header and no
    rows.
  - `--help` is answered as soon as it is seen, even if an unknown option
    follows; Go reports the unknown option.
  - Rarer usage errors read differently: a repeated option or `--ipv4 -4`,
    `---list`, `--json-stream` with `--json` or `--csv`, and `--server` with
    `--exclude`, which Go reports as "incompatible options" and "either
    --exclude or --server can be used" (`--specific` for a local list).
  - A malformed or out-of-range `--server` or `--exclude` value ends in
    `parse error` or `value out of range`, as any other number does; Go puts
    strconv's message there, such as `strconv.ParseInt: parsing "x": invalid
    syntax`.
- **Error causes use this client's libraries' words.** The prefixes match Go
  (`Error when fetching server list:`, `Terminated due to error:` and the
  rest), but a cause such as a refused connection or a JSON syntax error reads
  differently after them.
- **A server URL's scheme keeps its case.** Go lowercases `HTTP://` when it
  parses the list; `--list` and the reports show it here as the list gave it.

## Testing

```sh
cargo test          # unit + end-to-end tests
cargo clippy --all-targets
cargo fmt --check
```

`tests/integration.rs` starts an in-process LibreSpeed backend and drives the
built binary against it, covering the whole flow — server list, ping, download,
upload, telemetry and every output mode — with no network access, including the
Go client's error messages and what a backend full of hostile text does not get
to print. The unit tests cover the jitter estimator, Go-compatible path joining,
rounding, timestamps, CSV and JSON rendering, server list filtering and URL
scheme handling.

`tests/golden.rs` checks the report rendering against data Go itself wrote.
`tools/golden/main.go` runs `encoding/json`, `strconv` and `time` — the
packages the Go client renders its reports with — over a corpus of floats,
timestamps and whole documents, and the test feeds the same inputs to this
client. Only the recorded data is needed, so it runs everywhere; regenerating
it needs Go:

```sh
go run tools/golden/main.go tests/data
```

`tests/parity` is the differential test the Status section describes. Without
`LIBRESPEED_GO_BIN` it skips the comparison and prints why, so a plain
`cargo test` still passes; CI sets it, and by hand it is:

```sh
# GO_COMMIT is the commit tests/parity/cases.rs names.
git clone https://github.com/librespeed/speedtest-cli go-client
git -C go-client checkout "$GO_COMMIT"
(cd go-client && go build -o librespeed-go .)
LIBRESPEED_GO_BIN=$PWD/go-client/librespeed-go cargo test --test parity
```

## License

GNU Lesser General Public License v3.0, the same as the Go implementation. See
[LICENSE](LICENSE).

- LibreSpeed — Copyright (C) 2016-2020 Federico Dossena
- librespeed-cli — Copyright (C) 2020 Maddie Zhan
