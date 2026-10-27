---
name: rust-style-guide
description: >
  Rust coding rules for swarm-device-access: the workspace lint set (clippy pedantic plus the
  restriction lints) and how to satisfy it, #[expect] with a reason as the only suppression, the
  single audited unsafe module, the maintainer's rules (descriptive names, exhaustive matches,
  public items first, no panics or printing outside tests), the reuse ledger in docs/rust-port.md,
  and the daemon's patterns (hand-written errors whose text is the 1.x contract, the
  slog-compatible logger behind a LogHandle, the read-only Docker trait on the daemon path and the
  write client only in the launcher, the injected clock, the blocking cgroup transaction, tests
  ported from Go with their provenance). Use when writing, editing or reviewing any .rs file in
  swarm-device-access; consult it before generating Rust code, not after clippy fails.
---

# Rust Style Guide — swarm-device-access

Adapted from the `rust-style-guide` skills of swarm-scheduler-exporter (SSE, at `d57dcc220d29`)
and swarm-rootless-docker (SRD, at `16913a4d87d8`), the two projects the port reuses code from.
The rules come from the root `Cargo.toml` (`[workspace.lints]`), `clippy.toml`, `rustfmt.toml`,
the design record in [`docs/rust-port.md`](../../../docs/rust-port.md) and the Go code, which
stays the behavioural authority until the cutover. §1–§4 are what the lints enforce, §5–§7 are the
maintainer's rules, §8–§14 are this daemon's patterns, §15–§17 are review rules.

The Rust workspace lands phase by phase during the port. A rule that names a path, a module or a
make target applies from the commit that adds it; until then it is the target the new code is
written against, not a reason to skip the rule.

Every lint and test command runs workspace-wide, so the three members (the daemon package,
`testenv` and `meta`) are held to the same set:
`cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`,
`cargo test --workspace --locked`, `cargo fmt --all --check`. Never commit with `--no-verify`;
fix the underlying issue instead.

---

## 1. The lint set

`[workspace.lints]` is declared once, and every member opts in with `[lints] workspace = true`
(a member does not inherit it otherwise; `meta/tests/workspace_lints.rs` fails when one does not).
`clippy.toml` sits at the workspace root, so its settings apply to every member.

| Lint | Level | What to do |
| --- | --- | --- |
| `unsafe_code` | deny | No `unsafe` outside `src/cgroup/bpf_sys.rs` (§9). `deny`, not SSE's `forbid`, because `forbid` cannot be lifted even by `#[expect]` |
| `clippy::pedantic` | warn (priority -1), an error under `-D warnings` | Satisfy it; the usual ones below |
| `allow_attributes`, `allow_attributes_without_reason` | deny | `#[expect(lint, reason = "...")]` only (§2) |
| `unwrap_used`, `expect_used` | deny | Return an error; tests may use both (§3) |
| `print_stdout`, `print_stderr` | deny | Log through the `LogHandle`; `writeln!` to a handle when output is the point (`--help`, `--version`, the refusal before the logger exists) |
| `wildcard_enum_match_arm` | deny | Name every variant (§6) |
| `min_ident_chars` | deny | Descriptive names (§5) |
| `disallowed_methods` | deny | No wall clock outside `src/clock.rs` (§8) |

The pedantic lints that come up most:

- `must_use_candidate`: put `#[must_use]` on a pure function or constructor that returns a value.
- `missing_errors_doc`, `missing_panics_doc`: a public function returning `Result` has an
  `# Errors` section; one that can panic has `# Panics` (better: make it not panic).
- `doc_markdown`: identifiers in doc comments go in backticks (`` `container_id` ``,
  `` `SetDeviceRules` ``); `clippy.toml`'s `doc-valid-idents` lists only the proper names the
  docs need (`eBPF`, `OpenMetrics`, `ShellCheck`, …), never an identifier to dodge backticks.
- `too_many_lines` (100): split the function into named steps.
- `needless_pass_by_value`, `trivially_copy_pass_by_ref`: borrow what you only read; pass small
  `Copy` types by value.
- `cast_possible_truncation`, `cast_sign_loss`, `cast_precision_loss`: use `try_from` or `From`.
  Never an `as` cast on a device number, a file descriptor, a BPF field, an instruction count or a
  byte size, where a silent wrap is a security bug. Where the cast is the point (Go's float and
  duration formatting in `src/go_compat`), `#[expect]` it with the reason the range is safe.

`rustfmt.toml` sets `edition = "2024"` and `max_width = 100`. The `rustfmt-on-edit.sh` hook
formats a `.rs` file after every edit in Claude Code, and the `cargo-fmt` pre-commit hook checks.
rustfmt does not wrap comments: keep them at 100 columns by hand.

## 2. Suppressions: `#[expect]` with a reason, counted

- Only `#[expect(lint, reason = "...")]`. `#[allow]` is denied, and so is either attribute
  without a reason. An `#[expect]` fails the build once the lint stops firing, so a stale
  suppression cannot linger.
- Put it on the smallest item that needs it: the statement or the function, not the module. A
  file-level `#![expect(...)]` is for an integration-test file whose helpers all share the reason
  (`expect_used` in helpers that are not `#[test]` functions, which clippy's test allowance does
  not cover), and for the one `#![expect(unsafe_code, reason = "...")]` in `bpf_sys.rs`.
- The reason says why the fix does not apply here, not which lint fired.
- Suppressions are ratcheted: `scripts/check-suppressions.sh` counts every lint of an
  `#[expect]` or `#[allow]` against `.suppressions-baseline` and refuses a commit that adds one
  without changing the baseline. After adding, moving or removing one: stage it, run
  `scripts/check-suppressions.sh --update`, and commit the baseline with it. Raising a count
  needs a reviewer's agreement.

## 3. Errors

- Errors are hand-written enums with `Display` and `std::error::Error`, each variant carrying its
  source. There is no `thiserror` and no `anyhow` (SRD's `thiserror` enums are rewritten by hand
  when its launcher code is reused).
- **The text is the 1.x contract.** Every error is logged, so every `Display` reproduces the Go
  error string byte for byte, including the `%w` chain: a Go `fmt.Errorf("open cgroup %s: %w",
  path, err)` becomes a variant whose `Display` writes `open cgroup <path>`, a colon and a
  space, then the source's own `Display`. A test pins any new text, and a wording difference
  that cannot be avoided is a Q entry in `docs/rust-port.md`, never a silent change.
- Go's `errors.Join` is ported once (`go_compat`): the messages joined with newlines, nil entries
  dropped. Use it wherever the Go code joins; never build the joined string by hand.
- Classify by method, not by string: `is_not_found()`, `errno()` and friends, the way Go uses
  `errdefs.IsNotFound` and `errors.Is`.
- No `unwrap`/`expect` outside tests. A `Mutex` is read with
  `.lock().unwrap_or_else(PoisonError::into_inner)` only where the guarded state is still
  consistent after a panic; under `panic = "abort"` (§12) a poisoned lock is only reachable in
  tests, so say which case applies in a comment.
- A failure the daemon carries on from (one inspect, one container's apply) is logged with the
  error as the `err` attribute and the work continues or is retried; a startup failure returns an
  exit code from `run`. `main` is the only place that exits the process: `0` on a clean stop, `1`
  for a startup or validation error (written to stderr before the logger exists, as Go does), `2`
  for a usage error (Q1).

## 4. Documentation, comments and file headers

- Every module starts with a `//!` comment saying what it owns; every public item has a `///`
  comment starting with what it is or does.
- Every `.rs` file starts with this project's Apache-2.0 header (`Copyright <year> Roberto
  Leinardi.`, the year of the file's first commit), as the Go files do. A file reused from SSE or
  SRD keeps that header and adds, right below it, the provenance comment:
  `// Adapted from <repo>/<path> at <commit>, originally MIT-licensed, Copyright (c) <year>
  Roberto Leinardi`. Never paste the MIT block itself.
- `src/cgroup/{api,ebpf,v1,v2}.rs` port NVIDIA's files: they keep NVIDIA's Apache-2.0 block
  verbatim, the crun and runc provenance paragraph where the Go file has one, and a modification
  notice under it (Apache 2.0 §4b). Never reformat or relicense that block.
- A comment says **why the code is the way it is**; history and "changed X to Y" belong in the
  commit body.
- Where the Rust deliberately differs from 1.x (the Q entries), the module comment names the
  difference; where it ports a Go behaviour on purpose (`src/go_compat`), it says which and why
  the contract depends on it. A ported test's provenance is its `Go:` doc comment (§14).
- `TODO` is allowed; a comment on code you did not otherwise change is not.

## 5. Names

- `min_ident_chars` refuses single-character names. Name a binding for what it holds at any
  distance: `index`, `container_id`, `rule`, `major`, `program_fd`, never `i`, `c`, `fd`. Closure
  parameters too (`|mount| ...`, not `|m| ...`).
- Error bindings name their source when several are in scope: `inspect_error`, `attach_error`.
- Keep the Go names for ported concepts (`Reconcile`, `compute_desired`, `apply_pinned`,
  `SetDeviceRules` as `set_device_rules`), so a reader can hold the Go and the Rust side by side.
- Label keys, metric names and their labels are constants beside their owner, never literals at a
  call site.

## 6. Matches, item order, types

- **Exhaustive matches.** A `match` on an enum names every variant (`wildcard_enum_match_arm`);
  group them with `|` when they share an arm. A new variant is then a compile error everywhere it
  matters. Matching on integers or strings may keep a `_` arm, and that arm refuses (fail
  closed), never widens.
- **Item order.** Within an `impl`, public items come before private ones. Not linted: checked in
  review.
- **Types over strings.** A validated value gets a type whose constructor is the only way to make
  one (a mode, a glob list, a device rule), so unvalidated input cannot reach a policy decision or
  the daemon's container spec.
- **Open Docker strings.** Engine API enums (container states, event actions) are open sets:
  decode them as strings and compare against the known constants, so an unknown value is handled
  the way 1.x handled it.
- **Wire types.** The Docker types mirror the Engine API's keys (`#[serde(rename = ...)]`),
  default what Go's zero value would fill, and are pinned by the captures under
  `tests/testdata/engine_wire`. A field that changes the decoded shape updates those tests.

## 7. Reuse before writing

The binding reuse policy and the reuse ledger are in
[`docs/rust-port.md`](../../../docs/rust-port.md). Before designing a component, read its ledger
row and the SSE or SRD code and tests it names, at the pinned commit; copy and adapt them, keeping
their structure, naming, error handling and dependency choices wherever this project's contract
allows. A reused component's tests come with it in the same commit.

- **One implementation per feature.** No feature has two implementations (two Docker request
  paths, two loggers, two `ParseBool`s, two `errors.Join`s, two test-wait helpers) unless
  `docs/rust-port.md` records the technical reason. The read-only daemon client and the
  launcher's write engine share one transport for that reason.
- Every reused or newly ported component gets or updates its ledger row in the same commit:
  source path and commit, mode (unchanged, adapted or new), each adaptation, the reason for every
  departure. A later upstream commit is taken only by updating the row.
- A fix worth sending back to SSE or SRD goes on the upstream-candidates list in
  `docs/rust-port.md`; no shared crate is extracted during the port.
- The Go code stays the authority: a reference implementation never overrides this project's
  log and error text, its Engine request contract or its security boundaries.

| Need | Use | Not |
| --- | --- | --- |
| The current time | the injected `Arc<dyn Clock>`; `TokioClock` (follows the paused clock) or `FakeClock` in tests | `SystemTime::now()`, `Instant::now()`, `jiff` now |
| A log line | the component's cloned `LogHandle`, with `Attr` values | `format!` in the message, a global logger |
| Go-compatible text (quoting, durations, RFC3339Nano, floats, `ParseBool`, `filepath.Match`/`Clean`, `errors.Join`) | `src/go_compat` | `format!` that only looks right |
| A Docker call on the daemon path | the read-only Docker trait | a request built at the call site, the launcher engine |
| A Docker call in the launcher | `src/launcher/engine.rs` | a second write path, bollard |
| A syscall | rustix; a `bpf(2)` command through `src/cgroup/bpf_sys.rs` | `libc` outside `bpf_sys.rs`, a second `unsafe` block |
| A metric | the `Recorder` (nil-safe no-op form included) | a family registered at the call site |
| Captured logs, a bounded wait, a fake Docker in a unit test | `test_support` (`capture_logs`, `eventually`, the fake) | a sink, a sleep loop or a fake of your own |
| A fake Engine on the wire | the fake engine in `tests/support` replaying the captures | a hand-rolled server |

## 8. Time

- `clippy.toml`'s `disallowed-methods` bans `std::time::SystemTime::now`,
  `std::time::Instant::now` and the `jiff` "now" constructors. Only `src/clock.rs` reads them,
  behind an `#[expect(clippy::disallowed_methods, reason = "...")]`; everything else takes an
  `Arc<dyn Clock>`. `tokio::time::Instant` stays allowed: it follows the paused clock, so it is
  what deadlines (backoff, retries, the shutdown budget) are measured with.
- Durations on the command line and in the config file use Go's syntax (`go_compat::duration`),
  and durations in log lines are formatted as Go's `Duration.String`, as 1.x did.

## 9. `unsafe` and syscalls

- The workspace sets `unsafe_code = "deny"`. Exactly one module lifts it:
  `src/cgroup/bpf_sys.rs`, with a file-level `#![expect(unsafe_code, reason = "...")]`. A
  meta-test pins that list; adding a second entry is a design change recorded in
  `docs/rust-port.md`, not a lint fix.
- `bpf_sys.rs` is the only place `libc` is called: `libc::syscall(SYS_bpf, ...)` for the BPF
  commands the cgroup v2 code needs, with `#[repr(C)]` attribute unions whose layout and size a
  unit test pins. Each `unsafe` block has a `// SAFETY:` comment naming the invariant it relies
  on (buffer lengths, descriptor ownership, zeroed padding).
- Every other syscall goes through rustix (`openat2` with `RESOLVE_BENEATH` and
  `RESOLVE_NO_MAGICLINKS`, `fstat`, `readlinkat`, `pidfd_open`, `pidfd_send_signal`, `uname`,
  `setrlimit`). Descriptors are `OwnedFd`, so a close is never forgotten or doubled.
- The BPF instruction encoding is byte-identical to cilium asm in host byte order: ownership is
  decided by regenerating the wrapper and comparing bytes, so a single differing byte turns every
  wrapper the Go daemon attached into an owned-block conflict after an upgrade. The goldens and
  fixtures under `tests/testdata` pin it; never "simplify" an encoding they do not cover.

## 10. The Docker client: read-only daemon, write-capable launcher

- The daemon path (processor, coordinator, event loop, observability) takes only the read-only
  Docker trait: `ping`, `container_list` (running only), `container_inspect` and `events`. Its
  client issues `GET` only, plus the one `HEAD /_ping` of moby's lazy version negotiation, through
  one request builder, and dials only the configured unix socket (no `DOCKER_*` environment, no
  TCP, TLS or proxy). A meta-test enforces it.
- The write calls (create, start, attach, wait, stop, remove) live only in
  `src/launcher/engine.rs`, on the same transport with its own request builder. Only
  `src/launcher/**` may name it; a meta-test enforces that too. No write call is reachable from
  the privileged daemon.
- Request details are moby v0.6.0's: the version prefix, the negotiation, the `Host` header, the
  query and filter encoding, the error wording. The captures under `tests/testdata/engine_wire`
  are the reference; a new call adds a wire test.
- bollard appears only in `testenv`, behind its `integration` feature.

## 11. Logging

- The logger (`src/logger`) reproduces slog's text and JSON handlers and this project's plain
  handler byte for byte, against the Go goldens. There is no `tracing`.
- Components receive a cloned `LogHandle`, never a process global: a reconfigure (startup, a
  `SIGHUP` that changes the format or level) swaps the handler behind the shared cell, so every
  clone logs through the current one. Call
  `log.warn("message", &[Attr::string("container", id), Attr::error("err", &error)])`; the
  error key is always `err`. Never build the message with `format!`.
- Messages, attribute keys and levels are the 1.x ones (the integration tests and operators wait
  on them): lower case, no module prefix. A level is part of the contract.
- Never log a secret. Labels, mount paths, device numbers and container IDs are not secrets.

## 12. Concurrency, blocking work and shutdown

- The runtime is built by hand in `main`: multi-thread, `worker_threads` capped at 2. Shutdown
  has one budget from the first signal, covering every step, and ends with
  `Runtime::shutdown_timeout(remaining)`; each step gets only what remains, never a fresh grace
  period. Signals after the first are swallowed.
- The release profile sets `panic = "abort"`: Go has no `recover` here, so a panic crashes the
  process and Swarm or the launcher restarts it. There is no `catch_unwind` anywhere; a task that
  panics under unwind would leave a silently stuck daemon.
- Blocking syscalls run in `spawn_blocking`. The cgroup and BPF mutation of one reconcile (pin,
  open, verify, `set_device_rules`, the ledger and cache updates) is **one blocking
  transaction**: it takes the processor lock (a `std::sync::Mutex`) inside the closure and owns
  the pidfd, the `/proc`, `/dev` and cgroup descriptors and the program handles until it returns.
  Its `JoinHandle` is always awaited to completion, even after the caller's timeout or
  cancellation, and the container's reservation is released only then. Dropping a future must
  never be what ends a mutation. Only the async Docker calls around it (inspect, re-inspect) are
  cancellable.
- Cancellation is a `tokio_util::sync::CancellationToken` tree from `run`. Shared state sits
  behind a `std::sync::Mutex` held briefly and never across an `.await`, or an atomic; readers get
  copies.

## 13. Fail closed

The `trust-boundary` skill owns the rules; the short form for code review: an empty, unknown or
malformed mode, config value or label denies or skips, and anything uncertain yields the empty
desired set. A `_` arm on a string or integer refuses. A config value is validated at load, by
name, before anything is applied, and a rejected `SIGHUP` reload keeps the previous settings in
force. No new capability, host path or listener appears without the README and `AGENTS.md`
changing in the same commit.

## 14. Tests

- Unit tests sit beside the code (`#[cfg(test)] mod tests`, or a `tests.rs` in the module
  directory); black-box tests and the goldens are in `tests/`; the integration suite is in
  `testenv`, behind the `integration` feature; meta-tests are in `meta/tests`.
- **Go provenance.** A test ported from Go carries a doc comment naming the Go test it replaces:
  ``/// Go: `TestReconcile_RevokesOnInspectFailure` ``. A Go test that is not ported is listed in
  the module's tests with the reason. Integration tests keep their Go names, which the run filter
  selects.
- Go baselines (goldens, captures) come from Go programs or capture tests behind the
  `goldencapture` build tag, never from reasoning about what Go would print.
- Deterministic over timed: a race window is built by the test (a barrier in the fake), never hit
  by sleeping and hoping.
- Time-dependent tests run on a paused runtime (`#[tokio::test(start_paused = true)]`) with a
  `TokioClock`. Never `std::thread::sleep`.
- Waiting, by class:
  - **Positive eventual:** poll with `eventually` (unit) or the harness's positive wait
    (integration), under a bound, and fail naming what never happened. Under a paused clock an
    unbounded wait spins forever instead of failing.
  - **Negative assertion:** give the wrong behaviour a bounded window, then assert it did not
    happen, and say so in a comment.
  - **Real elapsed time:** only when the duration is the point (the shutdown budget), as a named
    constant.
- **Meta-tests never skip.** A meta-test that needs a tool fails without it; there is no
  `REQUIRE_TOOLS` switch on the Rust side. Every meta-test has a negative control: a case proving
  the rule fails on the input it exists to stop. A check that passes on an empty input is no
  check.

## 15. What to avoid

- `unwrap`/`expect`, `println!`/`eprintln!`, a wall-clock read, or `unsafe` outside the places
  above.
- `#[allow]`, or an `#[expect]` whose reason restates the lint (§2).
- A `_ =>` arm on an enum (§6).
- An error or log text that differs from 1.x without a Q entry (§3, §11).
- A write Docker call, or the launcher engine, reachable from the daemon path (§10).
- A cgroup or BPF mutation that a dropped future can interrupt, or a reservation released before
  its transaction ended (§12).
- A second implementation of a feature the ledger already has (§7).
- Designing for hypothetical requirements: no configurability or abstractions for features that
  do not exist yet.
- Skipping or suppressing pre-commit hooks (`--no-verify`).

## 16. Quick checklist before submitting Rust code

- [ ] `cargo fmt --all` clean; clippy clean with
      `--workspace --all-targets --all-features -- -D warnings`
- [ ] No `unwrap`/`expect`/printing outside tests; no `unsafe` outside `bpf_sys.rs`
- [ ] Errors hand-written, their `Display` the Go text including the `%w` chain
- [ ] Each `#[expect]` has a reason saying why the fix does not apply; baseline updated (§2)
- [ ] Descriptive names; every enum variant matched by name; public items first
- [ ] `#[must_use]`, `# Errors` and backticked identifiers where pedantic asks
- [ ] Ledger row added or updated; no second implementation; reused files carry the Apache
      header with the provenance line (§7, §4)
- [ ] Time from the `Clock`; logs through a `LogHandle` with the 1.x message and level
- [ ] Daemon path on the read-only Docker trait; writes only in the launcher engine
- [ ] Ported tests carry their `Go:` comment; no unbounded wait under a paused clock; every new
      meta-test has a negative control and never skips
- [ ] Any unavoidable difference from 1.x recorded as a Q entry in `docs/rust-port.md`

## 17. Lint and test loop

1. `make rust-fmt` (or let the edit hook do it).
2. `make verify` (`verify-rust`: `rust-build`, `rust-fmt-check`, `rust-clippy` and `rust-test`
   over the workspace, the meta-tests included). After a dependency change, also
   `make rust-deny` (network required).
3. Fix each report and re-run until clean. For a single test:
   `cargo test --locked --lib <module>::tests::<name>`.
4. After a change the integration suite exercises (daemon, processor, cgroup, launcher):
   `make test-integration`; enforcement only on a throwaway host.
5. `make check-stage` (or `make check`), then `git status`: hooks may rewrite files in place. If
   you added, moved or removed a suppression, run `scripts/check-suppressions.sh --update` and
   stage `.suppressions-baseline`.
