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
// Adapted from swarm-scheduler-exporter/meta/tests/dockerfiles.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The shipped image's invariants that the Dockerfile's own comments state: a pinned frontend and
//! the build and runtime stages from tag-pinned dhi.io bases. The rules are `repo_meta::image`'s,
//! with this repository's parameters. Each rule has its negative control, most of them edits of the
//! real Dockerfile.

use repo_meta::dockerfile;
use repo_meta::image::{BasePolicy, stage_problems, syntax_problems};
use repo_meta::report;

const SHIPPED_DOCKERFILE: &str = "deployments/docker/Dockerfile";
const STAGES: &[&str] = &["build", "runtime"];

fn shipped() -> String {
    meta::repo().read(SHIPPED_DOCKERFILE)
}

/// `content` with its first `from` replaced by `to`; `from` must be there, so an edit cannot
/// silently change nothing.
fn edit(content: &str, from: &str, to: &str) -> String {
    assert!(content.contains(from), "{from:?} is gone");
    content.replacen(from, to, 1)
}

/// Go: `TestSyntaxDirectiveIsPinned`. The `BuildKit` frontend that reads the Dockerfile is a
/// literal docker/dockerfile reference pinned by tag and digest: a bare docker/dockerfile:1.7 is
/// a tag that can be moved under a build, and the Dockerfile says the line is updated by hand.
#[test]
fn syntax_directive_is_pinned() {
    let problems = syntax_problems(SHIPPED_DOCKERFILE, &shipped(), true);
    assert!(problems.is_empty(), "{}", report(&problems));
}

/// The negative control of the syntax rule: a frontend by tag alone, another frontend, and no
/// directive at all are each refused.
#[test]
fn syntax_rules() {
    let content = shipped();
    let syntax = dockerfile::syntax(&content).expect("the Dockerfile has a syntax directive");
    let tag_only = syntax
        .split_once('@')
        .map_or(syntax.as_str(), |(reference, _)| reference)
        .to_owned();
    for (name, broken) in [
        (
            "a frontend by tag alone",
            edit(&content, &syntax, &tag_only),
        ),
        (
            "another frontend",
            edit(
                &content,
                "docker/dockerfile:",
                "example.invalid/dockerfile:",
            ),
        ),
        (
            "no directive",
            edit(&content, &format!("# syntax={syntax}\n"), ""),
        ),
    ] {
        assert!(
            !syntax_problems(SHIPPED_DOCKERFILE, &broken, true).is_empty(),
            "{name} was accepted"
        );
    }
}

/// Go: `TestShippedStages`. The two stages, in order, each from a literal dhi.io base pinned by
/// tag and not by digest: dhi.io republishes its tags with security fixes, and a rebuild should
/// pick those up. A variable, latest or another registry would let a build change base unseen.
#[test]
fn shipped_stages_are_build_and_runtime() {
    let problems = stage_problems(SHIPPED_DOCKERFILE, &shipped(), STAGES, BasePolicy::DhiOnly);
    assert!(problems.is_empty(), "{}", report(&problems));
}

/// The negative control of the stage rule: a base from another registry, a base by digest, a
/// floating base, and a third stage are each refused.
#[test]
fn stage_rules() {
    let content = shipped();
    let digest = "@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let runtime_from = "FROM dhi.io/static:20250419 AS runtime";
    for (name, broken) in [
        (
            "a base from another registry",
            edit(
                &content,
                "FROM dhi.io/static:",
                "FROM docker.io/library/alpine:",
            ),
        ),
        (
            "a base by digest",
            edit(
                &content,
                runtime_from,
                &format!("FROM dhi.io/static:20250419{digest} AS runtime"),
            ),
        ),
        (
            "a floating base",
            edit(
                &content,
                runtime_from,
                "FROM dhi.io/static:latest AS runtime",
            ),
        ),
        (
            "a third stage",
            format!("{content}\nFROM dhi.io/static:20250419 AS extra\n"),
        ),
    ] {
        assert!(
            !stage_problems(SHIPPED_DOCKERFILE, &broken, STAGES, BasePolicy::DhiOnly).is_empty(),
            "{name} was accepted"
        );
    }
}

/// Go: `TestParseRejectsWhatTheAssertionsMustNotMiss`, the negative control for the reader: each
/// of these inputs has to come out as something the assertions above refuse.
#[test]
fn parse_rejects_what_the_assertions_must_not_miss() {
    let digest = "@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    for (name, image) in [
        (
            "a variable tag",
            "dhi.io/rust:${RUST_LINE}-debian13-dev".to_owned(),
        ),
        ("a variable image", "$BASE".to_owned()),
        ("no tag", "dhi.io/static".to_owned()),
        ("latest", "dhi.io/static:latest".to_owned()),
        ("a digest", format!("dhi.io/static:20250419{digest}")),
        ("a stage name, not a ref", "build".to_owned()),
    ] {
        assert!(
            !dockerfile::tag_pinned(&image),
            "{name} ({image:?}) passes as tag-pinned"
        );
    }
    assert!(
        dockerfile::tag_pinned("dhi.io/static:20250419"),
        "a literal tag does not pass as tag-pinned"
    );

    for (name, image) in [
        ("a tag only", "docker/dockerfile:1.7".to_owned()),
        (
            "a short digest",
            "docker/dockerfile:1.7@sha256:0123".to_owned(),
        ),
        ("a digest with no tag", format!("docker/dockerfile{digest}")),
        (
            "a variable tag",
            format!("docker/dockerfile:${{TAG}}{digest}"),
        ),
    ] {
        assert!(
            !dockerfile::digest_pinned(&image),
            "{name} ({image:?}) passes as digest-pinned"
        );
    }
    assert!(
        dockerfile::digest_pinned(&format!("docker/dockerfile:1.7{digest}")),
        "a literal tag plus digest does not pass as digest-pinned"
    );

    let stages =
        dockerfile::parse("FROM a:1 AS one\nRUN echo \\\n  two\n# note\nFROM b:2\nUSER 1:1\n")
            .expect("parse");
    assert!(
        stages.len() == 2
            && stages[0].name == "one"
            && stages[1].name.is_empty()
            && stages[0].joined() == "RUN echo two"
            && stages[1].joined() == "USER 1:1",
        "parsed {stages:?}"
    );

    for (name, content) in [
        ("no directive", "FROM a:1\n".to_owned()),
        (
            "after a comment",
            format!("# note\n# syntax=docker/dockerfile:1.7{digest}\n"),
        ),
        (
            "after a blank line",
            format!("\n# syntax=docker/dockerfile:1.7{digest}\n"),
        ),
        (
            "after an instruction",
            format!("FROM a:1\n# syntax=docker/dockerfile:1.7{digest}\n"),
        ),
        ("a directive with no value", "# syntax=\n".to_owned()),
    ] {
        let syntax = dockerfile::syntax(&content);
        assert!(
            syntax.is_none(),
            "{name}: read the syntax directive {syntax:?}, which BuildKit ignores"
        );
    }

    assert!(
        dockerfile::parse("FROM a:1\nRUN echo \\\n").is_err(),
        "a file ending inside a continuation parsed"
    );
    assert!(
        dockerfile::parse("RUN echo\nFROM a:1\n").is_err(),
        "an instruction before the first FROM parsed"
    );
}
