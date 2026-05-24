# Conventions

## Build tags

- All files under `cmd/` and most of `internal/` require `//go:build linux` (first line).
- Cross-platform packages (`logger`, `policy`) omit the tag.
- New code touching devices, cgroups, or `unix.*` must carry the tag.

## Copyright header

Every `.go` file has an Apache 2.0 copyright block. `internal/cgroup/` retains NVIDIA's
original header — do not relicense, reformat, or alter that block.

## Package structure

- One binary: `cmd/swarm-device-access/` (package `main`).
- Internal packages: `internal/cgroup`, `internal/logger`, `internal/policy`, `internal/systemd`.
- Shared `.mk/` snippets fetched from `leinardi/make-common@v1` — do not commit generated `.mk/` files directly.

## CLI flags

Defined with `flag` stdlib in `flags.go`. Repeatable slice flags use `stringSliceFlag` type.
Override-able via YAML config file (`-config`); file values apply first, then CLI flags.
Config hot-reload on SIGHUP.

## Policy labels

Container labels use prefix `swarm-device-access.`:

- `swarm-device-access.enable` — opt-in/out
- `swarm-device-access.device-allow` — comma-separated glob patterns
- `swarm-device-access.device-deny` — comma-separated glob patterns

## Logging

Use `logger.L()` for structured slog logging. Format options: `text`, `json`, `plain`.
`-log-time=false` strips timestamps (default) — useful for systemd journal.

## Error handling

`listenEvents` uses exponential backoff (`minBackoff`/`maxBackoff`) on stream errors, not `log.Fatal`.
Systemd DBus watcher degrades gracefully when socket is absent.

## golangci-lint

v2, all linters enabled. `//nolint:gochecknoinits` is acceptable only for `flag` `init()` registration.
Violations block commits via pre-commit.
