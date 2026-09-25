# Architecture

## What it does

`swarm-device-access` is a privileged Linux daemon that fills a gap in Docker Swarm: Swarm services silently ignore `devices:` and
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
|  cmd/swarm-device-access/main.go                  |
|                                                   |
|  run()                                            |
|   ├─ Events(Since=now)            ← subscribe     |
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

The Docker event stream is opened **before** the enumeration, with `Since` set to the time captured just before subscribing. A container that starts
while the list is being processed is therefore still delivered as a `start` event instead of falling into the gap between the list and the
subscription (the typical node-boot case). The client delivers events on an unbuffered channel, so events that arrive during the enumeration wait
until the event loop starts consuming them; nothing is dropped.

A `processed map[string]time.Time` records, for every container processed successfully at startup, the time captured just before it was inspected.
An event for a container in the map is skipped only if its timestamp is not newer than that time: Docker emits `start` after the container is
running, so an older event was already visible to the startup inspect. A newer event (for example `docker restart` or `unpause` shortly after the
daemon started) belongs to a new run with a new cgroup and is applied. The entry is removed on the first event for that ID either way, and the whole
map is cleared after 60s to bound memory. Event time and daemon time come from the same host clock.

Startup and event-driven processing share the same code path (`processOne`), so both record the same metrics and log a `Warn` on failure
(`could not process running container` at startup, `could not process container` for events).

### Event loop reconnect

`listenEvents` wraps `consumeEvents` in a reconnect loop with exponential backoff (`1s`→`30s`). If the Docker event stream drops (daemon restart,
socket error, channel close), the loop reconnects rather than exiting — replacing the upstream `log.Fatal(err)` pattern.

On re-subscription `Since` is set to one nanosecond after the last received event (or the original startup time if no event arrived yet), so events
emitted during the disconnect are delivered while the last event is not replayed. Replaying it would re-apply rules, and on cgroup v2 every apply
prepends the rules again, growing the attached programs.

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

## Device mount collection

For every bind mount whose source is `/dev` or lives under `/dev/`, the processor collects one device rule per device node:

- **Single-file mount** (e.g. `/dev/nvidia0`): policy is checked against the (resolved) path. A missing, unresolvable or non-device file is an error,
  because it was mounted explicitly.
- **Directory mount** (e.g. `/dev`, `/dev/dri`): the tree is walked (depth-capped). Allow/deny globs match the **resolved target** of each entry,
  never the symlink name, so a link such as `/dev/disk/by-id/...` cannot bypass a deny on `/dev/sda`.

Symlinks are resolved in the daemon's own view. Because the daemon bind-mounts the host `/dev`, links that stay inside `/dev` (`disk/by-id/*`,
`dri/by-path/*`, `char/*`) resolve correctly. Links that leave `/dev` (`/dev/log -> /run/systemd/journal/dev-log`, `/dev/initctl -> /run/initctl`,
`/dev/stdin -> /proc/self/fd/0`) often dangle inside the daemon container; they never point to device nodes. Such unresolvable symlinks are
**skipped, not errors**:

- `Warn` `device symlink matches allow policy but cannot be resolved` — only when an explicit allow glob (global or label) matches the link path, since
  that likely means an intended device is missing.
- `Debug` `unresolvable symlink skipped` — every other case, including all mounts without allow globs.

Entries that pass policy but are not character or block devices (regular files, sockets, pipes) are skipped at `Debug` (`non-device entry skipped`).
Other failures on entries that pass policy (e.g. a `stat` error on a device node), and errors reading the tree itself during the walk (e.g. an
unreadable subdirectory, reported before any policy check), are per-device errors: each is logged once as
`Warn` `device rule failed` and counted in `sda_rule_failures_total`, while the remaining rules are still applied. Only container-level failures
(inspect, cgroup detection/path resolution, `AddDeviceRules`) make the container fail.

Each container with at least one `/dev` mount produces one summary line with counts only:

```
level=INFO msg="container processed" id=abc pid=1234 devices_granted=12 skipped=87 errors=0 dry_run=false
```

`devices_granted` is the number of deduplicated rules, `skipped` counts entries excluded by policy, unresolvable symlinks, symlinks to directories
and non-device entries, and `errors` counts per-device failures.

## Package layout

| Package | Responsibility |
| --- | --- |
| `cmd/swarm-device-access` | Daemon entrypoint, event loop, Docker client, apply pipeline |
| `internal/cgroup` | Device-rule application: cgroup v1 (`devices.allow` write) and v2 (BPF attach). Cgroup version + path detection via `/proc`. NVIDIA-derived code. |
| `internal/logger` | `slog`-based singleton with `text`, `json`, `plain` handlers |
| `internal/policy` | Cross-platform policy evaluation: mode (opt-in/all), label parsing, glob allow/deny logic |
| `internal/systemd` | DBus watcher for systemd `Reloading` signal |

## Policy model

The daemon uses a two-level policy:

**Global policy** (flags / config file):

- `-policy-mode` (`opt-in` or `all`): controls which containers are processed.
    - `opt-in`: only containers with label `swarm-device-access.enable=true`.
    - `all`: all containers unless `swarm-device-access.enable=false`.
- `-device-allow`: repeatable glob — defines the maximum allowed set of `/dev/...` paths. Empty = all.
- `-device-deny`: repeatable glob — paths always excluded. Takes priority over allow.

**Per-container policy** (Docker labels):

Declare these labels under top-level `labels:` in your Swarm stack file.
Docker copies top-level labels into each task container so the daemon can read
them on both manager and worker nodes. Do **not** use `deploy.labels:` — those
are Swarm service metadata only accessible via the manager API; the daemon
cannot see them on worker nodes and will warn if it finds them on a manager.

| Label | Description |
| --- | --- |
| `swarm-device-access.enable` | `true` to opt in, `false` to explicitly opt out. |
| `swarm-device-access.device-allow` | Comma-separated globs narrowing global allow. Empty = inherit. |
| `swarm-device-access.device-deny` | Comma-separated globs added on top of global deny. |

**Decision rule** for a given container and device path:

```
enabled = (enable != false) AND (mode=all OR enable=true)
allowed = NOT global-denied
       AND NOT container-denied
       AND (global-allow empty OR path matches global-allow)
       AND (container-allow empty OR path matches container-allow)
```

Global is the maximum allowed access; per-container labels can only narrow it. Deny always wins over allow. Invalid label values cause the container to be skipped (fail-closed).

## Runtime contract

The daemon **must** run as root with:

- `--privileged` (or equivalent capabilities: `CAP_BPF`, `CAP_PERFMON`, `CAP_SYS_ADMIN`, `CAP_SYS_RESOURCE`)
- `--cgroupns=host`, `--pid=host`, `--userns=host`
- `/sys` bind-mounted at `/host/sys` inside the container
- `/dev` bind-mounted (so device major/minor can be read via `unix.Stat`)
- `/var/run/docker.sock` bind-mounted

The DBus socket is optional — enables systemd reload handling. Mount as `-v /run/dbus/system_bus_socket:/var/run/dbus/system_bus_socket`; the container-side path must be under `/var/run/` because `dhi.io/static` has no `/var/run → /run` symlink.

## Observability

### Prometheus metrics (`--metrics-addr`)

When `-metrics-addr=:9090` is set, the daemon exposes:

| Endpoint | Description |
| --- | --- |
| `/metrics` | Prometheus text format |
| `/healthz` | Liveness probe — always `200 OK` |
| `/readyz` | `200 OK` when subscribed to Docker events; `503` on startup or reconnect |

Metrics exposed:

| Metric | Type | Labels | Description |
| --- | --- | --- | --- |
| `sda_events_total` | counter | `event` | Docker events received (`start`, `unpause`) |
| `sda_rules_applied_total` | counter | `result` | Containers handled: `ok` = no container-level failure (includes policy, label and no-pid skips and partial per-device failures); `error` = container-level failure |
| `sda_reload_reapplies_total` | counter | — | Re-applies after systemd daemon-reload |
| `sda_docker_reconnects_total` | counter | — | Docker event stream reconnects |
| `sda_apply_duration_seconds` | histogram | — | Wall-clock time per container apply |
| `sda_containers_scanned_total` | counter | — | Containers that passed policy and were processed |
| `sda_containers_skipped_total` | counter | `reason` | Containers skipped (`policy`, `invalid_labels`, `no_pid`) |
| `sda_device_files_discovered_total` | counter | — | Device files found across all processed containers |
| `sda_rule_failures_total` | counter | — | Per-device rule failures (including walk errors); the per-device failure signal |
| `sda_dry_run_skips_total` | counter | — | Rules skipped in dry-run mode |
| `sda_last_event_timestamp_seconds` | gauge | — | Unix timestamp of last container processed successfully (event, startup or reload) |

### pprof debug server (`--debug-addr`)

When `-debug-addr=:6060` is set, the standard Go pprof endpoints are available at `/debug/pprof/*`. Only bind to localhost in production.

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

Ensure the host DBus socket is bind-mounted as `-v /run/dbus/system_bus_socket:/var/run/dbus/system_bus_socket`. Without it, the reload watcher is disabled (logged at `Warn` on startup). The container-side path must be `/var/run/dbus/system_bus_socket` — `dhi.io/static` has no `/var/run → /run` symlink.

### Daemon cannot connect to Docker

Check the socket path with `-docker-socket`. Default: `/var/run/docker.sock`. On some hosts Docker Desktop uses a different path.

The daemon requires Docker Engine 19.03+ (API 1.40). The `github.com/moby/moby/client` SDK refuses older engines with `API version … is not supported by this client: the minimum supported API version is 1.40`.
