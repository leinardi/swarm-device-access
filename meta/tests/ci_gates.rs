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
//! needed job runs instead. The Rust side of `test/toolchain/ci_test.go`, which stays until the
//! cutover; SSE has no counterpart.

use std::collections::BTreeMap;

use meta::repo;
use serde_yaml_ng::Value;

const CI_WORKFLOW: &str = ".github/workflows/ci.yaml";
const RESULT_JOB: &str = "ci-result";
const PRE_COMMIT_JOB: &str = "pre-commit-all";

/// For each hook `pre-commit-all` may skip, the ci.yaml job that runs it instead. A skip is only
/// safe when that job exists and `ci-result` needs it.
const SKIPPED_HOOK_RUNNERS: [(&str, &str); 4] = [
    ("go-test-repo-mod", "test"),
    ("audit-deps-go", "audit-deps"),
    ("cargo-test", "rust"),
    ("cargo-deny", "rust"),
];

/// The jobs that only post review comments on a pull request; `ci-result` does not need them,
/// because `pre-commit-all` runs the same hooks on every file.
const ANNOTATE_ONLY_JOBS: [&str; 5] = [
    "actionlint",
    "pre-commit-hooks",
    "markdownlint",
    "shellcheck",
    "yamllint",
];

/// One job of the workflow: the jobs it needs, and the `SKIP` value of each of its steps.
#[derive(Debug, Default)]
struct Job {
    needs: Vec<String>,
    skips: Vec<String>,
}

/// The part of ci.yaml these checks read, by job name.
fn parse_ci(content: &str) -> BTreeMap<String, Job> {
    let document: Value = serde_yaml_ng::from_str(content)
        .unwrap_or_else(|error| panic!("parse the CI workflow: {error}"));
    let Some(jobs) = document.get("jobs").and_then(Value::as_mapping) else {
        return BTreeMap::new();
    };
    jobs.iter()
        .filter_map(|(name, job)| {
            let needs = match job.get("needs") {
                Some(Value::String(need)) => vec![need.clone()],
                Some(Value::Sequence(needs)) => needs
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
                Some(
                    Value::Null
                    | Value::Bool(_)
                    | Value::Number(_)
                    | Value::Mapping(_)
                    | Value::Tagged(_),
                )
                | None => Vec::new(),
            };
            let skips = job
                .get("steps")
                .and_then(Value::as_sequence)
                .map(|steps| {
                    steps
                        .iter()
                        .filter_map(|step| step.get("env")?.get("SKIP")?.as_str())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            Some((name.as_str()?.to_owned(), Job { needs, skips }))
        })
        .collect()
}

/// Every gate job `ci-result` does not need.
fn ci_result_problems(jobs: &BTreeMap<String, Job>) -> Vec<String> {
    let Some(result) = jobs.get(RESULT_JOB) else {
        return vec![format!("the CI workflow has no {RESULT_JOB} job")];
    };
    jobs.keys()
        .filter(|name| *name != RESULT_JOB && !ANNOTATE_ONLY_JOBS.contains(&name.as_str()))
        .filter(|name| !result.needs.contains(name))
        .map(|name| format!("{RESULT_JOB} does not need the {name} job"))
        .collect()
}

/// Every hook `pre-commit-all` skips that is not a hook of `hooks` (the pre-commit
/// configuration), or is not run by a job `ci-result` needs.
fn skip_problems(jobs: &BTreeMap<String, Job>, hooks: &str) -> Vec<String> {
    let Some(job) = jobs.get(PRE_COMMIT_JOB) else {
        return vec![format!("the CI workflow has no {PRE_COMMIT_JOB} job")];
    };
    let needed: &[String] = jobs
        .get(RESULT_JOB)
        .map_or(&[], |result| result.needs.as_slice());
    let mut problems = Vec::new();
    for hook in job.skips.iter().flat_map(|skip| skip.split(',')) {
        let hook = hook.trim();
        if hook.is_empty() {
            continue;
        }
        if !hooks.contains(&format!("- id: {hook}\n")) {
            problems.push(format!("{PRE_COMMIT_JOB} skips {hook}, which is no hook"));
            continue;
        }
        match SKIPPED_HOOK_RUNNERS
            .iter()
            .find(|(skipped, _)| *skipped == hook)
        {
            None => problems.push(format!(
                "{PRE_COMMIT_JOB} skips {hook}, which no dedicated job is recorded to run"
            )),
            Some((_, runner)) if !needed.iter().any(|need| need == runner) => {
                problems.push(format!(
                    "{PRE_COMMIT_JOB} skips {hook}, run by the {runner} job, which {RESULT_JOB} \
                     does not need"
                ));
            }
            Some(_) => {}
        }
    }
    problems
}

/// Go: `TestCIGatesAreRequired`. Every job of ci.yaml that is a gate rather than an annotation
/// is needed by `ci-result`, so a new gate (the rust job included) cannot fail without failing
/// the pull request.
#[test]
fn ci_gates_are_required() {
    let problems = ci_result_problems(&parse_ci(&repo::read(CI_WORKFLOW)));
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// Go: `TestPreCommitAllSkipsOnlyHooksRunElsewhere`. `pre-commit-all` runs every hook on every
/// file except the ones in its `SKIP` list, and each of those must be run by a job `ci-result`
/// needs.
#[test]
fn pre_commit_all_skips_only_hooks_run_elsewhere() {
    let jobs = parse_ci(&repo::read(CI_WORKFLOW));
    assert!(
        jobs.get(PRE_COMMIT_JOB)
            .is_some_and(|job| !job.skips.is_empty()),
        "{PRE_COMMIT_JOB} skips nothing; the check would pass on nothing"
    );
    let problems = skip_problems(&jobs, &repo::read(".pre-commit-config.yaml"));
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// Go: `TestCIStructureRules`, the negative control of the two checks above.
#[test]
fn ci_structure_rules() {
    let jobs = parse_ci(
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
    );
    let hooks = "      - id: cargo-test\n      - id: cargo-clippy\n";

    // rust is a gate ci-result does not need; markdownlint only annotates.
    let problems = ci_result_problems(&jobs);
    assert!(
        problems.len() == 1 && problems[0].contains("rust"),
        "want one problem naming rust, got {problems:?}"
    );

    // cargo-test is run by rust, which ci-result does not need; cargo-clippy is run by no
    // dedicated job; unknown-hook is not a hook at all.
    let problems = skip_problems(&jobs, hooks);
    assert_eq!(problems.len(), 3, "{problems:#?}");

    let problems = skip_problems(&parse_ci("jobs:\n  ci-result:\n    needs: []\n"), hooks);
    assert_eq!(
        problems.len(),
        1,
        "a workflow without pre-commit-all: {problems:?}"
    );
    let problems = ci_result_problems(&parse_ci("jobs:\n  test:\n    steps: []\n"));
    assert_eq!(
        problems.len(),
        1,
        "a workflow without ci-result: {problems:?}"
    );
}
