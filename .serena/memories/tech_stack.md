# Tech Stack

- **Language**: Rust 1.98.1 (exact release in `rust-toolchain.toml`), edition 2024; Cargo
  workspace `.` (package `swarm-device-access`), `meta`, `testenv`
- **Build**: Make wrapping `cargo`; shared `.mk/` snippets from `leinardi/make-common@v1`;
  static musl release binaries with cargo-zigbuild and pinned zig
- **Key crates**:
  - `tokio`, `tokio-util` — runtime, cancellation, task tracking
  - rust-common (git, pinned by release `tag`): `clock`,
    `docker-client`, `service-kit`, `testenv-core`, `repo-meta`
  - `serde_json` — the create body decoded into bollard's model (bollard comes with docker-client)
  - `rustix` — syscalls (openat2, fstat, pidfd); `libc` only for raw `bpf(2)` in `bpf_sys.rs`
  - `zbus` — systemd D-Bus watcher
  - `clap` — flags; `saphyr-parser` — config file; `serde`/`serde_json` — Engine API types
  - `jiff` — time; `sha2` — wrapper digest
  - `bollard` — integration harness only (`testenv`, feature `integration`)
- **Linters**: clippy (pedantic + restriction lints, `-D warnings`), rustfmt, cargo-deny,
  cargo-machete, droast, ShellCheck; enforced via pre-commit hooks
- **Pre-commit**: `make check` (all files) / `make check-stage` (staged only)
- **Container**: `deployments/docker/Dockerfile` (dhi.io bases by tag; targets `runtime`, the
  default, compiling its own binary, and `prebuilt`, COPYing `dist/swarm-device-access-linux-<arch>`,
  the one CI tests and the release ships); image name = `swarm-device-access`
- **CI/release**: `ci.yaml` (`ci-result` needs `pre-commit-all`, `rust`, `image`,
  `conventional-commits`), `integration.yaml`, `release.yaml` (`guard`, `ci-green`, `audit`,
  `release`; build once, smoke + `trivy-gate`), `scheduled-scan.yaml`; shared steps from
  gh-composite-actions; Trivy never pinned, exceptions in `.trivyignore.yaml`
- **Runtime**: must run with `privileged: true`, `cgroup: host`, `pid: host`, `userns_mode: host`;
  bind mounts: `/var/run/docker.sock`, `/sys → /host/sys`, `/dev`;
  `/run/dbus/system_bus_socket` optional (enables reload handling)
