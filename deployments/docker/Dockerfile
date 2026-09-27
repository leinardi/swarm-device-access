# syntax=docker/dockerfile:1.7@sha256:a57df69d0ea827fb7266491f2813635de6f17269be881f696fbfdf2d83dda33e

# Base images, and the Dockerfile frontend above, are pinned as
# tag@sha256:<index digest>, so a rebuild of the same commit (a release
# recovery, for one) uses the same bases and the same frontend. Dependabot's
# docker ecosystem updates the base images' tag and digest together, through
# reviewed PRs; the frontend pin is updated by hand.

########################
# deps: download modules in a layer keyed on go.mod and go.sum only
########################
FROM --platform=$BUILDPLATFORM dhi.io/golang:1-alpine3.23-dev@sha256:0d95d4f9889abcf60c182228d28fa83fde3e0dc2330bb1365e5960273b43ed57 AS deps
WORKDIR /src
RUN --mount=type=cache,target=/var/cache/apk \
	apk add --no-cache git
COPY go.mod go.sum ./
# The named cache is shared with the build stage, which starts from this one.
RUN --mount=type=cache,target=/go/pkg/mod,id=gomodcache \
	go mod download

########################
# build: compile static binary, on top of deps (same base, platform and
# WORKDIR): the modules are downloaded before the source is copied, so a
# source-only change does not repeat that step
########################
FROM deps AS build
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
FROM dhi.io/static:20250419@sha256:98ef7a853608577e8d66dad1d25ada75d745d782f28d84e9ecfb85dfeb1f9c98

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
