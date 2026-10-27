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
// Adapted from swarm-scheduler-exporter/meta/tests/suppressions.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! Drives scripts/check-suppressions.sh, the hook that keeps every in-file linter suppression
//! counted against a reviewed baseline. The Rust side of `test/suppressions`, which stays until
//! the cutover: its cases, the Go, shell and Dockerfile families as well as Rust's, since the
//! script counts all of them for as long as the Go code lives here.
//!
//! Each case builds a scratch git repository and runs the script inside it, rather than against
//! this repository: what is under test is the rule, not today's counts, and a case has to be able
//! to add a suppression without that being a suppression anybody has to review. git and the
//! script run with an isolated git environment (`meta::gitenv`). Every refusal case is the
//! negative control of the accepting case beside it, and names the complaint it expects, so a
//! case cannot pass on the wrong one.

use std::fs;
use std::path::Path;
use std::process::Command;

use meta::{gitenv, repo};

/// The name the script looks for, relative to the repository it is run in.
const BASELINE_FILE: &str = ".suppressions-baseline";

/// The refusal of the staged-diff rule.
const UNTOUCHED_BASELINE: &str = "without touching .suppressions-baseline";

/// The scratch Go file the Go cases edit.
const GO_SCRATCH: &str = "scratch.go";

/// The scratch Rust file the Rust cases edit.
const RUST_SCRATCH: &str = "scratch.rs";

/// The baseline key of the Rust fixtures' one lint.
const TOO_MANY_LINES: &str = "expect/clippy::too_many_lines";

/// A Go file carrying exactly one `//nolint:gosec` directive.
const GO_WITH_GOSEC: &str = r#"package scratch

// Run does nothing in particular.
func Run() string {
    return "ok" //nolint:gosec // scratch fixture
}
"#;

/// The same single directive on a different line: the count is unchanged, which is the case the
/// per-rule counting alone cannot see.
const GO_WITH_GOSEC_MOVED: &str = r#"package scratch

//nolint:gosec // scratch fixture, moved
func Run() string {
    return "ok"
}
"#;

/// Two directives for the same linter.
const GO_WITH_TWO_GOSEC: &str = r#"package scratch

// Run does nothing in particular.
func Run() string {
    value := "ok" //nolint:gosec // scratch fixture
    return value  //nolint:gosec // scratch fixture too
}
"#;

/// A string containing "//" before the directive. golangci-lint reads the directive; a detector
/// that stops at the first "//" on the line does not.
const GO_WITH_DIRECTIVE_AFTER_URL: &str = r#"package scratch

// Run does nothing in particular.
func Run() string {
    return "https://example.com/x" //nolint:gosec // scratch fixture behind a URL
}
"#;

/// A directive mentioned inside a sentence. It is not a suppression, and counting it as one would
/// make every comment about the ratchet fail the ratchet.
const GO_WITH_PROSE_MENTION: &str = r#"package scratch

// Run is written so that it needs no //nolint:gosec directive at all.
func Run() string {
    return "ok"
}
"#;

/// Both droast forms: the per-line one and the global one, which silences a rule for the whole
/// file and is therefore counted as its own family.
const DOCKERFILE_WITH_IGNORES: &str = r#"FROM scratch

# droast global ignore=DF005 reason="scratch fixture"
# droast ignore=DF052 reason="scratch fixture"
COPY x /x
"#;

/// A shellcheck directive.
const SHELL_WITH_DISABLE: &str = "#!/usr/bin/env bash
# shellcheck disable=SC2086
echo $NAMES
";

/// Two linters silenced with a space after the comma, which golangci-lint and nolintlint both
/// accept; stopping at the space would leave errcheck uncounted.
const GO_WITH_SPACED_LIST: &str = r"package scratch

// Run does nothing in particular.
func Run() error {
    return nil //nolint:gosec, ErrCheck // scratch fixture
}
";

/// The broadest Go suppression there is: no linter named, so every one of them is silenced.
const GO_WITH_BARE_NOLINT: &str = r"package scratch

//nolint // deliberately unspecific
func Run() {}
";

/// Directive text inside a Go raw string. None of it is a suppression.
const GO_WITH_RAW_STRING_FIXTURE: &str = r#"package scratch

// fixture is Go source held as data.
const fixture = `package other

func Run() string {
    return "ok" //nolint:gosec // inside a raw string
}
`
"#;

/// One more line added inside that raw string -- the ordinary way a new case gets written, and a
/// line the staged-diff rule must not read as a directive.
const GO_WITH_RAW_STRING_FIXTURE_EXTENDED: &str = r#"package scratch

// fixture is Go source held as data.
const fixture = `package other

func Run() string {
    value := "ok" //nolint:gosec // inside a raw string too
    return value //nolint:gosec // inside a raw string
}
`
"#;

/// A real directive added after the raw string closes, which the same rule must still catch.
const GO_WITH_RAW_STRING_FIXTURE_AND_DIRECTIVE: &str = r#"package scratch

// fixture is Go source held as data.
const fixture = `package other

func Run() string {
    return "ok" //nolint:gosec // inside a raw string
}
`

func Run() string {
    return "ok" //nolint:gosec // a directive in code this time
}
"#;

/// A directive mentioned on a continuation line of a block comment. golangci-lint honors no
/// directive there, so neither does the ratchet.
const GO_WITH_BLOCK_COMMENT: &str = r#"package scratch

/*
 * Run needs no //nolint:gosec directive, and this line is not one.
 */
func Run() string {
    return "ok"
}
"#;

/// One `#[expect(clippy::too_many_lines)]`.
const RUST_WITH_ONE: &str = r#"#[expect(clippy::too_many_lines, reason = "scratch fixture")]
fn run() {}
"#;

/// The same single attribute on a different line: the count is unchanged.
const RUST_WITH_ONE_MOVED: &str = r#"fn first() {}

#[expect(clippy::too_many_lines, reason = "scratch fixture, moved")]
fn run() {}
"#;

/// Two attributes for the same lint.
const RUST_WITH_TWO: &str = r#"#[expect(clippy::too_many_lines, reason = "scratch fixture")]
fn run() {}

#[expect(clippy::too_many_lines, reason = "scratch fixture too")]
fn walk() {}
"#;

/// An attribute after a string holding a comment opener on the same line. A detector that stops
/// at the first "//" on the line would miss it.
const RUST_AFTER_URL: &str = r#"const URL: &str = "https://example.com/x"; #[expect(clippy::too_many_lines, reason = "behind a URL")]
fn run() {}
"#;

/// An attribute mentioned in a line comment, which is not a suppression.
const RUST_PROSE_MENTION: &str =
    "// run is written so that it needs no #[expect(clippy::too_many_lines)] at all.
fn run() {}
";

/// An attribute mentioned inside a block comment, which is not code either.
const RUST_BLOCK_COMMENT: &str = "/*
 * run needs no #[expect(clippy::too_many_lines)], and this line is not one.
 */
fn run() {}
";

/// A lint attribute wrapped in `cfg_attr`, which the ratchet does not count.
const RUST_WITH_CFG_ATTR: &str = r#"#[cfg_attr(test, expect(clippy::too_many_lines, reason = "conditional"))]
fn run() {}
"#;

/// Attribute text held in a raw string spanning lines. None of it is a suppression.
const RUST_RAW_STRING_FIXTURE: &str = r##"const FIXTURE: &str = r#"
#[expect(clippy::too_many_lines, reason = "inside a raw string")]
fn run() {}
"#;
"##;

/// One more line added inside that raw string, which the staged-diff rule must not read as an
/// attribute.
const RUST_RAW_STRING_FIXTURE_EXTENDED: &str = r##"const FIXTURE: &str = r#"
#[expect(clippy::cast_possible_truncation, reason = "inside a raw string too")]
#[expect(clippy::too_many_lines, reason = "inside a raw string")]
fn run() {}
"#;
"##;

/// A real attribute added after the raw string closes, which the same rule must still catch.
const RUST_RAW_STRING_FIXTURE_AND_ATTRIBUTE: &str = r##"const FIXTURE: &str = r#"
#[expect(clippy::too_many_lines, reason = "inside a raw string")]
fn run() {}
"#;

#[expect(clippy::too_many_lines, reason = "in code this time")]
fn real() {}
"##;

/// Every Rust lint-attribute form the ratchet counts: an inner and an outer expect, an allow, and
/// a multi-line expect naming two clippy lints the way rustfmt wraps a long attribute. The reason
/// item is not a lint, commas and parentheses inside it included.
const RUST_WITH_ATTRIBUTES: &str = r#"#![expect(clippy::pedantic, reason = "scratch fixture, (inner)")]

#[expect(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    reason = "scratch fixture"
)]
fn long() {}

#[allow(dead_code)]
fn unused() {}

#![allow(missing_docs)]
"#;

/// Lint attributes only as data: in a raw string spanning lines, in a string, in a line comment
/// and in a nested block comment, around character literals holding a quote. None of it is a
/// suppression.
const RUST_WITH_ATTRIBUTE_TEXT: &str = r##"const FIXTURE: &str = r#"
#[allow(clippy::unwrap_used)]
"#;
const TEXT: &str = "#[expect(dead_code)]";
// #[allow(dead_code)]
/* #[allow(dead_code)] /* nested */ #[allow(dead_code)] */
fn quote<'a>(text: &'a str) -> char { let _quote = '"'; '#' }
"##;

/// A multi-line expect naming one lint.
const RUST_WITH_MULTI_LINE_EXPECT: &str = r#"#[expect(
    clippy::too_many_lines,
    reason = "scratch fixture"
)]
fn long() {}
"#;

/// A second lint added on its own line inside the same attribute: the opening line is untouched,
/// so only the line naming the lint shows the addition.
const RUST_WITH_MULTI_LINE_EXPECT_EXTENDED: &str = r#"#[expect(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    reason = "scratch fixture"
)]
fn long() {}
"#;

/// A scratch repository, removed when dropped.
struct Scratch(tempfile::TempDir);

impl Scratch {
    /// An empty git repository with an identity, so committing needs no global configuration.
    fn new() -> Self {
        let scratch =
            Self(tempfile::tempdir().unwrap_or_else(|error| panic!("scratch directory: {error}")));
        scratch.git(&["init", "-q", "-b", "main"]);
        scratch.git(&["config", "user.email", "ratchet@example.invalid"]);
        scratch.git(&["config", "user.name", "Ratchet Test"]);
        scratch.git(&["config", "commit.gpgsign", "false"]);
        scratch
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    /// Writes one file, creating the directories above it.
    fn write(&self, name: &str, content: &str) {
        let path = self.path().join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .unwrap_or_else(|error| panic!("create the directory for {name}: {error}"));
        }
        fs::write(&path, content).unwrap_or_else(|error| panic!("write {name}: {error}"));
    }

    fn read(&self, name: &str) -> String {
        fs::read_to_string(self.path().join(name))
            .unwrap_or_else(|error| panic!("read {name}: {error}"))
    }

    /// Writes a baseline holding `lines`, plus the comment a real one has.
    fn write_baseline(&self, lines: &str) {
        let mut content = "# scratch baseline\n".to_owned();
        if !lines.is_empty() {
            content.push_str(lines);
            content.push('\n');
        }
        self.write(BASELINE_FILE, &content);
    }

    /// Writes one source file under `name` and a baseline, and commits both, leaving a clean
    /// index.
    fn commit_initial_as(&self, name: &str, source: &str, baseline: &str) {
        self.write(name, source);
        self.write_baseline(baseline);
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", "scratch fixture"]);
    }

    /// Writes the scratch Go file and a baseline, and commits both.
    fn commit_initial(&self, source: &str, baseline: &str) {
        self.commit_initial_as(GO_SCRATCH, source, baseline);
    }

    fn git(&self, args: &[&str]) {
        let output = gitenv::isolate(Command::new("git").args(args).current_dir(self.path()))
            .output()
            .unwrap_or_else(|error| panic!("run git {args:?}: {error}"));
        assert!(
            output.status.success(),
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Runs the ratchet here: whether it accepted the tree, and its combined output.
    fn ratchet(&self, args: &[&str]) -> (bool, String) {
        let output = gitenv::isolate(
            Command::new(repo::path("scripts/check-suppressions.sh"))
                .args(args)
                .current_dir(self.path()),
        )
        .output()
        .unwrap_or_else(|error| panic!("run the ratchet: {error}"));
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        (output.status.success(), text)
    }
}

/// A scratch state the ratchet must accept (`None`) or refuse naming the given complaint, so a
/// case cannot pass on the wrong one.
type Case = (&'static str, fn(&Scratch), Option<&'static str>);

fn run_cases(cases: &[Case]) {
    let mut failures = Vec::new();
    for (name, set_up, refusal) in cases {
        let scratch = Scratch::new();
        set_up(&scratch);
        let (accepted, output) = scratch.ratchet(&[]);
        match refusal {
            None if !accepted => {
                failures.push(format!(
                    "{name}: refused a tree it should accept:\n{output}"
                ));
            }
            Some(_) if accepted => {
                failures.push(format!(
                    "{name}: accepted the tree, want a refusal:\n{output}"
                ));
            }
            Some(message) if !output.contains(message) => {
                failures.push(format!(
                    "{name}: output does not mention {message:?}:\n{output}"
                ));
            }
            None | Some(_) => {}
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Go: `TestCheckSuppressions`, the staged-diff and count rules on Go's `//nolint`: each state
/// the ratchet must accept or refuse.
#[test]
fn check_suppressions() {
    run_cases(&[
        (
            "a tree that matches its baseline passes",
            |scratch| scratch.commit_initial(GO_WITH_GOSEC, "nolint/gosec\t1"),
            None,
        ),
        (
            "an added suppression with an untouched baseline is refused",
            |scratch| {
                scratch.commit_initial(GO_WITH_GOSEC, "nolint/gosec\t1");
                scratch.write(GO_SCRATCH, GO_WITH_TWO_GOSEC);
                scratch.git(&["add", GO_SCRATCH]);
            },
            Some(UNTOUCHED_BASELINE),
        ),
        (
            "the same addition passes once the baseline is staged with it",
            |scratch| {
                scratch.commit_initial(GO_WITH_GOSEC, "nolint/gosec\t1");
                scratch.write(GO_SCRATCH, GO_WITH_TWO_GOSEC);
                scratch.write_baseline("nolint/gosec\t2");
                scratch.git(&["add", GO_SCRATCH, BASELINE_FILE]);
            },
            None,
        ),
        (
            "a suppression swapped one for one is refused even though the count holds",
            |scratch| {
                scratch.commit_initial(GO_WITH_GOSEC, "nolint/gosec\t1");
                scratch.write(GO_SCRATCH, GO_WITH_GOSEC_MOVED);
                scratch.git(&["add", GO_SCRATCH]);
            },
            Some(UNTOUCHED_BASELINE),
        ),
        (
            "a count above the baseline is refused with nothing staged",
            |scratch| scratch.commit_initial(GO_WITH_TWO_GOSEC, "nolint/gosec\t1"),
            Some("baseline allows 1"),
        ),
        (
            "a count below the baseline is refused too",
            |scratch| scratch.commit_initial(GO_WITH_GOSEC, "nolint/gosec\t3"),
            Some("baseline still claims 3"),
        ),
        (
            "a rule the baseline has never heard of is refused",
            |scratch| scratch.commit_initial(GO_WITH_GOSEC, "nolint/dupl\t1"),
            Some("does not list this rule"),
        ),
    ]);
}

/// Go: `TestCheckSuppressions`, the Go detector's boundaries and the missing baseline: what
/// counts as a suppression, and that no baseline is never read as no suppressions.
#[test]
fn check_suppressions_detector_and_missing_baseline() {
    run_cases(&[
        (
            "a directive behind a string containing a comment opener still counts",
            |scratch| scratch.commit_initial(GO_WITH_DIRECTIVE_AFTER_URL, "nolint/dupl\t1"),
            Some("nolint/gosec"),
        ),
        (
            "a directive mentioned in prose is not a suppression",
            |scratch| scratch.commit_initial(GO_WITH_PROSE_MENTION, ""),
            None,
        ),
        (
            "a directive inside a block comment is not a suppression either",
            |scratch| scratch.commit_initial(GO_WITH_BLOCK_COMMENT, ""),
            None,
        ),
        (
            "a deleted baseline is refused rather than treated as no suppressions",
            |scratch| {
                scratch.commit_initial(GO_WITH_GOSEC, "nolint/gosec\t1");
                scratch.git(&["rm", "-q", BASELINE_FILE]);
            },
            Some("is missing"),
        ),
        (
            "a deleted baseline is refused in a tree with no suppressions at all",
            |scratch| {
                scratch.commit_initial(GO_WITH_PROSE_MENTION, "");
                scratch.git(&["rm", "-q", BASELINE_FILE]);
            },
            Some("is missing"),
        ),
    ]);
}

/// Go: `TestCheckSuppressionsUpdate`. --update writes a baseline the checker then accepts: the
/// script's own instruction for a refusal is to run it, so a broken --update would leave no way
/// out.
#[test]
fn update_writes_an_accepted_baseline() {
    let scratch = Scratch::new();
    scratch.commit_initial(GO_WITH_TWO_GOSEC, "nolint/gosec\t1");
    let (accepted, output) = scratch.ratchet(&[]);
    assert!(
        !accepted,
        "the ratchet accepted a count above its baseline:\n{output}"
    );

    let (updated, output) = scratch.ratchet(&["--update"]);
    assert!(updated, "--update failed:\n{output}");
    let written = scratch.read(BASELINE_FILE);
    assert!(
        written.contains("nolint/gosec\t2"),
        "the rewritten baseline does not record the two directives:\n{written}"
    );

    let (accepted, output) = scratch.ratchet(&[]);
    assert!(
        accepted,
        "the ratchet still refuses the tree after --update:\n{output}"
    );
}

/// Go: `TestCheckSuppressionsCountsEveryFamily`. The detector across the Go, shell and
/// Dockerfile mechanisms at once (`counts_every_rust_form` covers Rust's two): the baseline is
/// keyed per rule, so a family that resolves to the wrong key -- or to no key -- takes its
/// suppressions out of the ratchet without anything else noticing.
#[test]
fn counts_every_family() {
    let scratch = Scratch::new();
    scratch.write("Dockerfile", DOCKERFILE_WITH_IGNORES);
    scratch.write("run.sh", SHELL_WITH_DISABLE);
    scratch.write("bare.go", GO_WITH_BARE_NOLINT);
    scratch.write("spaced.go", GO_WITH_SPACED_LIST);
    scratch.write_baseline("");
    scratch.git(&["add", "-A"]);
    scratch.git(&["commit", "-q", "-m", "one file per family"]);

    let (updated, output) = scratch.ratchet(&["--update"]);
    assert!(updated, "--update failed:\n{output}");
    let written = scratch.read(BASELINE_FILE);

    // Each linter of a two-linter nolint list counts separately, the global droast form is its
    // own family, and a bare //nolint is recorded as the everything-suppression it is.
    for line in [
        "droast-global-ignore/DF005\t1",
        "droast-ignore/DF052\t1",
        "shellcheck-disable/SC2086\t1",
        "nolint/*\t1",
        "nolint/gosec\t1",
        "nolint/errcheck\t1",
    ] {
        assert!(
            written.contains(line),
            "the baseline does not record {line:?}:\n{written}"
        );
    }
}

/// Go: `TestCheckSuppressionsReadsTheStagedFileNotTheHunk`. The staged-diff rule sees whole
/// files. The detector carries raw-string and block-comment state across lines, so judging added
/// lines on their own would both invent suppressions inside a fixture and miss real ones after a
/// closing backquote -- and an invented one cannot be resolved, because the whole-tree count
/// agrees with the baseline and --update would rewrite it unchanged.
#[test]
fn reads_the_staged_file_not_the_hunk() {
    // A line added inside a raw string is not a suppression.
    let scratch = Scratch::new();
    scratch.commit_initial(GO_WITH_RAW_STRING_FIXTURE, "");
    scratch.write(GO_SCRATCH, GO_WITH_RAW_STRING_FIXTURE_EXTENDED);
    scratch.git(&["add", GO_SCRATCH]);
    let (accepted, output) = scratch.ratchet(&[]);
    assert!(
        accepted,
        "the ratchet read a raw-string line as a directive:\n{output}"
    );

    // A real directive added after the raw string still is. The staged-diff rule's own message is
    // asserted: the count rule refuses this edit too, and asserting on the refusal alone would let
    // a detector that found nothing pass.
    let scratch = Scratch::new();
    scratch.commit_initial(GO_WITH_RAW_STRING_FIXTURE, "");
    scratch.write(GO_SCRATCH, GO_WITH_RAW_STRING_FIXTURE_AND_DIRECTIVE);
    scratch.git(&["add", GO_SCRATCH]);
    let (accepted, output) = scratch.ratchet(&[]);
    assert!(
        !accepted,
        "the ratchet accepted an added directive:\n{output}"
    );
    assert!(
        output.contains(UNTOUCHED_BASELINE),
        "the staged-diff rule did not see the directive:\n{output}"
    );
}

/// Go: `TestCheckSuppressionsUpdateDischargesASwap`. The way out of the one refusal the counts
/// cannot explain. A swap leaves every count where it was, so --update has to produce a baseline
/// that differs anyway -- otherwise there is nothing to stage, the hook refuses again, and the
/// only remaining exits are a cosmetic edit or skipping the hook.
#[test]
fn update_discharges_a_swap() {
    let scratch = Scratch::new();
    scratch.write(GO_SCRATCH, GO_WITH_GOSEC);
    scratch.write_baseline("");
    scratch.git(&["add", "-A"]);
    scratch.git(&["commit", "-q", "-m", "scratch fixture"]);

    // A real baseline, written by the script itself and committed, is the starting point: the
    // question is whether a second --update against unchanged counts still moves the file.
    let (updated, output) = scratch.ratchet(&["--update"]);
    assert!(updated, "--update failed:\n{output}");
    scratch.git(&["add", BASELINE_FILE]);
    scratch.git(&["commit", "-q", "-m", "record the baseline"]);
    let before = scratch.read(BASELINE_FILE);

    scratch.write(GO_SCRATCH, GO_WITH_GOSEC_MOVED);
    scratch.git(&["add", GO_SCRATCH]);
    let (accepted, output) = scratch.ratchet(&[]);
    assert!(
        !accepted,
        "the ratchet accepted a swapped suppression:\n{output}"
    );

    let (updated, output) = scratch.ratchet(&["--update"]);
    assert!(updated, "--update failed:\n{output}");
    let after = scratch.read(BASELINE_FILE);
    assert_ne!(
        after, before,
        "--update left the baseline byte-identical, so there is nothing to stage"
    );

    scratch.git(&["add", BASELINE_FILE]);
    let (accepted, output) = scratch.ratchet(&[]);
    assert!(
        accepted,
        "the ratchet still refuses the swap after the baseline was staged:\n{output}"
    );
}

/// Go: `TestCheckSuppressionsAllowsARename`. Moving a file is not adding its suppressions. The
/// staged-diff rule works from the new name, so without rename detection a moved file would look
/// wholly new and every directive it already carried would be reported as freshly added -- with
/// counts unchanged, and therefore with nothing the baseline could say about it.
#[test]
fn allows_a_rename() {
    let scratch = Scratch::new();
    scratch.write("old.go", GO_WITH_SPACED_LIST);
    scratch.write("Dockerfile", DOCKERFILE_WITH_IGNORES);
    scratch.write_baseline(
        "droast-global-ignore/DF005\t1\ndroast-ignore/DF052\t1\nnolint/errcheck\t1\n\
         nolint/gosec\t1",
    );
    scratch.git(&["add", "-A"]);
    scratch.git(&["commit", "-q", "-m", "files with suppressions"]);
    scratch.git(&["mv", "old.go", "new.go"]);
    scratch.git(&["mv", "Dockerfile", "build.Dockerfile"]);

    let (accepted, output) = scratch.ratchet(&[]);
    assert!(
        accepted,
        "the ratchet read a moved file as newly added suppressions:\n{output}"
    );
}

/// Go: `TestCheckSuppressionsRust`, the count and staged-diff rules on Rust's `#[expect]` and
/// `#[allow]`, and what the Rust detector must not read as a suppression.
#[test]
fn check_suppressions_rust() {
    run_cases(&[
        (
            "a tree that matches its baseline passes",
            |scratch| {
                scratch.commit_initial_as(
                    RUST_SCRATCH,
                    RUST_WITH_ONE,
                    "expect/clippy::too_many_lines\t1",
                );
            },
            None,
        ),
        (
            "an added attribute with an untouched baseline is refused",
            |scratch| {
                scratch.commit_initial_as(
                    RUST_SCRATCH,
                    RUST_WITH_ONE,
                    "expect/clippy::too_many_lines\t1",
                );
                scratch.write(RUST_SCRATCH, RUST_WITH_TWO);
                scratch.git(&["add", RUST_SCRATCH]);
            },
            Some(UNTOUCHED_BASELINE),
        ),
        (
            "the same addition passes once the baseline is staged with it",
            |scratch| {
                scratch.commit_initial_as(
                    RUST_SCRATCH,
                    RUST_WITH_ONE,
                    "expect/clippy::too_many_lines\t1",
                );
                scratch.write(RUST_SCRATCH, RUST_WITH_TWO);
                scratch.write_baseline("expect/clippy::too_many_lines\t2");
                scratch.git(&["add", RUST_SCRATCH, BASELINE_FILE]);
            },
            None,
        ),
        (
            "an attribute swapped one for one is refused even though the count holds",
            |scratch| {
                scratch.commit_initial_as(
                    RUST_SCRATCH,
                    RUST_WITH_ONE,
                    "expect/clippy::too_many_lines\t1",
                );
                scratch.write(RUST_SCRATCH, RUST_WITH_ONE_MOVED);
                scratch.git(&["add", RUST_SCRATCH]);
            },
            Some(UNTOUCHED_BASELINE),
        ),
        (
            "a count above the baseline is refused",
            |scratch| {
                scratch.commit_initial_as(
                    RUST_SCRATCH,
                    RUST_WITH_TWO,
                    "expect/clippy::too_many_lines\t1",
                );
            },
            Some("baseline allows 1"),
        ),
        (
            "a count below the baseline is refused",
            |scratch| {
                scratch.commit_initial_as(
                    RUST_SCRATCH,
                    RUST_WITH_ONE,
                    "expect/clippy::too_many_lines\t3",
                );
            },
            Some("baseline still claims 3"),
        ),
        (
            "a lint the baseline has never heard of is refused",
            |scratch| scratch.commit_initial_as(RUST_SCRATCH, RUST_WITH_ONE, "expect/dead_code\t1"),
            Some("does not list this rule"),
        ),
        (
            "an attribute behind a string containing a comment opener still counts",
            |scratch| scratch.commit_initial_as(RUST_SCRATCH, RUST_AFTER_URL, ""),
            Some(TOO_MANY_LINES),
        ),
        (
            "an attribute mentioned in a line comment is not a suppression",
            |scratch| scratch.commit_initial_as(RUST_SCRATCH, RUST_PROSE_MENTION, ""),
            None,
        ),
        (
            "an attribute inside a block comment is not a suppression either",
            |scratch| scratch.commit_initial_as(RUST_SCRATCH, RUST_BLOCK_COMMENT, ""),
            None,
        ),
        (
            "a cfg_attr-wrapped attribute is not counted",
            |scratch| scratch.commit_initial_as(RUST_SCRATCH, RUST_WITH_CFG_ATTR, ""),
            None,
        ),
    ]);
}

/// Go: `TestCheckSuppressionsCountsEveryRustForm`. The Rust detector's keys: each lint of a
/// multi-lint attribute counts separately, inner and outer forms both count, and attribute text
/// held as data -- strings, raw strings, comments, nested block comments -- counts as nothing.
#[test]
fn counts_every_rust_form() {
    let scratch = Scratch::new();
    scratch.write("src/attributes.rs", RUST_WITH_ATTRIBUTES);
    scratch.write("src/text.rs", RUST_WITH_ATTRIBUTE_TEXT);
    scratch.write_baseline("");
    scratch.git(&["add", "-A"]);
    scratch.git(&["commit", "-q", "-m", "every rust form"]);

    let (updated, output) = scratch.ratchet(&["--update"]);
    assert!(updated, "--update failed:\n{output}");
    let written = scratch.read(BASELINE_FILE);

    for line in [
        "expect/clippy::pedantic\t1",
        "expect/clippy::too_many_lines\t1",
        "expect/clippy::cast_possible_truncation\t1",
        "allow/dead_code\t1",
        "allow/missing_docs\t1",
    ] {
        assert!(
            written.contains(line),
            "the baseline does not record {line:?}:\n{written}"
        );
    }
    // The text fixture names clippy::unwrap_used and expect(dead_code) only as data; the one real
    // allow(dead_code) is in the attributes fixture.
    for unwanted in ["allow/clippy::unwrap_used", "expect/dead_code", "reason"] {
        assert!(
            !written.contains(unwanted),
            "the baseline records {unwanted:?}, which is not a suppression:\n{written}"
        );
    }
}

/// Go: `TestCheckSuppressionsSeesALintAddedToAMultiLineExpect`. A lint is located on the line that
/// names it, not on the line that opens the attribute: rustfmt puts each lint of a long attribute
/// on its own line, and adding one there leaves the opening line untouched.
#[test]
fn sees_a_lint_added_to_a_multi_line_expect() {
    let scratch = Scratch::new();
    scratch.commit_initial_as(
        RUST_SCRATCH,
        RUST_WITH_MULTI_LINE_EXPECT,
        "expect/clippy::too_many_lines\t1",
    );
    scratch.write(RUST_SCRATCH, RUST_WITH_MULTI_LINE_EXPECT_EXTENDED);
    scratch.git(&["add", RUST_SCRATCH]);

    let (accepted, output) = scratch.ratchet(&[]);
    assert!(
        !accepted,
        "the ratchet accepted a lint added to an existing expect:\n{output}"
    );
    assert!(
        output.contains("scratch.rs:3\texpect/clippy::cast_possible_truncation"),
        "the output does not locate the added lint on its own line:\n{output}"
    );
}

/// Go: `TestCheckSuppressionsSkipsRustFixtures`. A Rust file under a testdata/ directory is
/// data: neither counted nor treated as newly added.
#[test]
fn skips_rust_fixtures() {
    let scratch = Scratch::new();
    scratch.commit_initial(GO_WITH_PROSE_MENTION, "");
    scratch.write("tests/testdata/fixture.rs", RUST_WITH_ONE);
    scratch.write("testdata/top.rs", RUST_WITH_ONE);
    scratch.git(&["add", "-A"]);
    let (accepted, output) = scratch.ratchet(&[]);
    assert!(accepted, "the ratchet counted a Rust fixture:\n{output}");
}

/// Go: `TestCheckSuppressionsCountsNeighborsOfRustFixtures`, the negative control of the test
/// above: a Rust file next to a testdata/ directory but outside it is counted, and the exclusion
/// covers Rust only, so a Go file under testdata/ is counted exactly as before Rust was added.
#[test]
fn counts_neighbors_of_rust_fixtures() {
    let mut failures = Vec::new();
    for (name, content, key) in [
        ("tests/testdatax/x.rs", RUST_WITH_ONE, TOO_MANY_LINES),
        ("tests/testdata.rs", RUST_WITH_ONE, TOO_MANY_LINES),
        ("internal/x/testdata/x.go", GO_WITH_GOSEC, "nolint/gosec"),
    ] {
        let scratch = Scratch::new();
        scratch.commit_initial(GO_WITH_PROSE_MENTION, "");
        scratch.write(name, content);
        scratch.git(&["add", "-A"]);
        let (accepted, output) = scratch.ratchet(&[]);
        if accepted || !output.contains(key) {
            failures.push(format!("the ratchet did not count {name}:\n{output}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Go: `TestCheckSuppressionsReadsTheStagedRustFileNotTheHunk`, the Rust side of
/// `reads_the_staged_file_not_the_hunk`: the raw-string state carries across lines.
#[test]
fn reads_the_staged_rust_file_not_the_hunk() {
    // A line added inside a raw string is not a suppression.
    let scratch = Scratch::new();
    scratch.commit_initial_as(RUST_SCRATCH, RUST_RAW_STRING_FIXTURE, "");
    scratch.write(RUST_SCRATCH, RUST_RAW_STRING_FIXTURE_EXTENDED);
    scratch.git(&["add", RUST_SCRATCH]);
    let (accepted, output) = scratch.ratchet(&[]);
    assert!(
        accepted,
        "the ratchet read a raw-string line as an attribute:\n{output}"
    );

    // A real attribute added after the raw string still is, and the staged-diff rule says so.
    let scratch = Scratch::new();
    scratch.commit_initial_as(RUST_SCRATCH, RUST_RAW_STRING_FIXTURE, "");
    scratch.write(RUST_SCRATCH, RUST_RAW_STRING_FIXTURE_AND_ATTRIBUTE);
    scratch.git(&["add", RUST_SCRATCH]);
    let (accepted, output) = scratch.ratchet(&[]);
    assert!(
        !accepted && output.contains(UNTOUCHED_BASELINE),
        "the staged-diff rule did not see the added attribute:\n{output}"
    );
}

/// Go: `TestCheckSuppressionsAllowsARustRename`. Moving a Rust file is not adding its attributes.
#[test]
fn allows_a_rust_rename() {
    let scratch = Scratch::new();
    scratch.write("src/old.rs", RUST_WITH_ATTRIBUTES);
    scratch.write_baseline(
        "allow/dead_code\t1\nallow/missing_docs\t1\nexpect/clippy::cast_possible_truncation\t1\n\
         expect/clippy::pedantic\t1\nexpect/clippy::too_many_lines\t1",
    );
    scratch.git(&["add", "-A"]);
    scratch.git(&["commit", "-q", "-m", "a file with attributes"]);
    scratch.git(&["mv", "src/old.rs", "src/new.rs"]);

    let (accepted, output) = scratch.ratchet(&[]);
    assert!(
        accepted,
        "the ratchet read a moved file as newly added attributes:\n{output}"
    );
}
