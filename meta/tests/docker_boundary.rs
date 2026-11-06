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
//! rust-common's `docker-client` on bollard, reached through `src/docker` alone, and the daemon's
//! trait has the ping, list, inspect and events calls only. This pins it on the manifests and on
//! the text of the non-test sources under `src/`:
//!
//! - `docker-client` is the sanctioned client: only the root package declares it, once in
//!   `[dependencies]` with `attach` at most (the launcher's output and wait pumps) and in
//!   `[dev-dependencies]` with `test-util` besides; never `tcp` or `tls`, so the client dials the
//!   configured unix socket only and reads no `DOCKER_*` variable;
//! - only `src/docker` names the crate, and there no bollard write is called (`create_container`,
//!   `start_container`, `attach_container`, `wait_container` and the rest of the verbs) nor
//!   docker-client's `attach::attach` and `attach::wait`: `DockerApi` is built on three calls of
//!   the client, `ping`, `list_containers` and `inspect_container`, plus docker-client's
//!   `events_established`, and no other call is made on it;
//! - `DockerApi` declares exactly `ping`, `container_list`, `container_inspect` and `events`;
//! - nothing under `src/docker` reads an environment variable or dials TCP, and nothing under
//!   `src/` builds the client from the environment (`Config::from_env`);
//! - bollard is named in `src/docker` (which re-exports its models) and in the launcher's engine
//!   (`src/launcher/engine.rs` and its tests) alone, and nothing under `src/` names `testenv`,
//!   the harness that creates and removes containers on purpose, or `testenv_core`, its shared
//!   half, whose `bollard` feature brings the SDK; the root package declares none of bollard,
//!   `testenv` and `testenv_core` in any dependency table (bollard comes with docker-client).
//!
//! The launcher's write calls take bollard's client through `Client::transport` outside
//! `src/docker`, in the launcher's engine; who may do that is pinned by `launcher_boundary.rs`.
//!
//! The checks are functions of the text, and each is also run on input built to break it.

use std::sync::LazyLock;

use regex::Regex;
use toml::{Table, Value};

/// Where the trait lives.
const API: &str = "src/docker/api.rs";
/// The trait's calls, in declaration order.
const CALLS: &[&str] = &["ping", "container_list", "container_inspect", "events"];
/// The only calls `src/docker` makes on the client (`docker.<call>(`), which are the engine's
/// `GET /_ping`, `GET /containers/json` and `GET /containers/{id}/json`.
const READS: &[&str] = &["ping", "list_containers", "inspect_container"];
/// The fourth read, `GET /events`, which docker-client makes as a function of its own (it returns
/// once the engine answered the headers) and not as a call on bollard's client.
const EVENTS_READ: &str = "events_established";
/// The sanctioned Docker client.
const CLIENT: &str = "docker-client";
/// What the root package may enable: in its `[dependencies]` (the launcher's output and wait
/// pumps), and in its `[dev-dependencies]`.
const CLIENT_FEATURES: &[&str] = &["attach"];
const CLIENT_TEST_FEATURES: &[&str] = &["attach", "test-util"];
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

/// The crates the root package never depends on: the SDK, the harness that creates and removes
/// containers on purpose, and its shared half (whose `bollard` feature brings the SDK). The
/// workspace may pin `testenv-core` for the harness member; the root package's own tables, test
/// and build tables and target-specific ones included, never name any of them.
const HARNESS_CRATES: &[&str] = &["bollard", "testenv", "testenv-core"];

/// Every dependency entry of the root package (`manifest`, the root `Cargo.toml`'s text) on one of
/// [`HARNESS_CRATES`], by its crate name: a rename (`package = ...`) in the entry, or in the
/// workspace pin an entry inherits, is followed.
fn harness_dependency_problems(manifest: &str) -> Vec<String> {
    let manifest: Table = manifest
        .parse()
        .unwrap_or_else(|error| panic!("parse Cargo.toml: {error}"));
    let pins = manifest
        .get("workspace")
        .and_then(|workspace| workspace.get("dependencies"))
        .and_then(Value::as_table);
    let mut problems = Vec::new();
    for dependency in dependencies(&manifest) {
        if dependency.table == "workspace.dependencies" {
            continue;
        }
        let inherited = dependency.entry.get("workspace").and_then(Value::as_bool) == Some(true);
        let crate_name = if inherited {
            pins.and_then(|pins| pins.get(&dependency.crate_name))
                .and_then(|pin| pin.get("package"))
                .and_then(Value::as_str)
                .unwrap_or(&dependency.crate_name)
        } else {
            &dependency.crate_name
        };
        if HARNESS_CRATES.contains(&crate_name) {
            problems.push(format!(
                "Cargo.toml [{}] {crate_name}: the root package depends on neither the Docker \
                 SDK nor the harness",
                dependency.table
            ));
        }
    }
    problems
}

/// The client crate, by its Rust name.
static ENGINE_CRATE: LazyLock<Regex> = LazyLock::new(|| pattern(r"\bdocker_client\b"));
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
/// A write call: one of bollard's verbs (`create_container`, `start_exec`, `remove_image`, ...)
/// on anything, or docker-client's attach and wait pumps.
static WRITE_CALL: LazyLock<Regex> = LazyLock::new(|| {
    pattern(
        r"\.\s*(?:create|start|stop|restart|kill|pause|unpause|remove|rename|update|prune|attach|wait|resize|upload|commit|exec|build|push|pull|tag|import|connect|disconnect|join|leave|init|delete)_(?:containers?|exec|images?|networks?|volumes?|services?|swarm|nodes?|secrets?|configs?|plugins?|tasks?)\s*\(|\battach\s*::\s*(?:attach|wait)\b",
    )
});
/// An environment read or a TCP dial: anything but the configured unix socket.
static ENV_OR_TCP: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"\benv\s*::\s*var|\bTcpStream\b|DOCKER_"));
/// The client built from the environment (`DOCKER_HOST` and the rest).
static FROM_ENV: LazyLock<Regex> = LazyLock::new(|| pattern(r"\bfrom_env\b"));
/// A call on the client the daemon holds, which is always called `docker` in `src/docker`.
static CLIENT_CALL: LazyLock<Regex> = LazyLock::new(|| pattern(r"\bdocker\s*\.\s*(\w+)\s*\("));
/// A call of [`EVENTS_READ`].
static EVENTS_CALL: LazyLock<Regex> = LazyLock::new(|| pattern(&format!(r"\b{EVENTS_READ}\s*\(")));
/// The `DockerApi` trait's block, up to its closing brace.
static DOCKER_API_TRAIT: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"(?s)pub trait DockerApi\b.*?\n\}"));

/// The read rule: `src/docker` makes only the four reads on the client, and the pattern still sees
/// each of them.
fn read_problems(files: &[(String, String)]) -> Vec<String> {
    let mut problems = Vec::new();
    let mut seen = Vec::new();
    for (path, text) in files.iter().filter(|(path, _)| in_docker(path)) {
        for captures in CLIENT_CALL.captures_iter(text) {
            let Some(call) = captures.get(1) else {
                continue;
            };
            if !READS.contains(&call.as_str()) {
                problems.push(format!(
                    "{path}:{}: docker.{}( is not one of the daemon's reads {READS:?}",
                    line_of(text, call.start()),
                    call.as_str()
                ));
            }
            seen.push(call.as_str().to_owned());
        }
    }
    if !files
        .iter()
        .any(|(path, text)| in_docker(path) && EVENTS_CALL.is_match(text))
    {
        problems.push(format!(
            "{EVENTS_READ}( not found: the call pattern no longer matches"
        ));
    }
    for wanted in READS {
        if !seen.iter().any(|found| found == wanted) {
            problems.push(format!(
                "docker.{wanted}( not found: the call pattern no longer matches"
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
            "{site}: names docker_client; the client is reached through src/docker"
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
    problems.extend(read_problems(&files));
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

/// The launcher's engine and its tests, which name bollard's models and errors through
/// `crate::docker`.
fn is_engine(path: &str) -> bool {
    path == "src/launcher/engine.rs" || path.starts_with("src/launcher/engine/")
}

/// Files that name the harness anywhere, or the SDK outside `src/docker` and the launcher's
/// engine, in code or in a comment.
fn sdk_problems(files: &[(String, String)]) -> Vec<String> {
    let mut problems = Vec::new();
    for (path, text) in files {
        let sdk_allowed = in_docker(path) || is_engine(path);
        for (index, line) in text.lines().enumerate() {
            if let Some(found) = SDK_OR_HARNESS.find(line)
                && !(sdk_allowed && found.as_str() == "bollard")
            {
                problems.push(format!("{path}:{}: names {}", index + 1, found.as_str()));
            }
        }
    }
    problems
}

/// The crate's sources: everything under `src/` but the unit tests (`tests.rs` and the modules
/// under a `tests/` directory) and the test-only helpers (`src/test_support`).
fn sources() -> Vec<(String, String)> {
    let repo = meta::repo();
    let paths: Vec<String> = repo
        .tracked_or_new(&["src/*.rs"])
        .into_iter()
        .filter(|path| {
            !matches!(path.rsplit('/').next(), Some("tests.rs"))
                && !path.contains("/tests/")
                && !path.starts_with("src/test_support")
        })
        .collect();
    repo.read_all(&paths)
}

/// The workspace manifests: the root, then each member's.
fn workspace_manifests() -> Vec<(String, String)> {
    let repo = meta::repo();
    let root = repo.read("Cargo.toml");
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
        let text = repo.read(&member);
        manifests.push((member, text));
    }
    manifests
}

#[test]
fn src_names_neither_the_sdk_nor_the_harness() {
    let repo = meta::repo();
    let files = repo.read_all(&repo.tracked_or_new(&["src/*.rs"]));
    assert!(files.iter().any(|(path, _)| path == "src/lib.rs"));
    let problems = sdk_problems(&files);
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn the_docker_client_is_docker_client_on_the_unix_socket() {
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
    let package = r#"docker-client = { workspace = true, features = ["attach"] }"#;
    assert!(root.contains(package), "the root package's entry moved");
    let pin = "docker-client = { git = ";
    assert!(root.contains(pin), "the workspace pin moved");
    let mut member = manifests.clone();
    member[1]
        .1
        .push_str("\n[target.'cfg(unix)'.dependencies]\ndocker-client.workspace = true\n");
    let cases = [
        with_root(root.replacen(
            package,
            r#"docker-client = { workspace = true, features = ["attach", "tcp"] }"#,
            1,
        )),
        with_root(root.replacen(
            r#"features = ["attach", "test-util"]"#,
            r#"features = ["attach", "test-util", "tls"]"#,
            1,
        )),
        with_root(root.replacen(package, "", 1)),
        with_root(format!(
            "{root}\n[target.'cfg(unix)'.dependencies]\n{package}\n"
        )),
        with_root(root.replacen(pin, r#"docker-client = { features = ["tcp"], git = "#, 1)),
        with_root(root.replacen(
            "[features]\n",
            "[features]\nremote = [\"docker-client/tcp\"]\n",
            1,
        )),
        member,
    ];
    for (index, case) in cases.iter().enumerate() {
        assert!(!manifest_problems(case).is_empty(), "case {index} accepted");
    }
}

/// The root package depends on neither bollard nor the harness (`testenv`, `testenv-core`), in
/// any of its dependency tables.
#[test]
fn the_root_package_depends_on_neither_the_sdk_nor_the_harness() {
    let root = meta::repo().read("Cargo.toml");
    assert!(
        root.contains("testenv-core = { git = "),
        "the workspace pin of testenv-core moved; the control below would not see it allowed"
    );
    let problems = harness_dependency_problems(&root);
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// Negative control: the SDK or either half of the harness in each of the root package's tables,
/// renamed in the entry or in the workspace pin it inherits, is refused; the workspace pin alone
/// is not.
#[test]
fn a_harness_dependency_of_the_root_package_is_refused() {
    let root = meta::repo().read("Cargo.toml");
    assert!(harness_dependency_problems(&root).is_empty());
    for (name, extra, want) in [
        (
            "bollard in [dependencies]",
            "\n[dependencies.bollard]\nversion = \"0.19\"\n",
            "[dependencies] bollard",
        ),
        (
            "testenv in [dev-dependencies]",
            "\n[dev-dependencies.testenv]\npath = \"testenv\"\n",
            "[dev-dependencies] testenv",
        ),
        (
            "testenv-core in [build-dependencies]",
            "\n[build-dependencies]\ntestenv-core.workspace = true\n",
            "[build-dependencies] testenv-core",
        ),
        (
            "testenv-core in a target table",
            "\n[target.'cfg(unix)'.dependencies]\ntestenv-core.workspace = true\n",
            "[target.cfg(unix).dependencies] testenv-core",
        ),
        (
            "bollard in a target's dev-dependencies",
            "\n[target.'cfg(unix)'.dev-dependencies]\nbollard = \"0.19\"\n",
            "[target.cfg(unix).dev-dependencies] bollard",
        ),
        (
            "bollard renamed in the entry",
            "\n[dependencies.engine]\npackage = \"bollard\"\nversion = \"0.19\"\n",
            "[dependencies] bollard",
        ),
    ] {
        let problems = harness_dependency_problems(&format!("{root}{extra}"));
        assert!(
            problems.len() == 1 && problems[0].contains(want),
            "{name}: want one problem naming {want:?}, got {problems:?}"
        );
    }
    let renamed_pin = root.replacen(
        "[workspace.dependencies]\n",
        "[workspace.dependencies]\nharness = { package = \"testenv-core\", path = \"x\" }\n",
        1,
    );
    assert_ne!(
        renamed_pin, root,
        "the [workspace.dependencies] table moved"
    );
    let problems = harness_dependency_problems(&format!(
        "{renamed_pin}\n[dev-dependencies.harness]\nworkspace = true\n"
    ));
    assert!(
        problems.len() == 1 && problems[0].contains("[dev-dependencies] testenv-core"),
        "testenv-core renamed in the workspace pin: {problems:?}"
    );
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
            "\nasync fn remove(&self, docker: Docker) {\n    docker.remove_container(\"x\", None).await;\n}\n",
        ),
        with(
            API,
            "\nasync fn attach(&self, docker: Docker) {\n    attach::attach(&docker, \"x\", timeout, 8, cancel).await;\n}\n",
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
            "\nfn raw(config: Config) {\n    let client = docker_client::connect(config);\n}\n",
        ),
        with(
            "src/launcher/engine.rs",
            "\nuse docker_client::attach as pumps;\n",
        ),
        with(
            API,
            "\nasync fn kill(&self, docker: Docker) {\n    docker.kill_container(\"x\", None).await;\n}\n",
        ),
        with(
            API,
            "\nasync fn version(&self, docker: Docker) {\n    docker.version().await;\n}\n",
        ),
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
    // Every path gone from the pattern's sight reads as a broken pattern, not as a pass.
    let renamed: Vec<(String, String)> = files
        .iter()
        .map(|(path, text)| {
            (
                path.clone(),
                text.replace("inspect_container(", "inspect_one("),
            )
        })
        .collect();
    assert!(!problems(&renamed).is_empty());
}

/// Negative control: the SDK named outside `src/docker` and the launcher's engine, in code or in
/// a comment, and the harness named anywhere, are refused; the SDK is allowed where its models
/// are reached.
#[test]
fn the_sdk_and_the_harness_are_named_where_allowed_only() {
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
    let cases = [
        with(
            "src/daemon/mod.rs",
            "\n// Unlike bollard, this client is shared.\n",
        ),
        with("src/run.rs", "\nuse testenv::config::ENV_IDS;\n"),
        with("src/run.rs", "\nuse testenv_core::host::HostSweep;\n"),
        with("src/run.rs", "\nuse bollard::Docker;\n"),
    ];
    for (index, case) in cases.iter().enumerate() {
        assert!(!problems(case).is_empty(), "case {index} accepted");
    }
    // A comment naming a method or the crate is not a request, and the SDK may be named where its
    // models are reached.
    assert!(
        problems(&with(
            "src/launcher/engine.rs",
            "\nuse crate::docker::bollard::models;\n"
        ))
        .is_empty()
    );
    assert!(
        !problems(&with(
            "src/launcher/mod.rs",
            "\nuse crate::docker::bollard;\n"
        ))
        .is_empty()
    );
    assert!(problems(&with(API, "\n// Method::POST would be a write.\n")).is_empty());
    assert!(problems(&with("src/run.rs", "\n// docker_client, via src/docker.\n")).is_empty());
}
