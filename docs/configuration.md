# Configuration

You can configure the daemon with CLI flags, a YAML config file, or both. When a
value is set in both places, the CLI flag wins.

| Flag | Default | Description |
| --- | --- | --- |
| `--log-level` | `info` | `debug`, `info`, `warn`, `error` |
| `--log-format` | `text` | `text`, `json` |
| `--log-time` | `false` | Include timestamps in log lines (RFC 3339, UTC). Fixed at startup: a reload that changes it is logged and ignored |
| `--docker-socket` | `/var/run/docker.sock` | Path to the Docker daemon's UNIX socket |
| `--dry-run` | `false` | Log the device set each container would get, without reading its `/proc` entry or its cgroup and without writing anything. Not a cleanup path: grants left by a previous live run stay, and a reload can turn it off but not on (see [Config File](#config-file)) |
| `--policy-mode` | `opt-in` | `opt-in`: only `enable=true` containers. `all`: unless `enable=false`. |
| `--device-allow` | `""` | Glob for `/dev/...` paths to allow, one glob per flag; repeat the flag for more (not comma-split, unlike the labels). Empty means allow all. |
| `--device-deny` | `""` | Glob for `/dev/...` paths to deny, one glob per flag; repeat the flag for more (not comma-split, unlike the labels). Deny takes priority over allow. |
| `--metrics-addr` | `""` | `host:port` for Prometheus `/metrics`, `/healthz`, `/readyz`. Empty disables it. A busy or invalid address stops startup. |
| `--debug-addr` | `""` | Removed in 2.0 with the pprof debug server: any value but the empty one is refused at startup and on reload. |
| `--config` | `""` | Path to a YAML config file. CLI flags override file values. Reload with SIGHUP. |
| `-h`, `--help` |  | Print this flag list and exit |

`--log-level`, `--log-format` and `--policy-mode` (and their config file keys)
accept only the values listed; anything else is an error at startup and makes a
reload keep the previous config. The undocumented `--log-level` aliases
`warning`, `fatal` and `panic`, which earlier versions mapped to `warn` and
`error`, are no longer accepted.

## Config file

All CLI flags can be represented in YAML and loaded with `--config`. See
[`deployments/docker/config.yaml`](https://github.com/leinardi/swarm-device-access/blob/master/deployments/docker/config.yaml)
for an example.

The file is checked strictly: an unknown key (for example `device_deny` for `device-deny`), a second
YAML document, a YAML merge key (`<<`) or a value of the wrong type is an error. `dry-run` and
`log-time` must be YAML booleans (`true`, `True`, `TRUE`, `false`, `False`, `FALSE`; not `"yes"`);
`policy-mode`, `log-format`, `log-level` and `docker-socket` must be non-empty strings;
`metrics-addr` must be a string, and an empty one disables the listener; `debug-addr` must be empty;
`device-allow` and `device-deny` must be lists of non-empty strings. An explicit `null` (also
through an alias) is an error, never a silent "not set": leave a key out to keep the flag's value.

Send `SIGHUP` to reload the config file without restarting the daemon:

```bash
docker kill -s HUP swarm-device-access
```

Give the launcher `--config-dir` with the host directory that holds the file
(it binds it read-only at `/etc/swarm-device-access`) and pass `--config` to
the daemon after `--`; the compose file in
[Getting started](getting-started.md#docker-compose-for-swarm) shows both as
commented lines. Mount the directory rather than the file: editors and tools that write a
new file and rename it over the old one would leave a single-file mount on the
old content, and the reload would silently apply that.

Some settings are only read at startup and still require a restart:
`docker-socket` and `metrics-addr`. A reload that changes one of them logs
that it was ignored and applies the rest.

A reload computes every setting afresh from the command line, the file and the
defaults, in that order: a key removed from the file falls back to the flag
given on the command line, or to the default, not to what the file said
before. A reload that fails for any reason (unreadable or invalid file, invalid
value or policy) changes nothing, logging included, and the previous config
stays in force.

`dry-run` can be turned off by a reload, and the daemon then applies the
current policy for real. It cannot be turned on: a reload that would enable it
is rejected with `dry-run cannot be enabled on a running daemon; to stop
enforcing, restart the daemon live with a narrower policy, or restart the
affected containers`, and the previous config is kept, because dry-run would
stop revoking while every grant stayed in place.

A reload, a daemon start and a `systemctl daemon-reload` each reconcile every
running container under the current config, so narrowing the policy revokes
grants the new policy no longer allows. Until a pass has reached every running
container, `sda_reload_incomplete` is 1; containers whose reconcile failed are
retried with backoff and counted in `sda_reconcile_pending_containers`. The
daemon warns while either is non-zero and logs `config reload complete` once
both are zero for the latest config.

Restarting the daemon **live** is also a cleanup: the startup pass replaces or
strips the grants a previous instance left on running containers (on cgroup v1
only within the limits described under [Host Requirements](host-requirements.md)).
Restarting into **dry-run** is not: dry-run never looks at cgroups, so grants a
previous live run left stay in place until a live start or a container
restart. The daemon logs `dry-run: grants left by a previous live run cannot be
detected or cleaned in dry-run` at every dry-run start.
