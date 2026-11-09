# AGENTS.md

## What this is

A small **Linux-only** daemon that injects cgroup BPF device-allow rules into containers that
bind-mount `/dev/...` paths. Solves the Docker-Swarm gap where `devices:` / `device_cgroup_rules:`
are silently ignored.

Since 2.0 it is written in Rust (the Go 1.x code was removed at the cutover): a Cargo workspace
with the root package (the `swarm-device-access` library and binary), `meta` (repository
meta-tests) and `testenv` (the integration harness). It calls Linux interfaces (`openat2`, pidfds,
`bpf(2)`, cgroup v1 and v2) with no fallback for other systems, so it builds and its tests run on
Linux only. The code it shares with the other `leinardi/*` Rust services comes from
[leinardi/rust-common](https://github.com/leinardi/rust-common) (see *Shared crates*).
[`docs/rust-port.md`](docs/rust-port.md) is the permanent record of its design, its crates and the
provenance of every reused component. The 1.x behaviour it keeps, and the numbered differences
from it (Q1 and later), are listed in the README's
[Migrating from 1.x](README.md#-migrating-from-1x).

## Common commands

Build / test / lint — uses Make wrappers around `cargo`:

```bash
make verify-rust   # rust-build, rust-fmt-check, rust-clippy (-D warnings), rust-test, rust-doc
make verify        # verify-rust: every check a change has to pass
make rust-build    # cargo build --workspace --all-targets --all-features --locked, unstamped (as rust-test)
make rust-test     # cargo test --workspace --locked (meta-tests included)
make rust-clippy   # cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
make rust-fmt      # cargo fmt --all
make rust-fmt-check  # cargo fmt --all --check
make rust-doc      # cargo doc --workspace --all-features --no-deps --locked, warnings as errors
make rust-deny     # cargo deny --workspace check (network required)
make audit-deps    # cargo deny --workspace check advisories (network required)
make rust-release-binaries  # static musl amd64 and arm64 release binaries into dist/ (pinned zig and cargo-zigbuild on PATH; RUST_RELEASE_PLATFORMS for a subset)
make rust-release-checksums # dist/checksums.txt over both release binaries
make rust-release-dist      # rust-release-binaries, then rust-release-checksums (ordered, safe under make -j)
make image-build-prebuilt   # the image's prebuilt target from dist/, for IMAGE_PLATFORM (default: the host's; needs a dhi.io login)
make image-test             # scripts/image-test.sh on that image, IMAGE_TEST_TIER full (default), reduced or smoke
make image-build-test       # image-build-prebuilt, then image-test (ordered, safe under make -j)
make check         # pre-commit on all files: the authoritative local gate, as CI's pre-commit-all
make check-stage   # pre-commit on staging area only
make pre-commit-install  # install the pre-commit and commit-msg hooks
make docker-build  # the image's default target, which compiles the binary itself (needs a docker login dhi.io)
make docker-run    # runs image locally with required host bind mounts
make help          # every target with its one-line description
```

Integration tests (need a live Docker daemon and a `docker login dhi.io`; see
[`docs/testing.md`](docs/testing.md)):

```bash
make test-integration                   # the testenv suite against the image built from the Dockerfile and its binary; dry-run tests
make test-integration SDA_IT_ENFORCE=1  # plus the enforcement tests and the Go-to-Rust upgrade gate (pinned 1.x image; sudo -n); throwaway hosts only
make sweep-test-leaks                   # remove containers and images left behind by killed runs
```

Single test:

```bash
cargo test --locked --lib systemd::tests
```

The Makefile pulls shared snippets from `leinardi/make-common@v1` into `.mk/` on first run. To
refresh: `make mk-common-update`. Repo-local targets go in their own `.mk/<fragment>.mk`, listed in
the Makefile's `MK_LOCAL_FILES`; never add recipes to the Makefile itself.

The Dockerfile hook is droast (Dockerfile Roast), with its policy in `droast.toml`. pre-commit
builds it from source at its pinned rev, which needs a Rust toolchain (`cargo`) locally. Its
required ShellCheck bridge runs the `shellcheck` on PATH, so install ShellCheck locally at the
release the `shellcheck-py` hook wraps (CI installs it with gh-composite-actions'
`setup-shellcheck`).

The static Rust builds (`make rust-release-binaries`) link with zig through cargo-zigbuild and
refuse any release on PATH but the ones `.mk/rust-release.mk` pins. That file records the zig,
cargo-zigbuild and `rust-std` pins; the Dockerfile's build stage carries copies, and CI and the
release pass the zig and cargo-zigbuild versions to gh-composite-actions' `setup-zigbuild`, all of
which `meta/tests/toolchain.rs` keeps equal. CI and the release build the binaries with this one
target, and the image's `prebuilt` target COPYs them (`make image-build-prebuilt`), so the image
binary is the release asset byte for byte; `scripts/image-test.sh` tests that image in three tiers
(CI's full amd64 and reduced arm64 suites, and the release's smoke test).

The meta crate's tests (`meta/tests`, run by `make rust-test`) guard the tooling itself: the pins
and Trivy never pinned, the Dockerfile's frontend and its five stages (two targets, `runtime` and
`prebuilt`, on one tag-pinned `base`), the CI gates, flags that make a gate pass on nothing, and the
Rust boundaries. Most run rust-common's `repo-meta` checks with this repository's parameters (a
scratch repository runs git through `testenv_core::gitenv::isolate`); the boundaries are local. They
need only git and bash, never skip, and carry a negative control each. The `lychee` hook checks the
relative links and fragments in the Markdown.

### Generated or vendored: never hand-edit

- `.mk/{docker,help,password,pre-commit,rust}.mk` and `scripts/bootstrap-mk-common.sh`: fetched from
  make-common (under its MIT license) and refreshed with `make mk-common-update`; a change is
  ported upstream.
- `src/cgroup/{api,ebpf,v1,v2}.rs` are not vendored, but keep NVIDIA's copyright header
  (see *Conventions worth knowing*).

## Architecture

See [`docs/architecture.md`](docs/architecture.md) for the sequence diagram, BPF program structure,
module layout, and troubleshooting guide.

Four layers, all under `src/`:

**1. Entrypoint and config (`src/main.rs`, `src/run.rs`, `src/cli.rs`, `src/cli/settings.rs`,
`src/config/`, `src/policy/`)**

`main.rs` builds the tokio runtime with service-kit's `build_runtime` (two workers) and runs
`run::run_with` under `run_until_exit`, which bounds the runtime's shutdown by the returned
`Exit`'s deadline. `run_with` dispatches a leading `launch` to the launcher, otherwise parses the
flags (`cli::parse`, double-dash only, Q1), merges them with the config file (`Settings::merge`,
`Settings::validate`), refuses to start without `openat2` (`devfs::probe_openat2`), raises
`RLIMIT_MEMLOCK`, creates the `config::Store`/`Publisher`, and wires the Docker client, the
metrics server, the processor and `daemon::run`. One shutdown budget (`SHUTDOWN_BUDGET`) runs from
the first `SIGINT` or `SIGTERM` (service-kit's `shutdown_signal`, its handlers installed when
called, where Go calls `signal.NotifyContext`); the `SIGHUP` channel is service-kit's
`hangup_channel`, registered once the flags are parsed. `SIGHUP` re-reads the file
(`Reloader::reload`, `watch_sighup`) and publishes through `Processor::publish_and_reconcile`.
`src/config` is the strict YAML loader and the generation-numbered runtime store; `src/policy`
holds the mode, label parsing and the glob predicates (`Global::denied`, `Global::authorized`).

**2. Event loop and coordinator (`src/daemon/`)**

`daemon::run` subscribes to Docker events (`start`, `unpause`, `die`, `destroy`) **before** the
first pass, starts the coordinator, requests the startup pass, starts the systemd reload watcher
(`systemd::supervise`) on its own task, then consumes events (`listen_events` →
`consume_events` in `events.rs`, reconnecting with `MIN_BACKOFF`/`MAX_BACKOFF` backoff). The
`Coordinator` (`coordinator.rs`) is the single owner of reconciliation passes: startup, `SIGHUP`
and systemd reloads only request one; it retries failed containers with backoff, reserves each
container so concurrent work coalesces, sweeps the lifecycle history, and owns the `processed`
dedup (entries expire after `PROCESSED_TTL`) that skips start events an enumeration already
covered. Every reconcile goes through `process_one` for metrics and logging.

**3. Reconcile (`src/processor/`)**

`Processor::reconcile(id)` (`reconcile.rs`) is the one idempotent path: inspect, compute the
complete desired set (`compute_desired`: policy, then `collect_mount_rules` (`rules.rs`) per `/dev`
mount, which resolves each name beneath `/dev` with `openat2` (`devfs.rs`), identifies it by
`fstat` and sysfs `DEVNAME`, and aggregates per device), then apply it (`apply_pinned`: pidfd pin
(`pin.rs`), `/proc/<pid>/cgroup`, `CgroupHandle::open`, re-verify, `set_device_rules`). The cgroup
and BPF mutation runs as one blocking transaction under the processor lock, awaited to completion
even when its caller is dropped. Anything uncertain yields the empty set. `terminate` and `sweep`
revoke and release ended runs (`lifecycle.rs`).

**4. Cgroup (`src/cgroup/`)**

`api.rs` defines `DeviceRule`, the `Interface` trait (`set_device_rules(handle, rules)`), the
`new(version, ledger, cache, log)` factory and the `/proc` cgroup parsing; `handle.rs` holds the
directory-fd `CgroupHandle`. `v1.rs`/`v1set.rs`/`ledger.rs` write `devices.allow` and
`devices.deny` and track the daemon's own exceptions. `v2.rs`, `v2plan.rs`, `v2ops.rs`, `owned.rs`,
`ebpf.rs` and `asm.rs` wrap the runtime's `BPF_CGROUP_DEVICE` filter in an owned block and swap it
in with `BPF_F_ALLOW_MULTI` (or `BPF_F_REPLACE`); `bpf_sys.rs` issues the raw `bpf(2)` commands and
is the only module allowed `unsafe`. **Parts of this code are ported from NVIDIA (Apache 2.0) —
touch with care.**

**Launcher (`src/launcher/`)**

`run::run_with` dispatches a leading `launch` to the launcher, which has its own flag set
(`cli::parse_launch`); the arguments after `--` go to the daemon verbatim. The Swarm service runs
the image unprivileged in this mode. The launcher finds its own container in
`/proc/self/mountinfo` (`self_id.rs`), takes its `sha256:` image ID, removes a stale
`swarm-device-access` daemon only when its owning launcher is confirmed gone, or it came from the
old `sh` wrapper (`stale.rs`), then creates the daemon from `daemon_spec` (`spec.rs`), attaches,
registers the wait for `removed`, starts it and supervises it. Its Engine calls are the project's
only Docker writes (`engine.rs`). Stops go by container ID and share one 20 s shutdown budget.

**Glue (`src/docker/`, `src/systemd.rs`, `src/metrics/`, `src/server.rs`, `src/version.rs`, and
the rust-common crates)**

- `docker` — the daemon's read-only `DockerApi` (ping, list, inspect, events: `GET` plus one
  `GET /_ping`) and its types on docker-client's bollard client, over the configured unix socket
  only;
  `Client::transport` hands that client to the launcher's engine alone.
- logging (`src/logging.rs`) — `tracing` events (`info!`, `warn!`, ... with `key = value` fields,
  `error = %err` for an error) through rust-common's `tracing-setup`: tracing-subscriber's `text`
  lines or flat `json`, to stdout, without the target. The daemon's subscriber is reloadable
  (`build_reloadable`; `SIGHUP` calls `ReloadHandle::reconfigure` for the format and the level),
  `--log-time` is `Options::time` and fixed at startup, and the launcher installs its own fixed
  one without a timestamp and puts `component = launcher` on each of its events. The blocking
  transactions run under `logging::inherit`, so they log where their caller does. Tests capture
  with `test_support::capture_logs` (`tracing_setup::testing::Capture` under a scoped default),
  never a global subscriber.
- `systemd` — D-Bus `Reloading` signal watcher (zbus). Triggers re-apply on the **completion** edge
  only (signal body `active=false`). The start edge (`true`) is mid-reload and races the cgroup
  wipe. Degrades to retrying in the background when D-Bus is unreachable.
- `metrics`, `server` — the Prometheus `Recorder` (with a no-op form) on prometheus-client's
  registry (typed label sets, counters registered without `_total`, service-kit's
  `ProcessCollector`), exposed as OpenMetrics text, and the `/metrics`, `/healthz` and `/readyz`
  routes as an axum router on service-kit's `http` server (a 5 s header-read timeout, a 30 s
  write-stall timeout and a 5 s drain). There is no pprof server (Q2).
- clock — the one wall-clock read (`SystemClock`), behind the injected `Clock`. Settings, labels,
  policy globs and `/proc` lines are read with std and the `glob` crate (`policy::glob`, `text`,
  `lexpath`).

## Shared crates (rust-common)

`clock`, `docker-client`,
`service-kit`, `tracing-setup`, `testenv-core` and `repo-meta` come from
[leinardi/rust-common](https://github.com/leinardi/rust-common), a git dependency pinned by the
release tag (`tag = "v1.0.0"`, with `version = "1.0"`, the tag's `<major>.<minor>`) in the root
`Cargo.toml`'s `[workspace.dependencies]`; each member opts in with `<name>.workspace = true` and
its own features.

- **Shared code lives in rust-common.** Never copy a shared crate's code back into this repository.
  A behaviour difference between projects is a parameter of the shared crate, never a fork here:
  this daemon passes `Options` with no target and no span fields (tracing-setup),
  a `swarm-device-access/<version>` `User-Agent` and a response-header timeout with only the
  `attach` feature (docker-client), its envid,
  labels and resources (`testenv/src/config.rs`) and runner settings
  (`testenv/tests/integration/main.rs`) to testenv-core, and
  `buildinfo!("SWARM_DEVICE_ACCESS", …)` (service-kit).
- **A fix to shared code lands in rust-common first**, then reaches this repository in a release,
  by moving the `tag` (and `version` with it) on every rust-common line, with the ledger in
  [`docs/rust-port.md`](docs/rust-port.md) updated in the same commit. Refresh the lock with
  `cargo fetch`, never `cargo update -p`; `git diff Cargo.lock` must touch only rust-common lines.
- **Local iteration on a shared crate** uses an uncommitted
  `[patch."https://github.com/leinardi/rust-common"]` table in the root `Cargo.toml`, one
  `path = "../rust-common/crates/<name>"` line for every rust-common crate the workspace uses,
  directly or through another. It is never committed: CI cannot see the path.

This repository's intentional differences from rust-common's and the other services' settings:

- `unsafe_code = "deny"`, not `forbid`: `src/cgroup/bpf_sys.rs` has to lift it for the raw
  `bpf(2)` calls, and only `#[expect]` can lift a denied lint (`forbid` cannot be lifted at all).
- `panic = "abort"` in the release profile, where swarm-scheduler-exporter unwinds for its
  `catch_unwind`: the Go daemon has no `recover`, so a panic crashes the process and Swarm or the
  launcher restarts it; a task panic under unwind would leave a daemon that looks alive and does
  nothing.

## Runtime requirements

The daemon **must** run with `privileged: true`, `cgroup: host`, `pid: host`, `userns_mode: host`,
and bind mounts for `/var/run/docker.sock`, `/sys → /host/sys` and `/dev`. That privileged set lives
in one place, `daemon_spec` (`src/launcher/spec.rs`), which the launcher uses to create the daemon;
the Swarm service itself (`launch`) needs only the Docker socket. The `HOST_ROOT_PATH = "/host"`
constant in `src/run.rs` is the inside-container view of the host root; cgroup paths (and sysfs,
when mounted there) are joined against it. The DBus socket mount is optional — enables reload
handling. Mount as `-v /run/dbus/system_bus_socket:/var/run/dbus/system_bus_socket`; the
container-side path must be under `/var/run/` because `dhi.io/static` has no `/var/run → /run`
symlink.

## Conventions worth knowing

- The crate is Linux-only (see *What this is*). Syscalls go through rustix; `libc` and `unsafe` are
  confined to `src/cgroup/bpf_sys.rs`.
- Version strings (`BUILD.version`, `BUILD.commit`, `BUILD.date`) live in `src/version.rs`, read
  at compile time by service-kit's `buildinfo!` from `SWARM_DEVICE_ACCESS_VERSION`,
  `SWARM_DEVICE_ACCESS_COMMIT` and `SWARM_DEVICE_ACCESS_DATE`, which `RUST_VERSION_ENV` in
  make-common's `.mk/rust.mk` sets from git (the date is HEAD's commit date, `GIT_COMMIT_DATE`)
  under the Makefile's `RUST_STAMP_PREFIX` (`make rust-dist`, `make rust-run`,
  `make rust-release-binaries`, to which CI and the release pass `GIT_VERSION`, `GIT_COMMIT` and
  `BUILD_DATE`: the tag, the commit's first seven characters and its commit date; `make rust-build`
  builds unstamped, as the tests do, through the Makefile's `RUST_BUILD_ENV :=`) and the
  Dockerfile's build stage sets from its `VERSION`, `COMMIT` and `DATE` build arguments, which also
  fill the image labels of both targets; unset, they read `dev`, `none` and `unknown`, and an empty
  stamp stays empty (`EmptyStamp::Kept`).
- `src/cgroup/{api,ebpf,v1,v2}.rs` retain NVIDIA's original copyright header (Apache 2.0). Don't
  relicense or reformat that block.
- The README is the source of truth for the user-facing story; keep the flag tables
  (`src/cli.rs`), the config keys (`src/config`), the label table (`src/policy`) and the
  docker-compose snippet in sync if you change flags, keys, labels or mounts.
- Log messages, attribute keys and levels, error texts, exit codes, metric families and Engine
  requests are the 1.x contract. A difference that cannot be avoided gets a numbered Q entry in the
  README's Migrating from 1.x table in the same commit; any other difference is a defect.
- Every Rust and shell source, Dockerfile, the Makefile and the repo-local `.mk` fragments carry the
  Apache-2.0 header (the IntelliJ `Apache-2.0` copyright profile), with the year of the file's first
  commit, after any shebang or Dockerfile parser directive. `scripts/check-license-headers.sh` (the
  `check-license-headers` hook) checks the whole tree; `src/cgroup/{api,ebpf,v1,v2}.rs` keep
  NVIDIA's header, and make-common's files and Rust fixtures under `testdata/` are exempt. A Rust
  file reused from swarm-scheduler-exporter or swarm-rootless-docker adds the provenance comment of
  [`docs/rust-port.md`](docs/rust-port.md) below the header, complete, and never the MIT block.
- The image's `dhi.io` bases are pinned by tag, not by digest, on purpose (dhi.io rebuilds its
  tags too often; the Dockerfile says why, and each release records the base digest it shipped,
  read from the build's provenance); the BuildKit frontend is pinned by digest, and the rust tag
  names rust-toolchain.toml's exact release. Dependabot moves the `dhi.io/static` tag as
  `fix(deps)`, and ignores the rust image: a new Rust release moves rust-toolchain.toml, the tag,
  `RUST_VERSION` and the `rust-std` digests together, by hand.

## GitHub conventions

Shared by every `leinardi/*` repository.

- **Workflows:** `ci.yaml` (`pre-commit-all`, `rust`, `image`, `conventional-commits` and the
  `ci-result` aggregator; the reviewdog jobs on pull requests), `integration.yaml` (the testenv
  suite against a real Docker), `release.yaml` (`guard`, `ci-green`, `audit`, `release`;
  [`docs/release.md`](docs/release.md)), `scheduled-scan.yaml` (weekly: `trivy-gate` on the
  published `:latest` and cargo-deny on `master`) and `pre-commit-warmup.yaml`. Shared steps come
  from [gh-composite-actions](https://github.com/leinardi/gh-composite-actions), pinned by SHA.
  `.trivyignore.yaml` holds the scans' accepted findings.
- **Jobs:** every job sets `timeout-minutes` (a composite action cannot time out its own steps).
  Every checkout that pushes nothing sets `persist-credentials: false`; the release's checkout
  sets it too, and its tag push passes the token to that one `git push`. Every job that holds a
  write permission or `id-token` runs in the `release` environment.
- **Concurrency:** CI (and `integration.yaml`) groups a pull request's runs by
  `${{ github.workflow }} @ <head label>` and cancels the older one; every other run (a push to
  the default branch, a dispatch) gets its own group, `${{ github.run_id }}`, because GitHub keeps
  one pending run per group and the release gate needs CI on the exact commit (see `ci.yaml`). A
  warm-up uses `<name>-${{ github.ref }}`, never a static group. The release uses the one
  `release` group and is never cancelled.
- **Caches:** the default branch is the only writer, because every pull request can restore its
  caches and the default branch cannot restore a pull request's. The pre-commit cache is saved
  only by `pre-commit-warmup.yaml`; CI restores it with `actions/cache/restore` under the same key,
  `pre-commit|<OS>|<resolved Python version>|<hash of .pre-commit-config.yaml>`. The Rust cache is
  `Swatinem/rust-cache` with `shared-key: ci`, saved on `master` only (the `image` job under its
  own `key: static`, `integration.yaml` under `key: integration`); the release writes its own,
  `prefix-key: release`. No image build uses a layer cache: the prebuilt target is the base plus
  one COPY.
- **pre-commit** is pinned to the same version in CI, the warm-up and the
  `gha-pre-commit-reviewdog-actions` jobs.
- **Release:** `release.yaml` is a `workflow_dispatch` with an optional `version` and a `dry_run`,
  built from gh-composite-actions (`release-version`, `require-ci-green`, `require-job-success`,
  `release-mode`, `toolchain-record`, `toolchain-recover`, `check-pinned-bases`, `trivy-gate`,
  `binary-attestation-status`, `setup-zigbuild`, `setup-cargo-deny`). svu derives the version from
  the commits only when none is given, and is installed only then, so a version entered by hand
  releases even when GitHub's release downloads fail. Trivy is never pinned: `trivy-gate` installs
  its latest release.
- **Dependabot:** monthly, `cooldown: default-days: 7`, labels `dependencies` and `ci`, one group
  per ecosystem. The shipped ecosystems use `fix(deps)`: `cargo` (with `prefix-development:
  chore(deps)` and separate production and development groups) and `docker` (the runtime base's
  tag); GitHub Actions and pre-commit use `chore(deps)`.
- **Issue and pull request templates:** keep the Step 1/2/3 structure of template-default, with
  this repository's project name, version command, environment block, snippet language and extra
  checkboxes. The pull request checklist always keeps the Conventional Commits and
  validation-matrix items.

## Project skills

Skills live in `.agents/skills/` (symlinked as `.claude/skills`). Load them before the work, not
after review:

- `trust-boundary` — before touching `src/config`, `src/policy`, label parsing or rule collection
  in `src/processor`, the CLI and `SIGHUP` reload path (`src/cli.rs`, `src/cli/settings.rs`,
  `src/run.rs`), `src/launcher`, the HTTP server (`src/server.rs`, `src/server/`),
  `deployments/docker/Dockerfile` or anything else under `deployments/**`, or `examples/**`.
- `adversarial-review` — for any review request ("review my diff", "is this ready to merge").

`adversarial-review` points at [`docs/rust-port.md`](docs/rust-port.md) as
the reuse ledger, with the crates, the design decisions and the provenance of every component
reused from swarm-scheduler-exporter or swarm-rootless-docker. Check the ledger, and update its row
in the same commit, when a reused component changes or an upstream fix is taken.

## Quality rules not enforced by tooling

- **Reuse before writing.** Check the workspace, the rust-common crates and the reuse ledger in
  [`docs/rust-port.md`](docs/rust-port.md) for an existing helper or component (the `LogHandle`,
  `processor::rules::is_mount_source`, the injected `Clock`, the backoff constants, the `Recorder`
  and its no-op form, `lexpath`, `text`, `test_support`) before adding one. A helper every project would
  want goes to rust-common, not here.
- **Fail closed.** An empty, unknown or malformed mode, config value or label denies or skips — it
  never falls back to wider device access.
- **Deletion smell.** Removing a user-visible surface (flag, key, label, metric, documented
  behavior) and flipping its test to assert absence needs a line in `README.md` or `docs/**` that
  retires it; a commit message is not enough.
- **Suppressions show their work.** A new `#[expect(...)]`, `# shellcheck disable=` or
  `# droast ignore=` explains why the fix does not apply here, not which rule fired; `#[expect]`
  carries it in `reason = "..."` and droast in `reason="..."`.

## Validation matrix

Run before considering an edit done. `make check` applies to every row.

| You edited | Run |
| --- | --- |
| `**/*.rs`, `clippy.toml`, `rustfmt.toml` | `make verify`, then `make check-stage` |
| `src/{daemon,processor,cgroup,launcher}/**`, `testenv/**` | also `make test-integration` (Docker); `SDA_IT_ENFORCE=1` only on a throwaway host |
| `tests/testdata/**` (the 1.x captures and fixtures) | `make verify`; they are frozen 1.x captures (docs/rust-port.md, 1.x fixtures): never regenerate one by hand |
| a `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, `deny.toml`, a rust-common `tag` bump | `make verify`, `make rust-deny`; both need the network (cargo fetches the new git checkout and crates, cargo-deny the advisories). Iterate on a shared crate with the uncommitted `[patch]` table (*Shared crates*), never committed |
| `deployments/docker/Dockerfile`, `droast.toml`, `.dockerignore` | `cargo test --locked -p meta --test dockerfiles --test toolchain` (droast runs in `make check`); gh-composite-actions' `check-pinned-bases` with `--tag-only dhi.io` on the Dockerfile; `make rust-release-dist`, then `make image-build-test`, and with QEMU `make image-build-test IMAGE_PLATFORM=linux/arm64 IMAGE_TEST_TIER=reduced`; `make docker-build` (all need a `dhi.io` login) |
| `.mk/rust-release.mk`, `.mk/image.mk`, `scripts/image-test.sh` | `make rust-release-dist`, then `make image-build-test` (pinned zig on PATH) and the arm64 run above, `cargo test --locked -p meta --test toolchain`; keep `docs/release.md#what-ci-proves` true |
| `.trivyignore.yaml` | every entry scoped to paths or purls, with a statement and an `expired_at`; scan the prebuilt image with the latest Trivy as `trivy-gate` runs it (`--ignore-unfixed --severity HIGH,CRITICAL --ignorefile .trivyignore.yaml`) |
| `scripts/check-license-headers.sh` | `pre-commit run check-license-headers --all-files` |
| `meta/**` | `make verify` |
| `.github/workflows/**`, `.github/dependabot.yml` | `pre-commit run actionlint --all-files`, `cargo test --locked -p meta` (Rust release, zig, rust-std and ShellCheck pins, Trivy never pinned, vacuous flags, required gates and skipped hooks); reason through each trigger the workflow has (pull request, fork pull request, push, dispatch, schedule); for `release.yaml`, keep `docs/release.md` in step and dispatch a dry run once CI passed on the commit; for `ci.yaml`'s `image` job, keep `docs/release.md#what-ci-proves` true |
| `.pre-commit-config.yaml`, `Makefile`, `.mk/**` | `make check`, `cargo test --locked -p meta`, `make help` |
| `scripts/hooks/rustfmt-on-edit.sh` | shellcheck (in `make check`); no test package: run it once by editing a `.rs` file in Claude Code |
| `README.md`, `docs/**/*.md`, other Markdown | the `lychee` hook (`--offline --include-fragments`); prose wraps at 100 columns (markdownlint in `make check`) |
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
