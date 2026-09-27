# Device filter fixtures

Real `BPF_CGROUP_DEVICE` programs, as the daemon reads them back from the
kernel (translated dump, canonical bytes, one 8-byte instruction per line in
hex). `owned_test.go` uses them to prove that the programs container runtimes
attach pass the reloadability gate and survive a wrap/strip round trip.

| Fixture | Attached by |
| --- | --- |
| `systemd` | systemd 259 (`sd_devices`) for a Docker 29 container (systemd cgroup driver) |
| `runc` | runc 1.5.1 for the same container |
| `crun` | crun 1.28 under podman 5.8 (cgroupfs manager) |

Captured on Linux 7.0 (x86_64). For each fixture:

- `<name>.orig` is the program as dumped from the container's cgroup;
- `<name>.reloaded` is the dump after loading `.orig` back unchanged, which
  must equal `.orig` for the gate's premise to hold;
- `<name>.wrapped` is the dump after loading the daemon's wrapper around
  `.orig` for `fixtureRules` with `fixtureNonce` (`fixtures_test.go`).

## Capturing on another kernel or runtime

The capture only loads and dumps programs; it never attaches or detaches
anything. It needs a privileged container on the host that runs the target
container.

```bash
CGO_ENABLED=0 go test -c -tags fixturecapture -o /tmp/sda-capture ./internal/cgroup
docker run --rm --privileged --pid=host --cgroupns=host \
  -v /sys/fs/cgroup:/sys/fs/cgroup:ro -v /tmp/sda-capture:/capture:ro -v "$PWD/out:/out" \
  -e SDA_CAPTURE_CGROUP=/sys/fs/cgroup/<path of the target container's cgroup> \
  -e SDA_CAPTURE_OUT=/out -e SDA_CAPTURE_NAME=<name> \
  alpine:3.22 /capture -test.run TestCaptureFixtures -test.v
```

One set of files is written per attached program (`<name>-<index>.*`). Rename
them, add them to the fixture list in `owned_test.go`, and note the versions
here.
