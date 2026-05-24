# Core — swarm-device-access

Linux-only daemon that injects cgroup BPF device-allow rules into Docker Swarm containers
that bind-mount `/dev/...` paths. Solves the Docker Swarm gap where `devices:` /
`device_cgroup_rules:` are silently ignored.

## Source map

```
cmd/swarm-device-access/   # main binary (all files: //go:build linux)
  main.go                  # run() entrypoint, event loop
  flags.go                 # CLI flag definitions
  config.go                # YAML config load + SIGHUP reload
  containers.go            # processExistingContainers, processContainer
  devices.go               # device detection / rule application
  events.go                # listenEvents with exponential-backoff reconnect
  metrics.go               # Prometheus + health endpoints
  http.go                  # pprof debug endpoints
  systemd.go               # startReloadWatcher (DBus Reloading signal)
  version.go               # version/commit/date (filled by ldflags)

internal/
  cgroup/                  # BPF/cgroup device rules (NVIDIA-derived, Apache 2.0)
    api.go                 # Interface: GetDeviceCGroupMountPath/RootPath, AddDeviceRules
    v1.go                  # cgroup v1: writes to devices.allow
    v2.go + ebpf.go        # cgroup v2: BPF_CGROUP_DEVICE via cilium/ebpf, BPF_F_ALLOW_MULTI
  logger/                  # slog wrapper (text/json/plain handlers, L() lazy-init)
  policy/                  # Container/global policy, mode parsing, label parsing (cross-platform)
  systemd/                 # DBus Reloading signal watcher

test/integration/          # daemon integration tests (Linux only)
deployments/docker/        # Dockerfile
docs/                      # architecture.md with sequence diagram
```

## Key invariants

- `//go:build linux` on all files touching devices/cgroups/unix.*; cross-platform helpers (logger, policy) omit it.
- `internal/cgroup/` retains NVIDIA's Apache 2.0 copyright header — do not relicense or reformat.
- `hostRootPath = "/host"` is the in-container view of host root; cgroup paths join against it.
- Version strings live in `version.go`, set via `-ldflags -X main.version=...`.
- Container dedup: `processed map[string]struct{}` bridges startup enumeration ↔ live event stream.

See `mem:tech_stack`, `mem:conventions`, `mem:suggested_commands`, `mem:task_completion`.
