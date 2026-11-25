# Contributing

## Prerequisites

- Linux host: the daemon is Linux-only, and its tests run on a Linux kernel
- Rust through `rustup` (`rust-toolchain.toml` pins the exact release and its components)
- Docker, logged in to `dhi.io` (`docker login dhi.io`), for building the container image and
  running the integration suite: the image builds from Docker Hardened Images
- [`pre-commit`](https://pre-commit.com/) — install hooks once with `make pre-commit-install`
- `cargo-deny`, [droast](https://github.com/immanuwell/dockerfile-roast), `markdownlint-cli2`,
  `yamllint`, `shellcheck`, `actionlint` and `checkmake`. The pre-commit hooks install most of
  these when needed; droast is built from source, which needs `cargo`
- A local `shellcheck` at the release the `shellcheck-py` hook in `.pre-commit-config.yaml` wraps:
  droast's required ShellCheck bridge runs the one on PATH
- For the static release binaries only (`make rust-release-dist`): the zig and cargo-zigbuild
  releases the Makefile pins, on PATH; QEMU (binfmt) to run the arm64 image tests on another
  architecture
- For the documentation site only: [uv](https://docs.astral.sh/uv/), which runs the Zensical
  pinned in `tools/docs/uv.lock`

The Makefile pulls shared logic from [`leinardi/make-common`](https://github.com/leinardi/make-common),
pinned by `.mk/.mk-common-version`. On the first `make` run, the bootstrap script downloads the
snippets into `.mk/`; `make help` lists every target.

## Building

```bash
make rust-build        # builds the workspace (target/debug/swarm-device-access)
make docker-build      # the container image's default target, which compiles the binary (host arch)
make rust-release-dist # the static release binaries, then checksums.txt, into dist/
make image-build-test  # the prebuilt image from those binaries, then its tests
```

A multi-arch image build of the default target:

```bash
docker buildx build \
  --platform linux/amd64,linux/arm64 \
  -f deployments/docker/Dockerfile \
  --build-arg VERSION=dev \
  --build-arg COMMIT=$(git rev-parse --short HEAD) \
  --build-arg DATE=$(date -u +%Y-%m-%dT%H:%M:%SZ) \
  -t ghcr.io/leinardi/swarm-device-access:dev .
```

## Documentation

The published pages are in `docs/` (`docs/internal/` is never published), built with Zensical
into the site at <https://leinardi.github.io/swarm-device-access/>:

```bash
make docs-serve        # preview the site with live reload
make docs-build        # the strict build CI runs on every pull request
```

## Testing

```bash
make verify            # build, format check, clippy and every test, the meta-tests included
make test-integration  # the integration suite against the image (Docker; see docs/testing.md)
```

## Linting

```bash
make check           # runs the full pre-commit suite (clippy, droast, shellcheck, etc.)
make check-stage     # same, but only on staged files
```

CI runs the same checks on every pull request in `.github/workflows/ci.yaml`.

The Dockerfile is linted by [droast](https://github.com/immanuwell/dockerfile-roast) (Dockerfile
Roast) with the policy in `droast.toml`. pre-commit builds droast from source, so `make check`
needs a Rust toolchain (`cargo`); its required ShellCheck bridge runs the `shellcheck` on PATH, at
the release the `shellcheck-py` hook in `.pre-commit-config.yaml` wraps (CI installs it with
gh-composite-actions' `setup-shellcheck`).

## Branch and PR conventions

- Branch names: `feat/<short-description>`, `fix/<short-description>`, `chore/<short-description>`.
- Keep PRs focused; one logical change per PR.
- PR titles should be short and in the imperative mood: _Add metrics endpoint_, _Fix scanner.Err
  propagation_.
- All CI checks must pass before merge.

## `src/cgroup/` — NVIDIA-derived code

`src/cgroup/{api,ebpf,v1,v2}.rs` port NVIDIA's files and carry a
`Copyright (c) 2021, NVIDIA CORPORATION` header.

**Etiquette:**

- Do not remove or modify the NVIDIA copyright header.
- Do not relicense this code.
- The files are ports of this repository's 1.x Go code (`internal/cgroup/`, in the history
  before the 2.0 cutover), which carries the same header; keep their structure close to it.
- Before proposing a change, check whether the upstream already fixed it.
- If you fix a bug that also exists upstream, note the upstream file and line in your PR
  description, and consider sending a patch upstream.

## Reused code and the 1.x contract

Several Rust modules are adapted from swarm-scheduler-exporter and swarm-rootless-docker. The reuse
ledger in [`docs/internal/rust-port.md`](docs/internal/rust-port.md) names the source and pinned
commit of each one and every adaptation: check it before changing a reused component, and update its
row in the same commit when a reused component changes or an upstream fix is taken.

Log lines, error texts, exit codes, metrics and Docker Engine requests are those of 1.x. A change
that cannot keep one adds a numbered entry to the
[Migrating from 1.x](docs/migration.md) table in the same commit. `AGENTS.md` lists the
project skills (`trust-boundary`, `adversarial-review`) that hold the fail-closed and review rules.
General Rust coding guidance lives in the maintainer's user-level skills.

## Commit messages

All commits must follow [Conventional Commits 1.0.0](https://www.conventionalcommits.org/en/v1.0.0/)
with a scope: `<type>(<scope>)[!]: <description>`. The `conventional-pre-commit` hook enforces this
on `commit-msg`, so install the hooks with `make pre-commit-install` (it installs both the
`pre-commit` and `commit-msg` hooks). Release notes are not generated from these messages: the
release workflow runs `gh release create --generate-notes`, which lists the merged pull requests by
title, so write PR titles for readers of the release notes.

Common types: `feat`, `fix`, `docs`, `test`, `refactor`, `perf`, `build`, `ci`, `chore`, `style`,
`revert`. Use a lower-case, imperative description:

```
fix(processor): skip unresolvable symlinks under device mounts
ci(dependabot): authenticate docker updates against dhi.io
feat(policy)!: drop the legacy allow label
```

Mark breaking changes with `!` before the colon or a `BREAKING CHANGE: <description>` footer.

The release version is derived from these types: since the last release, any `feat` makes the next
release a minor, any `fix` a patch, and `!` or a `BREAKING CHANGE:` footer a major; `build`,
`chore`, `ci`, `docs`, `perf`, `refactor`, `revert`, `style` and `test` bump nothing. See
[`docs/internal/release.md`](docs/internal/release.md).

Pick the type by whether the change should ship, not by what kind of change it is. A performance
improvement, a refactor or a revert that changes the shipped binary or image and that users should
receive is a `fix` (or a `feat`). Use `perf`, `refactor`, `style` and `revert` only when the commit
is deliberately not meant to trigger a release on its own.

Pull requests are merged with merge commits; squash and rebase merging are disabled. Every commit in
a pull request therefore lands on `master` as it is and counts toward the version, so each commit
needs a correct type, not just the pull request as a whole. The `conventional-commits` CI job checks
every one of them. Enabling squash merging would make the pull-request title the commit subject
instead, and would need a CI check on pull-request titles first.

## Verification

After any change to the device-apply path, verify on a live host:

```bash
# Start an opted-in container that bind-mounts a device outside Docker's default allowlist
docker run --rm --label swarm-device-access.enable=true -v /dev/ttyS0:/dev/ttyS0 alpine \
  sh -c 'sleep 1 && echo x > /dev/ttyS0 && echo "device access works"'

# With the daemon at --log-level=debug, watch its logs for the apply sequence
docker logs swarm-device-access 2>&1 | grep -E 'device mount detected|setting device rule'
```

For GPU passthrough, bind-mount `/dev/nvidia0` and run `nvidia-smi` inside the container.
