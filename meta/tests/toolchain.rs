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
//! (`rust-toolchain.toml`'s channel, the workspace's `rust-version`, the image's dhi.io/rust tag
//! and `ARG RUST_VERSION`, and the Makefile's `rust-std` release); zig and cargo-zigbuild between
//! the Makefile, the Dockerfile and the setup-zigbuild steps of CI and the release, with
//! the archive digests between the Makefile and the Dockerfile; and one `ShellCheck` between the
//! hook and CI. Trivy is never pinned. `ci_gates.rs` and `vacuous_flags.rs` check the rest of the
//! gates.
//!
//! The Makefile is the one record of the zig, cargo-zigbuild and `rust-std` pins: the
//! Dockerfile's build stage carries copies, and every copy is checked against it. CI and the
//! release pass the versions to gh-composite-actions' setup-zigbuild, whose default digests are
//! Makefile's `x86_64` ones; a version moved without its digest fails that action's check. The
//! checks are `repo_meta::toolchain::one_value`'s over [`PinSource`]s, and each has a negative
//! control that runs it on an altered copy of the real files. CI installs the toolchain from
//! `rust-toolchain.toml`, so no workflow repeats the release.

use repo_meta::toolchain::{PinSource, one_value, rust_release, toolchain_targets};
use repo_meta::{Problem, report};

/// Runs make against the repository without inheriting flags from a parent make or commit hook.
fn make() -> std::process::Command {
    let mut command = std::process::Command::new("make");
    command
        .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/.."))
        .arg("--no-print-directory")
        .env_remove("MAKEFLAGS")
        .env_remove("MFLAGS")
        .env_remove("MAKEOVERRIDES");
    command
}

#[test]
fn release_pins_ignore_environment_but_keep_command_line_overrides() {
    let pins = meta::repo().read("Makefile");
    for line in pins
        .lines()
        .filter(|line| line.starts_with("RUST_RELEASE_"))
    {
        let Some((name, _)) = line.split_once(" := ") else {
            continue;
        };
        let print = format!("--eval=print-release-pin: ; @printf '%s' '$({name})'");
        let baseline = make()
            .arg(&print)
            .arg("print-release-pin")
            .output()
            .unwrap_or_else(|error| panic!("cannot run make: {error}"));
        assert!(baseline.status.success(), "{name}: {baseline:?}");
        for (arguments, expected) in [
            (Vec::new(), baseline.stdout.as_slice()),
            (vec![format!("{name}=override")], b"override".as_slice()),
        ] {
            let output = make()
                .arg(&print)
                .arg("print-release-pin")
                .args(arguments)
                .env(name, "environment-override")
                .output()
                .unwrap_or_else(|error| panic!("cannot run make: {error}"));
            assert!(output.status.success(), "{name}: {output:?}");
            assert_eq!(output.stdout, expected, "{name}");
        }
    }
}

#[test]
fn image_gates_refuse_empty_targets() {
    for target in ["image-build-prebuilt", "image-test", "image-build-test"] {
        let output = make()
            .args([target, "DOCKER_TARGETS="])
            .output()
            .unwrap_or_else(|error| panic!("cannot run make: {error}"));
        assert!(!output.status.success(), "{target} passed on no images");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("DOCKER_TARGETS is empty"),
            "{output:?}"
        );
    }
    let output = make()
        .args(["image-targets-check", "DOCKER_TARGETS=swarm-device-access"])
        .output()
        .unwrap_or_else(|error| panic!("cannot run make: {error}"));
    assert!(output.status.success(), "a nonempty image set: {output:?}");
}

const TOOLCHAIN: &str = "rust-toolchain.toml";
const MANIFEST: &str = "Cargo.toml";
const DOCKERFILE: &str = "deployments/docker/Dockerfile";
const RELEASE_PINS: &str = "Makefile";
const HOOKS: &str = ".pre-commit-config.yaml";
const CI_WORKFLOW: &str = ".github/workflows/ci.yaml";
const RELEASE_WORKFLOW: &str = ".github/workflows/release.yaml";
const SCHEDULED_SCAN: &str = ".github/workflows/scheduled-scan.yaml";

/// The `zig-version` input of gh-composite-actions' setup-zigbuild, in CI and the release.
const ZIG_ACTION: &str = r#"(?m)^\s*zig-version:\s*"?([\d.]+)"#;
const ZIGBUILD_ACTION: &str = r#"(?m)^\s*cargo-zigbuild-version:\s*"?([\d.]+)"#;
const ZIG_DOCKERFILE: &str = r"(?m)^ARG ZIG_VERSION=(\S+)\s*$";
const ZIGBUILD_DOCKERFILE: &str = r"(?m)^ARG CARGO_ZIGBUILD_VERSION=(\S+)\s*$";
/// shellcheck-py adds a fourth component of its own (v0.11.0.1), so only the first three count.
const SHELLCHECK_HOOK: &str =
    r"repo: https://github\.com/shellcheck-py/shellcheck-py\s+rev:\s*v?(\d+\.\d+\.\d+)(?:\.\d+)?";
/// The `version` input of every gh-composite-actions setup-shellcheck step in CI.
const SHELLCHECK_ACTION: &str =
    r"setup-shellcheck@[0-9a-f]{40}[^\n]*\n\s*with:\s*\n\s*version:\s*v?(\d+\.\d+\.\d+)";
/// A Trivy version written down anywhere under .github: trivy-gate's `trivy-version` input, a
/// `TRIVY_VERSION`, or a setup-trivy step of its own.
const TRIVY_PIN: &str = r"(?m)trivy-version:|TRIVY_VERSION|aquasecurity/setup-trivy";

/// The cargo-zigbuild archives' build hosts in the Makefile and the Dockerfile, with the
/// Dockerfile's `case` arm that names each: (target, arm text before the digest).
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
/// The musl standard libraries' targets, likewise.
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

/// `content` with its first `from` replaced by `to`; `from` must be there, so an edit cannot
/// silently change nothing.
fn edited(content: &str, from: &str, to: &str) -> String {
    assert!(content.contains(from), "{from:?} is gone");
    content.replacen(from, to, 1)
}

/// What the pin checks read of a file: `edit` applied to its path and its content.
type Edit<'edit> = &'edit dyn Fn(&str, String) -> String;

/// The content of `file`, which `edit` has applied.
fn read(edit: Edit, file: &str) -> String {
    edit(file, meta::repo().read(file))
}

/// A source of `pattern` in `file`, whose content `edit` has applied.
fn source(edit: Edit, file: &str, pattern: &str) -> PinSource {
    PinSource::new(file, &read(edit, file), pattern)
}

/// A repository-authoritative release pin: `NAME := value`.
fn release_pin(edit: Edit, name: &str) -> PinSource {
    source(
        edit,
        RELEASE_PINS,
        &format!(r"(?m)^{}\s*:=\s*(\S+)\s*$", name.replace('.', r"\.")),
    )
}

/// Every pin the repository writes in more than one file, each one value: the exact Rust release,
/// zig, cargo-zigbuild, the archive digests and `ShellCheck`. `edit` is applied to the content of
/// each file read.
fn pin_problems(edit: Edit) -> Vec<Problem> {
    let dockerfile = read(edit, DOCKERFILE);
    let mut problems = one_value(
        "the Rust release",
        &[
            PinSource::rust_channel(TOOLCHAIN, &read(edit, TOOLCHAIN)),
            PinSource::rust_version(MANIFEST, &read(edit, MANIFEST)),
            PinSource::rust_image_tag(DOCKERFILE, &dockerfile),
            PinSource::rust_version_arg(DOCKERFILE, &dockerfile),
            release_pin(edit, "RUST_RELEASE_RUST_STD_VERSION"),
        ],
        &[],
    );
    problems.extend(one_value(
        "zig",
        &[
            release_pin(edit, "RUST_RELEASE_ZIG_VERSION"),
            source(edit, DOCKERFILE, ZIG_DOCKERFILE),
            source(edit, CI_WORKFLOW, ZIG_ACTION),
            source(edit, RELEASE_WORKFLOW, ZIG_ACTION),
        ],
        &[],
    ));
    problems.extend(one_value(
        "cargo-zigbuild",
        &[
            release_pin(edit, "RUST_RELEASE_CARGO_ZIGBUILD_VERSION"),
            source(edit, DOCKERFILE, ZIGBUILD_DOCKERFILE),
            source(edit, CI_WORKFLOW, ZIGBUILD_ACTION),
            source(edit, RELEASE_WORKFLOW, ZIGBUILD_ACTION),
        ],
        &[],
    ));
    for arch in ["x86_64", "aarch64"] {
        problems.extend(one_value(
            &format!("the zig {arch} digest"),
            &[
                release_pin(edit, &format!("RUST_RELEASE_ZIG_SHA256_{arch}")),
                PinSource::new(
                    DOCKERFILE,
                    &dockerfile,
                    &format!("zig_arch={arch}; zig_sha256=([0-9a-f]{{64}})"),
                ),
            ],
            &[],
        ));
    }
    for (target, arm) in ZIGBUILD_ARMS {
        problems.extend(one_value(
            &format!("the cargo-zigbuild digest for {target}"),
            &[
                release_pin(
                    edit,
                    &format!("RUST_RELEASE_CARGO_ZIGBUILD_SHA256_{target}"),
                ),
                PinSource::new(DOCKERFILE, &dockerfile, &format!("{arm}([0-9a-f]{{64}})")),
            ],
            &[],
        ));
    }
    for (target, arm) in STD_ARMS {
        problems.extend(one_value(
            &format!("the rust-std digest for {target}"),
            &[
                release_pin(edit, &format!("RUST_RELEASE_RUST_STD_SHA256_{target}")),
                PinSource::new(DOCKERFILE, &dockerfile, &format!("{arm}([0-9a-f]{{64}})")),
            ],
            &[],
        ));
    }
    problems.extend(one_value(
        "ShellCheck",
        &[
            source(edit, HOOKS, SHELLCHECK_HOOK),
            source(edit, CI_WORKFLOW, SHELLCHECK_ACTION),
        ],
        &[],
    ));
    problems
}

/// The pins hold: rust-toolchain.toml pins an exact release, which the workspace's `rust-version`,
/// the image's tag, `ARG RUST_VERSION` and the musl standard libraries' release repeat; the release
/// binaries (CI's and the release's) and the image's build stage are built with the same zig and
/// cargo-zigbuild, from the same archives; and the `ShellCheck` CI installs is the release the
/// shellcheck-py hook wraps.
#[test]
fn pins_are_one_value() {
    let problems = pin_problems(&|_, content| content);
    assert!(problems.is_empty(), "{}", report(&problems));
    rust_release(TOOLCHAIN, &meta::repo().read(TOOLCHAIN))
        .unwrap_or_else(|problem| panic!("{problem}"));
}

#[test]
fn every_toolchain_target_has_a_rust_std_digest() {
    let pins = meta::repo().read(RELEASE_PINS);
    let targets = toolchain_targets(&meta::repo().read(TOOLCHAIN));
    assert!(!targets.is_empty(), "{TOOLCHAIN} names no target");
    for target in targets {
        assert!(
            pins.contains(&format!("RUST_RELEASE_RUST_STD_SHA256_{target} :=")),
            "{RELEASE_PINS} records no rust-std digest for {target}"
        );
    }
}

/// One negative control: `from` replaced by `to` in `file` breaks `want` problems.
type Refusal = (&'static str, &'static str, String, String, usize);

/// Each edit of the real files is refused with the problems it breaks.
fn assert_refused(refusals: Vec<Refusal>) {
    for (name, file, from, to, want) in refusals {
        let problems = pin_problems(&|path, content| {
            if path == file {
                edited(&content, &from, &to)
            } else {
                content
            }
        });
        assert_eq!(problems.len(), want, "{name}: {}", report(&problems));
    }
}

/// The negative control of the Rust pins, on the real files: another line or patch release, or a
/// channel that is not a release.
#[test]
fn rust_pin_rules_refuse_each_edit() {
    let release = rust_release(TOOLCHAIN, &meta::repo().read(TOOLCHAIN))
        .unwrap_or_else(|problem| panic!("{problem}"));
    let line = release
        .rsplit_once('.')
        .map_or(release.as_str(), |(line, _)| line);
    let channel = format!("channel = \"{release}\"");

    assert_refused(vec![
        (
            "a toolchain of another line",
            TOOLCHAIN,
            channel.clone(),
            format!("channel = \"{line}0.1\""),
            1,
        ),
        (
            "a toolchain of another patch",
            TOOLCHAIN,
            channel.clone(),
            format!("channel = \"{line}.99\""),
            1,
        ),
        (
            "a channel that is not a release",
            TOOLCHAIN,
            channel,
            "channel = \"stable\"".to_owned(),
            1,
        ),
        (
            "a workspace rust-version of another patch",
            MANIFEST,
            format!("rust-version = \"{release}\""),
            format!("rust-version = \"{line}.99\""),
            1,
        ),
        (
            "an image of another patch",
            DOCKERFILE,
            format!("dhi.io/rust:{release}-"),
            format!("dhi.io/rust:{line}.99-"),
            1,
        ),
        (
            "standard libraries of another patch",
            DOCKERFILE,
            format!("ARG RUST_VERSION={release}"),
            format!("ARG RUST_VERSION={line}.99"),
            1,
        ),
        (
            "the Makefile's rust-std of another patch",
            RELEASE_PINS,
            format!("RUST_RELEASE_RUST_STD_VERSION := {release}"),
            format!("RUST_RELEASE_RUST_STD_VERSION := {line}.99"),
            1,
        ),
    ]);
}

/// The negative control of the tool pins, on the real files: a second version, a flipped digest,
/// or a source that no longer writes the pin.
#[test]
fn tool_pin_rules_refuse_each_edit() {
    let digest = "bd9f35880de21dab0e387e84009c00001ae7836a4ba565cb16823f13f75f325f";
    assert_refused(vec![
        (
            "the Dockerfile's zig",
            DOCKERFILE,
            "ARG ZIG_VERSION=".to_owned(),
            "ARG ZIG_VERSION=9".to_owned(),
            1,
        ),
        (
            "zig in CI",
            CI_WORKFLOW,
            "zig-version:".to_owned(),
            "zig-version-gone:".to_owned(),
            1,
        ),
        (
            "zig in the release",
            RELEASE_WORKFLOW,
            "zig-version: ".to_owned(),
            "zig-version: 9".to_owned(),
            1,
        ),
        (
            "cargo-zigbuild in the Makefile",
            RELEASE_PINS,
            "RUST_RELEASE_CARGO_ZIGBUILD_VERSION := ".to_owned(),
            "RUST_RELEASE_CARGO_ZIGBUILD_VERSION := 9".to_owned(),
            1,
        ),
        (
            "cargo-zigbuild in CI",
            CI_WORKFLOW,
            "cargo-zigbuild-version: ".to_owned(),
            "cargo-zigbuild-version: 9".to_owned(),
            1,
        ),
        (
            "cargo-zigbuild in the release",
            RELEASE_WORKFLOW,
            "cargo-zigbuild-version:".to_owned(),
            "cargo-zigbuild-version-gone:".to_owned(),
            1,
        ),
        (
            "a rust-std digest in the Makefile",
            RELEASE_PINS,
            digest.to_owned(),
            format!("{}0", &digest[..63]),
            1,
        ),
        (
            "a rust-std digest the Dockerfile no longer writes",
            DOCKERFILE,
            "std_sha256=".to_owned(),
            "std_digest=".to_owned(),
            1,
        ),
        (
            "ShellCheck",
            CI_WORKFLOW,
            "version: v0.".to_owned(),
            "version: v9.0.".to_owned(),
            1,
        ),
    ]);
}

/// Every YAML file under .github, with its text; none is a failure, since the scan would check
/// nothing.
fn github_yaml() -> Vec<(String, String)> {
    let repo = meta::repo();
    let files = repo.files_under(".github", &[".yaml", ".yml"]);
    assert!(
        !files.is_empty(),
        "no YAML files under .github; the scan would check nothing"
    );
    repo.read_all(&files)
}

/// The Trivy pins under .github, one problem per file that writes one.
fn trivy_pin_problems(files: &[(String, String)]) -> Vec<Problem> {
    let pin = repo_meta::pattern(TRIVY_PIN);
    files
        .iter()
        .filter(|(_, content)| pin.is_match(content))
        .map(|(file, _)| {
            Problem::at(
                file,
                "pins Trivy: trivy-gate installs its latest release, so every scan runs the newest \
                 checks",
            )
        })
        .collect()
}

/// Trivy is never pinned: every scan is trivy-gate's, at its default `latest`, and no workflow
/// installs a Trivy of its own.
#[test]
fn trivy_is_never_pinned() {
    let problems = trivy_pin_problems(&github_yaml());
    assert!(problems.is_empty(), "{}", report(&problems));
}

/// The negative control: each way of pinning Trivy is found, in the file that writes it.
#[test]
fn trivy_pins_are_recognized() {
    for sample in [
        "        with:\n          trivy-version: 0.76.2\n",
        "env:\n  TRIVY_VERSION: v0.76.2\n",
        "      - uses: aquasecurity/setup-trivy@0000000000000000000000000000000000000000 # v0.3.1\n",
    ] {
        let problems = trivy_pin_problems(&[(SCHEDULED_SCAN.to_owned(), sample.to_owned())]);
        assert_eq!(problems.len(), 1, "{sample:?}: {}", report(&problems));
    }
}
