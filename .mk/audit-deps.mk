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
# Adapted from swarm-scheduler-exporter/.mk/audit-deps.mk at d57dcc220d29, originally MIT-licensed,
# Copyright (c) 2026 Roberto Leinardi.
#

ifndef MK_LOCAL_AUDIT_DEPS_INCLUDED
MK_LOCAL_AUDIT_DEPS_INCLUDED := 1

# Local snippet (NOT part of make-common): check every Rust dependency against the RustSec
# advisory database, with cargo-deny as configured in deny.toml. It is not reachability-aware: any
# advisory against a crate in the graph counts, which is why every ignore in deny.toml carries a
# reason. It needs the network for the database, so it runs when Cargo.toml, Cargo.lock or
# deny.toml change (the cargo-deny hook, with every check), on every pull request (CI's rust job)
# and every release (the release workflow's audit job), and deliberately here. --workspace: the
# root is a package as well as the workspace, and cargo-deny would otherwise check its graph alone.
# CARGO comes from rust.mk; rust-deny there runs every cargo-deny check, this only the advisories.

.PHONY: audit-deps
audit-deps: ## Check every Rust dependency against the RustSec advisories (cargo-deny; network required)
	$(CARGO) deny --workspace check advisories

endif  # MK_LOCAL_AUDIT_DEPS_INCLUDED
