# Alternatives

Swarm has had no `devices:` option since swarm mode shipped; the request is
[moby/swarmkit#1244](https://github.com/moby/swarmkit/issues/1244), and every
upstream patch so far was closed without being merged. These are the usual
workarounds and how they compare:

| Approach | How it works | Trade-off |
| --- | --- | --- |
| `generic_resources` with the NVIDIA runtime | Each node advertises its GPUs in `daemon.json`; the service reserves them under `deploy.resources.reservations.generic_resources` and the NVIDIA runtime injects the devices | The native option, and Swarm schedules on GPU count. NVIDIA GPUs only. |
| A service that runs `docker run --device ...` through the Docker socket | The service is only a wrapper; the device container is a plain container on the node | Works for any device and any `docker run` option, but the device container is not a Swarm task: no rolling updates, rollbacks, secrets or configs, and it keeps running when the stack is removed. |
| A script on `docker events` writing `devices.allow` | Adds a cgroup v1 device rule when a container starts | Only works on cgroup v1, which current distributions no longer use by default. |
| [`allfro/device-mapping-manager`](https://github.com/allfro/device-mapping-manager) | A privileged container that grants every device bind-mounted under `/dev` | The project this one forked. No commits since January 2024, its issues report it failing on Docker Engine 29, and it grants every container that mounts a device, with no opt-in. |
| This daemon | Grants the devices a task bind-mounts under `/dev`, on cgroup v1 and v2 | Any device and an unchanged Swarm task, gated by opt-in labels and allow and deny globs. It does not schedule: see [Placing Tasks on Nodes With the Device](tips.md#placing-tasks-on-nodes-with-the-device). |

For NVIDIA GPUs, prefer `generic_resources` when the NVIDIA runtime is installed
on the nodes. Use this daemon for every other device, and for GPUs whose
device nodes you would rather bind-mount yourself.
