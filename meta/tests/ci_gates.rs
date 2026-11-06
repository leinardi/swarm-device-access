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

//! Every CI gate is required: each job of ci.yaml that is a gate rather than an annotation is
//! needed by `ci-result`, the one required check, and `pre-commit-all` skips only the hooks a
//! needed job runs instead, and the rustdoc gate fails both. Ported from this project's Go
//! meta-test `test/toolchain/ci_test.go`; the checks are `repo_meta::ci`'s, extracted from this
//! file, with this workflow's jobs.

use repo_meta::ci::{CiGates, MAKE_COMMON_RUSTDOC};
use repo_meta::yaml::{self, Value};
use repo_meta::{report, strings};

const CI_WORKFLOW: &str = ".github/workflows/ci.yaml";
const PRE_COMMIT_CONFIG: &str = ".pre-commit-config.yaml";

/// `ci-result`, the one required check, needs every gate; the jobs that only post review
/// comments on a pull request are not gates, because `pre-commit-all` runs the same hooks on
/// every file. For each hook `pre-commit-all` may skip, the ci.yaml job that runs it instead: a
/// skip is only safe when that job exists and `ci-result` needs it.
const GATES: CiGates<'static> = CiGates {
    result_job: "ci-result",
    pre_commit_job: "pre-commit-all",
    annotate_only: &[
        "actionlint",
        "pre-commit-hooks",
        "markdownlint",
        "shellcheck",
        "yamllint",
    ],
    skipped_hook_runners: &[("cargo-test", "rust"), ("cargo-deny", "rust")],
};

fn parse(file: &str, content: &str) -> Value {
    yaml::parse(file, content).unwrap_or_else(|problem| panic!("{problem}"))
}

fn read_yaml(file: &str) -> Value {
    parse(file, &meta::repo().read(file))
}

fn hooks(file: &str, document: &Value) -> Vec<(String, Value)> {
    yaml::pre_commit_hooks(file, document).unwrap_or_else(|problem| panic!("{problem}"))
}

/// Go: `TestCIGatesAreRequired`. Every job of ci.yaml that is a gate rather than an annotation
/// is needed by `ci-result`, so a new gate (the rust job included) cannot fail without failing
/// the pull request.
#[test]
fn ci_gates_are_required() {
    let problems = GATES.result_problems(&yaml::jobs(&read_yaml(CI_WORKFLOW)));
    assert!(problems.is_empty(), "{}", report(&problems));
}

/// Go: `TestPreCommitAllSkipsOnlyHooksRunElsewhere`. `pre-commit-all` runs every hook on every
/// file except the ones in its `SKIP` list, and each of those must be a hook of the pre-commit
/// configuration run by a job `ci-result` needs; a job that skips nothing is a problem, on which
/// the check would pass having read nothing.
#[test]
fn pre_commit_all_skips_only_hooks_run_elsewhere() {
    let jobs = yaml::jobs(&read_yaml(CI_WORKFLOW));
    let hooks = hooks(PRE_COMMIT_CONFIG, &read_yaml(PRE_COMMIT_CONFIG));
    let problems = GATES.skip_problems(&jobs, &hooks);
    assert!(problems.is_empty(), "{}", report(&problems));
}

/// Go: `TestCIStructureRules`, the negative control of the two checks above.
#[test]
fn ci_structure_rules() {
    let jobs = yaml::jobs(&parse(
        "ci.yaml",
        "jobs:
  pre-commit-all:
    steps:
      - env:
          SKIP: cargo-test,cargo-clippy,unknown-hook
  rust:
    steps: []
  test:
    steps: []
  markdownlint:
    steps: []
  ci-result:
    needs: [pre-commit-all, test]
",
    ));
    let hooks = hooks(
        "hooks.yaml",
        &parse(
            "hooks.yaml",
            "repos:\n  - repo: local\n    hooks:\n      - id: cargo-test\n      - id: cargo-clippy\n",
        ),
    );

    // rust is a gate ci-result does not need; markdownlint only annotates.
    assert_eq!(
        strings(&GATES.result_problems(&jobs)),
        ["ci-result does not need the rust job"]
    );

    // cargo-test is run by rust, which ci-result does not need; cargo-clippy is run by no
    // dedicated job; unknown-hook is not a hook at all.
    assert_eq!(
        strings(&GATES.skip_problems(&jobs, &hooks)),
        [
            "pre-commit-all skips cargo-test, run by the rust job, which ci-result does not need",
            "pre-commit-all skips cargo-clippy, which no dedicated job is recorded to run",
            "pre-commit-all skips unknown-hook, which is no hook",
        ]
    );

    let without_pre_commit = yaml::jobs(&parse("ci.yaml", "jobs:\n  ci-result:\n    needs: []\n"));
    assert_eq!(
        strings(&GATES.skip_problems(&without_pre_commit, &hooks)),
        ["the CI workflow has no pre-commit-all job"]
    );
    let skips_nothing = yaml::jobs(&parse(
        "ci.yaml",
        "jobs:\n  pre-commit-all:\n    steps: []\n  ci-result:\n    needs: [pre-commit-all]\n",
    ));
    assert_eq!(
        strings(&GATES.skip_problems(&skips_nothing, &hooks)),
        ["pre-commit-all skips nothing; the check would pass on nothing"]
    );
    let without_result = yaml::jobs(&parse("ci.yaml", "jobs:\n  test:\n    steps: []\n"));
    assert_eq!(
        strings(&GATES.result_problems(&without_result)),
        ["the CI workflow has no ci-result job"]
    );
}

/// Rustdoc warnings must fail both the local verify target and the required Rust CI job, over
/// every feature (make-common's `rust.mk`, `repo_meta::ci::MAKE_COMMON_RUSTDOC`).
#[test]
fn rust_documentation_is_a_required_gate() {
    let repo = meta::repo();
    let problems = MAKE_COMMON_RUSTDOC.problems(
        &repo.read(".mk/rust.mk"),
        &repo.read("Makefile"),
        &read_yaml(CI_WORKFLOW),
    );
    assert!(problems.is_empty(), "{}", report(&problems));
}

/// Negative controls: a gate absent from verify or CI, one allowing warnings, one narrowed to
/// fewer features, and a CI step allowed to fail must each fail.
#[test]
fn weakened_rustdoc_gates_are_refused() {
    let repo = meta::repo();
    let fragment = repo.read(".mk/rust.mk");
    let makefile = repo.read("Makefile");
    let workflow_text = repo.read(CI_WORKFLOW);
    let workflow = parse(CI_WORKFLOW, &workflow_text);
    for (name, broken) in [
        (
            "not in verify-rust",
            fragment.replace("rust-test rust-doc", "rust-test"),
        ),
        ("warnings allowed", fragment.replace(" -D warnings", "")),
        (
            "fewer features",
            fragment.replace("RUST_FEATURES ?= --all-features", "RUST_FEATURES ?="),
        ),
    ] {
        assert_ne!(
            broken, fragment,
            "{name}: the sample is the fragment itself"
        );
        assert!(
            !MAKE_COMMON_RUSTDOC
                .problems(&broken, &makefile, &workflow)
                .is_empty(),
            "{name} was accepted"
        );
    }
    let narrowed = format!("RUST_FEATURES := --features x\n{makefile}");
    assert!(
        !MAKE_COMMON_RUSTDOC
            .problems(&fragment, &narrowed, &workflow)
            .is_empty(),
        "a Makefile narrowing the features was accepted"
    );
    for (name, broken) in [
        (
            "no CI step",
            workflow_text.replace("run: make rust-doc", "run: echo missing documentation gate"),
        ),
        (
            "a CI step allowed to fail",
            workflow_text.replace(
                "run: make rust-doc",
                "run: make rust-doc\n        continue-on-error: true",
            ),
        ),
    ] {
        assert_ne!(
            broken, workflow_text,
            "{name}: the sample is the workflow itself"
        );
        assert!(
            !MAKE_COMMON_RUSTDOC
                .problems(&fragment, &makefile, &parse(CI_WORKFLOW, &broken))
                .is_empty(),
            "{name} was accepted"
        );
    }
}
