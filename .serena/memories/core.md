# Core — swarm-device-access

Linux-only daemon that sets cgroup device-allow rules for Docker Swarm containers
that bind-mount `/dev/...` paths. Solves the Docker Swarm gap where `devices:` /
`device_cgroup_rules:` are silently ignored.

## Source map

```
cmd/swarm-device-access/   # main binary (//go:build linux)
  main.go                  # run(): flags, settings, openat2 probe, store, processor, daemon.Run
  flags.go                 # CLI flag definitions
  config.go                # settings merge/validate, SIGHUP reloader
  launch.go                # `launch` subcommand: launcher flags, runLaunch
  version.go               # version/commit/date (filled by ldflags)

internal/
  config/                  # strict YAML loader; runtime Store/Publisher with generations
  policy/                  # mode, label parsing, globs: Enabled, Denied, Authorized (cross-platform)
  launcher/                # launch mode: daemonSpec (privileged set), self ID, stale cleanup, supervise
  daemon/                  # event loop and coordinator
    daemon.go              # Run: subscribe, coordinator, startup pass, reload watcher, listenEvents
    events.go              # listenEvents/consumeEvents (reconnect backoff), processOne
    coordinator.go         # passes, retries, reservations, processed dedup (processedTTL), sweep
  processor/               # one idempotent Reconcile path
    processor.go           # Reconcile, computeDesired, collectContainerRules
    rules.go               # CollectMountRules, evaluateCandidate, aggregation per device
    devfs.go               # devFS seam: openat2 beneath /dev, fstat, sysfs uevent; ProbeOpenat2
    reconcile.go           # applyPinned, Terminate, Sweep, revokes
    pin.go                 # pidfd pinning, /proc/<pid>/cgroup
    lifecycle.go           # per-container run history
    publish.go             # PublishAndReconcile
  cgroup/                  # device rules (parts NVIDIA-derived, Apache 2.0)
    api.go                 # Interface: GetDeviceCGroupMountPath/RootPath, SetDeviceRules(handle, rules); New
    handle.go              # CgroupHandle (directory fd + identity)
    v1.go, v1set.go, ledger.go   # cgroup v1: devices.allow/devices.deny, owned-exception ledger
    v2.go, v2plan.go, v2ops.go, owned.go, ebpf.go  # cgroup v2: owned wrapper around the runtime's BPF filter
  observability/           # Prometheus Recorder (nil-safe), health/ready, pprof
  logger/                  # slog wrapper (text/json/plain handlers, L() lazy-init)
  systemd/                 # DBus Reloading signal watcher

test/integration/          # daemon integration tests (build tag integration)
deployments/docker/        # Dockerfile, compose file running the launcher, example config
docs/                      # architecture.md, testing.md, release.md
```

## Key invariants

- `//go:build linux` on all files touching devices/cgroups/unix.*; cross-platform helpers (logger, policy) omit it.
- `internal/cgroup/` retains NVIDIA's Apache 2.0 copyright header — do not relicense or reformat.
- `hostRootPath = "/host"` is the in-container view of host root; cgroup paths join against it.
- Version strings live in `version.go`, set via `-ldflags -X main.version=...`.
- Every trigger runs `Processor.Reconcile`; anything uncertain applies the empty set (fail closed).
- Container dedup: the coordinator's `processed map[string]time.Time` bridges enumeration and the event stream.

See `mem:tech_stack`, `mem:conventions`, `mem:suggested_commands`, `mem:task_completion`.
