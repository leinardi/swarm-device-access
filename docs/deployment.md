# Deployment

`swarm-device-access launch [launch flags] -- [daemon flags]` finds its own
container, reads the ID of the image it runs, and creates one container named
`swarm-device-access` from that image ID, with the arguments after `--` as the
daemon's flags. The daemon container gets exactly the documented privileged
set: `--privileged`, the host cgroup, PID and user namespaces, no network, and
bind mounts of the host Docker socket at `/var/run/docker.sock`, `/sys` at
`/host/sys` and `/dev` at `/dev`, plus the optional mounts below. The launcher
streams the daemon's output as its own, so `docker service logs` shows the
daemon, and exits with the daemon's status.

| Launch flag | Default | Description |
| --- | --- | --- |
| `--dbus` | `false` | Bind the host's `/run/dbus/system_bus_socket` at `/var/run/dbus/system_bus_socket` in the daemon, so it reapplies rules after `systemctl daemon-reload`. |
| `--config-dir` | `""` | Host directory bound read-only at `/etc/swarm-device-access`, for the daemon's `--config`. Must be an absolute, clean path. |
| `--host-network` | `false` | Run the daemon in the host network instead of none; only `--metrics-addr` needs it. |
| `--host-docker-socket` | `/var/run/docker.sock` | Host path of the Docker socket bound into the daemon. Must be an absolute, clean path. The launcher itself always uses its own `/var/run/docker.sock` mount. |
| `--log-level` | `info` | The launcher's own log level: `debug`, `info`, `warn`, `error`. |
| `--log-format` | `text` | The launcher's own log format: `text`, `json`. |
| `-h`, `--help` |  | Print the launch flags and exit |

Running the daemon from the launcher's own image ID, not a tag, means the
service image is the daemon image:

- An image watcher such as gantry, or `docker service update --image`, updates
  the service, and the new launcher starts the new daemon. Every node runs the
  same digest.
- A Swarm rollback (`docker service rollback`) rolls the daemon back with it.
- `update_config.order: stop-first` stops the old launcher, which stops and
  removes its daemon, before the new one starts. The launcher's shutdown (a 10s
  stop, the wait for the removal and the drain of the last log lines) shares a
  single 20s budget, inside `stop_grace_period: 30s`.

Each daemon container is labelled with the ID of the launcher that owns it.
Before it creates a daemon, a launcher removes a `swarm-device-access`
container only when it is a daemon whose launcher is confirmed gone or
stopped, or a daemon left by the `sh` wrapper of earlier releases. A daemon
whose launcher still runs, a launcher whose state cannot be read, or any other
container with that name makes the launcher exit with an error, and Swarm
retries it.

Releases before 1.0.0 documented a `docker:29` service that ran an `sh` wrapper
around `docker run`. It still works, but it is no longer documented: the
service image never changed, so image watchers and rollbacks did not follow
daemon releases. To migrate, replace the service with the one above; the first
launcher removes the daemon container the wrapper left running.
