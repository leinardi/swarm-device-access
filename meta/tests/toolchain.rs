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

//! The repository's tooling pins: the Go line of go.mod in the image's golang tag and every
//! setup-go step; one exact Rust release across rust-toolchain.toml, the workspace's
//! `rust-version` and every CI toolchain step; one zig and one cargo-zigbuild between the
//! setup-zigbuild action and the release fragment, and the fragment's musl standard libraries at
//! that exact Rust release; one droast and one ShellCheck between the hooks and the CI actions;
//! the whole-tree hooks running once; and no meta-test that skips on a missing tool. The Rust side
//! of `test/toolchain/pins_test.go` and of `TestToolGateListIsClosed`, which stay until the
//! cutover; `ci_gates.rs` and `vacuous_flags.rs` port the rest of that package.
//!
//! Not ported: `TestUntaggedTestsAvoidDocker` and its negative control read Go build tags and
//! imports through `go/ast`, which only the Go tests have; `integration_targets.rs` is the Rust
//! side of the same rule. SSE checks its zig, cargo-zigbuild and rust-std pins against its
//! Dockerfile; here the Dockerfile still builds Go, so they are checked against
//! `.mk/rust-release.mk`, the record the Rust image will take. SSE's Trivy pin and its Rust line in
//! the image come with the Rust image and release, which this repository does not have yet.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use meta::repo;
use regex::Regex;
use serde_yaml_ng::Value;

const SETUP_GO: &str = "actions/setup-go";
const RUST_TOOLCHAIN_ACTION: &str = "dtolnay/rust-toolchain";
const DOCKERFILE: &str = "deployments/docker/Dockerfile";
const PRE_COMMIT_CONFIG: &str = ".pre-commit-config.yaml";

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|error| panic!("invalid pattern {pattern}: {error}"))
}

/// rust-toolchain.toml's channel, which must name one exact release.
static RUST_CHANNEL: LazyLock<Regex> = LazyLock::new(|| regex(r#"(?m)^channel = "([^"]*)"$"#));
/// The workspace's rust-version in the root Cargo.toml.
static RUST_VERSION: LazyLock<Regex> = LazyLock::new(|| regex(r#"(?m)^rust-version = "([^"]*)"$"#));
/// A Rust release with its patch: 1.98.1, never 1.98 or stable.
static EXACT_RUST: LazyLock<Regex> = LazyLock::new(|| regex(r"^\d+\.\d+\.\d+$"));
/// go.mod's go line, with its patch version.
static GO_DIRECTIVE: LazyLock<Regex> = LazyLock::new(|| regex(r"(?m)^go (\d+\.\d+\.\d+)$"));
/// The version part of a dhi.io/golang tag: 1.26.9 in 1.26.9-alpine3.23-dev.
static GOLANG_IMAGE: LazyLock<Regex> = LazyLock::new(|| regex(r"dhi\.io/golang:([^-@\s]+)-"));

/// One step, in a workflow or a composite action, that uses an action.
#[derive(Debug)]
struct ActionStep {
    file: String,
    /// The `uses:` value without its `@ref`.
    action: String,
    with: BTreeMap<String, String>,
}

/// Every mapping with a `uses:` key anywhere in the document: job steps, composite action steps
/// and reusable workflow calls alike.
fn parse_action_steps(file: &str, content: &str) -> Vec<ActionStep> {
    fn walk(file: &str, node: &Value, steps: &mut Vec<ActionStep>) {
        match node {
            Value::Mapping(mapping) => {
                if let Some(Value::String(uses)) = mapping.get("uses") {
                    let action = uses
                        .split_once('@')
                        .map_or(uses.as_str(), |(action, _)| action);
                    let with = mapping
                        .get("with")
                        .and_then(Value::as_mapping)
                        .map(|inputs| {
                            inputs
                                .iter()
                                .filter_map(|(key, value)| {
                                    let value = match value {
                                        Value::String(text) => text.clone(),
                                        Value::Number(number) => number.to_string(),
                                        Value::Bool(flag) => flag.to_string(),
                                        Value::Null
                                        | Value::Sequence(_)
                                        | Value::Mapping(_)
                                        | Value::Tagged(_) => return None,
                                    };
                                    Some((key.as_str()?.to_owned(), value))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    steps.push(ActionStep {
                        file: file.to_owned(),
                        action: action.to_owned(),
                        with,
                    });
                }
                for value in mapping.values() {
                    walk(file, value, steps);
                }
            }
            Value::Sequence(sequence) => {
                for value in sequence {
                    walk(file, value, steps);
                }
            }
            Value::Null
            | Value::Bool(_)
            | Value::Number(_)
            | Value::String(_)
            | Value::Tagged(_) => {}
        }
    }

    let document: Value =
        serde_yaml_ng::from_str(content).unwrap_or_else(|error| panic!("parse {file}: {error}"));
    let mut steps = Vec::new();
    walk(file, &document, &mut steps);
    steps
}

/// Every YAML file under .github, the workflows and the composite actions alike.
fn github_yaml() -> Vec<String> {
    let files = repo::files_under(".github", &[".yaml", ".yml"]);
    assert!(
        !files.is_empty(),
        "no YAML files under .github; the checks would see nothing"
    );
    files
}

fn github_steps() -> Vec<ActionStep> {
    github_yaml()
        .iter()
        .flat_map(|file| parse_action_steps(file, &repo::read(file)))
        .collect()
}

/// The Go rules: no step writes a go-version, and every setup-go step reads
/// `go-version-file: go.mod`.
fn go_step_problems(steps: &[ActionStep]) -> Vec<String> {
    let mut problems = Vec::new();
    for step in steps {
        if let Some(version) = step
            .with
            .get("go-version")
            .filter(|version| !version.is_empty())
        {
            problems.push(format!(
                "{}: {} gets go-version {version:?}; the Go line comes from go.mod",
                step.file, step.action
            ));
        }
        if step.action == SETUP_GO
            && step.with.get("go-version-file").map(String::as_str) != Some("go.mod")
        {
            problems.push(format!(
                "{}: setup-go must read go-version-file: go.mod",
                step.file
            ));
        }
    }
    problems
}

/// The Rust rule: every toolchain step installs exactly `want`.
fn rust_step_problems(steps: &[ActionStep], want: &str) -> Vec<String> {
    steps
        .iter()
        .filter(|step| step.action == RUST_TOOLCHAIN_ACTION)
        .filter_map(|step| {
            let installed = step.with.get("toolchain").map_or("", String::as_str);
            (installed != want).then(|| {
                format!(
                    "{}: {} installs toolchain {installed:?}; it must be {want}, the release of \
                     rust-toolchain.toml",
                    step.file, step.action
                )
            })
        })
        .collect()
}

/// Go: `TestGoLineIsOneValue`. The go directive in go.mod is the Go every build uses: each build
/// stage of the Dockerfile names that exact version in its golang tag, so the image builds with
/// the Go CI tests and scans, and every setup-go step reads go.mod instead of writing a version of
/// its own.
#[test]
fn go_line_is_one_value() {
    let go_mod = repo::read("go.mod");
    let want = GO_DIRECTIVE
        .captures(&go_mod)
        .map(|captures| captures[1].to_owned())
        .expect("go.mod has a go directive with a patch version (go X.Y.Z)");

    let dockerfile = repo::read(DOCKERFILE);
    let tags: Vec<&str> = GOLANG_IMAGE
        .captures_iter(&dockerfile)
        .filter_map(|captures| captures.get(1).map(|tag| tag.as_str()))
        .collect();
    let mut problems = Vec::new();
    if tags.is_empty() {
        problems.push(format!("{DOCKERFILE} builds on no dhi.io/golang image"));
    }
    for tag in tags {
        if tag != want {
            problems.push(format!(
                "{DOCKERFILE} builds on golang {tag}, but go.mod says go {want}"
            ));
        }
    }
    problems.extend(go_step_problems(&github_steps()));
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// Go: `TestGoStepRules`, the negative control of the setup-go rules.
#[test]
fn go_step_rules() {
    let steps = parse_action_steps(
        "sample.yaml",
        r#"jobs:
  ok:
    steps:
      - uses: actions/setup-go@0123456789abcdef0123456789abcdef01234567 # v7.0.0
        with:
          go-version-file: go.mod
  bad:
    steps:
      - uses: actions/setup-go@0123456789abcdef0123456789abcdef01234567
        with:
          go-version: "1.26"
      - uses: actions/setup-go@0123456789abcdef0123456789abcdef01234567
      - uses: actions/setup-go@0123456789abcdef0123456789abcdef01234567
        with:
          go-version-file: .go-version
      - uses: some/other-action@0123456789abcdef0123456789abcdef01234567
        with:
          go-version: "1.27"
"#,
    );
    // The first unpinned step writes a version and reads no file: two problems. The two after it
    // read no go.mod, and the other action writes a version.
    let problems = go_step_problems(&steps);
    assert_eq!(problems.len(), 5, "{problems:#?}");
}

/// Go: `TestGoDirectiveReading`. The go.mod and image-tag readings the Go line rests on.
#[test]
fn go_directive_reading() {
    let directive = GO_DIRECTIVE
        .captures("module x\n\ngo 1.26.9\n")
        .map(|captures| captures[1].to_owned());
    assert_eq!(directive.as_deref(), Some("1.26.9"));
    assert!(
        !GO_DIRECTIVE.is_match("module x\n\ngo 1.26\n"),
        "a go directive without a patch version was accepted"
    );

    let tag = GOLANG_IMAGE
        .captures("FROM dhi.io/golang:1.26.9-alpine3.23-dev AS deps")
        .map(|captures| captures[1].to_owned());
    assert_eq!(tag.as_deref(), Some("1.26.9"));
    // The floating tag reads as 1, which no go directive equals.
    let floating = GOLANG_IMAGE
        .captures("FROM dhi.io/golang:1-alpine3.23-dev")
        .map(|captures| captures[1].to_owned());
    assert_eq!(floating.as_deref(), Some("1"));
}

/// Go: `TestRustLineIsOneRelease`. One exact Rust release, patch included, is shared by
/// rust-toolchain.toml's channel, the workspace's rust-version and every CI toolchain step, so the
/// toolchain a developer, a hook and CI build with is one compiler. (SSE pins only the line in
/// `rust-version`; this workspace pins the release there too.)
#[test]
fn rust_line_is_one_release() {
    let toolchain = repo::read("rust-toolchain.toml");
    let want = RUST_CHANNEL
        .captures(&toolchain)
        .map(|captures| captures[1].to_owned())
        .filter(|channel| EXACT_RUST.is_match(channel))
        .expect("rust-toolchain.toml names an exact release (channel = \"X.Y.Z\")");

    let mut problems = Vec::new();
    let manifest = repo::read("Cargo.toml");
    match RUST_VERSION.captures(&manifest) {
        Some(version) if version[1] == want => {}
        Some(version) => problems.push(format!(
            "Cargo.toml's rust-version is {:?}, but rust-toolchain.toml says {want}",
            &version[1]
        )),
        None => problems.push("the root Cargo.toml sets no rust-version".to_owned()),
    }

    let steps = github_steps();
    if !steps
        .iter()
        .any(|step| step.action == RUST_TOOLCHAIN_ACTION)
    {
        problems.push(format!(
            "no {RUST_TOOLCHAIN_ACTION} step under .github; the check would see nothing"
        ));
    }
    problems.extend(rust_step_problems(&steps, &want));
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// Go: `TestRustStepRules`, the negative control of the Rust toolchain step rule: another
/// release, a channel, a release without its patch, and no toolchain at all.
#[test]
fn rust_step_rules() {
    let steps = parse_action_steps(
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
    assert_eq!(steps.len(), 5, "{steps:?}");
    let problems = rust_step_problems(&steps, "1.98.1");
    assert_eq!(problems.len(), 4, "{problems:#?}");
}

/// Go: `TestRustLineReading`. The readings the Rust line rests on: a channel or a release without
/// its patch is not an exact release.
#[test]
fn rust_line_reading() {
    let channel = RUST_CHANNEL
        .captures("[toolchain]\nchannel = \"1.98.1\"\n")
        .map(|captures| captures[1].to_owned());
    assert_eq!(channel.as_deref(), Some("1.98.1"));
    let version = RUST_VERSION
        .captures("[workspace.package]\nrust-version = \"1.98\"\n")
        .map(|captures| captures[1].to_owned());
    assert_eq!(version.as_deref(), Some("1.98"));
    for inexact in ["stable", "1.98", "nightly-2026-10-01", "1.98.1-x86_64"] {
        assert!(
            !EXACT_RUST.is_match(inexact),
            "{inexact:?} was accepted as an exact release"
        );
    }
}

/// The version `hook_pattern` captures in .pre-commit-config.yaml, against the one
/// `action_pattern` captures in `action`, as a problem when they differ or either is missing.
fn hook_version_problem(
    tool: &str,
    hooks: &str,
    hook_pattern: &str,
    action: &str,
    action_pattern: &str,
) -> Option<String> {
    let hook = regex(hook_pattern)
        .captures(hooks)
        .map(|captures| captures[1].to_owned());
    let installed = regex(action_pattern)
        .captures(action)
        .map(|captures| captures[1].to_owned());
    match (hook, installed) {
        (Some(hook), Some(installed)) if hook == installed => None,
        (Some(hook), Some(installed)) => Some(format!(
            "the {tool} hook is at {hook}, but its CI action installs {installed}"
        )),
        (hook, installed) => Some(format!(
            "{tool} version not found: hook {hook:?}, action {installed:?}"
        )),
    }
}

const SHELLCHECK_HOOK: &str =
    r"repo: https://github\.com/shellcheck-py/shellcheck-py\s+rev:\s*(v\d+\.\d+\.\d+)";
const SHELLCHECK_ACTION: &str = ".github/actions/setup-shellcheck/action.yaml";
const SHELLCHECK_PIN: &str = r#"SHELLCHECK_VERSION:\s*["']?(v[\d.]+)"#;
const DROAST_HOOK: &str =
    r#"repo: https://github\.com/immanuwell/dockerfile-roast\s+rev:\s*["']?([\d.]+)"#;
const DROAST_ACTION: &str = ".github/actions/setup-droast/action.yaml";
const DROAST_PIN: &str = r#"DROAST_VERSION:\s*["']?([\d.]+)"#;

const SETUP_ZIGBUILD: &str = ".github/actions/setup-zigbuild/action.yaml";
const RUST_RELEASE_FRAGMENT: &str = ".mk/rust-release.mk";

/// One place a pin is written: a file's name, its content, and the pattern whose first group is
/// the pin there.
struct PinSource<'content> {
    file: &'content str,
    content: &'content str,
    pattern: &'content str,
}

/// SSE's `expect_one_value`, as a function of the contents: a problem for every source that does
/// not write the pin, and one when the values written are not all the same.
fn one_value_problems(what: &str, sources: &[PinSource<'_>]) -> Vec<String> {
    let mut found: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    let mut problems = Vec::new();
    for source in sources {
        let pattern = regex(source.pattern);
        let mut matched = false;
        for captures in pattern.captures_iter(source.content) {
            matched = true;
            found
                .entry(captures[1].to_owned())
                .or_default()
                .push(source.file);
        }
        if !matched {
            problems.push(format!(
                "{what}: {} does not write it ({pattern})",
                source.file
            ));
        }
    }
    if found.len() > 1 {
        let values: Vec<String> = found
            .iter()
            .map(|(value, files)| {
                let mut files = files.clone();
                files.dedup();
                format!("{value} in {}", files.join(", "))
            })
            .collect();
        problems.push(format!("{what} is not one value: {}", values.join("; ")));
    }
    problems
}

/// The zig and cargo-zigbuild pins of the action and the release fragment: each version, and the
/// digest of the `x86_64` archive both name, as (what, action pattern, fragment pattern).
const ZIGBUILD_PINS: [(&str, &str, &str); 4] = [
    (
        "zig",
        r#"(?m)^\s*ZIG_VERSION:\s*"?(\d+\.\d+\.\d+)"?\s*$"#,
        r"(?m)^RUST_RELEASE_ZIG_VERSION := (\S+)$",
    ),
    (
        "zig x86_64 sha256",
        r"(?m)^\s*ZIG_SHA256:\s*([0-9a-f]{64})\s*$",
        r"(?m)^RUST_RELEASE_ZIG_SHA256_x86_64 := ([0-9a-f]{64})$",
    ),
    (
        "cargo-zigbuild",
        r#"(?m)^\s*CARGO_ZIGBUILD_VERSION:\s*"?(\d+\.\d+\.\d+)"?\s*$"#,
        r"(?m)^RUST_RELEASE_CARGO_ZIGBUILD_VERSION := (\S+)$",
    ),
    (
        "cargo-zigbuild x86_64 sha256",
        r"(?m)^\s*CARGO_ZIGBUILD_SHA256:\s*([0-9a-f]{64})\s*$",
        r"(?m)^RUST_RELEASE_CARGO_ZIGBUILD_SHA256_x86_64-unknown-linux-musl := ([0-9a-f]{64})$",
    ),
];

/// Every zig and cargo-zigbuild pin that is not one value between the action and the fragment.
fn zigbuild_problems(action: &str, fragment: &str) -> Vec<String> {
    ZIGBUILD_PINS
        .iter()
        .flat_map(|(what, action_pattern, fragment_pattern)| {
            one_value_problems(
                what,
                &[
                    PinSource {
                        file: SETUP_ZIGBUILD,
                        content: action,
                        pattern: action_pattern,
                    },
                    PinSource {
                        file: RUST_RELEASE_FRAGMENT,
                        content: fragment,
                        pattern: fragment_pattern,
                    },
                ],
            )
        })
        .collect()
}

/// SSE's `zig_is_one_version` and `cargo_zigbuild_is_one_version`, with the release fragment in
/// place of the Dockerfile, which still builds Go: CI's static builds (setup-zigbuild) and the
/// record the Rust image will take use one zig and one cargo-zigbuild, from the same `x86_64`
/// archives, so a bump that moves one copy fails here rather than in the first build that runs it.
#[test]
fn zig_and_cargo_zigbuild_are_one_version() {
    let problems = zigbuild_problems(
        &repo::read(SETUP_ZIGBUILD),
        &repo::read(RUST_RELEASE_FRAGMENT),
    );
    assert!(problems.is_empty(), "{}", problems.join("\n"));
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
    assert_eq!(zigbuild_problems(&action, &fragment), Vec::<String>::new());

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

/// The release fragment's musl standard library pins.
static RUST_STD_VERSION: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?m)^RUST_RELEASE_RUST_STD_VERSION := (\S+)$"));
/// A rust-std digest line, of any value, so a malformed digest is seen rather than skipped.
static RUST_STD_SHA256: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?m)^RUST_RELEASE_RUST_STD_SHA256_(\S+) := (\S*)$"));
/// The release platforms, as `<docker arch>:<Rust target>` pairs.
static RELEASE_PLATFORMS: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?m)^RUST_RELEASE_PLATFORMS\s*:= (.*)$"));
/// A sha256, as the archives' `.sha256` files publish it.
static SHA256: LazyLock<Regex> = LazyLock::new(|| regex(r"^[0-9a-f]{64}$"));

/// The targets rust-toolchain.toml installs.
fn toolchain_targets(toolchain: &str) -> Vec<String> {
    let table: toml::Table = toml::from_str(toolchain)
        .unwrap_or_else(|error| panic!("parse rust-toolchain.toml: {error}"));
    table
        .get("toolchain")
        .and_then(|section| section.get("targets"))
        .and_then(toml::Value::as_array)
        .map(|targets| {
            targets
                .iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// What is wrong with the fragment's release targets and musl standard libraries, against the
/// exact release and the targets of rust-toolchain.toml: the rust-std release is the toolchain's,
/// each target has one well-formed digest and no other target has one, and the release platforms
/// build exactly those targets.
fn rust_std_problems(fragment: &str, release: &str, targets: &[String]) -> Vec<String> {
    let mut problems = Vec::new();
    if targets.is_empty() {
        problems.push("rust-toolchain.toml installs no target; the check would see nothing".into());
    }
    match RUST_STD_VERSION.captures(fragment) {
        Some(version) if version[1] == *release => {}
        Some(version) => problems.push(format!(
            "{RUST_RELEASE_FRAGMENT} pins rust-std {}, but rust-toolchain.toml says {release}",
            &version[1]
        )),
        None => problems.push(format!(
            "{RUST_RELEASE_FRAGMENT} writes no RUST_RELEASE_RUST_STD_VERSION"
        )),
    }

    let mut digests: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for captures in RUST_STD_SHA256.captures_iter(fragment) {
        if let (Some(target), Some(digest)) = (captures.get(1), captures.get(2)) {
            digests
                .entry(target.as_str())
                .or_default()
                .push(digest.as_str());
        }
    }
    for target in targets {
        match digests.get(target.as_str()).map(Vec::as_slice) {
            Some([digest]) if SHA256.is_match(digest) => {}
            Some([digest]) => problems.push(format!(
                "the rust-std digest of {target} is {digest:?}, not a sha256"
            )),
            Some(written) => problems.push(format!(
                "the rust-std digest of {target} is written {} times, want once",
                written.len()
            )),
            None => problems.push(format!("{target} has no rust-std digest")),
        }
    }
    for target in digests.keys() {
        if !targets.iter().any(|wanted| wanted == target) {
            problems.push(format!(
                "a rust-std digest names {target}, which rust-toolchain.toml does not install"
            ));
        }
    }

    let platforms: Vec<&str> = RELEASE_PLATFORMS
        .captures(fragment)
        .and_then(|captures| captures.get(1))
        .map(|platforms| {
            platforms
                .as_str()
                .split_whitespace()
                .filter_map(|platform| platform.split_once(':').map(|(_, target)| target))
                .collect()
        })
        .unwrap_or_default();
    if platforms != targets.iter().map(String::as_str).collect::<Vec<_>>() {
        problems.push(format!(
            "the release platforms build {platforms:?}, but rust-toolchain.toml installs \
             {targets:?}"
        ));
    }
    problems
}

/// The musl standard libraries the Rust image's build stage will unpack are the exact release of
/// rust-toolchain.toml, one sha256 for each of its targets, and the release binaries are built for
/// exactly those targets. (SSE's `image_toolchain_patch_is_the_standard_library_version`, against
/// the toolchain file rather than the image tag, which still names Go.)
#[test]
fn rust_std_is_the_rust_release() {
    let toolchain = repo::read("rust-toolchain.toml");
    let release = RUST_CHANNEL
        .captures(&toolchain)
        .map(|captures| captures[1].to_owned())
        .expect("rust-toolchain.toml names a channel");
    let problems = rust_std_problems(
        &repo::read(RUST_RELEASE_FRAGMENT),
        &release,
        &toolchain_targets(&toolchain),
    );
    assert!(problems.is_empty(), "{}", problems.join("\n"));
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
    assert_eq!(
        rust_std_problems(&fragment, "1.98.1", &targets),
        Vec::<String>::new()
    );
    assert!(
        !rust_std_problems(&fragment, "1.98.1", &[]).is_empty(),
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
        let problems = rust_std_problems(&sample, release, &targets);
        assert_eq!(problems.len(), 1, "{name}: {problems:#?}");
    }
}

/// Go: `TestShellCheckIsOneVersion`. droast's ShellCheck bridge runs the `shellcheck` on `PATH`,
/// which CI installs with setup-shellcheck; it must be the release the shellcheck-py hook wraps
/// (its rev adds a fourth component for the wrapper), which Dependabot moves.
#[test]
fn shellcheck_is_one_version() {
    let problem = hook_version_problem(
        "ShellCheck",
        &repo::read(PRE_COMMIT_CONFIG),
        SHELLCHECK_HOOK,
        &repo::read(SHELLCHECK_ACTION),
        SHELLCHECK_PIN,
    );
    assert!(problem.is_none(), "{problem:?}");
}

/// Go: `TestDroastIsOneVersion`. The droast hook builds the release at its rev, which Dependabot
/// moves, and the Go Dockerfile meta-tests run the binary CI installs with setup-droast; both must
/// be the same release, so the label test checks the droast that lints.
#[test]
fn droast_is_one_version() {
    let problem = hook_version_problem(
        "droast",
        &repo::read(PRE_COMMIT_CONFIG),
        DROAST_HOOK,
        &repo::read(DROAST_ACTION),
        DROAST_PIN,
    );
    assert!(problem.is_none(), "{problem:?}");
}

/// The negative control of the two version checks: a moved hook, a moved action and a missing
/// pin are each a problem.
#[test]
fn hook_version_rules() {
    let hooks = "  - repo: https://github.com/shellcheck-py/shellcheck-py\n    rev: v0.11.0.1\n\
                 \x20 - repo: https://github.com/immanuwell/dockerfile-roast\n    rev: 1.7.0\n";
    let shellcheck = "        SHELLCHECK_VERSION: v0.11.0\n";
    let droast = "        DROAST_VERSION: 1.7.0\n";
    assert_eq!(
        hook_version_problem(
            "ShellCheck",
            hooks,
            SHELLCHECK_HOOK,
            shellcheck,
            SHELLCHECK_PIN
        ),
        None
    );
    assert_eq!(
        hook_version_problem("droast", hooks, DROAST_HOOK, droast, DROAST_PIN),
        None
    );
    for (name, problem) in [
        (
            "a moved ShellCheck action",
            hook_version_problem(
                "ShellCheck",
                hooks,
                SHELLCHECK_HOOK,
                "        SHELLCHECK_VERSION: v0.10.0\n",
                SHELLCHECK_PIN,
            ),
        ),
        (
            "a moved droast hook",
            hook_version_problem(
                "droast",
                &hooks.replace("rev: 1.7.0", "rev: 1.8.0"),
                DROAST_HOOK,
                droast,
                DROAST_PIN,
            ),
        ),
        (
            "a missing droast pin",
            hook_version_problem("droast", hooks, DROAST_HOOK, "", DROAST_PIN),
        ),
    ] {
        assert!(problem.is_some(), "{name} was accepted");
    }
}

/// The hooks of .pre-commit-config.yaml by id, as written; an id with aliases (the actionlint,
/// shellcheck and yamllint variants) appears more than once.
fn pre_commit_hooks(content: &str) -> Vec<(String, Value)> {
    let config: Value = serde_yaml_ng::from_str(content)
        .unwrap_or_else(|error| panic!("parse {PRE_COMMIT_CONFIG}: {error}"));
    let repos = config
        .get("repos")
        .and_then(Value::as_sequence)
        .unwrap_or_else(|| panic!("{PRE_COMMIT_CONFIG} has no repos list"));
    repos
        .iter()
        .filter_map(|entry| entry.get("hooks").and_then(Value::as_sequence))
        .flatten()
        .map(|hook| {
            let id = hook.get("id").and_then(Value::as_str).unwrap_or_default();
            (id.to_owned(), hook.clone())
        })
        .collect()
}

/// The hooks that read the whole tree rather than the files pre-commit passes, so each one runs
/// on every commit and must not run twice per commit.
const ALWAYS_RUN_HOOKS: [&str; 3] = ["check-license-headers", "check-suppressions", "docs-links"];

/// What is wrong with the whole-tree hooks of a pre-commit configuration.
fn always_run_problems(content: &str) -> Vec<String> {
    let hooks = pre_commit_hooks(content);
    let mut problems = Vec::new();
    for id in ALWAYS_RUN_HOOKS {
        let found: Vec<&Value> = hooks
            .iter()
            .filter(|(hook_id, _)| hook_id == id)
            .map(|(_, hook)| hook)
            .collect();
        let [hook] = found.as_slice() else {
            problems.push(format!(
                "hook {id} appears {} times, want once",
                found.len()
            ));
            continue;
        };
        if hook.get("always_run").and_then(Value::as_bool) != Some(true) {
            problems.push(format!("hook {id} does not set always_run: true"));
        }
        if hook.get("pass_filenames").and_then(Value::as_bool) != Some(false) {
            problems.push(format!("hook {id} does not set pass_filenames: false"));
        }
        let stages: Option<Vec<&str>> = hook
            .get("stages")
            .and_then(Value::as_sequence)
            .map(|stages| stages.iter().filter_map(Value::as_str).collect());
        if stages.as_deref() != Some(["pre-commit"].as_slice()) {
            problems.push(format!(
                "hook {id} runs at stages {stages:?}, want exactly [pre-commit]"
            ));
        }
    }
    problems
}

/// A whole-tree hook takes no file list and always runs, because a commit that only deletes a
/// file passes no file to it; and it is pinned to the pre-commit stage, because `always_run`
/// makes a hook eligible at every installed stage, commit-msg included. (SSE's
/// `always_run_hooks_run_once`; the Go tests have no counterpart, the hooks' comments state it.)
#[test]
fn always_run_hooks_run_once() {
    let problems = always_run_problems(&repo::read(PRE_COMMIT_CONFIG));
    assert!(problems.is_empty(), "{}", problems.join("\n"));
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
        always_run_problems(&config(&hook("docs-links", good))),
        Vec::<String>::new()
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
            !always_run_problems(&config(&docs_links)).is_empty(),
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
    let own_lookup = regex(
        r"\bsplit_paths\(|\bvar_os\(\s*\x22PATH\x22|Command::new\(\s*\x22which\x22|\bSDA_REQUIRE[_]TOOLS\b",
    );
    let sources = repo::files_under("meta", &[".rs"]);
    assert!(
        sources.iter().any(|path| path == "meta/tests/toolchain.rs"),
        "the walk did not reach meta/tests: {sources:?}"
    );
    for path in &sources {
        assert!(
            !own_lookup.is_match(&repo::read(path)),
            "{path} looks a tool up on PATH or reads a skip switch; a meta-test needing a tool \
             must fail without it"
        );
    }

    // The negative control. The samples are split so this file does not match its own check.
    for sample in [
        concat!("let found = env::split", "_paths(&paths);"),
        concat!("std::env::var", "_os(\x22PATH\x22)"),
        concat!("Command::new(\x22wh", "ich\x22)"),
        concat!("env::var(\x22SDA_REQUIRE", "_TOOLS\x22)"),
    ] {
        assert!(own_lookup.is_match(sample), "{sample:?} was not recognised");
    }
}
