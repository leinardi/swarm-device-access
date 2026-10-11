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
	"regexp"
	"testing"
)

const setupGo = "actions/setup-go"

var (
	// goDirective is go.mod's go line, with its patch version.
	goDirective = regexp.MustCompile(`(?m)^go (\d+\.\d+\.\d+)$`)
	// golangImage is the version part of a dhi.io/golang tag: 1.26.9 in 1.26.9-alpine3.23-dev.
	golangImage = regexp.MustCompile(`dhi\.io/golang:([^-@\s]+)-`)
)

// TestGoLineIsOneValue: the go directive in go.mod is the Go every build uses. Each build stage of
// the Dockerfile names that exact version in its golang tag, so the image builds with the Go CI
// tests and scans, and every setup-go step reads go.mod instead of writing a version of its own.
func TestGoLineIsOneValue(t *testing.T) {
	t.Parallel()

	match := goDirective.FindStringSubmatch(readRepoFile(t, "go.mod"))
	if match == nil {
		t.Fatal("go.mod has no go directive with a patch version (go X.Y.Z)")
	}

	want := match[1]

	tags := golangImage.FindAllStringSubmatch(readRepoFile(t, "deployments/docker/Dockerfile"), -1)
	if len(tags) == 0 {
		t.Error("deployments/docker/Dockerfile builds on no dhi.io/golang image")
	}

	for _, tag := range tags {
		if tag[1] != want {
			t.Errorf("deployments/docker/Dockerfile builds on golang %s, but go.mod says go %s",
				tag[1], want)
		}
	}

	for _, problem := range goStepProblems(githubActionSteps(t)) {
		t.Error(problem)
	}
}

// TestGoStepRules is the negative control of the setup-go rules.
func TestGoStepRules(t *testing.T) {
	t.Parallel()

	steps, err := parseActionSteps("sample.yaml", []byte(`jobs:
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
`))
	if err != nil {
		t.Fatal(err)
	}

	// The first unpinned step writes a version and reads no file: two problems. The two after it
	// read no go.mod, and the other action writes a version.
	if problems := goStepProblems(steps); len(problems) != 5 {
		t.Errorf("want 5 problems, got %d: %q", len(problems), problems)
	}
}

// TestHadolintIsOneVersion: the hadolint hook runs the system binary, so the version CI installs
// is the one that lints; it must follow the hook's rev, which Dependabot moves.
func TestHadolintIsOneVersion(t *testing.T) {
	t.Parallel()

	hook := regexp.MustCompile(`repo: https://github\.com/hadolint/hadolint\s+rev:\s*(v[\d.]+)`).
		FindStringSubmatch(readRepoFile(t, ".pre-commit-config.yaml"))
	action := regexp.MustCompile(`HADOLINT_VERSION:\s*["']?(v[\d.]+)`).
		FindStringSubmatch(readRepoFile(t, ".github/actions/setup-hadolint/action.yaml"))

	if hook == nil || action == nil {
		t.Fatalf("hadolint version not found: hook %v, action %v", hook, action)
	}

	if hook[1] != action[1] {
		t.Errorf("the hadolint hook is at %s, but setup-hadolint installs %s", hook[1], action[1])
	}
}

// goStepProblems applies the Go rules to steps: no step writes a go-version, and every setup-go
// step reads go-version-file: go.mod.
func goStepProblems(steps []actionStep) []string {
	var problems []string

	for _, step := range steps {
		if step.with["go-version"] != "" {
			problems = append(problems, fmt.Sprintf(
				"%s: %s gets go-version %q; the Go line comes from go.mod",
				step.file, step.action, step.with["go-version"]))
		}

		if step.action == setupGo && step.with["go-version-file"] != "go.mod" {
			problems = append(problems, step.file+": setup-go must read go-version-file: go.mod")
		}
	}

	return problems
}

// TestGoDirectiveReading pins the go.mod reading the Go line rests on.
func TestGoDirectiveReading(t *testing.T) {
	t.Parallel()

	if match := goDirective.FindStringSubmatch("module x\n\ngo 1.26.9\n"); len(match) != 2 ||
		match[1] != "1.26.9" {
		t.Errorf("go 1.26.9 read as %v", match)
	}

	if goDirective.MatchString("module x\n\ngo 1.26\n") {
		t.Error("a go directive without a patch version was accepted")
	}

	tag := golangImage.FindStringSubmatch("FROM dhi.io/golang:1.26.9-alpine3.23-dev AS deps")
	if len(tag) != 2 || tag[1] != "1.26.9" {
		t.Errorf("the golang tag read as %v", tag)
	}

	// The floating tag reads as 1, which no go directive equals.
	floating := golangImage.FindStringSubmatch("FROM dhi.io/golang:1-alpine3.23-dev")
	if len(floating) != 2 || floating[1] != "1" {
		t.Errorf("the floating tag read as %v", floating)
	}
}
