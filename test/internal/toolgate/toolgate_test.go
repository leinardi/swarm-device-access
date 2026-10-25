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

package toolgate_test

import (
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"

	"github.com/leinardi/swarm-device-access/test/internal/toolgate"
)

// helperEnv selects the lookup the tests below run in a child test process.
const helperEnv = "TOOLGATE_LOOKUP_HELPER"

// TestLookupHelper is not a test on its own: TestLookupMissingTool runs the test binary again with
// only this test selected and helperEnv set, so the skip or failure Lookup ends it with is the
// child's outcome to inspect, not this process's. It looks the tools up as the Dockerfile test
// does.
func TestLookupHelper(t *testing.T) {
	if os.Getenv(helperEnv) != "1" {
		t.Skip("runs only as the child process of TestLookupMissingTool")
	}

	_ = toolgate.Lookup(t, toolgate.Droast)
	_ = toolgate.Lookup(t, toolgate.ShellCheck)

	t.Log("both tools found")
}

// TestLookupMissingTool pins Lookup's two outcomes for a missing tool: a skip on a developer
// machine, a failure under RequireEnv=1. droast alone is not enough, because droast.toml makes the
// ShellCheck bridge required. Finding both is the control that the stubs are found at all.
func TestLookupMissingTool(t *testing.T) {
	t.Parallel()

	both := []string{toolgate.Droast, toolgate.ShellCheck}

	for name, tc := range map[string]struct {
		stubs   []string
		require string
		outcome string
		want    string
	}{
		"neither, local":    {outcome: "SKIP", want: "droast not found"},
		"neither, required": {require: "1", outcome: "FAIL", want: "droast is required"},
		"neither, not 1":    {require: "true", outcome: "SKIP", want: "droast not found"},
		"only droast, local": {
			stubs: both[:1], outcome: "SKIP", want: "shellcheck not found",
		},
		"only droast, required": {
			stubs: both[:1], require: "1", outcome: "FAIL", want: "shellcheck is required",
		},
		"only shellcheck, local": {stubs: both[1:], outcome: "SKIP", want: "droast not found"},
		"both, required": {
			stubs: both, require: "1", outcome: "PASS", want: "both tools found",
		},
	} {
		t.Run(name, func(t *testing.T) {
			t.Parallel()

			out, err := runHelper(t, tc.stubs, tc.require)
			if (tc.outcome == "FAIL") != (err != nil) {
				t.Errorf("the child exited with %v, want outcome %s:\n%s", err, tc.outcome, out)
			}

			if !strings.Contains(out, "--- "+tc.outcome+": TestLookupHelper") {
				t.Errorf("the lookup did not end in %s:\n%s", tc.outcome, out)
			}

			if !strings.Contains(out, tc.want) {
				t.Errorf("the output does not say %q:\n%s", tc.want, out)
			}
		})
	}
}

// runHelper runs TestLookupHelper in a child process whose PATH holds only the tools named in
// stubs, each a link to the test binary (Lookup only checks that it is an executable file), with
// RequireEnv set to require.
func runHelper(t *testing.T, stubs []string, require string) (string, error) {
	t.Helper()

	self, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}

	bin := t.TempDir()

	for _, stub := range stubs {
		err = os.Symlink(self, filepath.Join(bin, stub))
		if err != nil {
			t.Fatal(err)
		}
	}

	env := []string{"PATH=" + bin, toolgate.RequireEnv + "=" + require, helperEnv + "=1"}

	for _, entry := range os.Environ() {
		key, _, _ := strings.Cut(entry, "=")
		if key != "PATH" && key != toolgate.RequireEnv && key != helperEnv {
			env = append(env, entry)
		}
	}

	cmd := exec.CommandContext(t.Context(), self, "-test.run=^TestLookupHelper$", "-test.v",
		"-test.count=1")
	cmd.Env = env

	out, err := cmd.CombinedOutput()

	return string(out), err
}
