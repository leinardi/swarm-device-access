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
	"path/filepath"
	"regexp"
	"strings"
	"testing"

	"github.com/leinardi/swarm-device-access/test/internal/toolgate"
)

// vacuousFlags turn a gate with nothing to check, or a failing one, into a pass. None of them may
// appear where gates are defined, outside the reviewed exceptions in vacuousAllowed.
var vacuousFlags = []struct {
	name    string
	pattern *regexp.Regexp
}{
	{"|| true", regexp.MustCompile(`\|\|\s*(true\b|:(\s|;|$)|exit\s+0\b)`)},
	{"make's - recipe prefix", regexp.MustCompile(`^\t[@+]*-`)},
	{".IGNORE", regexp.MustCompile(`^\.IGNORE\b`)},
	{
		"make --ignore-errors",
		regexp.MustCompile(`\$\(MAKE\).*\s(-[a-zA-Z]*i[a-zA-Z]*|--ignore-errors)\b`),
	},
	{"continue-on-error", regexp.MustCompile(`continue-on-error`)},
	{
		"if: false",
		regexp.MustCompile(`\bif:\s*(["']?false["']?\s*$|["']?\$\{\{\s*false\s*\}\}["']?)`),
	},
	// The Rust gates: a test run that only compiles, lints capped or allowed wholesale, tests
	// skipped by name, or a workspace member left out of a workspace-wide command. A cargo call is
	// cargo itself or make's $(CARGO).
	{"cargo test --no-run", regexp.MustCompile(`(?i)\bcargo\)?\s.*\btest\b.*\s--no-run\b`)},
	{"--cap-lints", regexp.MustCompile(`--cap-lints\b`)},
	{"warnings allowed", regexp.MustCompile(`(\s-A\s*|--allow[ =])warnings\b`)},
	{"cargo --skip", regexp.MustCompile(`(?i)\bcargo\)?\s.*\s--skip\b`)},
	{"cargo --exclude", regexp.MustCompile(`(?i)\bcargo\)?\s.*\s--exclude\b`)},
}

// clippyCall is a clippy run in a gate (cargo itself or make's $(CARGO)), which must turn warnings
// into errors.
var clippyCall = regexp.MustCompile(`(?i)\bcargo\)?\s+(\+\S+\s+)?clippy\b`)

// yamlName is a YAML name: line, a hook's or a step's label rather than a command it runs.
var yamlName = regexp.MustCompile(`^\s*(-\s+)?name:\s`)

// clippyDeniesWarnings is the -D warnings a clippy gate needs: without it every finding is a
// warning, and the gate passes whatever clippy reports.
var clippyDeniesWarnings = regexp.MustCompile(`\s(-D\s*|--deny[ =])warnings\b`)

// vacuousAllowed are the reviewed uses, by file and the exact text the line carries. Each one
// tolerates a command whose failure is an answer, not an error; none of them is a gate.
var vacuousAllowed = []struct {
	file, text string
}{
	// grep exits 1 when no tag matches the release pattern, and "no release tag" is a result the
	// release workflow handles after the pipe.
	{".github/workflows/release.yaml", `{ grep -E "$RELEASE_TAG_REGEX" || true; }`},
	// HEAD^ does not exist on a root commit, which has no previous release.
	{".github/workflows/release.yaml", `{ git tag --merged "HEAD^" 2>/dev/null || true; }`},
}

// TestNoVacuousFlags scans the files that define gates: the Makefile and its fragments, the
// pre-commit configuration and every YAML file under .github, the workflows included.
func TestNoVacuousFlags(t *testing.T) {
	t.Parallel()

	root := toolgate.RepoRoot(t)

	fragments, err := filepath.Glob(filepath.Join(root, ".mk", "*.mk"))
	if err != nil || len(fragments) == 0 {
		t.Fatalf("no make fragments found under .mk/ (%v); the scan would check nothing", err)
	}

	workflows := githubYAML(t)

	files := make([]string, 0, 2+len(fragments)+len(workflows))
	files = append(files, "Makefile", ".pre-commit-config.yaml")

	for _, fragment := range fragments {
		files = append(files, ".mk/"+filepath.Base(fragment))
	}

	files = append(files, workflows...)
	used := make([]bool, len(vacuousAllowed))

	for _, rel := range files {
		for _, hit := range vacuousHits(readRepoFile(t, rel)) {
			index := allowedIndex(rel, hit.line)
			if index < 0 {
				t.Errorf("%s:%d: %s: %s", rel, hit.number, hit.flag, strings.TrimSpace(hit.line))

				continue
			}

			used[index] = true
		}
	}

	for index, entry := range vacuousAllowed {
		if !used[index] {
			t.Errorf("the exception %q in %s matches nothing; remove it", entry.text, entry.file)
		}
	}
}

// TestVacuousFlagsAreRecognized is the negative control: each flag, written as a gate would carry
// it, is found, and an exception covers only its own file.
func TestVacuousFlagsAreRecognized(t *testing.T) {
	t.Parallel()

	for _, sample := range []string{
		"\tgo vet ./... || true",
		"\tgo vet ./... ||:",
		"\tgo vet ./... || exit 0",
		"\t-go vet ./...",
		"\t@-go vet ./...",
		".IGNORE: go-test",
		"\t$(MAKE) -i go-test",
		"\t$(MAKE) --ignore-errors go-test",
		"        continue-on-error: true",
		"    if: false",
		"    if: ${{ false }}",
		"    if: 'false'",
		"    if: \"${{ false }}\"",
		"\tcargo test --workspace --locked --no-run",
		"        run: cargo clippy --workspace -- --cap-lints warn",
		"\tcargo clippy --workspace -- -A warnings",
		"\tcargo clippy --workspace -- --allow=warnings",
		"        entry: cargo test --workspace --locked -- --skip slow",
		"\tcargo test --workspace --exclude meta --locked",
		"\t$(CARGO) test --workspace --locked --no-run",
		"        run: cargo clippy --workspace --all-targets --locked",
		"\t$(CARGO) clippy --workspace --all-targets --all-features --locked",
		"        entry: cargo clippy --workspace --all-targets --locked",
	} {
		if len(vacuousHits(sample)) == 0 {
			t.Errorf("no vacuous flag found in %q", sample)
		}
	}

	// A clippy gate that denies warnings, in either spelling, is not vacuous, and neither is the
	// label of a hook or a step.
	for _, sample := range []string{
		"\t$(CARGO) clippy --workspace --all-targets --all-features --locked -- -D warnings",
		"        entry: cargo clippy --workspace --all-targets --locked -- --deny=warnings",
		"        name: cargo clippy",
		"      - name: Lint with clippy",
	} {
		if hits := vacuousHits(sample); len(hits) != 0 {
			t.Errorf("a clippy gate that denies warnings was flagged: %q", sample)
		}
	}

	allowed := `            { grep -E "$RELEASE_TAG_REGEX" || true; } | sort -V | tail -n1`
	if allowedIndex(".github/workflows/release.yaml", allowed) < 0 {
		t.Error("a reviewed exception was not recognized in its own file")
	}

	if allowedIndex(".github/workflows/ci.yaml", allowed) >= 0 {
		t.Error("a reviewed exception was accepted outside its own file")
	}
}

type vacuousHit struct {
	number int
	flag   string
	line   string
}

func vacuousHits(content string) []vacuousHit {
	var hits []vacuousHit

	for index, line := range strings.Split(content, "\n") {
		if clippyCall.MatchString(line) && !clippyDeniesWarnings.MatchString(line) &&
			!yamlName.MatchString(line) {
			hits = append(hits, vacuousHit{
				number: index + 1, flag: "cargo clippy without -D warnings", line: line,
			})
		}

		for _, flag := range vacuousFlags {
			if flag.pattern.MatchString(line) {
				hits = append(hits, vacuousHit{number: index + 1, flag: flag.name, line: line})
			}
		}
	}

	return hits
}

// allowedIndex returns the index of the exception that covers line in file, or -1.
func allowedIndex(file, line string) int {
	for index, entry := range vacuousAllowed {
		if entry.file == file && strings.Contains(line, entry.text) {
			return index
		}
	}

	return -1
}

// TestVacuousHitNumbering pins that a hit names its line, so a failure is actionable.
func TestVacuousHitNumbering(t *testing.T) {
	t.Parallel()

	hits := vacuousHits("all:\n\tgo vet ./...\n\tgo test ./... || true\n")
	if len(hits) != 1 || hits[0].number != 3 {
		t.Errorf("hits = %+v, want one on line 3", hits)
	}
}
