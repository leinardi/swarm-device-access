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

// Package toolgate finds the external tools that the behavioral meta-tests drive.
//
// A behavioral test that skips when its tool is missing passes in exactly the place it is meant to
// guard: CI. So a missing tool is a skip on a developer machine and a failure wherever
// SDA_REQUIRE_TOOLS=1 is set, which the repo-gates CI job does. The tools are a closed list:
// Docker is deliberately not on it, because every Docker-dependent test sits behind the
// integration build tag and never runs untagged.
package toolgate

import (
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"testing"
)

// RequireEnv is the variable that turns a missing tool from a skip into a failure.
const RequireEnv = "SDA_REQUIRE_TOOLS"

// The tools a meta-test may gate on: the Dockerfile tests run droast with the repository's
// droast.toml, whose required ShellCheck bridge runs the shellcheck on PATH.
const (
	Droast     = "droast"
	ShellCheck = "shellcheck"
)

// Tools is the closed list of tools a meta-test may be gated on.
var Tools = []string{Droast, ShellCheck}

// Lookup returns the path of tool, or skips the test when it is missing locally and fails it when
// RequireEnv is set to 1. A tool outside Tools fails the test: widening the list is a decision, not
// a side effect of a new test.
func Lookup(t *testing.T, tool string) string {
	t.Helper()

	if !slices.Contains(Tools, tool) {
		t.Fatalf("toolgate: %q is not one of the gated tools %v", tool, Tools)
	}

	path, err := exec.LookPath(tool)
	if err == nil {
		return path
	}

	if os.Getenv(RequireEnv) == "1" {
		t.Fatalf("%s is required (%s=1) but was not found: %v", tool, RequireEnv, err)
	}

	t.Skipf("%s not found (%v); set %s=1 to make this a failure", tool, err, RequireEnv)

	return ""
}

// RepoRoot returns the repository root, found as the nearest ancestor of the working directory
// that holds go.mod.
func RepoRoot(t *testing.T) string {
	t.Helper()

	dir, err := os.Getwd()
	if err != nil {
		t.Fatalf("toolgate: working directory: %v", err)
	}

	for {
		_, statErr := os.Stat(filepath.Join(dir, "go.mod"))
		if statErr == nil {
			return dir
		}

		parent := filepath.Dir(dir)
		if parent == dir {
			t.Fatal("toolgate: no go.mod above the working directory")
		}

		dir = parent
	}
}
