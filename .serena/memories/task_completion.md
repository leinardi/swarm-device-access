# Task Completion Checklist

Run these after any code change before committing:

```bash
make go-fmt-check   # formatting (gofmt + goimports)
make go-vet         # static checks
make go-test        # tests with race detector (CGO_ENABLED=1)
make go-tidy        # go.mod/go.sum consistent
make check-stage    # pre-commit hooks on staged files (catches lint, trailing whitespace, etc.)
```

Or run `make check` to run pre-commit on all files (slower but thorough).

Note: tests are `//go:build linux`. On non-Linux hosts, run them inside the golang Docker image:

```bash
docker run --rm -v $PWD:/work -w /work golang:1.26 go test ./...
```
