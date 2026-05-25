# Local snippet (NOT part of make-common): run integration tests against a
# live Docker daemon. Tests carry //go:build integration and exercise the
# daemon binary with -dry-run (no BPF or elevated privileges required).
#
# Prerequisites: a running Docker daemon and the binary from go-build.
#
# Override SDA_TEST_BINARY to use a custom daemon binary:
#   make go-test-integration SDA_TEST_BINARY=/path/to/swarm-device-access
#
# Override INTEGRATION_TIMEOUT to change the per-test timeout:
#   make go-test-integration INTEGRATION_TIMEOUT=240s
#
# Override RUN to select specific tests:
#   make go-test-integration RUN=TestDaemon_DryRun_DetectsDeviceMount

INTEGRATION_TIMEOUT ?= 120s
INTEGRATION_PKG     ?= ./test/integration/...

# go test changes CWD to the package dir, so the binary path must be absolute.
export SDA_TEST_BINARY ?= $(REPO_ROOT)/$(DIST_DIR)/$(BIN_NAME)

.PHONY: go-test-integration
go-test-integration: go-build ## Run integration tests against a live Docker daemon (requires Docker)
	$(GO) test \
	  -tags=integration \
	  -timeout=$(INTEGRATION_TIMEOUT) \
	  -v \
	  $(if $(RUN),-run $(RUN),) \
	  $(INTEGRATION_PKG)
