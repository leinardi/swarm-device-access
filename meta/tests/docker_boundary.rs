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

// Adapted from swarm-scheduler-exporter/meta/tests/docker_boundary.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The daemon's Docker client is read-only (`docs/rust-port.md`, the Docker transport): it is
//! rust-common's `docker-engine`, reached through `src/docker` alone, and the daemon's trait has
//! the ping, list, inspect and events calls only. This pins it on the manifests and on the text of
//! the non-test sources under `src/`:
//!
//! - `docker-engine` is the sanctioned client: only the root package declares it, once in
//!   `[dependencies]` with `write` and `attach` at most (the launcher's calls) and in
//!   `[dev-dependencies]` with `test-util` besides; never `tcp` or `tls`, so the client dials the
//!   configured unix socket only and reads no `DOCKER_*` variable;
//! - only `src/docker` names the crate, and there no request is built, no method named or set and
//!   no write (`exchange`, `exchange_upgrade`) called: `DockerApi` is built on the client's
//!   `GET`s and its ping;
//! - `DockerApi` declares exactly `ping`, `container_list`, `container_inspect` and `events`, and
//!   asks only for the daemon's paths: `/_ping`, `/containers/json`, `/containers/{id}/json` and
//!   `/events`;
//! - nothing under `src/docker` reads an environment variable or dials TCP, and nothing under
//!   `src/` builds the client from the environment (`Settings::from_env`);
//! - nothing under `src/` names bollard, `testenv`, the harness that creates and removes
//!   containers on purpose, or `testenv_core`, its shared half, whose `bollard` feature brings the
//!   SDK.
//!
//! The launcher's write calls take docker-engine's client through `Client::transport` and set
//! their method outside `src/docker`, in the launcher's engine; who may do that is pinned by
//! `launcher_boundary.rs`.
//!
//! The checks are functions of the text, and each is also run on input built to break it.

use std::sync::LazyLock;

use meta::repo;
use regex::Regex;
use toml::{Table, Value};

/// Where the trait lives.
const API: &str = "src/docker/api.rs";
/// The trait's calls, in declaration order.
const CALLS: &[&str] = &["ping", "container_list", "container_inspect", "events"];
/// The paths the daemon reads, `{id}` standing for the container.
const PATHS: &[&str] = &[
    "/_ping",
    "/containers/json",
    "/containers/{id}/json",
    "/events",
];
/// The sanctioned Docker client.
const CLIENT: &str = "docker-engine";
/// What the root package may enable: in its `[dependencies]` (the launcher's writes and attach),
/// and in its `[dev-dependencies]`.
const CLIENT_FEATURES: &[&str] = &["write", "attach"];
const CLIENT_TEST_FEATURES: &[&str] = &["write", "attach", "test-util"];
const DEPENDENCY_TABLES: &[&str] = &["dependencies", "dev-dependencies", "build-dependencies"];

fn pattern(text: &str) -> Regex {
    Regex::new(text).unwrap_or_else(|error| panic!("pattern {text}: {error}"))
}

/// A function declaration, capturing its name.
static FUNCTION_NAME: LazyLock<Regex> = LazyLock::new(|| pattern(r"\bfn\s+(\w+)"));

/// The text without its line comments, so a comment naming a method is not counted; the line
/// numbers stay.
fn code(text: &str) -> String {
    text.lines()
        .map(|line| {
            if line.trim_start().starts_with("//") {
                ""
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn line_of(text: &str, offset: usize) -> usize {
    text[..offset].matches('\n').count() + 1
}

/// Every place `regex` matches in the files `selected` accepts, as `path:line: match`.
fn found_in(
    files: &[(String, String)],
    regex: &Regex,
    selected: impl Fn(&str) -> bool,
) -> Vec<String> {
    files
        .iter()
        .filter(|(path, _)| selected(path))
        .flat_map(|(path, text)| {
            regex.find_iter(text).map(move |found| {
                format!(
                    "{path}:{}: {}",
                    line_of(text, found.start()),
                    found.as_str()
                )
            })
        })
        .collect()
}

fn in_docker(path: &str) -> bool {
    path.starts_with("src/docker/")
}

/// One dependency entry: the table it sits in (`dependencies`, `target.'cfg(unix)'.dependencies`,
/// `workspace.dependencies`), the crate it names (its `package` if renamed), and the entry.
struct Dependency {
    table: String,
    crate_name: String,
    entry: Value,
}

/// Every dependency entry of a manifest, target-specific and workspace tables included.
fn dependencies(manifest: &Table) -> Vec<Dependency> {
    let mut tables: Vec<(String, &Table)> = Vec::new();
    for name in DEPENDENCY_TABLES {
        if let Some(table) = manifest.get(*name).and_then(Value::as_table) {
            tables.push(((*name).to_owned(), table));
        }
    }
    if let Some(targets) = manifest.get("target").and_then(Value::as_table) {
        for (target, contents) in targets {
            for name in DEPENDENCY_TABLES {
                if let Some(table) = contents.get(*name).and_then(Value::as_table) {
                    tables.push((format!("target.{target}.{name}"), table));
                }
            }
        }
    }
    if let Some(table) = manifest
        .get("workspace")
        .and_then(|workspace| workspace.get("dependencies"))
        .and_then(Value::as_table)
    {
        tables.push(("workspace.dependencies".to_owned(), table));
    }
    tables
        .into_iter()
        .flat_map(|(table, entries)| {
            entries.iter().map(move |(key, entry)| Dependency {
                table: table.clone(),
                crate_name: entry
                    .get("package")
                    .and_then(Value::as_str)
                    .unwrap_or(key)
                    .to_owned(),
                entry: entry.clone(),
            })
        })
        .collect()
}

/// The client's manifest rules over `(path, text)` pairs, the root manifest at `Cargo.toml`: the
/// workspace pins [`CLIENT`] without features (features unify across a workspace build), only the
/// root package declares it, once in `[dependencies]` with [`CLIENT_FEATURES`] at most and in its
/// `[dev-dependencies]` with [`CLIENT_TEST_FEATURES`] at most.
fn manifest_problems(manifests: &[(String, String)]) -> Vec<String> {
    let mut problems = Vec::new();
    let mut declared = 0;
    for (path, text) in manifests {
        let manifest: Table = text
            .parse()
            .unwrap_or_else(|error| panic!("parse {path}: {error}"));
        for dependency in dependencies(&manifest) {
            if dependency.crate_name != CLIENT {
                continue;
            }
            let place = format!("{path} [{}] {CLIENT}", dependency.table);
            let features: Vec<&str> = dependency
                .entry
                .get("features")
                .and_then(Value::as_array)
                .map(|features| features.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let allowed: &[&str] = match (path.as_str(), dependency.table.as_str()) {
                ("Cargo.toml", "workspace.dependencies") => &[],
                ("Cargo.toml", "dependencies") => {
                    declared += 1;
                    CLIENT_FEATURES
                }
                ("Cargo.toml", "dev-dependencies") => CLIENT_TEST_FEATURES,
                _ => {
                    problems.push(format!(
                        "{place}: only the root package declares {CLIENT}, in [dependencies] \
                         and [dev-dependencies]"
                    ));
                    continue;
                }
            };
            for feature in features {
                if !allowed.contains(&feature) {
                    problems.push(format!(
                        "{place}: feature `{feature}` is not one of {allowed:?}; the client \
                         dials the configured unix socket only"
                    ));
                }
            }
        }
    }
    if declared != 1 {
        problems.push(format!(
            "Cargo.toml declares {CLIENT} {declared} times in [dependencies], want once"
        ));
    }
    // A package feature forwarding one of the client's features would turn on what the entries
    // above refuse.
    for (path, text) in manifests {
        let manifest: Table = text
            .parse()
            .unwrap_or_else(|error| panic!("parse {path}: {error}"));
        let Some(features) = manifest.get("features").and_then(Value::as_table) else {
            continue;
        };
        for (name, enables) in features {
            let enables = enables.as_array().map(Vec::as_slice).unwrap_or_default();
            for enabled in enables.iter().filter_map(Value::as_str) {
                if enabled.starts_with(&format!("{CLIENT}/"))
                    || enabled.starts_with(&format!("{CLIENT}?/"))
                    || enabled == format!("dep:{CLIENT}")
                {
                    problems.push(format!(
                        "{path} [features] {name}: enables `{enabled}`; {CLIENT}'s features are \
                         set on its dependency entries only"
                    ));
                }
            }
        }
    }
    problems
}

/// The client crate, by its Rust name.
static ENGINE_CRATE: LazyLock<Regex> = LazyLock::new(|| pattern(r"\bdocker_engine\b"));
/// A request being built: a `Request` constructor or the `request::Builder` type.
static REQUEST_BUILT: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"\bRequest\s*::\s*\w+|\brequest\s*::\s*Builder\b"));
/// A method named or set: any `Method` item, a setter, a builder's `.method(..)`, a `Parts`
/// field, a parsed name, an alias or a method's name as a string.
static METHOD: LazyLock<Regex> = LazyLock::new(|| {
    pattern(
        r"\bMethod\s*::\s*\w+|\bmethod_mut\s*\(|\.method\s*\(\s*[^)\s]|\.method\s*=[^=]|\bMethod\s+as\b|\x22(?:GET|HEAD|POST|PUT|DELETE|PATCH|OPTIONS|CONNECT|TRACE)\x22",
    )
});
/// A write call of docker-engine's client.
static WRITE_CALL: LazyLock<Regex> = LazyLock::new(|| pattern(r"\bexchange(?:_upgrade)?\s*\("));
/// An environment read or a TCP dial: anything but the configured unix socket.
static ENV_OR_TCP: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"\benv\s*::\s*var|\bTcpStream\b|DOCKER_"));
/// The client built from the environment (`DOCKER_HOST` and the rest).
static FROM_ENV: LazyLock<Regex> = LazyLock::new(|| pattern(r"\bfrom_env\b"));
/// A request path: a client call's path argument.
static REQUEST_PATH: LazyLock<Regex> = LazyLock::new(|| {
    pattern(r#"\b(?:get_list|get_object|get_body|get)\(\s*(?:&format!\(\s*)?"(/[^"]*)""#)
});
/// The client's `events` and `ping`, whose paths (`/events`, `/_ping`) are the crate's.
static FIXED_PATH_CALL: LazyLock<Regex> = LazyLock::new(|| pattern(r"\.\s*(events|ping)\s*\("));
/// The `DockerApi` trait's block, up to its closing brace.
static DOCKER_API_TRAIT: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"(?s)pub trait DockerApi\b.*?\n\}"));

/// The path rule: only the daemon's paths, and the pattern still sees each of them.
fn path_problems(files: &[(String, String)]) -> Vec<String> {
    let mut problems = Vec::new();
    let mut seen = Vec::new();
    for (path, text) in files.iter().filter(|(path, _)| in_docker(path)) {
        let calls = REQUEST_PATH
            .captures_iter(text)
            .filter_map(|captures| captures.get(1))
            .map(|found| (found.start(), found.as_str().to_owned()))
            .chain(FIXED_PATH_CALL.captures_iter(text).filter_map(|captures| {
                captures.get(1).map(|found| {
                    let fixed = if found.as_str() == "events" {
                        "/events"
                    } else {
                        "/_ping"
                    };
                    (found.start(), fixed.to_owned())
                })
            }));
        for (offset, request_path) in calls {
            if !PATHS.contains(&request_path.as_str()) {
                problems.push(format!(
                    "{path}:{}: {request_path} is not one of the daemon's paths {PATHS:?}",
                    line_of(text, offset)
                ));
            }
            seen.push(request_path);
        }
    }
    for wanted in PATHS {
        if !seen.iter().any(|found| found == wanted) {
            problems.push(format!(
                "{wanted} not found: the path pattern no longer matches"
            ));
        }
    }
    problems
}

/// What is wrong with the sources, given as `(path, text)` pairs.
fn problems(sources: &[(String, String)]) -> Vec<String> {
    let files: Vec<(String, String)> = sources
        .iter()
        .map(|(path, text)| (path.clone(), code(text)))
        .collect();
    let mut problems = Vec::new();
    for site in found_in(&files, &ENGINE_CRATE, |path| !in_docker(path)) {
        problems.push(format!(
            "{site}: names docker_engine; the client is reached through src/docker"
        ));
    }
    for regex in [&*REQUEST_BUILT, &*METHOD] {
        for site in found_in(&files, regex, in_docker) {
            problems.push(format!(
                "{site}: src/docker builds no request and names no method"
            ));
        }
    }
    for site in found_in(&files, &WRITE_CALL, in_docker) {
        problems.push(format!("{site}: src/docker calls no write"));
    }
    for site in found_in(&files, &ENV_OR_TCP, in_docker) {
        problems.push(format!("{site}: dials only the unix socket"));
    }
    for site in found_in(&files, &FROM_ENV, |_| true) {
        problems.push(format!(
            "{site}: the client is built from --docker-socket, not the environment"
        ));
    }
    problems.extend(path_problems(&files));
    // Comments included: a comment naming the SDK is how it creeps back.
    problems.extend(sdk_problems(sources));
    // The trait's calls.
    match files.iter().find(|(path, _)| path == API) {
        Some((_, text)) => {
            let declared: Vec<String> = DOCKER_API_TRAIT
                .find(text)
                .map(|block| {
                    FUNCTION_NAME
                        .captures_iter(block.as_str())
                        .map(|captures| captures[1].to_owned())
                        .collect()
                })
                .unwrap_or_default();
            if declared != CALLS {
                problems.push(format!(
                    "{API}: DockerApi declares {declared:?}, want {CALLS:?}"
                ));
            }
        }
        None => problems.push(format!("{API} is missing")),
    }
    problems
}

/// The SDK, the harness or its shared half (rust-common's testenv-core), by name.
static SDK_OR_HARNESS: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"\b(bollard|testenv(_core)?)\b"));

/// Files that name the SDK or the harness, in code or in a comment.
fn sdk_problems(files: &[(String, String)]) -> Vec<String> {
    let mut problems = Vec::new();
    for (path, text) in files {
        for (index, line) in text.lines().enumerate() {
            if let Some(found) = SDK_OR_HARNESS.find(line) {
                problems.push(format!("{path}:{}: names {}", index + 1, found.as_str()));
            }
        }
    }
    problems
}

/// The crate's sources: everything under `src/` but the unit tests (`tests.rs` and the modules
/// under a `tests/` directory) and the test-only helpers (`src/test_support`).
fn sources() -> Vec<(String, String)> {
    repo::tracked_or_new(&["src/*.rs"])
        .into_iter()
        .filter(|path| repo::path(path).is_file())
        .filter(|path| {
            !matches!(path.rsplit('/').next(), Some("tests.rs"))
                && !path.contains("/tests/")
                && !path.starts_with("src/test_support")
        })
        .map(|path| {
            let text = repo::read(&path);
            (path, text)
        })
        .collect()
}

/// The workspace manifests: the root, then each member's.
fn workspace_manifests() -> Vec<(String, String)> {
    let root = repo::read("Cargo.toml");
    let parsed: Table = root
        .parse()
        .unwrap_or_else(|error| panic!("parse Cargo.toml: {error}"));
    let members: Vec<String> = parsed
        .get("workspace")
        .and_then(|workspace| workspace.get("members"))
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("the root Cargo.toml lists no [workspace] members"))
        .iter()
        .filter_map(Value::as_str)
        .filter(|member| *member != ".")
        .map(|member| format!("{member}/Cargo.toml"))
        .collect();
    let mut manifests = vec![("Cargo.toml".to_owned(), root)];
    for member in members {
        let text = repo::read(&member);
        manifests.push((member, text));
    }
    manifests
}

#[test]
fn src_names_neither_the_sdk_nor_the_harness() {
    let files: Vec<(String, String)> = repo::tracked_or_new(&["src/*.rs"])
        .into_iter()
        .filter(|path| repo::path(path).is_file())
        .map(|path| {
            let text = repo::read(&path);
            (path, text)
        })
        .collect();
    assert!(files.iter().any(|(path, _)| path == "src/lib.rs"));
    let problems = sdk_problems(&files);
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn the_docker_client_is_docker_engine_on_the_unix_socket() {
    let manifests = workspace_manifests();
    assert!(manifests.len() > 1, "no member manifest read");
    let problems = manifest_problems(&manifests);
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// Negative control: a feature that dials TCP or reads the environment, a second declaration,
/// a member's declaration and a feature in the workspace pin are refused.
#[test]
fn another_client_configuration_is_refused() {
    let manifests = workspace_manifests();
    assert!(manifest_problems(&manifests).is_empty());
    let root = &manifests[0].1;
    let with_root = |text: String| -> Vec<(String, String)> {
        let mut changed = manifests.clone();
        changed[0].1 = text;
        changed
    };
    let package = r#"docker-engine = { workspace = true, features = ["write", "attach"] }"#;
    assert!(root.contains(package), "the root package's entry moved");
    let pin = "docker-engine = { git = ";
    assert!(root.contains(pin), "the workspace pin moved");
    let mut member = manifests.clone();
    member[1]
        .1
        .push_str("\n[target.'cfg(unix)'.dependencies]\ndocker-engine.workspace = true\n");
    let cases = [
        with_root(root.replacen(
            package,
            r#"docker-engine = { workspace = true, features = ["write", "attach", "tcp"] }"#,
            1,
        )),
        with_root(root.replacen(
            r#"features = ["write", "attach", "test-util"]"#,
            r#"features = ["write", "attach", "test-util", "tls"]"#,
            1,
        )),
        with_root(root.replacen(package, "", 1)),
        with_root(format!(
            "{root}\n[target.'cfg(unix)'.dependencies]\n{package}\n"
        )),
        with_root(root.replacen(pin, r#"docker-engine = { features = ["tcp"], git = "#, 1)),
        with_root(root.replacen(
            "[features]\n",
            "[features]\nremote = [\"docker-engine/tcp\"]\n",
            1,
        )),
        member,
    ];
    for (index, case) in cases.iter().enumerate() {
        assert!(!manifest_problems(case).is_empty(), "case {index} accepted");
    }
}

#[test]
fn the_docker_client_is_read_only() {
    let files = sources();
    for wanted in [
        API,
        "src/docker/mod.rs",
        "src/launcher/engine.rs",
        "src/run.rs",
    ] {
        assert!(
            files.iter().any(|(path, _)| path == wanted),
            "the scan did not see {wanted}: {:?}",
            files.iter().map(|(path, _)| path).collect::<Vec<_>>()
        );
    }
    let problems = problems(&files);
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// Negative control: each way to send something other than the client's `GET`s and ping from
/// `src/docker`, to reach the crate around it, to grow the trait or to leave the socket is
/// refused.
#[test]
fn a_write_path_is_refused() {
    let files = sources();
    assert!(problems(&files).is_empty());
    let with = |path: &str, extra: &str| -> Vec<(String, String)> {
        let mut changed = files.clone();
        match changed.iter_mut().find(|(name, _)| name == path) {
            Some((_, text)) => text.push_str(extra),
            None => changed.push((path.to_owned(), extra.to_owned())),
        }
        changed
    };
    let api = files
        .iter()
        .find(|(path, _)| path == API)
        .map(|(_, text)| text.clone())
        .unwrap_or_default();
    let cases = [
        with(
            API,
            "\nasync fn remove(&self) {\n    self.inner.exchange(\"/containers/x\", \"\", |_| {}).await;\n}\n",
        ),
        with(
            API,
            "\nasync fn attach(&self) {\n    self.inner.exchange_upgrade(\"/x\", \"\", |_| {}).await;\n}\n",
        ),
        with(
            API,
            "\nfn shape(request: &mut Request<Body>) {\n    *request.method_mut() = Method::DELETE;\n}\n",
        ),
        with(
            "src/docker/mod.rs",
            "\nfn build() {\n    let request = Request::new(body);\n}\n",
        ),
        with(
            "src/docker/types.rs",
            "\nfn start() {\n    let request = hyper::Request::builder().method(\"POST\");\n}\n",
        ),
        with("src/docker/mod.rs", "\nuse hyper::Method as Verb;\n"),
        with(
            "src/docker/mod.rs",
            "\nfn host() {\n    let host = std::env::var(\"DOCKER_HOST\");\n}\n",
        ),
        with(
            "src/docker/mod.rs",
            "\nfn dial() {\n    let stream = TcpStream::connect(addr);\n}\n",
        ),
        with(
            "src/run.rs",
            "\nfn settings() {\n    let settings = docker::Settings::from_env(getenv);\n}\n",
        ),
        with(
            "src/daemon/mod.rs",
            "\nfn raw(settings: Settings) {\n    let client = docker_engine::Client::new(settings);\n}\n",
        ),
        with(
            "src/launcher/engine.rs",
            "\nuse docker_engine::Client as Raw;\n",
        ),
        with(
            API,
            "\nasync fn kill(&self) {\n    self.inner.get_object(&format!(\"/containers/{id}/kill\")).await;\n}\n",
        ),
        with(
            API,
            "\nasync fn version(&self) {\n    self.inner.get(\"/version\", \"\", &[]).await;\n}\n",
        ),
        with(API, "\n// Unlike bollard, this client is shared.\n"),
        with("src/run.rs", "\nuse testenv::config::ENV_IDS;\n"),
        with("src/run.rs", "\nuse testenv_core::host::HostSweep;\n"),
        files
            .iter()
            .map(|(path, text)| {
                let text = if path == API {
                    api.replacen(
                        "pub trait DockerApi: Send + Sync {",
                        "pub trait DockerApi: Send + Sync {\n    fn container_remove(&self);",
                        1,
                    )
                } else {
                    text.clone()
                };
                (path.clone(), text)
            })
            .collect(),
    ];
    for (index, case) in cases.iter().enumerate() {
        assert!(!problems(case).is_empty(), "case {index} accepted");
    }
    // A comment naming a method or the crate is not a request.
    assert!(problems(&with(API, "\n// Method::POST would be a write.\n")).is_empty());
    assert!(problems(&with("src/run.rs", "\n// docker_engine, via src/docker.\n")).is_empty());
    // Every path gone from the pattern's sight reads as a broken pattern, not as a pass.
    let renamed: Vec<(String, String)> = files
        .iter()
        .map(|(path, text)| (path.clone(), text.replace("get_object(", "fetch_object(")))
        .collect();
    assert!(!problems(&renamed).is_empty());
}
