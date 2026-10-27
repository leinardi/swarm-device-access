# Task Completion Checklist

Run these after any code change before committing:

```bash
make verify         # rust-build, rust-fmt-check, rust-clippy (-D warnings), rust-test
make check-stage    # pre-commit hooks on staged files (license headers, suppressions, docs links, ...)
```

Also, by what changed (full table: AGENTS.md, Validation matrix):

- a Cargo manifest, `Cargo.lock` or `deny.toml`: `make rust-deny` (network).
- `src/{daemon,processor,cgroup,launcher}/**`, `testenv/**`: `make test-integration` (Docker).
- Markdown: `cargo test --locked -p meta --test docs`; prose wraps at 100 columns.
- a new suppression: `scripts/check-suppressions.sh --update`, commit the baseline.

Or run `make check` to run pre-commit on all files (slower but thorough).
