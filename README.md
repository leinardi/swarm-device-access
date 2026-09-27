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

It requires Docker Engine 19.03 or newer (API 1.40): the Docker client it uses
refuses older engines.

Swarm does not allow those runtime options directly on a service. The common
workaround is to deploy a small wrapper service that runs the Docker CLI and
uses the host Docker socket to launch the real privileged daemon container.

### Host Requirements

On cgroup v2 hosts the daemon adds its grants to the device filter the
container runtime already attached, replacing that program in place rather
than removing it first, so the container is never left unfiltered. That needs:

- The runtime's device filter attached with `BPF_F_ALLOW_MULTI`, as runc 1.0+,
  crun and systemd do. A filter attached without it (exclusive or
  `BPF_F_ALLOW_OVERRIDE`) cannot be replaced without a window in which the
  container has no filter at all, so the container is skipped: the daemon logs
  an error naming the attach mode and counts it under
  `sda_containers_skipped_total{reason="unsupported_attach_mode"}`.
- Every device filter attached to the container's cgroup readable by the
  daemon. If any of them is hidden from it (for example an SELinux policy that
  labels systemd's device filter so the daemon cannot open it), nothing is
  changed and the container is retried: an unreadable program cannot be
  checked or safely replaced. This is stricter than runc, which ignores such
  programs, and such policies are unsupported.
- A device filter attached at all, or one this daemon has seen before.
  `systemctl daemon-reload` can detach every device filter of a container; the
  daemon remembers (in memory) the runtime filters it has seen per cgroup and
  reattaches them, with its grants if the container has any. This puts the
  runtime's own restrictions back too, which the reload would otherwise have
  lifted. A daemon that did not see the container before the reload (for
  example one started after it) cannot rebuild the filter: an unprivileged
  container with no filter is retried with reason `filter_missing` and a
  warning to restart the container, and is never reported as done. Privileged
  containers have no device filter and are left alone.
- A device filter made only of the instructions device filters normally use
  (context loads, simple ALU, jumps inside the program, exit; no helper calls,
  maps or 64-bit immediates) on Linux 4.16 or newer. The daemon rebuilds the
  runtime's filter from the kernel's dump of it, and only this subset is known
  to load back unchanged; the filters runc, crun and systemd attach are all
  inside it. A filter outside it is left untouched and the container is
  retried with reason `program_not_wrappable`.

The daemon marks every program it attaches with a header naming it as its own
(`bpftool` shows it as `sda_devfilter`) and, on the next change, strips that
wrapper back to the runtime's original before wrapping it again. Grants
therefore replace each other instead of piling up, and survive a daemon restart
without any saved state. A program that starts like such a header but does not
check out completely is never modified: the daemon logs an error with reason
`owned_block_conflict` and the container has to be restarted.

Every change attaches the new program before detaching the one it replaces, so
an interrupted change leaves the container narrower, never wider, and the next
pass finishes it. The daemon never tracks kernel program IDs. When it cannot
tell a runtime program from the leftover of a change that failed halfway, it
wraps both, which leaves one redundant, identical wrapper: the same grants,
never wider, and stable from then on.

> **Upgrading from versions without these markers:** programs attached by
> earlier versions look exactly like the runtime's own filter, so the grants
> they carry are kept (and wrapped again) until the container is restarted.
> Restart containers that were processed by an earlier version to drop grants
> the new version cannot see.

Atomic replacement (`BPF_F_REPLACE`) is used when the kernel supports it
(Linux 5.5+, detected on first use); otherwise each new program is attached
before the original is detached, which is narrower, never wider, if it is
interrupted.

On cgroup v1 hosts the daemon writes `devices.allow` and, to take a grant back,
`devices.deny`. It remembers in memory which exceptions it added itself, so it
never revokes one the runtime made. A restarted daemon has lost that memory:
exceptions granted by the previous instance are then indistinguishable from the
runtime's and are not revoked when policy narrows, until the container is
restarted. cgroup v2 hosts do not have this gap.

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
    # Use top-level labels: so they reach the task container, where the daemon
    # reads them. Labels under deploy.labels: are ignored.
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

| Flag | Default | Description |
| --- | --- | --- |
| `-log-level` | `info` | `debug`, `info`, `warn`, `error` |
| `-log-format` | `text` | `text`, `json`, `plain` |
| `-log-time` | `false` | Include timestamps in log lines |
| `-docker-socket` | `/var/run/docker.sock` | Path to the Docker daemon's UNIX socket |
| `-dry-run` | `false` | Log device rules that would be applied without writing to the cgroup |
| `-policy-mode` | `opt-in` | `opt-in`: only `enable=true` containers. `all`: unless `enable=false`. |
| `-device-allow` | `""` | Glob for `/dev/...` paths to allow, repeatable. Empty means allow all. |
| `-device-deny` | `""` | Glob for `/dev/...` paths to deny, repeatable. Deny takes priority over allow. |
| `-metrics-addr` | `""` | `host:port` for Prometheus `/metrics`, `/healthz`, `/readyz`. Empty disables it. |
| `-debug-addr` | `""` | `host:port` for pprof `/debug/pprof/*`. Empty disables it. |
| `-config` | `""` | Path to a YAML config file. CLI flags override file values. Reload with SIGHUP. |
| `-help` |  | Print this flag list and exit |

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

| Label | Values | Description |
| --- | --- | --- |
| `swarm-device-access.enable` | `true` / `false` | Opt in (`true`) or explicitly opt out (`false`) of processing. |
| `swarm-device-access.device-allow` | Comma-separated globs | Allow only matching `/dev/...` paths. Empty means inherit. |
| `swarm-device-access.device-deny` | Comma-separated globs | Deny matching `/dev/...` paths. Deny overrides allow. |

Declare these labels under top-level `labels:` in your service definition.
Docker copies top-level labels into each task container, so the daemon can read
them on every node — including worker nodes.

Policy comes from the daemon's own configuration and the container's labels
only, so every node makes the same decision for the same container, before and
after a daemon restart. Labels under `deploy.labels:` (Swarm service metadata)
are **ignored** on every node.

> **Migrating from earlier versions:** older releases also read
> `swarm-device-access.*` labels from `deploy.labels:` on manager nodes and
> warned about them there. That support is removed: service-level labels are
> never read, and the daemon no longer queries the node's Swarm role or
> inspects the parent service. A service whose only opt-in (or narrowing
> `device-allow`/`device-deny`) sits under `deploy.labels:` is now processed as
> if those labels were absent, which in the default `opt-in` mode means it is
> skipped. Move the labels to the service's top-level `labels:` so they reach
> the container.

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
