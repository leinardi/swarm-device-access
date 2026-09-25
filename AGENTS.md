# AGENTS.md

## What this is

A small **Linux-only** daemon (`//go:build linux` on every Go file under `cmd/` and most of `internal/`) that injects cgroup BPF device-allow rules
into containers that bind-mount `/dev/...` paths. Solves the Docker-Swarm gap where `devices:` / `device_cgroup_rules:` are silently ignored.

## Common commands

Build / test / vet — uses Make wrappers around `go`:

```bash
make go-build      # cross-compiles to Linux into ./dist/ (works from macOS/Win)
make go-test       # CGO_ENABLED=1 go test -race ./...
make go-vet
make go-tidy       # go mod tidy + go mod verify
make audit-deps    # govulncheck + banned-module check (network required); also runs in CI and as a pre-commit hook on go.mod/go.sum changes
make check         # pre-commit on all files
make check-stage   # pre-commit on staging area only
make docker-build
make docker-run    # runs image locally with required host bind mounts
```

Single test:

```bash
go test ./internal/systemd -run TestIsReloadCompleted -v
```

Tests are `//go:build linux`. On macOS/Windows hosts, run them inside the linux image:

```bash
docker run --rm -v $PWD:/work -w /work golang:1.26 go test ./...
```

The Makefile pulls shared snippets from `leinardi/make-common@v1` into `.mk/` on first run. To refresh: `make mk-common-update`.

## Architecture

See [`docs/architecture.md`](docs/architecture.md) for the sequence diagram, BPF program structure, package layout, and troubleshooting guide.

Three layers, all under `cmd/swarm-device-access` + `internal/`:

**1. Event loop (`cmd/swarm-device-access/main.go`)**

`run()` performs:

1. `processExistingContainers` — enumerates running containers at startup and applies rules to each (closes the "daemon restart loses state" gap).
2. `startReloadWatcher` — best-effort subscribe to systemd DBus `Reloading` signal so that `systemctl daemon-reload` (which wipes cgroup BPF programs)
   triggers a full re-apply.
3. `listenEvents` — subscribes to Docker `start` + `unpause` events, reconnects with exponential backoff (`minBackoff`/`maxBackoff`) instead of
   `log.Fatal` on stream errors.

A `processed map[string]struct{}` deduplicates the overlap window between the startup enumeration and the live event stream.

For each container, `processContainer` reads its PID, detects cgroup version via `cgroup.GetDeviceCGroupVersion`, resolves the host cgroup path (
`host/sys/...`), walks every `mount.Source` under `/dev`, and calls `cgroup.AddDeviceRules` with the major/minor pair from `unix.Stat`.

**2. Cgroup BPF (`internal/cgroup/`)**

`api.go` defines the `Interface` (`GetDeviceCGroupMountPath`, `GetDeviceCGroupRootPath`, `AddDeviceRules`) and a `New(version)` factory. `v1.go`
writes to `devices.allow`. `v2.go` + `ebpf.go` compile and attach a `BPF_CGROUP_DEVICE` program (cilium/ebpf) via `BPF_F_ALLOW_MULTI` so it composes
with the container runtime's existing filter rather than replacing it. **This code is preserved from NVIDIA's k8s-device-plugin (Apache 2.0) — touch
with care; the upstream PRs went into runc/containerd long ago.**

**3. Glue (`internal/logger/`, `internal/systemd/`)**

- `logger` — slog wrapper with `text`/`json`/`plain` handlers, `-log-time` strips timestamps via a `ReplaceAttr`. `L()` lazy-inits a default INFO text
  handler so packages can log without explicit wiring.
- `systemd` — DBus `Reloading` signal watcher. Triggers re-apply on the **completion** edge only (signal body `active=false`). The start edge (`true`)
  is mid-reload and races the cgroup wipe. Gracefully degrades when DBus is unreachable.

## Runtime requirements

The daemon **must** run with `privileged: true`, `cgroup: host`, `pid: host`, `userns_mode: host`, and bind mounts for `/var/run/docker.sock` and
`/sys → /host/sys`. The `hostRootPath = "/host"` constant in `main.go` is the inside-container view of the host root; cgroup paths are joined against
it. The DBus socket mount is optional — enables reload handling. Mount as `-v /run/dbus/system_bus_socket:/var/run/dbus/system_bus_socket`; the container-side path must be under `/var/run/` because `dhi.io/static` has no `/var/run → /run` symlink.

## Conventions worth knowing

- Every file under `cmd/` and most of `internal/` has `//go:build linux`. New code that touches devices, cgroups, or `unix.*` should keep that tag.
  Cross-platform helpers (e.g. logger) do not need it.
- Version strings (`version`, `commit`, `date`) live in `cmd/swarm-device-access/version.go` and are filled by `-ldflags -X main.version=...` from
  `GO_LDFLAGS` in `.mk/go.mk`.
- `internal/cgroup/` retains NVIDIA's original copyright header (Apache 2.0). Don't relicense or reformat that block.
- The README is the source of truth for the user-facing story; keep flag tables and the docker-compose snippet in sync if you change flags or mounts.

## Project skills

Skills live in `.agents/skills/` (symlinked as `.claude/skills`). Load them before the work, not after review:

- `go-style-guide` — before any `.go` edit.
- `trust-boundary` — before touching `internal/config`, `internal/policy`, label parsing or rule collection in `internal/processor`,
  `deployments/docker/Dockerfile` or anything else under `deployments/**`.
- `adversarial-review` — for any review request ("review my diff", "is this ready to merge").

## Quality rules not enforced by tooling

- **Reuse before writing.** Check `go-style-guide` §20 for an existing helper (`logger.L`, `processor.IsMountSource`, `sleepCtx`, the
  backoff constants, the nil-safe `observability.Recorder`) before adding one.
- **Fail closed.** An empty, unknown or malformed mode, config value or label denies or skips — it never falls back to wider device access.
- **Deletion smell.** Removing a user-visible surface (flag, key, label, metric, documented behavior) and flipping its test to assert
  absence needs a line in `README.md` or `docs/**` that retires it; a commit message is not enough.
- **Suppressions show their work.** A new `//nolint`, `# shellcheck disable=` or `# hadolint ignore=` explains why the fix does not apply
  here, not which rule fired.

## Commit messages

All commits MUST be Conventional Commits 1.0.0 **with a scope**: `<type>(<scope>)[!]: <description>`, optional blank-line body and
footers. Enforced by the `conventional-pre-commit` `commit-msg` hook (`--force-scope`). Types: `feat`, `fix`, `docs`, `test`, `refactor`,
`perf`, `build`, `ci`, `chore`, `style`, `revert`. Breaking changes use `!` before `:` or a `BREAKING CHANGE:` footer. Release notes are
generated from these messages. Examples: `fix(processor): skip unresolvable symlinks`, `ci(dependabot): add dhi registry`.

Release versions are derived from the commit types since the last tag (`feat` minor, `fix` patch, `!`/`BREAKING CHANGE` major;
anything else bumps nothing), so a wrong type ships a wrong version. PRs land as merge commits, so every commit counts, not just the
PR title. See [`docs/release.md`](docs/release.md).
