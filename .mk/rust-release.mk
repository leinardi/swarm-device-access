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
# Adapted from swarm-scheduler-exporter/.github/workflows/release.yaml (the static binaries' build
# step) and deployments/docker/Dockerfile (the zig, cargo-zigbuild and rust-std pins) at
# d57dcc220d29, originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi.
#

ifndef MK_LOCAL_RUST_RELEASE_INCLUDED
MK_LOCAL_RUST_RELEASE_INCLUDED := 1

# Local snippet (NOT part of make-common): the static musl builds of the Rust daemon, prepared
# beside the Go release, which still ships (docs/rust-port.md). Nothing here is published or goes
# into an image yet.

# The release toolchain beyond rust-toolchain.toml, each archive by its sha256, re-verified against
# the downloads: zig from https://ziglang.org/download/index.json, cargo-zigbuild from the release
# asset's .sha256, rust-std from the .sha256 static.rust-lang.org publishes beside each archive.
# .github/actions/setup-zigbuild installs the x86_64 zig and cargo-zigbuild, and the Rust image's
# build stage will take all of them; meta/tests/toolchain.rs keeps the versions and the x86_64
# digests one value with the action's, and RUST_RELEASE_RUST_STD_VERSION the exact release of
# rust-toolchain.toml. cargo-zigbuild publishes no aarch64 musl asset, so an arm64 build host
# takes the gnu one. Only the image's build stage will download the rust-std archives and the
# aarch64 build-host archives: everywhere else rustup installs the musl targets.
RUST_RELEASE_ZIG_VERSION := 0.17.0
RUST_RELEASE_ZIG_SHA256_x86_64 := 1cbe9df9f27e6b78d14ccbca43b6703a404ef79ef1c463de901d7f088d4e2026
RUST_RELEASE_ZIG_SHA256_aarch64 := 9e8d11661d4ae3bd57702a3832781e23ad151dde5798e16a5ccd503f65234ff8
RUST_RELEASE_CARGO_ZIGBUILD_VERSION := 0.24.0
RUST_RELEASE_CARGO_ZIGBUILD_SHA256_x86_64-unknown-linux-musl := 52be2cb85ad3875682564a29bac1832b2ab3db68b1c504765c92d5249cc9b96f
RUST_RELEASE_CARGO_ZIGBUILD_SHA256_aarch64-unknown-linux-gnu := de42626985f428033078dfc12ac8a59ee713f8e5627235ec883b1c899bee895c
RUST_RELEASE_RUST_STD_VERSION := 1.98.1
RUST_RELEASE_RUST_STD_SHA256_x86_64-unknown-linux-musl := bd9f35880de21dab0e387e84009c00001ae7836a4ba565cb16823f13f75f325f
RUST_RELEASE_RUST_STD_SHA256_aarch64-unknown-linux-musl := 7e0ad2e67817f224b7a6331e48d6397303fc050edaccf81784ed786ffddc884a

# The release platforms, as <docker arch>:<Rust target>, and where their binaries and
# checksums.txt go: under dist/, beside the Go binary, but in a directory of their own so the names
# the Go release uses (dist/swarm-device-access-linux-<arch>) are never overwritten.
RUST_RELEASE_PLATFORMS := amd64:x86_64-unknown-linux-musl arm64:aarch64-unknown-linux-musl
RUST_RELEASE_DIR       := $(REPO_ROOT)/$(DIST_DIR)/rust

# The one static build, for one Rust target: cargo-zigbuild with zig as the C compiler and linker,
# the version stamps of rust-build, and the checkout and the cargo home remapped, so the binary
# carries no path of the machine that built it and a rebuild of the same commit gives the same
# bytes. The toolchain's rust-src, when it is installed, is mapped back to the /rustc/<commit> the
# standard library was built with: rustc otherwise writes the local copy's path, under the rustup
# home, in place of that prefix, so a machine with rust-src and one without (CI's minimal profile)
# would build different bytes. rust-release-binaries runs it for each release platform and
# rust-integration-image for the host's target. RUSTFLAGS is set, not inherited: a flag from the
# environment would change the shipped bytes. With --target, cargo applies it to the daemon and
# not to build scripts. Recursive (=), so rustc is asked only by a recipe that builds.
RUST_RELEASE_CARGO_HOME = $(or $(CARGO_HOME),$(HOME)/.cargo)
RUST_RELEASE_RUST_SRC   = $(shell cd "$(REPO_ROOT)" && rustc --print sysroot)/lib/rustlib/src/rust
RUST_RELEASE_RUSTC_HASH = $(shell cd "$(REPO_ROOT)" && rustc -vV | sed -n 's/^commit-hash: //p')
RUST_RELEASE_RUSTFLAGS  = --remap-path-prefix=$(REPO_ROOT)=/src \
  --remap-path-prefix=$(RUST_RELEASE_CARGO_HOME)=/cargo \
  --remap-path-prefix=$(RUST_RELEASE_RUST_SRC)=/rustc/$(RUST_RELEASE_RUSTC_HASH)
# $(call rust_static_build,<Rust target>)
rust_static_build = RUSTFLAGS="$(RUST_RELEASE_RUSTFLAGS)" $(RUST_VERSION_ENV) \
  $(CARGO) zigbuild --release --locked --target $(1) --bin $(BIN_NAME)

# The static builds take only the pinned zig and cargo-zigbuild: another release links other bytes.
.PHONY: rust-zigbuild-check
rust-zigbuild-check: ## Fail unless the zig and cargo-zigbuild on PATH are the pinned releases
	@set -eu; \
	for tool in zig cargo-zigbuild; do \
	  if ! command -v "$$tool" >/dev/null 2>&1; then \
	    echo "$$tool is not on PATH; install the release .github/actions/setup-zigbuild pins" >&2; \
	    exit 1; \
	  fi; \
	done; \
	zig_version="$$(zig version)"; \
	if [ "$$zig_version" != "$(RUST_RELEASE_ZIG_VERSION)" ]; then \
	  echo "zig $$zig_version is on PATH; the static builds need zig $(RUST_RELEASE_ZIG_VERSION)" >&2; \
	  exit 1; \
	fi; \
	zigbuild_version="$$(cargo-zigbuild --version)"; \
	if [ "$$zigbuild_version" != "cargo-zigbuild $(RUST_RELEASE_CARGO_ZIGBUILD_VERSION)" ]; then \
	  echo "$$zigbuild_version is on PATH; the static builds need cargo-zigbuild $(RUST_RELEASE_CARGO_ZIGBUILD_VERSION)" >&2; \
	  exit 1; \
	fi

.PHONY: rust-release-binaries
rust-release-binaries: rust-zigbuild-check ## Build the static musl binaries (amd64, arm64) and checksums.txt into dist/rust/ (not shipped yet)
	set -eu; \
	mkdir -p "$(RUST_RELEASE_DIR)"; \
	for platform in $(RUST_RELEASE_PLATFORMS); do \
	  arch="$${platform%%:*}"; \
	  triple="$${platform#*:}"; \
	  $(call rust_static_build,"$$triple"); \
	  cp "$(REPO_ROOT)/target/$$triple/release/$(BIN_NAME)" "$(RUST_RELEASE_DIR)/$(BIN_NAME)-linux-$$arch"; \
	done; \
	cd "$(RUST_RELEASE_DIR)"; \
	sha256sum $(foreach platform,$(RUST_RELEASE_PLATFORMS),$(BIN_NAME)-linux-$(firstword $(subst :, ,$(platform)))) > checksums.txt

endif  # MK_LOCAL_RUST_RELEASE_INCLUDED
