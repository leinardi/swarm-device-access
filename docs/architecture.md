# Architecture

## What it does

`swarm-device-access` is a privileged Linux daemon that fills a gap in Docker Swarm: Swarm services silently ignore `devices:` and
`device_cgroup_rules:` in compose specs, yet the kernel still enforces cgroup device controls. A container that bind-mounts `/dev/nvidia0` can _see_
the device file but cannot open it (`EPERM`).

The daemon watches Docker for container events and, for every running container that bind-mounts something under `/dev/`, makes the device grants
in the container's cgroup equal exactly what the current policy allows it: on cgroup v2 by wrapping the runtime's `BPF_CGROUP_DEVICE` filter, on
cgroup v1 through `devices.allow` and `devices.deny`.

## High-level flow

```
Docker daemon
    |
    | container start / unpause / die / destroy events (Unix socket)
    v
+---------------------------------------------------------------+
| cmd/swarm-device-access/main.go                                |
|   flags + config file → config.Store; SIGHUP → reloader        |
|   processor.Processor; daemon.Run                              |
+---------------------------------------------------------------+
    |
    v
+---------------------------------------------------------------+
| internal/daemon                                                |
|   Run                                                          |
|    ├─ subscribe (Events, Since=now)       ← before the listing |
|    ├─ coordinator.run                     ← one goroutine      |
|    │    passes: list running containers, reconcile each        |
|    │    (startup, SIGHUP via PublishAndReconcile, systemd      |
|    │    reload), retries with backoff, lifecycle sweep         |
|    ├─ startReloadWatcher (systemd DBus)   ← requests a pass    |
|    └─ listenEvents → consumeEvents        ← reconnect loop     |
|         └─ coordinator.handleEvent                             |
|              start/unpause → reconcileContainer → processOne   |
|              die/destroy   → terminateContainer                |
+---------------------------------------------------------------+
    |
    v
+---------------------------------------------------------------+
| internal/processor: Processor.Reconcile(id)                    |
|   ContainerInspect → labels, State, Mounts                     |
|   computeDesired: policy.Enabled, CollectMountRules per /dev   |
|     mount (openat2 beneath /dev → fstat → sysfs DEVNAME →      |
|     deny/allow on every name), aggregate per device            |
|   applyPinned: pidfd → /proc/<pid>/cgroup → OpenCgroup →       |
|     re-verify → cgroup.Interface.SetDeviceRules(handle, rules) |
+---------------------------------------------------------------+
    |                          |
    | cgroup v1                | cgroup v2
    v                          v
devices.allow / devices.deny   owned wrapper around the runtime's
(v1.go, v1set.go, ledger.go)   BPF_CGROUP_DEVICE filter
                               (v2.go, v2plan.go, v2ops.go, owned.go, ebpf.go)
```

### Reconciliation

`Processor.Reconcile` is the one idempotent path every trigger runs: a start or unpause event, the startup pass, a systemd reload and a config
reload. It computes the complete desired set for the container under the current config and sets exactly that set, including none at all: a
container that policy disables or does not opt in, one with invalid labels, and one whose device set cannot be established all get the empty
set, so an earlier grant is revoked rather than kept. A container that is not running, or that Docker cannot report on, has its grants revoked in
the cgroup its lifecycle was last verified in (`Processor.Terminate` for `die` and `destroy`, a periodic `Processor.Sweep` for records whose
cgroup is gone).

Before changing a cgroup the processor pins the container's process with a pidfd, resolves the cgroup from `/proc/<pid>/cgroup`, opens it as a
directory handle, and re-checks that the process is alive, that Docker still reports it as the container's process with the same start time and,
for a grant, that it is in that cgroup. All mutation goes through the handle, never the path again.

### Coordinator

The coordinator (`internal/daemon/coordinator.go`) is the single owner of reconciliation passes. Startup, `SIGHUP` (through
`Processor.PublishAndReconcile`, which publishes the new config generation) and systemd reloads only request a pass; one goroutine runs them.
A request arriving while a pass runs makes that pass stop after its current container and start again under the newest request. Containers
whose reconcile failed are retried with backoff (`sda_reconcile_pending_containers`); `sda_reload_incomplete` is 1 until a pass for the latest
request has visited every running container, and `config reload complete` is logged when it has.

Events go through the coordinator too (`handleEvent`). Every reconcile or cleanup of a container first reserves it, so work requested for a
container that is already being handled coalesces into one follow-up instead of running twice.

### Startup back-fill

On daemon start, the first pass lists the running containers and reconciles each one. Without this, containers that started before the daemon
would have no device access until their next restart, and grants a previous instance left behind would stay.

The Docker event stream is opened **before** the listing, with `Since` set to the time captured just before subscribing. A container that starts
while the list is being processed is therefore still delivered as a `start` event instead of falling into the gap between the list and the
subscription (the typical node-boot case). The client delivers events on an unbuffered channel, so events that arrive during the listing wait
until the event loop starts consuming them; nothing is dropped. `Run` starts consuming events once the first pass has ended.

The coordinator's `processed map[string]time.Time` records, for every container a pass reconciled successfully, the time captured just before
it was inspected. A `start` or `unpause` event for a container in the map is skipped only if its timestamp is not newer than that time: Docker
emits `start` after the container is running, so an older event was already visible to the inspect. A newer event (for example `docker restart`
shortly after the daemon started) belongs to a new run with a new cgroup and is applied. The entry is removed by the first event for that ID
either way, and entries older than `processedTTL` (2 × the 30 s maximum backoff) are ignored and pruned. Event time and daemon time come from
the same host clock.

Passes, retries and events share the same per-container code path (`processOne`), so all of them record the same metrics and log a `Warn` on
failure (`could not process running container` in a pass, `could not reconcile pending container` on retry, `could not process container` for
events).

### Event loop reconnect

`listenEvents` wraps `consumeEvents` in a reconnect loop with exponential backoff (`1s`→`30s`). If the Docker event stream drops (daemon restart,
socket error, channel close), the loop reconnects rather than exiting.

On re-subscription `Since` is set to one nanosecond after the last received event (or the original startup time if no event arrived yet), so
events emitted during the disconnect are delivered while the last event is not replayed.

Context cancellation (SIGTERM/SIGINT via `signal.NotifyContext`) exits cleanly at any point; `Run` waits for the coordinator, so shutdown does
not cut off a reconcile in the middle of its cgroup mutation.

### systemd daemon-reload handling

`systemctl daemon-reload` can detach cgroup BPF programs. The optional DBus watcher (`internal/systemd/`) subscribes to
`org.freedesktop.systemd1.Manager.Reloading` and requests a pass when it receives the completion edge (`active=false`). It gracefully degrades
to a warning when the DBus socket is not mounted.

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

The rule blocks sit inside an owned wrapper around the runtime's original filter (`internal/cgroup/owned.go`):

```
HEADER, init, rule blocks, TRAILER, original
```

A request no rule block allows falls through to the original program, so the runtime's own restrictions still apply. The header and trailer are
dead stores to `R0` that identify the wrapper (`sda_devfilter` in `bpftool`); on the next change the daemon strips its wrapper back to the
original and wraps it again, so grants replace each other instead of piling up. The new program is attached with `BPF_F_ALLOW_MULTI` (or
`BPF_F_REPLACE` where supported) before the old one is detached. The README's Host Requirements section describes what the runtime filter must
look like for this to work.

## Device mount collection

For every bind mount whose source is `/dev` or lives under `/dev/`, the processor collects one device rule per device node:

- **Single-file mount** (e.g. `/dev/nvidia0`): the source is one candidate. A missing or non-device source is an error, because it was mounted
  explicitly.
- **Directory mount** (e.g. `/dev`, `/dev/dri`): the tree is walked only to enumerate names; each entry is one candidate named after the mount
  source. Enumeration must be complete: a root that cannot be opened, a directory that cannot be read, any walk error, or more than 4096
  entries in one mount (`mount too large; narrow the bind mount`) makes the container's whole desired set empty with a retryable error, since
  an alias that was not seen might deny a device that was. There is no depth cap and no subtree exception.

`/dev` is opened once per pass. Each candidate goes through one evaluation on file descriptors, so an entry swapped between enumeration and use
is judged by what it is when opened, not by its name:

1. The name must be clean (no `..`, `//` or trailing `/`) and under `/dev`; otherwise `Warn` `device path resolves outside /dev`
   (`reason=outside_dev`) and it is skipped.
2. It is opened with `openat2(O_PATH, RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS)` beneath the `/dev` descriptor: symlinks are followed, but no
   step may leave `/dev`. The kernel also refuses an absolute symlink back into `/dev` (`EXDEV`); the link itself is then read and, if its
   target is under `/dev`, followed by hand (at most 40 hops). A link that leaves `/dev` (`/dev/log -> /run/...`, `/dev/stdin ->
   /proc/self/fd/0`) is skipped as `outside_dev`; a dangling name is skipped as `dangling` (`Warn` `device symlink matches allow policy but
   cannot be resolved` when an explicit allow glob names it, `Debug` otherwise).
3. `fstat` on the descriptor gives the type and `major:minor`; a directory or a non-device entry is skipped at `Debug`.
4. The device's kernel name is read from `<sysfs>/dev/{char,block}/<major>:<minor>/uevent`: exactly one `DEVNAME=` line with a clean relative
   value. The canonical name is `/dev/` plus that value. sysfs is `/host/sys` when mounted, else `/sys`.
5. Deny globs are checked on the alias, the resolved node (from `/proc/self/fd`) and the canonical name; allow globs on the canonical and the
   resolved name.

Candidates are aggregated per device across all of the container's mounts: a device is granted once, only when it is authorized and no candidate
that resolves to it is denied. A deny on one name that suppresses an otherwise allowed device is logged at `Info` with `denied_by`.

A candidate whose identity cannot be established (another `openat2` error, `fstat` failure, a missing, unreadable or ambiguous `DEVNAME`) and
that policy would otherwise grant, and errors reading the tree itself during the walk, are per-device errors: each is logged as `Warn` `device
rule failed` and counted in `sda_rule_failures_total`, and the container's whole desired set becomes empty with a retryable error until every
device resolves. A device without a `DEVNAME` that its other names already exclude (e.g. `/dev/pts/*` under an allow list that does not name
it) is simply not granted.

Each container with at least one `/dev` mount produces one summary line with counts only:

```
level=INFO msg="container processed" id=abc pid=1234 devices_granted=12 skipped=87 errors=0 dry_run=false
```

`devices_granted` is the number of granted devices, `skipped` counts candidates excluded by policy, names outside `/dev`, dangling names,
symlinks to directories and non-device entries, and `errors` counts per-device failures.

## Package layout

| Package | Responsibility |
| --- | --- |
| `cmd/swarm-device-access` | Entrypoint: flags, settings merge and validation, config file reload on `SIGHUP` (`config.go`), wiring |
| `internal/config` | Strict YAML loader, runtime config `Store` with generations and its `Publisher` |
| `internal/policy` | Cross-platform policy evaluation: mode (opt-in/all), label parsing, glob allow/deny (`Denied`, `Authorized`) |
| `internal/daemon` | Event loop (`Run`, `listenEvents`, `consumeEvents`, `processOne`) and the coordinator that owns passes, retries, the `processed` dedup and container reservations |
| `internal/processor` | `Reconcile`, `Terminate`, `Sweep`, `PublishAndReconcile`; desired-set computation (`CollectMountRules`, `devfs.go`), process pinning (`pin.go`), lifecycle history (`lifecycle.go`) |
| `internal/cgroup` | Device-rule application behind `Interface.SetDeviceRules(handle, rules)`: cgroup v1 (`v1.go`, `v1set.go`, `ledger.go`) and v2 (`v2.go`, `v2plan.go`, `v2ops.go`, `owned.go`, `ebpf.go`); cgroup handles (`handle.go`); `/proc` cgroup parsing. NVIDIA-derived code. |
| `internal/observability` | Prometheus metrics (`Recorder`, nil-safe), `/healthz`, `/readyz`, pprof servers |
| `internal/logger` | `slog`-based singleton with `text`, `json`, `plain` handlers |
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
Docker copies top-level labels into each task container, which is the only
place the daemon reads them. Labels under `deploy.labels:` are Swarm service
metadata and are ignored: the daemon never inspects the parent service or the
node's Swarm role, so the policy for a container is the same on every node and
across daemon restarts.

| Label | Description |
| --- | --- |
| `swarm-device-access.enable` | `true` to opt in, `false` to explicitly opt out. |
| `swarm-device-access.device-allow` | Comma-separated globs narrowing global allow. Empty = inherit. |
| `swarm-device-access.device-deny` | Comma-separated globs added on top of global deny. |

**Decision rule** for a given container and device (see [Device mount collection](#device-mount-collection) for its names):

```
enabled    = (enable != false) AND (mode=all OR enable=true)
Denied(p)     = p matches global-deny OR p matches container-deny
Authorized(p) = (global-allow empty OR p matches global-allow)
            AND (container-allow empty OR p matches container-allow)
granted    = no name it was found by (alias, resolved, canonical) is Denied
         AND Authorized(canonical) AND Authorized(resolved)
```

Global is the maximum allowed access; per-container labels can only narrow it. Deny always wins over allow. Invalid label values cause the container to be skipped (fail-closed).

## Runtime contract

The daemon **must** run as root with:

- `--privileged` (or equivalent capabilities: `CAP_BPF`, `CAP_PERFMON`, `CAP_SYS_ADMIN`, `CAP_SYS_RESOURCE`)
- `--cgroupns=host`, `--pid=host`, `--userns=host`
- `/sys` bind-mounted at `/host/sys` inside the container
- `/dev` bind-mounted (device candidates are opened beneath it with `openat2` and identified with `fstat` and sysfs)
- `/var/run/docker.sock` bind-mounted

The DBus socket is optional — enables systemd reload handling. Mount as `-v /run/dbus/system_bus_socket:/var/run/dbus/system_bus_socket`; the container-side path must be under `/var/run/` because `dhi.io/static` has no `/var/run → /run` symlink.

## Observability

### Prometheus metrics (`--metrics-addr`)

When `-metrics-addr=127.0.0.1:9090` is set, the daemon exposes:

| Endpoint | Description |
| --- | --- |
| `/metrics` | Prometheus text format |
| `/healthz` | Liveness probe — always `200 OK` |
| `/readyz` | `200 OK` when subscribed to Docker events; `503` on startup or reconnect |

Metrics exposed:

| Metric | Type | Labels | Description |
| --- | --- | --- | --- |
| `sda_events_total` | counter | `event` | Docker events received (`start`, `unpause`, `die`, `destroy`) |
| `sda_rules_applied_total` | counter | `result` | Containers handled: `ok` = no container-level failure (includes policy, label and no-pid skips and partial per-device failures); `error` = container-level failure |
| `sda_reload_reapplies_total` | counter | — | Re-applies after systemd daemon-reload |
| `sda_docker_reconnects_total` | counter | — | Docker event stream reconnects |
| `sda_apply_duration_seconds` | histogram | — | Wall-clock time per container apply |
| `sda_containers_scanned_total` | counter | — | Containers that passed policy and were processed |
| `sda_containers_skipped_total` | counter | `reason` | Containers skipped (`policy`, `invalid_labels`, `no_pid`) |
| `sda_device_files_discovered_total` | counter | — | Device files found across all processed containers |
| `sda_device_candidates_skipped_total` | counter | `reason` | Names under a `/dev` mount that name no device: `outside_dev`, `dangling`, `not_device` |
| `sda_rule_failures_total` | counter | — | Per-device rule failures (including walk errors); the per-device failure signal |
| `sda_dry_run_skips_total` | counter | — | Rules skipped in dry-run mode |
| `sda_last_event_timestamp_seconds` | gauge | — | Unix timestamp of last container processed successfully (event, startup or reload) |
| `sda_reconcile_pending_containers` | gauge | — | Containers whose last reconcile failed and are retried with backoff |
| `sda_reload_incomplete` | gauge | — | 1 until a pass for the latest config and trigger has reached every running container, else 0 |

### pprof debug server (`--debug-addr`)

When `-debug-addr=127.0.0.1:6060` is set, the standard Go pprof endpoints are available at `/debug/pprof/*`. Only bind to localhost in production.

## Dry-run mode

`-dry-run` logs the complete device set each container _would_ get (at `Info` level) without pinning its process, reading its `/proc` entry,
querying its cgroup, calling `bpf(2)` or writing to `devices.allow`/`devices.deny`. Useful for auditing policy and CI smoke tests.

```
level=INFO msg="dry-run: would add device rule" id=abc type=c major=195 minor=0
level=INFO msg="dry-run: would set device rules" id=abc rules=1
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
   level=DEBUG msg="setting device rule" pid=1234 cgroup=/host/sys/fs/cgroup/... type=c major=195 minor=0
   ```

4. If step 2 fires but step 3 does not, look for `container processed` with `errors` above 0, `device rule failed` or `device set incomplete`
   (a device whose identity could not be established), and for `cgroup path resolved`; a `/proc` parse failure usually means `pid: host` is
   not set.
5. If rules were applied but the container still gets `EACCES`, verify the container is on cgroup v2 and `BPF_F_ALLOW_MULTI` is supported (kernel ≥
   4.15).

### Daemon does not re-apply rules after `systemctl daemon-reload`

Ensure the host DBus socket is bind-mounted as `-v /run/dbus/system_bus_socket:/var/run/dbus/system_bus_socket`. Without it, the reload watcher is disabled (logged at `Warn` on startup). The container-side path must be `/var/run/dbus/system_bus_socket` — `dhi.io/static` has no `/var/run → /run` symlink.

### Daemon cannot connect to Docker

Check the socket path with `-docker-socket`. Default: `/var/run/docker.sock`. On some hosts Docker Desktop uses a different path.

The daemon requires Docker Engine 19.03+ (API 1.40). The `github.com/moby/moby/client` SDK refuses older engines with `API version … is not supported by this client: the minimum supported API version is 1.40`.
