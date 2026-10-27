# AGENTS.md

## What this is

A small **Linux-only** daemon (`//go:build linux` on every Go file under `cmd/` and most of
`internal/`) that injects cgroup BPF device-allow rules into containers that bind-mount `/dev/...`
paths. Solves the Docker-Swarm gap where `devices:` / `device_cgroup_rules:` are silently ignored.

A Rust port is in progress beside the Go code, which keeps shipping until the cutover: a Cargo
workspace with the root package, `meta` (repository meta-tests) and `testenv` (the integration
harness). [`docs/rust-port.md`](docs/rust-port.md) records its design and provenance.

## Common commands

Build / test / vet — uses Make wrappers around `go` and `cargo`:

```bash
make go-build      # cross-compiles to Linux into ./dist/ (works from macOS/Win)
make go-test       # CGO_ENABLED=1 go test -race ./...
make go-vet
make go-vet-integration  # go vet -tags=integration over test/integration (no Docker needed)
make go-lint       # golangci-lint v2, all linters, at the pre-commit pin
make verify-go     # go-build, go-test, go-vet, go-vet-integration, go-lint, golden-check
make golden-check  # the Go golden captures under tests/testdata still match the Go code (goldencapture tag)
make golden-update # rewrite them after a reviewed Go change (docs/rust-port.md, Go golden captures)
make wire-capture  # recapture tests/testdata/engine_wire from the Go binary (needs Docker; wirecapture tag)
make verify-rust   # rust-build, rust-fmt-check, rust-clippy (-D warnings), rust-test (meta-tests included)
make verify        # verify-go, then verify-rust: both while the Go and Rust code live side by side
make rust-fmt      # cargo fmt --all
make rust-deny     # cargo deny --workspace check (network required)
make go-tidy       # go mod tidy + go mod verify
make audit-deps    # govulncheck -test + banned-module check (network required); also runs in CI and as a pre-commit hook on go.mod/go.sum changes
make check         # pre-commit on all files: the authoritative local gate, as CI's pre-commit-all
make check-stage   # pre-commit on staging area only
make pre-commit-install  # install the pre-commit and commit-msg hooks
make docker-build
make docker-run    # runs image locally with required host bind mounts
```

Integration tests (need a live Docker daemon; see [`docs/testing.md`](docs/testing.md)):

```bash
make go-test-integration                   # dry-run tests
make go-test-integration SDA_IT_ENFORCE=1  # plus the real-enforcement test: attaches BPF, runs systemctl daemon-reload; throwaway hosts only
make rust-test-integration                 # the Rust suite (testenv) against the Rust binary and scratch image
make sweep-test-leaks                      # remove containers and images left behind by killed runs
```

Single test:

```bash
go test ./internal/systemd -run TestIsReloadCompleted -v
```

Tests are `//go:build linux`. On macOS/Windows hosts, run them inside the linux image:

```bash
docker run --rm -v $PWD:/work -w /work golang:1.26 go test ./...
```

The Makefile pulls shared snippets from `leinardi/make-common@v1` into `.mk/` on first run. To
refresh: `make mk-common-update`. Repo-local targets go in their own `.mk/<fragment>.mk`, listed in
the Makefile's `MK_LOCAL_FILES`; never add recipes to the Makefile itself.

The Dockerfile hook is droast (Dockerfile Roast), with its policy in `droast.toml`. pre-commit
builds it from source at its pinned rev, which needs a Rust toolchain (`cargo`) locally. Its
required ShellCheck bridge runs the `shellcheck` on PATH, so install ShellCheck locally at the
release `.github/actions/setup-shellcheck` pins (CI installs it with that action).

The meta-tests under `test/` (all untagged, no Docker) guard the tooling itself: `test/toolchain`
(the Go line, the droast and ShellCheck pins, flags that make a gate pass on nothing, Docker kept
out of untagged tests), `test/dockerfiles`, `test/docs` (relative links), `test/suppressions` and
`test/licenseheaders` (the two check scripts), with the helpers in
`test/internal/{gitenv,toolgate}`. `make go-test` runs them; the Dockerfile label test needs a
`droast` binary on PATH (the release `.github/actions/setup-droast` pins; the hook's own build is
not on PATH) and `shellcheck`. `SDA_REQUIRE_TOOLS=1 go test ./test/...` turns a missing droast or
shellcheck into a failure instead of a skip, as CI's `repo-gates` job does.

The meta crate's tests (`meta/tests`, run by `make rust-test`) check the same rules on the Rust
side, plus the Rust boundaries; they need only git and bash, never skip, and carry a negative
control each. The `docs-links` hook runs `cargo test --locked -p meta --test docs`. Both sets stay
until the cutover.

### Generated or vendored: never hand-edit

- `.mk/{docker,help,go,password,pre-commit}.mk` and `scripts/bootstrap-mk-common.sh`: fetched from
  make-common (under its MIT license) and refreshed with `make mk-common-update`; a change is
  ported upstream.
- `internal/cgroup/{api,ebpf,v1,v2}.go` are not vendored, but keep NVIDIA's copyright header
  (see *Conventions worth knowing*).

## Architecture

See [`docs/architecture.md`](docs/architecture.md) for the sequence diagram, BPF program structure,
package layout, and troubleshooting guide.

Four layers, all under `cmd/swarm-device-access` + `internal/`:

**1. Entrypoint and config (`cmd/swarm-device-access/`, `internal/config/`, `internal/policy/`)**

`main.go` parses flags, merges them with the config file (`config.go`: `flagSettings`,
`mergeSettings`, `validate`), refuses to start without `openat2` (`processor.ProbeOpenat2`), creates
the `config.Store`/`Publisher`, and wires the processor and `daemon.Run`. `SIGHUP` re-reads the file
(`reloader.reload`) and publishes through `Processor.PublishAndReconcile`. `internal/config` is the
strict YAML loader and the generation-numbered runtime store; `internal/policy` holds mode, label
parsing and the glob predicates (`Denied`, `Authorized`).

**2. Event loop and coordinator (`internal/daemon/`)**

`Run` subscribes to Docker events (`start`, `unpause`, `die`, `destroy`) **before** the first pass,
starts the coordinator, requests the startup pass, starts the systemd reload watcher, then consumes
events (`listenEvents` → `consumeEvents`, reconnecting with `minBackoff`/`maxBackoff` backoff). The
coordinator (`coordinator.go`) is the single owner of reconciliation passes: startup, `SIGHUP` and
systemd reloads only request one; it retries failed containers with backoff, reserves each container
so concurrent work coalesces, sweeps the lifecycle history, and owns the
`processed map[string]time.Time` dedup (entries expire after `processedTTL`) that skips start events
an enumeration already covered. Every reconcile goes through `processOne` for metrics and logging.

**3. Reconcile (`internal/processor/`)**

`Processor.Reconcile(id)` is the one idempotent path: inspect, compute the complete desired set
(`computeDesired`: policy, then `CollectMountRules` per `/dev` mount, which resolves each name
beneath `/dev` with `openat2`, identifies it by `fstat` and sysfs `DEVNAME`, and aggregates per
device), then apply it (`applyPinned`: pidfd pin, `/proc/<pid>/cgroup`, `cgroup.OpenCgroup`,
re-verify, `SetDeviceRules`). Anything uncertain yields the empty set. `Terminate` and `Sweep`
revoke and release ended runs (`lifecycle.go`).

**4. Cgroup (`internal/cgroup/`)**

`api.go` defines the `Interface` (`GetDeviceCGroupMountPath`, `GetDeviceCGroupRootPath`,
`SetDeviceRules(handle, rules)`) and the `New(version, ledger, cache)` factory; `handle.go` holds
the directory-fd `CgroupHandle`. `v1.go`/`v1set.go`/`ledger.go` write `devices.allow` and
`devices.deny` and track the daemon's own exceptions. `v2.go`, `v2plan.go`, `v2ops.go`, `owned.go`
and `ebpf.go` wrap the runtime's `BPF_CGROUP_DEVICE` filter in an owned block and swap it in with
`BPF_F_ALLOW_MULTI` (or `BPF_F_REPLACE`). **Parts of this code are preserved from NVIDIA (Apache
2.0) — touch with care.**

**Launcher (`cmd/swarm-device-access/launch.go`, `internal/launcher/`)**

`run()` dispatches `launch` before `flag.Parse()` to `runLaunch`, which has its own flag set; the
arguments after `--` go to the daemon verbatim. The Swarm service runs the image unprivileged in
this mode. `launcher.Run` finds its own container in `/proc/self/mountinfo`, takes its `sha256:`
image ID, removes a stale `swarm-device-access` daemon only when its owning launcher is confirmed
gone (or it came from the old `sh` wrapper), then creates the daemon from `daemonSpec`, attaches,
registers `ContainerWait(removed)`, starts it and supervises it. Stops go by container ID and share
one 20 s shutdown budget.

**Glue (`internal/logger/`, `internal/systemd/`, `internal/observability/`)**

- `logger` — slog wrapper with `text`/`json`/`plain` handlers, `-log-time` strips timestamps via a
  `ReplaceAttr`. `L()` lazy-inits a default INFO text handler so packages can log without explicit
  wiring.
- `systemd` — DBus `Reloading` signal watcher. Triggers re-apply on the **completion** edge only
  (signal body `active=false`). The start edge (`true`) is mid-reload and races the cgroup wipe.
  Gracefully degrades when DBus is unreachable.
- `observability` — Prometheus `Recorder` (nil-safe), `/healthz`, `/readyz` and pprof servers.

## Runtime requirements

The daemon **must** run with `privileged: true`, `cgroup: host`, `pid: host`, `userns_mode: host`,
and bind mounts for `/var/run/docker.sock`, `/sys → /host/sys` and `/dev`. That privileged set lives
in one place, `daemonSpec` (`internal/launcher/spec.go`), which the launcher uses to create the
daemon; the Swarm service itself (`launch`) needs only the Docker socket. The
`hostRootPath = "/host"` constant in `main.go` is the inside-container view of the host root; cgroup
paths (and sysfs, when mounted there) are joined against it. The DBus socket mount is optional —
enables reload handling. Mount as `-v /run/dbus/system_bus_socket:/var/run/dbus/system_bus_socket`;
the container-side path must be under `/var/run/` because `dhi.io/static` has no `/var/run → /run`
symlink.

## Conventions worth knowing

- Every file under `cmd/` and most of `internal/` has `//go:build linux`. New code that touches
  devices, cgroups, or `unix.*` should keep that tag. Cross-platform helpers (e.g. logger) do not
  need it.
- Version strings (`version`, `commit`, `date`) live in `cmd/swarm-device-access/version.go` and are
  filled by `-ldflags -X main.version=...` from `GO_LDFLAGS` in `.mk/go.mk`.
- `internal/cgroup/` retains NVIDIA's original copyright header (Apache 2.0). Don't relicense or
  reformat that block.
- The README is the source of truth for the user-facing story; keep flag tables and the
  docker-compose snippet in sync if you change flags or mounts.
- Every Go, Rust and shell source, Dockerfile, the Makefile and the repo-local `.mk` fragments
  carry the Apache-2.0 header (the IntelliJ `Apache-2.0` copyright profile), with the year of the
  file's first commit, after any shebang, build constraint or Dockerfile parser directive.
  `scripts/check-license-headers.sh` (the `check-license-headers` hook) checks the whole tree;
  NVIDIA's four files and their Rust ports (`src/cgroup/{api,ebpf,v1,v2}.rs`) keep NVIDIA's
  header, and make-common's files and Rust fixtures under `testdata/` are exempt. A Rust file
  reused from swarm-scheduler-exporter or swarm-rootless-docker adds the provenance comment of
  [`docs/rust-port.md`](docs/rust-port.md) below the header, complete, and never the MIT block.
- The image's `dhi.io` bases are pinned by tag, not by digest, on purpose (the Dockerfile says
  why); the BuildKit frontend is pinned by digest, and the golang tag names go.mod's exact Go
  version. Dependabot takes only patch releases of golang: a new Go minor moves go.mod and the tag
  together, by hand.

## Project skills

Skills live in `.agents/skills/` (symlinked as `.claude/skills`). Load them before the work, not
after review:

- `go-style-guide` — before any `.go` edit.
- `rust-style-guide` — before any `.rs` edit, and before editing a Cargo manifest, `clippy.toml`,
  `rustfmt.toml`, `rust-toolchain.toml` or `deny.toml`. It must be loaded before the Rust edit,
  not after clippy fails. The design record it points to, with the crates, the intended
  differences from 1.x and the reuse ledger, is [`docs/rust-port.md`](docs/rust-port.md).
- `trust-boundary` — before touching `internal/config`, `internal/policy`, label parsing or rule
  collection in `internal/processor`, `internal/launcher`, `deployments/docker/Dockerfile` or
  anything else under `deployments/**`, or their Rust ports (`src/config`, `src/policy`,
  `src/processor`, `src/launcher`, the CLI and reload path, the HTTP server).
- `adversarial-review` — for any review request ("review my diff", "is this ready to merge").
- `go-to-rust-port` — before planning, implementing or reviewing any part of the Go-to-Rust port.

`go-to-rust-port` is the project copy of a user skill, kept here for the duration of the port.
Every new trap the port finds (a spike, a failed build, a Go probe, a review finding) is added to
it as trap, fix and evidence, versioned where the version matters, in the same commit as the change
that found it. When the port ends, the skill moves back to the user skills and is deleted from this
repository, together with this note.

## Quality rules not enforced by tooling

- **Reuse before writing.** Check `go-style-guide` §18 for an existing helper (`logger.L`,
  `processor.IsMountSource`, `sleepCtx`, the backoff constants, the nil-safe
  `observability.Recorder`) before adding one.
- **Fail closed.** An empty, unknown or malformed mode, config value or label denies or skips — it
  never falls back to wider device access.
- **Deletion smell.** Removing a user-visible surface (flag, key, label, metric, documented
  behavior) and flipping its test to assert absence needs a line in `README.md` or `docs/**` that
  retires it; a commit message is not enough.
- **Suppressions show their work.** A new `//nolint`, `# shellcheck disable=` or
  `# droast ignore=` explains why the fix does not apply here, not which rule fired; droast also
  requires the reason in the directive (`reason="..."`).

## Suppressions are ratcheted

Every in-file suppression is counted per rule against `.suppressions-baseline` by the
`check-suppressions` pre-commit hook (`scripts/check-suppressions.sh`): `//nolint:<linter>`,
each lint of a Rust `#[expect(...)]` or `#[allow(...)]` (not one wrapped in `cfg_attr`),
`# shellcheck disable=`, `# droast ignore=` and `# droast global ignore=`.

Adding one is allowed, but never silently: the hook refuses a commit that adds a suppression line
without changing the baseline in the same commit, including a swap or a moved file that leaves the
count unchanged. Prefer fixing the finding. If a suppression really is right, write it with a
reason, stage it, run `scripts/check-suppressions.sh --update`, and commit the baseline alongside
it. A count that has fallen below its baseline fails the same way, with the same fix: the file
describes the repository, not a past one.

## Validation matrix

Run before considering an edit done. `make check` applies to every row.

| You edited | Run |
| --- | --- |
| `**/*.go` | `make verify-go`, then `make check-stage` |
| `internal/{logger,policy,config,observability}/**`, a `golden_capture_test.go` | `make golden-check` (in `verify-go`); a deliberate change is recaptured with `make golden-update` and its diff reviewed |
| `internal/daemon/**`, `internal/processor/**`, `internal/cgroup/**`, `internal/launcher/**` | also `make go-test-integration` (Docker); `SDA_IT_ENFORCE=1` only on a throwaway host |
| `src/{daemon,processor,cgroup,launcher}/**`, `testenv/**` | also `make rust-test-integration` (Docker) beside `make go-test-integration`; `SDA_IT_ENFORCE=1` only on a throwaway host |
| `go.mod`, `go.sum` | `make go-tidy`, `make audit-deps` (network), `go test ./test/toolchain/` |
| `**/*.rs`, `clippy.toml`, `rustfmt.toml` | `make verify-rust` (load `rust-style-guide` first), then `make check-stage` |
| a `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, `deny.toml` | `make verify-rust`, `make rust-deny` (network) |
| `deployments/docker/Dockerfile`, `droast.toml`, `.dockerignore` | `go test ./test/dockerfiles/ ./test/toolchain/`, `cargo test --locked -p meta --test dockerfiles --test toolchain` (droast runs in `make check`), `make docker-build` |
| `.golangci.yaml` | `make go-lint` |
| `scripts/check-suppressions.sh`, `.suppressions-baseline` | `go test ./test/suppressions/`, `cargo test --locked -p meta --test suppressions` |
| `scripts/check-license-headers.sh` | `go test ./test/licenseheaders/`, `cargo test --locked -p meta --test license_headers` |
| `test/**` | `SDA_REQUIRE_TOOLS=1 go test -count=1 ./test/...`, `make go-lint` |
| `meta/**` | `make verify-rust` (load `rust-style-guide` first) |
| `.github/workflows/**`, `.github/actions/**` | `pre-commit run actionlint --all-files`, `go test ./test/toolchain/` (Go line, Rust release, droast and ShellCheck pins, vacuous flags, required gates and skipped hooks), `cargo test --locked -p meta` |
| `.pre-commit-config.yaml`, `Makefile`, `.mk/**` | `make check`, `go test ./test/toolchain/`, `cargo test --locked -p meta`, `make help` |
| `scripts/hooks/gofmt-on-edit.sh` | shellcheck (in `make check`); no test package: run it once by editing a `.go` file in Claude Code |
| `scripts/hooks/rustfmt-on-edit.sh` | shellcheck (in `make check`); no test package: run it once by editing a `.rs` file in Claude Code |
| `README.md`, `docs/**/*.md`, other Markdown | `cargo test --locked -p meta --test docs` (the `docs-links` hook), `go test -count=1 ./test/docs/`; prose wraps at 100 columns (markdownlint in `make check`) |
| `.agents/skills/**` | the Markdown row |

## Commit messages

All commits MUST be Conventional Commits 1.0.0 **with a scope**:
`<type>(<scope>)[!]: <description>`, optional blank-line body and footers. Enforced by the
`conventional-pre-commit` `commit-msg` hook (`--force-scope`) and by the `conventional-commits` CI
job. Types: `feat`, `fix`, `docs`, `test`, `refactor`, `perf`, `build`, `ci`, `chore`, `style`,
`revert`. Breaking changes use `!` before `:` or a `BREAKING CHANGE:` footer. Release notes are not
built from these messages: `gh release create --generate-notes` lists the merged pull requests by
title. Examples: `fix(processor): skip unresolvable symlinks`, `ci(dependabot): add dhi registry`.

Release versions are derived by `svu` from the commits since the last tag, so a wrong type ships a
wrong version:

| Release | Commit | Example |
| --- | --- | --- |
| major | any type with `!` before the colon, or a `BREAKING CHANGE:` footer | `feat(config)!: drop the legacy key` |
| minor | `feat` | `feat(policy): add a device allowlist` |
| patch | `fix` | `fix(processor): skip dangling links` |
| none | everything else: `perf`, `refactor`, `build`, `ci`, `chore`, `docs`, `style`, `test`, `revert` | `perf(daemon): ...` |

The highest bump among the commits wins; with only "none" commits since the last tag, a release with
no version fails with "nothing to bump".

**Pick the type by whether the change should ship, not by what kind of change it is.** Anything that
changes the shipped binary or image and that users should receive is `fix` (or `feat`), even when it
is a performance improvement, a refactor or a revert:
`fix(daemon): arm the prune timer only while it has entries`, not `perf(daemon): ...`. Use `perf`,
`refactor`, `style` and `revert` only when the commit is deliberately not meant to trigger a release
on its own. A `revert` of a shipped `feat` or `fix` is itself a `fix`. `svu` matches `feat`/`fix`
anywhere in the subject (e.g. `prefix:` counts as `fix:`), so avoid a word ending in `feat` or `fix`
directly before a colon in other subjects. PRs land as merge commits, so every commit counts, not
just the PR title. See [`docs/release.md`](docs/release.md).
