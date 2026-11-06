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
//! The flags and the scan are `repo_meta::vacuous`'s (extracted from this file).

use std::sync::LazyLock;

use repo_meta::report;
use repo_meta::vacuous::{AllowedFragment, Hit, VacuousFlags};

/// The flags none of the files that define gates may carry, outside [`ALLOWED_FRAGMENTS`]: the
/// core set and the Rust one (a test run that only compiles, lints capped or allowed wholesale,
/// tests skipped by name, a workspace member left out of a workspace-wide command, and a clippy
/// run without `-D warnings`). A cargo call is cargo itself or make's `$(CARGO)`.
static FLAGS: LazyLock<VacuousFlags> = LazyLock::new(|| VacuousFlags::core().with_rust());

/// The exact pieces of shell, each in one file, whose `|| true` is not a gate passing on
/// nothing; the reason says why. Each one tolerates a command whose failure is an answer, not an
/// error.
const ALLOWED_FRAGMENTS: [AllowedFragment<'static>; 2] = [
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

/// The flags on `content`'s lines, after removing the fragments allowed in `file`; `used` records
/// which allowlist entries matched.
fn vacuous_hits(file: &str, content: &str, used: &mut [bool; ALLOWED_FRAGMENTS.len()]) -> Vec<Hit> {
    FLAGS.hits(file, content, &ALLOWED_FRAGMENTS, used)
}

/// The hits on one sample line, in a file no exception covers.
fn sample_hits(sample: &str) -> Vec<Hit> {
    vacuous_hits("Makefile", sample, &mut [false; ALLOWED_FRAGMENTS.len()])
}

/// Go: `TestNoVacuousFlags`. Scans the files that define gates: the Makefile and its fragments,
/// the pre-commit configuration and every YAML file under .github, the workflows included. An
/// exception that matches nothing is reported too.
#[test]
fn no_vacuous_flags() {
    let repo = meta::repo();
    let fragments = repo.files_under(".mk", &[".mk"]);
    assert!(
        !fragments.is_empty(),
        "no make fragments found under .mk/; the scan would check nothing"
    );
    let workflows = repo.files_under(".github", &[".yaml", ".yml"]);
    assert!(
        !workflows.is_empty(),
        "no YAML found under .github; the checks would see nothing"
    );
    let mut files = vec!["Makefile".to_owned(), ".pre-commit-config.yaml".to_owned()];
    files.extend(fragments);
    files.extend(workflows);

    let problems = FLAGS.problems(&repo.read_all(&files), &ALLOWED_FRAGMENTS);
    assert!(problems.is_empty(), "{}", report(&problems));
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
        hits.len() == 1 && hits[0].line == 3,
        "want one hit on line 3, got {hits:?}"
    );
}

/// Negative control: an exception that matches no line is a problem, so it cannot outlive the
/// shell it excuses.
#[test]
fn an_unused_exception_is_refused() {
    let problems = FLAGS.problems(
        &[(
            ".github/workflows/release.yaml".to_owned(),
            "jobs: {}\n".to_owned(),
        )],
        &ALLOWED_FRAGMENTS,
    );
    assert_eq!(problems.len(), 2, "{problems:?}");
    assert!(
        problems
            .iter()
            .all(|problem| problem.message.contains("matches nothing")),
        "{problems:?}"
    );
}
