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

//! The line between the plain `cargo test` and the Docker-driving integration suite: the plain
//! test targets (`make rust-test`, the cargo-test hook, the rust CI job) run without Docker, so
//! everything that reaches a daemon sits behind testenv's `integration` feature, every testenv
//! test target requires that feature, no other source drives Docker, and no plain test command
//! turns the feature on. Ported from this project's Go meta-test `TestUntaggedTestsAvoidDocker`,
//! which read Go build tags instead.
//!
//! Where SSE scans the ungated modules lib.rs declares, this port scans every testenv source
//! outside a gated module, lib.rs included, and checks every test target the manifest declares;
//! as in SSE, the suite itself must be a test target named `integration` that requires the
//! feature, so none of the checks passes on nothing. The checks are `repo_meta::integration`'s
//! (extracted from this file); the images the integration target runs stay this repository's.

use std::collections::BTreeSet;

use repo_meta::dockerfile;
use repo_meta::integration::{IntegrationGate, cargo_test_commands, docker_drivers};
use repo_meta::{report, strings};

const HARNESS_LIB: &str = "testenv/src/lib.rs";
const INTEGRATION_FRAGMENT: &str = ".mk/integration-test.mk";

/// The harness, its feature and suite, and the only files that may run an integration test
/// command: the integration target and workflow.
const GATE: IntegrationGate<'static> = IntegrationGate {
    feature: "integration",
    src_dir: "testenv/src",
    manifest: "testenv/Cargo.toml",
    suite: "integration",
    integration_files: &[
        ".mk/integration-test.mk",
        ".github/workflows/integration.yaml",
    ],
};

/// The Docker SDK whose use, beside the docker CLI, counts as driving Docker. The daemon's own
/// Engine client (docker-client, on bollard, which `src/docker` and the launcher's engine name)
/// talks to a fake Engine in the plain tests, so it is not one.
const CLIENTS: &[&str] = &["bollard"];

/// Go: `TestIntegrationTreesAreTagged`. Every harness source built without the `integration`
/// feature stays away from Docker, and every harness test target requires the feature.
#[test]
fn integration_tree_is_gated() {
    let repo = meta::repo();
    let paths = repo.files_under(GATE.src_dir, &[".rs"]);
    let ungated = GATE.ungated_sources(&repo.read(HARNESS_LIB), &paths);
    assert!(
        ungated.iter().any(|path| path == HARNESS_LIB),
        "{HARNESS_LIB} was not scanned; the check would pass on nothing"
    );
    let mut problems = docker_drivers(&repo.read_all(&ungated), CLIENTS);
    problems.extend(GATE.manifest_problems(&repo.read(GATE.manifest)));
    assert!(problems.is_empty(), "{}", report(&problems));
}

/// Go: `TestUntaggedTestsAvoidDocker`. No source of the daemon, its tests or the meta crate
/// reaches Docker: the daemon's own client talks to a fake Engine in its tests, and only the
/// harness behind the feature drives a daemon.
#[test]
fn plain_tests_avoid_docker() {
    let repo = meta::repo();
    let mut paths = Vec::new();
    for tree in ["src", "tests", "meta"] {
        paths.extend(repo.files_under(tree, &[".rs"]));
    }
    assert!(
        paths.len() > 10,
        "only {} sources found; the check would pass on nothing",
        paths.len()
    );
    // The daemon's client and the launcher's engine are the SDK's users in the plain build, and
    // `docker_boundary.rs` pins that they are the only ones.
    paths.retain(|path| {
        !path.starts_with("src/docker/") && !path.starts_with("src/launcher/engine")
    });
    let problems = docker_drivers(&repo.read_all(&paths), CLIENTS);
    assert!(problems.is_empty(), "{}", report(&problems));
}

/// The plain test targets never turn the feature on: no `cargo test` in the Makefile, the make
/// fragments, the pre-commit configuration or a workflow enables it, outside the integration
/// fragment and workflow.
#[test]
fn plain_test_targets_leave_the_feature_off() {
    let repo = meta::repo();
    let mut files = vec!["Makefile".to_owned(), ".pre-commit-config.yaml".to_owned()];
    files.extend(repo.files_under(".mk", &[".mk"]));
    files.extend(repo.files_under(".github", &[".yaml", ".yml"]));
    let mut commands = 0;
    let mut problems = Vec::new();
    for (file, content) in repo.read_all(&files) {
        commands += cargo_test_commands(&content);
        problems.extend(GATE.plain_test_problems(&file, &content));
    }
    assert!(
        commands >= 3,
        "only {commands} cargo test commands found (make rust-test, the cargo-test hook, the rust \
         job); the check would pass on nothing"
    );
    assert!(problems.is_empty(), "{}", report(&problems));
}

/// Go: `TestDockerCheckRecognizesItsTargets`. The negative controls: a gated declaration is
/// recognised and an ungated one is scanned, a manifest whose test target or default features let
/// the suite into a plain run is refused, and a plain test command that turns the feature on is
/// reported outside the integration files only.
#[test]
fn separation_rules_are_recognized() {
    let lib = "#[cfg(feature = \x22integration\x22)]\nmod dind;\n\
               #[cfg(feature = \x22integration\x22)]\npub mod context;\n\
               mod error;\npub mod redact;\n#[cfg(test)]\nmod tests;\n";
    assert_eq!(
        GATE.gated_modules(lib),
        BTreeSet::from(["context".to_owned(), "dind".to_owned()])
    );
    let paths: Vec<String> = [
        "testenv/src/lib.rs",
        "testenv/src/dind.rs",
        "testenv/src/context/mod.rs",
        "testenv/src/error.rs",
        "testenv/src/redact/tests.rs",
    ]
    .map(str::to_owned)
    .to_vec();
    assert_eq!(
        GATE.ungated_sources(lib, &paths),
        [
            "testenv/src/lib.rs",
            "testenv/src/error.rs",
            "testenv/src/redact/tests.rs"
        ]
    );

    let good = "[features]\nintegration = []\n\n[[test]]\nname = \x22integration\x22\n\
                required-features = [\x22integration\x22]\n";
    assert!(GATE.manifest_problems(good).is_empty());
    for (name, manifest) in [
        ("no feature", "[features]\n".to_owned()),
        (
            "on by default",
            "[features]\ndefault = [\x22integration\x22]\nintegration = []\n".to_owned(),
        ),
        (
            "no integration test target",
            "[features]\nintegration = []\n".to_owned(),
        ),
        (
            "a test target without the feature",
            good.replace("required-features = [\x22integration\x22]\n", ""),
        ),
        (
            "a test target with another feature",
            good.replace(
                "[\x22integration\x22]\n",
                "[\x22integration\x22, \x22x\x22]\n",
            ),
        ),
    ] {
        assert!(
            !GATE.manifest_problems(&manifest).is_empty(),
            "{name} was accepted"
        );
    }

    for line in [
        "\t$(CARGO) test --workspace --locked --all-features",
        "        run: cargo test --workspace --locked --features integration",
        "        entry: cargo test --locked -p testenv -F testenv/integration",
        "\tcargo test --features=integration",
    ] {
        assert_eq!(
            GATE.plain_test_problems(".mk/rust.mk", line).len(),
            1,
            "{line:?} was accepted in a plain target"
        );
        assert!(
            GATE.plain_test_problems(".mk/integration-test.mk", line)
                .is_empty(),
            "{line:?} was refused in the integration fragment"
        );
    }
    for line in [
        "\t$(CARGO) test --workspace --locked",
        "\t$(CARGO) clippy --workspace --all-targets --all-features --locked -- -D warnings",
        "        entry: cargo test --locked -p meta --test docs",
    ] {
        assert!(
            GATE.plain_test_problems(".mk/rust.mk", line).is_empty(),
            "{line:?} was refused"
        );
    }
}

/// The negative control of the gate's module reading: a module declared a second time without the
/// gate (a stub for the plain build) is compiled into the plain test, so it is not gated and its
/// files are scanned.
#[test]
fn a_module_declared_again_without_the_gate_is_scanned() {
    let paths: Vec<String> = [
        "testenv/src/lib.rs",
        "testenv/src/dind.rs",
        "testenv/src/context/mod.rs",
        "testenv/src/error.rs",
        "testenv/src/redact/tests.rs",
    ]
    .map(str::to_owned)
    .to_vec();
    let doubled = "#[cfg(feature = \x22integration\x22)]\nmod dind;\n\
                   #[cfg(not(feature = \x22integration\x22))]\nmod dind;\n\
                   #[cfg(feature = \x22integration\x22)]\npub mod context;\n";
    assert_eq!(
        GATE.gated_modules(doubled),
        BTreeSet::from(["context".to_owned()])
    );
    assert_eq!(
        GATE.ungated_sources(doubled, &paths),
        [
            "testenv/src/lib.rs",
            "testenv/src/dind.rs",
            "testenv/src/error.rs",
            "testenv/src/redact/tests.rs"
        ]
    );
}

/// The negative control of the Docker scan: both ways of reaching Docker are found, in code and
/// not in a comment or a string, and a scan of nothing is a problem.
#[test]
fn docker_drivers_are_found_in_code_only() {
    let files = [
        ("a.rs".to_owned(), "use bollard::Docker;".to_owned()),
        (
            "b.rs".to_owned(),
            "let out = Command::new(\x22docker\x22).arg(\x22ps\x22);".to_owned(),
        ),
        (
            "c.rs".to_owned(),
            "let out = Command::new(\x22git\x22); // docker".to_owned(),
        ),
        (
            "d.rs".to_owned(),
            "let fixture = \x22use bollard::Docker;\x22; // bollard::Docker".to_owned(),
        ),
        (
            "e.rs".to_owned(),
            "    let client = bollard::Docker::connect();".to_owned(),
        ),
    ];
    let drivers: Vec<String> = docker_drivers(&files, CLIENTS)
        .into_iter()
        .map(|problem| problem.location)
        .collect();
    assert_eq!(drivers, ["a.rs", "b.rs", "e.rs"]);
    assert_eq!(
        strings(&docker_drivers(&[], CLIENTS)),
        ["no file or no Docker client to look for; the check would pass on nothing"]
    );
}

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
