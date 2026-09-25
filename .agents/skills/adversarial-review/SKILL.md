---
name: adversarial-review
description: >
  Adversarial code review of a set of changes to this repo — working tree, staged
  diff, a branch vs master, a commit range, or a PR. Language-agnostic (Go, bash,
  YAML, Dockerfile, Makefile, docs). Loads the relevant swarm-device-access domain
  skills for the paths that changed, hunts for real defects and device-access
  widening, then reports ranked findings. Use whenever the user asks to review
  changes/a diff/a PR/a branch, "check my work before committing", "is this ready to
  merge", or "poke holes in this" — even if they don't name a language or say the
  word "review".
---

# Adversarial Review — swarm-device-access

You are a hostile reviewer. Assume the change is **wrong until proven right**: it hides a
bug, breaks an invariant, or drifts from a contract. Your job is to find the specific
input, state, or path where it fails — not to praise it, not to restyle it. A review that
finds nothing is only credible after you have actively tried to break the code and failed.

This skill is the **entry point for reviewing any change in this repo, in any language**.
It does not replace the domain skills — it routes to them. The domain skills own the rules;
this skill owns the mindset, the routing, and the report.

---

## 1. Establish the diff (what am I reviewing?)

Never review from memory or from the user's description of the change — read the actual
diff. Pick the scope from what the user said, defaulting to the most useful:

| User intent | Command |
| --- | --- |
| "my work" / "before I commit" / uncommitted | `git status` then `git diff HEAD` (add `git diff --staged` if staged) |
| a branch / "this PR" / "ready to merge" | `git diff master...HEAD` (merge-base diff against the default branch) |
| a specific commit range | `git diff <base>..<head>` |
| a GitHub PR number | `gh pr diff <n>` (and `gh pr view <n>` for intent) |

Also read `git log --oneline` for the range and any linked issue/PR body — the stated
**intent** is what you check the code against. A change that works but does something other
than what it claims is a finding.

Read every changed file in full, not just the hunks. A hunk looks correct in isolation and
wrong against the 40 lines above it that git didn't show you. For non-trivial changes, also
read the callers of what changed — a signature or behavior change is only safe if every
call site agrees.

## 2. Route to the domain skills (path → authority)

For each changed path, load the matching skill **before** judging that file — the skill is
the source of truth for the rules, and violations there are findings even when lint is
green. Load only what the diff touches.

| Changed path | Load skill | It owns |
| --- | --- | --- |
| any `**/*.go` | `go-style-guide` | style/lint rules golangci-lint enforces |
| `internal/config/**`, `internal/policy/**`, `internal/processor/**` (rule collection), `deployments/**` (including `deployments/docker/Dockerfile`), `examples/**` | `trust-boundary` | fail-closed config and mode, label parsing, `/dev`-only rules, reload safety, image privilege |
| `internal/cgroup/**` | `go-style-guide` + the note below | NVIDIA-derived BPF/cgroup code |

**`internal/cgroup/**` is NVIDIA-derived code** (from k8s-device-plugin, Apache 2.0). Touch it
with care — as `AGENTS.md` puts it, the upstream PRs went into runc/containerd long ago. Never
reformat or relicense NVIDIA's copyright header block — a diff that
rewrites it is a finding, whatever else it does.

No skill matches (bash, Makefile, `.mk/**`, plain YAML, Markdown, workflows)? Fall back to the
language-agnostic checklist in §4 plus this repo's cross-cutting invariants in §3. **Same
skill, same rigor** — an unmatched language is not a lighter review.

## 3. Repo invariants — check these on every review, whatever changed

These are the ways this codebase breaks that generic reviewers miss. Read
`AGENTS.md` for the full statements.

- **The daemon only ever widens device access for `/dev` bind mounts that policy allows.** Every
  rule it writes must be backed by a container mount whose source resolves under `/dev` *and* by
  the global and per-container allow/deny policy — `processor.IsMountSource` and
  `policy.Global.DeviceAllowed`, both consulted by `processor.CollectMountRules`. **Any path
  that grants a device not backed by a mount or an allow label is a top-severity finding**,
  however clean the code is — the daemon runs privileged and every container on the node is in
  its blast radius.
- **Linux-only build tag.** Every file under `cmd/` and code under `internal/` that touches
  devices, cgroups or `unix.*` carries `//go:build linux`. A new file in that territory without
  the tag is a finding: `make go-build` still cross-compiles to Linux, but on a macOS/Windows host
  the untagged file is compiled alone against symbols that only the tagged files define, so
  `go vet`, golangci-lint and gopls break for the whole package there.
  Cross-platform helpers such as `internal/logger` do not need it.
- **`internal/cgroup` provenance.** NVIDIA's Apache 2.0 header stays as it is — no reformatting,
  no relicensing (see §2).
- **Cgroup v2 programs are append-only.** `internal/cgroup/ebpf.go` attaches with
  `BPF_F_ALLOW_MULTI` so it composes with the runtime's own filter, which means a second apply
  for the same container attaches a second program rather than replacing the first. The
  `processed` map in `internal/daemon/daemon.go` and `internal/daemon/events.go` (dedup of the
  startup-enumeration/event-stream overlap) and the `resubscribeSince` cursor used on reconnect
  exist to keep applies to one per start. A change that can replay an event or re-enumerate
  without going through them grows programs on the host — a finding. **One deliberate
  exception:** `startReloadWatcher` in `internal/daemon/daemon.go` re-enumerates with a
  `fresh` map on purpose, because `systemctl daemon-reload` has already wiped the programs and
  every running container needs its rules applied again. Routing that path through the shared
  `processed` map would leave already-seen containers without device access after a reload —
  that "fix" is itself a finding.
- **The README is the user-facing source of truth.** The flag table, the container-label table
  and the docker-compose snippet in `README.md` must stay in sync with
  `cmd/swarm-device-access/flags.go`, `internal/config/loader.go` and the documented mounts.
- **Conventional Commits with a scope** (`<type>(<scope>)[!]: <description>`), enforced by the
  `conventional-pre-commit` hook with `--force-scope`. Release notes are generated from them.
- **depguard deny list** in `.golangci.yaml`: `github.com/sirupsen/logrus` outside
  `internal/logger`, `github.com/pkg/errors`, and the `github.com/instana/testify` fork. A new
  import of any of them — or a `//nolint:depguard` to sneak one in — is a finding.

## 4. Adversarial passes — language-agnostic

Do not skim for style. Run these passes, each with a "how would I make this fail" framing:

- **Correctness / logic**: off-by-one, inverted conditions (`<` vs `<=`), wrong operator
  precedence, negated guards, early returns that skip cleanup, copy-paste that kept the old
  variable. Trace one concrete failing input end to end rather than asserting "looks fine".
- **Boundaries & nil/empty**: empty slice/map/string, zero, negative, missing key, `nil`
  receiver/pointer, unset optional, first/last element, single-element collection.
- **Errors**: swallowed errors, `err` checked then ignored, wrapped-but-not-returned,
  wrong sentinel, panics on attacker- or user-controlled input, partial writes left on the
  error path.
- **Concurrency**: shared state without a lock, lock held across I/O or a channel op, goroutine
  leak, context not honored, map written from two goroutines, TOCTOU between check and use.
- **Resources**: unclosed file/conn/rows/response body, missing `defer`, context/timer leak,
  unbounded growth, N+1 query, work inside a loop that belongs outside it.
- **Security**: input reaching a query/command/path/HTML without validation, authz check
  missing or after the effect, secret in a log or response, unsafe deserialization, SSRF via
  user-supplied URL, missing rate/size limits.
- **Contract drift**: does the code do what the commit message / PR / issue claims? Public
  signature, JSON field, DB column, error code, or config key changed without updating every
  consumer and the docs/spec.
- **Tests**: does the diff add or change a test for the behavior it introduces? A test that
  passes against the *old* code (asserts nothing new), that tests mocks instead of behavior,
  or that was weakened/deleted to make the change pass — all findings. A bug fix with no
  regression test is a gap worth flagging.

Prefer one confirmed, reproducible defect over ten vague "consider"s. If you cannot name the
input and the resulting wrong behavior, it is not yet a finding — keep digging or drop it.

## 5. Always-on passes

The passes above are shaped by the diff. These five run on **every** review, whatever changed,
because each names a way a repository like this one loses something without anyone noticing.

### (a) Trust boundary

If the diff touches `internal/config/**`, `internal/policy/**`, the label handling or rule
collection in `internal/processor/**`, `deployments/**` or `examples/**`, load the
`trust-boundary` skill and walk its items against the diff. Anywhere else, ask the one question
it is built on: does this change create a new place where something from outside the daemon
becomes something it believes — a Docker label that becomes a device rule, a config value or
CLI flag that becomes a mode or a policy, a mount source that becomes a major/minor pair? If it
does, the checklist applies there too.

### (b) Deletion smell

A diff that removes a user-visible surface — a flag, a config key, a label, a metric, a log
line operators rely on, a documented behavior — and in the same breath rewrites that surface's
test to assert it is *absent* must cite the specification line that retired it. In this repo the
specification is `README.md` (flags, labels, compose snippet, security notes) and `docs/**`
(`docs/architecture.md`, `docs/testing.md`). A commit message is neither.

A test flipped from "X happens" to "X does not happen" is not evidence that X should go — it is
the deletion wearing the test's clothes. Ask, in order: which spec line says this surface is
retired? If none, this is a blocker-level finding whatever the diff's stated intent was. If one
exists, is the diff removing exactly what that line retires and no more?

### (c) A new suppression has to show its work

Any new `//nolint:`, `# shellcheck disable=` or `# hadolint ignore=` is a standing decision to
let a linter stay silent.

So demand the attempt: for a complexity or length rule, was the obvious extraction tried and what
broke? For `wrapcheck`, why is wrapping wrong here? For `varnamelen`, why does the name have to be
short? A suppression whose reason comment restates the rule instead of explaining why the fix does
not apply is a finding, and so is a suppression with no reason comment at all.

### (d) Cross-file duplication

Before accepting a new helper, search for the one that already exists. Search `internal/**` (and
`cmd/swarm-device-access/`) by *behavior*, not by the name the author chose. Two implementations
of the same rule drift apart, and the one the reviewer did not read is the one that keeps the
bug.

### (e) Docs drift for config and flags

The configuration contract is written down more than once and can disagree silently. A new or
changed flag in `cmd/swarm-device-access/flags.go`, key in `internal/config/loader.go`
(`FileSchema`) or label in `internal/policy/policy.go` needs its row in the matching `README.md`
table (the flag table under "⚙️ Configuration", and "Container Labels"), any affected prose in `docs/architecture.md`
(Policy model, Runtime contract), and validation that rejects bad values by name at load. A key
the code reads and the docs do not mention is a finding; so is a documented key nothing reads,
and so is a default in the table that differs from the code.

## 6. Verify before you trust (don't hand-wave the gates)

Static reading misses things. Run the gates the change already owes and treat a failure as a
confirmed finding with the output attached:

| Diff touched | Run |
| --- | --- |
| any `**/*.go` | `make go-vet`, `make go-test`, then `make check-stage` |
| anything else | `make check-stage` |
| `internal/daemon/**`, `internal/processor/**`, `internal/cgroup/**` | `make go-test-integration` (needs Docker and privileges) — it runs `make go-build` first, then `go test -tags=integration ./test/integration/...` |
| `deployments/docker/Dockerfile` | `make docker-build` |

The integration suite drives the binary in `dist/` (or `$SDA_TEST_BINARY`), not the package
under test. Running the raw `go test -tags=integration ./test/integration/...` without a fresh
`make go-build` either tests a stale binary or skips every test (`t.Skipf` when the binary is
missing) and still exits 0. **A skipped test is not a pass** — check the `-v` output for `SKIP`.

If a gate is impractical here (no Docker, no privileges for the integration suite), say so
explicitly and mark that risk unverified rather than implying it passed.

## 7. Report

Rank by severity, worst first. Nothing is more important than a genuine correctness break or
an unbacked device grant; skip pure formatting the linters already catch unless it changes
meaning. For each finding:

```
<path>:<line> — <severity: blocker | high | medium | low>: <one-line defect>
  Failure: <the concrete input/state → the wrong result or broken invariant>
  Fix: <the specific change>
```

End with a one-line verdict: **block**, **approve with nits**, or **approve** — plus which
verification gates you actually ran and which you couldn't. If you found nothing, state what
you tried to break so the "no findings" is credible. Be blunt; do not soften a real defect to
be polite, and do not invent findings to look thorough.
