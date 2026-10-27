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

package toolchain_test

import (
	"fmt"
	"slices"
	"strings"
	"testing"

	"gopkg.in/yaml.v3"
)

const ciWorkflow = ".github/workflows/ci.yaml"

// skippedHookRunners names, for each hook pre-commit-all may skip, the ci.yaml job that runs it
// instead. A skip is only safe when that job exists and ci-result needs it.
var skippedHookRunners = map[string]string{
	"go-test-repo-mod": "test",
	"audit-deps-go":    "audit-deps",
	"cargo-test":       "rust",
	"cargo-deny":       "rust",
}

// annotateOnlyJobs only post review comments on a pull request; ci-result does not need them,
// because pre-commit-all runs the same hooks on every file.
var annotateOnlyJobs = []string{
	"actionlint", "pre-commit-hooks", "markdownlint", "shellcheck", "yamllint",
}

// ciJobs is the part of ci.yaml these checks read.
type ciJobs struct {
	Jobs map[string]struct {
		Needs []string `yaml:"needs"`
		Steps []struct {
			Env map[string]string `yaml:"env"`
		} `yaml:"steps"`
	} `yaml:"jobs"`
}

// TestCIGatesAreRequired: every job of ci.yaml that is a gate rather than an annotation is needed
// by ci-result, the one required check, so a new gate (the rust job included) cannot fail without
// failing the pull request.
func TestCIGatesAreRequired(t *testing.T) {
	t.Parallel()

	for _, problem := range ciResultProblems(parseCI(t, readRepoFile(t, ciWorkflow))) {
		t.Error(problem)
	}
}

// TestPreCommitAllSkipsOnlyHooksRunElsewhere: pre-commit-all runs every hook on every file except
// the ones in its SKIP list, and each of those must be run by a job ci-result needs.
func TestPreCommitAllSkipsOnlyHooksRunElsewhere(t *testing.T) {
	t.Parallel()

	workflow := parseCI(t, readRepoFile(t, ciWorkflow))
	hooks := readRepoFile(t, ".pre-commit-config.yaml")

	for _, problem := range skipProblems(workflow, hooks) {
		t.Error(problem)
	}
}

// TestCIStructureRules is the negative control of the two checks above.
func TestCIStructureRules(t *testing.T) {
	t.Parallel()

	workflow := parseCI(t, `jobs:
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
`)
	hooks := "      - id: cargo-test\n      - id: cargo-clippy\n"

	// rust is a gate ci-result does not need; markdownlint only annotates.
	if problems := ciResultProblems(workflow); len(problems) != 1 ||
		!strings.Contains(problems[0], "rust") {
		t.Errorf("want one problem naming rust, got %q", problems)
	}

	// cargo-test is run by rust, which ci-result does not need; cargo-clippy is run by no
	// dedicated job; unknown-hook is not a hook at all.
	if problems := skipProblems(workflow, hooks); len(problems) != 3 {
		t.Errorf("want 3 problems, got %d: %q", len(problems), problems)
	}

	if problems := skipProblems(parseCI(t, "jobs:\n  ci-result:\n    needs: []\n"), hooks); len(
		problems) != 1 {
		t.Errorf("a workflow without pre-commit-all was accepted: %q", problems)
	}
}

func parseCI(t *testing.T, content string) ciJobs {
	t.Helper()

	var workflow ciJobs

	err := yaml.Unmarshal([]byte(content), &workflow)
	if err != nil {
		t.Fatalf("parse the CI workflow: %v", err)
	}

	return workflow
}

// ciResultProblems lists every gate job ci-result does not need.
func ciResultProblems(workflow ciJobs) []string {
	result, ok := workflow.Jobs["ci-result"]
	if !ok {
		return []string{"the CI workflow has no ci-result job"}
	}

	var problems []string

	for name := range workflow.Jobs {
		if name == "ci-result" || slices.Contains(annotateOnlyJobs, name) {
			continue
		}

		if !slices.Contains(result.Needs, name) {
			problems = append(problems, fmt.Sprintf("ci-result does not need the %s job", name))
		}
	}

	slices.Sort(problems)

	return problems
}

// skipProblems lists every hook pre-commit-all skips that is not a hook of hooks (the pre-commit
// configuration) or is not run by a job ci-result needs.
func skipProblems(workflow ciJobs, hooks string) []string {
	job, ok := workflow.Jobs["pre-commit-all"]
	if !ok {
		return []string{"the CI workflow has no pre-commit-all job"}
	}

	needed := workflow.Jobs["ci-result"].Needs

	var problems []string

	for _, step := range job.Steps {
		for hook := range strings.SplitSeq(step.Env["SKIP"], ",") {
			hook = strings.TrimSpace(hook)
			if hook == "" {
				continue
			}

			if !strings.Contains(hooks, "- id: "+hook+"\n") {
				problems = append(problems, "pre-commit-all skips "+hook+", which is no hook")

				continue
			}

			runner, known := skippedHookRunners[hook]

			switch {
			case !known:
				problems = append(problems, "pre-commit-all skips "+hook+
					", which no dedicated job is recorded to run")
			case !slices.Contains(needed, runner):
				problems = append(problems, fmt.Sprintf(
					"pre-commit-all skips %s, run by the %s job, which ci-result does not need",
					hook, runner))
			}
		}
	}

	return problems
}
