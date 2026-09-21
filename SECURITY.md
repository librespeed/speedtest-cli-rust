# Security Policy

## Reporting a vulnerability

Report suspected vulnerabilities through GitHub's [private vulnerability
reporting](https://github.com/librespeed/speedtest-cli-rust/security/advisories/new).
Please do not open a public issue for anything exploitable.

Include what you did, what happened, and what you expected. A reproducer —
a server response, a server list, a command line — is worth more than a
description. Expect a first reply within a week.

## What this program trusts

It is a speed test client, so it talks to servers it does not control, and it
is often run unattended from a router. The threat model follows from that.

### Untrusted by design

**Everything a speed test server sends.** The server list, its entries, and
every response from a backend are attacker-influenced: `--server-json` takes a
URL from the user, and the list it returns names further hosts. Accordingly:

- Buffered responses are capped (8 MiB for the server list and control
  endpoints, 64 KiB for telemetry). Transfer bodies are streamed and never
  buffered. Server lists read from a file or stdin are capped the same way.
- Server names, sponsor strings and the getIP response are stripped of C0,
  DEL and C1 control characters before display, so a server cannot drive the
  terminal with escape sequences or forge an extra `--list` entry with an
  embedded newline.
- CSV fields that come from a server are prefixed with `'` when they start
  with `=`, `+`, `-`, `@`, TAB or CR, so opening a report in a spreadsheet
  cannot execute a formula.
- Redirects may not downgrade https to http, and a 307/308 that would replay
  a POST body to a different origin is refused — the telemetry POST carries
  the measurement, the client's IP and ISP details.

**Command line values.** Numeric options are bounded at parse time, so no
input can drive an allocation or a test duration out of range.

### Trusted

**The user.** `--server-json`, `--local-json`, `--ca-cert` and `--source` all
do what they say, including reaching hosts on the local network. If you wrap
this program in a service that passes user input to `--server-json`, you have
built an SSRF primitive; validate the URL yourself.

**The system trust store**, unless you override it with `--ca-cert` or
disable verification with `--skip-cert-verify`. The latter accepts any
certificate and makes the connection trivially interceptable; it exists for
self-signed test backends and should not be used against anything else.

### Privacy

No telemetry is sent unless you ask for it. `--share`, `--telemetry-level` and
the other `--telemetry-*` options upload your measurement, your public IP and
your ISP details to a telemetry server — by default `librespeed.org`, or
whichever server you point them at — and return a public link to the result.

## Supported versions

The latest release. Fixes are not backported.

## Dependencies

CI runs `cargo audit` against the RustSec database on every dependency change
and weekly, and `cargo deny` for advisories, licences, sources and duplicate
crates. Dependabot proposes updates for cargo and GitHub Actions.
