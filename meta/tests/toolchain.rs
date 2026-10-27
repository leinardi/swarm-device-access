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
//! `rust-version` and every CI toolchain step; one droast and one ShellCheck between the hooks and
//! the CI actions; the whole-tree hooks running once; and no meta-test that skips on a missing
//! tool. The Rust side of `test/toolchain/pins_test.go` and of `TestToolGateListIsClosed`, which
//! stay until the cutover; `ci_gates.rs` and `vacuous_flags.rs` port the rest of that package.
//!
//! Not ported: `TestUntaggedTestsAvoidDocker` and its negative control read Go build tags and
//! imports through `go/ast`, which only the Go tests have; `integration_targets.rs` is the Rust
//! side of the same rule. SSE's zig, cargo-zigbuild and Trivy pins and its Rust line in the image
//! come with the Rust image and release, which this repository does not have yet.

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
