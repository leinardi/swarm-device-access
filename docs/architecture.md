# Architecture

## What it does

`device-mapping-manager` is a privileged Linux daemon that fills a gap in Docker Swarm: Swarm services silently ignore `devices:` and
`device_cgroup_rules:` in compose specs, yet the kernel still enforces cgroup device controls. A container that bind-mounts `/dev/nvidia0` can _see_
the device file but cannot open it (`EACCES`).

The daemon watches Docker for container-start events and, for each new container that bind-mounts something under `/dev/`, attaches an extra BPF
`BPF_CGROUP_DEVICE` program to the container's cgroup that allows read/write/mknod on the mounted device's major/minor pair.

## High-level flow

```
Docker daemon
    |
    | container start / unpause events (Unix socket)
    v
+---------------------------------------------------+
|  cmd/device-mapping-manager/main.go               |
|                                                   |
|  run()                                            |
|   ├─ processExistingContainers()  ← startup       |
|   ├─ startReloadWatcher()         ← goroutine     |
|   └─ listenEvents()               ← main loop     |
|          │                                        |
|          │  per container-start event             |
|          ▼                                        |
|  processContainer(id, procRootPath, dryRun)        |
|   ├─ ContainerInspect  → PID, Mounts              |
|   ├─ GetDeviceCGroupVersion(procRootPath, pid)     |
|   ├─ New(version) → Interface                     |
|   ├─ GetDeviceCGroupMountPath(procRootPath, pid)   |
|   └─ for each /dev/... mount:                     |
|        applyMount → applyDeviceRules              |
|          └─ api.AddDeviceRules(cgroupPath, rules)  |
+---------------------------------------------------+
    |                          |
    | cgroup v1                | cgroup v2
    v                          v
devices.allow write     BPF_CGROUP_DEVICE attach
(cgroupv1.go)           (cgroupv2.go + ebpf.go)
```

### Startup back-fill

On daemon start, `processExistingContainers` calls `ContainerList` and applies rules to every already-running container. Without this, containers that
started before the daemon would have no device access until their next restart.

A `processed map[string]struct{}` records those container IDs. When the event stream is opened immediately after, any `start` event for a container
already in the map is skipped (deduplication window).

### Event loop reconnect

`listenEvents` wraps `consumeEvents` in a reconnect loop with exponential backoff (`1s`→`30s`). If the Docker event stream drops (daemon restart,
socket error, channel close), the loop reconnects rather than exiting — replacing the upstream `log.Fatal(err)` pattern.

Context cancellation (SIGTERM/SIGINT via `signal.NotifyContext`) exits cleanly at any point.

### systemd daemon-reload handling

`systemctl daemon-reload` clears all cgroup BPF programs. The optional DBus watcher (`internal/systemd/`) subscribes to
`org.freedesktop.systemd1.Manager.Reloading` and triggers a full re-apply when it receives the completion edge (`active=false`). It gracefully
degrades to a warning when the DBus socket is not mounted.

## BPF program structure

For cgroup v2 hosts, device control is implemented via `BPF_CGROUP_DEVICE` programs. The code in `internal/cgroup/ebpf.go` (ported from
runc/opencontainers-cgroups, originally from crun) builds the BPF instruction list using `cilium/ebpf/asm`.

Each device rule produces a block of instructions that match on four fields loaded at program entry:

```
R2 = type   (lower 16 bits of access_type: BPF_DEVCG_DEV_CHAR=2, BPF_DEVCG_DEV_BLOCK=1)
R3 = access (upper 16 bits: READ|WRITE|MKNOD bits)
R4 = major
R5 = minor
```

One rule block:

```
JNE  R2, bpfType  → next-block      ; type mismatch → skip
MOV  R6, R3
AND  R6, bpfAccess
JNE  R6, R3        → next-block      ; access not fully covered → skip
JNE  R4, major     → next-block      ; major mismatch
JNE  R5, minor     → next-block      ; minor mismatch
MOV  R0, 1                           ; allow
RETURN
```

Rules are prepended to any existing program (`BPF_F_ALLOW_MULTI` flag), so the daemon's rules compose with the container runtime's own device filter
rather than replacing it.

## Package layout

| Package                      | Responsibility                                                                                                                                    |
|------------------------------|---------------------------------------------------------------------------------------------------------------------------------------------------|
| `cmd/device-mapping-manager` | Daemon entrypoint, event loop, Docker client, apply pipeline                                                                                      |
| `internal/cgroup`            | Device-rule application: cgroup v1 (`devices.allow` write) and v2 (BPF attach). Cgroup version + path detection via `/proc`. NVIDIA-derived code. |
| `internal/logger`            | `slog`-based singleton with `text`, `json`, `plain` handlers                                                                                      |
| `internal/systemd`           | DBus watcher for systemd `Reloading` signal                                                                                                       |

## Runtime contract

The daemon **must** run as root with:

- `--privileged` (or equivalent capabilities: `CAP_BPF`, `CAP_PERFMON`, `CAP_SYS_ADMIN`, `CAP_SYS_RESOURCE`)
- `--cgroupns=host`, `--pid=host`, `--userns=host`
- `/sys` bind-mounted at `/host/sys` inside the container
- `/dev` bind-mounted (so device major/minor can be read via `unix.Stat`)
- `/var/run/docker.sock` bind-mounted

The DBus socket (`/run/dbus/system_bus_socket`) is optional — enables systemd reload handling.

## Dry-run mode

`-dry-run` logs what rules _would_ be written (at `Info` level) without calling `bpf(2)` or writing to `devices.allow`. Useful for auditing policy and
CI smoke tests.

```
level=INFO msg="dry-run: would add device rule" pid=1234 cgroup=/host/sys/fs/cgroup/docker/abc type=c major=195 minor=0
```

## Troubleshooting

### Container gets `EACCES` opening a device after daemon is running

1. Enable debug logs: `-log-level debug`
2. Confirm the container's mount source starts with `/dev`:

   ```
   level=DEBUG msg="device mount detected" id=abc source=/dev/nvidia0
   ```

3. Confirm a rule was applied:

   ```
   level=DEBUG msg="adding device rule" pid=1234 type=c major=195 minor=0
   ```

4. If step 2 fires but step 3 does not, check `cgroup version detected` — if it returns `-1`, the `/proc` parse failed (check `pid: host` is set).
5. If rules were applied but the container still gets `EACCES`, verify the container is on cgroup v2 and `BPF_F_ALLOW_MULTI` is supported (kernel ≥
   4.15).

### Daemon does not re-apply rules after `systemctl daemon-reload`

Ensure `/run/dbus/system_bus_socket` is bind-mounted into the daemon container. Without it, the reload watcher is disabled (logged at `Warn` on
startup).

### Daemon cannot connect to Docker

Check the socket path with `-docker-socket`. Default: `/var/run/docker.sock`. On some hosts Docker Desktop uses a different path.
