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

// Package suppressions_test drives scripts/check-suppressions.sh, the hook that keeps every
// in-file linter suppression counted against a reviewed baseline.
//
// Each case builds a scratch git repository and runs the script inside it, rather than against
// this repository: what is under test is the rule, not today's counts, and a case has to be able
// to add a suppression without that being a suppression anybody has to review.
package suppressions_test

import (
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/leinardi/swarm-device-access/test/internal/gitenv"
)

// baselineFile is the name the script looks for, relative to the repository it is run in.
const baselineFile = ".suppressions-baseline"

// commandTimeout bounds one git or script invocation; every one of them is local and immediate.
const commandTimeout = 2 * time.Minute

// goWithGosec is a Go file carrying exactly one //nolint:gosec directive.
const goWithGosec = `package scratch

// Run does nothing in particular.
func Run() string {
	return "ok" //nolint:gosec // scratch fixture
}
`

// goWithGosecMoved carries the same single directive on a different line: the count is unchanged,
// which is the case the per-rule counting alone cannot see.
const goWithGosecMoved = `package scratch

//nolint:gosec // scratch fixture, moved
func Run() string {
	return "ok"
}
`

// goWithTwoGosec carries two directives for the same linter.
const goWithTwoGosec = `package scratch

// Run does nothing in particular.
func Run() string {
	value := "ok" //nolint:gosec // scratch fixture
	return value  //nolint:gosec // scratch fixture too
}
`

// goWithDirectiveAfterURL puts a string containing "//" before the directive. golangci-lint reads
// the directive; a detector that stops at the first "//" on the line does not.
const goWithDirectiveAfterURL = `package scratch

// Run does nothing in particular.
func Run() string {
	return "https://example.com/x" //nolint:gosec // scratch fixture behind a URL
}
`

// goWithProseMention mentions a directive inside a sentence. It is not a suppression, and counting
// it as one would make every comment about the ratchet fail the ratchet.
const goWithProseMention = `package scratch

// Run is written so that it needs no //nolint:gosec directive at all.
func Run() string {
	return "ok"
}
`

// dockerfileWithIgnores carries both droast forms: the per-line one and the global one, which
// silences a rule for the whole file and is therefore counted as its own family.
const dockerfileWithIgnores = `FROM scratch

# droast global ignore=DF005 reason="scratch fixture"
# droast ignore=DF052 reason="scratch fixture"
COPY x /x
`

// shellWithDisable carries a shellcheck directive.
const shellWithDisable = `#!/usr/bin/env bash
# shellcheck disable=SC2086
echo $NAMES
`

// goWithSpacedList silences two linters with a space after the comma, which golangci-lint and
// nolintlint both accept; stopping at the space would leave errcheck uncounted.
const goWithSpacedList = `package scratch

// Run does nothing in particular.
func Run() error {
	return nil //nolint:gosec, ErrCheck // scratch fixture
}
`

// goWithBareNolint carries the broadest Go suppression there is: no linter named, so every one of
// them is silenced.
const goWithBareNolint = `package scratch

//nolint // deliberately unspecific
func Run() {}
`

// backquote opens and closes a Go raw string, which a raw string in this file cannot contain.
const backquote = "`"

// goWithRawStringFixture holds directive text inside a raw string, exactly as this file does. None
// of it is a suppression.
const goWithRawStringFixture = "package scratch\n" +
	"\n" +
	"// fixture is Go source held as data.\n" +
	"const fixture = " + backquote + "package other\n" +
	"\n" +
	"func Run() string {\n" +
	"\treturn \"ok\" //nolint:gosec // inside a raw string\n" +
	"}\n" +
	backquote + "\n"

// goWithRawStringFixtureExtended adds one more line to that raw string -- the ordinary way a new
// case gets written, and a line the staged-diff rule must not read as a directive.
const goWithRawStringFixtureExtended = "package scratch\n" +
	"\n" +
	"// fixture is Go source held as data.\n" +
	"const fixture = " + backquote + "package other\n" +
	"\n" +
	"func Run() string {\n" +
	"\tvalue := \"ok\" //nolint:gosec // inside a raw string too\n" +
	"\treturn value //nolint:gosec // inside a raw string\n" +
	"}\n" +
	backquote + "\n"

// goWithRawStringFixtureAndDirective adds a real directive after the raw string closes, which the
// same rule must still catch.
const goWithRawStringFixtureAndDirective = goWithRawStringFixture +
	"\n" +
	"func Run() string {\n" +
	"\treturn \"ok\" //nolint:gosec // a directive in code this time\n" +
	"}\n"

// goWithBlockComment mentions a directive on a continuation line of a block comment. golangci-lint
// honors no directive there, so neither does the ratchet.
const goWithBlockComment = `package scratch

/*
 * Run needs no //nolint:gosec directive, and this line is not one.
 */
func Run() string {
	return "ok"
}
`

// The Rust fixtures are ported from swarm-scheduler-exporter's meta/tests/suppressions.rs at
// d57dcc220d29 (MIT, same author), where the Rust detector comes from.

// rustWithOne is a Rust file carrying exactly one #[expect(clippy::too_many_lines)].
const rustWithOne = `#[expect(clippy::too_many_lines, reason = "scratch fixture")]
fn run() {}
`

// rustWithOneMoved carries the same single attribute on a different line: the count is unchanged.
const rustWithOneMoved = `fn first() {}

#[expect(clippy::too_many_lines, reason = "scratch fixture, moved")]
fn run() {}
`

// rustWithTwo carries two attributes for the same lint.
const rustWithTwo = `#[expect(clippy::too_many_lines, reason = "scratch fixture")]
fn run() {}

#[expect(clippy::too_many_lines, reason = "scratch fixture too")]
fn walk() {}
`

// rustAfterURL puts a string holding a comment opener before the attribute on the same line. A
// detector that stops at the first "//" on the line would miss it.
const rustAfterURL = `const URL: &str = "https://example.com/x"; ` +
	`#[expect(clippy::too_many_lines, reason = "behind a URL")]
fn run() {}
`

// rustProseMention mentions an attribute in a line comment, which is not a suppression.
const rustProseMention = `// run is written so that it needs no #[expect(clippy::too_many_lines)] at all.
fn run() {}
`

// rustBlockComment mentions an attribute inside a block comment, which is not code either.
const rustBlockComment = `/*
 * run needs no #[expect(clippy::too_many_lines)], and this line is not one.
 */
fn run() {}
`

// rustWithCfgAttr wraps the lint attribute in cfg_attr, which the ratchet does not count.
const rustWithCfgAttr = `#[cfg_attr(test, expect(clippy::too_many_lines, reason = "conditional"))]
fn run() {}
`

// rustRawStringFixture holds attribute text in a raw string spanning lines. None of it counts.
const rustRawStringFixture = `const FIXTURE: &str = r#"
#[expect(clippy::too_many_lines, reason = "inside a raw string")]
fn run() {}
"#;
`

// rustRawStringFixtureExtended adds one more line inside that raw string.
const rustRawStringFixtureExtended = `const FIXTURE: &str = r#"
#[expect(clippy::cast_possible_truncation, reason = "inside a raw string too")]
#[expect(clippy::too_many_lines, reason = "inside a raw string")]
fn run() {}
"#;
`

// rustRawStringFixtureAndAttribute adds a real attribute after the raw string closes.
const rustRawStringFixtureAndAttribute = rustRawStringFixture + `
#[expect(clippy::too_many_lines, reason = "in code this time")]
fn real() {}
`

// rustWithAttributes carries every Rust lint-attribute form the ratchet counts: an inner and an
// outer expect, an allow, and a multi-line expect naming two clippy lints the way rustfmt wraps a
// long attribute. The reason item is not a lint, commas and parentheses inside it included.
const rustWithAttributes = `#![expect(clippy::pedantic, reason = "scratch fixture, (inner)")]

#[expect(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    reason = "scratch fixture"
)]
fn long() {}

#[allow(dead_code)]
fn unused() {}

#![allow(missing_docs)]
`

// rustWithAttributeText holds lint attributes only as data: in a raw string spanning lines, in a
// string, in a line comment and in a nested block comment, around character literals holding a
// quote. None of it is a suppression.
const rustWithAttributeText = `const FIXTURE: &str = r#"
#[allow(clippy::unwrap_used)]
"#;
const TEXT: &str = "#[expect(dead_code)]";
// #[allow(dead_code)]
/* #[allow(dead_code)] /* nested */ #[allow(dead_code)] */
fn quote<'a>(text: &'a str) -> char { let _quote = '"'; '#' }
`

// rustWithMultiLineExpect is a multi-line expect naming one lint.
const rustWithMultiLineExpect = `#[expect(
    clippy::too_many_lines,
    reason = "scratch fixture"
)]
fn long() {}
`

// rustWithMultiLineExpectExtended adds a second lint on its own line inside the same attribute: the
// opening line is untouched, so only the line naming the lint shows the addition.
const rustWithMultiLineExpectExtended = `#[expect(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    reason = "scratch fixture"
)]
fn long() {}
`

func TestCheckSuppressions(t *testing.T) {
	t.Parallel()

	cases := []struct {
		name string
		// setUp prepares a scratch repository and leaves the index in the state under test.
		setUp func(t *testing.T, dir string)
		// wantFailure is whether the script is expected to refuse the state.
		wantFailure bool
		// wantOutput is a fragment the script must name, so a case cannot pass on the wrong
		// complaint.
		wantOutput string
	}{
		{
			name: "a tree that matches its baseline passes",
			setUp: func(t *testing.T, dir string) {
				t.Helper()

				commitInitial(t, dir, goWithGosec, "nolint/gosec\t1")
			},
		},
		{
			name: "an added suppression with an untouched baseline is refused",
			setUp: func(t *testing.T, dir string) {
				t.Helper()

				commitInitial(t, dir, goWithGosec, "nolint/gosec\t1")
				writeFile(t, dir, "scratch.go", goWithTwoGosec)
				git(t, dir, "add", "scratch.go")
			},
			wantFailure: true,
			wantOutput:  "without touching " + baselineFile,
		},
		{
			name: "the same addition passes once the baseline is staged with it",
			setUp: func(t *testing.T, dir string) {
				t.Helper()

				commitInitial(t, dir, goWithGosec, "nolint/gosec\t1")
				writeFile(t, dir, "scratch.go", goWithTwoGosec)
				writeBaseline(t, dir, "nolint/gosec\t2")
				git(t, dir, "add", "scratch.go", baselineFile)
			},
		},
		{
			name: "a suppression swapped one for one is refused even though the count holds",
			setUp: func(t *testing.T, dir string) {
				t.Helper()

				commitInitial(t, dir, goWithGosec, "nolint/gosec\t1")
				writeFile(t, dir, "scratch.go", goWithGosecMoved)
				git(t, dir, "add", "scratch.go")
			},
			wantFailure: true,
			wantOutput:  "without touching " + baselineFile,
		},
		{
			name: "a count above the baseline is refused with nothing staged",
			setUp: func(t *testing.T, dir string) {
				t.Helper()

				commitInitial(t, dir, goWithTwoGosec, "nolint/gosec\t1")
			},
			wantFailure: true,
			wantOutput:  "baseline allows 1",
		},
		{
			name: "a count below the baseline is refused too",
			setUp: func(t *testing.T, dir string) {
				t.Helper()

				commitInitial(t, dir, goWithGosec, "nolint/gosec\t3")
			},
			wantFailure: true,
			wantOutput:  "baseline still claims 3",
		},
		{
			name: "a rule the baseline has never heard of is refused",
			setUp: func(t *testing.T, dir string) {
				t.Helper()

				commitInitial(t, dir, goWithGosec, "nolint/dupl\t1")
			},
			wantFailure: true,
			wantOutput:  "does not list this rule",
		},
		{
			name: "a directive behind a string containing a comment opener still counts",
			setUp: func(t *testing.T, dir string) {
				t.Helper()

				commitInitial(t, dir, goWithDirectiveAfterURL, "nolint/dupl\t1")
			},
			wantFailure: true,
			wantOutput:  "nolint/gosec",
		},
		{
			name: "a directive mentioned in prose is not a suppression",
			setUp: func(t *testing.T, dir string) {
				t.Helper()

				commitInitial(t, dir, goWithProseMention, "")
			},
		},
		{
			name: "a directive inside a block comment is not a suppression either",
			setUp: func(t *testing.T, dir string) {
				t.Helper()

				commitInitial(t, dir, goWithBlockComment, "")
			},
		},
		{
			name: "a deleted baseline is refused rather than treated as no suppressions",
			setUp: func(t *testing.T, dir string) {
				t.Helper()

				commitInitial(t, dir, goWithGosec, "nolint/gosec\t1")
				git(t, dir, "rm", "-q", baselineFile)
			},
			wantFailure: true,
			wantOutput:  "is missing",
		},
		{
			name: "a deleted baseline is refused in a tree with no suppressions at all",
			setUp: func(t *testing.T, dir string) {
				t.Helper()

				commitInitial(t, dir, goWithProseMention, "")
				git(t, dir, "rm", "-q", baselineFile)
			},
			wantFailure: true,
			wantOutput:  "is missing",
		},
	}

	for _, testCase := range cases {
		t.Run(testCase.name, func(t *testing.T) {
			t.Parallel()

			dir := t.TempDir()
			initRepo(t, dir)
			testCase.setUp(t, dir)

			output, err := runRatchet(t, dir)

			if testCase.wantFailure && err == nil {
				t.Fatalf("the ratchet accepted the tree, want a refusal:\n%s", output)
			}

			if !testCase.wantFailure && err != nil {
				t.Fatalf("the ratchet refused a tree it should accept: %v\n%s", err, output)
			}

			if testCase.wantOutput != "" && !strings.Contains(output, testCase.wantOutput) {
				t.Errorf("output does not mention %q:\n%s", testCase.wantOutput, output)
			}
		})
	}
}

// TestCheckSuppressionsUpdate pins that --update writes a baseline the checker then accepts: the
// script's own instruction for a refusal is to run it, so a broken --update would leave no way out.
func TestCheckSuppressionsUpdate(t *testing.T) {
	t.Parallel()

	dir := t.TempDir()
	initRepo(t, dir)
	commitInitial(t, dir, goWithTwoGosec, "nolint/gosec\t1")

	output, err := runRatchet(t, dir)
	if err == nil {
		t.Fatalf("the ratchet accepted a count above its baseline:\n%s", output)
	}

	output, err = runRatchet(t, dir, "--update")
	if err != nil {
		t.Fatalf("--update failed: %v\n%s", err, output)
	}

	written := readFile(t, filepath.Join(dir, baselineFile))
	if !strings.Contains(written, "nolint/gosec\t2") {
		t.Errorf("rewritten baseline does not record the two directives:\n%s", written)
	}

	output, err = runRatchet(t, dir)
	if err != nil {
		t.Fatalf("the ratchet still refuses the tree after --update: %v\n%s", err, output)
	}
}

// TestCheckSuppressionsCountsEveryFamily pins the detector across the Go, shell and Dockerfile
// mechanisms at once (TestCheckSuppressionsCountsEveryRustForm covers Rust's two): the
// baseline is keyed per rule, so a family that resolves to the wrong key -- or to no key -- takes
// its suppressions out of the ratchet without anything else noticing.
func TestCheckSuppressionsCountsEveryFamily(t *testing.T) {
	t.Parallel()

	dir := t.TempDir()
	initRepo(t, dir)

	writeFile(t, dir, "Dockerfile", dockerfileWithIgnores)
	writeFile(t, dir, "run.sh", shellWithDisable)
	writeFile(t, dir, "bare.go", goWithBareNolint)
	writeFile(t, dir, "spaced.go", goWithSpacedList)
	writeBaseline(t, dir, "")
	git(t, dir, "add", "-A")
	git(t, dir, "commit", "-q", "-m", "one file per family")

	output, err := runRatchet(t, dir, "--update")
	if err != nil {
		t.Fatalf("--update failed: %v\n%s", err, output)
	}

	written := readFile(t, filepath.Join(dir, baselineFile))

	// Each linter of a two-linter nolint list counts separately, the global droast form is its
	// own family, and a bare //nolint is recorded as the everything-suppression it is.
	want := []string{
		"droast-global-ignore/DF005\t1",
		"droast-ignore/DF052\t1",
		"shellcheck-disable/SC2086\t1",
		"nolint/*\t1",
		"nolint/gosec\t1",
		"nolint/errcheck\t1",
	}

	for _, line := range want {
		if !strings.Contains(written, line) {
			t.Errorf("baseline does not record %q:\n%s", line, written)
		}
	}
}

// TestCheckSuppressionsReadsTheStagedFileNotTheHunk pins that the staged-diff rule sees whole
// files. The detector carries raw-string and block-comment state across lines, so judging added
// lines on their own would both invent suppressions inside a fixture and miss real ones after a
// closing backquote -- and an invented one cannot be resolved, because the whole-tree count agrees
// with the baseline and --update would rewrite it unchanged.
func TestCheckSuppressionsReadsTheStagedFileNotTheHunk(t *testing.T) {
	t.Parallel()

	cases := []struct {
		name   string
		edited string
		// wantOutput is empty when the ratchet must accept the edit, and otherwise the message
		// that names which rule refused it.
		wantOutput string
	}{
		{
			name:   "a line added inside a raw string is not a suppression",
			edited: goWithRawStringFixtureExtended,
		},
		{
			name:   "a real directive added after the raw string still is",
			edited: goWithRawStringFixtureAndDirective,
			// The staged-diff rule's own message: the count rule refuses this edit too, and
			// asserting on the refusal alone would let a detector that found nothing pass.
			wantOutput: "without touching " + baselineFile,
		},
	}

	for _, testCase := range cases {
		t.Run(testCase.name, func(t *testing.T) {
			t.Parallel()

			dir := t.TempDir()
			initRepo(t, dir)
			commitInitial(t, dir, goWithRawStringFixture, "")

			writeFile(t, dir, "scratch.go", testCase.edited)
			git(t, dir, "add", "scratch.go")

			output, err := runRatchet(t, dir)

			if testCase.wantOutput == "" {
				if err != nil {
					t.Fatalf("the ratchet read a raw-string line as a directive: %v\n%s",
						err, output)
				}

				return
			}

			if err == nil {
				t.Fatalf("the ratchet accepted an added directive:\n%s", output)
			}

			if !strings.Contains(output, testCase.wantOutput) {
				t.Errorf("output does not mention %q, so the staged-diff rule did not see the "+
					"directive:\n%s", testCase.wantOutput, output)
			}
		})
	}
}

// TestCheckSuppressionsUpdateDischargesASwap pins the way out of the one refusal the counts cannot
// explain. A swap leaves every count where it was, so --update has to produce a baseline that
// differs anyway -- otherwise there is nothing to stage, the hook refuses again, and the only
// remaining exits are a cosmetic edit or skipping the hook.
func TestCheckSuppressionsUpdateDischargesASwap(t *testing.T) {
	t.Parallel()

	dir := t.TempDir()
	initRepo(t, dir)

	writeFile(t, dir, "scratch.go", goWithGosec)
	writeBaseline(t, dir, "")
	git(t, dir, "add", "-A")
	git(t, dir, "commit", "-q", "-m", "scratch fixture")

	// A real baseline, written by the script itself and committed, is the starting point: the
	// question is whether a second --update against unchanged counts still moves the file.
	output, err := runRatchet(t, dir, "--update")
	if err != nil {
		t.Fatalf("--update failed: %v\n%s", err, output)
	}

	git(t, dir, "add", baselineFile)
	git(t, dir, "commit", "-q", "-m", "record the baseline")

	before := readFile(t, filepath.Join(dir, baselineFile))

	writeFile(t, dir, "scratch.go", goWithGosecMoved)
	git(t, dir, "add", "scratch.go")

	output, err = runRatchet(t, dir)
	if err == nil {
		t.Fatalf("the ratchet accepted a swapped suppression:\n%s", output)
	}

	output, err = runRatchet(t, dir, "--update")
	if err != nil {
		t.Fatalf("--update failed: %v\n%s", err, output)
	}

	after := readFile(t, filepath.Join(dir, baselineFile))
	if after == before {
		t.Fatalf("--update left the baseline byte-identical, so there is nothing to stage:\n%s",
			after)
	}

	git(t, dir, "add", baselineFile)

	output, err = runRatchet(t, dir)
	if err != nil {
		t.Fatalf("the ratchet still refuses the swap after the baseline was staged: %v\n%s",
			err, output)
	}
}

// TestCheckSuppressionsAllowsARename pins that moving a file is not adding its suppressions. The
// staged-diff rule works from the new name, so without rename detection a moved file would look
// wholly new and every directive it already carried would be reported as freshly added -- with
// counts unchanged, and therefore with nothing the baseline could say about it.
func TestCheckSuppressionsAllowsARename(t *testing.T) {
	t.Parallel()

	dir := t.TempDir()
	initRepo(t, dir)

	writeFile(t, dir, "old.go", goWithSpacedList)
	writeFile(t, dir, "Dockerfile", dockerfileWithIgnores)
	writeBaseline(t, dir, "droast-global-ignore/DF005\t1\n"+
		"droast-ignore/DF052\t1\n"+
		"nolint/errcheck\t1\n"+
		"nolint/gosec\t1")
	git(t, dir, "add", "-A")
	git(t, dir, "commit", "-q", "-m", "files with suppressions")

	git(t, dir, "mv", "old.go", "new.go")
	git(t, dir, "mv", "Dockerfile", "build.Dockerfile")

	output, err := runRatchet(t, dir)
	if err != nil {
		t.Fatalf("the ratchet read a moved file as newly added suppressions: %v\n%s", err, output)
	}
}

// TestCheckSuppressionsRust runs the count and staged-diff rules on Rust's #[expect] and #[allow],
// and pins what the Rust detector must not read as a suppression.
func TestCheckSuppressionsRust(t *testing.T) {
	t.Parallel()

	const rustScratch = "scratch.rs"

	const tooManyLines = "expect/clippy::too_many_lines"

	cases := []struct {
		name        string
		setUp       func(t *testing.T, dir string)
		wantFailure bool
		wantOutput  string
	}{
		{
			name: "a tree that matches its baseline passes",
			setUp: func(t *testing.T, dir string) {
				t.Helper()

				commitInitialAs(t, dir, rustScratch, rustWithOne, tooManyLines+"\t1")
			},
		},
		{
			name: "an added attribute with an untouched baseline is refused",
			setUp: func(t *testing.T, dir string) {
				t.Helper()

				commitInitialAs(t, dir, rustScratch, rustWithOne, tooManyLines+"\t1")
				writeFile(t, dir, rustScratch, rustWithTwo)
				git(t, dir, "add", rustScratch)
			},
			wantFailure: true,
			wantOutput:  "without touching " + baselineFile,
		},
		{
			name: "the same addition passes once the baseline is staged with it",
			setUp: func(t *testing.T, dir string) {
				t.Helper()

				commitInitialAs(t, dir, rustScratch, rustWithOne, tooManyLines+"\t1")
				writeFile(t, dir, rustScratch, rustWithTwo)
				writeBaseline(t, dir, tooManyLines+"\t2")
				git(t, dir, "add", rustScratch, baselineFile)
			},
		},
		{
			name: "an attribute swapped one for one is refused even though the count holds",
			setUp: func(t *testing.T, dir string) {
				t.Helper()

				commitInitialAs(t, dir, rustScratch, rustWithOne, tooManyLines+"\t1")
				writeFile(t, dir, rustScratch, rustWithOneMoved)
				git(t, dir, "add", rustScratch)
			},
			wantFailure: true,
			wantOutput:  "without touching " + baselineFile,
		},
		{
			name: "a count above the baseline is refused",
			setUp: func(t *testing.T, dir string) {
				t.Helper()

				commitInitialAs(t, dir, rustScratch, rustWithTwo, tooManyLines+"\t1")
			},
			wantFailure: true,
			wantOutput:  "baseline allows 1",
		},
		{
			name: "a count below the baseline is refused",
			setUp: func(t *testing.T, dir string) {
				t.Helper()

				commitInitialAs(t, dir, rustScratch, rustWithOne, tooManyLines+"\t3")
			},
			wantFailure: true,
			wantOutput:  "baseline still claims 3",
		},
		{
			name: "a lint the baseline has never heard of is refused",
			setUp: func(t *testing.T, dir string) {
				t.Helper()

				commitInitialAs(t, dir, rustScratch, rustWithOne, "expect/dead_code\t1")
			},
			wantFailure: true,
			wantOutput:  "does not list this rule",
		},
		{
			name: "an attribute behind a string containing a comment opener still counts",
			setUp: func(t *testing.T, dir string) {
				t.Helper()

				commitInitialAs(t, dir, rustScratch, rustAfterURL, "")
			},
			wantFailure: true,
			wantOutput:  tooManyLines,
		},
		{
			name: "an attribute mentioned in a line comment is not a suppression",
			setUp: func(t *testing.T, dir string) {
				t.Helper()

				commitInitialAs(t, dir, rustScratch, rustProseMention, "")
			},
		},
		{
			name: "an attribute inside a block comment is not a suppression either",
			setUp: func(t *testing.T, dir string) {
				t.Helper()

				commitInitialAs(t, dir, rustScratch, rustBlockComment, "")
			},
		},
		{
			name: "a cfg_attr-wrapped attribute is not counted",
			setUp: func(t *testing.T, dir string) {
				t.Helper()

				commitInitialAs(t, dir, rustScratch, rustWithCfgAttr, "")
			},
		},
	}

	for _, testCase := range cases {
		t.Run(testCase.name, func(t *testing.T) {
			t.Parallel()

			dir := t.TempDir()
			initRepo(t, dir)
			testCase.setUp(t, dir)

			output, err := runRatchet(t, dir)

			if testCase.wantFailure && err == nil {
				t.Fatalf("the ratchet accepted the tree, want a refusal:\n%s", output)
			}

			if !testCase.wantFailure && err != nil {
				t.Fatalf("the ratchet refused a tree it should accept: %v\n%s", err, output)
			}

			if testCase.wantOutput != "" && !strings.Contains(output, testCase.wantOutput) {
				t.Errorf("output does not mention %q:\n%s", testCase.wantOutput, output)
			}
		})
	}
}

// TestCheckSuppressionsCountsEveryRustForm pins the Rust detector's keys: each lint of a
// multi-lint attribute counts separately, inner and outer forms both count, and attribute text
// held as data -- strings, raw strings, comments, nested block comments -- counts as nothing.
func TestCheckSuppressionsCountsEveryRustForm(t *testing.T) {
	t.Parallel()

	dir := t.TempDir()
	initRepo(t, dir)

	writeFile(t, dir, "src/attributes.rs", rustWithAttributes)
	writeFile(t, dir, "src/text.rs", rustWithAttributeText)
	writeBaseline(t, dir, "")
	git(t, dir, "add", "-A")
	git(t, dir, "commit", "-q", "-m", "every rust form")

	output, err := runRatchet(t, dir, "--update")
	if err != nil {
		t.Fatalf("--update failed: %v\n%s", err, output)
	}

	written := readFile(t, filepath.Join(dir, baselineFile))

	for _, line := range []string{
		"expect/clippy::pedantic\t1",
		"expect/clippy::too_many_lines\t1",
		"expect/clippy::cast_possible_truncation\t1",
		"allow/dead_code\t1",
		"allow/missing_docs\t1",
	} {
		if !strings.Contains(written, line) {
			t.Errorf("baseline does not record %q:\n%s", line, written)
		}
	}

	// The text fixture names clippy::unwrap_used and expect(dead_code) only as data; the one real
	// allow(dead_code) is in the attributes fixture.
	for _, unwanted := range []string{"allow/clippy::unwrap_used", "expect/dead_code", "reason"} {
		if strings.Contains(written, unwanted) {
			t.Errorf("baseline records %q, which is not a suppression:\n%s", unwanted, written)
		}
	}
}

// TestCheckSuppressionsSeesALintAddedToAMultiLineExpect pins that a lint is located on the line
// that names it, not on the line that opens the attribute: rustfmt puts each lint of a long
// attribute on its own line, and adding one there leaves the opening line untouched.
func TestCheckSuppressionsSeesALintAddedToAMultiLineExpect(t *testing.T) {
	t.Parallel()

	dir := t.TempDir()
	initRepo(t, dir)
	commitInitialAs(
		t,
		dir,
		"scratch.rs",
		rustWithMultiLineExpect,
		"expect/clippy::too_many_lines\t1",
	)

	writeFile(t, dir, "scratch.rs", rustWithMultiLineExpectExtended)
	git(t, dir, "add", "scratch.rs")

	output, err := runRatchet(t, dir)
	if err == nil {
		t.Fatalf("the ratchet accepted a lint added to an existing expect:\n%s", output)
	}

	if !strings.Contains(output, "scratch.rs:3\texpect/clippy::cast_possible_truncation") {
		t.Errorf("the output does not locate the added lint on its own line:\n%s", output)
	}
}

// TestCheckSuppressionsSkipsRustFixtures pins that a Rust file under a testdata/ directory is data:
// neither counted nor treated as newly added.
func TestCheckSuppressionsSkipsRustFixtures(t *testing.T) {
	t.Parallel()

	dir := t.TempDir()
	initRepo(t, dir)
	commitInitial(t, dir, goWithProseMention, "")

	writeFile(t, dir, "tests/testdata/fixture.rs", rustWithOne)
	writeFile(t, dir, "testdata/top.rs", rustWithOne)
	git(t, dir, "add", "-A")

	output, err := runRatchet(t, dir)
	if err != nil {
		t.Fatalf("the ratchet counted a Rust fixture: %v\n%s", err, output)
	}
}

// TestCheckSuppressionsCountsNeighborsOfRustFixtures is the negative control of the test above: a
// Rust file next to a testdata/ directory but outside it is counted, and the exclusion covers Rust
// only, so a Go file under testdata/ is counted exactly as it was before Rust was added.
func TestCheckSuppressionsCountsNeighborsOfRustFixtures(t *testing.T) {
	t.Parallel()

	for _, testCase := range []struct {
		name, content, key string
	}{
		{"tests/testdatax/x.rs", rustWithOne, "expect/clippy::too_many_lines"},
		{"tests/testdata.rs", rustWithOne, "expect/clippy::too_many_lines"},
		{"internal/x/testdata/x.go", goWithGosec, "nolint/gosec"},
	} {
		t.Run(testCase.name, func(t *testing.T) {
			t.Parallel()

			dir := t.TempDir()
			initRepo(t, dir)
			commitInitial(t, dir, goWithProseMention, "")

			writeFile(t, dir, testCase.name, testCase.content)
			git(t, dir, "add", "-A")

			output, err := runRatchet(t, dir)
			if err == nil || !strings.Contains(output, testCase.key) {
				t.Fatalf("the ratchet did not count %s: %v\n%s", testCase.name, err, output)
			}
		})
	}
}

// TestCheckSuppressionsReadsTheStagedRustFileNotTheHunk is the Rust side of
// TestCheckSuppressionsReadsTheStagedFileNotTheHunk: the raw-string state carries across lines.
func TestCheckSuppressionsReadsTheStagedRustFileNotTheHunk(t *testing.T) {
	t.Parallel()

	for _, testCase := range []struct {
		name, edited, wantOutput string
	}{
		{"a line added inside a raw string is not a suppression", rustRawStringFixtureExtended, ""},
		{
			"a real attribute added after the raw string still is", rustRawStringFixtureAndAttribute,
			"without touching " + baselineFile,
		},
	} {
		t.Run(testCase.name, func(t *testing.T) {
			t.Parallel()

			dir := t.TempDir()
			initRepo(t, dir)
			commitInitialAs(t, dir, "scratch.rs", rustRawStringFixture, "")

			writeFile(t, dir, "scratch.rs", testCase.edited)
			git(t, dir, "add", "scratch.rs")

			output, err := runRatchet(t, dir)

			if testCase.wantOutput == "" {
				if err != nil {
					t.Fatalf("the ratchet read a raw-string line as an attribute: %v\n%s",
						err, output)
				}

				return
			}

			if err == nil || !strings.Contains(output, testCase.wantOutput) {
				t.Fatalf("the staged-diff rule did not see the added attribute: %v\n%s",
					err, output)
			}
		})
	}
}

// TestCheckSuppressionsAllowsARustRename pins that moving a Rust file is not adding its attributes.
func TestCheckSuppressionsAllowsARustRename(t *testing.T) {
	t.Parallel()

	dir := t.TempDir()
	initRepo(t, dir)

	writeFile(t, dir, "src/old.rs", rustWithAttributes)
	writeBaseline(t, dir, "allow/dead_code\t1\n"+
		"allow/missing_docs\t1\n"+
		"expect/clippy::cast_possible_truncation\t1\n"+
		"expect/clippy::pedantic\t1\n"+
		"expect/clippy::too_many_lines\t1")
	git(t, dir, "add", "-A")
	git(t, dir, "commit", "-q", "-m", "a file with attributes")

	git(t, dir, "mv", "src/old.rs", "src/new.rs")

	output, err := runRatchet(t, dir)
	if err != nil {
		t.Fatalf("the ratchet read a moved file as newly added attributes: %v\n%s", err, output)
	}
}

// initRepo creates an empty git repository with an identity, so committing needs no global config.
func initRepo(t *testing.T, dir string) {
	t.Helper()

	git(t, dir, "init", "-q", "-b", "main")
	git(t, dir, "config", "user.email", "ratchet@example.invalid")
	git(t, dir, "config", "user.name", "Ratchet Test")
	git(t, dir, "config", "commit.gpgsign", "false")
}

// commitInitial writes one Go file and a baseline, and commits both, leaving a clean index.
func commitInitial(t *testing.T, dir, source, baseline string) {
	t.Helper()

	commitInitialAs(t, dir, "scratch.go", source, baseline)
}

// commitInitialAs writes one source file under the given name and a baseline, and commits both.
func commitInitialAs(t *testing.T, dir, name, source, baseline string) {
	t.Helper()

	writeFile(t, dir, name, source)
	writeBaseline(t, dir, baseline)
	git(t, dir, "add", "-A")
	git(t, dir, "commit", "-q", "-m", "scratch fixture")
}

// writeBaseline writes a baseline file holding the given lines, plus the comment a real one has.
func writeBaseline(t *testing.T, dir, lines string) {
	t.Helper()

	content := "# scratch baseline\n"
	if lines != "" {
		content += lines + "\n"
	}

	writeFile(t, dir, baselineFile, content)
}

// writeFile writes one file inside the scratch repository, creating the directories above it.
func writeFile(t *testing.T, dir, name, content string) {
	t.Helper()

	path := filepath.Join(dir, name)

	err := os.MkdirAll(filepath.Dir(path), 0o750)
	if err != nil {
		t.Fatalf("create the directory for %s: %v", name, err)
	}

	err = os.WriteFile(path, []byte(content), 0o600)
	if err != nil {
		t.Fatalf("write %s: %v", name, err)
	}
}

// readFile reads one file back.
func readFile(t *testing.T, path string) string {
	t.Helper()

	content, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read %s: %v", path, err)
	}

	return string(content)
}

// git runs one git command inside the scratch repository and fails the test if it does not succeed.
func git(t *testing.T, dir string, args ...string) {
	t.Helper()

	ctx, cancel := context.WithTimeout(t.Context(), commandTimeout)
	defer cancel()

	cmd := exec.CommandContext(ctx, "git", args...)
	cmd.Dir = dir
	cmd.Env = gitenv.Isolated()

	output, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("git %s: %v\n%s", strings.Join(args, " "), err, output)
	}
}

// runRatchet runs the script inside the scratch repository and returns its combined output.
func runRatchet(t *testing.T, dir string, args ...string) (string, error) {
	t.Helper()

	script, err := filepath.Abs(filepath.Join("..", "..", "scripts", "check-suppressions.sh"))
	if err != nil {
		t.Fatalf("resolve the script path: %v", err)
	}

	// Reading the script makes it part of the test cache key; a program run only through exec is
	// not, and an edited script would otherwise replay a cached pass.
	_, err = os.ReadFile(script)
	if err != nil {
		t.Fatalf("read the script: %v", err)
	}

	ctx, cancel := context.WithTimeout(t.Context(), commandTimeout)
	defer cancel()

	cmd := exec.CommandContext(ctx, script, args...)
	cmd.Dir = dir
	cmd.Env = gitenv.Isolated()

	output, runErr := cmd.CombinedOutput()

	return string(output), runErr
}
