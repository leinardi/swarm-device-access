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
// Adapted from swarm-scheduler-exporter/meta/tests/docs.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The offline documentation check: every relative link and fragment in the tracked Markdown
//! resolves. The docs-links pre-commit hook runs this file (`cargo test --locked -p meta --test
//! docs`); it ports `test/docs`, its Go original, which the cutover removed. The checker is
//! `repo_meta::links`'s.

use repo_meta::links::{Checker, check_repository, slug};
use repo_meta::scratch::ScratchRepo;
use repo_meta::{report, strings};

/// Go: `TestRelativeLinksResolve`. Every relative link and fragment in the tracked Markdown
/// (`repo_meta::links::check_repository`). The list comes from git, which keeps an untracked
/// scratch file out, and a listing with no Markdown is a problem, not a pass on nothing.
#[test]
fn relative_links_resolve() {
    let problems = check_repository(&meta::repo());
    assert!(problems.is_empty(), "{}", report(&problems));
}

/// Go: `TestCheckerFindsBrokenLinks`, the negative control: the link check is not blind, on a
/// scratch tree. A missing file, a missing anchor and a link out of the tree are each reported;
/// a link in a code span or a fence and an absolute URL are not checked.
#[test]
fn checker_finds_broken_links() {
    let scratch = ScratchRepo::new();
    scratch.write(
        "a.md",
        &[
            "# Title",
            "## 1. Section — `GET /x`",
            "[ok](b.md#heading-two) [ok](#1-section--get-x) [ok](https://example.com/missing)",
            "[broken file](missing.md) [broken anchor](b.md#nope) `[in code](missing-too.md)`",
            "```text",
            "[in a fence](missing-three.md)",
            "```",
            "[outside](../outside.md)",
        ]
        .join("\n"),
    );
    scratch.write("b.md", "# Heading Two\n");

    assert_eq!(
        strings(&Checker::new(scratch.path()).check_file("a.md")),
        [
            r#"a.md:4: link "missing.md": missing.md does not exist"#,
            r#"a.md:4: link "b.md#nope": b.md has no heading or anchor #nope"#,
            r#"a.md:8: link "../outside.md" leaves the repository"#,
        ]
    );
}

/// A Markdown file that cannot be read is a problem, not a pass on nothing (the old reader
/// panicked on it).
#[test]
fn an_unreadable_file_is_a_problem() {
    let scratch = ScratchRepo::new();
    let problems = Checker::new(scratch.path()).check_file("gone.md");
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert_eq!(problems[0].location, "gone.md");
}

/// The repository check refuses a repository whose git lists no Markdown at all.
#[test]
fn no_markdown_is_a_problem() {
    let scratch = ScratchRepo::new();
    scratch.write("a.txt", "no Markdown here\n");
    scratch.git(&["add", "a.txt"]);
    assert_eq!(
        strings(&check_repository(&scratch.repo())),
        ["git lists no Markdown file; the link check would pass on nothing"]
    );
}

/// Go: `TestSlug`. The GitHub slug rules the anchors follow: Go's cases, then SSE's.
#[test]
fn slugs_follow_github() {
    for (heading, want) in [
        (
            "Why the Build Happens Before the Tag",
            "why-the-build-happens-before-the-tag",
        ),
        ("Dry-Run Coverage", "dry-run-coverage"),
        (
            "**Bold** and _emphasis_ with snake_case",
            "bold-and-emphasis-with-snake_case",
        ),
        ("[Linked](other.md) heading", "linked-heading"),
        (
            "1. Entrypoint (`cmd/`) — config",
            "1-entrypoint-cmd--config",
        ),
        ("Tags, Moves and `X.Y.Z`", "tags-moves-and-xyz"),
        ("Ünïcode Lëtters Stay", "ünïcode-lëtters-stay"),
        ("1. Test (Home) — `/`", "1-test-home--"),
        ("5. `GET /api/v1/info`", "5-get-apiv1info"),
        ("5.1 Implementation", "51-implementation"),
        (
            "11.2 Path A fallback — HTTP probe",
            "112-path-a-fallback--http-probe",
        ),
        ("5. History Module (`idb`)", "5-history-module-idb"),
        ("2.2 Stateless Server", "22-stateless-server"),
        (
            "6. `compat.networkquality.max_upload_bytes`",
            "6-compatnetworkqualitymax_upload_bytes",
        ),
        (
            "💡 What is “Swarm Device Access”?",
            "-what-is-swarm-device-access",
        ),
    ] {
        assert_eq!(slug(heading), want, "{heading:?}");
    }
}
