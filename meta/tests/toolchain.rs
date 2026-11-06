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

//! The repository's tooling pins: one exact Rust release across rust-toolchain.toml, the
//! workspace's `rust-version`, the image's dhi.io/rust tag and `RUST_VERSION`, and every CI
//! toolchain step; one zig and one cargo-zigbuild between the setup-zigbuild action, the release
//! fragment and the Dockerfile, and the musl standard libraries of the fragment and the Dockerfile
//! at that exact Rust release; one exact Trivy for the release scan; one ShellCheck between the
//! hook and the CI action; the whole-tree hooks running once; and no meta-test that skips on a
//! missing tool. `ci_gates.rs` and `vacuous_flags.rs` check the rest of the gates.
//!
//! `.mk/rust-release.mk` is the one record of the zig, cargo-zigbuild and rust-std pins: the
//! action and the Dockerfile's build stage carry copies, and every copy is checked against it.
//! SSE checks the Dockerfile against the action alone and keeps no such record. The checks are
//! `repo_meta::toolchain`'s, `repo_meta::ci`'s and `repo_meta::tools`'s (extracted from this
//! file), with this repository's parameters: the exact release in `rust-version`
//! (`RustVersion::Exact`), a CI toolchain read from its input alone (`ToolchainRef::Input`) and
//! the release fragment as the zig record. Every rule has its negative control.

use repo_meta::ci::always_run_problems;
use repo_meta::toolchain::{
    RUST_RELEASE_PINS, RustVersion, ToolchainRef, ZigRecord, ci_toolchain_problems,
    image_rust_problems, release_pin_problems, rust_release, rust_std_problems,
    rust_version_problems, shellcheck_problems, toolchain_targets, trivy_problems,
    zigbuild_action_problems,
};
use repo_meta::tools::tool_lookup_problems;
use repo_meta::yaml::{self, Value};
use repo_meta::{Problem, report, strings};

const DOCKERFILE: &str = "deployments/docker/Dockerfile";
const PRE_COMMIT_CONFIG: &str = ".pre-commit-config.yaml";
const SHELLCHECK_ACTION: &str = ".github/actions/setup-shellcheck/action.yaml";
const SETUP_ZIGBUILD: &str = ".github/actions/setup-zigbuild/action.yaml";
const RUST_RELEASE_FRAGMENT: &str = ".mk/rust-release.mk";
const RELEASE_WORKFLOW: &str = ".github/workflows/release.yaml";

/// The Go tests' switch that turned a skip on a missing tool into a failure, spelled in pieces so
/// this file does not match its own check.
const REQUIRE_TOOLS_SWITCH: &str = concat!("SDA_REQUIRE", "_TOOLS");

/// The hooks that read the whole tree rather than the files pre-commit passes, so each one runs
/// on every commit and must not run twice per commit.
const ALWAYS_RUN_HOOKS: [&str; 3] = ["check-license-headers", "check-suppressions", "docs-links"];

fn parse(file: &str, content: &str) -> Value {
    yaml::parse(file, content).unwrap_or_else(|problem| panic!("{problem}"))
}

/// The exact release rust-toolchain.toml pins: `channel = "X.Y.Z"`, never a channel or a release
/// without its patch.
fn release() -> String {
    rust_release(
        "rust-toolchain.toml",
        &meta::repo().read("rust-toolchain.toml"),
    )
    .unwrap_or_else(|problem| panic!("{problem}"))
}

/// Every YAML file under .github, the workflows and the composite actions alike, parsed.
fn github_documents() -> Vec<(String, Value)> {
    let repo = meta::repo();
    let files = repo.files_under(".github", &[".yaml", ".yml"]);
    assert!(
        !files.is_empty(),
        "no YAML files under .github; the checks would see nothing"
    );
    repo.read_all(&files)
        .into_iter()
        .map(|(file, content)| {
            let document = parse(&file, &content);
            (file, document)
        })
        .collect()
}

/// Go: `TestRustLineIsOneRelease`. One exact Rust release, patch included, is shared by
/// rust-toolchain.toml's channel, the workspace's rust-version, the image's build stage and every
/// CI toolchain (each action's `toolchain` input, and any a script names), so the toolchain a
/// developer, a hook, CI and the image build with is one compiler. (SSE pins only the line in
/// `rust-version`; this workspace pins the release there too.)
#[test]
fn rust_line_is_one_release() {
    let repo = meta::repo();
    let release = release();
    let mut problems = rust_version_problems(
        "Cargo.toml",
        &repo.read("Cargo.toml"),
        &release,
        RustVersion::Exact,
    );
    problems.extend(image_rust_problems(
        DOCKERFILE,
        &repo.read(DOCKERFILE),
        &release,
        RustVersion::Exact,
    ));
    problems.extend(ci_toolchain_problems(
        &github_documents(),
        &release,
        ToolchainRef::Input,
    ));
    assert!(problems.is_empty(), "{}", report(&problems));
}

/// Go: `TestRustStepRules`, the negative control of the Rust toolchain step rule: another
/// release, a channel, a release without its patch, and no toolchain at all.
#[test]
fn rust_step_rules() {
    let document = parse(
        "sample.yaml",
        r#"jobs:
  ok:
    steps:
      - uses: dtolnay/rust-toolchain@0123456789abcdef0123456789abcdef01234567 # master
        with:
          toolchain: 1.98.1
  bad:
    steps:
      - uses: dtolnay/rust-toolchain@0123456789abcdef0123456789abcdef01234567
        with:
          toolchain: 1.98.0
      - uses: dtolnay/rust-toolchain@0123456789abcdef0123456789abcdef01234567
        with:
          toolchain: stable
      - uses: dtolnay/rust-toolchain@0123456789abcdef0123456789abcdef01234567
        with:
          toolchain: "1.98"
      - uses: dtolnay/rust-toolchain@0123456789abcdef0123456789abcdef01234567
"#,
    );
    let documents = [("sample.yaml".to_owned(), document)];
    let problems = ci_toolchain_problems(&documents, "1.98.1", ToolchainRef::Input);
    assert_eq!(problems.len(), 4, "{problems:#?}");

    // A script naming another toolchain, and a workflow with no toolchain step at all.
    let script = parse(
        "script.yaml",
        "jobs:\n  a:\n    steps:\n      - uses: dtolnay/rust-toolchain@0123456789abcdef0123456789abcdef01234567\n        with:\n          toolchain: 1.98.1\n      - run: cargo +nightly test\n",
    );
    assert_eq!(
        ci_toolchain_problems(
            &[("script.yaml".to_owned(), script)],
            "1.98.1",
            ToolchainRef::Input
        )
        .len(),
        1
    );
    let none = parse("none.yaml", "jobs:\n  a:\n    steps:\n      - run: make\n");
    assert!(
        !ci_toolchain_problems(
            &[("none.yaml".to_owned(), none)],
            "1.98.1",
            ToolchainRef::Input
        )
        .is_empty(),
        "a CI with no toolchain step was accepted"
    );
}

/// SSE's `image_toolchain_patch_is_the_standard_library_version`, the negative control of the
/// image's Rust: another release in the tag or in `RUST_VERSION`, a floating tag, and a missing
/// argument are each refused.
#[test]
fn image_rust_rules() {
    let good = "FROM --platform=$BUILDPLATFORM dhi.io/rust:1.98.1-debian13-dev AS build\n\
                ARG RUST_VERSION=1.98.1\n";
    let check =
        |sample: &str| image_rust_problems(DOCKERFILE, sample, "1.98.1", RustVersion::Exact);
    assert_eq!(check(good), Vec::<Problem>::new());
    for (name, sample, want) in [
        // Another tag is both another release than ARG RUST_VERSION's and than the toolchain's.
        (
            "another tag",
            good.replace("rust:1.98.1-", "rust:1.98.0-"),
            2,
        ),
        (
            "another standard library",
            good.replace("RUST_VERSION=1.98.1", "RUST_VERSION=1.98.0"),
            1,
        ),
        ("a floating tag", good.replace("rust:1.98.1-", "rust:1-"), 1),
        (
            "no RUST_VERSION",
            good.replace("ARG RUST_VERSION=1.98.1\n", ""),
            1,
        ),
        ("both moved together", good.replace("1.98.1", "1.98.0"), 1),
    ] {
        assert_ne!(sample, good, "{name}: the sample is the good one");
        let problems = check(&sample);
        assert_eq!(problems.len(), want, "{name}: {problems:#?}");
    }
}

/// The negative control on the real Dockerfile: a dhi.io/rust reference of another release
/// anywhere, not only in the build stage's FROM (here a COPY --from= of another toolchain), is
/// refused.
#[test]
fn image_rust_refuses_another_release_anywhere() {
    let dockerfile = meta::repo().read(DOCKERFILE);
    let release = release();
    assert!(image_rust_problems(DOCKERFILE, &dockerfile, &release, RustVersion::Exact).is_empty());
    assert!(
        dockerfile.contains("WORKDIR /src\n"),
        "WORKDIR /src is gone"
    );
    let broken = dockerfile.replacen(
        "WORKDIR /src\n",
        "WORKDIR /src\nCOPY --from=dhi.io/rust:1.97.0-debian13-dev /usr/local/rustup /usr/local/rustup\n",
        1,
    );
    let problems = image_rust_problems(DOCKERFILE, &broken, &release, RustVersion::Exact);
    assert!(
        !problems.is_empty(),
        "a COPY --from= of another dhi.io/rust release was accepted"
    );
}

/// Go: `TestRustLineReading`. The readings the Rust line rests on: a channel or a release without
/// its patch is not an exact release, and a `rust-version` of the line alone is not the release.
#[test]
fn rust_line_reading() {
    assert_eq!(
        rust_release("rust-toolchain.toml", "[toolchain]\nchannel = \"1.98.1\"\n").ok(),
        Some("1.98.1".to_owned())
    );
    for inexact in ["stable", "1.98", "nightly-2026-10-01", "1.98.1-x86_64"] {
        assert!(
            rust_release(
                "rust-toolchain.toml",
                &format!("[toolchain]\nchannel = \"{inexact}\"\n")
            )
            .is_err(),
            "{inexact:?} was accepted as an exact release"
        );
    }
    assert!(rust_release("rust-toolchain.toml", "[toolchain]\n").is_err());
    for (manifest, refused) in [
        ("[workspace.package]\nrust-version = \"1.98.1\"\n", false),
        ("[workspace.package]\nrust-version = \"1.98\"\n", true),
        ("[workspace.package]\nrust-version = \"1.98.0\"\n", true),
        ("[workspace.package]\n", true),
    ] {
        assert_eq!(
            !rust_version_problems("Cargo.toml", manifest, "1.98.1", RustVersion::Exact).is_empty(),
            refused,
            "{manifest:?}"
        );
    }
}

/// The zig and cargo-zigbuild pins between the setup-zigbuild action and the release fragment.
fn zigbuild_problems(action: &str, fragment: &str) -> Vec<Problem> {
    zigbuild_action_problems(
        SETUP_ZIGBUILD,
        action,
        ZigRecord::Fragment {
            file: RUST_RELEASE_FRAGMENT,
            content: fragment,
        },
    )
}

/// SSE's `zig_is_one_version` and `cargo_zigbuild_is_one_version`, with the release fragment in
/// place of the Dockerfile: CI's static builds (setup-zigbuild) and the record the image takes use
/// one zig and one cargo-zigbuild, from the same `x86_64` archives, so a bump that moves one copy
/// fails here rather than in the first build that runs it.
#[test]
fn zig_and_cargo_zigbuild_are_one_version() {
    let repo = meta::repo();
    let problems = zigbuild_problems(
        &repo.read(SETUP_ZIGBUILD),
        &repo.read(RUST_RELEASE_FRAGMENT),
    );
    assert!(problems.is_empty(), "{}", report(&problems));
}

/// The negative control: a moved version or digest on either side, and a pin the fragment does
/// not write, are each a problem; a pin written twice with two values is one.
#[test]
fn zigbuild_pin_rules() {
    let digest = "1cbe9df9f27e6b78d14ccbca43b6703a404ef79ef1c463de901d7f088d4e2026";
    let zigbuild_digest = "52be2cb85ad3875682564a29bac1832b2ab3db68b1c504765c92d5249cc9b96f";
    let action = format!(
        "      env:\n        ZIG_VERSION: \"0.17.0\"\n        ZIG_SHA256: {digest}\n        \
         CARGO_ZIGBUILD_VERSION: \"0.24.0\"\n        CARGO_ZIGBUILD_SHA256: {zigbuild_digest}\n"
    );
    let fragment = format!(
        "RUST_RELEASE_ZIG_VERSION := 0.17.0\nRUST_RELEASE_ZIG_SHA256_x86_64 := {digest}\n\
         RUST_RELEASE_CARGO_ZIGBUILD_VERSION := 0.24.0\n\
         RUST_RELEASE_CARGO_ZIGBUILD_SHA256_x86_64-unknown-linux-musl := {zigbuild_digest}\n"
    );
    assert_eq!(zigbuild_problems(&action, &fragment), Vec::<Problem>::new());

    let other_digest = "9e8d11661d4ae3bd57702a3832781e23ad151dde5798e16a5ccd503f65234ff8";
    for (name, action, fragment) in [
        (
            "a moved zig in the action",
            action.replace("ZIG_VERSION: \"0.17.0\"", "ZIG_VERSION: \"0.16.0\""),
            fragment.clone(),
        ),
        (
            "a moved cargo-zigbuild in the fragment",
            action.clone(),
            fragment.replace("ZIGBUILD_VERSION := 0.24.0", "ZIGBUILD_VERSION := 0.23.0"),
        ),
        (
            "another zig digest in the fragment",
            action.clone(),
            fragment.replace(digest, other_digest),
        ),
        (
            "another cargo-zigbuild digest in the action",
            action.replace(zigbuild_digest, other_digest),
            fragment.clone(),
        ),
        (
            "a fragment without the zig digest",
            action.clone(),
            fragment.replace(&format!("RUST_RELEASE_ZIG_SHA256_x86_64 := {digest}\n"), ""),
        ),
        (
            "a fragment writing zig twice",
            action.clone(),
            format!("{fragment}RUST_RELEASE_ZIG_VERSION := 0.16.0\n"),
        ),
    ] {
        let problems = zigbuild_problems(&action, &fragment);
        assert_eq!(problems.len(), 1, "{name}: {problems:#?}");
    }
}

/// The release pins the Dockerfile repeats from the fragment.
fn dockerfile_pin_problems(dockerfile: &str, fragment: &str) -> Vec<Problem> {
    release_pin_problems(
        DOCKERFILE,
        dockerfile,
        RUST_RELEASE_FRAGMENT,
        fragment,
        &RUST_RELEASE_PINS,
    )
}

/// SSE's `zig_is_one_version` and `cargo_zigbuild_is_one_version` for the image, against the
/// fragment's record: the image's build stage downloads the zig, cargo-zigbuild and rust-std
/// releases the fragment pins, each `case` arm with the fragment's digest for its archive and no
/// other arm, so the image and the release binaries are built by one toolchain; and every other
/// sha256 the Dockerfile writes is an image or frontend digest.
#[test]
fn dockerfile_pins_are_the_fragments() {
    let repo = meta::repo();
    let problems =
        dockerfile_pin_problems(&repo.read(DOCKERFILE), &repo.read(RUST_RELEASE_FRAGMENT));
    assert!(problems.is_empty(), "{}", report(&problems));
}

/// The negative control: a moved version or digest in the Dockerfile, a pin the Dockerfile does
/// not write, a swapped arm and a digest the record does not hold are each refused.
#[test]
fn dockerfile_pin_rules() {
    let repo = meta::repo();
    let dockerfile = repo.read(DOCKERFILE);
    let fragment = repo.read(RUST_RELEASE_FRAGMENT);
    assert_eq!(
        dockerfile_pin_problems(&dockerfile, &fragment),
        Vec::<Problem>::new()
    );
    let arm64_std = "7e0ad2e67817f224b7a6331e48d6397303fc050edaccf81784ed786ffddc884a";
    assert!(
        dockerfile.contains(arm64_std) && fragment.contains(arm64_std),
        "the aarch64 rust-std digest; the negative control would test nothing"
    );
    for (name, sample) in [
        (
            "a moved zig",
            dockerfile.replace("ARG ZIG_VERSION=0.17.0", "ARG ZIG_VERSION=0.16.0"),
        ),
        (
            "another aarch64 rust-std digest",
            dockerfile.replace(arm64_std, &"0".repeat(64)),
        ),
        (
            "no cargo-zigbuild version",
            dockerfile.replace("ARG CARGO_ZIGBUILD_VERSION=0.24.0\n", ""),
        ),
        (
            "a swapped rust-std arm",
            dockerfile.replace(
                "arm64) triple=aarch64-unknown-linux-musl;",
                "arm64) triple=x86_64-unknown-linux-musl;",
            ),
        ),
        (
            "a stray digest",
            format!("{dockerfile}\n# {}\n", "a".repeat(64)),
        ),
    ] {
        assert_ne!(
            sample, dockerfile,
            "{name}: the sample is the Dockerfile itself"
        );
        let problems = dockerfile_pin_problems(&sample, &fragment);
        assert_eq!(problems.len(), 1, "{name}: {problems:#?}");
    }
}

/// The musl standard libraries the image's build stage unpacks are the exact release of
/// rust-toolchain.toml, one sha256 for each of its targets, and the release binaries are built for
/// exactly those targets. (SSE's `image_toolchain_patch_is_the_standard_library_version`, against
/// the toolchain file rather than the image tag.)
#[test]
fn rust_std_is_the_rust_release() {
    let repo = meta::repo();
    let toolchain = repo.read("rust-toolchain.toml");
    let problems = rust_std_problems(
        RUST_RELEASE_FRAGMENT,
        &repo.read(RUST_RELEASE_FRAGMENT),
        &release(),
        &toolchain_targets(&toolchain),
    );
    assert!(problems.is_empty(), "{}", report(&problems));
}

/// The negative control: another release, a missing, short, doubled or extra digest, and a
/// platform list that leaves a target out are each refused.
#[test]
fn rust_std_rules() {
    let targets = toolchain_targets(
        "[toolchain]\nchannel = \"1.98.1\"\ntargets = [\"x86_64-unknown-linux-musl\", \
         \"aarch64-unknown-linux-musl\"]\n",
    );
    assert_eq!(targets.len(), 2, "{targets:?}");
    let amd64 = "bd9f35880de21dab0e387e84009c00001ae7836a4ba565cb16823f13f75f325f";
    let arm64 = "7e0ad2e67817f224b7a6331e48d6397303fc050edaccf81784ed786ffddc884a";
    let fragment = format!(
        "RUST_RELEASE_RUST_STD_VERSION := 1.98.1\n\
         RUST_RELEASE_RUST_STD_SHA256_x86_64-unknown-linux-musl := {amd64}\n\
         RUST_RELEASE_RUST_STD_SHA256_aarch64-unknown-linux-musl := {arm64}\n\
         RUST_RELEASE_PLATFORMS := amd64:x86_64-unknown-linux-musl \
         arm64:aarch64-unknown-linux-musl\n"
    );
    let check = |fragment: &str, release: &str, targets: &[String]| {
        rust_std_problems(RUST_RELEASE_FRAGMENT, fragment, release, targets)
    };
    assert_eq!(check(&fragment, "1.98.1", &targets), Vec::<Problem>::new());
    assert!(
        !check(&fragment, "1.98.1", &[]).is_empty(),
        "no target at all was accepted"
    );

    let arm64_line =
        format!("RUST_RELEASE_RUST_STD_SHA256_aarch64-unknown-linux-musl := {arm64}\n");
    for (name, sample, release) in [
        ("another release", fragment.clone(), "1.98.0"),
        (
            "no rust-std version",
            fragment.replace("RUST_RELEASE_RUST_STD_VERSION := 1.98.1\n", ""),
            "1.98.1",
        ),
        (
            "a missing digest",
            fragment.replace(&arm64_line, ""),
            "1.98.1",
        ),
        (
            "a short digest",
            fragment.replace(arm64, &arm64[1..]),
            "1.98.1",
        ),
        (
            "a doubled digest",
            format!("{fragment}{arm64_line}"),
            "1.98.1",
        ),
        (
            "a digest of another target",
            format!(
                "{fragment}RUST_RELEASE_RUST_STD_SHA256_riscv64gc-unknown-linux-musl := {arm64}\n"
            ),
            "1.98.1",
        ),
        (
            "a platform left out",
            fragment.replace(" arm64:aarch64-unknown-linux-musl", ""),
            "1.98.1",
        ),
    ] {
        let problems = check(&sample, release, &targets);
        assert_eq!(problems.len(), 1, "{name}: {problems:#?}");
    }
}

/// Go: `TestShellCheckIsOneVersion`. droast's ShellCheck bridge runs the `shellcheck` on `PATH`,
/// which CI installs with setup-shellcheck; it must be the release the shellcheck-py hook wraps
/// (its rev adds a fourth component for the wrapper), which Dependabot moves.
#[test]
fn shellcheck_is_one_version() {
    let repo = meta::repo();
    let problems = shellcheck_problems(
        PRE_COMMIT_CONFIG,
        &repo.read(PRE_COMMIT_CONFIG),
        SHELLCHECK_ACTION,
        &repo.read(SHELLCHECK_ACTION),
    );
    assert!(problems.is_empty(), "{}", report(&problems));
}

/// The negative control of the version check: a moved hook, a moved action and a missing pin are
/// each a problem.
#[test]
fn hook_version_rules() {
    let hooks = "  - repo: https://github.com/shellcheck-py/shellcheck-py\n    rev: v0.11.0.1\n";
    let shellcheck = "        SHELLCHECK_VERSION: v0.11.0\n";
    let check = |hooks: &str, action: &str| {
        shellcheck_problems(PRE_COMMIT_CONFIG, hooks, SHELLCHECK_ACTION, action)
    };
    assert_eq!(check(hooks, shellcheck), Vec::<Problem>::new());
    for (name, problems) in [
        (
            "a moved ShellCheck action",
            check(hooks, "        SHELLCHECK_VERSION: v0.10.0\n"),
        ),
        (
            "a moved ShellCheck hook",
            check(
                &hooks.replace("rev: v0.11.0.1", "rev: v0.12.0.1"),
                shellcheck,
            ),
        ),
        ("a missing ShellCheck pin", check(hooks, "")),
    ] {
        assert!(!problems.is_empty(), "{name} was accepted");
    }
}

/// SSE's `trivy_is_one_version`, for every workflow that scans (the release, which scans the
/// image): one pinned Trivy, so a finding means the same on every release and a recovery re-run.
#[test]
fn trivy_is_pinned() {
    let repo = meta::repo();
    let scanning: Vec<(String, String)> = repo
        .read_all(&repo.files_under(".github/workflows", &[".yaml", ".yml"]))
        .into_iter()
        .filter(|(_, content)| content.contains("trivy") || content.contains("TRIVY"))
        .collect();
    assert!(
        scanning.iter().any(|(file, _)| file == RELEASE_WORKFLOW),
        "{RELEASE_WORKFLOW} no longer scans; the check would see nothing"
    );
    let problems = trivy_problems(&scanning);
    assert!(problems.is_empty(), "{}", report(&problems));
}

/// The negative control: a floating version, a second pin, no setup-trivy step, a step that
/// installs its own version, and two workflows on two versions are each refused.
#[test]
fn trivy_rules() {
    let good = "env:\n  TRIVY_VERSION: v0.74.0\njobs:\n  release:\n    steps:\n      \
                - uses: aquasecurity/setup-trivy@0123456789abcdef0123456789abcdef01234567\n        \
                with:\n          version: ${{ env.TRIVY_VERSION }}\n";
    let check = |samples: &[&str]| {
        let workflows: Vec<(String, String)> = samples
            .iter()
            .enumerate()
            .map(|(index, sample)| (format!("w{index}.yaml"), (*sample).to_owned()))
            .collect();
        trivy_problems(&workflows)
    };
    assert_eq!(check(&[good]), Vec::<Problem>::new());
    for (name, sample) in [
        (
            "a floating version",
            good.replace("TRIVY_VERSION: v0.74.0", "TRIVY_VERSION: latest"),
        ),
        (
            "a second pin",
            good.replace("jobs:", "  OTHER:\n    TRIVY_VERSION: v0.73.0\njobs:"),
        ),
        (
            "no setup-trivy step",
            good.replace("aquasecurity/setup-trivy", "some/other-action"),
        ),
        (
            "a step with its own version",
            good.replace("${{ env.TRIVY_VERSION }}", "v0.73.0"),
        ),
    ] {
        assert_eq!(check(&[&sample]).len(), 1, "{name} was not refused once");
    }
    let other = good.replace("v0.74.0", "v0.73.0");
    assert_eq!(check(&[good, &other]).len(), 1, "two Trivy versions");
}

/// The hooks of a pre-commit configuration, parsed.
fn hooks(content: &str) -> Vec<(String, Value)> {
    yaml::pre_commit_hooks(PRE_COMMIT_CONFIG, &parse(PRE_COMMIT_CONFIG, content))
        .unwrap_or_else(|problem| panic!("{problem}"))
}

/// A whole-tree hook takes no file list and always runs, because a commit that only deletes a
/// file passes no file to it; and it is pinned to the pre-commit stage, because `always_run`
/// makes a hook eligible at every installed stage, commit-msg included. (SSE's
/// `always_run_hooks_run_once`; the Go tests have no counterpart, the hooks' comments state it.)
#[test]
fn always_run_hooks_run_once() {
    let problems = always_run_problems(
        &hooks(&meta::repo().read(PRE_COMMIT_CONFIG)),
        &ALWAYS_RUN_HOOKS,
    );
    assert!(problems.is_empty(), "{}", report(&problems));
}

/// The negative control: a whole-tree hook that is missing, doubled, passed file names, not
/// always run, or eligible at another stage is refused.
#[test]
fn always_run_rules() {
    let hook = |id: &str, settings: &str| {
        format!("      - id: {id}\n        entry: x\n        language: system\n{settings}")
    };
    let good = "        pass_filenames: false\n        always_run: true\n        stages: \
                [pre-commit]\n";
    let config = |docs_links: &str| {
        format!(
            "repos:\n  - repo: local\n    hooks:\n{}{}{docs_links}",
            hook("check-license-headers", good),
            hook("check-suppressions", good),
        )
    };
    assert_eq!(
        always_run_problems(
            &hooks(&config(&hook("docs-links", good))),
            &ALWAYS_RUN_HOOKS
        ),
        Vec::<Problem>::new()
    );
    for (name, docs_links) in [
        ("a missing hook", String::new()),
        (
            "a doubled hook",
            format!("{}{}", hook("docs-links", good), hook("docs-links", good)),
        ),
        (
            "file names passed",
            hook(
                "docs-links",
                &good.replace("pass_filenames: false", "pass_filenames: true"),
            ),
        ),
        (
            "not always run",
            hook(
                "docs-links",
                &good.replace("always_run: true", "always_run: false"),
            ),
        ),
        (
            "every stage",
            hook(
                "docs-links",
                &good.replace("stages: [pre-commit]", "stages: [pre-commit, commit-msg]"),
            ),
        ),
    ] {
        assert!(
            !always_run_problems(&hooks(&config(&docs_links)), &ALWAYS_RUN_HOOKS).is_empty(),
            "{name} was accepted"
        );
    }
}

/// Go: `TestToolGateListIsClosed`. No meta-test may skip on a missing tool: none looks a tool up
/// on `PATH`, since a skip on a missing tool is a pass CI cannot turn into a failure, and none
/// reads the Go tests' switch that turns such a skip into a failure. A test that needs an
/// external tool fails without it.
#[test]
fn no_meta_test_skips_on_a_missing_tool() {
    let repo = meta::repo();
    let sources = repo.files_under("meta", &[".rs"]);
    assert!(
        sources.iter().any(|path| path == "meta/tests/toolchain.rs"),
        "the walk did not reach meta/tests: {sources:?}"
    );
    let problems = tool_lookup_problems(&repo.read_all(&sources), &[REQUIRE_TOOLS_SWITCH]);
    assert!(problems.is_empty(), "{}", report(&problems));

    // The negative control. The samples are split so this file does not match its own check.
    for sample in [
        concat!("let found = env::split", "_paths(&paths);"),
        concat!("std::env::var", "_os(\x22PATH\x22)"),
        concat!("Command::new(\x22wh", "ich\x22)"),
        concat!("env::var(\x22SDA_REQUIRE", "_TOOLS\x22)"),
    ] {
        let files = [("meta/tests/x.rs".to_owned(), sample.to_owned())];
        assert_eq!(
            strings(&tool_lookup_problems(&files, &[REQUIRE_TOOLS_SWITCH])),
            [
                "meta/tests/x.rs: looks a tool up on PATH or reads a skip switch; a meta-test \
              needing a tool must fail without it"
            ],
            "{sample:?} was not recognised"
        );
    }
    // testenv-core's tool gate looks a tool up too, whatever the switches; spelled in pieces so
    // this file does not match its own check.
    let gate = [(
        "meta/tests/x.rs".to_owned(),
        concat!(
            "use testenv_core::tool",
            "gate::Gate;\nfn droast(gate: &Gate) { gate.look",
            "up(\x22droast\x22); }\n"
        )
        .to_owned(),
    )];
    for switches in [&[][..], &[REQUIRE_TOOLS_SWITCH][..]] {
        assert_eq!(
            tool_lookup_problems(&gate, switches).len(),
            1,
            "a testenv-core tool-gate lookup was accepted with switches {switches:?}"
        );
    }
}
