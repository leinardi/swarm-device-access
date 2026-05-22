# Security Policy

## Threat model

`device-mapping-manager` is a privileged Linux daemon that:

- Connects to the host Docker socket (`/var/run/docker.sock`) and subscribes to container events.
- Reads `/proc/<pid>/{cgroup,mountinfo}` for every starting container.
- Writes eBPF `BPF_CGROUP_DEVICE` programs into the host cgroup v2 hierarchy, or writes to `devices.allow` on cgroup v1 hosts.
- Requires `privileged: true`, `cgroupns: host`, `pid: host`, `userns: host`, and bind mounts of the host `/sys` and `/dev`.

The security boundary is the host Docker socket: any process that can create containers with `/dev/...` bind mounts can influence which device-allow rules this daemon applies. Do not expose the daemon's host socket to untrusted callers.

## Supported versions

Only the latest release on the default branch (`master`) receives security fixes.
Older releases are not patched.

## Reporting a vulnerability

**Please do not open a public GitHub issue for security vulnerabilities.**

Use GitHub's private vulnerability reporting:

1. Go to the [Security Advisories](https://github.com/leinardi/device-mapping-manager/security/advisories) page.
2. Click **Report a vulnerability**.
3. Fill in the description, affected versions, and reproduction steps.

Expected response time: acknowledgment within 7 days, patch or mitigation plan within 30 days for confirmed issues.

## Known design constraints

- The daemon must run as root. Dropping to a minimal capability set (`CAP_BPF`, `CAP_PERFMON`, `CAP_SYS_ADMIN`, `CAP_SYS_RESOURCE`) is theoretically possible but has not been tested across kernel versions and is not supported at this time.
- The daemon uses `privileged: true` in the reference Swarm deployment. This is required because Swarm rejects `--cap-add` on service definitions. In non-Swarm deployments, explicit capabilities can be used instead.
- The cgroup BPF code in `internal/cgroup/` is derived from [NVIDIA's container toolkit](https://github.com/NVIDIA/libnvidia-container) (Apache 2.0). Security issues in that code should also be reported to NVIDIA.
