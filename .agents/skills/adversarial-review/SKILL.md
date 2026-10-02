---
name: adversarial-review
description: >
  Adversarial code review of changes to swarm-device-access — working tree, staged
  diff, a branch vs master, a commit range, or a PR — in any language (Go, bash,
  YAML, Dockerfile, Makefile, docs). Loads go-style-guide for Go and trust-boundary
  for config, policy, label, launcher and deployment paths, hunts for real defects
  and device-access widening, then reports ranked findings with a verdict. Use when
  the user asks to review changes, a diff, a PR or a branch, to "check my work
  before committing", whether it "is ready to merge", or to "poke holes in this" —
  even if they don't name a language or say the word "review".
---

# Adversarial Review — swarm-device-access

You are a hostile reviewer. Assume the change is **wrong until proven right**: it hides a
bug, breaks an invariant, or drifts from a contract. Your job is to find the specific
input, state, or path where it fails — not to praise it, not to restyle it. A review that
finds nothing is only credible after you have actively tried to break the code and failed.

This skill is the **entry point for reviewing any change in this repo, in any language**.
It does not replace the domain skills — it routes to them. The domain skills own the rules;
this skill owns the mindset, the routing, and the report.

Copy this checklist and tick items as you go:

```text
Review progress:
- [ ] 1. Scope chosen; diff, stated intent and every changed file read in full
- [ ] 2. Domain skills from §2 loaded for every changed path
- [ ] 3. Repo invariants checked
- [ ] 4. Adversarial passes run; every candidate confirmed or dropped
- [ ] 5. Always-on passes (a)–(e) run
- [ ] 6. Gates run; `git status` checked for hook rewrites; skipped gates marked unverified
- [ ] 7. Report written: findings, open questions, verdict, gates run and not run
```

---

## 1. Establish the diff (what am I reviewing?)

Never review from memory or from the user's description of the change — read the actual
diff. Pick the scope from what the user said:

| User intent | Command |
| --- | --- |
| "my work" / "before I commit" / uncommitted | `git status --short`, then `git diff HEAD`; read untracked files too, which no diff shows |
| staged changes only | `git diff --staged` |
| a branch / "this PR" / "ready to merge" | `git diff master...HEAD` (merge-base diff; `master` is this repo's default branch) |
| a specific commit range | `git diff <base>..<head>` |
| a GitHub PR number | `gh pr view <n>` for intent, then `gh pr diff <n>` |

If the user names no scope, review the uncommitted work (first row); if the tree is
clean, review the branch against `master` (third row).

Also read `git log --oneline` for the range and any linked issue/PR body — the stated
**intent** is what you check the code against. A change that works but does something other
than what it claims is a finding.

Read every changed file in full, not just the hunks. A hunk looks correct in isolation and
wrong against the 40 lines above it that git didn't show you. For non-trivial changes, also
read the callers, implementations, tests and docs of what changed — found with a reference
search, not assumed from the diff: a signature or behavior change is only safe if every call
site agrees.

## 2. Route to the domain skills (path → authority)

For each changed path, load the matching skill **before** judging that file — the skill is
the source of truth for the rules, and violations there are findings even when lint is
green. Load only what the diff touches.

| Changed path | Load skill | It owns |
| --- | --- | --- |
| any `**/*.go` | `go-style-guide` | style/lint rules golangci-lint enforces, the helpers to reuse, test waits, context handling |
| `internal/config/**`, `internal/policy/**`, `internal/processor/**` (label handling, rule collection), `cmd/swarm-device-access/config.go` (SIGHUP reload), `internal/launcher/**`, `deployments/**` (including `deployments/docker/Dockerfile`), `examples/**` | `trust-boundary` | fail-closed config and mode, label parsing, `/dev`-only rules, reload safety, image privilege |
| `internal/cgroup/**` | `go-style-guide` + the note below | NVIDIA-derived BPF/cgroup code |

**`internal/cgroup/**` is NVIDIA-derived code** (from k8s-device-plugin, Apache 2.0). Touch it
with care. Never reformat or relicense NVIDIA's copyright header block — a diff that rewrites
it is a finding, whatever else it does.

No skill matches (bash, Makefile, `.mk/**`, plain YAML, Markdown, workflows)? Fall back to the
passes in §4 plus the invariants in §3. **Same rigor** — an unmatched language is not a lighter
review.

## 3. Repo invariants — check these on every review, whatever changed

These are the ways this codebase breaks that generic reviewers miss. Read
`AGENTS.md` for the full statements.

- **The daemon only ever widens device access for `/dev` bind mounts that policy allows.** Every
  rule it writes must be backed by a container mount whose source resolves under `/dev` *and* by
  the global and per-container allow/deny policy — `processor.IsMountSource` for the bind
  mount, and `policy.Global.Denied` / `Authorized` for every name the device was found by
  (`processor.CollectMountRules`, `internal/processor/rules.go`). **Any path that grants a
  device not backed by a mount or an allow label is a critical finding**, however clean the
  code is — the daemon runs privileged and every container on the node is in its blast radius.
- **Linux-only build tag.** Every file under `cmd/` and code under `internal/` that touches
  devices, cgroups or `unix.*` carries `//go:build linux`. A new file in that territory without
  the tag is a finding: `make go-build` still cross-compiles to Linux, but on a macOS/Windows host
  the untagged file is compiled alone against symbols that only the tagged files define, so
  `go vet`, golangci-lint and gopls break for the whole package there.
  Cross-platform helpers such as `internal/logger` do not need it.
- **Cgroup v2 applies swap, never stack.** `internal/cgroup` wraps the runtime's device filter in an
  owned block (`owned.go`) and swaps the wrapper in (`v2plan.go`, `v2ops.go`: atomic `BPF_F_REPLACE`
  where the kernel supports it, attach-then-detach otherwise), so an apply replaces this daemon's
  previous program instead of adding one. Ownership is recognised only when the whole wrapper
  regenerates byte for byte; a partial match is a conflict and is never modified. A change that can
  attach without retiring the previous owned program, or that treats a program it cannot regenerate
  as owned, stacks or clobbers programs on the host — a finding.
- **Passes always re-apply.** The coordinator's `processed` map (`internal/daemon/coordinator.go`)
  only skips a start event that an enumeration already covered. Passes — startup, `SIGHUP`, and each
  completed systemd reload (`startReloadWatcher`, `internal/daemon/daemon.go`) — re-apply every
  running container, because `systemctl daemon-reload` can wipe the programs. Routing a pass through
  that map would leave already-seen containers without device access after a reload — a finding.
- **The README is the user-facing source of truth.** The flag table, the container-label table
  and the docker-compose snippet in `README.md` must stay in sync with
  `cmd/swarm-device-access/flags.go`, `internal/config/loader.go` and the documented mounts.
- **Release bump.** `svu` derives the release version from the commit types, so check that
  the type matches whether the change should ship (AGENTS.md, Commit messages).
- **depguard.** No new import on the deny list in `go-style-guide` (*Forbidden packages*),
  and no `//nolint:depguard` to sneak one in.

## 4. Adversarial passes — language-agnostic

Do not skim for style. Run these passes, each with a "how would I make this fail" framing:

- **Generic passes**: correctness and logic, boundaries and nil/empty, aliasing, error
  handling, concurrency, resources and security. For each, name one concrete failing input and
  trace it end to end rather than asserting "looks fine".
- **Contract drift**: does the code do what the commit message / PR / issue claims? A public
  signature, flag, config key, label, metric, output format or error text changed without
  updating every consumer and the docs (§5 (e)).
- **Tests**: does the diff add or change a test for the behavior it introduces? A test that
  passes against the *old* code (asserts nothing new), that asserts on a fake's recorded calls
  instead of the behavior they produced, or that was weakened/deleted to make the change pass —
  all findings. A bug fix with no regression test is a gap worth flagging.

For each candidate defect:

1. Reproduce it with a focused test, or trace one concrete input through the code to the wrong
   result.
2. Confirmed: it is a finding. Record the input and the wrong behavior.
3. Not confirmed: dig once more (callers, tests, config path). Still not confirmed: drop it.
   A vague "consider" is not a finding.

## 5. Always-on passes

The passes above are shaped by the diff. These run on **every** review, whatever changed.

### (a) Trust boundary

If §2 routed any changed path to `trust-boundary`, walk its items against the diff. Anywhere
else, ask the one question it is built on: does this change create a new place where something
from outside the daemon becomes something it believes — a Docker label that becomes a device
rule, a config value or CLI flag that becomes a mode or a policy, a mount source that becomes a
major/minor pair? If it does, the checklist applies there too.

### (b) Deletion smell

A diff that removes a user-visible surface — a flag, a config key, a label, a metric, a log
line operators rely on, a documented behavior — and in the same breath rewrites that surface's
test to assert it is *absent* must cite the specification line that retired it. The
specification here is `README.md` (flags, labels, compose snippet, security notes) and `docs/**`
(`docs/architecture.md`, `docs/testing.md`). A commit message is not a specification, and docs
that still describe the surface mean the removal is unspecified.

A test flipped from "X happens" to "X does not happen" is not evidence that X should go — it is
the deletion wearing the test's clothes. Ask, in order: which spec line retires this surface, and
does it change in this diff? If none, this is a **critical** finding whatever the diff's stated
intent was. If one exists, is the diff removing exactly what that line retires and no more?

### (c) A new suppression has to show its work

Any new `//nolint:`, `# shellcheck disable=` or `# hadolint ignore=` is a standing decision to
let a linter stay silent, and nothing in this repo ratchets their number. So demand the attempt:
for a complexity or length rule, was the obvious extraction tried and what broke? For
`wrapcheck`, why is wrapping wrong here? For `varnamelen`, why does the name have to be short?
A suppression whose reason comment restates the rule instead of explaining why the fix does not
apply is a finding, and so is a suppression with no reason comment at all. The same holds for a
new exclusion in `.golangci.yaml` — and an exclusion, enable or setting there that matches nothing
in this repo (copied from another project) is a finding too.

### (d) Cross-file duplication

Before accepting a new helper, search for the one that already exists — in `internal/**` and
`cmd/swarm-device-access/`, by *behavior*, not by the name the author chose. The *Reuse before
writing* table in `go-style-guide` lists the helpers that already exist (the logger, the `/dev`
mount check, context-aware waits, backoff, the nil-safe metrics recorder, test waits). Two
implementations of the same rule drift apart, and the one the reviewer did not read is the one
that keeps the bug.

### (e) Docs drift for config and flags

The configuration contract is written down more than once and can disagree silently. A new or
changed flag in `cmd/swarm-device-access/flags.go`, key in `internal/config/loader.go`
(`FileSchema`) or label in `internal/policy/policy.go` needs its row in the matching `README.md`
table (the flag table under "⚙️ Configuration", and "Container Labels"), any affected prose in
`docs/architecture.md` (Policy model, Runtime contract), and validation that rejects bad values
by name at load.

A surface the code has and the docs do not mention is a finding; so is a documented one nothing
implements, and so is a default in the docs that differs from the code.

## 6. Verify before you trust (don't hand-wave the gates)

Static reading misses things. Use focused tests while investigating (`go test
./internal/<pkg> -run <Name>`), then run the gates the change owes and treat a failure it
caused as a confirmed finding with the output attached:

| Diff touched | Run |
| --- | --- |
| any `**/*.go` | `make go-build`, `make go-vet`, `make go-test` (race detector on), then `make check` |
| `internal/daemon/**`, `internal/processor/**`, `internal/cgroup/**`, `internal/launcher/**` | also `make go-test-integration` (needs Docker and privileges) — it runs `make go-build` first, then `go test -tags=integration ./test/integration/...`; add `SDA_IT_ENFORCE=1` only on a throwaway host, since it attaches BPF and runs `systemctl daemon-reload` |
| `go.mod` / `go.sum` | `make go-tidy` and `make audit-deps` (govulncheck plus the banned-module check; network required) |
| `deployments/docker/Dockerfile` | hadolint via `make check`, plus `make docker-build` |
| anything else | `make check` (pre-commit on all files) |

The integration suite drives the binary in `dist/` (or `$SDA_TEST_BINARY`), not the package
under test. Running the raw `go test -tags=integration ./test/integration/...` without a fresh
`make go-build` either tests a stale binary or skips every test (`t.Skipf` when the binary is
missing) and still exits 0.

golangci-lint may not be on `PATH`; run it through pre-commit. Several hooks rewrite files
(prettier, markdownlint, the golangci formatters): check `git status` afterwards and report a
rewrite as a finding instead of reviewing the rewritten tree. A skipped test is not a pass —
check the `-v` output for `SKIP`. If a gate is impractical here (no Docker, no privileges for
the integration suite, no network for govulncheck), say so explicitly and mark that risk
unverified rather than implying it passed.

## 7. Report

Rank by severity, worst first. Nothing is more important than a genuine correctness break or
an unbacked device grant: those are normally **critical**. Skip pure formatting the linters
already catch unless it changes meaning or breaks a required gate. For each finding:

```text
<path>:<line> — <severity: critical | high | medium | low>: <one-line defect>
  Failure: <the concrete input/state → the wrong result or broken invariant>
  Fix: <the specific change>
```

Findings first, then open questions or assumptions, then a one-line verdict: **block**,
**approve with nits**, or **approve** — plus which verification gates you actually ran and which
you couldn't. If you found nothing, state what you tried to break so the "no findings" is
credible. Be blunt; do not soften a real defect to be polite, and do not invent findings to look
thorough.
