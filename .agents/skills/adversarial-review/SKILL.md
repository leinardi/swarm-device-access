---
name: adversarial-review
description: >
  Adversarial code review of changes to swarm-device-access — working tree, staged
  diff, a branch vs master, a commit range, or a PR — in any language (Rust, bash,
  YAML, Dockerfile, Makefile, docs). Uses user-level Rust skills for Rust and loads
  trust-boundary for config, policy, label, launcher and deployment paths, hunts for
  real defects, device-access widening and drift from the 1.x contract, checks the
  reuse ledger in docs/rust-port.md, then reports ranked findings with a verdict. Use when
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
than what it claims is a finding, and so is a commit type that ships the wrong version (§3,
*Release bump*): read every commit subject in the range, not only the PR title.

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
| any `**/*.rs`, `Cargo.toml` (any member), `Cargo.lock`, `clippy.toml`, `rustfmt.toml`, `rust-toolchain.toml`, `deny.toml` | user-level Rust skills, `AGENTS.md`, and `docs/rust-port.md` (decisions, crates, reuse ledger) | the lint set, `#[expect]` and the ratchet, the single unsafe module, error and log text as the 1.x contract, the Docker read/write split, the blocking transaction, Go test provenance, the reuse ledger |
| `src/config/**`, `src/policy/**`, `src/processor/**` (label handling, rule collection), the CLI and `SIGHUP` reload path (`src/cli.rs`, `src/cli/settings.rs`, `src/run.rs`), `src/launcher/**`, the HTTP server (`src/server.rs`, `src/server/**`), `deployments/**` (including `deployments/docker/Dockerfile`), `examples/**` | `trust-boundary` | fail-closed config and mode, label parsing, `/dev`-only rules, reload safety, image privilege, listeners |
| `src/cgroup/**` | user-level Rust skills, `AGENTS.md` + the note below | NVIDIA-derived BPF/cgroup code; `src/cgroup/bpf_sys.rs` is the one audited `unsafe` module |
| `meta/**`, `testenv/**` | `AGENTS.md`; the comment heading each test | a meta-test that can skip or pass on nothing is a finding; the integration harness's image identity check |
| `tests/testdata/**` | `docs/rust-port.md` (Go golden captures) | frozen 1.x captures: an edited or regenerated one is a finding unless it comes from running the 1.x code |
| `scripts/check-*.sh` | the script's header comment and its meta-test (`meta/tests/suppressions.rs`, `meta/tests/license_headers.rs`) | the check still fails on what it exists to stop |
| `.pre-commit-config.yaml`, `droast.toml`, `.suppressions-baseline`, `[workspace.lints]`, `clippy.toml` | `AGENTS.md` (coding rules and the ratchet); `meta/tests/toolchain.rs`, `meta/tests/vacuous_flags.rs`, `meta/tests/ci_gates.rs`, `meta/tests/dockerfiles.rs`, `meta/tests/workspace_lints.rs` | no lint disabled or narrowed, no hook dropped; §5 (c) |

**`src/cgroup/{api,ebpf,v1,v2}.rs` port NVIDIA-derived code** (from libnvidia-container, Apache
2.0). Touch it with care. They keep NVIDIA's block verbatim, the crun and runc provenance
paragraph and a modification notice under it; a diff that reformats or relicenses that block is a
finding, whatever else it does.

A meta-test under `meta/tests` guards a claim nothing else checks: the Rust release, zig,
cargo-zigbuild, `rust-std`, Trivy and ShellCheck pins, the Dockerfile's stages and labels, the CI
gates, the relative links, the suppression and license-header checks, the workspace lints, the
unsafe allowlist, the Docker read/write boundary, Docker kept out of the plain tests. Try to make
the guarded thing go wrong and see whether the test still notices. A test that passes because its
input is empty, its rule never ran or its negative control is missing is a false pass, and so is
one whose scratch repository runs git without `meta::gitenv::isolate`, which under the commit hook
acts on the real index.

No skill matches (bash, Makefile, `.mk/**`, plain YAML, Markdown, workflows)? Fall back to the
passes in §4 plus the invariants in §3. **Same rigor** — an unmatched language is not a lighter
review.

## 3. Repo invariants — check these on every review, whatever changed

These are the ways this codebase breaks that generic reviewers miss. Read
`AGENTS.md` for the full statements.

- **The daemon only ever widens device access for `/dev` bind mounts that policy allows.** Every
  rule it writes must be backed by a container mount whose source resolves under `/dev` *and* by
  the global and per-container allow/deny policy — `processor::rules::is_mount_source` and
  `is_device_mount` for the bind mount, and `Global::denied` / `Global::authorized` for every name
  the device was found by (`collect_mount_rules`, `src/processor/rules.rs`). **Any path that grants
  a device not backed by a mount or an allow label is a critical finding**, however clean the code
  is — the daemon runs privileged and every container on the node is in its blast radius.
- **Cgroup v2 applies swap, never stack.** `src/cgroup` wraps the runtime's device filter in an
  owned block (`owned.rs`) and swaps the wrapper in (`v2plan.rs`, `v2ops.rs`: atomic
  `BPF_F_REPLACE` where the kernel supports it, attach-then-detach otherwise), so an apply replaces
  this daemon's previous program instead of adding one. Ownership is recognised only when the whole
  wrapper regenerates byte for byte; a partial match is a conflict and is never modified. A change
  that can attach without retiring the previous owned program, or that treats a program it cannot
  regenerate as owned, stacks or clobbers programs on the host — a finding.
- **Passes always re-apply.** The coordinator's `processed` map (`src/daemon/coordinator.rs`) only
  skips a start event that an enumeration already covered. Passes — startup, `SIGHUP`, and each
  completed systemd reload (the `on_reload` callback `daemon::run` hands `systemd::supervise`,
  `src/daemon/mod.rs`) — re-apply every running container, because `systemctl daemon-reload` can
  wipe the programs. Routing a pass through that map would leave already-seen containers without
  device access after a reload — a finding.
- **The README is the user-facing source of truth.** The flag tables, the config keys, the
  container-label table and the docker-compose snippet in `README.md` must stay in sync with
  `src/cli.rs`, `src/config/mod.rs` (`FileSchema`), `src/policy/mod.rs` and the documented mounts.
- **Release bump.** `svu` derives the release version from the commit types, so check that
  every commit's type matches whether the change should ship (AGENTS.md, Commit messages).
  Anything users should receive in the binary or the image is `fix` or `feat`, even a
  performance change, a refactor or a revert of a shipped `feat`/`fix`; a `perf`, `refactor`,
  `build`, `chore` or `docs` commit carrying it means it never ships on its own. A stray `!`
  cuts a major. `svu` matches `feat`/`fix` anywhere before the colon, so a word such as
  `prefix:` in a subject counts as `fix:`. Pull requests land as merge commits, so every commit
  in the range counts, not just the PR title.
- **Parity with 1.x.** Any behaviour difference from 1.x (the Go daemon the Rust code replaced, as
  its frozen captures under `tests/testdata` and the published v1.0.3 image record it) that is not
  a numbered Q entry in the README's *Migrating from 1.x* table is a defect: a log message,
  attribute or level, an error text (including Go's `%w` chain and `errors.Join` newlines), an
  exit code, a metric family or label, an Engine API request, a default. A new difference that
  cannot be avoided adds its Q entry in the same commit. A ported test that lost its `Go:`
  provenance comment, or a golden derived from reasoning instead of a 1.x capture, is a finding.
- **No `unsafe` outside the bpf syscall module.** The workspace denies `unsafe_code`; only
  `src/cgroup/bpf_sys.rs` lifts it, with a file-level `#![expect(unsafe_code, reason = ...)]`,
  and a meta-test pins that list. A second module with `unsafe`, `libc` outside `bpf_sys.rs`, a
  widened allowlist or an `unsafe` block without a `// SAFETY:` comment is a finding.
- **No write Docker call outside the launcher.** The daemon path (processor, coordinator, event
  loop, observability) takes only the read-only Docker trait, whose client sends `GET` plus the
  one `HEAD /_ping` through one request builder. Create, start, attach, wait, stop and remove live
  only in `src/launcher/engine.rs`, and only `src/launcher/**` may name it. A write verb reachable
  from the daemon, a second request builder, or bollard outside `testenv` is a **critical**
  finding.
- **No new capability, path or listener.** The daemon and launcher get exactly the privileges,
  host paths, mounts and listeners 1.x had (`daemon_spec` in `src/launcher/spec.rs`, the README's
  runtime requirements). One that appears without the README and `AGENTS.md` changing in the same
  commit is a finding; `--debug-addr` with a non-empty value is refused (Q2), never served.
- **Byte-identical BPF wrapper encoding.** Ownership is decided by regenerating the wrapper and
  comparing bytes, so an encoding that differs from cilium asm by one byte turns every wrapper a
  1.x daemon attached into an owned-block conflict after an upgrade, and breaks a rollback to 1.x.
  A change to the instruction model, the encoder, the header or trailer markers, the nonce or
  digest layout that the fixtures and goldens under `tests/testdata` do not pin is a finding.
- **Fail-closed config and labels.** The loader, policy and processor keep every fail-closed rule
  of `trust-boundary`: an empty, unknown or malformed value denies or skips, a rejected reload
  keeps the previous settings, anything uncertain yields the empty desired set.
- **The blocking transaction.** A reconcile's cgroup and BPF mutation runs as one
  `spawn_blocking` transaction that owns the processor lock and its descriptors; its join handle
  is awaited to completion even after a timeout or cancellation, and the container's reservation
  is released only then. A mutation a dropped future can interrupt is a **critical** finding.
- **One implementation per feature, with its provenance.** `docs/rust-port.md` is the reuse
  ledger to check and update when a reused component changes or an upstream fix is taken. A
  component the ledger maps to SSE or SRD code is adapted from that code at the pinned commit, not
  rewritten independently; a change to it, or an upstream commit taken into it, updates its ledger
  row in the same commit, and a fix worth sending back goes on the ledger's upstream-candidates
  list. No equivalent feature is implemented twice without a technical reason recorded in
  `docs/rust-port.md`. A reused file carries this project's Apache-2.0 header followed by the
  provenance line (`Adapted from <repo>/<path> at <commit>, originally MIT-licensed, Copyright (c)
  <year> Roberto Leinardi`); a pasted MIT block, a missing provenance line, a reused component
  changed without its ledger row, or a missing row is a finding.

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

Any new `#[expect(...)]` or `#[allow(...)]` (`#[allow]` is denied outright),
`# shellcheck disable=` or `# droast ignore=` is a standing decision to let a linter stay
silent. The `check-suppressions` hook counts them per rule against
`.suppressions-baseline` and refuses one added without a baseline change, so the count is
reviewed; the reason is not. A baseline raised in the same diff is the thing to read. So demand
the attempt:
for `too_many_lines`, was the obvious extraction tried and what broke? For a cast lint, why is
`try_from` wrong here? For `min_ident_chars`, why does the name have to be short? A suppression
whose reason restates the lint instead of explaining why the fix does not apply is a finding, and
so is a suppression with no reason at all. The same holds for a lint lowered in
`[workspace.lints]` or a setting added to `clippy.toml` — and a setting there that matches nothing
in this repo (copied from another project) is a finding too.

### (d) Cross-file duplication

Before accepting a new helper, search for the one that already exists in the workspace (`src/**`,
`meta/**`, `testenv/**`), by *behavior*, not by the name the author chose. Check the logger, the
`/dev` mount check, the injected clock, backoff, the metrics recorder and its no-op form, test
waits and the `go_compat` ports. The reuse ledger in `docs/rust-port.md` lists the SSE and SRD
code a component must start from. Two implementations of the same rule drift apart, and the one the
reviewer did not read is the one that keeps the bug.

### (e) Docs drift for config and flags

The configuration contract is written down more than once and can disagree silently. A new or
changed flag in `src/cli.rs`, key in `src/config/mod.rs` (`FileSchema`) or label in
`src/policy/mod.rs` needs its row in the matching `README.md` table (the flag table under "⚙️
Configuration", and "Container Labels"), any affected prose in `docs/architecture.md` (Policy model,
Runtime contract), and validation that rejects bad values by name at load.

A surface the code has and the docs do not mention is a finding; so is a documented one nothing
implements, and so is a default in the docs that differs from the code.

## 6. Verify before you trust (don't hand-wave the gates)

Static reading misses things. Use focused tests while investigating
(`cargo test --locked --lib <module>::tests::<name>`), then run the gates the change owes and treat
a failure it caused as a confirmed finding with the output attached:

| Diff touched | Run |
| --- | --- |
| any `**/*.rs`, a Cargo manifest or lock, `clippy.toml`, `rustfmt.toml`, `rust-toolchain.toml`, `deny.toml` | `make verify` (build, fmt check, clippy with `-D warnings` and tests, the meta-tests included), `make rust-deny` when a manifest or `Cargo.lock` changed (network required), then `make check` |
| `src/daemon/**`, `src/processor/**`, `src/cgroup/**`, `src/launcher/**`, `testenv/**` | also `make test-integration` (needs Docker and a `dhi.io` login) — it builds the image from the Dockerfile, copies its binary out and pulls the pinned 1.x image; add `SDA_IT_ENFORCE=1` only on a throwaway host, since it attaches BPF and runs `systemctl daemon-reload`. Check that it proved the binary and the image under test are Rust before any test ran |
| `deployments/docker/Dockerfile` | droast via `make check`, `cargo test --locked -p meta --test dockerfiles --test toolchain`, plus `make docker-build` |
| `meta/**`, `scripts/check-*.sh`, `.github/**`, `.pre-commit-config.yaml` | `cargo test --locked -p meta`, `pre-commit run actionlint --all-files` |
| anything else | `make check` (pre-commit on all files) |

The integration suite drives the binary and the image `make test-integration` builds
(`SDA_RUST_TEST_BINARY`, `SDA_RUST_TEST_IMAGE`), not the code under review: running the raw
`cargo test -p testenv --features integration` without a fresh `make integration-image` tests a
stale image.

Several hooks rewrite files (prettier, markdownlint, rustfmt through `cargo fmt` when run by hand):
check `git status` afterwards and report a
rewrite as a finding instead of reviewing the rewritten tree. A skipped test is not a pass —
check the `-v` output for `SKIP`. If a gate is impractical here (no Docker, no privileges for
the integration suite, no network for cargo-deny), say so explicitly and mark that risk
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
