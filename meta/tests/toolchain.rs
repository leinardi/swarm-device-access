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
// Adapted from swarm-scheduler-exporter/meta/tests/toolchain.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The repository's tooling pins, each one value everywhere it is written: the exact Rust release
//! (`rust-toolchain.toml`, the workspace's `rust-version`, the image's dhi.io/rust tag and
//! `RUST_VERSION`, and the fragment's `rust-std` release); zig and cargo-zigbuild between the
//! setup-zigbuild action, the release fragment and the Dockerfile, with their archive digests, and
//! the musl standard libraries' digests between the fragment and the Dockerfile; one ShellCheck
//! between the hook and the CI action; and an exact Trivy for the release scan. `ci_gates.rs` and
//! `vacuous_flags.rs` check the rest of the gates.
//!
//! `.mk/rust-release.mk` is the one record of the zig, cargo-zigbuild and `rust-std` pins: the
//! action and the Dockerfile's build stage carry copies, and every copy is checked against it. The
//! checks are `repo_meta::toolchain::one_value`'s over [`PinSource`]s, and each has a negative
//! control that runs it on an altered copy. CI installs the toolchain from `rust-toolchain.toml`,
//! so no workflow repeats the release.

use repo_meta::toolchain::{PinSource, one_value, rust_release, toolchain_targets};
use repo_meta::{Problem, report};

const TOOLCHAIN: &str = "rust-toolchain.toml";
const MANIFEST: &str = "Cargo.toml";
const DOCKERFILE: &str = "deployments/docker/Dockerfile";
const FRAGMENT: &str = ".mk/rust-release.mk";
const ZIGBUILD_ACTION: &str = ".github/actions/setup-zigbuild/action.yaml";
const SHELLCHECK_ACTION: &str = ".github/actions/setup-shellcheck/action.yaml";
const HOOKS: &str = ".pre-commit-config.yaml";
const RELEASE_WORKFLOW: &str = ".github/workflows/release.yaml";

/// The musl archives' targets of the fragment and the Dockerfile, with the Dockerfile's `case` arm
/// that names each: (target, arm text before the digest).
const ZIGBUILD_ARMS: [(&str, &str); 2] = [
    (
        "x86_64-unknown-linux-musl",
        "zigbuild_asset=x86_64-unknown-linux-musl; zigbuild_sha256=",
    ),
    (
        "aarch64-unknown-linux-gnu",
        "zigbuild_asset=aarch64-unknown-linux-gnu; zigbuild_sha256=",
    ),
];
const STD_ARMS: [(&str, &str); 2] = [
    (
        "x86_64-unknown-linux-musl",
        "triple=x86_64-unknown-linux-musl; std_sha256=",
    ),
    (
        "aarch64-unknown-linux-musl",
        "triple=aarch64-unknown-linux-musl; std_sha256=",
    ),
];

/// The text of every file a pin is written in.
#[derive(Clone)]
struct Files {
    toolchain: String,
    manifest: String,
    dockerfile: String,
    fragment: String,
    zigbuild_action: String,
    shellcheck_action: String,
    hooks: String,
    release: String,
}

impl Files {
    fn read() -> Self {
        let repo = meta::repo();
        Self {
            toolchain: repo.read(TOOLCHAIN),
            manifest: repo.read(MANIFEST),
            dockerfile: repo.read(DOCKERFILE),
            fragment: repo.read(FRAGMENT),
            zigbuild_action: repo.read(ZIGBUILD_ACTION),
            shellcheck_action: repo.read(SHELLCHECK_ACTION),
            hooks: repo.read(HOOKS),
            release: repo.read(RELEASE_WORKFLOW),
        }
    }
}

/// A fragment variable's value: `NAME := value`.
fn fragment_pin(files: &Files, name: &str) -> PinSource {
    PinSource::new(
        FRAGMENT,
        &files.fragment,
        &format!(r"(?m)^{}\s*:=\s*(\S+)\s*$", regex_escape(name)),
    )
}

/// The `name` without regex metacharacters it could carry (`-` and `_` need none).
fn regex_escape(name: &str) -> String {
    name.replace('.', r"\.")
}

/// A Dockerfile pin: the text of its `pattern`'s first group.
fn dockerfile_pin(files: &Files, pattern: &str) -> PinSource {
    PinSource::new(DOCKERFILE, &files.dockerfile, pattern)
}

/// The exact Rust release, in every file that writes it.
fn rust_release_problems(files: &Files) -> Vec<Problem> {
    one_value(
        "the Rust release",
        &[
            PinSource::rust_channel(TOOLCHAIN, &files.toolchain),
            PinSource::rust_version(MANIFEST, &files.manifest),
            PinSource::rust_image_tag(DOCKERFILE, &files.dockerfile),
            PinSource::rust_version_arg(DOCKERFILE, &files.dockerfile),
            fragment_pin(files, "RUST_RELEASE_RUST_STD_VERSION"),
        ],
        &[],
    )
}

/// zig between the action, the fragment and the Dockerfile.
fn zig_problems(files: &Files) -> Vec<Problem> {
    one_value(
        "zig",
        &[
            PinSource::new(
                ZIGBUILD_ACTION,
                &files.zigbuild_action,
                r#"(?m)^\s*ZIG_VERSION:\s*"?([^"\s]+)"?\s*$"#,
            ),
            fragment_pin(files, "RUST_RELEASE_ZIG_VERSION"),
            dockerfile_pin(files, r"(?m)^ARG ZIG_VERSION=(\S+)\s*$"),
        ],
        &[],
    )
}

/// cargo-zigbuild between the action, the fragment and the Dockerfile.
fn cargo_zigbuild_problems(files: &Files) -> Vec<Problem> {
    one_value(
        "cargo-zigbuild",
        &[
            PinSource::new(
                ZIGBUILD_ACTION,
                &files.zigbuild_action,
                r#"(?m)^\s*CARGO_ZIGBUILD_VERSION:\s*"?([^"\s]+)"?\s*$"#,
            ),
            fragment_pin(files, "RUST_RELEASE_CARGO_ZIGBUILD_VERSION"),
            dockerfile_pin(files, r"(?m)^ARG CARGO_ZIGBUILD_VERSION=(\S+)\s*$"),
        ],
        &[],
    )
}

/// Every archive digest the fragment records, against the copies of the action and the
/// Dockerfile: zig for each build platform, cargo-zigbuild for each, rust-std for each target.
fn digest_problems(files: &Files) -> Vec<Problem> {
    let mut problems = one_value(
        "the zig x86_64 digest",
        &[
            PinSource::new(
                ZIGBUILD_ACTION,
                &files.zigbuild_action,
                r"(?m)^\s*ZIG_SHA256:\s*([0-9a-f]{64})\s*$",
            ),
            fragment_pin(files, "RUST_RELEASE_ZIG_SHA256_x86_64"),
            dockerfile_pin(files, r"zig_arch=x86_64; zig_sha256=([0-9a-f]{64})"),
        ],
        &[],
    );
    problems.extend(one_value(
        "the zig aarch64 digest",
        &[
            fragment_pin(files, "RUST_RELEASE_ZIG_SHA256_aarch64"),
            dockerfile_pin(files, r"zig_arch=aarch64; zig_sha256=([0-9a-f]{64})"),
        ],
        &[],
    ));
    for (index, (target, arm)) in ZIGBUILD_ARMS.iter().enumerate() {
        let mut sources = vec![
            fragment_pin(
                files,
                &format!("RUST_RELEASE_CARGO_ZIGBUILD_SHA256_{target}"),
            ),
            dockerfile_pin(files, &format!("{arm}([0-9a-f]{{64}})")),
        ];
        if index == 0 {
            sources.push(PinSource::new(
                ZIGBUILD_ACTION,
                &files.zigbuild_action,
                r"(?m)^\s*CARGO_ZIGBUILD_SHA256:\s*([0-9a-f]{64})\s*$",
            ));
        }
        problems.extend(one_value(
            &format!("the cargo-zigbuild digest for {target}"),
            &sources,
            &[],
        ));
    }
    for (target, arm) in STD_ARMS {
        problems.extend(one_value(
            &format!("the rust-std digest for {target}"),
            &[
                fragment_pin(files, &format!("RUST_RELEASE_RUST_STD_SHA256_{target}")),
                dockerfile_pin(files, &format!("{arm}([0-9a-f]{{64}})")),
            ],
            &[],
        ));
    }
    problems
}

/// The ShellCheck release of the hook (its rev adds a fourth component for the wrapper, so only
/// the first three count) and of the action that installs it.
fn shellcheck_problems(files: &Files) -> Vec<Problem> {
    one_value(
        "ShellCheck",
        &[
            PinSource::new(
                HOOKS,
                &files.hooks,
                r"repo: https://github\.com/shellcheck-py/shellcheck-py\s+rev:\s*(v\d+\.\d+\.\d+)",
            ),
            PinSource::new(
                SHELLCHECK_ACTION,
                &files.shellcheck_action,
                r#"SHELLCHECK_VERSION:\s*["']?(v\d+\.\d+\.\d+)"#,
            ),
        ],
        &[],
    )
}

/// Trivy is pinned to an exact release, not a floating tag, so a finding means the same on every
/// release and every recovery re-run.
fn trivy_problems(files: &Files) -> Vec<Problem> {
    one_value(
        "Trivy",
        &[PinSource::new(
            RELEASE_WORKFLOW,
            &files.release,
            r"(?m)^\s*TRIVY_VERSION:\s*(v\d+\.\d+\.\d+)\s*$",
        )],
        &[],
    )
}

/// A check over the files' text.
type Check = fn(&Files) -> Vec<Problem>;

fn assert_none(problems: &[Problem]) {
    assert!(problems.is_empty(), "{}", report(problems));
}

#[test]
fn rust_release_is_one_value() {
    let files = Files::read();
    assert_none(&rust_release_problems(&files));
    // The pin is an exact release, never a channel or a line.
    rust_release(TOOLCHAIN, &files.toolchain).unwrap_or_else(|problem| panic!("{problem}"));
}

#[test]
fn every_toolchain_target_has_a_rust_std_digest() {
    let files = Files::read();
    let targets = toolchain_targets(&files.toolchain);
    assert!(!targets.is_empty(), "{TOOLCHAIN} names no target");
    for target in targets {
        assert!(
            files
                .fragment
                .contains(&format!("RUST_RELEASE_RUST_STD_SHA256_{target} :=")),
            "{FRAGMENT} records no rust-std digest for {target}"
        );
    }
}

#[test]
fn zig_and_cargo_zigbuild_are_one_version() {
    let files = Files::read();
    assert_none(&zig_problems(&files));
    assert_none(&cargo_zigbuild_problems(&files));
}

#[test]
fn archive_digests_are_the_fragments() {
    assert_none(&digest_problems(&Files::read()));
}

#[test]
fn shellcheck_is_one_version() {
    assert_none(&shellcheck_problems(&Files::read()));
}

#[test]
fn trivy_is_pinned() {
    assert_none(&trivy_problems(&Files::read()));
}

/// The negative controls: a pin bumped in one file only, and a pin a file no longer writes, are
/// each refused.
#[test]
fn a_pin_changed_in_one_file_is_refused() {
    let files = Files::read();
    let release = rust_release(TOOLCHAIN, &files.toolchain).expect("an exact release");
    let other = "0.0.1";
    let digest_x86 = "bd9f35880de21dab0e387e84009c00001ae7836a4ba565cb16823f13f75f325f";
    let flipped = format!("{}0", &digest_x86[..63]);
    let cases: Vec<(&str, Files, Check)> = vec![
        (
            "the toolchain on another release",
            Files {
                toolchain: files.toolchain.replace(&release, other),
                ..files.clone()
            },
            rust_release_problems,
        ),
        (
            "the image on another release",
            Files {
                dockerfile: files.dockerfile.replace(
                    &format!("ARG RUST_VERSION={release}"),
                    &format!("ARG RUST_VERSION={other}"),
                ),
                ..files.clone()
            },
            rust_release_problems,
        ),
        (
            "the rust-std release",
            Files {
                fragment: files.fragment.replace(
                    &format!("RUST_RELEASE_RUST_STD_VERSION := {release}"),
                    &format!("RUST_RELEASE_RUST_STD_VERSION := {other}"),
                ),
                ..files.clone()
            },
            rust_release_problems,
        ),
        (
            "the Dockerfile's zig",
            Files {
                dockerfile: files
                    .dockerfile
                    .replace("ARG ZIG_VERSION=0.17.0", "ARG ZIG_VERSION=0.16.0"),
                ..files.clone()
            },
            zig_problems,
        ),
        (
            "the action's cargo-zigbuild",
            Files {
                zigbuild_action: files.zigbuild_action.replace(
                    "CARGO_ZIGBUILD_VERSION: \"0.24.0\"",
                    "CARGO_ZIGBUILD_VERSION: \"0.23.0\"",
                ),
                ..files.clone()
            },
            cargo_zigbuild_problems,
        ),
        (
            "a rust-std digest in the fragment",
            Files {
                fragment: files.fragment.replace(digest_x86, &flipped),
                ..files.clone()
            },
            digest_problems,
        ),
        (
            "a rust-std digest the Dockerfile no longer writes",
            Files {
                dockerfile: files.dockerfile.replace("std_sha256=", "std_digest="),
                ..files.clone()
            },
            digest_problems,
        ),
        (
            "the hook's ShellCheck",
            Files {
                hooks: files.hooks.replace("rev: v0.11.0.1", "rev: v0.12.0.1"),
                ..files.clone()
            },
            shellcheck_problems,
        ),
        (
            "a floating Trivy",
            Files {
                release: files
                    .release
                    .replace("TRIVY_VERSION: v0.74.0", "TRIVY_VERSION: latest"),
                ..files.clone()
            },
            trivy_problems,
        ),
    ];
    for (name, changed, check) in cases {
        assert!(!check(&changed).is_empty(), "{name} was accepted");
    }
}
