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

# Local snippet (NOT part of make-common): run integration tests against a
# live Docker daemon. Tests carry //go:build integration and exercise the
# daemon binary with -dry-run (no BPF or elevated privileges required).
#
# Prerequisites: a running Docker daemon and the binary from go-build.
#
# Override SDA_TEST_BINARY to use a custom daemon binary:
#   make go-test-integration SDA_TEST_BINARY=/path/to/swarm-device-access
#
# Override INTEGRATION_TIMEOUT to change the go test timeout. Keep it above
# SDA_IT_SUITE_TIMEOUT (the suite's own deadline) plus teardown time:
#   make go-test-integration INTEGRATION_TIMEOUT=10m SDA_IT_SUITE_TIMEOUT=8m
#
# Override RUN to select specific tests:
#   make go-test-integration RUN=TestDaemon_DryRun_DetectsDeviceMount
#
# SDA_IT_ENFORCE=1 also runs the real-enforcement test (enforce_test.go). It
# builds the daemon image as $(INTEGRATION_IMAGE), attaches BPF programs to test
# containers and runs `sudo -n systemctl daemon-reload` on this host, so only use
# it on a throwaway or trusted host. With it set, a missing prerequisite fails
# the run instead of skipping. SDA_IT_REQUIRE_RELOAD=1 also fails the run when
# the host cannot prove the reload scenario:
#   make go-test-integration SDA_IT_ENFORCE=1 SDA_IT_REQUIRE_RELOAD=1
#
# Every container the suite creates carries the swarm-device-access-it.envid
# label. The daemon a test launcher creates does not; it is swept once its
# launcher no longer exists. After a killed run, remove the leftovers of every
# run with:
#   make sweep-test-leaks
#
# The Rust port of the suite (the testenv crate, behind its integration feature;
# docs/rust-port.md) runs beside the Go one with rust-test-integration. It reads
# its own variables, never SDA_TEST_BINARY or SDA_TEST_IMAGE, which name the Go
# build: SDA_RUST_TEST_BINARY, the static host-architecture musl binary, run as
# a host process in dry-run, and SDA_RUST_TEST_IMAGE, a throwaway FROM scratch
# image of that binary (no release packaging, one architecture). Before any test
# the suite proves both are Rust: each must refuse -help with exit 2 and the
# single-dash refusal, where Go prints its usage and exits 0; a mismatch fails
# the run. Its negative control builds a scratch image of the Go binary
# (SDA_TEST_BINARY, from go-build) and checks the refusal. RUN goes to the
# runner's --run, with Go's -run semantics, and a pattern that selects no test
# fails the run:
#   make rust-test-integration RUN='^TestDaemon_DryRun_'
#
# SDA_IT_ENFORCE=1 and SDA_IT_REQUIRE_RELOAD=1 mean in the Rust suite what they
# mean in the Go one: the real-enforcement tests run (on the Rust image), and a
# missing prerequisite or reload capability fails the run instead of skipping.
# With them the target also builds $(INTEGRATION_IMAGE), the Go Dockerfile's
# build, as SDA_GO_TEST_IMAGE for the cross-language upgrade and rollback test
# (TestUpgrade_GoToRustAndBack), which must answer -help as the Go daemon. That
# test reads the BPF programs attached to its container through `sudo -n`, as
# the reload test runs systemctl; throwaway or trusted hosts only:
#   make rust-test-integration SDA_IT_ENFORCE=1 SDA_IT_REQUIRE_RELOAD=1

INTEGRATION_TIMEOUT  ?= 6m
INTEGRATION_PKG      ?= ./test/integration/...
INTEGRATION_ENV_LABEL := swarm-device-access-it.envid
# The labels internal/launcher puts on the daemon containers it creates.
LAUNCHER_ROLE_LABEL   := io.github.leinardi.swarm-device-access.role
LAUNCHER_OWNER_LABEL  := io.github.leinardi.swarm-device-access.launcher

# go test changes CWD to the package dir, so the binary path must be absolute.
export SDA_TEST_BINARY ?= $(REPO_ROOT)/$(DIST_DIR)/$(BIN_NAME)
export SDA_IT_SUITE_TIMEOUT ?= 4m

# Fixed tag, so the test can find the image docker-build produced without
# knowing the version-derived IMAGE_TAG.
INTEGRATION_IMAGE_REPO := swarm-device-access
INTEGRATION_IMAGE_TAG  := integration
INTEGRATION_IMAGE      := $(INTEGRATION_IMAGE_REPO):$(INTEGRATION_IMAGE_TAG)
export SDA_TEST_IMAGE ?= $(INTEGRATION_IMAGE)
export SDA_IT_ENFORCE SDA_IT_REQUIRE_RELOAD

# The Rust suite's binary and image: the host architecture's static musl build
# and a scratch image of it, under a fixed tag. The binary is built by
# rust-release.mk's rust_static_build, the release binaries' one build (zig,
# cargo-zigbuild, the version stamps and the remapped paths), for the host's
# target, so the suite tests the bytes a release of that architecture would
# carry; it needs the pinned zig and cargo-zigbuild on PATH. Its context (the
# binary) is copied under target/ and its Dockerfile goes to docker build on
# stdin, so no second Dockerfile exists in the tree, tracked or not
# (test/dockerfiles).
RUST_IT_TARGET    ?= $(shell uname -m)-unknown-linux-musl
RUST_IT_BINARY    := $(REPO_ROOT)/target/$(RUST_IT_TARGET)/release/$(BIN_NAME)
RUST_IT_CONTEXT   := $(REPO_ROOT)/target/rust-integration-image
RUST_IT_IMAGE     := $(INTEGRATION_IMAGE_REPO):rust-integration
export SDA_RUST_TEST_BINARY ?= $(RUST_IT_BINARY)
export SDA_RUST_TEST_IMAGE ?= $(RUST_IT_IMAGE)
# The Go daemon image of the Rust suite's upgrade test: the Go suite's image.
export SDA_GO_TEST_IMAGE ?= $(INTEGRATION_IMAGE)

# -count=1: the results depend on Docker and host state that go test's result
# cache cannot see, so a cached PASS would replay without running anything.
.PHONY: go-test-integration
go-test-integration: go-build $(if $(filter 1,$(SDA_IT_ENFORCE)),integration-image) ## Run integration tests against a live Docker daemon (requires Docker)
	$(GO) test \
	  -tags=integration \
	  -count=1 \
	  -timeout=$(INTEGRATION_TIMEOUT) \
	  -v \
	  $(if $(RUN),-run $(RUN),) \
	  $(INTEGRATION_PKG)

# go-vet sees only untagged files; this vets the integration suite, which needs no Docker to vet,
# so a change that breaks it fails on any machine and not first where the suite runs.
.PHONY: go-vet-integration
go-vet-integration: ## Vet the integration-tagged test packages
	$(GO) vet -tags=integration $(INTEGRATION_PKG)

.PHONY: integration-image
integration-image: ## Build the daemon image the enforcement test runs, as $(INTEGRATION_IMAGE)
	$(MAKE) docker-build IMAGE_REPO=$(INTEGRATION_IMAGE_REPO) IMAGE_TAG=$(INTEGRATION_IMAGE_TAG)

.PHONY: rust-integration-image
rust-integration-image: rust-zigbuild-check ## Build the static host-arch Rust binary and its throwaway scratch image, $(RUST_IT_IMAGE)
	$(call rust_static_build,$(RUST_IT_TARGET))
	@set -eu; \
	mkdir -p "$(RUST_IT_CONTEXT)"; \
	cp "$(RUST_IT_BINARY)" "$(RUST_IT_CONTEXT)/$(BIN_NAME)"; \
	printf '%s\n' 'FROM scratch' \
	  'COPY $(BIN_NAME) /usr/local/bin/$(BIN_NAME)' \
	  'ENTRYPOINT ["/usr/local/bin/$(BIN_NAME)"]' | \
	  docker build --quiet --tag "$(RUST_IT_IMAGE)" --file - "$(RUST_IT_CONTEXT)"

# The suite's own deadline is SDA_IT_SUITE_TIMEOUT, as the Go suite's; cargo has
# no test timeout of its own. go-build provides the negative control's Go binary,
# and with SDA_IT_ENFORCE=1 integration-image the upgrade test's Go image.
.PHONY: rust-test-integration
rust-test-integration: go-build rust-integration-image $(if $(filter 1,$(SDA_IT_ENFORCE)),integration-image) ## Run the Rust integration suite against the Rust binary and image (requires Docker)
	$(CARGO) test --locked -p testenv --features integration --test integration -- --run '$(RUN)'

.PHONY: sweep-test-leaks
sweep-test-leaks: ## Remove every container and image left behind by integration test runs (Go and Rust)
	@set -eu; \
	ids=$$(docker ps -aq --filter "label=$(INTEGRATION_ENV_LABEL)"); \
	if [ -n "$$ids" ]; then docker rm -f $$ids; fi; \
	images=$$(docker image ls -q --filter "label=$(INTEGRATION_ENV_LABEL)" | sort -u); \
	if [ -n "$$images" ]; then docker rmi -f $$images; fi; \
	daemons=$$(docker ps -a --filter "label=$(LAUNCHER_ROLE_LABEL)=daemon" \
	  --format '{{.ID}} {{.Label "$(LAUNCHER_OWNER_LABEL)"}}'); \
	printf '%s\n' "$$daemons" | while read -r id owner; do \
	  [ -n "$$id" ] && [ -n "$$owner" ] || continue; \
	  if err=$$(docker container inspect "$$owner" 2>&1 >/dev/null); then continue; fi; \
	  case "$$err" in \
	    *"No such"*) docker rm -f "$$id" ;; \
	    *) echo "inspect launcher $$owner of daemon $$id: $$err" >&2; exit 1 ;; \
	  esac; \
	done
