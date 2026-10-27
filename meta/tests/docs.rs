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
//! docs`); it ports `test/docs`, its Go original, which the cutover removed.

use std::fs;

use meta::links::{Checker, slug};
use meta::repo;

/// Go: `TestRelativeLinksResolve`. Every relative link and fragment in the tracked Markdown. The
/// list comes from git, which keeps an untracked scratch file out.
#[test]
fn relative_links_resolve() {
    let files = repo::tracked(&["*.md"]);
    assert!(
        !files.is_empty(),
        "git ls-files lists no Markdown; the check would pass on nothing"
    );
    let mut checker = Checker::new(&repo::root());
    let mut problems = Vec::new();
    for file in &files {
        let found = checker
            .check_file(file)
            .unwrap_or_else(|error| panic!("read {file}: {error}"));
        problems.extend(found.iter().map(ToString::to_string));
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// Go: `TestCheckerFindsBrokenLinks`, the negative control: the link check is not blind, on a
/// scratch tree. A missing file, a missing anchor and a link out of the tree are each reported;
/// a link in a code span or a fence and an absolute URL are not checked.
#[test]
fn checker_finds_broken_links() {
    let scratch = tempfile::tempdir().expect("scratch directory");
    fs::write(
        scratch.path().join("a.md"),
        [
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
    )
    .expect("write a.md");
    fs::write(scratch.path().join("b.md"), "# Heading Two\n").expect("write b.md");

    let problems: Vec<String> = Checker::new(scratch.path())
        .check_file("a.md")
        .expect("check a.md")
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        problems,
        [
            r#"a.md:4: link "missing.md": missing.md does not exist"#,
            r#"a.md:4: link "b.md#nope": b.md has no heading or anchor #nope"#,
            r#"a.md:8: link "../outside.md" leaves the repository"#,
        ]
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
