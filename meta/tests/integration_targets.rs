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
//! turns the feature on. The Rust side of `TestUntaggedTestsAvoidDocker`, whose Go build-tag
//! reading stays Go-only until the cutover.
//!
//! Where SSE scans the ungated modules lib.rs declares, this port scans every testenv source
//! outside a gated module, lib.rs included, and checks every test target the manifest declares;
//! as in SSE, the suite itself must be a test target named `integration` that requires the
//! feature, so none of the checks passes on nothing.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use meta::repo;
use regex::Regex;
use toml::Table;

const HARNESS_LIB: &str = "testenv/src/lib.rs";
const HARNESS_MANIFEST: &str = "testenv/Cargo.toml";
const FEATURE: &str = "integration";

/// The files that may run an integration test command: where the Rust integration target and
/// workflow will live, beside the Go ones.
const INTEGRATION_FILES: [&str; 2] = [
    ".mk/integration-test.mk",
    ".github/workflows/integration.yaml",
];

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|error| panic!("invalid pattern {pattern}: {error}"))
}

/// What reaches a Docker daemon from a source file: the SDK imported or named in code (not in a
/// comment or a string, as the boundary tests' own fixtures do), or the docker CLI as a child.
static DRIVES_DOCKER: LazyLock<Regex> = LazyLock::new(|| {
    regex(r"(?m)^\s*(?:pub )?use bollard\b|^[^\x22/\n]*\bbollard::|Command::new\(\s*\x22docker\x22")
});
/// A module declaration in lib.rs, with the attribute lines right above it.
static MODULE: LazyLock<Regex> = LazyLock::new(|| {
    regex(r"(?m)^((?:#\[[^\n]*\]\n)*)(?:pub(?:\([a-z]+\))? )?mod ([a-z_][a-z0-9_]*);")
});
/// A cargo test command: cargo itself or make's `$(CARGO)`.
static CARGO_TEST: LazyLock<Regex> = LazyLock::new(|| regex(r"(?i)\bcargo\)?\s+(\+\S+\s+)?test\b"));
/// A flag that turns the integration feature on: all features, or a feature list naming it.
static ENABLES_FEATURE: LazyLock<Regex> =
    LazyLock::new(|| regex(r"--all-features\b|(?:--features[ =]|-F\s*)\S*\bintegration\b"));

/// The modules lib.rs declares behind the `integration` feature gate.
fn gated_modules(lib: &str) -> BTreeSet<String> {
    let gate = format!("#[cfg(feature = \x22{FEATURE}\x22)]");
    MODULE
        .captures_iter(lib)
        .filter(|captures| captures[1].contains(&gate))
        .map(|captures| captures[2].to_owned())
        .collect()
}

/// The testenv sources built without the feature: every file under testenv/src whose top-level
/// module is not gated, lib.rs itself included.
fn ungated_sources(lib: &str, paths: &[String]) -> Vec<String> {
    let gated = gated_modules(lib);
    paths
        .iter()
        .filter(|path| {
            let module = path
                .trim_start_matches("testenv/src/")
                .split(['/', '.'])
                .next()
                .unwrap_or_default();
            !gated.contains(module)
        })
        .cloned()
        .collect()
}

/// The files of `files` (path and text) that drive Docker.
fn docker_drivers(files: &[(String, String)]) -> Vec<&str> {
    files
        .iter()
        .filter(|(_, text)| DRIVES_DOCKER.is_match(text))
        .map(|(path, _)| path.as_str())
        .collect()
}

fn read_all(paths: &[String]) -> Vec<(String, String)> {
    paths
        .iter()
        .map(|path| (path.clone(), repo::read(path)))
        .collect()
}

/// What is wrong with the harness manifest: the feature must exist and be off by default, the
/// suite must be a test target named `integration`, and every `[[test]]` target must require
/// exactly the feature.
fn manifest_problems(manifest: &str) -> Vec<String> {
    let manifest: Table = match manifest.parse() {
        Ok(manifest) => manifest,
        Err(error) => return vec![format!("parse {HARNESS_MANIFEST}: {error}")],
    };
    let mut problems = Vec::new();
    let declared = manifest
        .get("features")
        .and_then(toml::Value::as_table)
        .is_some_and(|features| features.contains_key(FEATURE));
    if !declared {
        problems.push(format!("{HARNESS_MANIFEST} declares no {FEATURE} feature"));
    }
    let default_on = manifest
        .get("features")
        .and_then(|features| features.get("default"))
        .and_then(toml::Value::as_array)
        .is_some_and(|defaults| {
            defaults
                .iter()
                .any(|feature| feature.as_str() == Some(FEATURE))
        });
    if default_on {
        problems.push(format!("{HARNESS_MANIFEST} turns {FEATURE} on by default"));
    }
    let targets = manifest
        .get("test")
        .and_then(toml::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let suite = targets
        .iter()
        .any(|target| target.get("name").and_then(toml::Value::as_str) == Some(FEATURE));
    if !suite {
        problems.push(format!(
            "{HARNESS_MANIFEST} declares no test target named {FEATURE}"
        ));
    }
    for target in &targets {
        let name = target
            .get("name")
            .and_then(toml::Value::as_str)
            .unwrap_or("<unnamed>");
        let required: Vec<&str> = target
            .get("required-features")
            .and_then(toml::Value::as_array)
            .map(|features| features.iter().filter_map(toml::Value::as_str).collect())
            .unwrap_or_default();
        if required != [FEATURE] {
            problems.push(format!(
                "the test target {name} of {HARNESS_MANIFEST} requires {required:?}, want \
                 exactly [{FEATURE:?}]"
            ));
        }
    }
    problems
}

/// The plain test commands of `content` (a make fragment, the pre-commit configuration or a
/// workflow) that turn the integration feature on, as `<line>: <text>`. Only the integration
/// files may.
fn plain_test_problems(file: &str, content: &str) -> Vec<String> {
    if INTEGRATION_FILES.contains(&file) {
        return Vec::new();
    }
    content
        .lines()
        .enumerate()
        .filter(|(_, line)| CARGO_TEST.is_match(line) && ENABLES_FEATURE.is_match(line))
        .map(|(index, line)| format!("{file}:{}: {}", index + 1, line.trim()))
        .collect()
}

/// Go: `TestIntegrationTreesAreTagged`. Every harness source built without the `integration`
/// feature stays away from Docker, and every harness test target requires the feature.
#[test]
fn integration_tree_is_gated() {
    let paths = repo::files_under("testenv/src", &[".rs"]);
    let ungated = ungated_sources(&repo::read(HARNESS_LIB), &paths);
    assert!(
        ungated.iter().any(|path| path == HARNESS_LIB),
        "{HARNESS_LIB} was not scanned; the check would pass on nothing"
    );
    let sources = read_all(&ungated);
    let drivers = docker_drivers(&sources);
    assert!(
        drivers.is_empty(),
        "harness sources built without the {FEATURE} feature reach Docker: {drivers:?}"
    );
    let problems = manifest_problems(&repo::read(HARNESS_MANIFEST));
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// Go: `TestUntaggedTestsAvoidDocker`. No source of the daemon, its tests or the meta crate
/// reaches Docker: the daemon's own client talks to a fake Engine in its tests, and only the
/// harness behind the feature drives a daemon.
#[test]
fn plain_tests_avoid_docker() {
    let mut paths = Vec::new();
    for tree in ["src", "tests", "meta"] {
        paths.extend(repo::files_under(tree, &[".rs"]));
    }
    assert!(
        paths.len() > 10,
        "only {} sources found; the check would pass on nothing",
        paths.len()
    );
    let sources = read_all(&paths);
    let drivers = docker_drivers(&sources);
    assert!(
        drivers.is_empty(),
        "sources outside the gated harness reach Docker: {drivers:?}"
    );
}

/// The plain test targets never turn the feature on: no `cargo test` in the Makefile, the make
/// fragments, the pre-commit configuration or a workflow enables it, outside the integration
/// fragment and workflow.
#[test]
fn plain_test_targets_leave_the_feature_off() {
    let mut files = vec!["Makefile".to_owned(), ".pre-commit-config.yaml".to_owned()];
    files.extend(repo::files_under(".mk", &[".mk"]));
    files.extend(repo::files_under(".github", &[".yaml", ".yml"]));
    let mut commands = 0;
    let mut problems = Vec::new();
    for file in &files {
        let content = repo::read(file);
        commands += content
            .lines()
            .filter(|line| CARGO_TEST.is_match(line))
            .count();
        problems.extend(plain_test_problems(file, &content));
    }
    assert!(
        commands >= 3,
        "only {commands} cargo test commands found (make rust-test, the cargo-test hook, the rust \
         job); the check would pass on nothing"
    );
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// Go: `TestDockerCheckRecognizesItsTargets`. The negative controls: a gated declaration is
/// recognised and an ungated one is scanned, both ways of reaching Docker are found, a manifest
/// whose test target or default features let the suite into a plain run is refused, and a plain
/// test command that turns the feature on is reported outside the integration files only.
#[test]
fn separation_rules_are_recognized() {
    let lib = "#[cfg(feature = \x22integration\x22)]\nmod dind;\n\
               #[cfg(feature = \x22integration\x22)]\npub mod context;\n\
               mod error;\npub mod redact;\n#[cfg(test)]\nmod tests;\n";
    assert_eq!(
        gated_modules(lib),
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
        ungated_sources(lib, &paths),
        [
            "testenv/src/lib.rs",
            "testenv/src/error.rs",
            "testenv/src/redact/tests.rs"
        ]
    );

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
    assert_eq!(docker_drivers(&files), ["a.rs", "b.rs", "e.rs"]);

    let good = "[features]\nintegration = []\n\n[[test]]\nname = \x22integration\x22\n\
                required-features = [\x22integration\x22]\n";
    assert_eq!(manifest_problems(good), Vec::<String>::new());
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
            !manifest_problems(&manifest).is_empty(),
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
            plain_test_problems(".mk/rust.mk", line).len(),
            1,
            "{line:?} was accepted in a plain target"
        );
        assert!(
            plain_test_problems(".mk/integration-test.mk", line).is_empty(),
            "{line:?} was refused in the integration fragment"
        );
    }
    for line in [
        "\t$(CARGO) test --workspace --locked",
        "\t$(CARGO) clippy --workspace --all-targets --all-features --locked -- -D warnings",
        "        entry: cargo test --locked -p meta --test docs",
    ] {
        assert!(
            plain_test_problems(".mk/rust.mk", line).is_empty(),
            "{line:?} was refused"
        );
    }
}
