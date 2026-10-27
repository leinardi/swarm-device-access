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
# Adapted from swarm-scheduler-exporter/.mk/rust.mk at d57dcc220d29, originally MIT-licensed,
# Copyright (c) 2026 Roberto Leinardi.
#

ifndef MK_LOCAL_RUST_INCLUDED
MK_LOCAL_RUST_INCLUDED := 1

# Local snippet (NOT part of make-common): the Rust workspace, beside the Go module until the
# cutover (docs/rust-port.md). Every lint and test command runs workspace-wide, so the meta and
# testenv crates are held to the same lints and tests as the daemon: the lint set lives once in the
# root [workspace.lints], and a command scoped to one package would leave the others unchecked.
# --locked everywhere: Cargo.lock is the pin, and a command that rewrote it would test a dependency
# graph nobody reviewed. cargo deny takes --workspace for the same reason: the root is a package
# too, and cargo-deny would otherwise check its graph alone. Build and clippy take --all-features,
# so the harness code behind testenv's integration feature is compiled and linted like the rest;
# the tests do not, since that feature's suite needs Docker.

CARGO ?= cargo

# The version stamps the daemon reads at compile time, from git and the clock, as go.mk stamps the
# Go binary (its definitions win when both are included; these are the same commands). Only the
# builds stamp, so the tests see the defaults. Cargo tracks the variables: going from a stamped
# build to an unstamped one recompiles the daemon crate, nothing else.
GIT_COMMIT  ?= $(shell git rev-parse --short=12 HEAD 2>/dev/null || echo "none")
GIT_VERSION ?= $(shell git describe --tags --dirty --always 2>/dev/null || echo "v0.0.0")
BUILD_DATE  ?= $(shell date -u +"%Y-%m-%dT%H:%M:%SZ")
RUST_VERSION_ENV = \
  SWARM_DEVICE_ACCESS_VERSION="$(GIT_VERSION)" \
  SWARM_DEVICE_ACCESS_COMMIT="$(GIT_COMMIT)" \
  SWARM_DEVICE_ACCESS_DATE="$(BUILD_DATE)"

.PHONY: rust-build
rust-build: ## Build the Rust workspace, with the version stamps
	$(RUST_VERSION_ENV) $(CARGO) build --workspace --all-targets --all-features --locked

.PHONY: rust-test
rust-test: ## Run the Rust tests, meta-tests included
	$(CARGO) test --workspace --locked

.PHONY: rust-clippy
rust-clippy: ## Lint the Rust workspace with clippy, warnings as errors
	$(CARGO) clippy --workspace --all-targets --all-features --locked -- -D warnings

.PHONY: rust-fmt
rust-fmt: ## Format the Rust code
	$(CARGO) fmt --all

.PHONY: rust-fmt-check
rust-fmt-check: ## Fail if the Rust code needs formatting
	$(CARGO) fmt --all --check

.PHONY: rust-deny
rust-deny: ## Check Rust dependencies with cargo-deny (advisories, licences, bans, sources; network required)
	$(CARGO) deny --workspace check

# rust-deny stays out of verify-rust: its advisories need the network, as audit-deps does.
.PHONY: verify-rust
verify-rust: rust-build rust-fmt-check rust-clippy rust-test ## Verify the Rust code (build, fmt, clippy, tests)

endif  # MK_LOCAL_RUST_INCLUDED
