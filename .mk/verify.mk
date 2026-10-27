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

# Local snippet (NOT part of make-common): one command that runs every check a change has to pass,
# the way CI does (AGENTS.md, Validation matrix). verify-rust lives in rust.mk. The integration
# suite needs Docker, so it stays a separate target (test-integration); its code is still built and
# linted here, through --all-features.

.PHONY: verify
verify: verify-rust ## Verify everything (build, fmt, clippy, tests)

endif  # MK_LOCAL_VERIFY_INCLUDED
