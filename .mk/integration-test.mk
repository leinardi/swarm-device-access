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
# Every container the suite creates carries the swarm-device-access-it.envid
# label. After a killed run, remove the leftovers of every run with:
#   make sweep-test-leaks

INTEGRATION_TIMEOUT  ?= 6m
INTEGRATION_PKG      ?= ./test/integration/...
INTEGRATION_ENV_LABEL := swarm-device-access-it.envid

# go test changes CWD to the package dir, so the binary path must be absolute.
export SDA_TEST_BINARY ?= $(REPO_ROOT)/$(DIST_DIR)/$(BIN_NAME)
export SDA_IT_SUITE_TIMEOUT ?= 4m

.PHONY: go-test-integration
go-test-integration: go-build ## Run integration tests against a live Docker daemon (requires Docker)
	$(GO) test \
	  -tags=integration \
	  -timeout=$(INTEGRATION_TIMEOUT) \
	  -v \
	  $(if $(RUN),-run $(RUN),) \
	  $(INTEGRATION_PKG)

.PHONY: sweep-test-leaks
sweep-test-leaks: ## Remove every container left behind by integration test runs
	docker ps -aq --filter "label=$(INTEGRATION_ENV_LABEL)" | xargs -r docker rm -f
