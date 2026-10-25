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

package dockerfiles_test

import (
	"io/fs"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"slices"
	"strings"
	"testing"

	"github.com/leinardi/swarm-device-access/test/dockerfiles"
	"github.com/leinardi/swarm-device-access/test/internal/toolgate"
)

const (
	shippedDockerfile = "deployments/docker/Dockerfile"
	binary            = "/usr/local/bin/swarm-device-access"
)

// TestShippedDockerfileIsTheOnlyOne pins that the repository holds exactly the image that ships. A
// second Dockerfile would look shipped to every reader and be built by nothing.
func TestShippedDockerfileIsTheOnlyOne(t *testing.T) {
	t.Parallel()

	if found := allDockerfiles(t); !slices.Equal(found, []string{shippedDockerfile}) {
		t.Errorf("the repository holds the Dockerfiles %v, want exactly [%s]", found,
			shippedDockerfile)
	}
}

// TestSyntaxDirectiveIsPinned pins the BuildKit frontend that reads the Dockerfile. A bare
// docker/dockerfile:1.7 is a tag that can be moved under a build, and Dependabot never updates the
// line, so it is a literal reference pinned by digest.
func TestSyntaxDirectiveIsPinned(t *testing.T) {
	t.Parallel()

	syntax, ok := dockerfiles.Syntax(readRepoFile(t, shippedDockerfile))
	if !ok {
		t.Fatalf("%s has no syntax parser directive on its first line", shippedDockerfile)
	}

	if dockerfiles.Repository(syntax) != "docker/dockerfile" {
		t.Errorf("the frontend is %q, want docker/dockerfile", syntax)
	}

	if !dockerfiles.DigestPinned(syntax) {
		t.Errorf("the frontend %q is not pinned by digest", syntax)
	}
}

// TestShippedStages pins the stages, in order, each from a literal tag. The bases are pinned by
// tag and not by digest, deliberately (the Dockerfile says why): dhi.io republishes its tags to
// ship security fixes. A variable in a reference would hide it from Dependabot, and latest would
// float across releases of the base.
func TestShippedStages(t *testing.T) {
	t.Parallel()

	stages := shippedStages(t)

	names := make([]string, 0, len(stages))
	for _, stage := range stages {
		names = append(names, stage.Name)
	}

	if !slices.Equal(names, []string{"deps", "build", ""}) {
		t.Fatalf("the stages are %q, want deps, build and the unnamed runtime stage, in order",
			names)
	}

	for _, stage := range []dockerfiles.Stage{stages[0], stages[2]} {
		if !dockerfiles.TagPinned(stage.Image) {
			t.Errorf("stage %q is FROM %q, which is not a literal reference pinned by tag",
				stage.Name, stage.Image)
		}
	}

	if stages[1].Image != "deps" {
		t.Errorf("the build stage is FROM %q, want deps, whose module download it reuses",
			stages[1].Image)
	}
}

// TestBuildStages pins where the build runs and what it compiles. deps runs on the build platform
// and Go cross-compiles, so no emulation is needed; the golang tag names go.mod's version, which
// test/toolchain checks.
func TestBuildStages(t *testing.T) {
	t.Parallel()

	stages := shippedStages(t)
	deps, build := stages[0], stages[1]

	if deps.Platform != "$BUILDPLATFORM" {
		t.Errorf("the deps stage runs on --platform=%q, want $BUILDPLATFORM", deps.Platform)
	}

	if got := dockerfiles.Repository(deps.Image); got != "dhi.io/golang" {
		t.Errorf("the deps stage is FROM %s, want dhi.io/golang", got)
	}

	if !strings.Contains(joined(deps), "go mod download") {
		t.Errorf("the deps stage does not download the modules:\n%s", joined(deps))
	}

	instructions := joined(build)
	for _, want := range []string{
		"ARG VERSION", "ARG COMMIT", "ARG DATE", "ARG TARGETOS", "ARG TARGETARCH",
		"CGO_ENABLED=0", "-trimpath", "-buildvcs=false",
		"-X 'main.version=${VERSION}'", "-X 'main.commit=${COMMIT}'", "-X 'main.date=${DATE}'",
		"-o /out/swarm-device-access ./cmd/swarm-device-access",
	} {
		if !strings.Contains(instructions, want) {
			t.Errorf("the build stage lacks %q:\n%s", want, instructions)
		}
	}
}

// TestRuntimeStage pins the image that ships: the static base, no shell to run anything in, the
// binary as the entrypoint, root (bpf(2) and the cgroup writes need it), and the seven OCI labels,
// with the version, revision and creation date from the build arguments.
func TestRuntimeStage(t *testing.T) {
	t.Parallel()

	runtime := shippedStages(t)[2]

	if got := dockerfiles.Repository(runtime.Image); got != "dhi.io/static" {
		t.Errorf("the runtime stage is FROM %s, want dhi.io/static", got)
	}

	if runtime.Platform != "" {
		t.Errorf("the runtime stage pins --platform=%s; it has to follow the target platform",
			runtime.Platform)
	}

	want := map[string]string{
		"USER":       "0",
		"ENTRYPOINT": `["` + binary + `"]`,
		"COPY":       "--from=build /out/swarm-device-access " + binary,
	}

	seen := map[string][]string{}

	for _, instruction := range runtime.Instructions {
		switch instruction.Keyword {
		case "RUN", "SHELL", "VOLUME", "ADD":
			t.Errorf("the runtime stage carries %s %s; the base has no shell",
				instruction.Keyword, instruction.Args)
		}

		seen[instruction.Keyword] = append(seen[instruction.Keyword], instruction.Args)
	}

	for keyword, args := range want {
		if !slices.Equal(seen[keyword], []string{args}) {
			t.Errorf("the runtime stage has %s %q, want exactly %s %s", keyword, seen[keyword],
				keyword, args)
		}
	}

	labels := dockerfiles.Labels(runtime)
	wantLabels := map[string]string{
		"org.opencontainers.image.title":       "swarm-device-access",
		"org.opencontainers.image.description": labels["org.opencontainers.image.description"],
		"org.opencontainers.image.source":      "https://github.com/leinardi/swarm-device-access",
		"org.opencontainers.image.version":     "${VERSION}",
		"org.opencontainers.image.revision":    "${COMMIT}",
		"org.opencontainers.image.created":     "${DATE}",
		"org.opencontainers.image.licenses":    "Apache-2.0",
	}

	if labels["org.opencontainers.image.description"] == "" {
		t.Error("the runtime stage has no org.opencontainers.image.description")
	}

	for key, value := range wantLabels {
		if labels[key] != value {
			t.Errorf("label %s is %q, want %q", key, labels[key], value)
		}
	}

	if len(labels) != len(wantLabels) {
		t.Errorf("the runtime stage carries labels %v, want exactly the seven OCI labels", labels)
	}
}

// TestDroastRequiresTheLabels runs droast with the repository's droast.toml: the shipped
// Dockerfile passes, and the same file without one of its labels fails with DF074, so the required
// labels are enforced and not only written down. droast.toml makes the ShellCheck bridge required,
// so droast fails without shellcheck on PATH: both are looked up before droast runs.
func TestDroastRequiresTheLabels(t *testing.T) {
	t.Parallel()

	droast := toolgate.Lookup(t, toolgate.Droast)
	_ = toolgate.Lookup(t, toolgate.ShellCheck)
	config := repoPath(t, "droast.toml")
	content := readRepoFile(t, shippedDockerfile)
	_ = readRepoFile(t, "droast.toml") // in the test cache key

	out, err := exec.CommandContext(t.Context(), droast, "--config", config,
		repoPath(t, shippedDockerfile)).CombinedOutput()
	if err != nil {
		t.Errorf("droast rejects %s: %v\n%s", shippedDockerfile, err, out)
	}

	label := regexp.MustCompile(`(?m)^\s*org\.opencontainers\.image\.created=.*$`)
	if !label.MatchString(content) {
		t.Fatal("the created label line was not found; the negative control would test nothing")
	}

	stripped := filepath.Join(t.TempDir(), "Dockerfile")

	err = os.WriteFile(stripped, []byte(label.ReplaceAllString(content, "")), 0o600)
	if err != nil {
		t.Fatal(err)
	}

	out, err = exec.CommandContext(t.Context(), droast, "--config", config, stripped).
		CombinedOutput()
	if err == nil || !strings.Contains(string(out), "DF074") {
		t.Errorf("droast accepts the Dockerfile without its created label: %v\n%s", err, out)
	}
}

// TestParseRejectsWhatTheAssertionsMustNotMiss is the negative control for the reader: each of
// these inputs has to come out as something the assertions above refuse.
func TestParseRejectsWhatTheAssertionsMustNotMiss(t *testing.T) {
	t.Parallel()

	const digest = "@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"

	for name, image := range map[string]string{
		"a variable tag":          "dhi.io/golang:${GO_LINE}-alpine3.23-dev",
		"a variable image":        "$BASE",
		"no tag":                  "dhi.io/static",
		"latest":                  "dhi.io/static:latest",
		"a digest":                "dhi.io/static:20250419" + digest,
		"a stage name, not a ref": "deps",
	} {
		if dockerfiles.TagPinned(image) {
			t.Errorf("%s (%q) passes as tag-pinned", name, image)
		}
	}

	if !dockerfiles.TagPinned("dhi.io/static:20250419") {
		t.Error("a literal tag does not pass as tag-pinned")
	}

	for name, image := range map[string]string{
		"a tag only":           "docker/dockerfile:1.7",
		"a short digest":       "docker/dockerfile:1.7@sha256:0123",
		"a digest with no tag": "docker/dockerfile" + digest,
	} {
		if dockerfiles.DigestPinned(image) {
			t.Errorf("%s (%q) passes as digest-pinned", name, image)
		}
	}

	stages, err := dockerfiles.Parse(
		"FROM a:1 AS one\nRUN echo \\\n  two\n# note\nFROM b:2\nUSER 1:1\n",
	)
	if err != nil {
		t.Fatalf("parse: %v", err)
	}

	if len(stages) != 2 || stages[0].Name != "one" || stages[1].Name != "" ||
		joined(stages[0]) != "RUN echo two" || joined(stages[1]) != "USER 1:1" {
		t.Errorf("parsed %+v", stages)
	}

	for name, content := range map[string]string{
		"no directive":         "FROM a:1\n",
		"after a comment":      "# note\n# syntax=docker/dockerfile:1.7" + digest + "\n",
		"after a blank line":   "\n# syntax=docker/dockerfile:1.7" + digest + "\n",
		"after an instruction": "FROM a:1\n# syntax=docker/dockerfile:1.7" + digest + "\n",
	} {
		if syntax, ok := dockerfiles.Syntax(content); ok {
			t.Errorf("%s: read the syntax directive %q, which BuildKit ignores", name, syntax)
		}
	}

	_, err = dockerfiles.Parse("FROM a:1\nRUN echo \\\n")
	if err == nil {
		t.Error("a file ending inside a continuation parsed")
	}
}

func shippedStages(t *testing.T) []dockerfiles.Stage {
	t.Helper()

	stages, err := dockerfiles.Parse(readRepoFile(t, shippedDockerfile))
	if err != nil {
		t.Fatalf("%s: %v", shippedDockerfile, err)
	}

	if len(stages) != 3 {
		t.Fatalf("%s has %d stages, want 3", shippedDockerfile, len(stages))
	}

	return stages
}

func joined(stage dockerfiles.Stage) string {
	lines := make([]string, 0, len(stage.Instructions))
	for _, instruction := range stage.Instructions {
		lines = append(lines, instruction.Keyword+" "+instruction.Args)
	}

	return strings.Join(lines, "\n")
}

func isDockerfile(name string) bool {
	return name == "Dockerfile" || strings.HasSuffix(name, ".Dockerfile")
}

// allDockerfiles walks the repository for Dockerfiles, skipping build outputs. Walking rather than
// asking git keeps a new file in the test cache key.
func allDockerfiles(t *testing.T) []string {
	t.Helper()

	root, err := os.OpenRoot(repoPath(t, "."))
	if err != nil {
		t.Fatalf("open the repository: %v", err)
	}

	defer root.Close()

	var found []string

	err = fs.WalkDir(root.FS(), ".", func(path string, entry fs.DirEntry, walkErr error) error {
		if walkErr != nil {
			return walkErr
		}

		if entry.IsDir() {
			switch entry.Name() {
			case ".git", "dist", ".cache":
				return fs.SkipDir
			}

			return nil
		}

		if isDockerfile(entry.Name()) {
			found = append(found, path)
		}

		return nil
	})
	if err != nil {
		t.Fatalf("walk the repository: %v", err)
	}

	if !slices.Contains(found, shippedDockerfile) {
		t.Fatalf("the walk did not find %s, so it would find nothing else either",
			shippedDockerfile)
	}

	return found
}

func repoPath(t *testing.T, rel string) string {
	t.Helper()

	return filepath.Join(toolgate.RepoRoot(t), filepath.FromSlash(rel))
}

func readRepoFile(t *testing.T, rel string) string {
	t.Helper()

	data, err := os.ReadFile(repoPath(t, rel))
	if err != nil {
		t.Fatalf("read %s: %v", rel, err)
	}

	return string(data)
}
