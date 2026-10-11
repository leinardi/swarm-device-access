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

package docs_test

import (
	"os"
	"os/exec"
	"slices"
	"strings"
	"testing"

	"github.com/leinardi/swarm-device-access/test/docs"
	"github.com/leinardi/swarm-device-access/test/internal/toolgate"
)

// TestRelativeLinksResolve checks every relative link and fragment in the tracked Markdown.
func TestRelativeLinksResolve(t *testing.T) {
	t.Parallel()

	root := toolgate.RepoRoot(t)
	checker := docs.NewChecker(root)

	files := markdownFiles(t, root)
	if len(files) == 0 {
		t.Fatal("git ls-files lists no Markdown; the check would pass on nothing")
	}

	for _, rel := range files {
		problems, err := checker.CheckFile(rel)
		if err != nil {
			t.Fatal(err)
		}

		for _, problem := range problems {
			t.Error(problem)
		}
	}
}

// TestCheckerFindsBrokenLinks proves the link check is not blind, on a scratch tree.
func TestCheckerFindsBrokenLinks(t *testing.T) {
	t.Parallel()

	dir := t.TempDir()
	writeFixture(t, dir, "a.md", strings.Join([]string{
		"# Title",
		"## 1. Section — `GET /x`",
		"[ok](b.md#heading-two) [ok](#1-section--get-x) [ok](https://example.com/missing)",
		"[broken file](missing.md) [broken anchor](b.md#nope) `[in code](missing-too.md)`",
		"```text",
		"[in a fence](missing-three.md)",
		"```",
		"[outside](../outside.md)",
	}, "\n"))
	writeFixture(t, dir, "b.md", "# Heading Two\n")

	problems, err := docs.NewChecker(dir).CheckFile("a.md")
	if err != nil {
		t.Fatal(err)
	}

	got := make([]string, 0, len(problems))
	for _, problem := range problems {
		got = append(got, problem.String())
	}

	want := []string{
		`a.md:4: link "missing.md": missing.md does not exist`,
		`a.md:4: link "b.md#nope": b.md has no heading or anchor #nope`,
		`a.md:8: link "../outside.md" leaves the repository`,
	}
	if !slices.Equal(got, want) {
		t.Errorf("problems = %q, want %q", got, want)
	}
}

// TestSlug pins the GitHub slug rules the anchors follow.
func TestSlug(t *testing.T) {
	t.Parallel()

	for heading, want := range map[string]string{
		"Why the Build Happens Before the Tag":    "why-the-build-happens-before-the-tag",
		"Dry-Run Coverage":                        "dry-run-coverage",
		"**Bold** and _emphasis_ with snake_case": "bold-and-emphasis-with-snake_case",
		"[Linked](other.md) heading":              "linked-heading",
		"1. Entrypoint (`cmd/`) — config":         "1-entrypoint-cmd--config",
		"Tags, Moves and `X.Y.Z`":                 "tags-moves-and-xyz",
		"Ünïcode Lëtters Stay":                    "ünïcode-lëtters-stay",
	} {
		if got := docs.Slug(heading); got != want {
			t.Errorf("Slug(%q) = %q, want %q", heading, got, want)
		}
	}
}

// markdownFiles returns the tracked Markdown as slash-separated paths relative to root.
func markdownFiles(t *testing.T, root string) []string {
	t.Helper()

	cmd := exec.CommandContext(t.Context(), "git", "ls-files", "-z", "--", "*.md")
	cmd.Dir = root

	out, err := cmd.Output()
	if err != nil {
		t.Fatalf("git ls-files: %v", err)
	}

	var files []string

	for rel := range strings.SplitSeq(strings.TrimRight(string(out), "\x00"), "\x00") {
		if rel != "" {
			files = append(files, rel)
		}
	}

	return files
}

func writeFixture(t *testing.T, dir, name, content string) {
	t.Helper()

	root, err := os.OpenRoot(dir)
	if err != nil {
		t.Fatalf("open %s: %v", dir, err)
	}

	defer func() { _ = root.Close() }()

	err = root.WriteFile(name, []byte(content), 0o600)
	if err != nil {
		t.Fatalf("write %s: %v", name, err)
	}
}
