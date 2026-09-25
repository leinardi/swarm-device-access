---
name: trust-boundary
description: >
  Checklist for code that decides which devices a container may open, or what the
  daemon believes about its own configuration. Apply before editing internal/config/**,
  internal/policy/**, the label handling and rule collection in internal/processor/**,
  the SIGHUP reload path in cmd/swarm-device-access/config.go, deployments/** (including
  the Dockerfile) or examples/**. Read it before writing the change, not after review
  finds the hole.
---

# Trust Boundary — swarm-device-access

A trust boundary is any place where something outside the daemon becomes something the daemon
believes: a Docker label becomes a device rule, a config file value or CLI flag becomes a mode or a
policy, a mount source becomes a major/minor pair in a cgroup program. Every item below exists
because the cheap version of that translation fails open — it grants more device access than
intended when the input is empty, unknown, hostile, or simply malformed.

The stakes are specific to this daemon: it runs privileged, in the host cgroup and PID
namespaces, and every rule it writes widens what a container can open on the host. A wrong answer
here is not a failed request; it is a container reading a raw disk.

Nothing here is enforced by a linter. The tests named under each item are what enforces it. Where
an item notes a **known gap**, the code is as described but the test or check is missing — close it
the next time you touch that file, and do not widen it in the meantime.

---

## 1. Fail closed on empty or unknown config and mode

A `switch` on a config value must have a default that denies, not one that falls through to the
most permissive branch — and never a zero value that means "process everything".

`policy.Global.Enabled` (`internal/policy/policy.go`) processes a container in `ModeOptIn` only
when `swarm-device-access.enable` is `true`, in `ModeAll` unless it is `false`, and returns `false`
in its default branch — so an empty or unknown `policy.Mode` processes nothing. The zero-value
`config.Runtime` that `config.Store.Load` returns before `Set` has an empty mode, and is therefore
safe for the same reason. Validation at startup refuses both values already, so that branch is
unreachable in practice — which is exactly why it must stay: it is the second lock, for the day a
new construction path skips validation.

- [ ] Every new mode-like switch denies in its default branch.
- [ ] A test asserts the empty value and an unknown value both deny. **Known gap:** `TestEnabled`
      (`internal/policy/policy_test.go`) covers only `ModeOptIn` and `ModeAll`, and
      `TestGlobalValidate` covers `"invalid"` but not `""` — add both cases.

## 2. Validate every enum-like config value at load

A string in YAML or on the command line is not an enum. If a value is compared anywhere against a
fixed set, it must be rejected at load time, naming the key and the bad value.

`policy.ParseMode` rejects an unknown `policy-mode`, and `policy.Global.Validate` wraps glob errors
as `device-allow: …` / `device-deny: …` via `policy.ValidateGlobs`. `run` in
`cmd/swarm-device-access/main.go` calls `Validate` after `applyFileConfig` merges the file into the
flags, and exits `1` with `invalid config: …` on failure; an unreadable or unparsable file from
`config.LoadFile` (`internal/config/loader.go`) makes it exit `1` with `config file error: …`. Covered by
`TestGlobalValidate` and `TestValidateGlobs`.

- [ ] A new enum-like key or flag is checked in `Validate` (or an equivalent load-time check) with
      a table test covering an unknown value.
- [ ] The error names the key, so an operator can fix it without reading Go.
- [ ] **Known gap:** `log-format` and `log-level` are not validated — `logger.Configure` falls back
      to `text` and `INFO` on anything it does not recognize. That cannot widen device access, but
      it is the pattern this item forbids; do not copy it for a key that can.

## 3. Labels are untrusted input

Anyone who can `docker service create` or `docker run` sets `swarm-device-access.*` labels, and on
manager nodes service labels are merged in too (`policy.MergeLabels`, container labels winning,
called from `Processor.ProcessContainer` in `internal/processor/processor.go`). Treat every label
as attacker-controlled.

- `policy.ParseContainer` rejects a non-boolean `enable` and any malformed glob in `device-allow`
  or `device-deny`, naming the label. `ProcessContainer` then **skips the container** (warns,
  records `invalid_labels`) rather than falling back to the global policy — a typo in a narrowing
  label must never widen access to "whatever the global allows".
- Labels can only narrow: `policy.Global.DeviceAllowed` checks the global deny, the container deny,
  the global allow and the container allow, in that order. **Deny wins over allow**, and a
  per-container allow can never re-admit a path the global allow excludes.
- An empty or all-whitespace `device-allow` label means "inherit the global allow set", not
  "allow nothing" — that is documented in the README label table; keep it that way or change both.

Covered by `TestParseContainer`, `TestDeviceAllowed`, `TestExplicitlyAllowed`, `TestMergeLabels`
and `TestProcessContainer_SwarmServiceLabels`.

- [ ] New label parsing returns an error on malformed input, and the caller skips the container.
- [ ] A new label can only narrow; a test proves deny still beats allow with it set.
- [ ] Unknown `swarm-device-access.*` keys keep being reported (`policy.UnknownLabels`), not
      silently accepted.
- [ ] **Known gap:** on a manager node, when the parent-service inspect fails,
      `Processor.resolveServiceLabels` logs `could not inspect parent service; using container
      labels only` and returns no service labels — so a service-level `device-deny` (set under
      `deploy.labels:`, which the README already tells users not to do) is silently dropped and
      the container gets wider access than the deployer asked for. It is not an escalation — the
      same deployer sets both label sets — but it is the "lost narrowing label means wider access"
      pattern this item forbids. Do not extend it; the fail-closed fix is to skip the container
      when a service it belongs to cannot be inspected.

## 4. Only `/dev` mount sources become rules

`collectContainerRules` (`internal/processor/processor.go`) passes a mount to
`processor.CollectMountRules` only when `processor.IsMountSource` says its `Source` is `/dev` or
under `/dev/`. `CollectMountRules` (`internal/processor/rules.go`) resolves a symlinked mount
source, walks directory mounts, applies `DeviceAllowed` to each resolved entry, and emits a rule only
for a character or block device. Unresolvable symlinks get two different treatments:

- a dangling or unresolvable symlink **found while walking a directory mount** is skipped, with a
  warning only when an explicit allow glob names it (`skipUnresolvable`, the fix merged in commit
  `244d2a5`);
- a mount **source** that is itself an unresolvable symlink returns an error in
  `MountResult.Errs`, which `ProcessContainer` logs as a rule failure — no rule is emitted for it
  either way.
Covered by `TestIsDeviceMountSource`, `TestCollectMountRules_*` and
`TestCollectMountRules_UnresolvableSymlinks_ExplicitAllow` / `_NoAllowGlobs`.

- [ ] No new code path turns a non-`/dev` source into a rule.
- [ ] A new resolution step (symlink, bind, overlay) fails only that entry on error — a skip or a
      per-mount error in `MountResult.Errs` — and never falls back to the unresolved path.
- [ ] **Known gap:** `IsMountSource` is a lexical prefix check on the unresolved `Source`, and
      the mount `Source` comes from whoever writes the service spec — the same untrusted deployer
      who sets the labels — so `/dev/../etc` passes it. The resolved targets in
      `CollectMountRules` and `visitSymlink` are policy-checked but not re-checked against
      `/dev`, and `/dev/shm` and `/dev/mqueue` are world-writable (mode 1777) on normal hosts, so
      a non-root host user or a container that bind-mounts `/dev/shm` can plant symlinks the walk
      follows. What bounds this today is not who can write to `/dev`; it is that
      `DeviceAllowed` runs on the **resolved** path, so allow globs anchored at `/dev/...` reject a
      target they do not name, and that only character and block device nodes become rules. With
      no allow globs configured (the default) that bound is only as tight as the deployer's
      ability to mount the target directly. A change that touches resolution must not widen
      this; the right fix is `filepath.Clean` plus an `IsMountSource` check on every resolved
      path.

## 5. Hot reload never widens access on a parse error

`watchSIGHUP` (`cmd/swarm-device-access/config.go`) re-reads the file on `SIGHUP`. On a read or
parse error from `config.LoadFile` it logs `config reload failed` and keeps the running config; on
a `policy.Global.Validate` failure it logs `config reload: invalid policy; keeping previous config`
and keeps it too. `config.Store.Set` swaps the whole `config.Runtime` (policy and `dry-run`)
atomically, so no reader sees half a policy.

- [ ] Every new reloadable key is validated before `store.Set`, and a failure `continue`s without
      touching the store.
- [ ] A reload that fails leaves the previous policy and `dry-run` in force — never a zero value,
      never a partially merged one.
- [ ] **Known gap:** `watchSIGHUP` calls `logger.Configure` with the file's `log-format`,
      `log-level` and `log-time` before `Validate` runs, so a reload rejected for its policy still
      changes logging. Device access is unaffected, but do not move any access-relevant setting
      ahead of validation the same way.
- [ ] **Known gap:** there is no test for the reload path. A change to `watchSIGHUP` adds one that
      sends an invalid file and asserts the previous policy is still loaded.

## 6. Least privilege in the image and the deployment

The daemon has to be privileged: it attaches BPF programs to other containers' cgroups and stats
host device nodes. So the rule is not "unprivileged" but "no more than the documented set". The
README's Docker Compose for Swarm snippet and `deployments/docker/docker-compose.yaml` run it with
`--privileged`, `--cgroupns=host`, `--pid=host`, `--userns=host` and exactly three bind mounts:
`/sys:/host/sys`, `/var/run/docker.sock` and `/dev:/dev`. The DBus socket
(`/run/dbus/system_bus_socket:/var/run/dbus/system_bus_socket`) stays **optional and commented
out** — the daemon degrades to a startup warning without it. `deployments/docker/Dockerfile` builds
a static binary onto `dhi.io/static` with `USER 0` and nothing else in the runtime stage.

- [ ] No new capability, namespace flag or host bind mount beyond the documented set, in the
      Dockerfile, `deployments/**`, `examples/**` or the README — and if one is truly needed, the
      README and `AGENTS.md` runtime requirements change in the same commit.
- [ ] The DBus mount is never made mandatory.
- [ ] A Dockerfile change is checked with `make docker-build`.
- [ ] Nothing secret is baked into a layer: build args and `COPY` sources are not a secret store.

---

## Two rules that outrank convenience

- **Never grant a device that is not backed by a `/dev` mount and the allow/deny policy.** No
  shortcut — a "grant everything under the mount", a retry that skips the policy check, a
  dry-run path that writes anyway — is worth a container opening a device nobody allowed. See the
  device-access invariant in the `adversarial-review` skill.
- **Never log or export a secret value.** The daemon handles none today — labels, mount paths and
  device numbers are not secrets — so a change that starts reading one (registry credentials, a
  token in a label) must keep it out of logs, metrics labels and error strings.

## Checklist

- [ ] Empty and unknown mode deny; a test covers both
- [ ] Enum-like config and flags validated at load, by name
- [ ] Malformed labels skip the container; labels only narrow; deny beats allow
- [ ] Only resolved `/dev` device nodes become rules; unresolvable symlinks are skipped
- [ ] SIGHUP reload keeps the previous policy and `dry-run` on any read, parse or validation error
- [ ] No privilege or mount beyond the README's documented set; DBus mount stays optional
