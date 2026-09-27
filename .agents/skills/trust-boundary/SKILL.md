---
name: trust-boundary
description: >
  Checklist for code that decides which devices a container may open, or what the
  daemon believes about its own configuration. Apply before editing internal/config/**,
  internal/policy/**, the label handling and rule collection in internal/processor/**,
  the SIGHUP reload path in cmd/swarm-device-access/config.go, internal/launcher/**,
  deployments/** (including the Dockerfile) or examples/**. Read it before writing the change, not after review
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
`cmd/swarm-device-access/main.go` calls `Validate` and `config.ValidateEnums` (log-format, log-level,
policy-mode) on the effective values after `applyFileConfig` merges the file into the flags, and exits
`1` with `invalid config: …` on failure. `config.LoadFile` (`internal/config/loader.go`) is strict: it
checks every known key's exact YAML shape (following aliases; explicit `null`, a quoted `"yes"` for a
bool and `[null]` in a list are errors), refuses merge keys, unknown keys and a second document, and
validates the enums; any failure makes the daemon exit `1` with `config file error: …`. Covered by
`TestGlobalValidate`, `TestValidateGlobs`, `TestLoadFile_*` and `TestValidateEnums`.

- [ ] A new enum-like key or flag is checked in `Validate` (or an equivalent load-time check) with
      a table test covering an unknown value.
- [ ] The error names the key, so an operator can fix it without reading Go.
- [ ] A new file key gets an entry in `keyShapes` (`internal/config/loader.go`), so an explicit
      `null` or a wrong type is rejected instead of decoding as "not set".

## 3. Labels are untrusted input

Anyone who can `docker service create` or `docker run` sets `swarm-device-access.*` labels. The
container's own labels are the **only** label input: `Processor.Reconcile`
(`internal/processor/processor.go`) parses `Config.Labels` from the container inspect and nothing
else. Swarm service labels (`deploy.labels:`) are ignored, and the processor never calls
`ServiceInspect` or `Info` (neither is part of `DockerInspector`), so a decision cannot depend on
which node runs the daemon or on whether a service inspect happened to succeed. Treat every label
as attacker-controlled.

- `policy.ParseContainer` rejects a non-boolean `enable` and any malformed glob in `device-allow`
  or `device-deny`, naming the label. `Reconcile` then gives the container the **empty** device
  set (warns, records `invalid_labels`, revokes any earlier grant) rather than falling back to the
  global policy — a typo in a narrowing label must never widen access to "whatever the global
  allows", nor keep what an earlier, valid label granted.
- Labels can only narrow: `policy.Global.DeviceAllowed` checks the global deny, the container deny,
  the global allow and the container allow, in that order. **Deny wins over allow**, and a
  per-container allow can never re-admit a path the global allow excludes.
- An empty or all-whitespace `device-allow` label means "inherit the global allow set", not
  "allow nothing" — that is documented in the README label table; keep it that way or change both.

Covered by `TestParseContainer`, `TestDeviceAllowed`, `TestExplicitlyAllowed`,
`TestReconcile_IgnoresServiceLevelLabels` and
`TestReconcile_WarnsOnUnknownContainerLabel`.

- [ ] New label parsing returns an error on malformed input, and the caller applies the empty set.
- [ ] A new label can only narrow; a test proves deny still beats allow with it set.
- [ ] Unknown `swarm-device-access.*` keys on the container keep being reported
      (`policy.UnknownLabels`, warned by `Reconcile`), not silently accepted.
- [ ] No second label source (service, node, stack) is added back: a label that only some nodes
      can read, or that disappears when an API call fails, turns a lost narrowing label into wider
      access.

## 4. Only `/dev` mount sources become rules

`collectContainerRules` (`internal/processor/processor.go`) passes a mount to
`processor.CollectMountRules` only when `processor.IsMountSource` says its `Source` is `/dev` or
under `/dev/`. From there every name (the source, each walked entry, each symlink) goes through one
`evaluateCandidate` (`internal/processor/rules.go`) on file descriptors, never re-walked paths:

- a lexical gate: the name must be clean and under `/dev`, so `/dev/../etc/x` is skipped
  (`outside_dev`);
- `openat2` beneath a `/dev` descriptor with `RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS` (the `devFS`
  seam in `devfs.go`): no symlink step can leave `/dev`. An absolute link back into `/dev` is
  followed by hand, bounded to 40 hops; any other escape is skipped (a warning only when an explicit
  allow glob names it, as for a dangling name; the skip itself never depends on the log level). The daemon
  refuses to start without `openat2` (`ProbeOpenat2`);
- `fstat` on that descriptor gives the device number, and sysfs `DEVNAME` gives the canonical name,
  so a node planted under `/dev/shm` is judged as the device it is;
- `Denied` is checked on the alias, the resolved and the canonical name, `Authorized` on the
  canonical and the resolved name; candidates are aggregated per device across all mounts, and one
  deny vote suppresses the device.

A dangling name found while walking is skipped (a warning only when an explicit allow glob names it).
Everything that leaves the set unknown is an entry in `MountResult.Errs` and makes the container's
whole desired set empty (reason `incomplete_device_set`) until a retry succeeds: a missing mount
source, an identity that cannot be established for a candidate policy would otherwise grant, any
walk error, and a directory mount over `maxMountEntries`. A partly known set is never applied.
Covered by `TestIsDeviceMountSource`, `TestEvaluateIdentity`, `TestCanonicalName`, `TestCollect_*`,
`TestProcessor_*EmptiesTheSet` and `TestRealDevFS_Openat2Containment`.

- [ ] No new code path turns a non-`/dev` source into a rule.
- [ ] A new resolution step goes through `evaluateCandidate`; it skips a name that names nothing
      under `/dev`, and anything it cannot establish is an error that empties the set — never a
      fallback to path-only policy and never a silent skip.
- [ ] Policy keeps judging the canonical identity: deny on every name, allow on the canonical and
      resolved names. An alias-level deny is only a reconciliation-time veto (a container can hide
      the alias between passes); the durable boundary is a glob on the canonical name.
- [x] **Closed gap (containment):** mount sources and symlink targets are contained to `/dev` by
      the lexical gate and `openat2 RESOLVE_BENEATH`, not by a prefix check on the unresolved path.
- [x] **Closed gap (planted nodes):** a node planted in a world-writable `/dev` directory
      (`/dev/shm`, `/dev/mqueue`) is judged by its sysfs `DEVNAME`, so `/dev/shm/x` made as `b 8:0`
      is denied by a deny on `/dev/sd*` and not authorized by an allow on `/dev/shm/*`.

## 5. Hot reload never widens access on a parse error

`watchSIGHUP` (`cmd/swarm-device-access/config.go`) calls `reloader.reload` on `SIGHUP`. It
re-reads the file and recomputes every setting with the pure `mergeSettings(flags, cliSet, file)`
from the flag snapshot taken right after `flag.Parse`, so a key removed from the file falls back to
the command line or the default. Validation (`settings.validate`: enums and policy) and the dry-run
rule (a reload may turn `dry-run` off, never on) run before anything is applied; any failure is
logged as `config reload rejected; keeping previous config` and leaves the store, the logger and
the in-force settings untouched. On success the logger is reconfigured and the runtime is published
through `Processor.PublishAndReconcile`, which swaps the whole `config.Runtime` (policy and
`dry-run`) atomically and reconciles every running container. Restart-only settings changed in the
file are logged and ignored. Covered by `TestReload_*` and `TestMergeSettings_*`.

- [ ] Every new setting goes through `settings`/`mergeSettings`, is checked in
      `settings.validate`, and a failure returns before `configureLogger` or `publish`.
- [ ] A reload that fails leaves the previous policy and `dry-run` in force — never a zero value,
      never a partially merged one.
- [ ] A new restart-only setting is added to `settings.coldChanges`.

## 6. Least privilege in the image and the deployment

The daemon has to be privileged: it attaches BPF programs to other containers' cgroups and stats
host device nodes. So the rule is not "unprivileged" but "no more than the documented set".

The single source of truth for that set is `daemonSpec` (`internal/launcher/spec.go`): the Swarm
service in the README's Docker Compose for Swarm snippet, `deployments/docker/docker-compose.yaml`
and `examples/**` runs the image as an unprivileged `launch` task with only the Docker socket, and
the launcher creates the daemon with `Privileged`, host cgroup, PID and user namespaces, network
`none`, `AutoRemove`, and exactly three bind `Mounts`: the Docker socket at `/var/run/docker.sock`,
`/sys:/host/sys` and `/dev:/dev`. `Mounts`, never `Binds`: a missing source must fail the create,
not make Docker create an empty directory on the host. The optional extras are opt-in launch flags:
`-dbus` (`/run/dbus/system_bus_socket:/var/run/dbus/system_bus_socket` — the daemon degrades to a
startup warning without it), `-config-dir` (read-only at `/etc/swarm-device-access`) and
`-host-network`. `TestDaemonSpec` pins the whole host config, so a new mount or flag shows up as a
test change. The launcher's own inputs are checked too: `-config-dir` and `-host-docker-socket`
must be absolute, clean paths, and it removes a `swarm-device-access` container only when its
owning launcher is confirmed gone — never on a transient error, and never a foreign container.
`deployments/docker/Dockerfile` builds a static binary onto `dhi.io/static` with `USER 0` and
nothing else in the runtime stage.

- [ ] No new capability, namespace flag or host bind mount beyond the documented set, in
      `daemonSpec`, the Dockerfile, `deployments/**`, `examples/**` or the README — and if one is truly needed, the
      README and `AGENTS.md` runtime requirements change in the same commit.
- [ ] The DBus mount is never made mandatory (`-dbus` stays off by default).
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
- [ ] Only device nodes resolved beneath `/dev` become rules; an unknown identity or walk empties the set
- [ ] SIGHUP reload keeps the previous policy and `dry-run` on any read, parse or validation error
- [ ] No privilege or mount beyond the README's documented set; DBus mount stays optional
