# Field test

> **Status:** Historical: the one-time Go-to-Rust hand-over test, run on 2026-10-27 before the 2.0
> cutover, split from docs/testing.md in November 2026. The current test levels are in
> docs/testing.md.

[`scripts/field-test.sh`](../../../scripts/field-test.sh) hands one Swarm node from the deployed 1.x
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
ssh <node> mkdir -p /dev/shm/sda-field
scp /dev/shm/sda-field.tar scripts/field-test.sh <node>:/dev/shm/sda-field/
ssh -t <node> sudo docker load -i /dev/shm/sda-field/sda-field.tar
ssh -t <node> sudo systemd-run --unit=sda-field-test \
  --setenv=SDA_FIELD_RUST_IMAGE=swarm-device-access:field \
  --setenv=SDA_FIELD_MOUNT=/dev/dri /dev/shm/sda-field/field-test.sh
ssh -t <node> sudo journalctl -u sda-field-test -f
ssh <node> rm -r /dev/shm/sda-field
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

## Results

Run on a test node on 2026-10-27: TrueNAS, kernel 6.12.105, x86_64, Docker 28.3.1, cgroup v2, a
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
