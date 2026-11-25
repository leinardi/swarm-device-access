<h1 align="center">
  <img alt="swarm-device-access logo" src="docs/assets/icon.svg" width="96">
  <br>swarm-device-access
</h1>

<p align="center"><strong>Linux daemon that enables GPU, USB, and other /dev device passthrough
for Docker Swarm services by injecting cgroup device-allow rules for bind-mounted
devices.</strong></p>

<p align="center">
  <a href="https://github.com/leinardi/swarm-device-access/actions/workflows/ci.yaml"><img alt="CI"
    src="https://img.shields.io/github/actions/workflow/status/leinardi/swarm-device-access/ci.yaml?branch=master&style=flat-square"></a>
  <a href="https://github.com/leinardi/swarm-device-access/releases/latest"><img alt="Release"
    src="https://img.shields.io/github/v/release/leinardi/swarm-device-access?style=flat-square"></a>
  <a href="LICENSE"><img alt="License"
    src="https://img.shields.io/github/license/leinardi/swarm-device-access?style=flat-square"></a>
</p>

<p align="center">
  <a href="https://leinardi.github.io/swarm-device-access/">Documentation</a> ·
  <a href="#-installation">Install</a> ·
  <a href="CONTRIBUTING.md">Contributing</a>
</p>

Docker Swarm rejects `devices:` and `device_cgroup_rules:` in service compose files. A task can
still bind-mount `/dev/nvidia0` or `/dev/bus/usb`, but the kernel's cgroup device policy may block
the container from opening it: the file is visible, and access fails with `EACCES`. This daemon
watches Docker and adds the cgroup device rule each matching container needs.

It is a fork of [`allfro/device-mapping-manager`](https://github.com/allfro/device-mapping-manager).
Version 2.0 is a Rust rewrite of the 1.x Go daemon; the differences are listed in
[Migrating from 1.x][migration].

## ✨ Features

- **Device access Swarm leaves out:** each bind mount under `/dev` gets its device rule: a
  `BPF_CGROUP_DEVICE` program on cgroup v2, `devices.allow` on cgroup v1.
- **Opt-in, narrowed by labels:** only labelled containers by default; allow and deny globs judge
  each device by its real name, deny wins, and a label can only narrow the policy.
- **Rules that stay applied:** a startup pass over running containers, Docker events with
  reconnect, and, with `--dbus`, rules reapplied after `systemctl daemon-reload`.
- **One global Swarm service:** an unprivileged launcher creates the privileged daemon from its own
  image ID, so service updates and rollbacks move the daemon with it.

## 📦 Installation

```bash
git clone https://github.com/leinardi/swarm-device-access.git
cd swarm-device-access/deployments/docker
docker stack deploy -c docker-compose.yaml swarm-device-access
```

This deploys the image `ghcr.io/leinardi/swarm-device-access:2` as a global service on every node.
Hosts need Linux 5.6 or newer and, on cgroup v2, the runtime's device filter attached with
`BPF_F_ALLOW_MULTI` (runc 1.0+, crun and systemd do): see [Host requirements][host] and the
[Getting started][start] guide.

## 🚀 Usage

Give a service's containers the opt-in label and bind-mount the devices they need, with top-level
`labels:` (labels under `deploy.labels:` are ignored):

```yaml
services:
  app:
    image: alpine
    volumes:
      - /dev/ttyS0:/dev/ttyS0
    labels:
      swarm-device-access.enable: "true"
      swarm-device-access.device-allow: "/dev/ttyS0"
```

Every label, the global allow and deny globs and the daemon's flags are in the [documentation][docs].

## 📖 Documentation

- [Getting started][start]: the Swarm service, a consumer example and a passthrough check.
- [Deployment](docs/deployment.md), [Configuration](docs/configuration.md) and
  [Container labels](docs/labels.md): the launcher, the flags, the config file and the labels.
- [Architecture](docs/architecture.md), [Security model](docs/security.md) and
  [Migrating from 1.x][migration].
- [Contributing](CONTRIBUTING.md): building, testing and the commit conventions;
  [Releasing](RELEASING.md) for maintainers.
- Bug reports and questions: the [issue tracker](https://github.com/leinardi/swarm-device-access/issues).

## 🔒 Security

The daemon runs privileged, with the host Docker socket and `/dev`. Report vulnerabilities
privately, as [SECURITY.md](SECURITY.md) describes.

## 📝 License

[Apache License 2.0](LICENSE). The cgroup code in `src/cgroup/{api,ebpf,v1,v2}.rs` is ported from
[NVIDIA's libnvidia-container](https://github.com/NVIDIA/libnvidia-container) and keeps its Apache
2.0 copyright header.

[docs]: https://leinardi.github.io/swarm-device-access/
[start]: docs/getting-started.md
[host]: docs/host-requirements.md
[migration]: docs/migration.md
