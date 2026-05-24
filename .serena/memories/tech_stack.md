# Tech Stack

- **Language**: Go 1.26.3 (module `github.com/leinardi/swarm-device-access`)
- **Build**: Make wrapping `go`; shared `.mk/` snippets from `leinardi/make-common@v1`
- **Key deps**:
    - `github.com/cilium/ebpf v0.19.0` — BPF program attach (cgroup v2)
    - `github.com/docker/docker v28.5.2` — Docker API client
    - `github.com/godbus/dbus/v5 v5.1.0` — systemd DBus watcher
    - `github.com/prometheus/client_golang v1.23.2` — metrics
    - `golang.org/x/sys v0.42.0` — unix syscalls
    - `gopkg.in/yaml.v3 v3.0.1` — config file parsing
- **Linter**: golangci-lint v2, default all linters; enforced via pre-commit hooks
- **Pre-commit**: `make check` (all files) / `make check-stage` (staged only)
- **Container**: `deployments/docker/Dockerfile`; image name = `swarm-device-access`
- **Runtime**: must run with `privileged: true`, `cgroup: host`, `pid: host`, `userns_mode: host`;
  bind mounts: `/var/run/docker.sock`, `/sys → /host/sys`;
  `/run/dbus/system_bus_socket` optional (enables reload handling)
