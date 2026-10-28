# Conventions

## Platform

- Linux only: openat2, pidfds, bpf(2) and cgroups with no fallback; build and test on Linux.
- Syscalls through rustix; `libc` and `unsafe` only in `src/cgroup/bpf_sys.rs` (meta-test pinned).

## Lints and suppressions

- `[workspace.lints]`: clippy pedantic plus restriction lints (`unwrap_used`, `expect_used`,
  `print_stdout`, `print_stderr`, `wildcard_enum_match_arm`, `min_ident_chars`,
  `disallowed_methods` for wall-clock reads, `allow_attributes`). `-D warnings` in the gates.
- Only `#[expect(lint, reason = "...")]`; counted against `.suppressions-baseline`
  (`scripts/check-suppressions.sh --update`, committed with the change).
- Project rules: `AGENTS.md`; general guidance: user-level Rust skills.

## Copyright header

Every `.rs`, shell, Dockerfile, Makefile and local `.mk` file has the Apache 2.0 block.
`src/cgroup/{api,ebpf,v1,v2}.rs` keep NVIDIA's header. Files reused from swarm-scheduler-exporter
or swarm-rootless-docker add the provenance line from `docs/rust-port.md`, never the MIT block.

## Makefile

- Shared `.mk/{docker,help,password,pre-commit}.mk` fetched from `leinardi/make-common@v1` — never
  hand-edit; repo-local targets in their own `.mk/<fragment>.mk` listed in `MK_LOCAL_FILES`.

## CLI flags

clap in `src/cli.rs`, double-dash only (Q1). Repeatable `--device-allow`/`--device-deny`.
YAML config file (`--config`): command line over file over default (`Settings::merge`).
Hot reload on SIGHUP, all or nothing; restart-only keys logged and ignored.

## Policy labels

Container labels use prefix `swarm-device-access.`:

- `swarm-device-access.enable` — opt-in/out
- `swarm-device-access.device-allow` — comma-separated glob patterns
- `swarm-device-access.device-deny` — comma-separated glob patterns

## Logging and errors

- Log through a cloned `LogHandle` with `Attr` values; error key `err`; 1.x messages and levels.
  Formats `text`, `json`, `plain`; `--log-time` controls timestamps.
- Errors are hand-written enums whose `Display` is the 1.x text (no thiserror/anyhow).
- `listen_events` reconnects with `MIN_BACKOFF`/`MAX_BACKOFF`; the systemd watcher retries in the
  background when D-Bus is absent.

## Time

Inject `Arc<dyn Clock>`; only `src/clock.rs` reads the wall clock. Tests use `TokioClock`
on a paused runtime or `FakeClock`.
