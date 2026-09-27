# Security Policy

## Threat model

`swarm-device-access` is a privileged Linux daemon that:

- Connects to the host Docker socket (`/var/run/docker.sock`) and subscribes to container events.
- Reads `/proc/<pid>/{cgroup,mountinfo}` for every starting container.
- Writes eBPF `BPF_CGROUP_DEVICE` programs into the host cgroup v2 hierarchy, or writes to `devices.allow` and `devices.deny` on cgroup v1 hosts.
- Requires `privileged: true`, `cgroupns: host`, `pid: host`, `userns: host`, and bind mounts of the host `/sys` and `/dev`.

The combination of `privileged: true`, host `/dev` bind mount, and host Docker
socket is functionally equivalent to host-root access on the node. A compromised
or misconfigured daemon can read or write any host device and affect any container
running on the same machine.

The security boundary is the host Docker socket: any process that can create
containers with `/dev/...` bind mounts can influence which device-allow rules
this daemon applies. Do not expose the daemon's host socket to untrusted callers.

**Policy inputs.** Which devices a container may open is decided only by the
daemon's own configuration (flags and config file) and the container's own
`swarm-device-access.*` labels. Swarm service-level labels (`deploy.labels:`)
are ignored, and the daemon does not inspect services or the node's Swarm role,
so a decision cannot differ between manager and worker nodes or change after a
daemon restart.

**Device identity.** A mount source, and every name under a mounted
directory, is resolved beneath `/dev` with `openat2(RESOLVE_BENEATH)`, so no
path or symlink can lead the daemon outside `/dev`. Each device is identified
by the device number of the opened node and its kernel name from sysfs
(`DEVNAME`), and policy is applied to that identity: a node planted in a
world-writable `/dev` directory is judged as the device it is. A device whose
identity cannot be established, or a directory mount that cannot be
enumerated completely, leaves the container with no grants until a retry
succeeds. Deny globs also apply to aliases, but only as a best-effort veto at
evaluation time; the boundary to rely on is a glob on the kernel name (see the
README's Security section).

**Opt-in default.** The daemon defaults to `-policy-mode=opt-in`, processing only
containers that carry `swarm-device-access.enable: "true"`. This minimises blast
radius — containers that happen to bind-mount `/dev/...` paths are not silently
granted extra device access unless an operator explicitly opts them in.

## Supported versions

Only the latest release on the default branch (`master`) receives security fixes.
Older releases are not patched.

## Reporting a vulnerability

**Please do not open a public GitHub issue for security vulnerabilities.**

Use GitHub's private vulnerability reporting:

1. Go to the [Security Advisories](https://github.com/leinardi/swarm-device-access/security/advisories) page.
2. Click **Report a vulnerability**.
3. Fill in the description, affected versions, and reproduction steps.

Expected response time: acknowledgment within 7 days, patch or mitigation plan within 30 days for confirmed issues.

## Known design constraints

- The daemon must run as root. Dropping to a minimal capability set (`CAP_BPF`, `CAP_PERFMON`, `CAP_SYS_ADMIN`, `CAP_SYS_RESOURCE`) is theoretically possible but has not been tested across kernel versions and is not supported at this time.
- The daemon uses `privileged: true` in the reference Swarm deployment, started by a wrapper service with `docker run`. Swarm services do support `cap_add` (Docker Engine 20.10 and newer), but not the host cgroup, PID and user namespaces the daemon also needs, which is why the wrapper exists. With the wrapper, or outside Swarm, explicit capabilities could be used instead of `privileged`; that is not tested or supported (see above).
- **cgroup v1: grants made by a previous daemon instance are not revoked.** On cgroup v1 the daemon
  can only tell its own `devices.allow` exceptions from the runtime's through an in-memory ledger. A
  restarted daemon starts with an empty ledger, so exceptions its predecessor added look like the
  runtime's baseline and are never revoked by a later narrowing; they go away when the container is
  restarted. Within one daemon run, narrowing revokes exactly what was granted and never touches the
  runtime's own exceptions. cgroup v1 is deprecated by Docker and systemd; cgroup v2 hosts are not
  affected by this gap.
- **Restarting into dry-run does not revoke anything.** Dry-run never queries or mutates cgroups
  (it runs unprivileged in tests), so grants left by a previous live run stay until a live start
  or a container restart; the daemon warns about this at every dry-run start. Restart live with a
  narrower policy, or restart the affected containers, to take grants back.
- The cgroup BPF code in `internal/cgroup/` is derived from [NVIDIA's container toolkit](https://github.com/NVIDIA/libnvidia-container) (Apache 2.0). Security issues in that code should also be reported to NVIDIA.
