# Resolve repository root (Makefile can live anywhere)
REPO_ROOT := $(shell git rev-parse --show-toplevel 2>/dev/null || pwd)

MK_COMMON_REPO        ?= leinardi/make-common
MK_COMMON_VERSION     ?= v1

MK_COMMON_DIR         := $(REPO_ROOT)/.mk

# Shared snippets coming from make-common
MK_COMMON_FILES       := docker.mk help.mk go.mk password.mk pre-commit.mk

# Repo-local snippets that are NOT in make-common
MK_LOCAL_FILES        := docker-run.mk

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
BIN_NAME     ?= device-mapping-manager
GO_CMD       ?= ./cmd/device-mapping-manager
GO_PKG       ?= ./...
DIST_DIR     ?= dist

# Docker build settings
DOCKERFILE     ?= deployments/docker/Dockerfile
DOCKER_CONTEXT ?= .

IMAGE_NAME   ?= $(BIN_NAME)
IMAGE_REPO   ?= $(IMAGE_NAME)
IMAGE_TAG    ?= $(GIT_VERSION)

# Drop-in runtime defaults for `make go-run`
ARGS ?= -log-level debug

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
# Project overrides
# -----------------------------------------------------------------------------
# device-mapping-manager is Linux-only: all files in cmd/ and most of internal/
# have //go:build linux. Override the shared go-build target to cross-compile
# so `make go-build` works on macOS / Windows dev machines without needing the
# caller to set GOOS by hand.
.PHONY: go-build
go-build: ## Build linux binary into $(DIST_DIR)/ (cross-compiles from any host OS)
	@mkdir -p "$(DIST_DIR)"
	GOOS=linux $(GO) build -ldflags "$(GO_LDFLAGS)" -o "$(DIST_DIR)/$(BIN_NAME)" "$(GO_CMD)"
