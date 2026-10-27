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

//! The shipped image's invariants that the Dockerfile's own comments and `droast.toml` state: one
//! Dockerfile, a pinned frontend, the deps, build and runtime stages from tag-pinned bases, the
//! version stamps, the runtime posture, and the seven OCI labels `droast.toml` requires. The Rust
//! side of `test/dockerfiles`, which stays until the cutover.
//!
//! The image still builds the Go daemon, so the stage assertions are Go's (`go mod download`,
//! the `-ldflags` stamps); SSE's Rust build stage, version stamps and platform checks come with
//! the Rust image. `TestDroastRequiresTheLabels` ran droast; here the same rule is read from the
//! files, so no tool is needed: `droast.toml` requires exactly the runtime stage's labels, with
//! `strict-labels` refusing any other, and each negative control drops one side of that.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use meta::dockerfile::{self, Stage};
use meta::repo;
use toml::Table;

const SHIPPED_DOCKERFILE: &str = "deployments/docker/Dockerfile";
const DROAST_CONFIG: &str = "droast.toml";
const BINARY: &str = "/usr/local/bin/swarm-device-access";
const CREATED_LABEL: &str = "org.opencontainers.image.created";

fn shipped_stages() -> Vec<Stage> {
    let stages = dockerfile::parse(&repo::read(SHIPPED_DOCKERFILE))
        .unwrap_or_else(|error| panic!("{SHIPPED_DOCKERFILE}: {error}"));
    assert_eq!(
        stages.len(),
        3,
        "{SHIPPED_DOCKERFILE} has {} stages, want deps, build and runtime",
        stages.len()
    );
    stages
}

fn is_dockerfile(name: &str) -> bool {
    name == "Dockerfile" || name.ends_with(".Dockerfile")
}

/// Every Dockerfile in the repository, skipping installed and generated trees: hidden
/// directories (.git, the tool caches, a nested worktree under .claude/worktrees, whose
/// Dockerfile is not this tree's), dist, vendor and cargo's target. Walking rather than asking
/// git finds an untracked one too.
fn all_dockerfiles() -> Vec<String> {
    let mut found = Vec::new();
    let mut pending = vec![String::new()];
    while let Some(directory) = pending.pop() {
        let path = if directory.is_empty() {
            repo::root()
        } else {
            repo::path(&directory)
        };
        let entries =
            fs::read_dir(&path).unwrap_or_else(|error| panic!("list {directory:?}: {error}"));
        for entry in entries {
            let entry = entry.unwrap_or_else(|error| panic!("list {directory:?}: {error}"));
            let name = entry.file_name().to_string_lossy().into_owned();
            let relative = if directory.is_empty() {
                name.clone()
            } else {
                format!("{directory}/{name}")
            };
            let kind = entry
                .file_type()
                .unwrap_or_else(|error| panic!("inspect {relative}: {error}"));
            if kind.is_dir() {
                if !name.starts_with('.') && !matches!(name.as_str(), "dist" | "vendor" | "target")
                {
                    pending.push(relative);
                }
            } else if is_dockerfile(&name) {
                found.push(relative);
            }
        }
    }
    assert!(
        found.iter().any(|path| path == SHIPPED_DOCKERFILE),
        "the walk did not find {SHIPPED_DOCKERFILE}, so it would find nothing else either"
    );
    found.sort();
    found
}

/// Go: `TestShippedDockerfileIsTheOnlyOne`. The repository holds exactly the image that ships: a
/// second Dockerfile would look shipped to every reader and be built by nothing.
#[test]
fn shipped_dockerfile_is_the_only_one() {
    assert_eq!(
        all_dockerfiles(),
        [SHIPPED_DOCKERFILE],
        "the repository holds other Dockerfiles"
    );
}

/// Go: `TestSyntaxDirectiveIsPinned`. The `BuildKit` frontend that reads the Dockerfile is a
/// literal reference pinned by digest: a bare docker/dockerfile:1.7 is a tag that can be moved
/// under a build, and the Dockerfile says the line is updated by hand.
#[test]
fn syntax_directive_is_pinned() {
    let syntax = dockerfile::syntax(&repo::read(SHIPPED_DOCKERFILE))
        .expect("the Dockerfile has a syntax parser directive on its first line");
    assert_eq!(
        dockerfile::repository(&syntax),
        "docker/dockerfile",
        "{syntax}"
    );
    assert!(
        dockerfile::digest_pinned(&syntax),
        "the frontend {syntax:?} is not pinned by digest"
    );
}

/// Go: `TestShippedStages`. The three stages, in order: deps and the unnamed runtime stage from
/// literal bases pinned by tag and not by digest (dhi.io republishes its tags with security
/// fixes, and a rebuild should pick those up), and build from deps, whose module download it
/// reuses.
#[test]
fn shipped_stages_are_deps_build_and_runtime() {
    let stages = shipped_stages();
    let names: Vec<&str> = stages.iter().map(|stage| stage.name.as_str()).collect();
    assert_eq!(
        names,
        ["deps", "build", ""],
        "want deps, build and the unnamed runtime stage, in order"
    );
    for stage in [&stages[0], &stages[2]] {
        assert!(
            dockerfile::tag_pinned(&stage.image),
            "stage {:?} is FROM {:?}, which is not a literal reference pinned by tag",
            stage.name,
            stage.image
        );
    }
    assert_eq!(
        stages[1].image, "deps",
        "the build stage starts from deps, whose module download it reuses"
    );
}

/// Go: `TestBuildStages`. deps runs on the build platform from dhi.io/golang and downloads the
/// modules; build compiles the static binary with the version stamps the daemon reports.
#[test]
fn build_stages() {
    let stages = shipped_stages();
    let (deps, build) = (&stages[0], &stages[1]);
    assert_eq!(
        deps.platform, "$BUILDPLATFORM",
        "the deps stage's --platform"
    );
    assert_eq!(
        dockerfile::repository(&deps.image),
        "dhi.io/golang",
        "the deps stage's base"
    );
    assert!(
        deps.joined().contains("go mod download"),
        "the deps stage does not download the modules:\n{}",
        deps.joined()
    );

    let instructions = build.joined();
    let missing: Vec<&str> = [
        "ARG VERSION",
        "ARG COMMIT",
        "ARG DATE",
        "ARG TARGETOS",
        "ARG TARGETARCH",
        "CGO_ENABLED=0",
        "-trimpath",
        "-buildvcs=false",
        "-X 'main.version=${VERSION}'",
        "-X 'main.commit=${COMMIT}'",
        "-X 'main.date=${DATE}'",
        "-o /out/swarm-device-access ./cmd/swarm-device-access",
    ]
    .into_iter()
    .filter(|want| !instructions.contains(want))
    .collect();
    assert!(
        missing.is_empty(),
        "the build stage lacks {missing:?}:\n{instructions}"
    );
}

/// Go: `TestRuntimeStage`. The posture of the image that ships: no shell to run anything in, root
/// (the daemon writes BPF programs and cgroup files), the binary as the entrypoint, and the seven
/// OCI labels.
#[test]
fn runtime_stage() {
    let runtime = shipped_stages().swap_remove(2);
    assert_eq!(
        dockerfile::repository(&runtime.image),
        "dhi.io/static",
        "the runtime stage's base"
    );
    assert!(
        runtime.platform.is_empty(),
        "the runtime stage pins --platform={}; it has to follow the target platform",
        runtime.platform
    );

    let mut seen: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for instruction in &runtime.instructions {
        assert!(
            !matches!(
                instruction.keyword.as_str(),
                "RUN" | "SHELL" | "VOLUME" | "ADD"
            ),
            "the runtime stage carries {} {}; the base has no shell",
            instruction.keyword,
            instruction.args
        );
        seen.entry(instruction.keyword.as_str())
            .or_default()
            .push(instruction.args.as_str());
    }
    let entrypoint = format!(r#"["{BINARY}"]"#);
    let copy = format!("--from=build /out/swarm-device-access {BINARY}");
    for (keyword, args) in [
        ("USER", "0"),
        ("ENTRYPOINT", entrypoint.as_str()),
        ("COPY", copy.as_str()),
    ] {
        assert_eq!(
            seen.get(keyword).map(Vec::as_slice),
            Some([args].as_slice()),
            "the runtime stage's {keyword}, want exactly {keyword} {args}"
        );
    }

    let labels = dockerfile::labels(&runtime);
    let description = labels
        .get("org.opencontainers.image.description")
        .cloned()
        .unwrap_or_default();
    assert!(
        !description.is_empty(),
        "the runtime stage has no org.opencontainers.image.description"
    );
    let want: BTreeMap<String, String> = [
        ("org.opencontainers.image.title", "swarm-device-access"),
        ("org.opencontainers.image.description", description.as_str()),
        (
            "org.opencontainers.image.source",
            "https://github.com/leinardi/swarm-device-access",
        ),
        ("org.opencontainers.image.version", "${VERSION}"),
        ("org.opencontainers.image.revision", "${COMMIT}"),
        (CREATED_LABEL, "${DATE}"),
        ("org.opencontainers.image.licenses", "Apache-2.0"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect();
    assert_eq!(labels, want, "want exactly the seven OCI labels");
}

/// What is wrong with the label policy: `droast.toml` must refuse any label outside its
/// `[required-labels]` (`strict-labels = true`), and require exactly the labels the runtime stage
/// of `dockerfile` carries, so droast fails (DF074) on a Dockerfile that loses one.
fn label_policy_problems(dockerfile_text: &str, droast: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let config: Table = match droast.parse() {
        Ok(config) => config,
        Err(error) => return vec![format!("parse {DROAST_CONFIG}: {error}")],
    };
    if config.get("strict-labels").and_then(toml::Value::as_bool) != Some(true) {
        problems.push(format!(
            "{DROAST_CONFIG} does not set strict-labels = true, so droast accepts any label"
        ));
    }
    let required: BTreeSet<String> = config
        .get("required-labels")
        .and_then(toml::Value::as_table)
        .map(|labels| labels.keys().cloned().collect())
        .unwrap_or_default();
    if required.is_empty() {
        problems.push(format!("{DROAST_CONFIG} requires no label"));
    }
    let carried: BTreeSet<String> = match dockerfile::parse(dockerfile_text) {
        Ok(stages) => stages
            .last()
            .map(|runtime| dockerfile::labels(runtime).into_keys().collect())
            .unwrap_or_default(),
        Err(error) => return vec![format!("{SHIPPED_DOCKERFILE}: {error}")],
    };
    for label in required.difference(&carried) {
        problems.push(format!(
            "{DROAST_CONFIG} requires {label}, which the runtime stage does not carry"
        ));
    }
    for label in carried.difference(&required) {
        problems.push(format!(
            "the runtime stage carries {label}, which {DROAST_CONFIG} does not require"
        ));
    }
    problems
}

/// Go: `TestDroastRequiresTheLabels`, read from the files: `droast.toml` requires the runtime
/// stage's labels and only those.
#[test]
fn droast_requires_the_labels() {
    let problems =
        label_policy_problems(&repo::read(SHIPPED_DOCKERFILE), &repo::read(DROAST_CONFIG));
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// Go: `TestDroastRequiresTheLabels`'s negative control, which dropped the created label and
/// expected DF074: a Dockerfile without it, a policy without it, and a policy that no longer
/// refuses other labels are each refused.
#[test]
fn label_policy_refuses_a_missing_label() {
    let content = repo::read(SHIPPED_DOCKERFILE);
    let droast = repo::read(DROAST_CONFIG);
    let label_line = content
        .lines()
        .find(|line| line.trim_start().starts_with(CREATED_LABEL))
        .expect("the created label line; the negative control would test nothing");
    let without_label = content.replace(&format!("{label_line}\n"), "");
    assert_ne!(without_label, content, "the created label was not removed");

    let required_line = droast
        .lines()
        .find(|line| line.starts_with(&format!("\x22{CREATED_LABEL}\x22")))
        .expect("the created label in droast.toml");
    let policy_without_label = droast.replace(&format!("{required_line}\n"), "");
    let not_strict = droast.replace("strict-labels = true", "strict-labels = false");
    assert_ne!(not_strict, droast, "strict-labels was not turned off");

    for (name, dockerfile_text, policy) in [
        (
            "a Dockerfile without the label",
            without_label.as_str(),
            droast.as_str(),
        ),
        (
            "a policy without the label",
            content.as_str(),
            policy_without_label.as_str(),
        ),
        (
            "a policy without strict-labels",
            content.as_str(),
            not_strict.as_str(),
        ),
    ] {
        assert!(
            !label_policy_problems(dockerfile_text, policy).is_empty(),
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
            "dhi.io/golang:${GO_LINE}-alpine3.23-dev".to_owned(),
        ),
        ("a variable image", "$BASE".to_owned()),
        ("no tag", "dhi.io/static".to_owned()),
        ("latest", "dhi.io/static:latest".to_owned()),
        ("a digest", format!("dhi.io/static:20250419{digest}")),
        ("a stage name, not a ref", "deps".to_owned()),
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
