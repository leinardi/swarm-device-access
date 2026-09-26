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
# label. After a killed run, remove the leftovers of every run with:
#   make sweep-test-leaks

INTEGRATION_TIMEOUT  ?= 6m
INTEGRATION_PKG      ?= ./test/integration/...
INTEGRATION_ENV_LABEL := swarm-device-access-it.envid

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

.PHONY: integration-image
integration-image: ## Build the daemon image the enforcement test runs, as $(INTEGRATION_IMAGE)
	$(MAKE) docker-build IMAGE_REPO=$(INTEGRATION_IMAGE_REPO) IMAGE_TAG=$(INTEGRATION_IMAGE_TAG)

.PHONY: sweep-test-leaks
sweep-test-leaks: ## Remove every container left behind by integration test runs
	docker ps -aq --filter "label=$(INTEGRATION_ENV_LABEL)" | xargs -r docker rm -f
