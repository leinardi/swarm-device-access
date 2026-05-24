# Suggested Commands

## Build & run

```bash
make go-build        # cross-compile to Linux → dist/swarm-device-access (works from macOS/Win)
make go-run          # go run with ARGS="-log-level debug" default
make docker-build    # build Docker image
make docker-run      # run image locally with required host bind mounts
```

## Test

```bash
make go-test                                                          # CGO_ENABLED=1 go test -race ./...
make go-test-cover                                                    # with coverage → coverage.out
go test ./internal/systemd -run TestIsReloadCompleted -v              # single test
# On macOS/Windows (tests are //go:build linux):
docker run --rm -v $PWD:/work -w /work golang:1.26 go test ./...
```

## Quality

```bash
make go-vet
make go-tidy          # go mod tidy + go mod verify
make go-fmt           # gofmt + goimports
make go-fmt-check     # fail if formatting needed
make check            # pre-commit on all files
make check-stage      # pre-commit on staged files only
make mk-common-update # refresh shared .mk snippets
```
