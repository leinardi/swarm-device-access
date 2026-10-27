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

const (
	setupGo = "actions/setup-go"
	// rustToolchainAction installs the Rust toolchain in CI.
	rustToolchainAction = "dtolnay/rust-toolchain"
)

var (
	// rustChannel is rust-toolchain.toml's channel, which must name one exact release.
	rustChannel = regexp.MustCompile(`(?m)^channel = "([^"]*)"$`)
	// rustVersion is the workspace's rust-version in the root Cargo.toml.
	rustVersion = regexp.MustCompile(`(?m)^rust-version = "([^"]*)"$`)
	// exactRust is a Rust release with its patch: 1.98.1, never 1.98 or stable.
	exactRust = regexp.MustCompile(`^\d+\.\d+\.\d+$`)

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

// TestRustLineIsOneRelease: one exact Rust release, patch included, is shared by
// rust-toolchain.toml's channel, the workspace's rust-version and every CI toolchain step, so the
// toolchain a developer, a hook and CI build with is one compiler.
func TestRustLineIsOneRelease(t *testing.T) {
	t.Parallel()

	channel := rustChannel.FindStringSubmatch(readRepoFile(t, "rust-toolchain.toml"))
	if len(channel) != 2 || !exactRust.MatchString(channel[1]) {
		t.Fatalf("rust-toolchain.toml names no exact release (channel = \"X.Y.Z\"): %v", channel)
	}

	want := channel[1]

	version := rustVersion.FindStringSubmatch(readRepoFile(t, "Cargo.toml"))
	if version == nil {
		t.Error("the root Cargo.toml sets no rust-version")
	} else if version[1] != want {
		t.Errorf(
			"Cargo.toml's rust-version is %q, but rust-toolchain.toml says %s",
			version[1],
			want,
		)
	}

	steps := githubActionSteps(t)

	toolchainSteps := 0

	for _, step := range steps {
		if step.action == rustToolchainAction {
			toolchainSteps++
		}
	}

	if toolchainSteps == 0 {
		t.Errorf("no %s step under .github; the check would see nothing", rustToolchainAction)
	}

	for _, problem := range rustStepProblems(steps, want) {
		t.Error(problem)
	}
}

// TestRustStepRules is the negative control of the Rust toolchain step rules.
func TestRustStepRules(t *testing.T) {
	t.Parallel()

	steps, err := parseActionSteps("sample.yaml", []byte(`jobs:
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
`))
	if err != nil {
		t.Fatal(err)
	}

	// Another release, a channel, a release without its patch, and no toolchain at all.
	if problems := rustStepProblems(steps, "1.98.1"); len(problems) != 4 {
		t.Errorf("want 4 problems, got %d: %q", len(problems), problems)
	}
}

// TestRustLineReading pins the readings the Rust line rests on: a channel or a release without
// its patch is not an exact release.
func TestRustLineReading(t *testing.T) {
	t.Parallel()

	channel := rustChannel.FindStringSubmatch("[toolchain]\nchannel = \"1.98.1\"\n")
	if len(channel) != 2 || channel[1] != "1.98.1" {
		t.Errorf("channel read as %v", channel)
	}

	version := rustVersion.FindStringSubmatch("[workspace.package]\nrust-version = \"1.98\"\n")
	if len(version) != 2 || version[1] != "1.98" {
		t.Errorf("rust-version read as %v", version)
	}

	for _, inexact := range []string{"stable", "1.98", "nightly-2026-10-01", "1.98.1-x86_64"} {
		if exactRust.MatchString(inexact) {
			t.Errorf("%q was accepted as an exact release", inexact)
		}
	}
}

// rustStepProblems applies the Rust rule to steps: every toolchain step installs exactly want.
func rustStepProblems(steps []actionStep, want string) []string {
	var problems []string

	for _, step := range steps {
		if step.action != rustToolchainAction {
			continue
		}

		if got := step.with["toolchain"]; got != want {
			problems = append(problems, fmt.Sprintf(
				"%s: %s installs toolchain %q; it must be %s, the release of rust-toolchain.toml",
				step.file, step.action, got, want))
		}
	}

	return problems
}

// TestShellCheckIsOneVersion: droast's ShellCheck bridge runs the shellcheck on PATH, which CI
// installs with setup-shellcheck; it must be the release the shellcheck-py hook wraps (its rev adds
// a fourth component for the wrapper), which Dependabot moves.
func TestShellCheckIsOneVersion(t *testing.T) {
	t.Parallel()

	expectHookVersion(t, "ShellCheck",
		`repo: https://github\.com/shellcheck-py/shellcheck-py\s+rev:\s*(v\d+\.\d+\.\d+)`,
		".github/actions/setup-shellcheck/action.yaml",
		`SHELLCHECK_VERSION:\s*["']?(v[\d.]+)`)
}

// TestDroastIsOneVersion: the droast hook builds the release at its rev, which Dependabot moves,
// and the Dockerfile meta-tests run the binary CI installs with setup-droast; both must be the
// same release, so the label test checks the droast that lints.
func TestDroastIsOneVersion(t *testing.T) {
	t.Parallel()

	expectHookVersion(t, "droast",
		`repo: https://github\.com/immanuwell/dockerfile-roast\s+rev:\s*["']?([\d.]+)`,
		".github/actions/setup-droast/action.yaml",
		`DROAST_VERSION:\s*["']?([\d.]+)`)
}

// expectHookVersion fails the test unless the version the hook pattern captures in
// .pre-commit-config.yaml equals the one the action pattern captures in actionFile.
func expectHookVersion(t *testing.T, tool, hookPattern, actionFile, actionPattern string) {
	t.Helper()

	hook := regexp.MustCompile(hookPattern).
		FindStringSubmatch(readRepoFile(t, ".pre-commit-config.yaml"))
	action := regexp.MustCompile(actionPattern).FindStringSubmatch(readRepoFile(t, actionFile))

	if hook == nil || action == nil {
		t.Fatalf("%s version not found: hook %v, action %v", tool, hook, action)
	}

	if hook[1] != action[1] {
		t.Errorf("the %s hook is at %s, but %s installs %s", tool, hook[1], actionFile, action[1])
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
