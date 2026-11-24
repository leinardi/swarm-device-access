---
name: adversarial-review
description: >
  Adversarial code review of changes to swarm-device-access — working tree, staged
  diff, a branch vs master, a commit range, or a PR — in any language (Rust, bash,
  YAML, Dockerfile, Makefile, docs). Uses user-level Rust skills for Rust and loads
  trust-boundary for config, policy, label, launcher and deployment paths, hunts for
  real defects, device-access widening and drift from the 1.x contract, checks the
  reuse ledger in docs/internal/rust-port.md, then reports ranked findings with a verdict. Use when
  the user asks to review changes, a diff, a PR or a branch, to "check my work
  before committing", whether it "is ready to merge", or to "poke holes in this" —
  even if they don't name a language or say the word "review".
---

# Adversarial Review — swarm-device-access

You are a hostile reviewer. Assume the change is **wrong until proven right**: it hides a
bug, breaks an invariant, or drifts from a contract. Your job is to find the specific
input, state, or path where it fails — not to praise it, not to restyle it. A review that
finds nothing is only credible after you have actively tried to break the code and failed.

This skill routes to the domain skills rather than replacing them: they own the rules. Read
`AGENTS.md` first: its coding rules and invariants are the review's floor.

## Scope

When invoked directly: review the scope the user gives (`gh pr view <n>` for intent, then `gh pr
diff <n>`); with none, the uncommitted work (`git diff HEAD` plus untracked files, which no diff
shows); on a clean tree, `git diff master...HEAD`. Never review from a description of the change.

Read the stated **intent** (`git log --oneline` for the range, any linked issue or PR body): a
change that works but does something other than what it claims is a finding, and so is a commit
type that ships the wrong version (*Release bump* below): read every commit subject in the range,
not only the PR title. Read every changed file in full, not just the hunks, and for non-trivial
changes the callers, implementations, tests and docs of what changed, found with a reference
search, not assumed from the diff: a signature or behavior change is only safe if every call
site agrees.

## Authority (path → domain skill)

For each changed path, load the matching skill **before** judging that file — the skill is
the source of truth for the rules, and violations there are findings even when lint is
green. Load only what the diff touches.

| Changed path | Load skill | It owns |
| --- | --- | --- |
| any `**/*.rs`, `Cargo.toml` (any member), `Cargo.lock`, `clippy.toml`, `rustfmt.toml`, `rust-toolchain.toml`, `deny.toml` | user-level Rust skills, `AGENTS.md`, and `docs/internal/rust-port.md` (decisions, crates, reuse ledger) | the lint set, `#[expect]`, the single unsafe module, error and log text as the 1.x contract, the Docker read/write split, the blocking transaction, Go test provenance, the reuse ledger |
| `src/config/**`, `src/policy/**`, `src/processor/**` (label handling, rule collection), the CLI and `SIGHUP` reload path (`src/cli.rs`, `src/cli/settings.rs`, `src/run.rs`), `src/launcher/**`, the HTTP server (`src/server.rs`, `src/server/**`), `deployments/**` (including `deployments/docker/Dockerfile`), `examples/**` | `trust-boundary` | fail-closed config and mode, label parsing, `/dev`-only rules, reload safety, image privilege, listeners |
| `src/cgroup/**` | user-level Rust skills, `AGENTS.md` + the note below | NVIDIA-derived BPF/cgroup code; `src/cgroup/bpf_sys.rs` is the one audited `unsafe` module |
| `meta/**`, `testenv/**` | `AGENTS.md`; the comment heading each test | a meta-test that can skip or pass on nothing is a finding; the integration harness's image identity check |
| `tests/testdata/**` | `docs/internal/rust-port.md` (1.x fixtures) | frozen 1.x captures: an edited or regenerated one is a finding unless it comes from running the 1.x code |
| `scripts/check-*.sh` | the script's header comment | the check still fails on what it exists to stop |
| `.pre-commit-config.yaml`, `droast.toml`, `[workspace.lints]`, `clippy.toml` | `AGENTS.md` (coding rules); `meta/tests/toolchain.rs`, `meta/tests/vacuous_flags.rs`, `meta/tests/ci_gates.rs`, `meta/tests/dockerfiles.rs`, `meta/tests/workspace_lints.rs` | no lint disabled or narrowed, no hook dropped; *A new suppression has to show its work* below |

**`src/cgroup/{api,ebpf,v1,v2}.rs` port NVIDIA-derived code** (from libnvidia-container, Apache
2.0). Touch it with care. They keep NVIDIA's block verbatim, the crun and runc provenance
paragraph and a modification notice under it; a diff that reformats or relicenses that block is a
finding, whatever else it does.

A meta-test under `meta/tests` guards a claim nothing else checks: the Rust release, zig,
cargo-zigbuild, `rust-std` and ShellCheck pins, Trivy never pinned, the Dockerfile's five stages
and the `prebuilt` target's binary, the CI gates, the
workspace lints, the Docker read/write boundary. Try to make the guarded thing go wrong and see
whether the test still notices. A test that passes because its input is empty, its rule never ran
or its negative control is missing is a false pass, and so is one whose scratch repository runs git
without `testenv_core::gitenv::isolate` (repo-meta's `ScratchRepo` does), which under the commit
hook acts on the real index.

No skill matches (bash, Makefile, `.mk/**`, plain YAML, Markdown, workflows)? Fall back to the
passes and the invariants below. **Same rigor** — an unmatched language is not a lighter
review.

## Repository invariants (check on every review, whatever changed)

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
- **The published docs are the user-facing source of truth.** The flag tables, the config keys, the
  container-label table and the docker-compose snippet in `docs/` must stay in sync with
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
  a numbered Q entry in the *Migrating from 1.x* table (`docs/migration.md`) is a defect: a log message,
  attribute or level, an error text (including Go's `%w` chain and `errors.Join` newlines), an
  exit code, a metric family or label, an Engine API request, a default. A new difference that
  cannot be avoided adds its Q entry in the same commit. A ported test that lost its `Go:`
  provenance comment, or a golden derived from reasoning instead of a 1.x capture, is a finding.
- **No `unsafe` outside the bpf syscall module.** The workspace denies `unsafe_code`; only
  `src/cgroup/bpf_sys.rs` lifts it, with a file-level `#![expect(unsafe_code, reason = ...)]`,
  and nothing else enforces that. A second module with `unsafe`, `libc` outside `bpf_sys.rs` or an
  `unsafe` block without a `// SAFETY:` comment is a finding.
- **No write Docker call outside the launcher.** The daemon path (processor, coordinator, event
  loop, observability) takes only the read-only Docker trait, whose client sends `GET` plus the
  one `HEAD /_ping` through one request builder. Create, start, attach, wait, stop and remove live
  only in `src/launcher/engine.rs`, and only `src/launcher/**` may name it. A write verb reachable
  from the daemon, a second request builder, or bollard outside `testenv` is a **critical**
  finding.
- **No new capability, path or listener.** The daemon and launcher get exactly the privileges, host
  paths, mounts and listeners 1.x had (`daemon_spec` in `src/launcher/spec.rs`,
  `docs/deployment.md`'s runtime requirements). One that appears without `docs/deployment.md` and
  `AGENTS.md` changing in the same commit is a finding; `--debug-addr` with a non-empty value is
  refused (Q2), never served.
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
- **One implementation per feature, with its provenance.** `docs/internal/rust-port.md` is the reuse
  ledger to check and update when a reused component changes or an upstream fix is taken. A
  component the ledger maps to SSE or SRD code is adapted from that code at the pinned commit, not
  rewritten independently; a change to it, or an upstream commit taken into it, updates its ledger
  row in the same commit, and a fix worth sending back goes on the ledger's upstream-candidates
  list. No equivalent feature is implemented twice without a technical reason recorded in
  `docs/internal/rust-port.md`. A reused file carries this project's Apache-2.0 header followed by the
  provenance line (`Adapted from <repo>/<path> at <commit>, originally MIT-licensed, Copyright (c)
  <year> Roberto Leinardi`); a pasted MIT block, a missing provenance line, a reused component
  changed without its ledger row, or a missing row is a finding.

## Adversarial passes

Beyond the generic passes (logic, nil and empty input, aliasing, error handling, concurrency,
resources, security), run these with a "how would I make this fail" framing, and name one
concrete failing input per candidate rather than asserting "looks fine".

- **Contract drift:** a public signature, flag, config key, label, metric, output format or error
  text changed without updating every consumer and the docs (*Docs drift* below).
- **Tests:** a test that passes against the *old* code, that asserts on a fake's recorded calls
  instead of the behavior they produced, or that was weakened or deleted to make the change pass
  is a finding. A bug fix with no regression test is a gap worth flagging.

## Always-on passes

The passes above are shaped by the diff. These run on **every** review, whatever changed.

### (a) Trust boundary

If the Authority table routed any changed path to `trust-boundary`, walk its items against the
diff. Anywhere else, ask the one question it is built on: does this change create a new place
where something from outside the daemon becomes something it believes — a Docker label that
becomes a device rule, a config value or CLI flag that becomes a mode or a policy, a mount
source that becomes a major/minor pair? If it does, the checklist applies there too.

### (b) Deletion smell

A diff that removes a user-visible surface — a flag, a config key, a label, a metric, a log line
operators rely on, a documented behavior — and in the same breath rewrites that surface's test to
assert it is *absent* must cite the specification line that retired it. The specification here is
the published pages under `docs/` (flags, labels, compose snippet, security notes) and `docs/**`
(`docs/architecture.md`, `docs/testing.md`). A commit message is not a specification, and docs that
still describe the surface mean the removal is unspecified.

A test flipped from "X happens" to "X does not happen" is not evidence that X should go — it is
the deletion wearing the test's clothes. Ask, in order: which spec line retires this surface, and
does it change in this diff? If none, this is a **critical** finding whatever the diff's stated
intent was. If one exists, is the diff removing exactly what that line retires and no more?

### (c) A new suppression has to show its work

Any new `#[expect(...)]` or `#[allow(...)]` (`#[allow]` is denied outright),
`# shellcheck disable=` or `# droast ignore=` is a standing decision to let a linter stay
silent. Nothing counts them, so the reason is the only record. Demand the attempt: for
`too_many_lines`, was the obvious extraction tried and what broke? For a cast lint, why is
`try_from` wrong here? For `min_ident_chars`, why does the name have to be short? A suppression
whose reason restates the lint instead of explaining why the fix does not apply is a finding, and
so is a suppression with no reason at all. The same holds for a lint lowered in
`[workspace.lints]` or a setting added to `clippy.toml` — and a setting there that matches nothing
in this repo (copied from another project) is a finding too.

### (d) Cross-file duplication

Before accepting a new helper, search for the one that already exists in the workspace (`src/**`,
`meta/**`, `testenv/**`) and in the rust-common crates it depends on, by *behavior*, not by the name
the author chose. Shared code is changed in rust-common, never forked here. Check the logger, the
`/dev` mount check, the injected clock, backoff, the metrics recorder and its no-op form, test waits
and the in-tree `text` and `lexpath` helpers. The reuse ledger in `docs/internal/rust-port.md` lists
the SSE and SRD code a component must start from. Two implementations of the same rule drift apart,
and the one the reviewer did not read is the one that keeps the bug.

### (e) Docs drift for config and flags

The configuration contract is written down more than once and can disagree silently. A new or
changed flag in `src/cli.rs`, key in `src/config/mod.rs` (`FileSchema`) or label in
`src/policy/mod.rs` needs its row in the matching table in `docs/` (the flag table of `docs/configuration.md`
and `docs/labels.md`), any affected prose in `docs/architecture.md` (Policy model,
Runtime contract), and validation that rejects bad values by name at load.

A surface the code has and the docs do not mention is a finding; so is a documented one nothing
implements, and so is a default in the docs that differs from the code.

## Gates

Do not re-run the repository's gates (`make verify`, `make check`, `make rust-deny`, `make
test-integration`, `make image-build-test`, `make docker-build`, `cargo test -p meta`, `pre-commit
run actionlint --all-files`). The implementer is expected to run them and CI runs them again, but do
not assume either happened: report the verification evidence you actually have (under
adversarial-review-loop, `verification.txt` when supplied, which is the implementer's claim and not
proof; when invoked directly, the CI status or gate output in the PR or session) and what is
missing, such as a daemon, processor, cgroup, launcher or `testenv` change with no integration run
reported, or a Dockerfile change with no image build. A failure in that evidence caused by the
change is a confirmed finding. Never say that a check you did not see passed, and a skipped test
(`SKIP` in `-v` output) is not a pass. Judge whether the change's tests prove it: a behaviour change
needs a test that fails on the old code. When invoked directly, one focused test (`cargo test
--locked --lib <module>::tests::<name>`) may be run to confirm a single suspected finding, never a
full suite or gate. A candidate that is neither reproduced nor traced end to end is dropped: a vague
"consider" is not a finding.

Facts the evidence has to be read against:

- The integration suite (`make test-integration`) drives the binary and the image it builds
  (`SDA_RUST_TEST_BINARY`, `SDA_RUST_TEST_IMAGE`), not the code under review: a raw
  `cargo test -p testenv --features integration` without a fresh `make integration-image` tests a
  stale image. Evidence of it should show that the binary and the image under test are Rust before
  any test ran.
- `SDA_IT_ENFORCE=1` attaches BPF and runs `systemctl daemon-reload`: it belongs only on a
  throwaway host.
- Which gates a changed path owes is `AGENTS.md`'s *Validation matrix*: a changed path with none
  of its rows in the evidence is missing evidence. A gate that was impractical (no Docker, no
  privileges for the integration suite, no network for cargo-deny) is unverified, not passed.
- A change to `release.yaml` or `ci.yaml`'s `image` job must leave `docs/internal/release.md` ("What
  CI proves") still true.

## Report

Rank by severity, worst first. Nothing is more important than a genuine correctness break or
an unbacked device grant: those are normally **critical**. Skip pure formatting the linters
already catch unless it changes meaning or breaks a required gate. For each finding:

```text
<path>:<line> — <severity: critical | high | medium | low>: <one-line defect>
  Failure: <the concrete input/state → the wrong result or broken invariant>
  Fix: <the specific change>
```

Findings first, then open questions or assumptions, then a one-line verdict: **block**,
**approve with nits**, or **approve**, and one line: `Verification evidence: seen … / missing …`.
If you found nothing, state what you tried to break so the "no findings" is
credible. Be blunt; do not soften a real defect to be polite, and do not invent findings to look
thorough.
