---
title: Home
template: home.html
hide: [navigation, toc]
home:
  tagline: Linux daemon that enables GPU, USB, and other /dev device passthrough for Docker Swarm services by injecting cgroup device-allow rules for bind-mounted devices.
  logo: assets/icon.svg
  actions:
    - { text: Get started, link: getting-started/, primary: true }
    - { text: GitHub, link: https://github.com/leinardi/swarm-device-access }
---

<div class="rdt-install" markdown>

```bash
git clone https://github.com/leinardi/swarm-device-access.git
cd swarm-device-access/deployments/docker
docker stack deploy -c docker-compose.yaml swarm-device-access
```

</div>

## Features

<div class="grid cards" markdown>

- :lucide-usb:{ .lg }

    **Device access Swarm leaves out**

    Each bind mount under `/dev` gets its device rule: a `BPF_CGROUP_DEVICE` program on cgroup v2,
    `devices.allow` on cgroup v1.

- :lucide-tag:{ .lg }

    **Opt-in, narrowed by labels**

    Only labelled containers by default. Allow and deny globs judge each device by its real name,
    deny wins, and a label can only narrow the policy.

- :lucide-refresh-cw:{ .lg }

    **Rules that stay applied**

    A startup pass over running containers, Docker events with reconnect, and, with `--dbus`, rules
    reapplied after `systemctl daemon-reload`. A run's grant is revoked when it ends.

- :lucide-layers:{ .lg }

    **One global Swarm service**

    An unprivileged launcher creates the privileged daemon from its own image ID, so service updates
    and rollbacks move the daemon with it.

</div>
