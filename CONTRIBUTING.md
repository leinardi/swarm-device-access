# Contributing

## Prerequisites

- Linux host (all device/cgroup/BPF code carries `//go:build linux`; building for Linux from macOS/Windows works via cross-compilation but tests require a Linux runner)
- Go 1.26+
- Docker (for running Linux tests from a non-Linux host and for building the container image)
- [`pre-commit`](https://pre-commit.com/) — install hooks once with `make pre-commit-install`

## Building

```bash
make go-build        # cross-compiles to linux/amd64 into ./dist/
make docker-build    # builds the multi-arch container image
```

## Testing

```bash
make go-test         # go test -race ./... (requires Linux; see below)
```

Tests carry `//go:build linux` and must run on a Linux kernel. From macOS or Windows:

```bash
docker run --rm -v "$PWD:/work" -w /work golang:1.26 go test -race ./...
```

## Linting

```bash
make check           # runs the full pre-commit suite (golangci-lint, hadolint, shellcheck, etc.)
make check-stage     # same, but only on staged files
```

CI runs the same checks on every pull request via `golangci-lint-full` in `.github/workflows/ci.yaml`.

## Branch and PR conventions

- Branch names: `feat/<short-description>`, `fix/<short-description>`, `chore/<short-description>`.
- Keep PRs focused; one logical change per PR.
- PR titles should be short and in the imperative mood: _Add metrics endpoint_, _Fix scanner.Err propagation_.
- All CI checks must pass before merge.

## `internal/cgroup/` — NVIDIA-derived code

The files in `internal/cgroup/` carry a `Copyright (c) 2021, NVIDIA CORPORATION` header.

**Etiquette:**

- Do not remove or modify the NVIDIA copyright header.
- Do not relicense this code.
- Keep diffs against the upstream (`NVIDIA/libnvidia-container` `src/nvcgo/internal/cgroup/`) minimal.
- Before proposing a change, check whether the upstream already fixed it. See [`UPSTREAM_AUDIT.md`](UPSTREAM_AUDIT.md) for the current audit state.
- If you fix a bug that also exists upstream, note the upstream file and line in your PR description, and consider sending a patch upstream.
- If golangci-lint flags something in this directory, check whether the linter exclusion in `.golangci.yaml` covers it before adding a `//nolint` directive.

## Commit messages

No conventional-commit prefix is required. Start with a capitalized verb in the imperative mood:

```
Fix scanner.Err propagation in /proc cgroup parsers
Add govulncheck workflow
Replace fmt.Sprintf proc path pattern with strconv.Itoa
```

## Verification

After any change to the device-apply path, verify on a live host:

```bash
# Start a container that bind-mounts a device
docker run --rm -v /dev/null:/dev/null alpine sh -c 'echo ok > /dev/null && echo "device access works"'

# Watch the daemon's logs for the apply sequence
docker logs <daemon-container> 2>&1 | grep -E 'device mount detected|adding device rule'
```

For GPU passthrough, bind-mount `/dev/nvidia0` and run `nvidia-smi` inside the container.
