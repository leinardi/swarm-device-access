# Documentation restructure report

Stage 1 of the documentation rework: `docs/` becomes a published set plus an unpublished
`docs/internal/`, and the README's reference material becomes published pages. The plan is
[`plan.toml`](plan.toml). Status: **plan approved by the owner and applied; 63 mappings and 4 GitHub
losses await the owner's review**.

## Baselines

- Base commit: `2f1f26a2deaf6611a24b637aed2a280ecfcb0b76` (`origin/master`, fetched
  2026-11-24), clean tree.
- `blocks.json`: the content baseline of every Markdown file (22 files), from
  `docstool inventory --blocks`. It exited 0 with no findings.
- `urls.json`: source-derived, because the repository has no site yet. URL preservation can only
  be **partially verified**: GitHub paths and heading anchors are covered; built routes are not.
- `docstool check --site-planned`: 12 links that leave `docs/` (in `release.md`,
  `rust-port.md` and `testing.md`). Each is re-pointed to its GitHub URL after apply.

## Classification

| File or section | Class | Lifecycle | Page type | Cited by | Outcome |
| --- | --- | --- | --- | --- | --- |
| `docs/architecture.md` | published | living | explanation, troubleshooting | AGENTS.md, skills, Serena | stays |
| `docs/testing.md` | published | living | contributor guide | AGENTS.md, CONTRIBUTING, README, skills | stays; *Field test* splits out |
| `docs/testing.md`, *Field test* | internal | record (2026-10-27) | QA record | `scripts/field-test.sh` | splits to `internal/qa/field-test.md` (d4) |
| `docs/release.md` | internal | living | runbook | workflows, Dependabot, Dockerfile, meta-test, AGENTS.md, skills | moves (d2) |
| `docs/release.md`, Go 1.x notes | internal | record | history | none | splits to `internal/history/` (d2) |
| `docs/rust-port.md` | internal | living record | port and provenance ledger | 12 code and test comments, Cargo.toml, testdata READMEs, AGENTS.md, skills, Serena | moves (d1) |
| README *Alternatives* | published | living | explanation | none | new `alternatives.md` (d3) |
| README *Quick Start* intro, compose snippet, *Verifying Device Passthrough* | published | living | tutorial | trust-boundary skill (compose snippet) | new `getting-started.md` (d3) |
| README *Host Requirements* | published | living | reference, explanation | `architecture.md`, README | new `host-requirements.md` (d3) |
| README launcher prose and launch flags | published | living | reference | skills ("documented set") | new `deployment.md` (d3) |
| README *Configuration*, *Config File* | published | living | reference | AGENTS.md, skills | new `configuration.md` (d3) |
| README *Container Labels* | published | living | reference | AGENTS.md, trust-boundary skill | new `labels.md` (d3) |
| README *Migrating from 1.x* | published | living | reference | AGENTS.md, CONTRIBUTING, rust-port.md, 9 code comments, skills, Serena | new `migration.md` (d3) |
| README *Tips and Limitations* | published | living | how-to | none | new `tips.md` (d3) |
| README *Security* | published | living | explanation | SECURITY.md | new `security.md` (d3) |
| `release.md`, the verification commands | published copy | living | how-to | none | new `verifying-releases.md` (d2); the runbook keeps its copy |
| README intro, *What It Does*, *Development*, *License* | root | living | README |  | stay for stage 3 (repo-readme) |

## New layout

```text
docs/
  index.md               landing page (stage 2)
  getting-started.md     new: quick start, compose snippet, passthrough check
  deployment.md          new: the launcher, launch flags, updates and rollbacks
  host-requirements.md   new: kernel, cgroup v2 and v1 requirements, how grants are kept
  configuration.md       new: flags, config file, reload
  labels.md              new: container labels, globs, device identity
  security.md            new: security model (SECURITY.md stays the policy)
  tips.md                new: placement, stable names, hot-plug, FUSE
  alternatives.md        new: comparison with the usual workarounds
  migration.md           new: Migrating from 1.x (Q1 to Q18)
  verifying-releases.md  new: attestation and signature checks
  architecture.md
  testing.md
  internal/
    README.md            what lives here and why
    release.md           the maintainer runbook
    rust-port.md         the port's design and provenance record
    history/go-releases.md
    qa/field-test.md
    restructure/         this record
```

The README keeps its intro, *What It Does*, *Development* and *License*, and a list of links to
the new pages, until stage 3 rewrites it. The Q numbers keep their numbering: they are cited from
code.

## Owner decisions

All decisions were given by the owner on 2026-11-24, in answer to the gates. They are recorded
as `[[decision]]` entries in the plan.

- **d1 (G3, G10):** `docs/rust-port.md` moves to `docs/internal/rust-port.md`, and every
  citation is updated.
- **d2 (G3, G8, G10):** follow swarm-scheduler-exporter. The runbook moves, its Go notes go to
  `history/`, and a published `verifying-releases.md` is added.
- **d3 (G8, R4, G10):** the README's reference material moves to the published pages above in
  this stage, and every README citation is re-pointed.
- **d4 (G2, G10):** the *Field test* section goes to `internal/qa/`. At approval the owner asked
  for the maintainer's host name to be replaced by a generic one (`<node>`, "a test node").
- **d5:** contradicted claims are corrected in this stage, in the docs only, and each rewording is
  recorded in `mappings.toml`.
- **d6 (G10):** the stale comments in code and in the example config (rows 34 and 35) are fixed
  through `[[reference]]` edits. These are comment-only changes.
- **d7:** the docs drop "the client refuses engines older than 19.03 (API 1.40)". The minimum
  Engine version stays open (see *Not verifiable*).

## Claims the code contradicts

"Confirmed" rows were opened on both sides during this stage. "Audit" rows come from the
read-only audit and are confirmed again when their correction is written.

| # | Claim | Code | Status |
| --- | --- | --- | --- |
| 1 | README:76, architecture.md:470: the client refuses engines below 19.03 (API 1.40) | no floor: docker-client v3.0.0 `config.rs:388-402` and bollard's `negotiate_version` only lower the version | Confirmed |
| 2 | README:412: globs use Go's `filepath.Match` | `policy::glob` on the `glob` crate, with refusals (`src/policy/glob.rs`); the README's own Q17 says so | Confirmed |
| 3 | README:322: `dry-run`, `log-time` must be `true` or `false` in the file | YAML `True`/`TRUE`/`False`/`FALSE` also resolve (`src/config/resolve.rs:88-89`); only flags are `true`/`false` | Confirmed |
| 4 | README:336: pass `--config` "after the image name" | the launcher mounts `--config-dir`; `--config` is a daemon flag after `--` (`deployments/docker/docker-compose.yaml`) | Audit |
| 5 | README:631: debug lines read `level=DEBUG device mount detected id=… source=…` | tracing-subscriber text prints `DEBUG …`; fields `id pid source destination` (`src/processor/rules.rs:647`) | Audit |
| 6 | README:649, release.md:17: zig and cargo-zigbuild pins in `.mk/rust-release.mk` | `.mk/rust-release.mk` has `?=` defaults; the pins are `Makefile:58-68` | Audit |
| 7 | README:21, :688: `src/cgroup/` ported from NVIDIA's container toolkit, linked to `k8s-device-plugin` | only `api.rs`, `ebpf.rs`, `v1.rs`, `v2.rs` carry NVIDIA's header; the 1.x Go code (`internal/cgroup/ebpf.go:175`) names `NVIDIA/libnvidia-container` | Audit |
| 8 | README:498 (Q9): the error reads `pinned container process exited: no such process` | `…: No such process (os error 3)` (`src/processor/pin.rs:121-124`, `pin/tests.rs:53`) | Confirmed |
| 9 | README:458: the behaviours 2.0 dropped are Q2, Q3, Q5 | Q4, Q13 and Q14 also drop behaviour | Audit |
| 10 | rust-port.md:17, :95: "Q1 to Q17" | the table has Q18 | Confirmed |
| 11 | architecture.md:417: 5 s read-header, 10 s read, 30 s write, 60 s idle timeouts | 5 s header read, 30 s write stall, 5 s drain (`src/server.rs:59-65`); those are 1.x's (Q11) | Confirmed |
| 12 | architecture.md:260: `/dev` is opened once per pass | once per container reconcile with a `/dev` mount (`src/processor/reconcile.rs:532-538`) | Audit |
| 13 | architecture.md:294: every container with a `/dev` mount logs a summary | only enabled containers (`reconcile.rs:454`) | Audit |
| 14 | testing.md:9, :118: dry-run tests cover cgroup path resolution and `/proc` parsing | dry-run never runs `apply_pinned` (`reconcile.rs:430-431`) | Audit |
| 15 | testing.md:34: the integration workflow runs on every push | `integration.yaml`: pull requests and pushes to `master` | Confirmed |
| 16 | testing.md:48: a push to a fork's branch fails that check | only a fork's `master` push triggers it | Audit |
| 17 | testing.md:28: no variables means dry-run tests only | `TestIdentity_RefusesGoImage` runs too (`testenv/tests/integration/identity.rs:50`) | Audit |
| 18 | release.md:567: Go releases up to and including `v1.0.4` | `v1.0.4` contains the cutover `83e2080` and no `go.mod`; the last Go release is `v1.0.3` | Confirmed |
| 19 | release.md:90: other pushes skip only the `dependabot` job | `dependabot` runs; `guard`, `audit` and later jobs are skipped | Audit |
| 20 | release.md:147: the run has four jobs | five: `dependabot`, `guard`, `ci-green`, `audit`, `release` | Confirmed |
| 21 | release.md:473: the `release` environment is set up by hand | gh-leinardi-iac `stacks/github-repos/repos.tf:558-560` manages it | Audit |
| 22 | release.md:480: three places need `DHI_TOKEN` | `integration.yaml` also logs in to dhi.io: four | Audit |
| 23 | release.md:495: the `master` ruleset has no required check | gh-leinardi-iac `repos.tf:553` requires `ci-result` | Audit |
| 24 | rust-port.md:47: rustix covers `pidfd_send_signal`; a meta-test pins the `unsafe` list | the pidfd is polled (`src/processor/pin.rs:21,95`); `unsafe_allowlist.rs` was deleted in `b8a2139` | Confirmed |
| 25 | rust-port.md:49: zbus 6.1.0 | `Cargo.lock` locks 6.2.1; 6.1.0 is the requirement | Audit |
| 26 | rust-port.md:137-140, :168-169: `integration_targets.rs`, `license_headers.rs`, `suppressions.rs`, `unsafe_allowlist.rs`, `meta/src/*`, stamp and `rust-doc` checks | none exist in `meta/` now (`ls meta/tests`) | Confirmed |
| 27 | rust-port.md:167: the metrics registry is go-prom's | `prometheus-client` | Audit |
| 28 | rust-port.md:41: flags keep every Go `ParseBool` spelling | `value_parser!(bool)` (`src/cli.rs:96`) and Q17 | Confirmed |
| 29 | CONTRIBUTING.md:118: the `/dev/null` run shows device access works | no opt-in label, and `/dev/null` is in the runtime's allowlist anyway | Audit |
| 30 | CONTRIBUTING.md:121: `grep` shows the apply sequence | both lines are `debug!`; the default level is `info` | Audit |
| 31 | CONTRIBUTING.md:57: keep diffs against `libnvidia-container`'s `src/nvcgo/internal/cgroup/` minimal | the Rust files are ports of this repository's own Go code (`src/cgroup/api.rs:17`) | Audit |
| 32 | SECURITY.md:11: `cgroupns: host`, `userns: host` | the compose keys are `cgroup: host` and `userns_mode: host` | Audit |
| 33 | SECURITY.md:68, architecture.md:369: the capability set includes `CAP_PERFMON` | only `CGROUP_DEVICE` programs are loaded; nothing needs it. `CAP_BPF`, `CAP_SYS_ADMIN`, `CAP_SYS_RESOURCE` are supported by tests | Audit |
| 34 | `src/cli.rs:27`, `src/cli/tests.rs:118`: booleans take every `ParseBool` spelling | Q17; fixed by `[[reference]]` (d6) | Confirmed |
| 35 | `deployments/docker/config.yaml:7,12`: a `plain` log format and `debug-addr` "pprof endpoints" | Q13 dropped `plain`, Q2 the pprof server; fixed by `[[reference]]` (d6) | Confirmed |
| 36 | `testenv/src/identity.rs:20`, `enforce.rs:109`, `src/metrics/mod.rs:19`: Q1 and Q3 "in `docs/rust-port.md`" | the Q table is the README's; re-pointed to `docs/migration.md` (d6) | Confirmed |

`docs/rust-port.md` also names files that are gone: `tests/engine_wire.rs`,
`src/docker/{client,connect,pool,events,version,host,fake}.rs`, `src/launcher/{stdcopy,probe}.rs`,
`.github/actions/setup-zigbuild/`, `.github/scripts/binary-attestation-status.sh` and the CI jobs
`rust-static`, `release-scripts` and `rust-integration`. In the reuse ledger they are history and
stay as written; a status note is added where a row reads as current (G7).

## Not verifiable from the code

- The minimum Docker Engine version (d7). The launcher sets `HostConfig.CgroupnsMode`
  (`src/launcher/spec.rs:153`), which may need API 1.41; that is unverified.
  TODO(verify: create the daemon against an Engine 20.10 and a 19.03).
- External behaviour: Swarm rejecting `devices:`, labels copied into task containers, kernel
  versions of `openat2`, `pidfd_open` and `BPF_F_REPLACE`, runc, crun and systemd attach modes,
  FUSE and AppArmor, `/dev/ttyS0` outside Docker's default allowlist, udev names, moby, godbus
  and net/http error wording, and cosign bundle formats.
- `allfro/device-mapping-manager` activity and Engine 29 failures; moby/swarmkit#1244; issue #41.
- Measurements: the Go daemon's "about 17k lines", the field-test resident sets, the D-Bus spike
  observations.
- Runtime behaviour was not exercised: nothing was built or run for this audit.

## Drift coverage

`docstool drift --scope repo --paths --leads` (toolkit 0.2.0) exits 3: its `make` coverage is
partial (targets defined dynamically in `.mk/docker.mk` and `.mk/rust.mk`), and so is `env` (one
read through a variable, `testenv/src/host.rs`). Every make target the docs name was checked by
hand in `.mk/*.mk`; the leads it raised were triaged:

- `CAP_BPF`, `CAP_PERFMON` (SECURITY.md:68): row 33.
- `src/nvcgo/internal/cgroup/` (CONTRIBUTING.md:57): an upstream path, not this repository's; row 31.
- `testdata/` (AGENTS.md:272): a directory pattern (`tests/testdata`, `testenv/testdata`), correct.
- `daemon.json`, `encoding/json`, and `owner/repo` names: not paths.
- "last changed N days before" leads for CONTRIBUTING.md and SECURITY.md: reviewed in the audit.

## Work after apply

- Write the new pages from the README sections (each block in `mappings.toml` as `rewritten`),
  with the corrections above, and replace the README sections with links.
- Write `verifying-releases.md` and `docs/internal/README.md`.
- Re-point the links into the README and out of `docs/` (`architecture.md:242,414`,
  `testing.md:163`, `internal/rust-port.md`, the 12 `--site-planned` links).
- Rewrap the 17 lines the dry run lists as longer than 100 columns.
- Record the README anchors that no longer exist as `[[github_loss]]` entries for review.

## After apply

- The machine baselines (`blocks.json`, `urls.json`, `apply-log.json`) served only this run and
  are not committed: `blocks.json` (517 KB) is over the repository's 500 KB file limit. The plan,
  this report, the mappings and their review receipts are the record.

- `plan apply --write` wrote 42 paths. The README went from 690 to 116 lines; its sections are
  now the pages of the new layout.
- **`deployments/docker/config.yaml` was not changed, contrary to d6.** `tests/config_loader.rs`
  (`shipped_example_loads`) compares the file byte for byte with the frozen 1.x corpus, so the two
  comment fixes failed `make verify` and were reverted. The file still lists a `plain` log format
  (Q13) and calls `debug-addr` "pprof endpoints" (Q2). Fixing them means changing the corpus, which
  AGENTS.md forbids by hand: an owner decision. Status (2026-11-25): resolved without touching the
  corpus. The test now compares only the lines that are not comments, and the file's comments
  describe 2.x (owner).
- **G6, settled by the owner:** the README and its license note linked "NVIDIA's container
  toolkit" to `NVIDIA/k8s-device-plugin`; CONTRIBUTING.md and SECURITY.md named
  `NVIDIA/libnvidia-container`. The code names no upstream repository, but
  `NVIDIA/libnvidia-container/src/nvcgo/internal/cgroup/` holds exactly `api.go`, `ebpf.go`,
  `v1.go` and `v2.go`, and `k8s-device-plugin` has no `BPF_CGROUP_DEVICE` code. Both links now
  name libnvidia-container (owner, 2026-11-24).
- Found while editing, not fixed (outside the plan): `.github/workflows/release.yaml:258-260`
  says the `release` environment is created on first run and its branch rule is configured by
  hand, but gh-leinardi-iac manages it (row 21). Status (2026-11-25): the comment now says so.
- `drift --scope docs --paths` still reports about 150 paths in `internal/rust-port.md`: Go files,
  removed modules and earlier test names in the ledger's history rows. The ledger records them on
  purpose (G7); its current-state sentences were corrected (rows 24 to 28). Published pages have
  only false positives (`daemon.json`, `checksums.txt`, `allfro/…`, `cilium/ebpf/asm`,
  `encoding/json`).
