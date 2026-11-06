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

# Resolve repository root (Makefile can live anywhere)
REPO_ROOT := $(shell git rev-parse --show-toplevel 2>/dev/null || pwd)

MK_COMMON_REPO        ?= leinardi/make-common
MK_COMMON_VERSION     ?= v1

MK_COMMON_DIR         := $(REPO_ROOT)/.mk

# Shared snippets coming from make-common
MK_COMMON_FILES       := docker.mk help.mk password.mk pre-commit.mk rust.mk

# Repo-local snippets that are NOT in make-common
MK_LOCAL_FILES        := docker-run.mk integration-test.mk audit-deps.mk rust-release.mk verify.mk

MK_COMMON_BOOTSTRAP_SCRIPT := $(REPO_ROOT)/scripts/bootstrap-mk-common.sh

# Bootstrap: the script will self-update and fetch the selected .mk snippets
MK_COMMON_BOOTSTRAP := $(shell "$(MK_COMMON_BOOTSTRAP_SCRIPT)" \
  "$(MK_COMMON_REPO)" \
  "$(MK_COMMON_VERSION)" \
  "$(MK_COMMON_DIR)" \
  "$(MK_COMMON_FILES)")

# -----------------------------------------------------------------------------
# Project-specific config
# -----------------------------------------------------------------------------
BIN_NAME     ?= swarm-device-access
DIST_DIR     ?= dist

# rust.mk stamps the binary's compile-time version: src/version.rs reads
# SWARM_DEVICE_ACCESS_VERSION, SWARM_DEVICE_ACCESS_COMMIT and SWARM_DEVICE_ACCESS_DATE.
RUST_STAMP_PREFIX := SWARM_DEVICE_ACCESS

# The date stamp is HEAD's commit date in UTC (rust.mk's GIT_COMMIT_DATE, the clock's only
# outside git), the date release.yaml stamps. A stamp that moves only with a commit keeps a
# repeated build a cache hit: a new date on every make would recompile the crate that reads it
# each time.
BUILD_DATE ?= $(or $(GIT_COMMIT_DATE),$(shell date -u +"%Y-%m-%dT%H:%M:%SZ"))

# rust-build unstamped, as rust-test builds: the two share the swarm-device-access crate's units
# instead of each recompiling it after the other. rust-dist, rust-run and the release builds stay
# stamped.
RUST_BUILD_ENV :=

# Docker build settings
DOCKERFILE     ?= deployments/docker/Dockerfile
DOCKER_CONTEXT ?= .

IMAGE_NAME   ?= $(BIN_NAME)
IMAGE_REPO   ?= $(IMAGE_NAME)
IMAGE_TAG    ?= $(GIT_VERSION)

# -----------------------------------------------------------------------------
# Include shared make logic (fetched from make-common)
# -----------------------------------------------------------------------------
include $(addprefix $(MK_COMMON_DIR)/,$(MK_COMMON_FILES))

# -----------------------------------------------------------------------------
# Include repo-local logic (no bootstrap; lives only in this repo)
# -----------------------------------------------------------------------------
-include $(addprefix $(REPO_ROOT)/.mk/,$(MK_LOCAL_FILES))

.PHONY: mk-common-update
mk-common-update: ## Check for remote updates of shared .mk files
	@echo "[mk] Checking for updates from $(MK_COMMON_REPO)@$(MK_COMMON_VERSION)"
	MK_COMMON_UPDATE=1 "$(MK_COMMON_BOOTSTRAP_SCRIPT)" \
	  "$(MK_COMMON_REPO)" \
	  "$(MK_COMMON_VERSION)" \
	  "$(MK_COMMON_DIR)" \
	  "$(MK_COMMON_FILES)"

# -----------------------------------------------------------------------------
# Adding new targets
# -----------------------------------------------------------------------------
# Do NOT add recipes directly to this file. Instead:
#   - Project-specific targets -> new .mk/<fragment>.mk added to MK_LOCAL_FILES
#   - Generic targets (useful beyond this repo) -> new or updated .mk/<fragment>.mk
#     added to MK_COMMON_FILES, then open a PR to port the change upstream at
#     https://github.com/leinardi/make-common so mk-common-update keeps working.
