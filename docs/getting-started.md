# Getting started

Run one daemon instance on every Swarm node that may run services needing host
device access.

The daemon needs host-level privileges: `privileged`, host cgroup namespace,
host PID namespace, host user namespace, the host Docker socket, `/sys` mounted
at `/host/sys`, and `/dev` mounted into the daemon container.

It negotiates the Docker API version with the engine once, so current
releases work, Docker Engine 29 included.

Swarm does not allow those runtime options directly on a service. So the image
has a second mode, `launch`: the Swarm service runs the image unprivileged, with
only the Docker socket, and the launcher creates the privileged daemon container
from the very image it runs itself (see
[Docker Compose for Swarm](#docker-compose-for-swarm)).

## Docker Compose for Swarm

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
[`deployments/docker/docker-compose.yaml`](https://github.com/leinardi/swarm-device-access/blob/master/deployments/docker/docker-compose.yaml).

## Verifying device passthrough

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
DEBUG device mount detected id=abc123 pid=1234 source=/dev/ttyS0 destination=/dev/ttyS0
DEBUG setting device rule pid=1234 cgroup=/host/sys/fs/cgroup/... type=c major=4 minor=64
```
