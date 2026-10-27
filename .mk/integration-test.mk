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
# Adapted from swarm-scheduler-exporter/.mk/integration-test.mk at d57dcc220d29, originally
# MIT-licensed, Copyright (c) 2026 Roberto Leinardi.
#

# Local snippet (NOT part of make-common): the integration suite (the testenv crate, behind its
# integration feature) against a live Docker daemon. It runs the daemon binary as a host process
# in dry-run (no BPF or elevated privileges required) and the daemon image in launcher mode.
#
# What it tests is what ships: test-integration builds the image from deployments/docker/Dockerfile
# with make docker-build, under the fixed tag $(INTEGRATION_IMAGE), and copies the binary out of
# that image, so the host process runs the image's own static musl bytes. The build pulls the
# dhi.io bases, so it needs a `docker login dhi.io`. Before any test the suite proves both are the
# Rust daemon: each must refuse -help with exit 2 and the single-dash refusal (Q1), where the 1.x
# Go daemon prints its usage and exits 0; a mismatch fails the run.
#
# SDA_GO_TEST_IMAGE is the last published 1.x image, pinned by digest, which test-integration
# pulls first. The identity check's negative control runs it and must see it refused, and the
# cross-language upgrade and rollback test (TestUpgrade_GoToRustAndBack) runs it as the Go daemon,
# after proving it answers -help as Go.
#
# RUN goes to the runner's --run, with Go's -run semantics, and a pattern that selects no test
# fails the run:
#   make test-integration RUN='^TestDaemon_DryRun_'
#
# SDA_IT_SUITE_TIMEOUT is the suite's own deadline (default 4m); cargo has no test timeout of its
# own.
#
# SDA_IT_ENFORCE=1 also runs the real-enforcement tests and the upgrade test. They attach BPF
# programs to test containers, run `sudo -n systemctl daemon-reload` on this host and read the
# attached programs through `sudo -n`, so only use it on a throwaway or trusted host. With it set,
# a missing prerequisite fails the run instead of skipping. SDA_IT_REQUIRE_RELOAD=1 also fails the
# run when the host cannot prove the reload scenario:
#   make test-integration SDA_IT_ENFORCE=1 SDA_IT_REQUIRE_RELOAD=1
#
# Every container the suite creates carries the swarm-device-access-it.envid label. The daemon a
# test launcher creates does not; it is swept once its launcher no longer exists. After a killed
# run, remove the leftovers of every run with:
#   make sweep-test-leaks

INTEGRATION_ENV_LABEL := swarm-device-access-it.envid
# The labels src/launcher puts on the daemon containers it creates.
LAUNCHER_ROLE_LABEL   := io.github.leinardi.swarm-device-access.role
LAUNCHER_OWNER_LABEL  := io.github.leinardi.swarm-device-access.launcher

export SDA_IT_SUITE_TIMEOUT ?= 4m
export SDA_IT_ENFORCE SDA_IT_REQUIRE_RELOAD

# The image under test, built from the shipped Dockerfile under a fixed tag, and the binary copied
# out of it. A fixed tag, so the suite finds the image docker-build produced without knowing the
# version-derived IMAGE_TAG.
INTEGRATION_IMAGE_REPO := swarm-device-access
INTEGRATION_IMAGE_TAG  := integration
INTEGRATION_IMAGE      := $(INTEGRATION_IMAGE_REPO):$(INTEGRATION_IMAGE_TAG)
INTEGRATION_BINARY     := $(REPO_ROOT)/target/integration/$(BIN_NAME)
export SDA_RUST_TEST_IMAGE ?= $(INTEGRATION_IMAGE)
export SDA_RUST_TEST_BINARY ?= $(INTEGRATION_BINARY)

# The last published 1.x release, v1.0.3, by its multi-arch index digest: a tag could be moved, the
# digest names the exact Go daemon the upgrade and rollback test was proven against.
export SDA_GO_TEST_IMAGE ?= ghcr.io/leinardi/swarm-device-access:v1.0.3@sha256:f49322681563406d98bb1137634cd91e60cb716828218f17835e58173888d7d0

.PHONY: integration-image
integration-image: ## Build the daemon image from the Dockerfile as swarm-device-access:integration and copy its binary out
	$(MAKE) docker-build IMAGE_REPO=$(INTEGRATION_IMAGE_REPO) IMAGE_TAG=$(INTEGRATION_IMAGE_TAG)
	@set -eu; \
	mkdir -p "$(dir $(INTEGRATION_BINARY))"; \
	container=$$(docker create "$(INTEGRATION_IMAGE)"); \
	status=0; \
	docker cp "$$container:/usr/local/bin/$(BIN_NAME)" "$(INTEGRATION_BINARY)" || status=$$?; \
	docker rm "$$container" >/dev/null; \
	exit $$status

.PHONY: go-test-image
go-test-image: ## Pull the pinned 1.x Go daemon image the identity control and the upgrade test run
	docker pull --quiet "$(SDA_GO_TEST_IMAGE)"

.PHONY: test-integration
test-integration: integration-image go-test-image ## Run the integration suite against the image built from the Dockerfile (requires Docker)
	$(CARGO) test --locked -p testenv --features integration --test integration -- --run '$(RUN)'

.PHONY: sweep-test-leaks
sweep-test-leaks: ## Remove every container and image left behind by integration test runs
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
