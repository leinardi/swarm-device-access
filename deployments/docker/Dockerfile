# syntax=docker/dockerfile:1.7@sha256:a57df69d0ea827fb7266491f2813635de6f17269be881f696fbfdf2d83dda33e

#
# Copyright 2026 Roberto Leinardi.
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     https://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.
#
# Adapted from swarm-scheduler-exporter/deployments/docker/Dockerfile at d57dcc220d29, originally
# MIT-licensed, Copyright (c) 2026 Roberto Leinardi.
#

# The Dockerfile frontend above is pinned as tag@sha256:<index digest>, so a moved 1.7 tag cannot
# change how this file is built. Dependabot's docker ecosystem does not update it: bump it by hand
# (docs/release.md).
#
# The dhi.io bases are pinned by an explicit tag, deliberately not by digest: dhi.io rebuilds its
# tags too often (security fixes) for a digest pin to be worth following by hand, and a rebuild
# picks those fixes up. Each is one literal FROM, so Dependabot's docker ecosystem can move the tag
# (it does not read ARG defaults). Every release build is smoke-tested and scanned, and the release
# notes record the base digest each platform was built on, read from the build's provenance. The
# rust tag names the exact release of rust-toolchain.toml, so the image builds with the toolchain
# CI tests; a new Rust release moves rust-toolchain.toml, this tag and RUST_VERSION together, by
# hand.
#
# The zig, cargo-zigbuild and rust-std versions and digests below are copies of the pins recorded
# in .mk/rust-release.mk; meta/tests/toolchain.rs keeps them equal.
#
# Two targets ship the same runtime stage on the same base:
#   runtime (the default, last stage): the binary is cross-compiled in the build stage below. For
#     local development: `make docker-build`.
#   prebuilt: the binary is the release asset already built outside Docker (`make
#     rust-release-binaries`), COPYed from the `dist` build context, so the image binary is byte for
#     byte the attested release asset. CI and the release build this one: `make
#     image-build-prebuilt`. The build stage, and so dhi.io/rust, is never used.

########################
# build: cross-compile a static musl binary for the target platform on the build platform, with
# cargo-zigbuild and zig as the C compiler and linker.
#
# The debian variant, because it carries the upstream toolchain, for which the upstream musl
# standard libraries below fit; the alpine variant's rustc is Alpine's own build for its
# x86_64-alpine-linux-musl host and cannot take another target's standard library.
########################
FROM --platform=$BUILDPLATFORM dhi.io/rust:1.98.1-debian13-dev AS build

# The image's own toolchain version: the standard libraries added below must be the same release.
ARG RUST_VERSION=1.98.1
ARG ZIG_VERSION=0.17.0
ARG CARGO_ZIGBUILD_VERSION=0.24.0

SHELL ["/bin/bash", "-o", "pipefail", "-c"]

# curl and xz fetch and unpack zig and the standard libraries; nothing from this stage but the
# binary ships. Unpinned: dhi.io republishes this tag with a refreshed package index, which would
# break any pinned version, and every download below is checked by its own sha256.
RUN apt-get update \
	&& apt-get install -y --no-install-recommends ca-certificates curl xz-utils \
	&& rm -rf /var/lib/apt/lists/*

# zig for the build platform, checked against the sha256 ziglang.org publishes.
ARG BUILDARCH
RUN case "${BUILDARCH}" in \
		amd64) zig_arch=x86_64; zig_sha256=1cbe9df9f27e6b78d14ccbca43b6703a404ef79ef1c463de901d7f088d4e2026 ;; \
		arm64) zig_arch=aarch64; zig_sha256=9e8d11661d4ae3bd57702a3832781e23ad151dde5798e16a5ccd503f65234ff8 ;; \
		*) echo "no zig for build platform ${BUILDARCH}" >&2; exit 1 ;; \
	esac \
	&& curl -fsSL -o /tmp/zig.tar.xz "https://ziglang.org/download/${ZIG_VERSION}/zig-${zig_arch}-linux-${ZIG_VERSION}.tar.xz" \
	&& echo "${zig_sha256}  /tmp/zig.tar.xz" | sha256sum -c - \
	&& mkdir /opt/zig \
	&& tar -xJf /tmp/zig.tar.xz -C /opt/zig --strip-components=1 \
	&& rm /tmp/zig.tar.xz
ENV PATH="/opt/zig:${PATH}"

# cargo-zigbuild's release binary for the build platform, checked against the sha256 its release
# publishes. The release has no aarch64 musl asset, and this stage is glibc-based, so arm64 takes
# the gnu one.
RUN case "${BUILDARCH}" in \
		amd64) zigbuild_asset=x86_64-unknown-linux-musl; zigbuild_sha256=52be2cb85ad3875682564a29bac1832b2ab3db68b1c504765c92d5249cc9b96f ;; \
		arm64) zigbuild_asset=aarch64-unknown-linux-gnu; zigbuild_sha256=de42626985f428033078dfc12ac8a59ee713f8e5627235ec883b1c899bee895c ;; \
		*) echo "no cargo-zigbuild for build platform ${BUILDARCH}" >&2; exit 1 ;; \
	esac \
	&& curl -fsSL -o /tmp/cargo-zigbuild.tar.xz "https://github.com/rust-cross/cargo-zigbuild/releases/download/v${CARGO_ZIGBUILD_VERSION}/cargo-zigbuild-${zigbuild_asset}.tar.xz" \
	&& echo "${zigbuild_sha256}  /tmp/cargo-zigbuild.tar.xz" | sha256sum -c - \
	&& mkdir /tmp/cargo-zigbuild \
	&& tar -xJf /tmp/cargo-zigbuild.tar.xz -C /tmp/cargo-zigbuild --strip-components=1 \
	&& install -m 0755 /tmp/cargo-zigbuild/cargo-zigbuild /usr/local/bin/cargo-zigbuild \
	&& rm -rf /tmp/cargo-zigbuild /tmp/cargo-zigbuild.tar.xz \
	&& cargo-zigbuild --version

# The musl standard library of the target platform, checked against static.rust-lang.org's sha256.
ARG TARGETARCH
RUN case "${TARGETARCH}" in \
		amd64) triple=x86_64-unknown-linux-musl; std_sha256=bd9f35880de21dab0e387e84009c00001ae7836a4ba565cb16823f13f75f325f ;; \
		arm64) triple=aarch64-unknown-linux-musl; std_sha256=7e0ad2e67817f224b7a6331e48d6397303fc050edaccf81784ed786ffddc884a ;; \
		*) echo "no Rust target for platform ${TARGETARCH}" >&2; exit 1 ;; \
	esac \
	&& curl -fsSL -o /tmp/rust-std.tar.xz "https://static.rust-lang.org/dist/rust-std-${RUST_VERSION}-${triple}.tar.xz" \
	&& echo "${std_sha256}  /tmp/rust-std.tar.xz" | sha256sum -c - \
	&& mkdir /tmp/rust-std \
	&& tar -xJf /tmp/rust-std.tar.xz -C /tmp/rust-std --strip-components=1 \
	&& /tmp/rust-std/install.sh --prefix=/usr/local --disable-ldconfig \
	&& rm -rf /tmp/rust-std /tmp/rust-std.tar.xz \
	&& echo "${triple}" > /tmp/triple

WORKDIR /src
COPY . .

# The version stamps, declared only here: a build argument is part of the environment of every RUN
# after it, so declared at the top of the stage a new stamp would rebuild the toolchain layers too.
ARG VERSION="dev"
ARG COMMIT="none"
ARG DATE="unknown"

# The registry, the git checkouts (the rust-common crates) and the target directory are BuildKit
# caches, so the binary is copied out of them. The registry and git caches are shared by both
# platforms' builds, which run in parallel: sharing=locked serialises them, since two cargo
# processes unpacking the same crate into one fail with "File exists". The target cache is per
# platform and needs no lock.
RUN --mount=type=cache,target=/root/.cargo/registry,id=cargo-registry,sharing=locked \
	--mount=type=cache,target=/root/.cargo/git,id=cargo-git,sharing=locked \
	--mount=type=cache,target=/src/target,id=cargo-target-${TARGETARCH} \
	triple="$(cat /tmp/triple)" \
	&& SWARM_DEVICE_ACCESS_VERSION="${VERSION}" \
		SWARM_DEVICE_ACCESS_COMMIT="${COMMIT}" \
		SWARM_DEVICE_ACCESS_DATE="${DATE}" \
		cargo zigbuild --release --locked --target "${triple}" --bin swarm-device-access \
	&& mkdir -p /out \
	&& cp "target/${triple}/release/swarm-device-access" /out/swarm-device-access

########################
# dist: the prebuilt target's binaries. Empty here: the build replaces this stage with the `dist`
# directory (`--build-context dist=dist`), which holds the release assets
# swarm-device-access-<os>-<arch>. Without that context the prebuilt target fails on the missing
# file instead of pulling anything.
########################
FROM scratch AS dist

########################
# base: dhi.io hardened static base (distroless-style, no shell), shared by both targets: the user
# and the entrypoint. The binary is statically linked against musl, so no libc is required.
########################
FROM dhi.io/static:20250419 AS base

# NOTE: the daemon must run as root and with elevated privileges. It writes
# eBPF programs into the host's cgroup v2 hierarchy via /sys/fs/cgroup and
# the bpf(2) syscall. The deployment is responsible for the bind mounts and
# namespace settings — see deployments/docker/docker-compose.yaml for an
# example layout (privileged, cgroupns=host, pid=host, /sys mount).
#
# The daemon spawns no child processes and handles SIGTERM/SIGINT itself, so
# no init shim (tini, dumb-init) is needed — even when the container is started
# with --pid=host (which would place an init shim outside PID 1 of its own pid
# namespace and prevent it from working anyway). No HEALTHCHECK and no EXPOSE:
# the metrics listener is off by default (--metrics-addr), and the launcher's
# daemon has no network unless --host-network is set.

# dhi.io/static defaults to a non-root user; bpf(2) and cgroup writes require root.
# droast ignore=DF002 reason="bpf(2) and cgroup v2 writes require root"
USER 0
ENTRYPOINT ["/usr/local/bin/swarm-device-access"]

########################
# prebuilt: the release asset of the target platform, from the dist context, as the binary.
########################
FROM base AS prebuilt

# The same build arguments as the build stage: an ARG is scoped to the stage that declares it.
ARG VERSION="dev"
ARG COMMIT="none"
ARG DATE="unknown"

# The seven OCI labels droast.toml requires ([required-labels], strict-labels), the same in both
# targets (meta/tests/dockerfiles.rs): droast reads the default target's own. Version, revision and
# created come from the same build arguments the binary is stamped with.
LABEL org.opencontainers.image.title="swarm-device-access" \
	org.opencontainers.image.description="Listens to Docker container-start events and injects cgroup v2 BPF device-allow rules for containers that bind-mount /dev/... paths" \
	org.opencontainers.image.source="https://github.com/leinardi/swarm-device-access" \
	org.opencontainers.image.version="${VERSION}" \
	org.opencontainers.image.revision="${COMMIT}" \
	org.opencontainers.image.created="${DATE}" \
	org.opencontainers.image.licenses="Apache-2.0"

ARG TARGETOS
ARG TARGETARCH
COPY --from=dist --chmod=0755 swarm-device-access-${TARGETOS}-${TARGETARCH} /usr/local/bin/swarm-device-access

########################
# runtime (default): the binary the build stage cross-compiled.
########################
FROM base AS runtime

# The same build arguments as the build stage: an ARG is scoped to the stage that declares it.
ARG VERSION="dev"
ARG COMMIT="none"
ARG DATE="unknown"

# The seven OCI labels droast.toml requires ([required-labels], strict-labels), the same in both
# targets (meta/tests/dockerfiles.rs): droast reads the default target's own. Version, revision and
# created come from the same build arguments the binary is stamped with.
LABEL org.opencontainers.image.title="swarm-device-access" \
	org.opencontainers.image.description="Listens to Docker container-start events and injects cgroup v2 BPF device-allow rules for containers that bind-mount /dev/... paths" \
	org.opencontainers.image.source="https://github.com/leinardi/swarm-device-access" \
	org.opencontainers.image.version="${VERSION}" \
	org.opencontainers.image.revision="${COMMIT}" \
	org.opencontainers.image.created="${DATE}" \
	org.opencontainers.image.licenses="Apache-2.0"

COPY --from=build /out/swarm-device-access /usr/local/bin/swarm-device-access
