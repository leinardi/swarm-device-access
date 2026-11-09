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
# Adapted from swarm-scheduler-exporter/.mk/image.mk at 88394aad4cf9, originally MIT-licensed,
# Copyright (c) 2026 Roberto Leinardi.
#

ifndef MK_LOCAL_IMAGE_INCLUDED
MK_LOCAL_IMAGE_INCLUDED := 1

# Local snippet (NOT part of make-common): the image's prebuilt target, the one CI tests and the
# release ships. It is built from the release binaries `make rust-release-binaries` wrote, named as
# the release assets, through the `dist` build context, so the image binary is the asset byte for
# byte and the Dockerfile's build stage (dhi.io/rust) is never used. The base (dhi.io/static) still
# needs a dhi.io login. `make docker-build` builds the default target, with its own Rust build.
#
# The stamps are rust.mk's GIT_VERSION, GIT_COMMIT and BUILD_DATE, the ones rust-release-binaries
# compiles in: pass the same values to both.

# One platform per build, loaded into the local image store: the host's by default; another one
# needs QEMU (binfmt) to run its tests.
IMAGE_PLATFORM ?= linux/$(patsubst x86_64,amd64,$(patsubst aarch64,arm64,$(shell uname -m)))
PREBUILT_IMAGE ?= $(IMAGE_REPO):prebuilt-$(subst /,-,$(IMAGE_PLATFORM))

.PHONY: image-build-prebuilt
image-build-prebuilt: ## Build the image's prebuilt target for IMAGE_PLATFORM from dist/ (make rust-release-binaries first)
	docker buildx build --load --platform "$(IMAGE_PLATFORM)" --target prebuilt \
	  --build-context dist="$(RUST_RELEASE_DIR)" -f "$(DOCKERFILE)" -t "$(PREBUILT_IMAGE)" \
	  --build-arg VERSION="$(GIT_VERSION)" --build-arg COMMIT="$(GIT_COMMIT)" --build-arg DATE="$(BUILD_DATE)" \
	  "$(DOCKER_CONTEXT)"

# smoke (the release's), reduced (CI's arm64, under QEMU) or full (CI's amd64): scripts/image-test.sh.
IMAGE_TEST_TIER ?= full

.PHONY: image-test
image-test: ## Test the prebuilt image against its release binary (IMAGE_TEST_TIER smoke, reduced or full)
	"$(REPO_ROOT)/scripts/image-test.sh" --tier "$(IMAGE_TEST_TIER)" --image "$(PREBUILT_IMAGE)" \
	  --platform "$(IMAGE_PLATFORM)" --binary "$(call rust_release_asset,$(IMAGE_PLATFORM))" \
	  --expect-version "$(GIT_VERSION) $(GIT_COMMIT) $(BUILD_DATE)"

# The two above in order, for the same reason as rust-release-dist: under make -j,
# `make image-build-prebuilt image-test` could test a missing or previous image before the build
# has loaded the new one.
.PHONY: image-build-test
image-build-test: ## Build the prebuilt image for IMAGE_PLATFORM, then test it (ordered, safe under make -j)
	$(MAKE) --no-print-directory image-build-prebuilt
	$(MAKE) --no-print-directory image-test

endif  # MK_LOCAL_IMAGE_INCLUDED
