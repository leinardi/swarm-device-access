# Rust port: design and provenance record

swarm-device-access 2.0 is a Rust rewrite of the Go daemon (about 17k lines: daemon, launcher,
cgroup v1 and BPF device filters, D-Bus reload watcher). This file records why the Rust code is
the way it is: the design decisions with their reasons, the crates, the intended differences from
1.x, the reuse policy and the reuse ledger. It stays in the repository after the port as the
permanent design and provenance record.

- Go keeps shipping until one breaking cutover commit. Until then the Rust code lands beside it,
  phase by phase, and nothing Rust is built into an image.
- Behaviour stays identical to 1.x except for the numbered intended differences below. **Any
  other difference is a defect.** The Go code is the behavioural authority until the cutover.
- The [`rust-style-guide`](../.agents/skills/rust-style-guide/SKILL.md) skill holds the coding
  rules; the porting traps found along the way go into
  [`go-to-rust-port`](../.agents/skills/go-to-rust-port/SKILL.md).

## Contents

- [Design decisions](#design-decisions)
- [Crates](#crates)
- [Intended differences from 1.x (draft)](#intended-differences-from-1x-draft)
- [Reuse policy](#reuse-policy)
- [Reuse ledger](#reuse-ledger)
- [Go golden captures](#go-golden-captures)
- [Upstream candidates](#upstream-candidates)

## Design decisions

| Area | Decision | Reason |
| --- | --- | --- |
| CLI | clap 4 derive with only `--` flags, as in SSE. Before clap runs, every single-dash argument longer than one character (up to `--`, skipping a value flag's value) and every positional is refused with exit 2. Kept from 1.x: `--x=false` with every Go `ParseBool` spelling, last value wins, repeatable `--device-allow`/`--device-deny`, and "explicitly set" taken from clap's `ValueSource::CommandLine` (Go's `flag.Visit`, which counts a value equal to the default). The `launch` subcommand splits at the first `--` exactly as Go does and passes the rest verbatim. | Maintainer's choice; Q1. |
| pprof | `--debug-addr` and the `debug-addr` key still parse. An empty or absent value is accepted. A non-empty value fails at startup (exit 1, error on stderr), and on `SIGHUP` the reload is rejected as an invalid setting. | Maintainer's choice; fails closed rather than silently ignoring a listener the operator asked for. Q2. |
| Logging | SSE's in-tree slog-compatible logger, reused: the text and JSON `commonHandler` port, typed `Attr`, and SSE's `plain_handler.rs`, checked against this repository's Go plain handler and its goldens and adapted only where the two Go handlers differ. No `tracing`. A cloned `LogHandle` (a shared cell, swapped by `configure`) is passed to every component in place of Go's global `logger.L()`, which keeps `TestWatch_UsesTheCurrentLogger` meaningful without process globals. | Log lines are an operator and integration contract: the tests wait on message text. `tracing` cannot reproduce slog byte for byte (`go-to-rust-port`). |
| Errors | Hand-written error enums with a `Display` that matches Go's `%w` chains. A port of `errors.Join` joins with newlines. No `thiserror` or `anyhow`. | Error text is logged, so it is part of the contract. Follows SSE's style. |
| Docker, daemon path | In-tree client from SSE, trimmed to a unix socket only (Go used `client.New(WithHost("unix://"+socket))`, with no `FromEnv`, TCP, TLS or proxy). It keeps moby's lazy negotiation (`HEAD /_ping`, then `GET /_ping`) and offers `ping`, `container_list` (running only, no filters), `container_inspect` and `events` (`since`, plus `event` filters start, unpause, die and destroy, with no `type` filter, as in Go). The read-only contract (`GET` plus one `HEAD` in `ping`, one request builder) is enforced by a meta-test. | Parity of the Engine request sequence. Avoids bollard's pitfalls (negotiation through `/version`, a missing User-Agent). |
| Docker, launcher path | A separate module `src/launcher/engine.rs` on the same transport, with its own request builder: create (`?name=`), start, attach (HTTP upgrade hijack plus the stdcopy 8-byte frame demux), wait (`condition=removed`), stop (`t=10`), remove (`force`), and list with a `name` filter. A meta-test allows only `src/launcher/**` to name the write builder; the daemon, processor and observability code take only the read-only trait. bollard appears only in `testenv` (optional, `integration` feature). | No write call is reachable from the privileged daemon path. Same wire as moby v0.6.0, checked against captures. |
| eBPF | A raw `bpf(2)` call through `libc::syscall(SYS_bpf, …)` in one audited module, `src/cgroup/bpf_sys.rs`. It covers `PROG_QUERY` (flags 0, 64 ids, retry on `ENOSPC` up to 10 times), `PROG_GET_FD_BY_ID`, `OBJ_GET_INFO_BY_FD` (xlated instructions plus `nr_map_ids` for the map-id "known" semantics), `PROG_LOAD` (`CGROUP_DEVICE`, licence `Apache`, name), `PROG_ATTACH` (`ALLOW_MULTI`, optional `REPLACE` plus `replace_bpf_fd`) and `PROG_DETACH`. The workspace sets `unsafe_code = "deny"` (not `forbid`, which `#[expect]` cannot lift); only `bpf_sys.rs` carries `#![expect(unsafe_code, reason = …)]`, and a meta-test pins that list. Every other syscall goes through rustix (`openat2` with `RESOLVE_BENEATH` and `RESOLVE_NO_MAGICLINKS`, `fstat`, `readlinkat`, `pidfd_open`, `pidfd_send_signal`, `uname`, `setrlimit` `MEMLOCK`). | Go wraps the runtime's own filter by reading its instructions back, which needs exactly these commands. aya was rejected after reading its docs: it has no ELF-less load, no `BPF_F_REPLACE`, no access to the xlated dump of a foreign program, and its `CgroupDevice::attach` returns a link id rather than doing a plain `BPF_PROG_ATTACH`. |
| BPF instructions | A plain 8-byte instruction model (`op, dst, src, off, imm`, with `ld_imm64` decoded as 16 bytes so the reloadability gate rejects it rather than failing to decode). Jump labels are resolved in-tree. Encoding is byte-identical to cilium asm in host byte order. | Ownership is decided by regenerating the wrapper and comparing bytes, so a single differing byte would turn every Go-attached wrapper into an owned-block conflict after the upgrade. |
| D-Bus | zbus (default features off, `tokio`), in the system-bus connection order godbus uses (`DBUS_SYSTEM_BUS_ADDRESS`, else `/var/run/dbus/system_bus_socket`). The same two match rules, the signal stream registered before `GetNameOwner`, and no `Manager.Subscribe` call, as in Go. The systemd phase starts with a spike; if zbus cannot reproduce the sequence, or drags in async-io, the fallback is a minimal in-tree client (EXTERNAL auth, `AddMatch`, `GetNameOwner`, signal decoding). | Parity of the reload-completed edge, the owner tracking and the backoff. |
| YAML | `saphyr-parser` events (tags, anchors, styles, explicit document start), feeding an in-tree port of yaml.v3's `resolve()` and of the loader's two passes: a shape check (aliases followed with cycle detection, merge keys refused) and a strict decode (known fields, duplicate keys, one document). Accept and reject parity is checked against a Go corpus. If the syntax-error wording differs from yaml.v3's, it becomes a Q entry. | `serde_yaml_ng` runs on `unsafe-libyaml` and resolves tags differently; the loader depends on node tags. |
| Metrics and HTTP | SSE's in-tree registry (counter, gauge, histogram with `DefBuckets`, the Vec types), text exposition, promhttp negotiation and handler metrics, the `/proc` process collector, and the hyper server with Go's four deadlines and the request framer. This project's timeouts are 5 s, 10 s, 30 s and 60 s. Any method is accepted; `/healthz`, `/readyz` (503 `not ready`) and 404 otherwise. Readiness is an injected `Arc<AtomicBool>`, and the `Recorder` owns its registry. | Same contract as SSE. Q3 (no `go_*`) and Q4 (text format only). |
| Clock and runtime | An injected `Clock` and `clippy.toml` wall-clock bans, as in SSE; tests run on tokio's paused clock. The runtime is built by hand, multi-thread with `worker_threads` capped at 2 (RSS is measured in the field test). Blocking syscalls run in `spawn_blocking`, under one ownership rule: the cgroup and BPF mutation of one reconcile (pin, open, verify, `SetDeviceRules`, the ledger and cache updates) is a single blocking transaction. It owns the processor lock (a `std::sync::Mutex` taken inside the blocking closure), the pidfd, the `/proc`, `/dev` and cgroup descriptors and the program handles until it returns. Dropping its future is never what ends it: the coordinator always awaits the `JoinHandle` to completion, even after the caller's timeout or cancellation, and releases the container's reservation only then. Only the async Docker calls before and between the steps (inspect and re-inspect) are cancellable. Shutdown waits for in-flight transactions inside the one shutdown budget, as Go's `daemon.Run` waits for its goroutines, and then `shutdown_timeout` runs. `panic = "abort"`. | Same failure semantics as Go, where the context bounds the Docker calls and the cgroup syscalls are uninterruptible. Go has no `recover` here, so a panic crashes the process and Swarm or the launcher restarts it, whereas a Rust task panic under unwind would leave a silently stuck daemon. |
| Integration harness | `testenv` drives the **host** Docker daemon, as `test/integration` does today; it is not SSE's DinD Swarm, because enforcement needs real host cgroups. It has a Go-style `-run` runner (`harness = false`, zero matches fails), envid labels and a sweep. The Rust suite reads its own variable, `SDA_RUST_TEST_IMAGE`, never `SDA_TEST_IMAGE`, which `.mk/integration-test.mk` exports globally as the Go-built `swarm-device-access:integration`. Before the cutover, the Rust make target builds a throwaway `FROM scratch` image from the host-architecture static musl binary and sets `SDA_RUST_TEST_IMAGE` to it. Before running any test, the harness proves that the selected image is Rust: it runs the image with `-help`, which exits 0 in Go and is refused with exit 2 and the single-dash refusal text in Rust (Q1). A mismatch fails the run and never skips it. At the cutover, `SDA_RUST_TEST_IMAGE` is set to the image built from the Rust Dockerfile, and the same identity check stays. A cross-language test also uses the Go image, under its own variable `SDA_GO_TEST_IMAGE`: before the cutover its default is the Go Dockerfile build; from the cutover on, it is the last published 1.x image, pinned by digest. The same identity check, inverted, proves that this image is Go. | Keeps Go shipping until the cutover without ever letting a Go image stand in for Rust. |
| Meta-tests | SSE's policy: a meta-test never skips on a missing tool, and every rule has a negative control. This replaces `toolgate` and `SDA_REQUIRE_TOOLS` at the cutover. | A gate that can pass on nothing is no gate. |
| Licence | The project stays Apache-2.0. The Rust ports of `api.go`, `ebpf.go`, `v1.go` and `v2.go` (`src/cgroup/{api,ebpf,v1,v2}.rs`) keep NVIDIA's block verbatim, the crun and runc provenance paragraph, and a modification notice under it (Apache 2.0 §4b). The header check lists those four Rust paths as the NVIDIA exception, alongside the Go ones until the cutover. Code reused from SSE and SRD is MIT-licensed, with Roberto Leinardi as sole copyright holder. Each reused file carries this project's Apache-2.0 header, followed by the provenance comment `Adapted from <repo>/<path> at <commit>, originally MIT-licensed, Copyright (c) <year> Roberto Leinardi`. The header check accepts that comment and does not require it. Reused TOML configuration carries the same header and provenance as `#` comments, although the header check does not govern TOML. | The Apache terms. The copyright holder may relicense their own MIT code, and the provenance line keeps the origin and the original licence visible. |
| Release stamps | `option_env!("SWARM_DEVICE_ACCESS_VERSION" / "_COMMIT" / "_DATE")`, with the defaults `dev`, `none` and `unknown`. | Same as SSE. |
| Go baselines | Goldens and wire captures come from Go programs or capture tests behind a `goldencapture` build tag, never from reasoning about what Go would do. A Go release binary (`v1.0.3`) is kept for side-by-side probes. | The Rust side is checked against what Go actually does. |

## Crates

Each crate is added in the commit that first needs it, with its version pinned in `Cargo.toml`
and `Cargo.lock` and checked by `cargo deny`.

| Crate | Where | Reason |
| --- | --- | --- |
| `clap` 4 (derive, no colour) | daemon | The CLI, as in SSE (Q1). |
| `tokio` (hand-built multi-thread runtime, `signal`, `net`, `time`, `sync`) | daemon | The runtime, the sockets, the timers that follow the paused clock, `spawn_blocking` for the cgroup transaction. |
| `tokio-util` | daemon | `CancellationToken`, Go's context cancellation. |
| `hyper`, `hyper-util`, `http-body-util`, `bytes` (HTTP/1 only) | daemon | The Docker transport (SSE's client) and the metrics server (SSE's server). |
| `socket2` | daemon | The listener's dual stack, backlog and `SO_REUSEADDR`, as Go sets them (SSE). |
| `flate2` (`rust_backend`) | daemon | gzip on `/metrics`, as promhttp negotiates it (SSE); pure Rust, no C zlib. |
| `serde`, `serde_json` | daemon | The Engine API JSON, the launcher's create body and the `DeviceRule` JSON form. |
| `jiff` (`std` only) | daemon | Civil time and the local zone for the logger's timestamps (SSE's `go_compat::time_layout`, `local_zone`). |
| `rustix` | daemon, testenv | Every syscall except `bpf(2)`, without `unsafe`: `openat2`, `fstat`, `readlinkat`, `pidfd_*`, `uname`, `setrlimit`, the nonce's `getrandom`. |
| `libc` | `src/cgroup/bpf_sys.rs` only | `syscall(SYS_bpf, …)`: no safe crate offers the commands the v2 filter needs (see aya below). |
| `sha2` | daemon | The owned wrapper's digest (Go: `crypto/sha256`); pure Rust. |
| `zbus` (default features off, `tokio`) | daemon | The systemd reload watcher, unless the spike forces the in-tree fallback. |
| `saphyr-parser` | daemon | YAML events for the strict loader. |
| `regex` | testenv | The Go-style `-run` filter, whose patterns are regular expressions (SSE). |
| `bollard` (optional, `integration` feature) | testenv only | The integration harness creates and removes containers on purpose; never in the daemon package. |

Rejected:

| Crate | Reason |
| --- | --- |
| `aya` | No ELF-less load, no `BPF_F_REPLACE`, no xlated dump of a foreign program; `CgroupDevice::attach` returns a link id instead of a plain `BPF_PROG_ATTACH`. |
| `serde_yaml_ng` | Runs on `unsafe-libyaml` and resolves tags differently from yaml.v3. |
| `tracing`, `tracing-subscriber` | Cannot reproduce slog's text byte for byte. |
| `thiserror`, `anyhow` | Error text is a contract written by hand; SSE's style. |
| `bollard` in the daemon | Negotiates through `/version` and omits moby's User-Agent; the request sequence would drift. |

## Intended differences from 1.x (draft)

This is the draft of the table the README's *Migrating from 1.x* section takes over at the
cutover. Q6 and later are only differences that a phase proves unavoidable (for example the YAML
syntax-error wording); each one is added in the commit that introduces it.

| # | Area | 1.x | 2.0 |
| --- | --- | --- | --- |
| Q1 | CLI | Go's `flag`: `-flag` and `--flag` both accepted; the daemon ignores positional arguments (`launch` refuses them) | Only `--flag`. A single-dash flag or a positional argument is refused with exit 2; `-h`/`--help` print to stdout and exit 0. This also applies to the daemon arguments after `launch --`. |
| Q2 | `debug-addr` / `--debug-addr` | Serves pprof | No pprof. A non-empty value is refused at startup and on reload. |
| Q3 | `/metrics` | `go_*` runtime families | No `go_*` families; `process_*` and `promhttp_metric_handler_*` are kept. |
| Q4 | `/metrics` | Protobuf or text, negotiated | Always text 0.0.4, with no protobuf negotiation. |
| Q5 | Environment | `GOMAXPROCS`, `GOMEMLIMIT`, `GOGC`, `GODEBUG` tune the runtime | These variables no longer have any effect. |

## Reuse policy

Binding for every phase of the port, and for later changes to a reused component:

- Before designing a component, read the existing Rust code and tests listed in the ledger. Copy
  and adapt them; do not rewrite them independently. Keep their structure, naming, error handling
  and dependency choices wherever this project's contract allows.
- Every reused component gets a ledger row: source path and commit, mode (unchanged, adapted or
  new), each adaptation, and the reason for every departure. The same commit brings the source's
  tests along; this project's parity and regression tests are added on top.
- No feature has two implementations unless this file records the technical reason. The
  `rust-style-guide` and `adversarial-review` skills check this.
- No shared crate is extracted during the port. Fixes worth sending back upstream go to the
  upstream-candidates list below.
- This project's Go code stays the behavioural authority. A reference implementation never
  overrides this project's logger, error or Docker-client decisions, its log and error text, its
  Engine request contract, or its security boundaries.
- Sources are pinned: swarm-scheduler-exporter (SSE) at `d57dcc220d29` and swarm-rootless-docker
  (SRD) at `16913a4d87d8`, both MIT-licensed by the same author. A later upstream commit is taken
  only by updating the ledger.
- Every reused file carries this project's Apache-2.0 header followed by the provenance line (see
  *Licence* above).

## Reuse ledger

Paths are relative to the source repository at its pinned commit. "Tests" are the source's tests
that come along with the component. The *Mode* is the target; a row is refined, with every
actual adaptation, in the commit that lands the component.

| Component | Source (SSE `d57dcc220d29`, SRD `16913a4d87d8`) | Tests | Mode | Adaptation (the contract difference) |
| --- | --- | --- | --- | --- |
| Workspace config: `[workspace.lints]`, `clippy.toml`, `rustfmt.toml`, `rust-toolchain.toml`, `deny.toml` | SSE `Cargo.toml`, `clippy.toml`, `rustfmt.toml`, `rust-toolchain.toml`, `deny.toml` | SSE `meta/tests/workspace_lints.rs` | adapted | Lint set verbatim except `unsafe_code = "deny"` (SSE: `forbid`) plus the unsafe allowlist meta-test; members `.`, `meta`, `testenv`; licence Apache-2.0; `rust-version = "1.98.1"`, the exact release of `rust-toolchain.toml` (SSE: `1.98`); `panic = "abort"` (SSE: `unwind`); `doc-valid-idents` adds `eBPF`; `deny.toml` allows Apache-2.0 and, for the meta crate's dev-only crates, MIT (cargo-deny 0.20.2 does not see dev-dependencies, so that allowance is decided by hand), and has no advisory ignore (SSE's covers a crate this workspace does not use); the reused TOML files carry this project's Apache-2.0 header as `#` comments, above the provenance line; no dependencies yet, each crate added by the commit that needs it. The root package is a library plus a placeholder binary that refuses to run and goes into no image until the cutover |
| Make fragments | SSE `.mk/rust.mk`, `.mk/verify.mk`, `.mk/audit-deps.mk`, `.mk/integration-test.mk` | SSE `meta/tests/integration_targets.rs`, `meta/tests/toolchain.rs` | adapted | `.mk/rust.mk`: SSE's targets with the `SWARM_DEVICE_ACCESS_*` stamps, without `rust-run` (the placeholder binary does nothing yet); `rust-deny` stays out of `verify-rust`, as in SSE. `.mk/verify.mk` keeps this project's `verify-go` and adds `verify` as `verify-go` plus `verify-rust` while Go exists. Still to come: the Rust side of `audit-deps.mk` and `integration-test.mk` (host Docker integration, a static host-architecture target) |
| pre-commit Rust hooks, CI `rust` job, rustfmt-on-edit hook, setup-zigbuild action, release build steps, Dockerfile, droast policy | SSE `.pre-commit-config.yaml`, `.github/workflows/ci.yaml`, `.github/workflows/release.yaml`, `.github/actions/setup-zigbuild/action.yaml`, `scripts/hooks/rustfmt-on-edit.sh`, `deployments/docker/Dockerfile`, `deployments/docker/Dockerfile.dockerignore`, `droast.toml` | SSE `meta/tests/dockerfiles.rs`, `meta/tests/toolchain.rs` | adapted | The image keeps `USER 0`, no `EXPOSE`, no `HEALTHCHECK` (no `--healthcheck`); Apache labels; this project's Trivy and cosign flow kept. Landed so far: the Rust pre-commit hooks (`cargo-fmt`, `cargo-clippy`, `cargo-test`, `cargo-deny`, `cargo-machete`) beside the Go ones, as in SSE; the rustfmt edit hook beside the gofmt one; the `rust` CI job as in SSE (with `permissions: contents: read`, which SSE leaves to the workflow default), required by `ci-result`, and Rust set up in `pre-commit-all`, which also skips `cargo-test` and `cargo-deny`. Until the meta crate takes them over, the Rust release pin and the Rust vacuous flags are checked by this project's Go toolchain meta-tests, extended with what SSE's `toolchain.rs` checks (one exact release across the toolchain file, `rust-version` and every CI step) plus Rust-specific vacuous flags SSE does not have (`--no-run`, `--cap-lints`, allowed warnings, `--skip`, `--exclude`, clippy without `-D warnings`) and a check that every CI gate is required and every skipped hook runs in a required job |
| Licence-header and suppression scripts | SSE `scripts/check-license-headers.sh`, `scripts/check-suppressions.sh` (Rust-aware, the `rs_scan` awk) | SSE `meta/tests/license_headers.rs`, `meta/tests/suppressions.rs`; their Rust cases ported into this project's Go meta-tests `test/licenseheaders` and `test/suppressions` until the meta crate takes them over | adapted | Merged into this project's scripts rather than replacing them, so the Go, shell and Dockerfile checks are unchanged. Suppressions: SSE's `rs_scan` detector verbatim (the `expect` and `allow` families, per lint, `cfg_attr` not counted); Rust fixtures under `testdata/` skipped, but only `.rs` files, where SSE skips every kind; SSE's `EXCLUDED_PREFIXES` not taken, since this script never had it. Headers: Apache text and copyright form instead of MIT; `.rs` governed everywhere except under `testdata/`; the four Rust NVIDIA ports added to the exception; new, Rust only: a provenance comment, when present, must be complete (it may wrap), and the MIT permission text is refused |
| meta crate | SSE `meta/src/{lib,repo,gitenv,dockerfile,links}.rs`; `meta/tests/{docker_boundary,dockerfiles,docs,integration_targets,license_headers,suppressions,toolchain,workspace_lints}.rs` | the tests themselves, each with its negative control | adapted | Boundary rules split into a read-only daemon client and a launcher-only write module; Go pins until the cutover. New: the unsafe allowlist, the single config publisher, the launcher write boundary. Landed so far: `lib.rs` and `repo.rs` (plus `tracked_or_new`, so a source check also sees a file nobody has added yet); `workspace_lints.rs` with its checks made functions of the manifests and negative controls added (SSE's has none); `unsafe_allowlist.rs` (new). The other helpers come with the tests that use them |
| Clock (`Clock`, `SystemClock`, `TokioClock`, `FakeClock`) | SSE `src/clock.rs` | its inline tests | unchanged | Deadlines use `tokio::time::Instant` (monotonic). Landed: verbatim but for the module comment's wording |
| Version stamps | SSE `src/version.rs` | its inline tests | adapted | The `SWARM_DEVICE_ACCESS_*` variable names |
| CLI: `refuse_single_dash`, Go `ParseBool`, clap settings, exit 2 | SSE `src/cli.rs` | SSE `src/cli/tests.rs` | adapted | This project's flags, the `launch` subcommand and its `--` split, `ValueSource` explicit-set |
| Runtime, signals, shutdown budget | SSE `src/run.rs`, `src/main.rs` | SSE `tests/run.rs` | adapted | `SIGHUP` channel registered first; signals swallowed after the first; `panic = "abort"` (SSE unwinds for its `catch_unwind`, which this project does not have) |
| go_compat | SSE `src/go_compat/{mod,quote,duration,float,time_layout,local_zone,strings,errno,http_status,host_port,parse_float,sort}.rs`, the subset the reused modules import | SSE `tests/go_compat_golden.rs`, `src/go_compat/host_port/tests.rs`, `tests/testdata/{floats,strings,time,sort}` | unchanged subset | Only the modules used. New here: `filepath.Match` and `Clean`, `errors.Join`, and `ParseBool` if the CLI does not already carry it. Landed with the logger: `quote` (with its private `unicode_tables`), `duration`, `float`, `time_layout` and `local_zone`, verbatim with their inline tests; `mod.rs` lists only those; `tests/go_compat_golden.rs` keeps the float, duration and time-layout checks with `tests/testdata/floats/go-corpus.tsv`, `tests/testdata/time/go-layouts.tsv` and SSE's `tests/testdata/logger/fuzz_numbers.txt`, moved to `tests/testdata/floats/` so `tests/testdata/logger/` holds only this project's captures. Landed with the policy: `strings` (with its private `unicode_case`), verbatim, and its check on `tests/testdata/strings/go-strings.tsv`; new here, `parse_bool` (`strconv.ParseBool` with its error text, replacing the private one of SSE's `cli.rs` when the CLI lands) and `filepath` (`filepath.Match` and `filepath.Clean`, ported from the Go 1.26 sources and checked against the policy golden) |
| Logger: mod, slog handler, plain handler, value; golden harness | SSE `src/logger/{mod,slog_handler,plain_handler,value}.rs` | SSE `src/logger/tests.rs`, `tests/logger_golden.rs`, `tests/testdata/logger` | adapted | The text and JSON handlers verbatim. The process-global logger (`global`, `set_global`, `configure`) is replaced by `LogHandle`, a cloned handle on one shared cell that `configure` (Go's `Configure`, stdout) and `set` swap, with a `with_attrs` handle for Go's `logger.L().With(...)` at the call site. `parse_level` and `parse_format` keep Go's fallbacks (info, text) instead of SSE's refusals, since the settings are validated before they reach the logger. The plain handler follows this project's Go handler, which differs from SSE's: message and keys raw (a newline included), a string quoted only for a space or a tab with only `"` escaped, an error, a stringer and a `[]string` written raw, an attribute with an empty key dropped. Two value kinds added for the Go corpus: a nil `[]string` (`null` in JSON) and a string-typed stringer. SSE's plain escaping test is not taken (this project's format does not escape); the logger's private `go_to_lower` gave way to `go_compat::strings::to_lower` when that landed, so `strings.ToLower` has one port; the harness reads `corpus.json` instead of rebuilding the records, and compares whole files, since a plain record with a raw newline spans two lines |
| Test support: `capture_logs`, `eventually`, fake Docker | SSE `src/test_support/{mod,fake_docker}.rs` | SSE `src/test_support/tests.rs` | adapted | This project's traits |
| Docker transport: client, connect, pool, error, events decoder, version negotiation, request builder | SSE `src/docker/{mod,client,connect,pool,error,events,version,api,types}.rs`; `tests/support/{mod,fake_engine}.rs` | SSE `src/docker/tests.rs`, `src/docker/{connect,events,api,types}/tests.rs`, `tests/engine_wire.rs`, `tests/testdata/engine_wire` | adapted | Unix socket only: `host.rs` and `tls.rs`, TCP, proxy and the `DOCKER_*` environment dropped. This project's endpoints and wire types; captures from this project's Go |
| Metrics: registry, families (with the histogram), text, exposition, promhttp, process collector | SSE `src/metrics/{mod,families,text,exposition,promhttp,process,testing}.rs` | SSE `src/metrics/tests.rs`, `src/metrics/{exposition,process,promhttp}/tests.rs`, `tests/exposition.rs`, `tests/testdata/exposition` | adapted | `snapshot.rs` dropped; this project's `Recorder` families; no `go_*` (Q3) |
| HTTP server: server, timeouts, framing, gopath | SSE `src/server.rs`, `src/server/{framing,gopath,timeouts}.rs` | SSE `src/server/tests.rs`, `src/server/tests/{shutdown,timeouts,wire}.rs`, `src/server/{framing,gopath}/tests.rs`, `tests/testdata/http` (Go captures) | adapted | Timeouts 5/10/30/60 s; routes `/metrics`, `/healthz`, `/readyz`; any method; no debug server (Q2) |
| Integration helpers: run filter, envid, context, error, redact, sweep rules, `harness = false` runner, child-process launcher | SSE `testenv/src/{lib,run_filter,envid,context,error,redact,sweep_rules}.rs`; `testenv/tests/integration/{main,testing,wait}.rs` | SSE `testenv/src/sweep_rules/tests.rs`, `testenv/tests/integration/harness.rs` | adapted | Host Docker. Not reused: `dind.rs`, `cluster.rs`, `images.rs`, `metrics_text.rs`. Landed so far: the crate and its `integration` feature, empty until the harness |
| Launcher: `self_id`, supervise `select!`, `Timing` budget, stop requests, attach pump to output, wait-before-start ordering, call timeouts, `DockerApi` trait shape, recording fake | SRD `src/launcher/mod.rs`, `src/docker/{mod,fake}.rs` | SRD `src/launcher/tests.rs`, `src/docker/tests.rs` | adapted | The bollard implementation is replaced by the in-tree engine on SSE's transport (this project's wire contract); `tracing` becomes the `LogHandle` and `thiserror` hand-written errors; wait condition `removed`; this project's `daemonSpec`, stale-owner rules, content-addressed image check, a 20 s budget and its log and error text. Not reused: the node lock, `src/launcher/probe.rs`, the subuid upload, the status socket |
| Events reconnect loop | SSE `src/collector/reconciler/listener.rs` (patterns only) | SSE `src/collector/reconciler/tests/listener.rs` (patterns only) | adapted | This project's backoff 1–30 s, `since` plus 1 ns, re-enumeration and ping-before-subscribe semantics |
| Policy, YAML loader, config store, cgroup v1 and v2, BPF assembly and syscalls, systemd watcher, processor, coordinator, `Recorder` | none | this project's Go tests, ported with provenance | new | Ported from this project's Go code. Policy (`src/policy`): a `Global` is made only by `Global::new`, which runs Go's `Validate`, and `Mode` names the two modes, so Go's empty or unknown mode, which `Enabled` had to deny, cannot exist; a `Container` only by `Container::parse`; lists keep Go's nil apart from an empty list; checked against `tests/testdata/policy/policy.json` |

## Go golden captures

The Rust ports are checked against files the Go code wrote, never against what someone reasoned
Go would write. The capture tests are Go tests behind the `goldencapture` build tag, so a plain
`go test ./...` (`make go-test`, the `go-test-repo-mod` hook) never runs them; golangci-lint builds
them with that tag, so they are linted like the rest. They share `internal/goldencapture`, which
exists only under the tag.

- `make golden-check` runs every `TestGolden*` and compares what the Go code produces now with the
  committed file; any difference fails. It is part of `make verify-go` and of CI's `test` job, so a
  Go change that would move a golden cannot land without the golden moving with it.
- `make golden-update` (`SDA_GOLDEN_UPDATE=1`) rewrites the files, for a reviewed change.

| Capture test | Files under `tests/testdata/` | What they hold |
| --- | --- | --- |
| `internal/logger`, `TestGoldenLogger` | `logger/{text,json,plain}_{time,notime}.log`, `logger/corpus.json` | Every corpus record logged through the handlers `Configure` installs (one constructor, `newHandler`), at 2026-10-25T14:03:04.567891234+01:00, with and without the time. `corpus.json` holds the records as data (level, message, `With` steps, typed attributes: strings needing quoting, nil and empty string lists, joined multi-line errors, durations, integers, floats, booleans, times, nil, groups) and how `parseLevel` reads each `--log-level` spelling, so the Rust harness builds the same records from it. A record whose value holds a raw newline spans two lines in the plain format, so the files are compared whole |
| `internal/policy`, `TestGoldenPolicy` | `policy/policy.json` | `strconv.ParseBool` spellings, `ParseMode`, `ParseContainer` with `UnknownLabels` (a nil list kept apart from an empty one), `ValidateGlobs`, `Global.Validate`, every predicate (`Enabled`, `Denied`, `Authorized`, `DeviceAllowed`, `ExplicitlyAllowed`) over globals, containers and paths, and `filepath.Match` and `filepath.Clean` cases. An error is its text; an empty string is no error |
| `internal/config`, `TestGoldenConfig` | `config/loader.json` | `LoadFile` on the loader test corpus and yaml.v3 edge cases (YAML 1.1 booleans, null and tilde, empty documents and markers, anchors and aliases, merge keys, duplicate and unknown keys, explicit tags, several documents, syntax errors, CRLF, a byte order mark), each written to a `config.yaml` whose path reads `{path}` in the error; an empty path (the zero config, no read) and a path naming no file (the read error); the decoded schema of an accepted file; the shipped `deployments/docker/config.yaml`; and `ValidateEnums` for both sources |
| `internal/observability`, `TestGoldenRecorderExposition` | `metrics/recorder.prom`, `metrics/recorder.headers` | What `GET /metrics` on the daemon's metrics server serves for the `sda_` families after a scripted sequence of every `Recorder` method, and the `Content-Type` of a request without `Accept`. The `go_`, `process_` and `promhttp_` families vary with the run and are left out |

## Upstream candidates

Fixes found during the port that are worth sending back to SSE or SRD. None yet.
