# Testing

This project has two test levels: the unit, golden and meta-tests of the Cargo workspace
(`make rust-test`, part of `make verify`) and the integration suite in the `testenv` crate, behind
its `integration` feature, which runs against a live Docker daemon.

The integration suite has three parts:

- **Dry-run tests** run the daemon binary as an unprivileged host process with `--dry-run`. They
  verify the event, inspect, policy, cgroup path resolution and device discovery pipeline without
  attaching BPF programs, and are safe on any host with Docker.
- **The enforcement tests** run the daemon image for real. They check that an opted-in container
  can open a bind-mounted device only after the daemon attached its BPF program, that an
  unlabelled container still cannot, and that the rule comes back after `systemctl daemon-reload`.
  `TestLauncher_RunsDaemonFromItsOwnImage` runs the image in launch mode. They are guarded by
  `SDA_IT_ENFORCE=1` and skip without it.
- **The upgrade and rollback gate** (`TestUpgrade_GoToRustAndBack`, also behind
  `SDA_IT_ENFORCE=1`) runs the last 1.x Go daemon and the Rust daemon on one container, described
  below.

`make test-integration` is the single way to run the suite. It builds the image from the shipped
`deployments/docker/Dockerfile` with `make docker-build` under the fixed tag
`swarm-device-access:integration`, copies the binary out of that image, pulls the pinned 1.x
image, and owns the suite deadline. The build pulls the `dhi.io` bases, so it needs a
`docker login dhi.io`:

```bash
make test-integration                                        # dry-run tests only
make test-integration RUN='^TestDaemon_DryRun_'              # Go's -run semantics
make test-integration SDA_IT_ENFORCE=1                       # plus the enforcement tests and the gate
make test-integration SDA_IT_ENFORCE=1 SDA_IT_REQUIRE_RELOAD=1
```

The GitHub integration workflow runs the guarded suite on every push and on every pull request
from this repository, with both variables set:

- `SDA_IT_ENFORCE=1` is the guard. With it set, a missing prerequisite fails the run instead of
  skipping it, and the workflow also fails the job if any test reports `SKIP`, so a green run
  always means BPF was exercised.
- `SDA_IT_REQUIRE_RELOAD=1` makes reload coverage mandatory. The reload subtest first proves that
  `daemon-reload` wipes the device program on this host; without the variable, a host where that
  cannot be proven skips only that subtest.

Pull requests from another repository (a fork) are the exception: GitHub gives them no secrets, so
they cannot log in to `dhi.io` to build the image every test runs against. For them the job is
named `integration (fork: not run)`, runs nothing and says so with a warning; the push to master
that merges the pull request runs the suite in full. The gate is where the pull request comes from,
not whether the secret is set: on a same-repository pull request or a push, a missing `DHI_TOKEN`
fails the job. A push to a branch of a contributor's fork therefore fails that check, since the
fork has no `DHI_TOKEN`. The release workflow runs the same enforced suite.

Prerequisites for the enforcement tests:

- cgroup v2;
- systemd as init, with its DBus socket at `/run/dbus/system_bus_socket`;
- passwordless `sudo` for `sudo -n systemctl daemon-reload` (reload subtest) and for the gate's
  `sudo -n <binary> --dump-device-programs <pid>`;
- the device node `/dev/loop-control`, or `/dev/loop0` as a fallback;
- a Docker login to `dhi.io`, whose base images the daemon image is built from.

**Safety.** The enforcement tests attach BPF programs to their own test containers and run
`systemctl daemon-reload` on the host. Only run them on a throwaway or trusted host, such as a CI
runner or a disposable VM.

Every container the suite creates carries the `swarm-device-access-it.envid` label. Each run
prints a one-line cleanup command for its own containers; after a killed run,
`make sweep-test-leaks` removes the leftovers of every run.

The native privileged checklist below is now partly automated. The enforcement tests cover step
3 (an opted-in container with a real `/dev/...` bind mount gets its rule), check the outcome of
step 4 by opening the device rather than inspecting the program with `bpftool`, and cover step 7
(the rule is re-applied after `systemctl daemon-reload`). Run the checklist by hand for the
remaining steps, and for these on hosts the tests do not cover.

## How the suite runs

- **Identity check.** Before any test, the runner runs the binary and the image with `-help`. The
  1.x Go daemon's `flag` package prints its usage and exits 0; the Rust daemon refuses a
  single-dash long flag with exit 2 and
  `error: single-dash long flags are not supported: '-help'; use '--help'`. The binary must also
  be a static ELF. Any other answer, an unset variable or an unreachable Docker daemon fails the
  run; none of them skips it. `TestIdentity_RefusesGoImage` is the check's negative control: it
  requires the check to refuse the pinned 1.x image (`SDA_GO_TEST_IMAGE`), after proving that
  image answers `-help` as the Go daemon.
- **Tests.** The dry-run, enforcement and launcher tests keep the names of the 1.x Go tests they
  were ported from. The binary runs as a host process with `--dry-run` and the double-dash flags;
  the enforcement and launcher tests run the image.
- **Upgrade and rollback gate.** `TestUpgrade_GoToRustAndBack` runs both daemons on one labelled
  container whose cgroup survives every step: the Go daemon grants the device, the Rust daemon
  with `--device-deny` for it revokes the Go-attached wrapper, the Rust daemon with the original
  policy grants it again, and the Go daemon with the narrower policy revokes the Rust-attached
  wrapper. After each step it probes the device from inside the container and reads the BPF
  programs attached to the container's cgroup: after a grant exactly one owned wrapper per runtime
  original and no stale wrapper, after a revocation no owned wrapper and each runtime original
  attached bare exactly once. The programs are read as root by the suite's own binary
  (`sudo -n <binary> --dump-device-programs <pid>`) and classified with the daemon's own code. The
  Go daemon is `SDA_GO_TEST_IMAGE`, the last published 1.x image pinned by digest in
  `.mk/integration-test.mk`, which `make test-integration` pulls (`make go-test-image`); it must
  answer `-help` as the Go daemon (exit 0 and its usage), the identity check inverted, so neither
  build can stand in for the other.
- **Run filter.** `RUN` goes to the runner's `--run` with Go's `-run` semantics (a regular
  expression per `/`-separated level), and a pattern that selects no test fails the run, where Go
  would pass with "no tests to run".
- **Deadline and cleanup.** `SDA_IT_SUITE_TIMEOUT` (default 4m) bounds the run, and SIGINT or
  SIGTERM ends it the same way, so the cleanups still run. Everything the suite creates carries the
  `swarm-device-access-it.envid` label plus its owner, start time and PID namespace; a run removes
  the leftovers of earlier runs only once they are older than an hour (`SDA_IT_STALE_AFTER`) and
  their owner process is gone from this host. `make sweep-test-leaks` removes the leftovers of
  every run, whatever their age.

## Dry-Run Coverage

The dry-run tests cover:

- Docker event subscription for `start` events.
- Startup enumeration against currently running containers.
- Container inspection and `/proc/<pid>` cgroup parsing.
- Host cgroup path resolution for private Docker cgroup namespaces.
- `/dev/...` bind mount detection.
- Dry-run device rule generation for `/dev/null`.
- `--policy-mode=opt-in` filtering: containers without `swarm-device-access.enable=true` are skipped.
- Per-container `swarm-device-access.enable` label opt-in.

## Rust `bpf(2)` test

`tests/bpf_privileged.rs` loads each runtime fixture under `tests/testdata/devfilter`, and the
daemon's wrapper around it, into the running kernel through the Rust `bpf(2)` module, and compares
the kernel's dumps with the committed `.reloaded` and `.wrapped` files, which the Go
`TestCaptureFixtures` recorded. It attaches nothing, but it needs `CAP_BPF` and `CAP_SYS_ADMIN`,
so it is built only with the `privileged-tests` feature and no gate runs it. Run it by hand, on a
throwaway host: build it as your user, then run the binary cargo prints as root.

```bash
cargo test --features privileged-tests --test bpf_privileged --no-run
sudo target/debug/deps/bpf_privileged-<hash>
```

## Native Privileged Checklist

Use this checklist manually on a trusted Linux host when changing cgroup, BPF,
or deployment behavior:

1. Build and run the daemon container with host privileges:

   ```bash
   make docker-build
   make docker-run DOCKER_RUN_ARGS="--log-level debug"
   ```

2. Verify the container has the required runtime wiring:

   ```bash
   docker inspect swarm-device-access
   ```

3. Start a consumer container with `--label swarm-device-access.enable=true` and
   a real `/dev/...` bind mount and confirm the daemon logs `device mount detected`
   and `setting device rule`. (For Swarm stacks, the equivalent placement is the
   service's top-level `labels:`, which Docker copies into every task container;
   `docker service create --container-label` writes to the same location. Do not
   use `deploy.labels:` or `docker service create --label`: those are service
   metadata, which the daemon ignores; see the README.)

4. If the host uses cgroup v2, confirm a `BPF_CGROUP_DEVICE` program is attached
   to the consumer cgroup with `bpftool`.

5. Test directory walking with a directory such as `/dev/bus/usb` when such
   devices are present.

6. Test `--device-allow` and `--device-deny` with a dry-run daemon first, then with
   the privileged daemon.

7. If `/run/dbus/system_bus_socket` is mounted and systemd is available, run
   `systemctl daemon-reload` and verify the daemon logs that it re-applied rules.

8. On a Swarm node, `kill -9` the launcher task's container and confirm that Swarm's replacement
   task removes the orphaned daemon and that exactly one `swarm-device-access` daemon container is
   running afterwards, labelled with the new launcher's ID
   (`docker ps --filter 'name=^swarm-device-access$' --format '{{.ID}} {{.Label "io.github.leinardi.swarm-device-access.launcher"}}'`;
   without the anchors the filter also matches the launcher task's container).
   `TestLauncher_RunsDaemonFromItsOwnImage` covers the same case outside Swarm.

9. Verify observability by starting with `--metrics-addr 127.0.0.1:9090`, then checking
   `/healthz`, `/readyz` and `/metrics`.

## Field test

[`scripts/field-test.sh`](../scripts/field-test.sh) hands one Swarm node from the deployed 1.x
daemon to the Rust daemon and back, with real device access checked from inside Swarm tasks at
every step. The deployed Go service is only kept
off the node with a placement constraint on its node ID, so other nodes keep running it; both
daemons under test run as services of their own, pinned to the node, in launch mode with the
deployed service's mounts and arguments. A cleanup trap removes every service the script created
and lifts the constraint, whichever step failed.

Everything is staged under `/dev/shm`, so nothing is left on the node's disk. The root steps are
run by the operator over ssh with `sudo`, and the run itself goes through `systemd-run`, so an ssh
drop does not stop it halfway:

```bash
make docker-build IMAGE_TAG=field
docker save swarm-device-access:field -o /dev/shm/sda-field.tar
ssh castor-dev mkdir -p /dev/shm/sda-field
scp /dev/shm/sda-field.tar scripts/field-test.sh castor-dev:/dev/shm/sda-field/
ssh -t castor-dev sudo docker load -i /dev/shm/sda-field/sda-field.tar
ssh -t castor-dev sudo systemd-run --unit=sda-field-test \
  --setenv=SDA_FIELD_RUST_IMAGE=swarm-device-access:field \
  --setenv=SDA_FIELD_MOUNT=/dev/dri /dev/shm/sda-field/field-test.sh
ssh -t castor-dev sudo journalctl -u sda-field-test -f
ssh castor-dev rm -r /dev/shm/sda-field
```

`SDA_FIELD_MOUNT` is the `/dev` directory the test tasks bind-mount (`/dev/dri`, or a USB bus such
as `/dev/bus/usb/001`), and `SDA_FIELD_DEVICE` the device under it they open (by default the first
`renderD*` node or bus device). The script checks, each from inside a task:

1. The deployed Go daemon grants a new task after start.
2. With Go stopped on the node, the Rust daemon starts with the same policy and the Go-granted task
   keeps working.
3. Rust grants a new task after start, and reconciles it (a `container processed` line for its ID
   after the event) and keeps it granted after `docker unpause` and after `systemctl
   daemon-reload`, whose pass the reload watcher logs (`systemd reload completed; re-applying
   device rules`).
4. Rust cleans up after die and destroy, when the task's service is removed: a `device grants
   revoked` line for its ID while its cgroup still opens, or `container cgroup gone; released`
   once the cgroup is gone, and no `could not clean up after container`.
5. Rust started with `--device-deny` for the device revokes the Go-granted task, and with the
   original policy grants it again.
6. The 1.x daemon keeps the Rust-granted task with the same policy, revokes it when started with
   `-device-deny`, and the deployed Go service, restored on the node, grants it again.

It logs the daemon's resident set (`VmRSS`, and the peak `VmHWM`) at each daemon start and at the
end, keeps each daemon's log beside the script, prints `PASS` or `FAIL` per check, and exits
non-zero when any check failed. A denial counts only when the open fails with the device cgroup's
`Operation not permitted`, and a check fails if the task's container changed during it. The Rust
daemons under test log at debug (the flag is appended last, so it overrides the deployed level),
because the revocation line is a debug one.

### Results

Run on castor-dev on 2026-10-27: TrueNAS, kernel 6.12.105, x86_64, Docker 28.3.1, cgroup v2, a
swarm whose `swarm-device-access_daemon` global service runs the 1.x image
`ghcr.io/leinardi/swarm-device-access:1@sha256:f49322681563406d98bb1137634cd91e60cb716828218f17835e58173888d7d0`
with `launch -dbus -- -log-level info -log-format text` and `-device-allow` for `/dev/sd*`,
`/dev/sg*`, `/dev/nvme*` and `/dev/dri/*`. The tasks bind-mounted `/dev/dri` and opened
`/dev/dri/renderD128`; the Rust image was built from commit `1705c20`. All 16 checks passed:

| Step | Check | Result |
| --- | --- | --- |
| 1 | the deployed Go daemon grants a new task | PASS |
| 2 | the Go-granted task keeps working under Rust | PASS |
| 3 | Rust grants a new task after start | PASS |
| 3 | Rust reconciles the task on unpause, and it keeps working | PASS, PASS |
| 3 | the reload watcher logs the `systemctl daemon-reload` | PASS |
| 3 | Rust reconciles the task after the reload, and it keeps working | PASS, PASS |
| 4 | Rust revokes or releases the task after die and destroy | PASS |
| 5 | Rust with `--device-deny` revokes the Go grant | PASS |
| 5 | Rust with the original policy grants it again | PASS |
| 6 | the 1.x daemon keeps the Rust grant with the same policy | PASS |
| 6 | the 1.x daemon with `-device-deny` revokes it | PASS |
| 6 | the deployed Go service, back on the node, grants it again | PASS |

The daemon's resident set, from `/proc/<pid>/status`:

| Daemon | When | `VmRSS` | `VmHWM` |
| --- | --- | --- | --- |
| Go 1.x (deployed) | before the hand-over | 14884 kB | 14884 kB |
| Rust | after startup | 3712 kB | 3712 kB |
| Rust | after the start, unpause, reload and removal of a task | 3956 kB | 3956 kB |
| Rust | at the end, after a restart with the original policy | 3576 kB | 3576 kB |
| Go 1.x (restored) | at the end | 15448 kB | 15448 kB |

An earlier run reported one false failure: its watcher check piped the daemon's log into `grep
-q` under `pipefail`, and the `SIGPIPE` the early exit gives the log reader failed the pipeline
although the daemon's log carried `systemd reload watcher started`. The checks now read the whole
log, and the reload is proven by the watcher's line after the reload instead. That run measured
the same resident sets (Rust 3688 to 3772 kB, Go 14852 to 15116 kB).
