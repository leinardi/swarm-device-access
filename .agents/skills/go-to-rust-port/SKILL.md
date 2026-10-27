---
name: go-to-rust-port
description: >
  Hard-won knowledge for porting, rewriting or migrating a Go project (or a Go package) to Rust:
  semantic traps a straight port silently changes, Go library to Rust crate choices with their
  pitfalls, concurrency and shutdown differences, and tooling, lint and static-packaging traps.
  Use when planning or implementing any part of a Go-to-Rust port, and when reviewing such a port
  for behaviour drift.
---

# Porting Go to Rust

Only knowledge that took a spike, a test, a source read or a failed build to find. Each entry is
**trap → fix → (evidence)**, versioned where the version matters, so it can be re-checked. Not a
Rust tutorial. Seen in: awtrix-controller (an MQTT controller), swarm-scheduler-exporter (a
Prometheus exporter).

## 1. Approach

- Keep the Go code shipping until one breaking cutover commit; until then the Rust code lands
  beside it, under non-release commit types, and the shipped artifact does not change.
- Port leaf modules first, each with its Go tests. Every ported test gets a doc comment naming
  the Go test it replaces (`/// Go: TestX`); justify every Go test you do not port.
- Capture Go reference values with a throwaway Go program outside the module, never by reasoning
  about what Go "probably" returns. Put operands in variables: Go folds constant expressions
  exactly (`0.1+0.2` is `0.3`), so a constant tests nothing.
- List every intended behaviour change up front; any other difference from Go is a defect. Diff
  the whole output surface (an exporter's `/metrics`, a CLI's help) against the Go binary before
  the cutover, masking only what varies, each mask with its own assertion.

## 2. Semantic traps

### Time

- `time.Date` normalises out-of-range fields (Feb 31 → Mar 3) and its DST gap/fold resolution
  differs by zone (gap: Berlin 02:30 → 03:30, New York → 01:30; fold: Berlin later, New York
  earlier); `AddDate` keeps the shifted wall clock, so a New York daily 02:30 drifts to 01:30
  for good → decide clamping and gap/fold explicitly, route every local→instant conversion
  through one helper, golden-test both DST days in two zones. (go1.25 program)
- A time of day stored as "duration since local midnight" is an hour off on DST days, and a daily
  window resolved to instants can be **empty** on one → store civil times, resolve per date,
  derive state from instants, and let the next-transition search walk past empty days. (spike)
- jiff's `SignedDuration::from_hours`/`from_mins` **panic** where Go's multiply wraps → build
  config durations with `from_secs(n.saturating_mul(3600))`. (jiff 0.2 docs)
- `Zoned::now()`/`Timestamp::now()` ignore tokio's paused clock → a `Clock` trait whose test
  implementation is a base timestamp plus tokio's elapsed time, and a `disallowed-methods` ban on
  every wall-clock read outside it (§5). (spike)
- `time.Parse("15:04", "7:00")` succeeds; `time.Parse("01-02", …)` parses into year 0, a leap
  year (`02-29` valid). A strict parser rejects configs Go took. (go program)
- Zones: jiff's global database prefers the host's zoneinfo (a trimmed copy lacks the `backward`
  aliases) and matches names case-insensitively → look names up in `TimeZoneDatabase::bundled()`
  and require `iana_name() == Some(name)`; special-case `""`, `UTC`, `Local`. `TimeZone::system()`
  accepts POSIX `TZ` strings where Go's `time.Local` falls back to UTC → resolve `TZ` as
  `initLocal` does and read the TZif file into `TimeZone::tzif`. jiff's RFC 3339 writes UTC as
  `+00:00` (Go `Z`) and rounds an LMT offset (Go truncates) → format with an in-tree layout port.
  (slim-host probe; 70 of 70 layouts against Go)
- A Go `time.Now()` carries a monotonic reading, so `due.Sub(time.Now())` and an age check
  between two `time.Now()`s ignore wall-clock steps; a `Timestamp` from the injected `Clock` is
  wall time only, and a backward NTP step delays every retry by its size → keep deadlines and
  ages as `tokio::time::Instant`, and a `Timestamp` only where Go compares with a wall time from
  outside (a Docker event time). (coordinator review)
- go-solar computes sunrise/sunset for the **UTC** date of the instant; the `sunrise` crate 3
  matches it within ±1 s. (golden test against go-solar v1.0.1)

### Strings and numbers

- A Go `string` holds arbitrary bytes and `utf8.DecodeRune` reads each byte of an invalid sequence
  as its own rune; `from_utf8_lossy` replaces a truncated sequence with **one** U+FFFD → port
  per-rune byte logic over `&[u8]` (`a\xe2\x82b`: 4 runes in Go, 3 chars lossy). (go1.26)
- Go's Unicode is 15.0, Rust 1.98's 17: `strings.EqualFold` compares simple-fold orbits (`K`,
  U+212A), `ToLower` maps per code point (no final sigma; `İ` → plain `i`), `strconv.Quote` and
  slog's quoting escape code points Rust prints raw → generate Go's `SimpleFold`, `ToLower`,
  `IsPrint`, `IsSpace` tables with a Go program and port the loops. A Go `key[:n]` may cut a
  rune where `&key[..n]` panics → check `is_char_boundary`. (2,500 `EqualFold` pairs identical)
- Rust's shortest float formatting breaks an exact tie **upward**; Go's `strconv` rounds to even
  (577 of 100,000 random values differ) → take the digit count from `{:e}`, recompute with
  `{:.N$e}`, lay out as `'g'` (exponent below -4 or from 6). (250,034 Go values)
- `strconv.ParseFloat` reads hex floats and `_` separators, refuses `+nan`, and on overflow
  returns ±Inf **with** an error callers discard → port `readFloat` and its hex path. (go1.26)
- `Iterator::sum` over no `f64` is `-0.0`; Go's `total := 0.0` loop gives `+0` → fold from `0.0`.
  (a failed integration test)
- `sort.Sort` with a comparison that is not a strict weak order (goautoneg's `Accept` sort) has
  one defined result, pdqsort's; Rust's `sort_by` differs and since 1.81 may panic → port
  `sort.Sort` (insertion up to 12, then pdqsort with its seeded `breakPatterns`). (237 inputs)
- No crate parses Go durations (`1h2m3.5s`, `.5s`, `1µs`, Go's error texts) → port
  `time.ParseDuration` with the `time` package's own `quote`. (49 inputs identical)
- `filepath.Match` negates a class with `^` only (`[!a]` is a class of `!` and `a`), works on
  bytes (a `*` skip can land inside a character, which `?` then reads as U+FFFD), reports a bad
  pattern even after the name has failed to match, and refuses `[]a]`-style leading `]`; glob
  crates (`glob`, `globset`) differ on all of these → port `match.go` and `Clean` line for line.
  (go program)

- `bufio.Scanner` (default buffer) fails a line of 64 KiB or more with `token too long`, after
  yielding the lines before it, and drops one trailing `\r`; `str::lines` has no limit and drops
  `\r` only before `\n` → port the scanner once and read every Go-scanned file through it.
  `strings.Split(s, " ")` keeps empty fields where `split_whitespace` drops them, and mountinfo
  indexes fields from the end. `strings.TrimPrefix` strips a string, not a path element
  (`/docker/abc` off `/docker/abc123def` leaves `def`). (go1.26 scratch program)
- yaml.v3 (libyaml) reads a tab between `:` and a plain value as separation (`key:<TAB>value`);
  saphyr-parser 0.3 refuses it (`':' must be followed by a valid YAML whitespace`), though it
  takes the tab before a quoted value → a Q entry, or a pre-scan that is safe inside quoted
  scalars; a fixture with the tab in the parity probes. (v1.0.3 against saphyr-parser 0.3.2)
- cilium's `rlimit.RemoveMemlock` is not a plain `setrlimit`: at process start it lowers the
  soft limit to 0, creates a one-entry array map and restores it; a map created means memcg
  accounting and the limit is left alone, `EPERM` means raise it to infinity. Unprivileged, Go
  logs `failed to set memlock rlimit: operation not permitted` → port the probe (one
  `BPF_MAP_CREATE`), or the warning and the limit differ from 1.x. (cilium v0.22.0 source; the
  parity run as a user)
- `strconv.ParseInt` returns a range error at the digit that overflows, before a later invalid
  byte (`99999999999999999999x` is out of range); `i64::from_str` says invalid. `%+v` of a struct
  with pointer fields prints addresses → a Q entry, not a guess. (go1.26 scratch program)

### JSON and YAML

- `encoding/json` escapes `<>&` as `\u003c` and so on; serde_json does not. It matches keys
  case-insensitively (last wins), reads `null` as "leave it", and `Decode` ignores trailing data
  → for device input collect entries with a `MapAccess` visitor, lowercase keys, decode with
  defaults; `Deserializer::from_reader(r).into_iter::<T>().next()`. (go program)
- A Go named string type with constants is **open** → an `Other(String)` variant with
  hand-written serde. `omitempty` → `skip_serializing_if`, but never on a field Go always emits.
  "Zero means unset" → `Option<T>` with defaults in one place. (port review)
- serde_yaml is archived; serde_yaml_ng 0.10 parses with `unsafe-libyaml` (RUSTSEC-2024-0449,
  ignore by ID with a reason). yaml.v3 also accepts `yes`/`on`/`y` bools, floats into ints, `010`
  octal, `1_000` and `null` anywhere → `deserialize_with` visitors mirroring yaml.v3. (probe)

- yaml.v3 is YAML 1.1 on libyaml: it refuses any `%YAML` but 1.1 and a `...` before the first
  document, which YAML 1.2 parsers (saphyr) accept; its parser errors report a 0-based line
  (none on the first), scanner errors a 1-based one; a block scalar's line is its indicator's,
  where saphyr's span starts at the content. An alias used as a key is looked up by its anchor
  name in a shape check but decodes as the key it names, so a strict pre-pass can be bypassed
  → port the node tree and both passes, and grow the Go corpus with every such case. (golden
  diff)

### Logging

- `slog` prints before anything is configured; `tracing` drops every event until a subscriber
  exists → install it first, peeking the config's log format if needed. `slog.Level` parses any
  case plus `+N` offsets on -4/0/4/8 → port the parser and map thresholds. (seen in
  awtrix-controller: lost config warnings)
- tracing's `Visit` has no duration, time, list or group type, `None` fields vanish and `INFO+1`
  has no `Level` → for byte parity with slog's handlers, an in-tree logger with typed values that
  ports `commonHandler`. slog's JSON has **two** escapers (its own for strings and keys, encoding/
  json for `[]string`), three float layouts, and `ReplaceAttr` runs on every attribute at any
  depth, so a time stripper also drops user `time` keys. (306 of 306 records, go1.26)
- A per-test tracing `set_default` is thread-local and races the global callsite-interest cache
  → `current_thread` runtime, a global catch-all subscriber installed once, then
  `rebuild_interest_cache()`. Or inject the logger's sink and skip tracing. (3 of 40 runs lost)
- A nil `[]string` and an empty one log alike in text (`[]`) but not in JSON (`null` against
  `[]`), and a `Vec` cannot tell them apart → a separate nil value kind. A `fmt.Stringer` goes
  through `json.Marshal` (its underlying type decides the shape), not slog's string escaper. (go
  program)
- Two Go projects' handlers with the same name and the same author can differ: one plain handler
  escapes and quotes every value, the other writes messages, keys, errors and lists raw (a newline
  splits the record) → reuse the sibling port, but diff it against this repo's Go goldens before
  trusting it, and compare golden files whole, not line by line. (golden diff)

### CLI

- `flag` accepts `-long`; clap 4 reads `-config` as `-c onfig`, `-unknown` as `-u` and exits 0
  for `-hx` → before parsing, refuse every single-dash argument longer than one character (up to
  `--`, skipping a value flag's value), naming the `--` spelling; a breaking change. (clap 4.6.7)
- Other `flag` semantics: last value wins (`args_override_self`), values may start with `-`,
  bools take `=false` and every `ParseBool` spelling but no separate value (`-b false` is
  **true** plus an ignored positional in Go: refuse it), ints are base-0 literals, positionals are
  ignored. clap derive drops a trailing period from help and uses the struct doc as `about` →
  `help = "…"`, `about = None`. (flag source, help snapshot)

### BPF instructions and fmt verbs

- cilium/ebpf's `asm` does not round-trip bytes: decoding turns a `Div`/`Mod` with offset 1 and a
  `Mov` with offset 8, 16 or 32 into wider opcodes (offset 0), encoding writes every other ALU op
  with offset 0, a decoded atomic carries `imm << 8` in its opcode yet encodes its original
  immediate, a helper call's ID is checked for a platform tag in bits 28-30 on both sides, and a
  `ld_imm64` takes two slots → model its 32-bit `OpCode` and capture every opcode byte with
  register, offset and immediate variants (decode, `%v`, re-encode, gate) from a Go test before
  writing any of it. Its symbol resolution is only "the next block's first instruction" for a
  device filter → resolve offsets while laying out, no labels. (2,333 rows, cilium v0.22.0)
- `%x` on a `fmt.Stringer` hex-encodes its `String()`; `%s` on a type whose `Format` answers only
  `v` prints `{UNRECOGNIZED: s}`; `%v` of a rune prints its number → take error texts from a Go
  run, not from the format string. `fmt.Sscanf("%d.%d")` skips spaces (Unicode ones too, but not a
  newline) before each number, takes a sign, needs no input after the last verb. (go program)
- A Go fake behind an interface returns any `error` (`errors.New("injected failure")`) and the
  test asks `errors.Is`; a Rust trait with a concrete error enum cannot → inject an errno, let the
  fake return the error the real operation would, and print the composite text from the Go code
  over its own fake with injected errors of that wording. `errors.Is` on a sentinel becomes a
  classifier method that walks every wrap and every join, or a sentinel inside a join is missed.
  (cgroup v2 swap tests)

## 3. Ecosystem map

- **HTTP client, simple:** reqwest's rustls backend builds `aws-lc-sys` (C) → ureq 3 with
  rustls + ring (28 vs 86 crates), `http_status_as_error(false)` for Go's status checks, in
  `spawn_blocking`. But ureq reads `*_PROXY` and follows 10 redirects → for a Go client built
  without a proxy, hyper-util's legacy client; rebuild `resp.Status` from `ReasonPhrase` or
  `canonical_reason()` and walk `source()` for the error text. (ureq 3.5, hyper 1.12 source)
- **Docker (moby client):** bollard 0.21 drops the `/v1.xx` prefix (`Url::join`), negotiates via
  `GET /version` (403 behind a read-only socket proxy), sends no User-Agent, times out only the
  headers and fails a list on one unknown enum; its models rename `Healthcheck` to `HealthCheck`, so
  inspect → update drops a service's healthcheck → for an exporter, an in-tree client on hyper
  `client::conn::http1`: moby's `HEAD /_ping` then `GET /_ping`, the discarded negotiation error,
  go-connections' transport (10s dial, 15s keep-alive, two idle for 30s, retry on a reused
  connection, gzip on TCP only). go-connections also sends TCP hosts through `ProxyFromEnvironment`:
  a client without it is a breaking change to list. Keep bollard for a test harness, re-setting the
  healthcheck before every update. (8 of 8 ping cases, moby v0.6.0) A scoped IPv6
  `tcp://[::1%25lo]:2375` works in Go: `url.Parse` decodes `%25` to the zone (`::1%lo`), the
  dialer resolves it to an interface index and net/http's `Host` drops it (`removeZone`);
  `lookup_host` refuses `%` → port `parseHost`'s checks, build the `SocketAddrV6` with its scope
  (sysfs `ifindex`, else the zone as a number, else 0), and strip the zone from `Host`. (Go 1.26
  probe against a fake Engine on `[::1]`)
- **D-Bus (godbus):** zbus 6 with `default-features = false, features = ["tokio"]` pulls no
  async-io, smol or blocking crate, and its system bus address order is godbus's
  (`DBUS_SYSTEM_BUS_ADDRESS`, else `/var/run/dbus/system_bus_socket`).
  `MessageStream::for_match_rule` sends the `AddMatch` and registers the stream together, so the
  stream exists before a later `GetNameOwner`, as godbus's channel did; it writes `path` after
  `member` in the rule text, which the bus ignores. A dead bus makes the stream yield an I/O error
  rather than end → treat an error as the end, where godbus closed its channel. A failed dial reads
  `dial unix <path>: connect: <errno>` in Go → dial the socket yourself and hand zbus the stream
  (`Builder::unix_stream`). zbus keeps the bus's own error replies as `Error::FDO`, whose text
  prefixes the error name, where godbus printed the message alone → map `MethodError`, `FDO` and
  `DBus` to the description. Compare the two clients on a private `dbus-daemon` with `dbus-monitor`
  rather than by reading code. (zbus 6.1.0, godbus v5)
- **Syscalls (golang.org/x/sys/unix):** rustix's `Signal` wraps a non-zero value, so Go's
  `PidfdSendSignal(fd, 0, nil, 0)` liveness check cannot be written → poll the pidfd for
  `POLLIN` (the process exited), and record that a zombie now counts as gone. `Pid::from_raw`
  asserts a non-negative value (a panic in a debug build) where Go hands `-1` to the kernel and
  gets `EINVAL` → filter `> 0` first and return the errno Go's call got. `filepath.WalkDir`
  sorts each directory and visits an unreadable one a second time with the error, after the
  entries read before it; the walkdir crate does neither by default → port it, and let the fake
  walk a real temporary directory through the port, as Go's fake did. (rustix 1.1.6, scratch Go
  tests)
- **Docker over a unix socket:** moby's defaults there are not its TCP ones: no gzip, no
  response-header timeout, `Host: api.moby.localhost`, the socket path as the escaped host in
  error URLs, and the `User-Agent` `moby-client/<version> <GOOS>/<GOARCH>` (Go's arch names);
  `ContainerList` with no options sends no query at all (running containers only). moby's
  `ensureReaderClosed` drains up to 512 bytes of every response it does not decode, the ping's
  `GET` fallback included → drain it too, or hyper drops the connection and the replayed wire
  shows a connection the Go client never opened. Record the Go client's wire through a proxy and
  replay it to the Rust client, connection numbers included: per-request unit tests do not see
  connection reuse. (moby v0.6.0; `tests/testdata/engine_wire`)
- **Docker writes (moby client):** net/http sends `Content-Length: 0` with an empty `POST`
  (start, stop, wait, attach) and none with an empty `DELETE`; hyper sends none for an empty body
  → set it by hand on the `POST`s, or the wire differs. moby's attach dials a connection of its
  own and hijacks it after the `101`: hyper needs `connection.with_upgrades()` on that
  connection, `hyper::upgrade::on`, and `TokioIo` around the `Upgraded`; moby's errors there are
  `cannot connect to the Docker daemon. Is 'docker daemon' running on this host?: <dial>` and
  `unable to upgrade to tcp, received <code>`, and a read error on the hijacked unix socket is
  `read unix @-><socket>: read: <errno>` (Go writes an unbound socket's address as `@`).
  `ContainerWait` resolves once the headers are in and decodes the body later; a JSON syntax
  error there is reported as the answer's text, not the decoder's. `json.Marshal` of a
  `CreateRequest` writes every non-`omitempty` field, `null` for a nil slice and `[]` for an
  empty one → write the body by hand and pin it against a Go-printed body and the captured
  create. (moby v0.6.0 source; `launcher.jsonl`, compared byte for byte)
- **Wrapped `os.Open` errors and `bufio.Scanner`:** `fmt.Errorf("open %s: %w", path, err)` of an
  `os.Open` error names the path twice (`open <p>: open <p>: <errno>`), since the `PathError`
  already has it; a default `Scanner` fails a line of 64 KiB or more, its newline counted, with
  `bufio.Scanner: token too long`, and drops a trailing `\r` → port both, not a
  `read_to_string` and `lines()`. (Go source; launcher self identification)
- **Event streams:** every real close of Docker's chunked stream is a cut without the
  terminating chunk (HAProxy timeout, stopped daemon), surfaced by hyper as a body `Err` with
  `is_incomplete_message() == false` → classify "the body returned Err" vs "ended", decode with
  serde_json's `StreamDeserializer` over a growing buffer, and port moby's RS filter for
  `json-seq`. (fake Engine endings; HAProxy `timeout server`)
- **TLS:** rustls PEM via `rustls_pki_types::pem::PemObject`, which skips unknown sections (an
  encrypted key is invisible) → mirror go-connections' checks on the text; Go trusts the system
  pool **plus** `ca.pem` → `rustls-native-certs` plus the file. (rustls-pki-types 1.15)
- **HTTP server (net/http):** hyper 1.12 + hyper-util reproduce Go's wire, axum 0.8 does not. Go's
  `ServeMux` answers unclean paths with **307** re-escaping the escaped path, writes sorted headers
  then `Date`, `Content-Length`, `Connection` → `title_case_headers(true)`,
  `auto_date_header(false)`, all three by hand, `is_end_stream() == false`. Go frames bodies up to
  2048 bytes with `Content-Length`, larger ones chunked. hyper's `header_read_timeout` also closes
  idle keep-alives (and panics without a timer) → enforce Go's four deadlines in an I/O wrapper that
  keeps and wakes `cx.waker()`. hyper reads ahead, so a pipelined request's first bytes sit in its
  buffer and the wrapper would arm the 60s idle deadline where Go (`bufr.Peek`) arms the 5s header
  one → track the request framing (head, `Content-Length`, chunked) in the wrapper and count started
  against answered requests, with hyper's own rules (leading empty lines skipped, `Content-Length`
  items trimmed and equal, `Transfer-Encoding` winning, chunk sizes in hex) and byte by byte: a line
  cap that truncates a padded `Content-Length` loses the body and lets the next request slip past
  the header deadline. `TcpListener::bind(":8888")` is a DNS error and its backlog 128 → socket2,
  `[::]` dual stack, `listen(65535)`, `SO_REUSEADDR`. Go makes any wildcard dual-stack, not only the
  empty host: `0.0.0.0`, `[::]` and `[::ffff:0.0.0.0]` all listen on `[::]` (errors show
  `0.0.0.0:<port>`), a mapped address listens as IPv4, a zone is an interface name or `dtoi` of it,
  and its own resolver never reads inet_aton's `0` or `127.1` as addresses where getaddrinfo does. A
  boxed-error body in a spawned `serve_connection` fails "not general enough" → a concrete error
  type. A handler that sets no `Content-Type` gets a sniffed one written after `Content-Length`, not
  in the sorted block (`/healthz` writing `ok`) → add it last by hand. A port name in the address
  (`:http`) goes through `LookupPort`, and a static image has no `/etc/services`: Go then falls back
  to its own TCP table (14 names, case-insensitive; `domain` is UDP-only) → port that table, not
  getservbyname. Reused captures belong to the server they came from (its routes, its deadlines) →
  recapture against the Go server being replaced. (37 Go captures, then 41 of this daemon's;
  paused-clock tests; a pipelining probe against Go 1.26; a static Go binary in busybox)
- **Prometheus (client_golang, expfmt):** no crate writes Go's text (float formats, `charset`,
  `escaping=`, HELP escaping, sort order) → an in-tree registry and encoder porting `writeFloat`
  and `Gather`'s normalisation. promhttp negotiates `escaping=` over all four formats, protobuf
  included, and gddo's encoding negotiation prefers identity on ties and is case-sensitive →
  port both even when only text is written; flate2's gzip differs in deflate bytes, so compare
  decompressed. The process collector counts 100 ticks per second (not `CLK_TCK`), reads
  `open_fds` from `st_size` of `/proc/self/fd` and namespace-wide netstat counters → an in-tree
  `/proc` reader. To diff against Go, scrape over raw TCP (Go's client adds gzip silently), pin
  the `Accept` string, and settle on the fake daemon's request log. The negotiation changed in
  prometheus/common v0.71.0 (`NegotiateAccept` lets `*/plain` or `application/*` match a
  format; v0.70.1's `Negotiate` needs the exact type and keeps reading `escaping` past a
  wildcard), and client_golang does not pin common → port the version in the Go module's
  `go.sum`, not a reused port's, and capture the answers from the Go program being replaced.
  (client_golang v1.24.1, common v0.70.1 and v0.71.0)

## 4. Concurrency and shutdown

- Dropping a tokio runtime waits for `spawn_blocking` tasks → build it by hand and end it with
  `shutdown_timeout`; in tests `shutdown_background()`. Give shutdown one overall budget and hand
  each step what remains. (a test went 10 s → 0.1 s)
- `#[tokio::main]`'s multi-thread runtime starts one worker per CPU (cgroup-aware): on a 32-CPU
  host a small exporter's RSS matched Go's (18.6 vs 20.6 MB), 15.3 MB under `--cpus 2` → measure
  on the target host; cap `worker_threads` if the footprint is the point. (side-by-side, 10 min)
- A job re-deriving state when its monotonic timer fires can read before the boundary → evaluate
  at `max(clock.now(), due)`. Under a paused clock an unbounded wait spins forever → bound every
  wait. (frozen-clock tests)
- `recover()` → poll the step inside `std::panic::catch_unwind` yourself (futures' `CatchUnwind`
  cannot silence the hook or keep the backtrace), with a chaining hook that stores
  `Backtrace::force_capture()` on a marked thread; keep `panic = "unwind"`. (recover tests)
- `sync.Map.LoadOrStore` never overwrites, so a "log when changed" cache on it keeps the first key
  → `entry().or_insert()`, pinned by a test, or list the fix. (Go source read)
- `time.Ticker` drops overrun ticks; tokio's `interval` bursts by default →
  `MissedTickBehavior::Skip`, consuming the immediate first tick. (3 of 3 slow-poll cases)
- Go's two locks with a lock order can often become one `std::sync::Mutex` held briefly, never
  across `.await`, by moving the second structure's key set into the first. (port review)
- A Go mutex held across a whole transaction that makes a context-bound call in the middle (a
  re-inspect before a mutation) → run the transaction in `spawn_blocking`, take the
  `std::sync::Mutex` inside it, and drive the call with `Handle::block_on` under the caller's
  context; await the `JoinHandle` to completion, so a dropped caller never ends the mutation.
  On a `current_thread` test runtime `block_on` works because the test thread drives the timers
  and I/O while it awaits the handle. (reconcile cancellation test)
- A Go call under `context.WithTimeout` that times out inside net/http fails with a `url.Error`
  (`Get "<url>": context deadline exceeded`), which moby passes through; a
  `tokio::time::timeout` around the Rust call yields only the bare text → make the context
  visible to the client for the call (a task-local) and race each request against it there.
  (Go program against a socket that never answers, moby v0.6.0)
- A Go caller that reserves a key, works and releases it cannot be stopped between the steps; a
  Rust future can be dropped there (a `timeout` or `select!` around it), which either leaks the
  reservation or, with a drop guard, frees it while the work still runs → run the reserved work
  on a tracked task of its own that releases the key itself, and have the caller await its
  handle; shutdown waits for the tracker. (coordinator mutation test: an inline hold fails both
  regression tests)
- moby's `Events` returns once the request has been answered or has failed, and reports a
  failed request on the stream's error channel, so Go logs "subscribed" and then a stream error;
  a Rust `events()` that fails the call itself skips the first line → keep the refusal as the
  stream's first item. (moby v0.6.0 `Events` source)
- Go's one-slot wake channel with a non-blocking send (`select { case ch <- x: default: }`) is
  `Notify::notify_one`: its single stored permit is the slot. (coordinator port)
- Under `start_paused`, a running `spawn_blocking` task stops the clock from auto-advancing
  (tokio 1.23, #5115), so a test whose blocking task waits for the test hangs on any tokio sleep
  → signal entry with a token and move time with `tokio::time::advance`. (coordinator regression
  test)

## 5. Tooling, lints and packaging

- **Lints:** golangci-lint maps to clippy (exhaustive → `wildcard_enum_match_arm`, varnamelen →
  `min_ident_chars`, forbidigo → `print_stdout`/`disallowed_methods`, depguard → a meta-test over
  the Cargo manifests), `//nolint` → `#[expect(lint, reason)]` with `allow_attributes` denied.
  `[lints]` is not inherited → `[workspace.lints]` plus `[lints] workspace = true` in every
  member, tested. (spike)
- clippy's test allowances do not cover helpers in `tests/*.rs` → `unwrap_or_else(|error|
  panic!(…))` or a file-level `#![expect]`. `tests/support/mod.rs` compiles once per test crate,
  so a helper one crate leaves unused is dead code there → `#[expect(dead_code)]` on that crate's
  `mod support;`. (clippy 1.98)
- A Go build tag → a Cargo feature, which `--workspace --all-targets` neither builds nor lints →
  `--all-features` for build and clippy, `required-features` on the test target that needs a
  daemon. libtest's filter is a substring, so `'^TestNode_'` selects nothing and passes → a
  `harness = false` runner with Go's `-run` semantics that fails on zero selected. (10 hidden
  lint errors)
- `t.Skip` has no libtest equivalent → a gated test returns early with a note, and an
  environment variable CI sets turns the missing tool into a failure. (ported meta-tests)
- Teaching the shell check scripts Rust (`#[expect]` counting, the header rules) means rewrapping
  their prose, and ShellCheck parses any comment line whose first word is `shellcheck` as a
  directive (SC1073, SC1072) → never start a wrapped comment line with that word. Keep a Rust-only
  rule (a `testdata/` exemption, a header check) scoped to `*.rs`, with a Go case in the script's
  tests as the control that the Go side did not move. (ShellCheck 0.11, swarm-device-access)
- cargo-deny 0.20: webpki-roots is `CDLA-Permissive-2.0` (allow with a comment); `{ crate = … }`
  ignores only yanked versions → ignore advisories by ID with a reason. (cargo-deny warnings)
- cargo-deny 0.20.2 leaves workspace members' dev-dependencies out of its graph: with only a meta
  crate's dev-dependencies, `cargo deny --workspace list` names the members alone, and
  `exclude-dev = false` changes nothing. Their licences and advisories are never checked → decide
  the dev graph's licences in `deny.toml` by hand, with a comment, and expect a
  `license-not-encountered` warning for them. (`cargo deny list`, swarm-device-access)
- **Static builds:** cargo-zigbuild builds static musl for amd64 and arm64 from one platform; it
  publishes no aarch64 musl asset (an arm64 build host takes `aarch64-unknown-linux-gnu` in a
  glibc stage). Alpine's rust links dynamically; `+crt-static` needs `--target`. (failed builds)
- `dhi.io/rust` has no rustup; its `-debian*-dev` variant takes a target from
  `rust-std-<exact version>-<triple>.tar.xz` and `install.sh --prefix=/usr/local`. Its tag carries
  its own patch, so a copied `rust-toolchain.toml` can build the release binaries with another
  compiler than the image → one exact release in the toolchain file, every CI step and the tag,
  tested patch included, bumped by hand with the `rust-std` digests. (cutover review)
- Version stamps via `option_env!` make a golden test need a stamped build → build a stamped copy
  once into `CARGO_TARGET_TMPDIR` behind a `OnceLock`, not in the shared target dir. (11 s once)
- **Calibration:** an MQTT controller: Go 12 MB binary, 18 MB RSS; Rust 4.9–6.3 MB, about 5 MB.
  A Docker Swarm exporter: Go 11.2/10.5 MB binaries (amd64/arm64), 13.1 MB image, 20.6 MB idle
  RSS; Rust 4.4/3.9 MB, 6.3 MB image, 18.6 MB on 32 CPUs and 15.3 MB on 2 (§4). Clean release
  builds take 36–110 s per target.

Verified against (2026-10): Rust 1.98.1, tokio 1.54, hyper 1.12.0, hyper-util 0.1.21, jiff
0.2.38, clap 4.6.7, serde_json 1.0, serde_yaml_ng 0.10.0, ureq 3.5.0, rustls 0.23.46, flate2
1.1.10, bollard 0.21.1, tracing-subscriber 0.3.23, sunrise 3.0.0, cargo-deny 0.20.2,
cargo-zigbuild 0.24.0, zig 0.17.0; Go baselines from go1.25 and go1.26.
