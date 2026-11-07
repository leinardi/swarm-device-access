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

use repo_meta::ci::CiGates;
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
