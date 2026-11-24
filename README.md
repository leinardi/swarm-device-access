# swarm-device-access

`swarm-device-access` is a small Linux daemon for Docker Swarm nodes. It gives
Swarm services the device access they usually expect from bind-mounting
`/dev/...` paths, such as GPUs, USB buses, serial devices, or other host
devices.

Docker Swarm rejects `devices:` and `device_cgroup_rules:` in service compose
files. A task can still bind-mount `/dev/nvidia0` or `/dev/bus/usb`, but the
kernel's cgroup device policy may block the container from opening the device.
The result is confusing: the file is visible inside the container, but access
fails with `EACCES`.

This daemon watches Docker for container starts and unpauses. When a matching
container has a bind mount under `/dev`, it adds the needed cgroup device rule
for that device's major/minor pair. On cgroup v2 hosts that means attaching a
`BPF_CGROUP_DEVICE` program. On cgroup v1 hosts it writes to `devices.allow`.

This project is a fork of
[`allfro/device-mapping-manager`](https://github.com/allfro/device-mapping-manager).
The cgroup/BPF code in `src/cgroup/` (`api.rs`, `ebpf.rs`, `v1.rs` and `v2.rs`) is
ported from [NVIDIA's libnvidia-container](https://github.com/NVIDIA/libnvidia-container)
and remains Apache 2.0 licensed.

Version 2.0 is a Rust rewrite of the 1.x Go daemon, with the same behaviour
except for the differences listed in
[Migrating from 1.x](docs/migration.md), the first of them that every flag
is spelled with two dashes.

## 💡 What It Does

At runtime the daemon:

- Processes already-running containers at startup, so daemon restarts do not
  leave existing tasks without device access.
- Subscribes to Docker `start` and `unpause` events (to grant) and `die` and
  `destroy` events (to clean up after a run), with reconnect and backoff if the
  Docker event stream drops.
- Inspects each eligible container for bind mounts whose source is under
  `/dev`.
- Walks directory mounts such as `/dev/bus/usb` and applies one rule per device
  file.
- Applies cgroup v1 `devices.allow` rules or cgroup v2 BPF device rules,
  depending on the host.
- Optionally watches systemd over DBus and reapplies rules after
  `systemctl daemon-reload`, which clears cgroup BPF programs.

## 📚 Documentation

- [Getting started](docs/getting-started.md): the Swarm service, a consumer example and a
  passthrough check.
- [Deployment](docs/deployment.md): the launcher, its flags, updates and rollbacks.
- [Host requirements](docs/host-requirements.md): kernel, cgroup v2 and v1, and how grants are
  kept and revoked.
- [Configuration](docs/configuration.md): flags, the config file and reloads.
- [Container labels](docs/labels.md): opt-in, allow and deny globs, device identity.
- [Security model](docs/security.md), [Tips and limitations](docs/tips.md),
  [Alternatives](docs/alternatives.md) and [Migrating from 1.x](docs/migration.md).
- [Verifying releases](docs/verifying-releases.md), [Architecture](docs/architecture.md) and
  [Testing](docs/testing.md).

## 🛠️ Development

### Prerequisites

- Linux host. The daemon is Linux-only.
- Rust, through `rustup`: `rust-toolchain.toml` pins the exact release and its components.
- Docker, logged in to `dhi.io` (`docker login dhi.io`): the image builds from Docker Hardened
  Images.
- `pre-commit`, `cargo-deny`, [droast](https://github.com/immanuwell/dockerfile-roast),
  `markdownlint-cli2`, `yamllint`, `shellcheck`, `actionlint`, and `checkmake`. The pre-commit
  hooks install most of these tools when needed; droast is built from source, which needs `cargo`.
- A local `shellcheck` at the release the `shellcheck-py` hook in `.pre-commit-config.yaml` wraps:
  droast's required ShellCheck bridge runs the one on PATH.
- For the static release binaries only (`make rust-release-binaries`): the zig and cargo-zigbuild
  releases the Makefile pins, on PATH. QEMU (binfmt) to run the arm64 image tests on
  another architecture.

### Common Tasks

```bash
make help              # list all available targets
make check             # run every pre-commit hook on every file
make verify            # build, format check, clippy and every test, meta-tests included
make rust-build        # build the workspace (target/debug/swarm-device-access)
make docker-build      # build the container image's default target, which compiles the binary
make rust-release-dist # the static release binaries, then checksums.txt, into dist/
make image-build-test  # the prebuilt image from those binaries, then its tests
make test-integration  # the integration suite against the image (Docker, dhi.io login)
```

See [`docs/testing.md`](docs/testing.md) for the CI-safe integration suite and
the opt-in native privileged checklist.

The Makefile pulls shared logic from
[`leinardi/make-common`](https://github.com/leinardi/make-common), pinned by
`.mk/.mk-common-version`. On the first `make` run, the bootstrap script downloads
the snippets into `.mk/`.

### Multi-Arch Image Build

```bash
docker buildx build \
  --platform linux/amd64,linux/arm64 \
  -f deployments/docker/Dockerfile \
  --build-arg VERSION=dev \
  --build-arg COMMIT=$(git rev-parse --short HEAD) \
  --build-arg DATE=$(date -u +%Y-%m-%dT%H:%M:%SZ) \
  -t ghcr.io/leinardi/swarm-device-access:dev .
```

## 📜 License

[Apache License 2.0](LICENSE). `src/cgroup/{api,ebpf,v1,v2}.rs` are ported from
[NVIDIA's libnvidia-container](https://github.com/NVIDIA/libnvidia-container) under
the same license.
