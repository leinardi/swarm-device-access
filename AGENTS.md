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

Integration tests (need a live Docker daemon; see [`docs/testing.md`](docs/testing.md)):

```bash
make go-test-integration                   # dry-run tests
make go-test-integration SDA_IT_ENFORCE=1  # plus the real-enforcement test: attaches BPF, runs systemctl daemon-reload; throwaway hosts only
make sweep-test-leaks                      # remove containers left behind by killed runs
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

Four layers, all under `cmd/swarm-device-access` + `internal/`:

**1. Entrypoint and config (`cmd/swarm-device-access/`, `internal/config/`, `internal/policy/`)**

`main.go` parses flags, merges them with the config file (`config.go`: `flagSettings`, `mergeSettings`, `validate`), refuses to start
without `openat2` (`processor.ProbeOpenat2`), creates the `config.Store`/`Publisher`, and wires the processor and `daemon.Run`. `SIGHUP`
re-reads the file (`reloader.reload`) and publishes through `Processor.PublishAndReconcile`. `internal/config` is the strict YAML loader and
the generation-numbered runtime store; `internal/policy` holds mode, label parsing and the glob predicates (`Denied`, `Authorized`).

**2. Event loop and coordinator (`internal/daemon/`)**

`Run` subscribes to Docker events (`start`, `unpause`, `die`, `destroy`) **before** the first pass, starts the coordinator, requests the
startup pass, starts the systemd reload watcher, then consumes events (`listenEvents` → `consumeEvents`, reconnecting with
`minBackoff`/`maxBackoff` backoff). The coordinator (`coordinator.go`) is the single owner of reconciliation passes: startup, `SIGHUP` and
systemd reloads only request one; it retries failed containers with backoff, reserves each container so concurrent work coalesces, sweeps
the lifecycle history, and owns the `processed map[string]time.Time` dedup (entries expire after `processedTTL`) that skips start events an
enumeration already covered. Every reconcile goes through `processOne` for metrics and logging.

**3. Reconcile (`internal/processor/`)**

`Processor.Reconcile(id)` is the one idempotent path: inspect, compute the complete desired set (`computeDesired`: policy, then
`CollectMountRules` per `/dev` mount, which resolves each name beneath `/dev` with `openat2`, identifies it by `fstat` and sysfs `DEVNAME`,
and aggregates per device), then apply it (`applyPinned`: pidfd pin, `/proc/<pid>/cgroup`, `cgroup.OpenCgroup`, re-verify,
`SetDeviceRules`). Anything uncertain yields the empty set. `Terminate` and `Sweep` revoke and release ended runs (`lifecycle.go`).

**4. Cgroup (`internal/cgroup/`)**

`api.go` defines the `Interface` (`GetDeviceCGroupMountPath`, `GetDeviceCGroupRootPath`, `SetDeviceRules(handle, rules)`) and the
`New(version, ledger, cache)` factory; `handle.go` holds the directory-fd `CgroupHandle`. `v1.go`/`v1set.go`/`ledger.go` write
`devices.allow` and `devices.deny` and track the daemon's own exceptions. `v2.go`, `v2plan.go`, `v2ops.go`, `owned.go` and `ebpf.go` wrap the
runtime's `BPF_CGROUP_DEVICE` filter in an owned block and swap it in with `BPF_F_ALLOW_MULTI` (or `BPF_F_REPLACE`). **Parts of this code are
preserved from NVIDIA (Apache 2.0) — touch with care.**

**Launcher (`cmd/swarm-device-access/launch.go`, `internal/launcher/`)**

`run()` dispatches `launch` before `flag.Parse()` to `runLaunch`, which has its own flag set; the arguments after `--` go to the daemon
verbatim. The Swarm service runs the image unprivileged in this mode. `launcher.Run` finds its own container in `/proc/self/mountinfo`,
takes its `sha256:` image ID, removes a stale `swarm-device-access` daemon only when its owning launcher is confirmed gone (or it came
from the old `sh` wrapper), then creates the daemon from `daemonSpec`, attaches, registers `ContainerWait(removed)`, starts it and
supervises it. Stops go by container ID and share one 20 s shutdown budget.

**Glue (`internal/logger/`, `internal/systemd/`, `internal/observability/`)**

- `logger` — slog wrapper with `text`/`json`/`plain` handlers, `-log-time` strips timestamps via a `ReplaceAttr`. `L()` lazy-inits a default INFO text
  handler so packages can log without explicit wiring.
- `systemd` — DBus `Reloading` signal watcher. Triggers re-apply on the **completion** edge only (signal body `active=false`). The start edge (`true`)
  is mid-reload and races the cgroup wipe. Gracefully degrades when DBus is unreachable.
- `observability` — Prometheus `Recorder` (nil-safe), `/healthz`, `/readyz` and pprof servers.

## Runtime requirements

The daemon **must** run with `privileged: true`, `cgroup: host`, `pid: host`, `userns_mode: host`, and bind mounts for `/var/run/docker.sock`,
`/sys → /host/sys` and `/dev`. That privileged set lives in one place, `daemonSpec` (`internal/launcher/spec.go`), which the launcher uses to
create the daemon; the Swarm service itself (`launch`) needs only the Docker socket. The `hostRootPath = "/host"` constant in `main.go` is the inside-container view of the host root; cgroup paths (and sysfs, when
mounted there) are joined against it. The DBus socket mount is optional — enables reload handling. Mount as `-v /run/dbus/system_bus_socket:/var/run/dbus/system_bus_socket`; the container-side path must be under `/var/run/` because `dhi.io/static` has no `/var/run → /run` symlink.

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
  `internal/launcher`, `deployments/docker/Dockerfile` or anything else under `deployments/**`.
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
not built from these messages: `gh release create --generate-notes` lists the merged pull requests by title. Examples: `fix(processor): skip unresolvable symlinks`, `ci(dependabot): add dhi registry`.

Release versions are derived from the commit types since the last tag (`feat` minor, `fix` patch, `!`/`BREAKING CHANGE` major;
anything else bumps nothing), so a wrong type ships a wrong version. PRs land as merge commits, so every commit counts, not just the
PR title. See [`docs/release.md`](docs/release.md).
