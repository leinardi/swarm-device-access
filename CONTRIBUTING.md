# Contributing

## Prerequisites

- Linux host: the daemon is Linux-only, and its tests run on a Linux kernel
- Rust through `rustup` (`rust-toolchain.toml` pins the exact release and its components)
- Docker, logged in to `dhi.io` (`docker login dhi.io`), for building the container image and
  running the integration suite
- [`pre-commit`](https://pre-commit.com/) — install hooks once with `make pre-commit-install`

## Building

```bash
make rust-build      # builds the workspace (target/debug/swarm-device-access)
make docker-build    # builds the container image for the host's architecture (single-arch)
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
the release `.github/actions/setup-shellcheck` pins.

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
- Keep diffs against the upstream (`NVIDIA/libnvidia-container` `src/nvcgo/internal/cgroup/`)
  minimal.
- Before proposing a change, check whether the upstream already fixed it.
- If you fix a bug that also exists upstream, note the upstream file and line in your PR
  description, and consider sending a patch upstream.

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
[`docs/release.md`](docs/release.md).

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
# Start a container that bind-mounts a device
docker run --rm -v /dev/null:/dev/null alpine sh -c 'echo ok > /dev/null && echo "device access works"'

# Watch the daemon's logs for the apply sequence
docker logs swarm-device-access 2>&1 | grep -E 'device mount detected|setting device rule'
```

For GPU passthrough, bind-mount `/dev/nvidia0` and run `nvidia-smi` inside the container.
