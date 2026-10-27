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
The cgroup/BPF implementation in `src/cgroup/` is ported from
[NVIDIA's container toolkit](https://github.com/NVIDIA/k8s-device-plugin)
and remains Apache 2.0 licensed.

Version 2.0 is a Rust rewrite of the 1.x Go daemon, with the same behaviour
except for the differences listed in
[Migrating from 1.x](#-migrating-from-1x), the first of them that every flag
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

## 🧭 Alternatives

Swarm has had no `devices:` option since swarm mode shipped; the request is
[moby/swarmkit#1244](https://github.com/moby/swarmkit/issues/1244), and every
upstream patch so far was closed without being merged. These are the usual
workarounds and how they compare:

| Approach | How it works | Trade-off |
| --- | --- | --- |
| `generic_resources` with the NVIDIA runtime | Each node advertises its GPUs in `daemon.json`; the service reserves them under `deploy.resources.reservations.generic_resources` and the NVIDIA runtime injects the devices | The native option, and Swarm schedules on GPU count. NVIDIA GPUs only. |
| A service that runs `docker run --device ...` through the Docker socket | The service is only a wrapper; the device container is a plain container on the node | Works for any device and any `docker run` option, but the device container is not a Swarm task: no rolling updates, rollbacks, secrets or configs, and it keeps running when the stack is removed. |
| A script on `docker events` writing `devices.allow` | Adds a cgroup v1 device rule when a container starts | Only works on cgroup v1, which current distributions no longer use by default. |
| [`allfro/device-mapping-manager`](https://github.com/allfro/device-mapping-manager) | A privileged container that grants every device bind-mounted under `/dev` | The project this one forked. No commits since January 2024, its issues report it failing on Docker Engine 29, and it grants every container that mounts a device, with no opt-in. |
| This daemon | Grants the devices a task bind-mounts under `/dev`, on cgroup v1 and v2 | Any device and an unchanged Swarm task, gated by opt-in labels and allow and deny globs. It does not schedule: see [Placing Tasks on Nodes With the Device](#placing-tasks-on-nodes-with-the-device). |

For NVIDIA GPUs, prefer `generic_resources` when the NVIDIA runtime is installed
on the nodes. Use this daemon for every other device, and for GPUs whose
device nodes you would rather bind-mount yourself.

## 🚀 Quick Start

Run one daemon instance on every Swarm node that may run services needing host
device access.

The daemon needs host-level privileges: `privileged`, host cgroup namespace,
host PID namespace, host user namespace, the host Docker socket, `/sys` mounted
at `/host/sys`, and `/dev` mounted into the daemon container.

It requires Docker Engine 19.03 or newer (API 1.40): the Docker client it uses
refuses older engines. It negotiates the API version with the engine, so
current releases, Docker Engine 29 included, work too.

Swarm does not allow those runtime options directly on a service. So the image
has a second mode, `launch`: the Swarm service runs the image unprivileged, with
only the Docker socket, and the launcher creates the privileged daemon container
from the very image it runs itself (see
[Docker Compose for Swarm](#docker-compose-for-swarm)).

### Host Requirements

On every host the daemon needs Linux 5.6 or newer. It resolves every device
path beneath `/dev` with `openat2(2)` and `RESOLVE_BENEATH` (Linux 5.6), and
refuses to start with `unsupported kernel` when that is missing. It also pins
each running container's process with `pidfd_open(2)` (Linux 5.3) before
changing its cgroup (see below).

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

Every time the daemon looks at a container (start, unpause, startup, systemd
reload) it sets the container's grants to the complete set the current
configuration allows, including none at all. A container that policy disables
or does not opt in, one with invalid labels, and one where any mounted device
cannot be resolved all get the empty set, so an earlier grant is revoked rather
than kept. The price is that a transient error (an unreadable device node, for
example) revokes the container's grants until the retry that grants them again.

The daemon pins the container's process with a pidfd (Linux 5.3+) before it
resolves the cgroup, and immediately before changing anything checks that the
process is still alive, that Docker still reports it as the container's
running process with the same start time and, for a grant, that it is still in
that cgroup. A recycled pid therefore never directs grants to another cgroup.
Once a container has exited, or when Docker cannot be asked about it, its
grants are revoked in the cgroup the daemon last verified for it, and only if
that directory is still the same one (same inode) and was not recreated.
A running container found in another cgroup than the one it was granted in
(systemd inside the container moving its processes into a child cgroup, for
example) loses the grants in the previous cgroup before it is granted in the
new one, since a grant on a parent cgroup applies to every cgroup below it.

The daemon keeps a short history of each container's runs. A `die` or
`destroy` event revokes the grants of the run it ended (the latest run that
started before the event, so a late event for an earlier run never touches a
container restarted under the same ID). What the daemon remembers about a run
is dropped only once its cgroup is verified gone; a periodic sweep checks.

### Docker Compose for Swarm

```yaml
services:
  swarm-device-access:
    image: ghcr.io/leinardi/swarm-device-access:2
    # Swarm rejects privileged/cgroup/pid/userns on services. The launcher runs
    # unprivileged and creates the daemon container from this same image.
    command:
      - launch
      # Launcher flags. Reapply device rules after systemctl daemon-reload:
      # - --dbus
      # Bind a host directory read-only at /etc/swarm-device-access. Mount the
      # directory, not the file: an editor that replaces the file would leave a
      # file mount on the old content, and a reload would silently apply it.
      # - --config-dir=/etc/swarm-device-access
      - --
      # Daemon flags, passed to the daemon verbatim.
      # Reload the config file with: docker kill -s HUP swarm-device-access
      # - --config=/etc/swarm-device-access/config.yaml
      - --log-level=info
      - --log-format=text
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock
    deploy:
      mode: global
      restart_policy:
        condition: any
      update_config:
        order: stop-first
    # The launcher's shutdown takes at most 20s; see below.
    stop_grace_period: 30s

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

`swarm-device-access launch [launch flags] -- [daemon flags]` finds its own
container, reads the ID of the image it runs, and creates one container named
`swarm-device-access` from that image ID, with the arguments after `--` as the
daemon's flags. The daemon container gets exactly the documented privileged
set: `--privileged`, the host cgroup, PID and user namespaces, no network, and
bind mounts of the host Docker socket at `/var/run/docker.sock`, `/sys` at
`/host/sys` and `/dev` at `/dev`, plus the optional mounts below. The launcher
streams the daemon's output as its own, so `docker service logs` shows the
daemon, and exits with the daemon's status.

| Launch flag | Default | Description |
| --- | --- | --- |
| `--dbus` | `false` | Bind the host's `/run/dbus/system_bus_socket` at `/var/run/dbus/system_bus_socket` in the daemon, so it reapplies rules after `systemctl daemon-reload`. |
| `--config-dir` | `""` | Host directory bound read-only at `/etc/swarm-device-access`, for the daemon's `--config`. Must be an absolute, clean path. |
| `--host-network` | `false` | Run the daemon in the host network instead of none; only `--metrics-addr` needs it. |
| `--host-docker-socket` | `/var/run/docker.sock` | Host path of the Docker socket bound into the daemon. Must be an absolute, clean path. The launcher itself always uses its own `/var/run/docker.sock` mount. |
| `--log-level` | `info` | The launcher's own log level: `debug`, `info`, `warn`, `error`. |
| `--log-format` | `text` | The launcher's own log format: `text`, `json`, `plain`. |
| `-h`, `--help` |  | Print the launch flags and exit |

Running the daemon from the launcher's own image ID, not a tag, means the
service image is the daemon image:

- An image watcher such as gantry, or `docker service update --image`, updates
  the service, and the new launcher starts the new daemon. Every node runs the
  same digest.
- A Swarm rollback (`docker service rollback`) rolls the daemon back with it.
- `update_config.order: stop-first` stops the old launcher, which stops and
  removes its daemon, before the new one starts. The launcher's shutdown (a 10s
  stop, the wait for the removal and the drain of the last log lines) shares a
  single 20s budget, inside `stop_grace_period: 30s`.

Each daemon container is labelled with the ID of the launcher that owns it.
Before it creates a daemon, a launcher removes a `swarm-device-access`
container only when it is a daemon whose launcher is confirmed gone or
stopped, or a daemon left by the `sh` wrapper of earlier releases. A daemon
whose launcher still runs, a launcher whose state cannot be read, or any other
container with that name makes the launcher exit with an error, and Swarm
retries it.

Releases before 1.0.0 documented a `docker:29` service that ran an `sh` wrapper
around `docker run`. It still works, but it is no longer documented: the
service image never changed, so image watchers and rollbacks did not follow
daemon releases. To migrate, replace the service with the one above; the first
launcher removes the daemon container the wrapper left running.

## ⚙️ Configuration

You can configure the daemon with CLI flags, a YAML config file, or both. When a
value is set in both places, the CLI flag wins.

| Flag | Default | Description |
| --- | --- | --- |
| `--log-level` | `info` | `debug`, `info`, `warn`, `error` |
| `--log-format` | `text` | `text`, `json`, `plain` |
| `--log-time` | `false` | Include timestamps in log lines |
| `--docker-socket` | `/var/run/docker.sock` | Path to the Docker daemon's UNIX socket |
| `--dry-run` | `false` | Log the device set each container would get, without reading its `/proc` entry or its cgroup and without writing anything. Not a cleanup path: grants left by a previous live run stay, and a reload can turn it off but not on (see [Config File](#config-file)) |
| `--policy-mode` | `opt-in` | `opt-in`: only `enable=true` containers. `all`: unless `enable=false`. |
| `--device-allow` | `""` | Glob for `/dev/...` paths to allow, one glob per flag; repeat the flag for more (not comma-split, unlike the labels). Empty means allow all. |
| `--device-deny` | `""` | Glob for `/dev/...` paths to deny, one glob per flag; repeat the flag for more (not comma-split, unlike the labels). Deny takes priority over allow. |
| `--metrics-addr` | `""` | `host:port` for Prometheus `/metrics`, `/healthz`, `/readyz`. Empty disables it. A busy or invalid address stops startup. |
| `--debug-addr` | `""` | Removed in 2.0 with the pprof debug server: any value but the empty one is refused at startup and on reload. |
| `--config` | `""` | Path to a YAML config file. CLI flags override file values. Reload with SIGHUP. |
| `-h`, `--help` |  | Print this flag list and exit |

`--log-level`, `--log-format` and `--policy-mode` (and their config file keys)
accept only the values listed; anything else is an error at startup and makes a
reload keep the previous config. The undocumented `--log-level` aliases
`warning`, `fatal` and `panic`, which earlier versions mapped to `warn` and
`error`, are no longer accepted.

### Config File

All CLI flags can be represented in YAML and loaded with `--config`. See
[`deployments/docker/config.yaml`](deployments/docker/config.yaml) for an
example.

The file is checked strictly: an unknown key (for example `device_deny` for
`device-deny`), a second YAML document, a YAML merge key (`<<`) or a value of
the wrong type is an error. `dry-run` and `log-time` must be `true` or
`false` (not `"yes"`); `policy-mode`, `log-format`, `log-level` and
`docker-socket` must be non-empty strings; `metrics-addr` must be a string,
and an empty one disables the listener; `debug-addr` must be empty; `device-allow` and
`device-deny` must be lists of non-empty strings. An explicit `null` (also
through an alias) is an error, never a silent "not set": leave a key out to
keep the flag's value.

Send `SIGHUP` to reload the config file without restarting the daemon:

```bash
docker kill -s HUP swarm-device-access
```

Mount the directory holding the file into the daemon container and pass
`--config` after the image name; the compose files above show both as commented
lines. Mount the directory rather than the file: editors and tools that write a
new file and rename it over the old one would leave a single-file mount on the
old content, and the reload would silently apply that.

Some settings are only read at startup and still require a restart:
`docker-socket` and `metrics-addr`. A reload that changes one of them logs
that it was ignored and applies the rest.

A reload computes every setting afresh from the command line, the file and the
defaults, in that order: a key removed from the file falls back to the flag
given on the command line, or to the default, not to what the file said
before. A reload that fails for any reason (unreadable or invalid file, invalid
value or policy) changes nothing, logging included, and the previous config
stays in force.

`dry-run` can be turned off by a reload, and the daemon then applies the
current policy for real. It cannot be turned on: a reload that would enable it
is rejected with `dry-run cannot be enabled on a running daemon; to stop
enforcing, restart the daemon live with a narrower policy, or restart the
affected containers`, and the previous config is kept, because dry-run would
stop revoking while every grant stayed in place.

A reload, a daemon start and a `systemctl daemon-reload` each reconcile every
running container under the current config, so narrowing the policy revokes
grants the new policy no longer allows. Until a pass has reached every running
container, `sda_reload_incomplete` is 1; containers whose reconcile failed are
retried with backoff and counted in `sda_reconcile_pending_containers`. The
daemon warns while either is non-zero and logs `config reload complete` once
both are zero for the latest config.

Restarting the daemon **live** is also a cleanup: the startup pass replaces or
strips the grants a previous instance left on running containers (on cgroup v1
only within the limits described under [Host Requirements](#host-requirements)).
Restarting into **dry-run** is not: dry-run never looks at cgroups, so grants a
previous live run left stay in place until a live start or a container
restart. The daemon logs `dry-run: grants left by a previous live run cannot be
detected or cleaned in dry-run` at every dry-run start.

### Container Labels

Consumer services opt in and narrow their allowed device set with labels:

| Label | Values | Description |
| --- | --- | --- |
| `swarm-device-access.enable` | `true` / `false` | Opt in (`true`) or explicitly opt out (`false`) of processing. |
| `swarm-device-access.device-allow` | Comma-separated globs | Allow only matching `/dev/...` paths. Empty means inherit. |
| `swarm-device-access.device-deny` | Comma-separated globs | Deny matching `/dev/...` paths. Deny overrides allow. |

Write deny globs against kernel device names (`/dev/sd*`, `/dev/nvme*`), not
against aliases such as `/dev/disk/by-id/*`: see [Security](#-security).

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

Globs use Go's `filepath.Match` syntax, and `*` does not match across a `/`:
`/dev/bus/usb/*` matches the bus directories `/dev/bus/usb/001`, not the device
node `/dev/bus/usb/001/002`. Write one `*` per level, e.g. `/dev/bus/usb/*/*`.
Every glob, in flags, the config file and labels, must be an absolute, clean
path under `/dev/` (no relative pattern, `..`, `//` or trailing `/`); anything
else is rejected at startup or reload, and in a label it makes the container
get no devices. Such a glob could never match a device path, so a deny written
that way would silently deny nothing.

A device can be reached under several names: the mounted name (an alias such
as `/dev/disk/by-id/usb-...`), the node it resolves to, and its kernel name
(`DEVNAME` in sysfs, e.g. `/dev/sda`). Deny globs are checked against all three,
and a match on any name of a device, in any of the container's mounts, denies
it. Allow globs must match the kernel name and the resolved node; an alias
that no allow glob names does not deny anything. A node planted elsewhere
under `/dev` is therefore judged as the device it really is. A name that
leads outside `/dev` is skipped (logged as a warning only when an allow glob
names it, since every whole-`/dev` mount has such links, like `/dev/stdin`); a
device whose identity cannot be established (no or ambiguous `DEVNAME` in
sysfs, an unreadable node) leaves
the container with no grants until a retry succeeds, unless policy already
excludes it under its other names. The same holds when a directory mount
cannot be enumerated completely: a read error, or more than 4096 entries
(`mount too large; narrow the bind mount`). A whole-`/dev` mount counts
everything under it, including files programs keep in `/dev/shm`, so on a busy
host it can hit that limit: mount the device directories you need instead.

Some pseudo devices have no `DEVNAME`; `/dev/pts/*` nodes, for example, have
no `/sys/dev/char/136:0` entry. To see why a device blocks a container, look
up its numbers (`ls -l /dev/pts/0` shows `136, 0`) and check
`ls /sys/dev/char/136:0` (`/sys/dev/block/M:m` for block devices). Blocking
such a device is intentional: bind-mount only the devices you need, or
exclude the rest with an allow list.

Global `--device-allow` and `--device-deny` define the broadest access the daemon
may grant. Per-container labels can only narrow that access. Deny rules always
win.

## 🔁 Migrating from 1.x

Version 2.0 replaces the Go daemon with a Rust one. The image name, the
`launch` mode, the config file keys, the labels, the log lines, the error texts
and the `sda_` metrics are those of 1.x; what differs is in the numbered table
below. Every flag now takes two dashes (Q1): update the `command:` of the Swarm
service, both the launch flags and the daemon flags after `--`, before you
deploy 2.0. A 1.x command line such as `- -log-level=info` makes the 2.0
launcher or daemon exit with status 2 and
`error: single-dash long flags are not supported: '-log-level'; use '--log-level'`,
and Swarm keeps restarting the task until the command is fixed.

Upgrading in place keeps every grant: the 2.0 daemon recognises the device
filters 1.x attached, and a rollback to 1.x recognises the ones 2.0 attached,
so neither direction needs the containers restarted. The numbers are stable
and referenced from the code and from
[`docs/rust-port.md`](docs/rust-port.md).

| # | Area | 1.x | 2.0 |
| --- | --- | --- | --- |
| Q1 | CLI | Go's `flag`: `-flag` and `--flag` both accepted; the daemon ignores positional arguments (`launch` refuses them) | Only `--flag`. A single-dash flag or a positional argument is refused with exit 2; `-h`/`--help` print to stdout and exit 0, with clap's layout of the same flag help. This also applies to the daemon arguments after `launch --`. The launcher's own refusals of an unknown flag or a stray argument, which Go's flag package made exit 1 after printing its usage, are clap's usage errors with exit 2; its host-path errors name the flag by its `--` spelling (`launch: flag "--config-dir": "rel" must be an absolute, clean path`). |
| Q2 | `debug-addr` / `--debug-addr` | Serves pprof | No pprof. A non-empty value is refused at startup and on reload. |
| Q3 | `/metrics` | `go_*` runtime families | No `go_*` families; `process_*` and `promhttp_metric_handler_*` are kept. |
| Q4 | `/metrics` | Protobuf or text, negotiated | Always text 0.0.4, with no protobuf negotiation. |
| Q5 | Environment | `GOMAXPROCS`, `GOMEMLIMIT`, `GOGC`, `GODEBUG` tune the runtime | These variables no longer have any effect. |
| Q6 | Config file syntax errors | yaml.v3's libyaml wording, e.g. `parse config file "<path>": yaml: line 2: found unexpected end of stream` | The same files are refused, with the same prefix up to `yaml:` and the space after it; the rest is saphyr-parser's wording and line, e.g. `yaml: line 1: while scanning a quoted scalar, found unexpected end of stream`. A file that is not UTF-8 reads `yaml: invalid UTF-8`, and a UTF-16 file, which yaml.v3 decodes, is refused the same way. saphyr-parser reads YAML 1.2 more strictly in three places, all refusals: a quoted value continued on a line indented no more than its key, which libyaml accepts; flow collections nested deeper than 255, which 1.x refused as a wrong shape or an unknown key; and a tab between `:` and a plain value (`policy-mode:<TAB>all`), which libyaml reads as separation and saphyr-parser refuses with `yaml: line 1: ':' must be followed by a valid YAML whitespace` (a space, or a tab before a quoted value, reads as in 1.x). yaml.v3's own errors keep their text: unknown anchors, nesting past libyaml's depth of 10000, a `%YAML` version other than 1.1, a `...` before the first document, and every shape, decode, duplicate-key and multiple-document error. |
| Q7 | cgroup v1 refusal of a device rule that is not a concrete allow (unreachable: the daemon builds only concrete rules) | Go's `%+v` of the rule prints the addresses of its numbers: `{Allow:true Type:a Major:0xc000012345 Minor:0xc000012350 Access:r}` | The numbers themselves: `{Allow:true Type:a Major:10 Minor:200 Access:r}`; an absent one still reads `<nil>`. |
| Q8 | systemd reload watcher: the `err` of `systemd reload handling unavailable; retrying in the background` for the rarer failures | godbus's wording for a failed authentication or `Hello`, a broken reply, and any bus address other than one `unix:path=`, e.g. `connect system bus: dbus: authentication failed` | The same prefixes (`connect system bus:`, `add match signal <signal>:`, `resolve the owner of org.freedesktop.systemd1:`, each followed by a space) with zbus's wording after them. A socket that is missing or refuses the connection, a D-Bus error reply (no systemd on the bus) and the setup timeout read as in 1.x. |
| Q9 | Container process liveness before a device-rule mutation | `pidfd_send_signal(fd, 0)`: an exited process that is not yet reaped (a zombie) counts as alive, and the mutation goes ahead | The pidfd is polled, and a process that has exited counts as gone, reaped or not: the reconcile fails with `pinned container process exited: no such process` and is retried. rustix cannot send signal 0. |
| Q10 | Launcher: a wait answer whose JSON has a wrong type (unreachable: dockerd sends a number for `StatusCode`) | `encoding/json`'s wording, e.g. `daemon wait stream failed: json: cannot unmarshal string into Go struct field WaitResponse.StatusCode of type int64` | The same prefix, with serde_json's wording after it, e.g. `daemon wait stream failed: invalid type: string "x", expected i64 at line 1 column 17`. A wait answer that is not JSON at all reads as in 1.x: the answer's own text. |
| Q11 | Metrics server: requests net/http refuses or frames otherwise | net/http's own answers to a malformed request (a bad request line or header, an unknown version, a missing `Host`), its header-count and size limits, its 400 to a partial pipelined request at the header deadline, and its 10 s drain of a slowly sent body before answering | hyper's: a 400 without a reason or a body to a malformed request or an unknown version, an HTTP/1.1 request without `Host` served, other header limits (for example a 431 above 100 headers), a close without a 400 at the header deadline, and the answer sent at once without reading the body. Paths, routes, status codes, bodies, header order, the 307 for unclean paths, HEAD, `Content-Length` or chunked framing and the four server deadlines are unchanged; chunk boundaries follow 2 KiB chunks rather than the encoder's writes. |
| Q12 | Metrics address with a host name that does not resolve | Go's resolver: `metrics server: listen on <addr>: listen tcp: lookup <host> on <nameserver>: no such host` (or its timeout and server-failure wordings) | The same prefix up to `lookup <host>:`, with getaddrinfo's wording after it, e.g. `failed to lookup address information: Name does not resolve`. inet_aton's shorthands (`0`, `127.1`) read `lookup <host>: no such host`. A name that resolves binds the same address. |

## 🧩 Tips and Limitations

### Placing Tasks on Nodes With the Device

The daemon grants access on whichever node a task lands on; it does not decide
where tasks run. Label the nodes that have the device and constrain the service
to them, or Swarm may start the task on a node where the bind mount fails:

```bash
docker node update --label-add device.zigbee=true node-1
```

```yaml
    deploy:
      placement:
        constraints:
          - node.labels.device.zigbee == true
```

For NVIDIA GPUs, `generic_resources` also lets Swarm schedule by GPU count (see
[Alternatives](#-alternatives)).

### Stable Names for USB and Serial Devices

A USB adapter can come up as `/dev/ttyUSB0` on one boot and `/dev/ttyUSB1` on
the next. Mount a stable alias instead: the links udev already creates under
`/dev/serial/by-id/`, or a name of your own from a udev rule:

```text
# /etc/udev/rules.d/99-zigbee.rules
SUBSYSTEM=="tty", ATTRS{idVendor}=="10c4", ATTRS{idProduct}=="ea60", SYMLINK+="zigbee"
```

```yaml
    volumes:
      - /dev/zigbee:/dev/zigbee
    labels:
      swarm-device-access.enable: "true"
      swarm-device-access.device-allow: "/dev/ttyUSB*"
```

Write `device-allow` against the kernel name the alias points to
(`/dev/ttyUSB*` here), not the alias: an allow glob that names only the alias
grants nothing (see [Container Labels](#container-labels)).

### Devices That Appear After the Container Starts

The daemon walks a container's device mounts when it starts and on each later
pass (a `SIGHUP`, a daemon restart, a `systemctl daemon-reload`); it does not
watch for devices plugged in between. Tracked in
[#41](https://github.com/leinardi/swarm-device-access/issues/41). Until then:

- A mount of a single node or alias (`/dev/zigbee`) is fixed when the container
  starts: Docker resolves it then. If the device is unplugged and plugged back,
  restart the task.
- A directory mount (`/dev/bus/usb`) shows new nodes inside the container, but
  they are not granted until the next pass. Restart the task, or trigger a pass
  for every container on that node with
  `docker kill -s HUP swarm-device-access`; it works without a config file.

### FUSE

Granting `/dev/fuse` lets a container open it, but mounting a FUSE filesystem
also needs `CAP_SYS_ADMIN` (`cap_add`) and an AppArmor profile that allows
`mount`. Docker's default profile denies it, and `docker stack deploy` ignores
`security_opt`, so on AppArmor hosts the device grant alone is not enough.

## 🔒 Security

This daemon is powerful by design. It runs privileged, mounts the host `/dev`
tree, and can talk to the Docker socket. Treat it as host-root equivalent: if it
is compromised or misconfigured, every container on that node may be affected.

The default policy mode is `opt-in` for that reason. Only containers with
`swarm-device-access.enable: "true"` are processed, so unrelated containers that
happen to bind-mount something under `/dev` are not silently granted access.

If every workload on the node is trusted, `--policy-mode=all` can be used to
process all containers unless they explicitly set
`swarm-device-access.enable: "false"`.

The security authority is a device's canonical identity: its device number,
named by its kernel name (`DEVNAME` in sysfs). Cgroup rules grant a
`major:minor`, not a path. A deny glob that matches only an alias (a
`/dev/disk/by-id/...` link, a node someone planted under `/dev/shm`) vetoes the
device while that alias is present when the daemon evaluates the container,
but a container that can change the directory it mounts can hide the alias
before a later pass. Treat alias-level deny as a best-effort veto, and write
the denies you rely on against kernel names, for example `/dev/sd*` and
`/dev/nvme*`. Allow globs are always checked against the kernel name and the
node a name resolves to, so an alias alone never grants a device.

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
level=DEBUG setting device rule pid=1234 cgroup=/host/sys/fs/cgroup/... type=c major=4 minor=64
```

## 🛠️ Development

### Prerequisites

- Linux host. The daemon is Linux-only.
- Rust, through `rustup`: `rust-toolchain.toml` pins the exact release and its components.
- Docker, logged in to `dhi.io` (`docker login dhi.io`): the image builds from Docker Hardened
  Images.
- `pre-commit`, `cargo-deny`, [droast](https://github.com/immanuwell/dockerfile-roast),
  `markdownlint-cli2`, `yamllint`, `shellcheck`, `actionlint`, and `checkmake`. The pre-commit
  hooks install most of these tools when needed; droast is built from source, which needs `cargo`.
- A local `shellcheck` at the release `.github/actions/setup-shellcheck` pins: droast's required
  ShellCheck bridge runs the one on PATH.
- For the static release binaries only (`make rust-release-binaries`): the zig and cargo-zigbuild
  releases `.github/actions/setup-zigbuild` pins, on PATH.

### Common Tasks

```bash
make help              # list all available targets
make check             # run every pre-commit hook on every file
make verify            # build, format check, clippy and every test, meta-tests included
make rust-build        # build the workspace (target/debug/swarm-device-access)
make docker-build      # build the container image
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

[Apache License 2.0](LICENSE). The code in `src/cgroup/` is ported from
[NVIDIA's container toolkit](https://github.com/NVIDIA/k8s-device-plugin) under
the same license.
