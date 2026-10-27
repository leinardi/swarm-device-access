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
// Adapted from swarm-scheduler-exporter/meta/tests/toolchain.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! No gate carries a flag that makes it pass on nothing: the Makefile and its fragments, the
//! pre-commit configuration and every YAML file under .github. The Rust port of
//! `test/toolchain/vacuous_flags_test.go`, which the cutover removed: its flags, the Rust ones
//! included (SSE has only the first six), and its reviewed exceptions. The exceptions are SSE's
//! form: the exact fragment is removed from the line before the flags are matched, so a second
//! `|| true` on an allowed line is still caught, where the Go test lets the whole line through.

use std::sync::LazyLock;

use meta::repo;
use regex::Regex;

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|error| panic!("invalid pattern {pattern}: {error}"))
}

/// A flag that turns a gate with nothing to check, or a failing one, into a pass.
struct VacuousFlag {
    name: &'static str,
    pattern: Regex,
}

/// The flags none of the files that define gates may carry, outside [`ALLOWED_FRAGMENTS`]. The
/// Rust gates: a test run that only compiles, lints capped or allowed wholesale, tests skipped by
/// name, or a workspace member left out of a workspace-wide command. A cargo call is cargo itself
/// or make's `$(CARGO)`.
static VACUOUS_FLAGS: LazyLock<Vec<VacuousFlag>> = LazyLock::new(|| {
    [
        ("|| true", r"\|\|\s*(true\b|:(\s|;|$)|exit\s+0\b)"),
        ("make's - recipe prefix", r"^\t[@+]*-"),
        (".IGNORE", r"^\.IGNORE\b"),
        (
            "make --ignore-errors",
            r"\$\(MAKE\).*\s(-[a-zA-Z]*i[a-zA-Z]*|--ignore-errors)\b",
        ),
        ("continue-on-error", r"continue-on-error"),
        (
            "if: false",
            r#"\bif:\s*(["']?false["']?\s*$|["']?\$\{\{\s*false\s*\}\}["']?)"#,
        ),
        (
            "cargo test --no-run",
            r"(?i)\bcargo\)?\s.*\btest\b.*\s--no-run\b",
        ),
        ("--cap-lints", r"--cap-lints\b"),
        ("warnings allowed", r"(\s-A\s*|--allow[ =])warnings\b"),
        ("cargo --skip", r"(?i)\bcargo\)?\s.*\s--skip\b"),
        ("cargo --exclude", r"(?i)\bcargo\)?\s.*\s--exclude\b"),
    ]
    .into_iter()
    .map(|(name, pattern)| VacuousFlag {
        name,
        pattern: regex(pattern),
    })
    .collect()
});

/// A clippy run in a gate (cargo itself or make's `$(CARGO)`), which must turn warnings into
/// errors.
static CLIPPY_CALL: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?i)\bcargo\)?\s+(\+\S+\s+)?clippy\b"));
/// A YAML `name:` line, a hook's or a step's label rather than a command it runs.
static YAML_NAME: LazyLock<Regex> = LazyLock::new(|| regex(r"^\s*(-\s+)?name:\s"));
/// The `-D warnings` a clippy gate needs: without it every finding is a warning, and the gate
/// passes whatever clippy reports.
static CLIPPY_DENIES_WARNINGS: LazyLock<Regex> =
    LazyLock::new(|| regex(r"\s(-D\s*|--deny[ =])warnings\b"));

/// One exact piece of shell, in one file, whose `|| true` is not a gate passing on nothing; the
/// reason says why. Each one tolerates a command whose failure is an answer, not an error.
struct AllowedFragment {
    file: &'static str,
    fragment: &'static str,
    reason: &'static str,
}

const ALLOWED_FRAGMENTS: [AllowedFragment; 2] = [
    AllowedFragment {
        file: ".github/workflows/release.yaml",
        fragment: r#"{ grep -E "$RELEASE_TAG_REGEX" || true; }"#,
        reason: "grep exits 1 when no tag matches the release pattern, and no release tag is a \
                 result the release workflow handles after the pipe",
    },
    AllowedFragment {
        file: ".github/workflows/release.yaml",
        fragment: r#"{ git tag --merged "HEAD^" 2>/dev/null || true; }"#,
        reason: "HEAD^ does not exist on a root commit, which has no previous release",
    },
];

/// One flag found on one line.
#[derive(Debug)]
struct VacuousHit {
    /// The line, counted from 1.
    number: usize,
    flag: &'static str,
    line: String,
}

/// The flags on `content`'s lines, after removing the fragments allowed in `file`; `used` records
/// which allowlist entries matched.
fn vacuous_hits(
    file: &str,
    content: &str,
    used: &mut [bool; ALLOWED_FRAGMENTS.len()],
) -> Vec<VacuousHit> {
    let mut hits = Vec::new();
    for (index, original) in content.split('\n').enumerate() {
        let mut line = original.to_owned();
        for (allowed_index, allowed) in ALLOWED_FRAGMENTS.iter().enumerate() {
            if allowed.file == file && line.contains(allowed.fragment) {
                line = line.replace(allowed.fragment, "");
                used[allowed_index] = true;
            }
        }
        if CLIPPY_CALL.is_match(&line)
            && !CLIPPY_DENIES_WARNINGS.is_match(&line)
            && !YAML_NAME.is_match(&line)
        {
            hits.push(VacuousHit {
                number: index + 1,
                flag: "cargo clippy without -D warnings",
                line: original.to_owned(),
            });
        }
        for flag in VACUOUS_FLAGS.iter() {
            if flag.pattern.is_match(&line) {
                hits.push(VacuousHit {
                    number: index + 1,
                    flag: flag.name,
                    line: original.to_owned(),
                });
            }
        }
    }
    hits
}

/// The hits on one sample line, in a file no exception covers.
fn sample_hits(sample: &str) -> Vec<VacuousHit> {
    vacuous_hits("Makefile", sample, &mut [false; ALLOWED_FRAGMENTS.len()])
}

/// Go: `TestNoVacuousFlags`. Scans the files that define gates: the Makefile and its fragments,
/// the pre-commit configuration and every YAML file under .github, the workflows included. An
/// exception that matches nothing is reported too.
#[test]
fn no_vacuous_flags() {
    let fragments = repo::files_under(".mk", &[".mk"]);
    assert!(
        !fragments.is_empty(),
        "no make fragments found under .mk/; the scan would check nothing"
    );
    let workflows = repo::files_under(".github", &[".yaml", ".yml"]);
    assert!(
        !workflows.is_empty(),
        "no YAML found under .github; the checks would see nothing"
    );
    let mut files = vec!["Makefile".to_owned(), ".pre-commit-config.yaml".to_owned()];
    files.extend(fragments);
    files.extend(workflows);

    let mut used = [false; ALLOWED_FRAGMENTS.len()];
    let mut problems = Vec::new();
    for file in &files {
        for hit in vacuous_hits(file, &repo::read(file), &mut used) {
            problems.push(format!(
                "{file}:{}: {}: {}",
                hit.number,
                hit.flag,
                hit.line.trim()
            ));
        }
    }
    for (allowed, used) in ALLOWED_FRAGMENTS.iter().zip(used) {
        if !used {
            problems.push(format!(
                "the exception {:?} in {} ({}) matches nothing; remove it",
                allowed.fragment, allowed.file, allowed.reason
            ));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// Go: `TestVacuousFlagsAreRecognized`, the negative control: each flag, written as a gate would
/// carry it, is found; a clippy gate that denies warnings and a hook's or step's label are not;
/// and an exception covers only its own fragment in its own file.
#[test]
fn vacuous_flags_are_recognized() {
    for sample in [
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
    ] {
        assert!(
            !sample_hits(sample).is_empty(),
            "no vacuous flag found in {sample:?}"
        );
    }

    for sample in [
        "\t$(CARGO) clippy --workspace --all-targets --all-features --locked -- -D warnings",
        "        entry: cargo clippy --workspace --all-targets --locked -- --deny=warnings",
        "        name: cargo clippy",
        "      - name: Lint with clippy",
        "        entry: cargo test --locked -p meta --test docs",
    ] {
        let hits = sample_hits(sample);
        assert!(hits.is_empty(), "{sample:?} was flagged: {hits:?}");
    }

    let release = ".github/workflows/release.yaml";
    let allowed = r#"            { grep -E "$RELEASE_TAG_REGEX" || true; } | sort -V | tail -n1"#;
    let mut used = [false; ALLOWED_FRAGMENTS.len()];
    let hits = vacuous_hits(release, allowed, &mut used);
    assert!(
        hits.is_empty() && used[0],
        "the allowed fragment was flagged or not recorded: {hits:?}"
    );

    let extended = format!("{allowed} || true");
    for (file, sample) in [
        (".github/workflows/ci.yaml", allowed),
        (release, extended.as_str()),
        (
            ".github/workflows/x.yaml",
            r#"  { git tag --merged "HEAD^" 2>/dev/null || true; }"#,
        ),
    ] {
        assert!(
            !vacuous_hits(file, sample, &mut [false; ALLOWED_FRAGMENTS.len()]).is_empty(),
            "{file}: {sample:?} passed through the allowlist"
        );
    }
}

/// Go: `TestVacuousHitNumbering`. A hit names its line, so a failure is actionable.
#[test]
fn vacuous_hit_numbering() {
    let hits = sample_hits("all:\n\tgo vet ./...\n\tgo test ./... || true\n");
    assert!(
        hits.len() == 1 && hits[0].number == 3,
        "want one hit on line 3, got {hits:?}"
    );
}
