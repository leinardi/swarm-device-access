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
The cgroup/BPF implementation in `internal/cgroup/` comes from
[NVIDIA's container toolkit](https://github.com/NVIDIA/k8s-device-plugin)
and remains Apache 2.0 licensed.

## 💡 What It Does

At runtime the daemon:

- Processes already-running containers at startup, so daemon restarts do not
  leave existing tasks without device access.
- Subscribes to Docker `start` and `unpause` events, with reconnect and backoff
  if the Docker event stream drops.
- Inspects each eligible container for bind mounts whose source is under
  `/dev`.
- Walks directory mounts such as `/dev/bus/usb` and applies one rule per device
  file.
- Applies cgroup v1 `devices.allow` rules or cgroup v2 BPF device rules,
  depending on the host.
- Optionally watches systemd over DBus and reapplies rules after
  `systemctl daemon-reload`, which clears cgroup BPF programs.

## 🚀 Quick Start

Run one daemon instance on every Swarm node that may run services needing host
device access.

The daemon needs host-level privileges: `privileged`, host cgroup namespace,
host PID namespace, host user namespace, the host Docker socket, `/sys` mounted
at `/host/sys`, and `/dev` mounted into the daemon container.

Swarm does not allow those runtime options directly on a service. The common
workaround is to deploy a small wrapper service that runs the Docker CLI and
uses the host Docker socket to launch the real privileged daemon container.

### Docker Compose for Swarm

```yaml
services:
  swarm-device-access:
    image: docker:29
    entrypoint: docker
    # Swarm rejects privileged/cgroup/pid/userns on services. This wrapper
    # launches the actual daemon with `docker run`, where those flags are valid.
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
      # Optional: reapply device rules after systemctl daemon-reload.
      # NOTE: dhi.io/static has no /var/run→/run symlink; use /var/run/dbus/... inside the container.
      # - -v
      # - /run/dbus/system_bus_socket:/var/run/dbus/system_bus_socket
      - ghcr.io/leinardi/swarm-device-access:latest
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
    # Use top-level labels: so the daemon can read them on both manager and
    # worker nodes. deploy.labels: are service-level metadata only visible to
    # the Docker API on manager nodes.
    labels:
      swarm-device-access.enable: "true"
      swarm-device-access.device-allow: "/dev/nvidia*"
    deploy:
      mode: replicated
      replicas: 1
```

The daemon compose file is also available at
[`deployments/docker/docker-compose.yaml`](deployments/docker/docker-compose.yaml).

## ⚙️ Configuration

You can configure the daemon with CLI flags, a YAML config file, or both. When a
value is set in both places, the CLI flag wins.

| Flag             | Default                | Description                                                                      |
|------------------|------------------------|----------------------------------------------------------------------------------|
| `-log-level`     | `info`                 | `debug`, `info`, `warn`, `error`                                                 |
| `-log-format`    | `text`                 | `text`, `json`, `plain`                                                          |
| `-log-time`      | `false`                | Include timestamps in log lines                                                  |
| `-docker-socket` | `/var/run/docker.sock` | Path to the Docker daemon's UNIX socket                                          |
| `-dry-run`       | `false`                | Log device rules that would be applied without writing to the cgroup             |
| `-policy-mode`   | `opt-in`               | `opt-in`: only `enable=true` containers. `all`: unless `enable=false`.           |
| `-device-allow`  | `""`                   | Glob for `/dev/...` paths to allow, repeatable. Empty means allow all.           |
| `-device-deny`   | `""`                   | Glob for `/dev/...` paths to deny, repeatable. Deny takes priority over allow.   |
| `-metrics-addr`  | `""`                   | `host:port` for Prometheus `/metrics`, `/healthz`, `/readyz`. Empty disables it. |
| `-debug-addr`    | `""`                   | `host:port` for pprof `/debug/pprof/*`. Empty disables it.                       |
| `-config`        | `""`                   | Path to a YAML config file. CLI flags override file values. Reload with SIGHUP.  |
| `-help`          |                        | Print this flag list and exit                                                    |

### Config File

All CLI flags can be represented in YAML and loaded with `-config`. See
[`deployments/docker/config.yaml`](deployments/docker/config.yaml) for an
example.

Send `SIGHUP` to reload the config file without restarting the daemon:

```bash
kill -HUP $(docker inspect --format '{{.State.Pid}}' swarm-device-access)
```

Some settings are only read at startup and still require a restart:
`docker-socket`, `metrics-addr`, and `debug-addr`.

### Container Labels

Consumer services opt in and narrow their allowed device set with labels:

| Label                              | Values                | Description                                                    |
|------------------------------------|-----------------------|----------------------------------------------------------------|
| `swarm-device-access.enable`       | `true` / `false`      | Opt in (`true`) or explicitly opt out (`false`) of processing. |
| `swarm-device-access.device-allow` | Comma-separated globs | Allow only matching `/dev/...` paths. Empty means inherit.     |
| `swarm-device-access.device-deny`  | Comma-separated globs | Deny matching `/dev/...` paths. Deny overrides allow.          |

Declare these labels under top-level `labels:` in your service definition.
Docker copies top-level labels into each task container, so the daemon can read
them on every node — including worker nodes.

> **Note:** Do not use `deploy.labels:` for these labels. `deploy.labels:` are
> Swarm service metadata that only the Docker Swarm API (manager nodes) can
> read. The daemon cannot see them on worker nodes and will warn you if it
> detects them on a manager.

### Worker and Manager Nodes

The daemon auto-detects its role at startup by querying the local Docker API.
On **worker nodes** it skips the Swarm service inspect entirely (no spurious
warnings). On **manager nodes** it additionally reads service-level metadata
for backward compatibility — but emits a warning if it finds
`swarm-device-access.*` labels under `deploy.labels:`, advising you to move
them to top-level `labels:`.

If the node role changes (promote/demote), restart the daemon so it re-detects
its role.

If you bind-mount a directory (for example `source: /dev/dri`), the
`device-allow` glob is evaluated **per child node** inside that directory —
write the glob against the children, e.g. `/dev/dri/*` or `/dev/dri/renderD128`.

Global `-device-allow` and `-device-deny` define the broadest access the daemon
may grant. Per-container labels can only narrow that access. Deny rules always
win.

## 🔒 Security

This daemon is powerful by design. It runs privileged, mounts the host `/dev`
tree, and can talk to the Docker socket. Treat it as host-root equivalent: if it
is compromised or misconfigured, every container on that node may be affected.

The default policy mode is `opt-in` for that reason. Only containers with
`swarm-device-access.enable: "true"` are processed, so unrelated containers that
happen to bind-mount something under `/dev` are not silently granted access.

If every workload on the node is trusted, `-policy-mode=all` can be used to
process all containers unless they explicitly set
`swarm-device-access.enable: "false"`.

See [SECURITY.md](SECURITY.md) for the threat model and disclosure policy.

## 🧪 Verifying Device Passthrough

On most x86 Linux hosts, `/dev/ttyS0` (a serial port) is present but **not** in
Docker's default cgroup allowlist, making it a reliable test target.

First, confirm access is denied without the label (opt-in mode, the default):

```bash
docker run --rm -v /dev/ttyS0:/dev/ttyS0 alpine \
  sh -c 'echo x >/dev/ttyS0' 2>&1
# Expected: sh: write error: Operation not permitted
```

Now run with the opt-in label. The daemon detects the start event and applies the
cgroup device rule asynchronously. The `sleep 1` gives it time to do so before
the write is attempted — in production, long-lived services start up slowly
enough that this race does not arise:

```bash
docker run --rm \
  --label swarm-device-access.enable=true \
  --label 'swarm-device-access.device-allow=/dev/ttyS0' \
  -v /dev/ttyS0:/dev/ttyS0 \
  alpine sh -c 'sleep 1 && echo x >/dev/ttyS0 && echo "access granted"'
# Expected: access granted
```

With debug logs enabled, successful processing looks like this:

```text
level=DEBUG device mount detected id=abc123 source=/dev/ttyS0 ...
level=DEBUG adding device rule pid=1234 type=c major=4 minor=64
```

## 🛠️ Development

### Prerequisites

- Linux host. The daemon is Linux-only and uses `//go:build linux` constraints.
- Go 1.26+
- Docker
- `pre-commit`, `golangci-lint`, `hadolint`, `markdownlint-cli2`, `yamllint`,
  `shellcheck`, `actionlint`, and `checkmake`. The pre-commit hooks install
  these tools when needed.

### Common Tasks

```bash
make help              # list all available targets
make check             # run pre-commit + golangci-lint
make go-build          # build the binary into ./dist/swarm-device-access
make go-test           # run unit tests
make docker-build      # build the container image
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

[Apache License 2.0](LICENSE). The code in `internal/cgroup/` originates from
[NVIDIA's container toolkit](https://github.com/NVIDIA/k8s-device-plugin) under
the same license.
