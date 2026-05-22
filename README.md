# device-mapping-manager

A small Linux daemon that listens to Docker container-start events and injects
cgroup v2 BPF device-allow rules for any container that bind-mounts a `/dev/...`
path. It enables GPU, USB, and other device passthrough for Docker Swarm
services, which reject `devices:` and `device_cgroup_rules:` in their compose
schema.

This is a fork of the upstream
[`allfro/device-mapping-manager`](https://github.com/allfro/device-mapping-manager).

## 💡 What problem this solves

Docker Swarm services do not honor the `devices:` or `device_cgroup_rules:`
compose keys. The kernel still enforces cgroup device controls on every
container, so a Swarm task that bind-mounts a `/dev/...` path can see the
device file but cannot read or write to it (EACCES on open). The only
supported escape hatch is to write directly to the cgroup's BPF device filter.

`device-mapping-manager` watches the Docker daemon for container starts and,
for each container that bind-mounts something under `/dev`, attaches an extra
BPF program to the container's cgroup that allows read/write/mknod on the
mounted device's major/minor pair. The container then has the access it
expected from the bind mount.

## 📦 What this daemon does

- Subscribes to Docker's `start` and `unpause` event stream (with
  reconnect-with-backoff).
- At startup, enumerates already-running containers so a daemon restart does
  not leave them with missing device rules.
- For each new container, inspects its mounts and applies a BPF
  `BPF_CGROUP_DEVICE` program for every `/dev/...` bind mount.
- Walks directory mounts (e.g. `/dev/bus/usb`) and applies a rule per device
  file.
- Supports both cgroup v1 (`devices.allow` write) and cgroup v2 (BPF program
  attach). Most modern hosts use v2.
- Optionally watches systemd's DBus `Reloading` signal and re-applies device
  rules after `systemctl daemon-reload` (which clears cgroup BPF programs).
  Enabled when the host's DBus socket is bind-mounted into the daemon;
  silently skipped otherwise.

The cgroup/BPF code is the original NVIDIA implementation from
[NVIDIA's container toolkit](https://github.com/NVIDIA/k8s-device-plugin)
(Apache 2.0), preserved in `internal/cgroup/`.

## 🚀 Quick start

The daemon must run on every Swarm node that hosts services needing device
access. It requires `privileged: true`, `cgroupns: host`, `pid: host`,
`userns: host`, and bind mounts of the host `/sys` and `/dev`.

Swarm's compose schema rejects `privileged`, `cgroup`, `pid`, and
`userns_mode` on service definitions. The workaround is a wrapper service
that runs a `docker` CLI image and shells out to `docker run` against the
host's Docker socket — that `docker run` invocation accepts every flag
Swarm forbids.

### Docker Compose (Swarm)

```yaml
services:
  device-mapping-manager:
    image: docker:29
    entrypoint: docker
    # Run the actual daemon outside Swarm's purview via the host Docker socket.
    # Swarm rejects privileged/cgroup/pid/userns on services, so this wrapper
    # launches the privileged container with `docker run` instead.
    command:
      - run
      - -i
      - --rm
      - --privileged
      - --cgroupns=host
      - --pid=host
      - --userns=host
      - -v
      - /sys:/host/sys
      - -v
      - /var/run/docker.sock:/var/run/docker.sock
      - -v
      - /dev:/dev
      # Optional: enables re-apply of device rules after systemctl daemon-reload.
      # - -v
      # - /run/dbus/system_bus_socket:/run/dbus/system_bus_socket
      - ghcr.io/leinardi/device-mapping-manager:latest
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock
    deploy:
      mode: global
      restart_policy:
        condition: any

  # Example consumer service: a Swarm task that bind-mounts a GPU device.
  cuda-worker:
    image: nvidia/cuda:12.4.0-base-ubuntu22.04
    command: ["nvidia-smi"]
    volumes:
      - /dev/nvidia0:/dev/nvidia0
      - /dev/nvidiactl:/dev/nvidiactl
      - /dev/nvidia-uvm:/dev/nvidia-uvm
    deploy:
      mode: replicated
      replicas: 1
```

A copy of the daemon's compose file is in
[`deployments/docker/docker-compose.yaml`](deployments/docker/docker-compose.yaml).

## ⚙️ Configuration

The binary is configured via CLI flags:

| Flag             | Default                | Description                                                                     |
|------------------|------------------------|---------------------------------------------------------------------------------|
| `-log-level`     | `info`                 | `debug`, `info`, `warn`, `error`                                                |
| `-log-format`    | `text`                 | `text`, `json`, `plain`                                                         |
| `-log-time`      | `false`                | Include timestamps in log lines                                                 |
| `-docker-socket` | `/var/run/docker.sock` | Path to the Docker daemon's UNIX socket                                         |
| `-dry-run`       | `false`                | Log device rules that would be applied without writing to the cgroup            |
| `-require-label` | `""`                   | Only process containers with this label (`key=value`). Empty = all containers.  |
| `-device-allow`  | `""`                   | Glob for `/dev/...` paths to allow (repeatable). Empty = allow all.             |
| `-device-deny`   | `""`                   | Glob for `/dev/...` paths to deny (repeatable). Deny takes priority over allow. |
| `-metrics-addr`  | `""`                   | `host:port` for Prometheus `/metrics`, `/healthz`, `/readyz`. Empty = disabled. |
| `-debug-addr`    | `""`                   | `host:port` for pprof `/debug/pprof/*`. Empty = disabled.                       |
| `-help`          |                        | Print this flag list and exit                                                   |

## 🛠️ Development

### Prerequisites

- Linux host (the daemon is Linux-only; see `//go:build linux` constraints)
- Go 1.26+
- Docker
- `pre-commit`, `golangci-lint`, `hadolint`, `markdownlint-cli2`, `yamllint`,
  `shellcheck`, `actionlint`, `checkmake` (installed by the pre-commit hooks)

### Common tasks

```bash
make help              # list all available targets
make check             # run pre-commit + golangci-lint
make go-build          # build the binary into ./dist/device-mapping-manager
make go-test           # run unit tests
make docker-build      # build the container image
```

The Makefile pulls its shared logic from
[`leinardi/make-common`](https://github.com/leinardi/make-common) at the
version pinned in `.mk/.mk-common-version`. On first `make` run, the bootstrap
script downloads the snippets into `.mk/`.

### Multi-arch image build

```bash
docker buildx build \
  --platform linux/amd64,linux/arm64 \
  -f deployments/docker/Dockerfile \
  --build-arg VERSION=dev \
  --build-arg COMMIT=$(git rev-parse --short HEAD) \
  --build-arg DATE=$(date -u +%Y-%m-%dT%H:%M:%SZ) \
  -t ghcr.io/leinardi/device-mapping-manager:dev .
```

## 🧪 Verifying device passthrough

On a host running the daemon, start a container that bind-mounts a device:

```bash
docker run --rm -v /dev/null:/dev/null alpine sh -c 'echo hello > /dev/null'
```

Without the daemon, this works because `/dev/null` is universally allowed.
For a real GPU/USB device, the same pattern will fail unless the daemon is
running. Watch the daemon's logs:

```text
level=DEBUG device mount detected id=abc123 source=/dev/nvidia0 ...
level=DEBUG adding device rule pid=1234 type=c major=195 minor=0
```

## 📜 License

[Apache License 2.0](LICENSE). The code in `internal/cgroup/` originates from
[NVIDIA's container toolkit](https://github.com/NVIDIA/k8s-device-plugin)
under the same license.
