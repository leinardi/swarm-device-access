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
//! Dockerfile, a pinned frontend, the build and runtime stages from tag-pinned bases, the Rust
//! build stage and its version stamps, the runtime posture (root, no healthcheck, no exposed
//! port), the platforms, and the seven OCI labels `droast.toml` requires. Each rule has its
//! negative control.
//!
//! `TestDroastRequiresTheLabels` ran droast; here the same rule is read from the files, so no tool
//! is needed: `droast.toml` requires exactly the runtime stage's labels, with `strict-labels`
//! refusing any other, and each negative control drops one side of that.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::sync::LazyLock;

use meta::dockerfile::{self, Stage};
use meta::repo;
use regex::Regex;
use toml::Table;

const SHIPPED_DOCKERFILE: &str = "deployments/docker/Dockerfile";
const DROAST_CONFIG: &str = "droast.toml";
const BINARY: &str = "/usr/local/bin/swarm-device-access";
const CREATED_LABEL: &str = "org.opencontainers.image.created";
const STAMP_FRAGMENT: &str = ".mk/rust.mk";
const WORKFLOWS_DIR: &str = ".github/workflows";
const WANT_PLATFORMS: &str = "linux/amd64,linux/arm64";

fn shipped_stages() -> Vec<Stage> {
    let stages = dockerfile::parse(&repo::read(SHIPPED_DOCKERFILE))
        .unwrap_or_else(|error| panic!("{SHIPPED_DOCKERFILE}: {error}"));
    assert_eq!(
        stages.len(),
        2,
        "{SHIPPED_DOCKERFILE} has {} stages, want build and runtime",
        stages.len()
    );
    stages
}

/// The stage named `name` of `stages`, which must be the only one.
fn stage(stages: Vec<Stage>, name: &str) -> Stage {
    let found: Vec<Stage> = stages
        .into_iter()
        .filter(|candidate| candidate.name == name)
        .collect();
    match <[Stage; 1]>::try_from(found) {
        Ok([stage]) => stage,
        Err(found) => panic!(
            "{SHIPPED_DOCKERFILE} has {} stages named {name}, want one",
            found.len()
        ),
    }
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

/// Go: `TestShippedStages`. The two stages, in order, each from a literal base pinned by tag and
/// not by digest: dhi.io republishes its tags with security fixes, and a rebuild should pick
/// those up. A variable or latest would let a build change base unseen.
#[test]
fn shipped_stages_are_build_and_runtime() {
    let stages = shipped_stages();
    for stage in &stages {
        assert!(
            dockerfile::tag_pinned(&stage.image),
            "stage {:?} is FROM {:?}, which is not a literal reference pinned by tag alone",
            stage.name,
            stage.image
        );
    }
    let names: Vec<&str> = stages.iter().map(|stage| stage.name.as_str()).collect();
    assert_eq!(names, ["build", "runtime"]);
}

/// What is wrong with the build stage: it must run on the build platform from dhi.io/rust,
/// cross-compile with cargo-zigbuild to the target platform's static musl triple from the locked
/// dependency graph, check every toolchain download against its own digest, and stamp the version
/// strings the binary reads.
fn build_stage_problems(build: &Stage) -> Vec<String> {
    let mut problems = Vec::new();
    if build.platform != "$BUILDPLATFORM" {
        problems.push(format!(
            "the build stage runs on --platform={:?}, want $BUILDPLATFORM",
            build.platform
        ));
    }
    if dockerfile::repository(&build.image) != "dhi.io/rust" {
        problems.push(format!(
            "the build stage is FROM {:?}, want dhi.io/rust",
            build.image
        ));
    }

    let instructions = build.joined();
    for want in [
        "ARG VERSION",
        "ARG COMMIT",
        "ARG DATE",
        "ARG RUST_VERSION=",
        "ARG ZIG_VERSION=",
        "ARG CARGO_ZIGBUILD_VERSION=",
        "ARG BUILDARCH",
        "ARG TARGETARCH",
        "amd64) triple=x86_64-unknown-linux-musl;",
        "arm64) triple=aarch64-unknown-linux-musl;",
        "/releases/download/v${CARGO_ZIGBUILD_VERSION}/cargo-zigbuild-${zigbuild_asset}.tar.xz",
        "https://static.rust-lang.org/dist/rust-std-${RUST_VERSION}-${triple}.tar.xz",
        r#"cargo zigbuild --release --locked --target "${triple}" --bin swarm-device-access"#,
        r#"cp "target/${triple}/release/swarm-device-access" /out/swarm-device-access"#,
    ] {
        if !instructions.contains(want) {
            problems.push(format!("the build stage lacks {want:?}"));
        }
    }

    // Every download is checked against its own digest: each curl in the stage is followed by a
    // sha256sum check of the file it wrote.
    let downloads = instructions.matches("curl -fsSL").count();
    let checks = instructions.matches("| sha256sum -c -").count();
    if downloads == 0 {
        problems.push("the build stage downloads nothing".to_owned());
    }
    if downloads != checks {
        problems.push(format!(
            "the build stage downloads {downloads} files and checks {checks}"
        ));
    }
    problems
}

/// Go: `TestBuildStage`, for the Rust build.
#[test]
fn build_stage() {
    let problems = build_stage_problems(&stage(shipped_stages(), "build"));
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// The negative control of the build stage rules: a stage on the target platform, from another
/// base, without --locked, or with a download nobody checks is each refused.
#[test]
fn build_stage_rules() {
    let content = repo::read(SHIPPED_DOCKERFILE);
    let unchecked = content.replacen(
        r#"&& echo "${zig_sha256}  /tmp/zig.tar.xz" | sha256sum -c - \"#,
        r#"&& echo "${zig_sha256}" \"#,
        1,
    );
    for (name, sample) in [
        (
            "the target platform",
            content.replace("FROM --platform=$BUILDPLATFORM dhi.io", "FROM dhi.io"),
        ),
        (
            "another base",
            content.replace("dhi.io/rust:", "docker.io/library/rust:"),
        ),
        (
            "an unlocked build",
            content.replace(
                "cargo zigbuild --release --locked",
                "cargo zigbuild --release",
            ),
        ),
        ("an unchecked download", unchecked),
    ] {
        assert_ne!(
            sample, content,
            "{name}: the sample is the Dockerfile itself"
        );
        let stages = dockerfile::parse(&sample).expect("parse the sample");
        assert!(
            !build_stage_problems(&stage(stages, "build")).is_empty(),
            "{name} was accepted"
        );
    }
}

/// What is wrong with the version stamps: `version_rs` must read each of the three variables at
/// compile time, `build` (the build stage's instructions) must set each from its build argument,
/// and `make` (the make fragment) must set each.
fn stamp_problems(version_rs: &str, build: &str, make: &str) -> Vec<String> {
    let mut problems = Vec::new();
    for field in ["VERSION", "COMMIT", "DATE"] {
        let variable = format!("SWARM_DEVICE_ACCESS_{field}");
        if !version_rs.contains(&format!("option_env!(\x22{variable}\x22)")) {
            problems.push(format!("src/version.rs does not read {variable}"));
        }
        if !build.contains(&format!("{variable}=\x22${{{field}}}\x22")) {
            problems.push(format!(
                "the build stage does not stamp {variable} from ${field}"
            ));
        }
        if !make.contains(&format!("{variable}=")) {
            problems.push(format!("{STAMP_FRAGMENT} does not stamp {variable}"));
        }
    }
    problems
}

/// Go: `TestVersionIsStamped`. The version, commit and date the binary reports come from the
/// build: the image's build stage and the make targets set the three variables src/version.rs
/// reads at compile time, from the same build arguments and git values. A path that stamped
/// nothing would ship a binary reporting "dev".
#[test]
fn version_is_stamped() {
    let problems = stamp_problems(
        &repo::read("src/version.rs"),
        &stage(shipped_stages(), "build").joined(),
        &repo::read(STAMP_FRAGMENT),
    );
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// The negative control of the stamp rules: a stamp dropped from each of the three places is a
/// problem.
#[test]
fn stamp_rules() {
    let version_rs = "stamp(option_env!(\x22SWARM_DEVICE_ACCESS_VERSION\x22))\n\
                      stamp(option_env!(\x22SWARM_DEVICE_ACCESS_COMMIT\x22))\n\
                      stamp(option_env!(\x22SWARM_DEVICE_ACCESS_DATE\x22))\n";
    let build = "SWARM_DEVICE_ACCESS_VERSION=\x22${VERSION}\x22 \
                 SWARM_DEVICE_ACCESS_COMMIT=\x22${COMMIT}\x22 \
                 SWARM_DEVICE_ACCESS_DATE=\x22${DATE}\x22";
    let make = "SWARM_DEVICE_ACCESS_VERSION=x SWARM_DEVICE_ACCESS_COMMIT=x \
                SWARM_DEVICE_ACCESS_DATE=x";
    assert_eq!(
        stamp_problems(version_rs, build, make),
        Vec::<String>::new()
    );
    let problems = stamp_problems(
        &version_rs.replace("ACCESS_DATE", "ACCESS_WHEN"),
        &build.replace("${COMMIT}", "none"),
        &make.replace("SWARM_DEVICE_ACCESS_VERSION=x ", ""),
    );
    assert_eq!(problems.len(), 3, "{problems:#?}");
}

/// What is wrong with the runtime stage: from dhi.io/static on the target platform, no shell to
/// run anything in and nothing to write to, root (the daemon writes BPF programs and cgroup
/// files), the binary as the entrypoint, no healthcheck and no exposed port (the metrics listener
/// is off by default, and the launcher's daemon has no network), and the seven OCI labels.
fn runtime_problems(runtime: &Stage) -> Vec<String> {
    let mut problems = Vec::new();
    if dockerfile::repository(&runtime.image) != "dhi.io/static" {
        problems.push(format!(
            "the runtime stage is FROM {:?}, want dhi.io/static",
            runtime.image
        ));
    }
    if !runtime.platform.is_empty() {
        problems.push(format!(
            "the runtime stage pins --platform={}; it has to follow the target platform",
            runtime.platform
        ));
    }

    let mut seen: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for instruction in &runtime.instructions {
        if matches!(
            instruction.keyword.as_str(),
            "RUN" | "SHELL" | "VOLUME" | "WORKDIR" | "ADD" | "ENV" | "HEALTHCHECK" | "EXPOSE"
        ) {
            problems.push(format!(
                "the runtime stage carries {} {}; it has no shell, writes nothing, and has no \
                 healthcheck or exposed port",
                instruction.keyword, instruction.args
            ));
        }
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
        if seen.get(keyword).map(Vec::as_slice) != Some([args].as_slice()) {
            problems.push(format!(
                "the runtime stage's {keyword} is {:?}, want exactly {keyword} {args}",
                seen.get(keyword)
            ));
        }
    }

    let labels = dockerfile::labels(runtime);
    let description = labels
        .get("org.opencontainers.image.description")
        .cloned()
        .unwrap_or_default();
    if description.is_empty() {
        problems.push("the runtime stage has no org.opencontainers.image.description".to_owned());
    }
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
    if labels != want {
        problems.push(format!(
            "the runtime stage's labels are {labels:?}, want exactly the seven OCI labels"
        ));
    }
    problems
}

/// Go: `TestRuntimeStage`. The posture of the image that ships.
#[test]
fn runtime_stage() {
    let problems = runtime_problems(&stage(shipped_stages(), "runtime"));
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// The negative control of the runtime rules: a healthcheck, an exposed port, a non-root user, a
/// shell command and a pinned platform are each refused.
#[test]
fn runtime_rules() {
    let content = repo::read(SHIPPED_DOCKERFILE);
    let entrypoint = format!("ENTRYPOINT [\x22{BINARY}\x22]\n");
    assert!(
        content.contains(&entrypoint),
        "the Dockerfile's entrypoint line; the negative control would test nothing"
    );
    for (name, sample) in [
        (
            "a healthcheck",
            content.replace(
                &entrypoint,
                &format!("HEALTHCHECK CMD [\x22{BINARY}\x22, \x22--help\x22]\n{entrypoint}"),
            ),
        ),
        (
            "an exposed port",
            content.replace(&entrypoint, &format!("EXPOSE 9090\n{entrypoint}")),
        ),
        (
            "a non-root user",
            content.replace("\nUSER 0\n", "\nUSER 65532:65532\n"),
        ),
        (
            "a shell command",
            content.replace(&entrypoint, &format!("RUN true\n{entrypoint}")),
        ),
        (
            "a pinned platform",
            content.replace(
                "FROM dhi.io/static:",
                "FROM --platform=linux/amd64 dhi.io/static:",
            ),
        ),
    ] {
        assert_ne!(
            sample, content,
            "{name}: the sample is the Dockerfile itself"
        );
        let stages = dockerfile::parse(&sample).expect("parse the sample");
        assert!(
            !runtime_problems(&stage(stages, "runtime")).is_empty(),
            "{name} was accepted"
        );
    }
}

/// A `platforms:` or `PLATFORMS:` declaration in a workflow, with its value.
static PLATFORM_DECLARATION: LazyLock<Regex> = LazyLock::new(|| {
    let pattern = r"(?mi)^\s*(?:-\s*)?(platforms?|PLATFORMS)\s*:\s*(.+)$";
    Regex::new(pattern).unwrap_or_else(|error| panic!("invalid pattern {pattern}: {error}"))
});

/// The declarations of `workflows` (path and content) that name other platforms than
/// [`WANT_PLATFORMS`], and how many literal declarations they hold.
fn platform_problems(workflows: &[(String, String)]) -> (usize, Vec<String>) {
    let mut declared = 0;
    let mut problems = Vec::new();
    for (workflow, content) in workflows {
        for captures in PLATFORM_DECLARATION.captures_iter(content) {
            let value = captures[2].trim().trim_matches(['"', '\'']);
            if value.starts_with("${{") {
                continue;
            }
            declared += 1;
            if value != WANT_PLATFORMS {
                problems.push(format!(
                    "{workflow} declares {} {value:?}, want {WANT_PLATFORMS:?}",
                    &captures[1]
                ));
            }
        }
        if content.contains("linux/arm/v") || content.contains("linux/386") {
            problems.push(format!(
                "{workflow} names a platform other than {WANT_PLATFORMS}"
            ));
        }
    }
    (declared, problems)
}

/// Go: `TestPlatforms`. The two platforms wherever a workflow names platforms: dhi.io/static has
/// no arm/v7, and the release binaries are built for exactly these two.
#[test]
fn platforms() {
    let workflows: Vec<(String, String)> = repo::files_under(WORKFLOWS_DIR, &[".yaml", ".yml"])
        .into_iter()
        .map(|path| {
            let content = repo::read(&path);
            (path, content)
        })
        .collect();
    let (declared, problems) = platform_problems(&workflows);
    assert!(
        declared > 0,
        "no workflow declares its platforms; the check would pass on nothing"
    );
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// The negative control of the platform rule: a third platform in a declaration, and an arm/v7
/// image named anywhere, are each refused; an expression is not a declaration.
#[test]
fn platform_rules() {
    let good = "env:\n  PLATFORMS: linux/amd64,linux/arm64\n".to_owned();
    let (declared, problems) = platform_problems(&[("a.yaml".to_owned(), good.clone())]);
    assert_eq!((declared, problems.len()), (1, 0), "{problems:#?}");
    for (name, sample) in [
        (
            "a third platform",
            good.replace("linux/arm64", "linux/arm64,linux/s390x"),
        ),
        ("an arm/v7 image", format!("{good}  other: linux/arm/v7\n")),
    ] {
        let (_, problems) = platform_problems(&[("a.yaml".to_owned(), sample)]);
        assert_eq!(problems.len(), 1, "{name}: {problems:#?}");
    }
    let (declared, _) = platform_problems(&[(
        "a.yaml".to_owned(),
        "      platforms: ${{ env.PLATFORMS }}\n".to_owned(),
    )]);
    assert_eq!(declared, 0, "an expression was read as a declaration");
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
