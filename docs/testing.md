# Testing

This project has two test levels: unit tests (`make go-test`) and the
integration suite under `test/integration`, which runs against a live Docker
daemon.

The integration suite has two parts:

- **Dry-run tests** (`daemon_test.go`) run the daemon binary as an unprivileged
  host process with `-dry-run`. They verify the event, inspect, policy, cgroup
  path resolution and device discovery pipeline without attaching BPF programs,
  and are safe on any host with Docker.
- **The enforcement test** (`enforce_test.go`) runs the daemon image for real.
  It checks that an opted-in container can open a bind-mounted device only
  after the daemon attached its BPF program, that an unlabelled container still
  cannot, and that the rule comes back after `systemctl daemon-reload`. It is
  guarded by `SDA_IT_ENFORCE=1` and skips without it.

`make go-test-integration` is the single way to run either part. It builds the
binary (and, with `SDA_IT_ENFORCE=1`, the daemon image as
`swarm-device-access:integration`), and owns the `go test` timeout and the
suite deadline:

```bash
make go-test-integration                                        # dry-run tests only
make go-test-integration SDA_IT_ENFORCE=1                       # plus the enforcement test
make go-test-integration SDA_IT_ENFORCE=1 SDA_IT_REQUIRE_RELOAD=1
```

The GitHub integration workflow runs the guarded enforcement test on every pull
request and push, with both variables set:

- `SDA_IT_ENFORCE=1` is the guard. With it set, a missing prerequisite fails the
  run instead of skipping it, and the workflow also fails the job if any test
  reports `SKIP`, so a green run always means BPF was exercised.
- `SDA_IT_REQUIRE_RELOAD=1` makes reload coverage mandatory. The reload subtest
  first proves that `daemon-reload` wipes the device program on this host;
  without the variable, a host where that cannot be proven skips only that
  subtest.

Prerequisites for the enforcement test:

- cgroup v2;
- systemd as init, with its DBus socket at `/run/dbus/system_bus_socket`;
- passwordless `sudo` for `sudo -n systemctl daemon-reload` (reload subtest);
- the device node `/dev/loop-control`, or `/dev/loop0` as a fallback;
- a Docker login to `dhi.io`, whose base images the daemon image is built from.

**Safety.** The enforcement test attaches BPF programs to its own test
containers and runs `systemctl daemon-reload` on the host. Only run it on a
throwaway or trusted host, such as a CI runner or a disposable VM.

Every container the suite creates carries the `swarm-device-access-it.envid`
label. Each run prints a one-line cleanup command for its own containers; after
a killed run, `make sweep-test-leaks` removes the leftovers of every run.

The native privileged checklist below is now partly automated. The enforcement
test covers step 3 (an opted-in container with a real `/dev/...` bind mount gets
its rule), checks the outcome of step 4 by opening the device rather than
inspecting the program with `bpftool`, and covers step 7 (the rule is
re-applied after `systemctl daemon-reload`). Run the checklist by hand for the
remaining steps, and for these on hosts the test does not cover.

## Dry-Run Coverage

The dry-run tests cover:

- Docker event subscription for `start` events.
- Startup enumeration against currently running containers.
- Container inspection and `/proc/<pid>` cgroup parsing.
- Host cgroup path resolution for private Docker cgroup namespaces.
- `/dev/...` bind mount detection.
- Dry-run device rule generation for `/dev/null`.
- `-policy-mode=opt-in` filtering: containers without `swarm-device-access.enable=true` are skipped.
- Per-container `swarm-device-access.enable` label opt-in.

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

3. Start a consumer container with `--label swarm-device-access.enable=true` and
   a real `/dev/...` bind mount and confirm the daemon logs `device mount detected`
   and `adding device rule`. (For Swarm stacks, the equivalent placement is
   `deploy.labels:` in the service spec — `docker service create --label` writes
   to the same location.)

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
