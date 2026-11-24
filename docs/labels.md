# Container labels

Consumer services opt in and narrow their allowed device set with labels:

| Label | Values | Description |
| --- | --- | --- |
| `swarm-device-access.enable` | `true` / `false` | Opt in (`true`) or explicitly opt out (`false`) of processing. |
| `swarm-device-access.device-allow` | Comma-separated globs | Allow only matching `/dev/...` paths. Empty means inherit. |
| `swarm-device-access.device-deny` | Comma-separated globs | Deny matching `/dev/...` paths. Deny overrides allow. |

Write deny globs against kernel device names (`/dev/sd*`, `/dev/nvme*`), not
against aliases such as `/dev/disk/by-id/*`: see [Security](security.md).

Declare these labels under top-level `labels:` in your service definition.
Docker copies top-level labels into each task container, so the daemon can read
them on every node — including worker nodes.

Policy comes from the daemon's own configuration and the container's labels
only, so every node makes the same decision for the same container, before and
after a daemon restart. Labels under `deploy.labels:` (Swarm service metadata)
are **ignored** on every node.

> **Migrating from earlier versions:** older releases also read
> `swarm-device-access.*` labels from `deploy.labels:` on manager nodes and
> warned about them there. That support is removed: service-level labels are
> never read, and the daemon no longer queries the node's Swarm role or
> inspects the parent service. A service whose only opt-in (or narrowing
> `device-allow`/`device-deny`) sits under `deploy.labels:` is now processed as
> if those labels were absent, which in the default `opt-in` mode means it is
> skipped. Move the labels to the service's top-level `labels:` so they reach
> the container.

If you bind-mount a directory (for example `source: /dev/dri`), the
`device-allow` glob is evaluated **per child node** inside that directory —
write the glob against the children, e.g. `/dev/dri/*` or `/dev/dri/renderD128`.

Globs are case-sensitive, and `*`, `?` and classes never match a `/` (the
spellings Go 1.x read differently are refused, see
[Q17](migration.md)):
`/dev/bus/usb/*` matches the bus directories `/dev/bus/usb/001`, not the device
node `/dev/bus/usb/001/002`. Write one `*` per level, e.g. `/dev/bus/usb/*/*`.
Every glob, in flags, the config file and labels, must be an absolute, clean
path under `/dev/` (no relative pattern, `..`, `//` or trailing `/`); anything
else is rejected at startup or reload, and in a label it makes the container
get no devices. Such a glob could never match a device path, so a deny written
that way would silently deny nothing.

A device can be reached under several names: the mounted name (an alias such
as `/dev/disk/by-id/usb-...`), the node it resolves to, and its kernel name
(`DEVNAME` in sysfs, e.g. `/dev/sda`). Deny globs are checked against all three,
and a match on any name of a device, in any of the container's mounts, denies
it. Allow globs must match the kernel name and the resolved node; an alias
that no allow glob names does not deny anything. A node planted elsewhere
under `/dev` is therefore judged as the device it really is. A name that
leads outside `/dev` is skipped (logged as a warning only when an allow glob
names it, since every whole-`/dev` mount has such links, like `/dev/stdin`); a
device whose identity cannot be established (no or ambiguous `DEVNAME` in
sysfs, an unreadable node) leaves
the container with no grants until a retry succeeds, unless policy already
excludes it under its other names. The same holds when a directory mount
cannot be enumerated completely: a read error, or more than 4096 entries
(`mount too large; narrow the bind mount`). A whole-`/dev` mount counts
everything under it, including files programs keep in `/dev/shm`, so on a busy
host it can hit that limit: mount the device directories you need instead.

Some pseudo devices have no `DEVNAME`; `/dev/pts/*` nodes, for example, have
no `/sys/dev/char/136:0` entry. To see why a device blocks a container, look
up its numbers (`ls -l /dev/pts/0` shows `136, 0`) and check
`ls /sys/dev/char/136:0` (`/sys/dev/block/M:m` for block devices). Blocking
such a device is intentional: bind-mount only the devices you need, or
exclude the rest with an allow list.

Global `--device-allow` and `--device-deny` define the broadest access the daemon
may grant. Per-container labels can only narrow that access. Deny rules always
win.
