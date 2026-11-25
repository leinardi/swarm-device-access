# Tips and limitations

## Placing tasks on nodes with the device

The daemon grants access on whichever node a task lands on; it does not decide
where tasks run. Label the nodes that have the device and constrain the service
to them, or Swarm may start the task on a node where the bind mount fails:

```bash
docker node update --label-add device.zigbee=true node-1
```

```yaml
    deploy:
      placement:
        constraints:
          - node.labels.device.zigbee == true
```

For NVIDIA GPUs, `generic_resources` also lets Swarm schedule by GPU count (see
[Alternatives](alternatives.md)).

## Stable names for USB and serial devices

A USB adapter can come up as `/dev/ttyUSB0` on one boot and `/dev/ttyUSB1` on
the next. Mount a stable alias instead: the links udev already creates under
`/dev/serial/by-id/`, or a name of your own from a udev rule:

```text
# /etc/udev/rules.d/99-zigbee.rules
SUBSYSTEM=="tty", ATTRS{idVendor}=="10c4", ATTRS{idProduct}=="ea60", SYMLINK+="zigbee"
```

```yaml
    volumes:
      - /dev/zigbee:/dev/zigbee
    labels:
      swarm-device-access.enable: "true"
      swarm-device-access.device-allow: "/dev/ttyUSB*"
```

Write `device-allow` against the kernel name the alias points to
(`/dev/ttyUSB*` here), not the alias: an allow glob that names only the alias
grants nothing (see [Container Labels](labels.md)).

## Devices that appear after the container starts

The daemon walks a container's device mounts when it starts and on each later
pass (a `SIGHUP`, a daemon restart, a `systemctl daemon-reload`); it does not
watch for devices plugged in between. Tracked in
[#41](https://github.com/leinardi/swarm-device-access/issues/41). Until then:

- A mount of a single node or alias (`/dev/zigbee`) is fixed when the container
  starts: Docker resolves it then. If the device is unplugged and plugged back,
  restart the task.
- A directory mount (`/dev/bus/usb`) shows new nodes inside the container, but
  they are not granted until the next pass. Restart the task, or trigger a pass
  for every container on that node with
  `docker kill -s HUP swarm-device-access`; it works without a config file.

## FUSE

Granting `/dev/fuse` lets a container open it, but mounting a FUSE filesystem
also needs `CAP_SYS_ADMIN` (`cap_add`) and an AppArmor profile that allows
`mount`. Docker's default profile denies it, and `docker stack deploy` ignores
`security_opt`, so on AppArmor hosts the device grant alone is not enough.
