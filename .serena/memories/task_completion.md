# Task Completion Checklist

Run these after any code change before committing:

```bash
make verify         # rust-build, rust-fmt-check, rust-clippy (-D warnings), rust-test
make check-stage    # pre-commit hooks on staged files (license headers, docs links, ...)
```

Also, by what changed (full table: AGENTS.md, Validation matrix):

- a Cargo manifest, `Cargo.lock` or `deny.toml`: `make rust-deny` (network).
- `src/{daemon,processor,cgroup,launcher}/**`, `testenv/**`: `make test-integration` (Docker).
- the Dockerfile, `.mk/rust-release.mk`, `.mk/image.mk`, `scripts/image-test.sh`:
  `make rust-release-binaries image-build-prebuilt image-test` (zig, `dhi.io` login), and the arm64
  reduced tier under QEMU.
- `.github/**`: `cargo test --locked -p meta`, `pre-commit run actionlint --all-files`; keep
  `docs/release.md` true.
- Markdown: `pre-commit run lychee --all-files`; prose wraps at 100 columns.

Or run `make check` to run pre-commit on all files (slower but thorough).
