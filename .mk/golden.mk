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

ifndef MK_LOCAL_GOLDEN_INCLUDED
MK_LOCAL_GOLDEN_INCLUDED := 1

# Local snippet (NOT part of make-common): the Go golden captures the Rust port is checked against
# (docs/rust-port.md, "Go golden captures"). The capture tests sit behind the goldencapture build
# tag, so a plain go test never runs them. golden-check compares what the Go code produces now with
# the committed files under tests/testdata and fails on any difference; golden-update rewrites
# them, for a reviewed change. -count=1: the committed files are read through a path the test cache
# does not key on.

GOLDEN_PKG ?= ./internal/...

.PHONY: golden-check
golden-check: ## Check the committed Go golden captures against the Go code
	$(GO) test -tags=goldencapture -count=1 -run '^TestGolden' $(GOLDEN_PKG)

.PHONY: golden-update
golden-update: ## Rewrite the Go golden captures under tests/testdata (review the diff)
	SDA_GOLDEN_UPDATE=1 $(GO) test -tags=goldencapture -count=1 -run '^TestGolden' $(GOLDEN_PKG)

# wire-capture records what the Go daemon and launcher send a live Docker daemon and what it
# answers, into tests/testdata/engine_wire/api-<version> (its README says how and why). The capture
# tests sit behind the wirecapture build tag and need Docker, so only this target runs them; it is
# not part of any gate, since the answers carry IDs and times that change with every run.

.PHONY: wire-capture
wire-capture: go-build ## Recapture the engine-wire fixtures from the Go binary against the local Docker daemon
	SDA_WIRE_BINARY=$(REPO_ROOT)/$(DIST_DIR)/$(BIN_NAME) $(GO) test -tags=wirecapture -count=1 -run '^TestCapture' ./internal/wirecapture/

endif  # MK_LOCAL_GOLDEN_INCLUDED
