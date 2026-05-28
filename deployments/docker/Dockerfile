# syntax=docker/dockerfile:1.7

########################
# deps: cache modules
########################
FROM --platform=$BUILDPLATFORM dhi.io/golang:1-alpine3.23-dev AS deps
WORKDIR /src
RUN --mount=type=cache,target=/var/cache/apk \
	apk add --no-cache git
COPY go.mod go.sum ./
# Use a named cache so we can reuse it in the build stage (no COPY needed)
RUN --mount=type=cache,target=/go/pkg/mod,id=gomodcache \
	go mod download

########################
# build: compile static binary
########################
FROM --platform=$BUILDPLATFORM dhi.io/golang:1-alpine3.23-dev AS build
ARG VERSION="dev"
ARG COMMIT="none"
ARG DATE="unknown"

# Buildx provides these automatically for multi-arch builds
ARG TARGETOS
ARG TARGETARCH

ENV CGO_ENABLED=0 \
    GOOS=${TARGETOS} \
    GOARCH=${TARGETARCH}

WORKDIR /src

# Bring in source
COPY . .

# Reuse the same module & build caches (BuildKit)
RUN --mount=type=cache,target=/go/pkg/mod,id=gomodcache \
	--mount=type=cache,target=/root/.cache/go-build \
	go build -trimpath -buildvcs=false \
		-ldflags="-s -w -X 'main.version=${VERSION}' -X 'main.commit=${COMMIT}' -X 'main.date=${DATE}'" \
		-o /out/swarm-device-access ./cmd/swarm-device-access

########################
# runtime: dhi.io hardened static base (distroless-style, no shell).
# The daemon binary is CGO_ENABLED=0 so no libc is required.
########################
FROM dhi.io/static:20250419

# hadolint ignore=DL3050
LABEL org.opencontainers.image.title="swarm-device-access" \
	org.opencontainers.image.description="Listens to Docker container-start events and injects cgroup v2 BPF device-allow rules for containers that bind-mount /dev/... paths" \
	org.opencontainers.image.source="https://github.com/leinardi/swarm-device-access" \
	org.opencontainers.image.licenses="Apache-2.0"

# Copy the binary
COPY --from=build /out/swarm-device-access /usr/local/bin/swarm-device-access

# NOTE: the daemon must run as root and with elevated privileges. It writes
# eBPF programs into the host's cgroup v2 hierarchy via /sys/fs/cgroup and
# the bpf(2) syscall. The deployment is responsible for the bind mounts and
# namespace settings — see deployments/docker/docker-compose.yaml for an
# example layout (privileged, cgroupns=host, pid=host, /sys mount).
#
# The daemon spawns no child processes and handles SIGTERM/SIGINT directly via
# signal.NotifyContext, so no init shim (tini, dumb-init) is needed — even when
# the container is started with --pid=host (which would place an init shim
# outside PID 1 of its own pid namespace and prevent it from working anyway).

# dhi.io/static defaults to a non-root user; bpf(2) and cgroup writes require root.
# hadolint ignore=DL3002
USER 0
ENTRYPOINT ["/usr/local/bin/swarm-device-access"]
