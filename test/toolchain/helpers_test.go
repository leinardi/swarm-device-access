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

// Package toolchain_test pins the repository's tooling: the Go line and the hadolint version are
// each one value everywhere they are written, Docker stays out of untagged tests, and no gate
// carries a flag that makes it pass on nothing.
package toolchain_test

import (
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"gopkg.in/yaml.v3"

	"github.com/leinardi/swarm-device-access/test/internal/toolgate"
)

// readRepoFile reads a file by its slash-separated path relative to the repository root. Reading
// it is also what puts it in the test cache key.
func readRepoFile(t *testing.T, rel string) string {
	t.Helper()

	data, err := os.ReadFile(filepath.Join(toolgate.RepoRoot(t), filepath.FromSlash(rel)))
	if err != nil {
		t.Fatalf("read %s: %v", rel, err)
	}

	return string(data)
}

// githubYAML lists every YAML file under .github, the workflows and the composite actions alike,
// relative to the repository root.
func githubYAML(t *testing.T) []string {
	t.Helper()

	root := toolgate.RepoRoot(t)

	var files []string

	err := filepath.WalkDir(
		filepath.Join(root, ".github"),
		func(path string, entry fs.DirEntry, err error) error {
			if err != nil {
				return err
			}

			if !entry.IsDir() &&
				(strings.HasSuffix(path, ".yaml") || strings.HasSuffix(path, ".yml")) {
				rel, relErr := filepath.Rel(root, path)
				if relErr != nil {
					return fmt.Errorf("relative path of %s: %w", path, relErr)
				}

				files = append(files, filepath.ToSlash(rel))
			}

			return nil
		},
	)
	if err != nil {
		t.Fatal(err)
	}

	if len(files) == 0 {
		t.Fatal("no YAML found under .github; the checks would see nothing")
	}

	return files
}

// actionStep is one step, in a workflow or a composite action, that uses an action.
type actionStep struct {
	file string
	// action is the uses: value without its @ref.
	action string
	with   map[string]string
}

// githubActionSteps parses every YAML file under .github for steps that use an action.
func githubActionSteps(t *testing.T) []actionStep {
	t.Helper()

	var steps []actionStep

	for _, file := range githubYAML(t) {
		parsed, err := parseActionSteps(file, []byte(readRepoFile(t, file)))
		if err != nil {
			t.Fatal(err)
		}

		steps = append(steps, parsed...)
	}

	return steps
}

// parseActionSteps finds every mapping with a uses: key anywhere in the document, so job steps,
// composite action steps and reusable workflow calls alike.
func parseActionSteps(file string, content []byte) ([]actionStep, error) {
	var document any

	err := yaml.Unmarshal(content, &document)
	if err != nil {
		return nil, fmt.Errorf("parse %s: %w", file, err)
	}

	var steps []actionStep

	var walk func(node any)

	walk = func(node any) {
		switch value := node.(type) {
		case map[string]any:
			if uses, ok := value["uses"].(string); ok {
				step := actionStep{
					file:   file,
					action: strings.SplitN(uses, "@", 2)[0],
					with:   map[string]string{},
				}

				inputs, _ := value["with"].(map[string]any)
				for key, input := range inputs {
					step.with[key] = fmt.Sprint(input)
				}

				steps = append(steps, step)
			}

			for _, child := range value {
				walk(child)
			}
		case []any:
			for _, child := range value {
				walk(child)
			}
		}
	}

	walk(document)

	return steps, nil
}
