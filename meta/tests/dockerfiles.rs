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
//! Dockerfile, a pinned frontend, the build and runtime stages from tag-pinned dhi.io bases, the
//! Rust build stage and its version stamps, the runtime posture (root, no healthcheck, no exposed
//! port), the platforms, and the seven OCI labels `droast.toml` requires. The rules are
//! `repo_meta::image`'s, with this repository's parameters. Each rule has its negative control,
//! most of them edits of the real Dockerfile, Makefile, workflows and `droast.toml`.
//!
//! `TestDroastRequiresTheLabels` ran droast; here the same rule is read from the files, so no tool
//! is needed: `droast.toml` requires exactly the runtime stage's labels, with `strict-labels`
//! refusing any other, and each negative control drops one side of that.

use repo_meta::dockerfile::{self, Stage};
use repo_meta::image::{
    BasePolicy, BuildBase, BuildStage, Download, RuntimeStage, STAMP_FIELDS, StampSources, Stamps,
    build_stage_problems, dockerfiles, label_policy_problems, label_problems,
    only_dockerfile_problems, platform_problems, runtime_problems, stage_problems, syntax_problems,
};
use repo_meta::scratch::ScratchRepo;
use repo_meta::toolchain::{make_value, rust_release};
use repo_meta::{Problem, report, strings};

const SHIPPED_DOCKERFILE: &str = "deployments/docker/Dockerfile";
const DROAST_CONFIG: &str = "droast.toml";
const BINARY: &str = "/usr/local/bin/swarm-device-access";
const CREATED_LABEL: &str = "org.opencontainers.image.created";
/// The one record of the zig, cargo-zigbuild and rust-std pins the build stage downloads.
const RUST_RELEASE_FRAGMENT: &str = ".mk/rust-release.mk";
const WORKFLOWS_DIR: &str = ".github/workflows";
/// The workflow whose `PLATFORMS` the image is built for.
const RELEASE_WORKFLOW: &str = ".github/workflows/release.yaml";
const WANT_PLATFORMS: &str = "linux/amd64,linux/arm64";
/// dhi.io/static has no arm/v7, and no 386.
const FORBIDDEN_PLATFORMS: &[&str] = &["linux/arm/v", "linux/386"];
const STAGES: &[&str] = &["build", "runtime"];

/// The version stamps src/version.rs reads, set in the RUN that runs cargo zigbuild.
const STAMPS: Stamps<'static> = Stamps {
    prefix: "SWARM_DEVICE_ACCESS",
    fields: &STAMP_FIELDS,
    version_file: "src/version.rs",
    build_command: "cargo zigbuild",
};

fn shipped() -> String {
    meta::repo().read(SHIPPED_DOCKERFILE)
}

/// The stage named `name` of `content`, which must be the only one.
fn stage(content: &str, name: &str) -> Stage {
    let stages =
        dockerfile::parse(content).unwrap_or_else(|error| panic!("{SHIPPED_DOCKERFILE}: {error}"));
    dockerfile::stage_named(SHIPPED_DOCKERFILE, &stages, name)
        .cloned()
        .unwrap_or_else(|problem| panic!("{problem}"))
}

/// The value the release fragment records for `variable`, as make reads it.
fn recorded(fragment: &str, variable: &str) -> String {
    make_value(fragment, variable)
        .unwrap_or_else(|| panic!("{RUST_RELEASE_FRAGMENT} does not set {variable}"))
}

/// The arms a download must have: each `(platform, asset)` with the sha256 the release fragment
/// records for that asset under `prefix`, so an arm whose digest is not the record's is refused
/// here as well as by toolchain.rs's `release_pin_problems`.
fn arms(fragment: &str, prefix: &str, pairs: &[(&str, &str)]) -> Vec<(String, String, String)> {
    pairs
        .iter()
        .map(|(platform, asset)| {
            (
                (*platform).to_owned(),
                (*asset).to_owned(),
                recorded(fragment, &format!("{prefix}{asset}")),
            )
        })
        .collect()
}

/// The build stage's three downloads: zig and cargo-zigbuild for the build platform, and the
/// target platform's musl standard library, each at the release and with the digests the release
/// fragment records; each in its own RUN, chosen by its platform argument's `case` with exactly
/// these arms, and checked with `sha256sum -c -`.
fn downloads(fragment: &str) -> Vec<Download<'static>> {
    vec![
        Download {
            tool: "zig",
            url: "https://ziglang.org/download/${ZIG_VERSION}/zig-${zig_arch}-linux-${ZIG_VERSION}.tar.xz",
            platform_arg: "BUILDARCH",
            version_arg: "ZIG_VERSION",
            version: recorded(fragment, "RUST_RELEASE_ZIG_VERSION"),
            asset_var: "zig_arch",
            sha_var: "zig_sha256",
            arms: arms(
                fragment,
                "RUST_RELEASE_ZIG_SHA256_",
                &[("amd64", "x86_64"), ("arm64", "aarch64")],
            ),
        },
        Download {
            tool: "cargo-zigbuild",
            url: "https://github.com/rust-cross/cargo-zigbuild/releases/download/v${CARGO_ZIGBUILD_VERSION}/cargo-zigbuild-${zigbuild_asset}.tar.xz",
            platform_arg: "BUILDARCH",
            version_arg: "CARGO_ZIGBUILD_VERSION",
            version: recorded(fragment, "RUST_RELEASE_CARGO_ZIGBUILD_VERSION"),
            asset_var: "zigbuild_asset",
            sha_var: "zigbuild_sha256",
            arms: arms(
                fragment,
                "RUST_RELEASE_CARGO_ZIGBUILD_SHA256_",
                &[
                    ("amd64", "x86_64-unknown-linux-musl"),
                    ("arm64", "aarch64-unknown-linux-gnu"),
                ],
            ),
        },
        Download {
            tool: "the musl rust-std",
            url: "https://static.rust-lang.org/dist/rust-std-${RUST_VERSION}-${triple}.tar.xz",
            platform_arg: "TARGETARCH",
            version_arg: "RUST_VERSION",
            version: recorded(fragment, "RUST_RELEASE_RUST_STD_VERSION"),
            asset_var: "triple",
            sha_var: "std_sha256",
            arms: arms(
                fragment,
                "RUST_RELEASE_RUST_STD_SHA256_",
                &[
                    ("amd64", "x86_64-unknown-linux-musl"),
                    ("arm64", "aarch64-unknown-linux-musl"),
                ],
            ),
        },
    ]
}

/// The exact release rust-toolchain.toml pins.
fn release() -> String {
    rust_release(
        "rust-toolchain.toml",
        &meta::repo().read("rust-toolchain.toml"),
    )
    .unwrap_or_else(|problem| panic!("{problem}"))
}

/// The build stage's rules against `dockerfile`: on the build platform, from the debian variant of
/// the exact toolchain release (whose toolchain takes the upstream musl standard libraries),
/// cross-compiled by cargo-zigbuild to the target platform's static musl triple from the locked
/// dependency graph, with every download a rule's and checked in its RUN.
fn build_problems(dockerfile: &str) -> Vec<Problem> {
    let base = format!("dhi.io/rust:{}-debian13-dev", release());
    let downloads = downloads(&meta::repo().read(RUST_RELEASE_FRAGMENT));
    let rules = BuildStage {
        platform: "$BUILDPLATFORM",
        base: BuildBase::Exact(&base),
        required: &[
            r#"cargo zigbuild --release --locked --target "${triple}" --bin swarm-device-access"#,
            r#"cp "target/${triple}/release/swarm-device-access" /out/swarm-device-access"#,
        ],
        downloads: &downloads,
    };
    build_stage_problems(&stage(dockerfile, "build"), &rules)
}

/// The runtime stage's rules: from dhi.io/static on the target platform, no shell to run anything
/// in and nothing to write to, root (the daemon writes BPF programs and cgroup files), the binary
/// as the entrypoint, no healthcheck and no exposed port (the metrics listener is off by default,
/// and the launcher's daemon has no network), and exactly the seven OCI labels.
fn runtime_stage_problems(dockerfile: &str) -> Vec<Problem> {
    let entrypoint = format!(r#"["{BINARY}"]"#);
    let copy = format!("--from=build /out/swarm-device-access {BINARY}");
    let rules = RuntimeStage {
        repository: "dhi.io/static",
        forbidden: &[
            "RUN",
            "SHELL",
            "VOLUME",
            "WORKDIR",
            "ADD",
            "ENV",
            "HEALTHCHECK",
            "EXPOSE",
        ],
        exactly: &[("USER", "0"), ("ENTRYPOINT", &entrypoint), ("COPY", &copy)],
    };
    let runtime = stage(dockerfile, "runtime");
    let mut problems = runtime_problems(&runtime, &rules);
    problems.extend(label_problems(
        &runtime,
        &[
            (
                "org.opencontainers.image.title",
                Some("swarm-device-access"),
            ),
            ("org.opencontainers.image.description", None),
            (
                "org.opencontainers.image.source",
                Some("https://github.com/leinardi/swarm-device-access"),
            ),
            ("org.opencontainers.image.version", Some("${VERSION}")),
            ("org.opencontainers.image.revision", Some("${COMMIT}")),
            (CREATED_LABEL, Some("${DATE}")),
            ("org.opencontainers.image.licenses", Some("Apache-2.0")),
        ],
    ));
    problems
}

/// The stamps of `dockerfile` and `makefile`, against src/version.rs, make-common's rust.mk and
/// the release fragment's static build recipe.
fn stamp_problems(dockerfile: &str, makefile: &str) -> Vec<Problem> {
    stamp_problems_with(
        dockerfile,
        makefile,
        &meta::repo().read(STAMPS.version_file),
    )
}

/// [`stamp_problems`], against the version module `version_rs`.
fn stamp_problems_with(dockerfile: &str, makefile: &str, version_rs: &str) -> Vec<Problem> {
    let repo = meta::repo();
    let build = stage(dockerfile, "build");
    STAMPS.problems(&StampSources {
        version_rs,
        build: &build,
        makefile,
        rust_mk: &repo.read(".mk/rust.mk"),
        release: Some((&repo.read(RUST_RELEASE_FRAGMENT), "rust_static_build")),
    })
}

/// Every workflow, with its text.
fn workflows() -> Vec<(String, String)> {
    let repo = meta::repo();
    repo.read_all(&repo.files_under(WORKFLOWS_DIR, &[".yaml", ".yml"]))
}

/// `content` with its first `from` replaced by `to`; `from` must be there, so an edit cannot
/// silently change nothing.
fn edit(content: &str, from: &str, to: &str) -> String {
    assert!(content.contains(from), "{from:?} is gone");
    content.replacen(from, to, 1)
}

/// Go: `TestShippedDockerfileIsTheOnlyOne`. The repository holds exactly the image that ships
/// (tracked or new, hidden directories and `Dockerfile.<name>` included): a second Dockerfile
/// would look shipped to every reader and be built by nothing.
#[test]
fn shipped_dockerfile_is_the_only_one() {
    let problems = only_dockerfile_problems(&dockerfiles(&meta::repo()), SHIPPED_DOCKERFILE);
    assert!(problems.is_empty(), "{}", report(&problems));
}

/// The negative control: a second Dockerfile beside the shipped one, tracked or new, is refused,
/// wherever it sits and however it is named.
#[test]
fn a_second_dockerfile_is_refused() {
    for (name, other, tracked) in [
        ("a tracked Dockerfile", "tools/Dockerfile", true),
        ("a new <name>.Dockerfile", "build/ci.Dockerfile", false),
        (
            "a new Dockerfile.<name>",
            ".devcontainer/Dockerfile.dev",
            false,
        ),
    ] {
        let scratch = ScratchRepo::new();
        scratch.write(SHIPPED_DOCKERFILE, "FROM scratch\n");
        scratch.write(other, "FROM scratch\n");
        scratch.git(&["add", SHIPPED_DOCKERFILE]);
        if tracked {
            scratch.git(&["add", other]);
        }
        let found = dockerfiles(&scratch.repo());
        assert!(
            !only_dockerfile_problems(&found, SHIPPED_DOCKERFILE).is_empty(),
            "{name} was accepted: {found:?}"
        );
    }
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

/// Go: `TestBuildStage`, for the Rust build: on the build platform, cross-compiled by
/// cargo-zigbuild to the target platform's static musl triple, from the locked dependency graph,
/// with every toolchain download checked against its own digest in the RUN that fetches it.
#[test]
fn build_stage() {
    let problems = build_problems(&shipped());
    assert!(problems.is_empty(), "{}", report(&problems));
}

/// The negative control of the build stage rules: each edit of the real Dockerfile below breaks
/// one, and the rules refuse it.
#[test]
fn build_stage_rules() {
    let content = shipped();
    let edit = |from: &str, to: &str| edit(&content, from, to);
    let zig_check = "\t&& echo \"${zig_sha256}  /tmp/zig.tar.xz\" | sha256sum -c - \\\n";
    for (name, broken) in [
        (
            "the target platform",
            edit("FROM --platform=$BUILDPLATFORM dhi.io", "FROM dhi.io"),
        ),
        (
            "another base",
            edit("dhi.io/rust:", "docker.io/library/rust:"),
        ),
        (
            "the alpine variant",
            edit("-debian13-dev AS build", "-alpine3.22-dev AS build"),
        ),
        (
            "an unlocked build",
            edit(
                "cargo zigbuild --release --locked",
                "cargo zigbuild --release",
            ),
        ),
        ("an unchecked download", edit(zig_check, "\\\n")),
        (
            "an extra download beside a listed one",
            edit(
                zig_check,
                &format!("{zig_check}\t&& curl -fsSL -o /tmp/x https://example.invalid/x \\\n"),
            ),
        ),
        (
            "an arm for another platform",
            edit(
                "\t\t*) echo \"no zig for",
                "\t\tarm) zig_arch=arm; zig_sha256=0000000000000000000000000000000000000000000000000000000000000000 ;; \\\n\t\t*) echo \"no zig for",
            ),
        ),
        (
            "another zig release",
            edit("ARG ZIG_VERSION=0.17.0", "ARG ZIG_VERSION=0.16.0"),
        ),
    ] {
        assert!(!build_problems(&broken).is_empty(), "{name} was accepted");
    }
}

/// Go: `TestVersionIsStamped`. The version, commit and date the binary reports come from the
/// build: the image's build stage (in the RUN that runs cargo zigbuild) and the make targets set
/// the three variables src/version.rs reads at compile time (service-kit's `buildinfo!`), from the
/// same build arguments and
/// git values; the release binaries' recipe carries them too. A path that stamped nothing would
/// ship a binary reporting "dev".
#[test]
fn version_is_stamped() {
    let problems = stamp_problems(&shipped(), &meta::repo().read("Makefile"));
    assert!(problems.is_empty(), "{}", report(&problems));
}

/// The negative control of the stamp rules on samples: a stamp dropped from each place, a version
/// module that reads none (another prefix, the call commented out, a `buildinfo!` that is not
/// service-kit's), another prefix and a rust-dist or rust-run recipe that does not stamp are each a
/// problem; an unstamped rust-build is not.
#[test]
fn stamp_rules() {
    let version_rs = "use service_kit::buildinfo;\n\
                      const BUILD: BuildInfo = buildinfo!(\n    \x22SWARM_DEVICE_ACCESS\x22,\n    D,\n);\n";
    let build = "FROM a:1 AS build\nARG VERSION\nARG COMMIT\nARG DATE\n\
                 RUN SWARM_DEVICE_ACCESS_VERSION=\x22${VERSION}\x22 \
                 SWARM_DEVICE_ACCESS_COMMIT=\x22${COMMIT}\x22 \
                 SWARM_DEVICE_ACCESS_DATE=\x22${DATE}\x22 cargo zigbuild --release\n";
    let makefile = "BIN_NAME ?= x\nRUST_STAMP_PREFIX := SWARM_DEVICE_ACCESS\n";
    let fragment = "RUST_VERSION_ENV = $(if $(strip $(RUST_STAMP_PREFIX)), \\\n\
                    \x20 $(RUST_STAMP_PREFIX)_VERSION=\x22$(GIT_VERSION)\x22 \\\n\
                    \x20 $(RUST_STAMP_PREFIX)_COMMIT=\x22$(GIT_COMMIT)\x22 \\\n\
                    \x20 $(RUST_STAMP_PREFIX)_DATE=\x22$(BUILD_DATE)\x22)\n\
                    \n\
                    rust-build: ## Build\n\
                    \t$(RUST_BUILD_ENV) $(CARGO) build --workspace\n\
                    \n\
                    rust-dist: ## Build the binary\n\
                    \t$(RUST_VERSION_ENV) $(CARGO) build --release\n\
                    \n\
                    rust-run: ## Run the binary\n\
                    \t$(RUST_VERSION_ENV) $(CARGO) run\n";
    let release_mk = "rust_static_build = RUSTFLAGS=\x22x\x22 $(RUST_VERSION_ENV) \\\n\
                      \x20 $(CARGO) zigbuild --release\n";
    let problems = |version_rs: &str, build: &str, makefile: &str, fragment: &str| {
        let stages = dockerfile::parse(build).expect("parse the sample");
        strings(&STAMPS.problems(&StampSources {
            version_rs,
            build: &stages[0],
            makefile,
            rust_mk: fragment,
            release: Some((release_mk, "rust_static_build")),
        }))
    };
    assert_eq!(
        problems(version_rs, build, makefile, fragment),
        Vec::<String>::new()
    );
    let broken = problems(
        version_rs,
        &build.replace("=\x22${COMMIT}\x22", "=none"),
        makefile,
        &fragment.replace("$(RUST_STAMP_PREFIX)_VERSION=", "VERSION="),
    );
    assert_eq!(broken.len(), 2, "{broken:#?}");
    // The version module: another prefix, the call commented out, and a buildinfo! that is not
    // service-kit's each read none of the three.
    for broken_version_rs in [
        version_rs.replace("\x22SWARM_DEVICE_ACCESS\x22", "\x22SWARM_DEVICE\x22"),
        version_rs.replace("const BUILD", "// const BUILD"),
        version_rs.replace("use service_kit::buildinfo;\n", ""),
    ] {
        assert_eq!(
            problems(&broken_version_rs, build, makefile, fragment).len(),
            3,
            "{broken_version_rs:?} was accepted"
        );
    }
    for broken_makefile in [
        makefile.replace(":= SWARM_DEVICE_ACCESS", ":= SDA"),
        makefile.replace("RUST_STAMP_PREFIX := SWARM_DEVICE_ACCESS\n", ""),
        makefile.replace("RUST_STAMP_PREFIX :=", "# RUST_STAMP_PREFIX :="),
        format!("{makefile}RUST_STAMP_PREFIX += X\n"),
    ] {
        assert_eq!(
            problems(version_rs, build, &broken_makefile, fragment).len(),
            1,
            "{broken_makefile:?} was accepted"
        );
    }
    // rust-dist and rust-run must stamp, each on its own; rust-build may build unstamped, as the
    // Makefile's `RUST_BUILD_ENV :=` has it.
    for (target, command) in [
        ("rust-dist", "$(CARGO) build --release"),
        ("rust-run", "$(CARGO) run"),
    ] {
        let unstamped = fragment.replace(
            &format!("\t$(RUST_VERSION_ENV) {command}"),
            &format!("\t{command}"),
        );
        assert_ne!(unstamped, fragment, "the {target} sample did not change");
        assert_eq!(
            problems(version_rs, build, makefile, &unstamped).len(),
            1,
            "a {target} recipe without the stamps was accepted"
        );
    }
}

/// The negative control of the stamp rules on the real files: a stamp moved out of the building
/// RUN, an undeclared argument, a version module stamped under another prefix, and a Makefile
/// whose winning prefix is another.
#[test]
fn stamp_rules_refuse_each_edit() {
    let content = shipped();
    let makefile = meta::repo().read("Makefile");
    let edit = |from: &str, to: &str| edit(&content, from, to);
    for (name, broken) in [
        (
            "a stamp outside the building RUN",
            edit("\t\tSWARM_DEVICE_ACCESS_DATE=\"${DATE}\" \\\n", "").replacen(
                "WORKDIR /src\n",
                "WORKDIR /src\nENV SWARM_DEVICE_ACCESS_DATE=\"${DATE}\"\n",
                1,
            ),
        ),
        ("an undeclared argument", edit("ARG DATE=\"unknown\"\n", "")),
    ] {
        assert!(
            !stamp_problems(&broken, &makefile).is_empty(),
            "{name} was accepted"
        );
    }
    let version_rs = meta::repo().read(STAMPS.version_file);
    let call = "\x22SWARM_DEVICE_ACCESS\x22,";
    assert!(version_rs.contains(call), "{call:?} is gone");
    let broken = version_rs.replacen(call, "\x22SWARM_DEVICE\x22,", 1);
    assert_eq!(
        stamp_problems_with(&content, &makefile, &broken).len(),
        3,
        "another buildinfo! prefix was accepted"
    );
    let prefix = "RUST_STAMP_PREFIX := SWARM_DEVICE_ACCESS";
    assert!(makefile.contains(prefix), "{prefix:?} is gone");
    for broken in [
        makefile.replacen(prefix, "RUST_STAMP_PREFIX := OTHER", 1),
        format!("{makefile}\nRUST_STAMP_PREFIX += X\n"),
    ] {
        assert!(
            !stamp_problems(&content, &broken).is_empty(),
            "{broken} was accepted"
        );
    }
}

/// Go: `TestRuntimeStage`. The posture of the image that ships.
#[test]
fn runtime_stage() {
    let problems = runtime_stage_problems(&shipped());
    assert!(problems.is_empty(), "{}", report(&problems));
}

/// The negative control of the runtime rules: a healthcheck, an exposed port, a non-root user, a
/// shell command, a pinned platform, another base and an extra label are each refused.
#[test]
fn runtime_rules() {
    let content = shipped();
    let entrypoint = format!("ENTRYPOINT [\x22{BINARY}\x22]\n");
    let edit = |from: &str, to: &str| edit(&content, from, to);
    for (name, sample) in [
        (
            "a healthcheck",
            edit(
                &entrypoint,
                &format!("HEALTHCHECK CMD [\x22{BINARY}\x22, \x22--help\x22]\n{entrypoint}"),
            ),
        ),
        (
            "an exposed port",
            edit(&entrypoint, &format!("EXPOSE 9090\n{entrypoint}")),
        ),
        ("a non-root user", edit("\nUSER 0\n", "\nUSER 65532:65532\n")),
        (
            "a shell command",
            edit(&entrypoint, &format!("RUN true\n{entrypoint}")),
        ),
        (
            "a pinned platform",
            edit(
                "FROM dhi.io/static:",
                "FROM --platform=linux/amd64 dhi.io/static:",
            ),
        ),
        (
            "another base",
            edit("FROM dhi.io/static:", "FROM dhi.io/alpine-base:"),
        ),
        (
            "an extra label",
            edit(
                "org.opencontainers.image.licenses=\"Apache-2.0\"",
                "org.opencontainers.image.licenses=\"Apache-2.0\" extra.label=\"x\"",
            ),
        ),
        (
            "an empty description",
            content.replacen(
                &format!(
                    "org.opencontainers.image.description=\"{}\"",
                    dockerfile::labels(&stage(&content, "runtime"))
                        ["org.opencontainers.image.description"]
                ),
                "org.opencontainers.image.description=\"\"",
                1,
            ),
        ),
    ] {
        assert_ne!(
            sample, content,
            "{name}: the sample is the Dockerfile itself"
        );
        assert!(
            !runtime_stage_problems(&sample).is_empty(),
            "{name} was accepted"
        );
    }
}

/// Go: `TestPlatforms`. The two platforms wherever a workflow names platforms, the release
/// workflow among them: dhi.io/static has no arm/v7, and the release binaries are built for
/// exactly these two.
#[test]
fn platforms() {
    let problems = platform_problems(
        &workflows(),
        RELEASE_WORKFLOW,
        WANT_PLATFORMS,
        FORBIDDEN_PLATFORMS,
    );
    assert!(problems.is_empty(), "{}", report(&problems));
}

/// The negative control of the platform rule: a third platform in a declaration, and an arm/v7
/// image named anywhere, are each refused; an expression is not a declaration, so a workflow
/// declaring only one declares nothing.
#[test]
fn platform_rules() {
    let check = |sample: &str| {
        strings(&platform_problems(
            &[("a.yaml".to_owned(), sample.to_owned())],
            "a.yaml",
            WANT_PLATFORMS,
            FORBIDDEN_PLATFORMS,
        ))
    };
    let good = "env:\n  PLATFORMS: linux/amd64,linux/arm64\n".to_owned();
    assert_eq!(check(&good), Vec::<String>::new());
    for (name, sample) in [
        (
            "a third platform",
            good.replace("linux/arm64", "linux/arm64,linux/s390x"),
        ),
        ("an arm/v7 image", format!("{good}  other: linux/arm/v7\n")),
        (
            "an expression only",
            "      platforms: ${{ env.PLATFORMS }}\n".to_owned(),
        ),
    ] {
        let problems = check(&sample);
        assert_eq!(problems.len(), 1, "{name}: {problems:#?}");
    }

    let workflows = workflows();
    let release = workflows
        .iter()
        .position(|(file, _)| file == RELEASE_WORKFLOW)
        .unwrap_or_else(|| panic!("no {RELEASE_WORKFLOW}"));
    let mut broken = workflows.clone();
    broken[release].1 = edit(
        &broken[release].1,
        &format!("PLATFORMS: {WANT_PLATFORMS}"),
        "PLATFORMS: linux/amd64,linux/arm64,linux/arm/v7",
    );
    assert_eq!(
        platform_problems(
            &broken,
            RELEASE_WORKFLOW,
            WANT_PLATFORMS,
            FORBIDDEN_PLATFORMS,
        )
        .len(),
        2,
        "a third platform in the release workflow"
    );
}

/// The label policy (`repo_meta::image::label_policy_problems`): `droast.toml` must refuse any
/// label outside its `[required-labels]` (`strict-labels = true`), leave DF074's severity as
/// droast ships it, and require exactly the labels the runtime stage of `dockerfile_text`
/// carries, so droast fails (DF074) on a Dockerfile that loses one.
fn droast_label_problems(dockerfile_text: &str, droast: &str) -> Vec<Problem> {
    label_policy_problems(SHIPPED_DOCKERFILE, dockerfile_text, DROAST_CONFIG, droast)
}

/// Go: `TestDroastRequiresTheLabels`, read from the files: `droast.toml` requires the runtime
/// stage's labels and only those.
#[test]
fn droast_requires_the_labels() {
    let repo = meta::repo();
    let problems = droast_label_problems(&repo.read(SHIPPED_DOCKERFILE), &repo.read(DROAST_CONFIG));
    assert!(problems.is_empty(), "{}", report(&problems));
}

/// Go: `TestDroastRequiresTheLabels`'s negative control, which dropped the created label and
/// expected DF074: a Dockerfile without it, a policy without it, a policy that no longer refuses
/// other labels, and a policy that lowers DF074's severity are each refused.
#[test]
fn label_policy_refuses_a_missing_label() {
    let content = shipped();
    let droast = meta::repo().read(DROAST_CONFIG);
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
    let df074_lowered = droast.replacen(
        "[severity-overrides]\n",
        "[severity-overrides]\nDF074 = \"info\"\n",
        1,
    );
    assert_ne!(
        df074_lowered, droast,
        "droast.toml has no [severity-overrides]"
    );

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
        (
            "a policy lowering DF074",
            content.as_str(),
            df074_lowered.as_str(),
        ),
    ] {
        assert!(
            !droast_label_problems(dockerfile_text, policy).is_empty(),
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
