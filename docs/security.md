# Security model

This daemon is powerful by design. It runs privileged, mounts the host `/dev`
tree, and can talk to the Docker socket. Treat it as host-root equivalent: if it
is compromised or misconfigured, every container on that node may be affected.

The default policy mode is `opt-in` for that reason. Only containers with
`swarm-device-access.enable: "true"` are processed, so unrelated containers that
happen to bind-mount something under `/dev` are not silently granted access.

If every workload on the node is trusted, `--policy-mode=all` can be used to
process all containers unless they explicitly set
`swarm-device-access.enable: "false"`.

The security authority is a device's canonical identity: its device number,
named by its kernel name (`DEVNAME` in sysfs). Cgroup rules grant a
`major:minor`, not a path. A deny glob that matches only an alias (a
`/dev/disk/by-id/...` link, a node someone planted under `/dev/shm`) vetoes the
device while that alias is present when the daemon evaluates the container,
but a container that can change the directory it mounts can hide the alias
before a later pass. Treat alias-level deny as a best-effort veto, and write
the denies you rely on against kernel names, for example `/dev/sd*` and
`/dev/nvme*`. Allow globs are always checked against the kernel name and the
node a name resolves to, so an alias alone never grants a device.

See [SECURITY.md](https://github.com/leinardi/swarm-device-access/blob/master/SECURITY.md) for the
threat model and disclosure policy.
