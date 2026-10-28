# AGENTS.md

## What this is

A small **Linux-only** daemon that injects cgroup BPF device-allow rules into containers that
bind-mount `/dev/...` paths. Solves the Docker-Swarm gap where `devices:` / `device_cgroup_rules:`
are silently ignored.

Since 2.0 it is written in Rust (the Go 1.x code was removed at the cutover): a Cargo workspace
with the root package (the `swarm-device-access` library and binary), `meta` (repository
meta-tests) and `testenv` (the integration harness). It calls Linux interfaces (`openat2`, pidfds,
`bpf(2)`, cgroup v1 and v2) with no fallback for other systems, so it builds and its tests run on
Linux only. [`docs/rust-port.md`](docs/rust-port.md) is the permanent record of its design, its
crates and the provenance of every reused component. The 1.x behaviour it keeps, and the numbered
differences from it (Q1 and later), are listed in the README's
[Migrating from 1.x](README.md#-migrating-from-1x).

## Common commands

Build / test / lint — uses Make wrappers around `cargo`:

```bash
make verify-rust   # rust-build, rust-fmt-check, rust-clippy (-D warnings), rust-test (meta-tests included)
make verify        # verify-rust: every check a change has to pass
make rust-build    # cargo build --workspace --all-targets --all-features --locked, with the version stamps
make rust-test     # cargo test --workspace --locked (meta-tests included)
make rust-clippy   # cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
make rust-fmt      # cargo fmt --all
make rust-fmt-check  # cargo fmt --all --check
make rust-deny     # cargo deny --workspace check (network required)
make audit-deps    # cargo deny --workspace check advisories (network required)
make rust-release-binaries  # static musl amd64 and arm64 release binaries and checksums.txt into dist/ (pinned zig and cargo-zigbuild on PATH)
make check         # pre-commit on all files: the authoritative local gate, as CI's pre-commit-all
make check-stage   # pre-commit on staging area only
make pre-commit-install  # install the pre-commit and commit-msg hooks
make docker-build  # the image, from deployments/docker/Dockerfile (needs a docker login dhi.io)
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
release `.github/actions/setup-shellcheck` pins (CI installs it with that action).

The static Rust builds (`make rust-release-binaries`) link with zig through cargo-zigbuild and
refuse any release on PATH but the ones `.github/actions/setup-zigbuild` pins. `.mk/rust-release.mk`
records the zig, cargo-zigbuild and `rust-std` pins; the Dockerfile's build stage carries copies,
which `meta/tests/toolchain.rs` keeps equal.

The meta crate's tests (`meta/tests`, run by `make rust-test`) guard the tooling itself: the pins,
the Dockerfile, the CI gates, flags that make a gate pass on nothing, Docker kept out of the plain
tests, relative links, and the two check scripts, plus the Rust boundaries. They need only git and
bash, never skip, and carry a negative control each. The `docs-links` hook runs
`cargo test --locked -p meta --test docs`.

### Generated or vendored: never hand-edit

- `.mk/{docker,help,password,pre-commit}.mk` and `scripts/bootstrap-mk-common.sh`: fetched from
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

`main.rs` builds the tokio runtime by hand (two workers) and runs `run::run_with`, which dispatches
a leading `launch` to the launcher, otherwise parses the flags (`cli::parse`, double-dash only,
Q1), merges them with the config file (`Settings::merge`, `Settings::validate`), refuses to start
without `openat2` (`devfs::probe_openat2`), raises `RLIMIT_MEMLOCK`, creates the
`config::Store`/`Publisher`, and wires the Docker client, the metrics server, the processor and
`daemon::run`. One shutdown budget (`SHUTDOWN_BUDGET`) runs from the first `SIGINT` or `SIGTERM`.
`SIGHUP` re-reads the file (`Reloader::reload`, `watch_sighup`) and publishes through
`Processor::publish_and_reconcile`. `src/config` is the strict YAML loader and the
generation-numbered runtime store; `src/policy` holds the mode, label parsing and the glob
predicates (`Global::denied`, `Global::authorized`).

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

**Glue (`src/docker/`, `src/logger/`, `src/systemd.rs`, `src/metrics/`, `src/server.rs`,
`src/go_compat/`, `src/clock.rs`)**

- `docker` — the daemon's read-only Engine client on hyper (ping, list, inspect, events: `GET`
  plus one `HEAD /_ping`), over the configured unix socket only.
- `logger` — slog's `text` and `json` handlers and the project's `plain` handler, byte for byte;
  components log through a cloned `LogHandle`, which a reconfigure swaps behind the shared cell.
  `--log-time` controls the timestamp.
- `systemd` — D-Bus `Reloading` signal watcher (zbus). Triggers re-apply on the **completion** edge
  only (signal body `active=false`). The start edge (`true`) is mid-reload and races the cgroup
  wipe. Degrades to retrying in the background when D-Bus is unreachable.
- `metrics`, `server` — the Prometheus `Recorder` (with a no-op form) and the `/metrics`,
  `/healthz` and `/readyz` server. There is no pprof server (Q2).
- `go_compat` — the Go standard library's formatting and parsing the 1.x log, error and flag text
  depends on; `clock` — the one wall-clock read, behind the injected `Clock`.

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
  confined to `src/cgroup/bpf_sys.rs`, which a meta-test pins.
- Version strings (`VERSION`, `COMMIT`, `DATE`) live in `src/version.rs` and are read at compile
  time from `SWARM_DEVICE_ACCESS_VERSION`, `SWARM_DEVICE_ACCESS_COMMIT` and
  `SWARM_DEVICE_ACCESS_DATE`, which `RUST_VERSION_ENV` in `.mk/rust.mk` sets from git and the clock
  (`make rust-build`, `make rust-release-binaries`) and the Dockerfile sets from its `VERSION`,
  `COMMIT` and `DATE` build arguments; unset, they read `dev`, `none` and `unknown`.
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
- The image's `dhi.io` bases are pinned by tag, not by digest, on purpose (the Dockerfile says
  why); the BuildKit frontend is pinned by digest, and the rust tag names rust-toolchain.toml's
  exact release. Dependabot ignores the rust image: a new Rust release moves rust-toolchain.toml,
  the tag, `RUST_VERSION` and the `rust-std` digests together, by hand.

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

- **Reuse before writing.** Check the workspace and the reuse ledger in
  [`docs/rust-port.md`](docs/rust-port.md) for an existing helper or component (the `LogHandle`,
  `processor::rules::is_mount_source`, the injected `Clock`, the backoff constants, the `Recorder`
  and its no-op form, `go_compat`, `test_support`) before adding one.
- **Fail closed.** An empty, unknown or malformed mode, config value or label denies or skips — it
  never falls back to wider device access.
- **Deletion smell.** Removing a user-visible surface (flag, key, label, metric, documented
  behavior) and flipping its test to assert absence needs a line in `README.md` or `docs/**` that
  retires it; a commit message is not enough.
- **Suppressions show their work.** A new `#[expect(...)]`, `# shellcheck disable=` or
  `# droast ignore=` explains why the fix does not apply here, not which rule fired; `#[expect]`
  carries it in `reason = "..."` and droast in `reason="..."`.

## Suppressions are ratcheted

Every in-file suppression is counted per rule against `.suppressions-baseline` by the
`check-suppressions` pre-commit hook (`scripts/check-suppressions.sh`): each lint of a Rust
`#[expect(...)]` or `#[allow(...)]` (not one wrapped in `cfg_attr`; `#[allow]` is denied by the
workspace lints anyway), `# shellcheck disable=`, `# droast ignore=` and `# droast global ignore=`.

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
| `**/*.rs`, `clippy.toml`, `rustfmt.toml` | `make verify`, then `make check-stage` |
| `src/{daemon,processor,cgroup,launcher}/**`, `testenv/**` | also `make test-integration` (Docker); `SDA_IT_ENFORCE=1` only on a throwaway host |
| `tests/testdata/**` (the Go golden captures and fixtures) | `make verify`; they are frozen 1.x captures (docs/rust-port.md, Go golden captures): never regenerate one by hand |
| a `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, `deny.toml` | `make verify`, `make rust-deny` (network) |
| `deployments/docker/Dockerfile`, `droast.toml`, `.dockerignore` | `cargo test --locked -p meta --test dockerfiles --test toolchain` (droast runs in `make check`), `make docker-build` |
| `.mk/rust-release.mk`, `.github/actions/setup-zigbuild/**` | `make rust-release-binaries` (pinned zig on PATH), `cargo test --locked -p meta --test toolchain` |
| `scripts/check-suppressions.sh`, `.suppressions-baseline` | `cargo test --locked -p meta --test suppressions` |
| `scripts/check-license-headers.sh` | `cargo test --locked -p meta --test license_headers` |
| `.github/scripts/**` | `bash .github/scripts/binary-attestation-status.test.sh`, shellcheck (in `make check`) |
| `meta/**` | `make verify` |
| `.github/workflows/**`, `.github/actions/**` | `pre-commit run actionlint --all-files`, `cargo test --locked -p meta` (Rust release, zig, rust-std, Trivy and ShellCheck pins, platforms, vacuous flags, required gates and skipped hooks) |
| `.pre-commit-config.yaml`, `Makefile`, `.mk/**` | `make check`, `cargo test --locked -p meta`, `make help` |
| `scripts/hooks/rustfmt-on-edit.sh` | shellcheck (in `make check`); no test package: run it once by editing a `.rs` file in Claude Code |
| `README.md`, `docs/**/*.md`, other Markdown | `cargo test --locked -p meta --test docs` (the `docs-links` hook); prose wraps at 100 columns (markdownlint in `make check`) |
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
