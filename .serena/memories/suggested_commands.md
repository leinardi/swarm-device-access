# Suggested Commands

## Build & run

```bash
make rust-build         # cargo build --workspace --all-targets --all-features --locked (stamped)
make rust-release-dist  # static musl amd64/arm64, then checksums.txt, into dist/ (pinned zig)
make image-build-test   # prebuilt image from dist/, then its full tier (IMAGE_PLATFORM, IMAGE_TEST_TIER)
make docker-build       # build Docker image's default target (needs docker login dhi.io)
make docker-run         # run image locally with required host bind mounts
```

## Test

```bash
make rust-test                                   # cargo test --workspace --locked (meta included)
cargo test --locked --lib systemd::tests         # one module's unit tests
pre-commit run lychee --all-files               # docs relative links
make test-integration                            # testenv suite vs the Dockerfile image (Docker)
make test-integration SDA_IT_ENFORCE=1           # plus enforcement; throwaway hosts only
make sweep-test-leaks                            # remove leftovers of killed integration runs
```

## Quality

```bash
make verify           # build, fmt check, clippy -D warnings, tests
make rust-fmt         # cargo fmt --all
make rust-clippy
make rust-deny        # cargo deny --workspace check (network)
make check            # pre-commit on all files
make check-stage      # pre-commit on staged files only
make mk-common-update # refresh shared .mk snippets
make help             # every target
```
