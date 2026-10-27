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

ifndef MK_LOCAL_VERIFY_INCLUDED
MK_LOCAL_VERIFY_INCLUDED := 1

# Local snippet (NOT part of make-common): umbrella targets for the validation an edit needs
# (AGENTS.md, Validation matrix).
#
# go-vet sees only untagged files, so the integration suite is vetted on its own; go-lint is
# make-common's, running the pinned golangci-lint-full hook over the whole module.
.PHONY: verify-go
verify-go: go-build go-test go-vet go-vet-integration go-lint ## Verify the Go code (build, race tests, vet untagged and tagged, lint)

# While the Go and the Rust code live side by side (docs/rust-port.md), one target runs both;
# verify-rust is in rust.mk.
.PHONY: verify
verify: verify-go verify-rust ## Verify the Go and the Rust code (verify-go, then verify-rust)

endif  # MK_LOCAL_VERIFY_INCLUDED
