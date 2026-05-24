# Testing

This project has two test levels.

The default integration suite must stay safe for public GitHub runners. It runs
the daemon as an unprivileged host process with `-dry-run`, uses the Docker
socket provided by the runner, and verifies the event, inspect, policy, cgroup
path resolution, and device discovery pipeline without attaching BPF programs.

Run the CI-safe suite from the repository root:

```bash
make go-build
go test -tags=integration -timeout=60s -v ./test/integration/...
```

The native Linux host can also be used for opt-in privileged checks. Do not add
these checks to the default GitHub workflow unless they are explicitly guarded,
because they require host namespaces, privileged containers, and sometimes
systemd DBus access.

## CI-Safe Coverage

The default integration tests cover:

- Docker event subscription for `start` events.
- Startup enumeration against currently running containers.
- Container inspection and `/proc/<pid>` cgroup parsing.
- Host cgroup path resolution for private Docker cgroup namespaces.
- `/dev/...` bind mount detection.
- Dry-run device rule generation for `/dev/null`.
- `-require-label` filtering.

## Native Privileged Checklist

Use this checklist manually on a trusted Linux host when changing cgroup, BPF,
or deployment behavior:

1. Build and run the daemon container with host privileges:

   ```bash
   make docker-build
   make docker-run DOCKER_RUN_ARGS="-log-level debug"
   ```

2. Verify the container has the required runtime wiring:

   ```bash
   docker inspect swarm-device-access
   ```

3. Start a consumer container with a real `/dev/...` bind mount and confirm the
   daemon logs `device mount detected` and `adding device rule`.

4. If the host uses cgroup v2, confirm a `BPF_CGROUP_DEVICE` program is attached
   to the consumer cgroup with `bpftool`.

5. Test directory walking with a directory such as `/dev/bus/usb` when such
   devices are present.

6. Test `-device-allow` and `-device-deny` with a dry-run daemon first, then with
   the privileged daemon.

7. If `/run/dbus/system_bus_socket` is mounted and systemd is available, run
   `systemctl daemon-reload` and verify the daemon logs that it re-applied rules.

8. Verify observability by starting with `-metrics-addr :9090` and
   `-debug-addr :6060`, then checking `/healthz`, `/readyz`, `/metrics`, and
   `/debug/pprof/`.
