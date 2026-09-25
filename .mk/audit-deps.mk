ifndef MK_LOCAL_AUDIT_DEPS_INCLUDED
MK_LOCAL_AUDIT_DEPS_INCLUDED := 1

# Local snippet (NOT part of make-common): check the Go dependencies this repository ships against
# known vulnerabilities, and keep banned modules out of the module graph.
#
# What this is and is not. govulncheck is source-aware: it reports a vulnerable module only when
# the build actually reaches the affected symbol, so it answers "is this repository exposed", not
# "does a vulnerable version appear in the graph". The banned-module check answers the graph
# question for the modules listed in BANNED_GO_MODULES: depguard (.golangci.yaml) only sees this
# repository's own import lines, so it cannot notice a banned module coming back transitively
# through some other dependency. go mod why -m can.
#
# Both need the network, and govulncheck over every package is too slow to run on every Go
# change. The pre-commit hook runs this same target, but only when go.mod or go.sum change; CI runs
# it on every pull request (the audit-deps job in .github/workflows/ci.yaml).
#
# The version is pinned so two runs mean the same thing. Bump it deliberately.

GOVULNCHECK_VERSION ?= v1.8.0
GOVULNCHECK        ?= golang.org/x/vuln/cmd/govulncheck@$(GOVULNCHECK_VERSION)

# github.com/docker/docker is affected by GO-2026-4887 and GO-2026-4883 in every version, with no
# fix in that module; the SDK is github.com/moby/moby/client + github.com/moby/moby/api.
BANNED_GO_MODULES ?= github.com/docker/docker

.PHONY: audit-deps
audit-deps: audit-deps-go audit-deps-banned ## Scan Go dependencies for known vulnerabilities and banned modules (network required)

.PHONY: audit-deps-go
audit-deps-go: ## Run the pinned govulncheck over every Go package
	$(GO) run $(GOVULNCHECK) ./...

# go mod why -m prints "(main module does not need module <mod>)" when no package in the build
# (tests included) imports the module; anything else is the import chain that pulls it in.
.PHONY: audit-deps-banned
audit-deps-banned: ## Fail if a module in BANNED_GO_MODULES is reachable from the main module
	@status=0; \
	for mod in $(BANNED_GO_MODULES); do \
	  if ! why="$$($(GO) mod why -m "$$mod" 2>&1)"; then \
	    echo "[audit-deps] go mod why -m $$mod failed:" >&2; \
	    printf '%s\n' "$$why" >&2; \
	    status=1; \
	  elif printf '%s\n' "$$why" | grep -qxF "(main module does not need module $$mod)"; then \
	    echo "[audit-deps] $$mod: not needed"; \
	  else \
	    echo "[audit-deps] banned module $$mod is needed by the main module, via:" >&2; \
	    printf '%s\n' "$$why" >&2; \
	    status=1; \
	  fi; \
	done; \
	exit $$status

endif  # MK_LOCAL_AUDIT_DEPS_INCLUDED
