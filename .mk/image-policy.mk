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

# Repository policy: an image gate must not pass without building or testing an image.
.PHONY: image-targets-check
image-targets-check:
	@test -n "$(strip $(DOCKER_TARGETS))" || { echo "DOCKER_TARGETS is empty; image gates require an image" >&2; exit 1; }

.PHONY: image-build-prebuilt
image-build-prebuilt: image-targets-check

.PHONY: image-test
image-test: image-targets-check

.PHONY: image-build-test
image-build-test: image-targets-check
