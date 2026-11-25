# Host requirements

On every host the daemon needs Linux 5.6 or newer. It resolves every device
path beneath `/dev` with `openat2(2)` and `RESOLVE_BENEATH` (Linux 5.6), and
refuses to start with `unsupported kernel` when that is missing. It also pins
each running container's process with `pidfd_open(2)` (Linux 5.3) before
changing its cgroup (see below).

On cgroup v2 hosts the daemon adds its grants to the device filter the
container runtime already attached, replacing that program in place rather
than removing it first, so the container is never left unfiltered. That needs:

- The runtime's device filter attached with `BPF_F_ALLOW_MULTI`, as runc 1.0+,
  crun and systemd do. A filter attached without it (exclusive or
  `BPF_F_ALLOW_OVERRIDE`) cannot be replaced without a window in which the
  container has no filter at all, so the container is skipped: the daemon logs
  an error naming the attach mode and counts it under
  `sda_containers_skipped_total{reason="unsupported_attach_mode"}`.
- Every device filter attached to the container's cgroup readable by the
  daemon. If any of them is hidden from it (for example an SELinux policy that
  labels systemd's device filter so the daemon cannot open it), nothing is
  changed and the container is retried: an unreadable program cannot be
  checked or safely replaced. This is stricter than runc, which ignores such
  programs, and such policies are unsupported.
- A device filter attached at all, or one this daemon has seen before.
  `systemctl daemon-reload` can detach every device filter of a container; the
  daemon remembers (in memory) the runtime filters it has seen per cgroup and
  reattaches them, with its grants if the container has any. This puts the
  runtime's own restrictions back too, which the reload would otherwise have
  lifted. A daemon that did not see the container before the reload (for
  example one started after it) cannot rebuild the filter: an unprivileged
  container with no filter is retried with reason `filter_missing` and a
  warning to restart the container, and is never reported as done. Privileged
  containers have no device filter and are left alone.
- A device filter made only of the instructions device filters normally use
  (context loads, simple ALU, jumps inside the program, exit; no helper calls,
  maps or 64-bit immediates) on Linux 4.16 or newer. The daemon rebuilds the
  runtime's filter from the kernel's dump of it, and only this subset is known
  to load back unchanged; the filters runc, crun and systemd attach are all
  inside it. A filter outside it is left untouched and the container is
  retried with reason `program_not_wrappable`.

The daemon marks every program it attaches with a header naming it as its own
(`bpftool` shows it as `sda_devfilter`) and, on the next change, strips that
wrapper back to the runtime's original before wrapping it again. Grants
therefore replace each other instead of piling up, and survive a daemon restart
without any saved state. A program that starts like such a header but does not
check out completely is never modified: the daemon logs an error with reason
`owned_block_conflict` and the container has to be restarted.

Every change attaches the new program before detaching the one it replaces, so
an interrupted change leaves the container narrower, never wider, and the next
pass finishes it. The daemon never tracks kernel program IDs. When it cannot
tell a runtime program from the leftover of a change that failed halfway, it
wraps both, which leaves one redundant, identical wrapper: the same grants,
never wider, and stable from then on.

> **Upgrading from versions without these markers:** programs attached by
> earlier versions look exactly like the runtime's own filter, so the grants
> they carry are kept (and wrapped again) until the container is restarted.
> Restart containers that were processed by an earlier version to drop grants
> the new version cannot see.

Atomic replacement (`BPF_F_REPLACE`) is used when the kernel supports it
(Linux 5.5+, detected on first use); otherwise each new program is attached
before the original is detached, which is narrower, never wider, if it is
interrupted.

On cgroup v1 hosts the daemon writes `devices.allow` and, to take a grant back,
`devices.deny`. It remembers in memory which exceptions it added itself, so it
never revokes one the runtime made. A restarted daemon has lost that memory:
exceptions granted by the previous instance are then indistinguishable from the
runtime's and are not revoked when policy narrows, until the container is
restarted. cgroup v2 hosts do not have this gap.

Every time the daemon looks at a container (start, unpause, startup, systemd
reload) it sets the container's grants to the complete set the current
configuration allows, including none at all. A container that policy disables
or does not opt in, one with invalid labels, and one where any mounted device
cannot be resolved all get the empty set, so an earlier grant is revoked rather
than kept. The price is that a transient error (an unreadable device node, for
example) revokes the container's grants until the retry that grants them again.

The daemon pins the container's process with a pidfd (Linux 5.3+) before it
resolves the cgroup, and immediately before changing anything checks that the
process is still alive, that Docker still reports it as the container's
running process with the same start time and, for a grant, that it is still in
that cgroup. A recycled pid therefore never directs grants to another cgroup.
Once a container has exited, or when Docker cannot be asked about it, its
grants are revoked in the cgroup the daemon last verified for it, and only if
that directory is still the same one (same inode) and was not recreated.
A running container found in another cgroup than the one it was granted in
(systemd inside the container moving its processes into a child cgroup, for
example) loses the grants in the previous cgroup before it is granted in the
new one, since a grant on a parent cgroup applies to every cgroup below it.

The daemon keeps a short history of each container's runs. A `die` or
`destroy` event revokes the grants of the run it ended (the latest run that
started before the event, so a late event for an earlier run never touches a
container restarted under the same ID). What the daemon remembers about a run
is dropped only once its cgroup is verified gone; a periodic sweep checks.
