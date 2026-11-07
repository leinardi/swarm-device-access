/*
 * Copyright 2026 Roberto Leinardi.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     https://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */
// Adapted from swarm-scheduler-exporter/meta/tests/integration_targets.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The images the integration target runs: the image under test is the one `docker-build` builds
//! from the shipped Dockerfile under the fixed integration tag, and the Go image of the identity
//! control and the upgrade test is a published reference pinned by digest. This repository's own
//! rules, read from `.mk/integration-test.mk`.

use repo_meta::dockerfile;

const INTEGRATION_FRAGMENT: &str = ".mk/integration-test.mk";

/// A make variable's default in the integration fragment: `export NAME ?= value`.
fn exported_default<'content>(fragment: &'content str, name: &str) -> Option<&'content str> {
    fragment.lines().find_map(|line| {
        line.strip_prefix(&format!("export {name} ?= "))
            .map(str::trim)
    })
}

/// What is wrong with the images the integration target runs: the image under test is the one
/// `docker-build` builds from the shipped Dockerfile under the fixed integration tag, and the Go
/// image of the identity control and the upgrade test is a published reference pinned by digest,
/// which the target pulls before the suite runs.
fn image_problems(fragment: &str) -> Vec<String> {
    let mut problems = Vec::new();
    if exported_default(fragment, "SDA_RUST_TEST_IMAGE") != Some("$(INTEGRATION_IMAGE)") {
        problems.push(format!(
            "{INTEGRATION_FRAGMENT} does not test $(INTEGRATION_IMAGE) as SDA_RUST_TEST_IMAGE"
        ));
    }
    if !fragment.contains(
        "$(MAKE) docker-build IMAGE_REPO=$(INTEGRATION_IMAGE_REPO) IMAGE_TAG=$(INTEGRATION_IMAGE_TAG)",
    ) {
        problems.push(format!(
            "{INTEGRATION_FRAGMENT} does not build $(INTEGRATION_IMAGE) with docker-build"
        ));
    }
    match exported_default(fragment, "SDA_GO_TEST_IMAGE") {
        Some(image)
            if dockerfile::digest_pinned(image)
                && dockerfile::repository(image) == "ghcr.io/leinardi/swarm-device-access" => {}
        other => problems.push(format!(
            "{INTEGRATION_FRAGMENT} sets SDA_GO_TEST_IMAGE to {other:?}, want the published image \
             pinned by tag and digest"
        )),
    }
    if !fragment.contains("docker pull --quiet \x22$(SDA_GO_TEST_IMAGE)\x22") {
        problems.push(format!(
            "{INTEGRATION_FRAGMENT} does not pull $(SDA_GO_TEST_IMAGE)"
        ));
    }
    if !fragment.contains("test-integration: integration-image go-test-image ") {
        problems.push(
            "test-integration does not build the image and pull the Go image first".to_owned(),
        );
    }
    problems
}

/// The integration target tests the image the Dockerfile builds, and runs the Go daemon of the
/// identity control and the upgrade test from the last 1.x release, pinned by digest: a tag could
/// move, and a locally built Go image no longer exists.
#[test]
fn integration_images_are_the_shipped_and_the_pinned_ones() {
    let problems = image_problems(&meta::repo().read(INTEGRATION_FRAGMENT));
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// The negative control: a Go image by tag alone, one from elsewhere, an image under test that is
/// not the Dockerfile's build, and a target that skips the pull are each refused.
#[test]
fn integration_image_rules() {
    let fragment = meta::repo().read(INTEGRATION_FRAGMENT);
    let go_image = exported_default(&fragment, "SDA_GO_TEST_IMAGE")
        .expect("the fragment sets SDA_GO_TEST_IMAGE; the negative control would test nothing")
        .to_owned();
    let tag_only = go_image
        .split_once('@')
        .map_or(go_image.as_str(), |(reference, _)| reference)
        .to_owned();
    for (name, sample) in [
        (
            "a Go image by tag alone",
            fragment.replace(&go_image, &tag_only),
        ),
        (
            "a Go image from elsewhere",
            fragment.replace("ghcr.io/leinardi/", "docker.io/someone/"),
        ),
        (
            "another image under test",
            fragment.replace(
                "export SDA_RUST_TEST_IMAGE ?= $(INTEGRATION_IMAGE)",
                "export SDA_RUST_TEST_IMAGE ?= swarm-device-access:scratch",
            ),
        ),
        (
            "no pull before the suite",
            fragment.replace(
                "test-integration: integration-image go-test-image ",
                "test-integration: integration-image ",
            ),
        ),
    ] {
        assert_ne!(
            sample, fragment,
            "{name}: the sample is the fragment itself"
        );
        assert_eq!(
            image_problems(&sample).len(),
            1,
            "{name} was not refused once"
        );
    }
}
