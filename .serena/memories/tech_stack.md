# Tech Stack

- **Language**: Rust 1.98.1 (exact release in `rust-toolchain.toml`), edition 2024; Cargo
  workspace `.` (package `swarm-device-access`), `meta`, `testenv`
- **Build**: Make wrapping `cargo`; shared `.mk/` snippets from `leinardi/make-common@v1`;
  static musl release binaries with cargo-zigbuild and pinned zig
- **Key crates**:
  - `tokio`, `tokio-util` — runtime, cancellation, task tracking
  - rust-common (git, pinned by `rev`): `go-compat`, `go-clock`, `go-slog`, `go-prom`,
    `go-http`, `docker-engine`, `service-kit`, `testenv-core`, `repo-meta`
  - `hyper`, `hyper-util` — the launcher's writes and attach on docker-engine's transport
  - `rustix` — syscalls (openat2, fstat, pidfd); `libc` only for raw `bpf(2)` in `bpf_sys.rs`
  - `zbus` — systemd D-Bus watcher
  - `clap` — flags; `saphyr-parser` — config file; `serde`/`serde_json` — Engine API types
  - `jiff` — time; `sha2` — wrapper digest
  - `bollard` — integration harness only (`testenv`, feature `integration`)
- **Linters**: clippy (pedantic + restriction lints, `-D warnings`), rustfmt, cargo-deny,
  cargo-machete, droast, ShellCheck; enforced via pre-commit hooks
- **Pre-commit**: `make check` (all files) / `make check-stage` (staged only)
- **Container**: `deployments/docker/Dockerfile`; image name = `swarm-device-access`
- **Runtime**: must run with `privileged: true`, `cgroup: host`, `pid: host`, `userns_mode: host`;
  bind mounts: `/var/run/docker.sock`, `/sys → /host/sys`, `/dev`;
  `/run/dbus/system_bus_socket` optional (enables reload handling)
