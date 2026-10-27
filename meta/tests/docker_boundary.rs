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

//! The daemon's Docker client is read-only (`docs/rust-port.md`, the Docker transport): it
//! sends `GET` requests, plus the one `HEAD /_ping` of moby's lazy version negotiation, through
//! one request builder, and its trait has the ping, list, inspect and events calls only. This pins
//! it on the text of the non-test sources under `src/docker`:
//!
//! - one request is built in one place, `Client::request` in `src/docker/client.rs`, which sets
//!   `Method::GET`; the only other method set is `Method::HEAD`, in `Client::ping`; those are the
//!   only two method setters and the only two `Method` items named, so no helper can take a
//!   method from a caller, and no method string appears;
//! - `DockerApi` declares exactly `ping`, `container_list`, `container_inspect` and `events`;
//! - it asks only for the daemon's paths: `/_ping`, `/containers/json`, `/containers/{id}/json`
//!   and `/events`;
//! - nothing reads an environment variable or dials TCP: the client dials the configured unix
//!   socket;
//! - nothing under `src/` names bollard or `testenv`, the harness that creates and removes
//!   containers on purpose.
//!
//! The launcher's write calls take the builder's request through the client's two crate-private
//! hooks and set their method outside `src/docker`, in the launcher's engine; who may do that is
//! pinned by `launcher_boundary.rs`.
//!
//! The checks are functions of the text, and each is also run on input built to break it.

use meta::repo;
use regex::Regex;

/// Where the request builder lives.
const CLIENT: &str = "src/docker/client.rs";
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

fn pattern(text: &str) -> Regex {
    Regex::new(text).unwrap_or_else(|error| panic!("pattern {text}: {error}"))
}

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

/// The name of the function whose body holds byte `offset`: the last `fn` declared before it.
fn enclosing_fn(text: &str, offset: usize) -> String {
    pattern(r"\bfn\s+(\w+)")
        .captures_iter(&text[..offset])
        .last()
        .map(|captures| captures[1].to_owned())
        .unwrap_or_default()
}

fn line_of(text: &str, offset: usize) -> usize {
    text[..offset].matches('\n').count() + 1
}

/// Every place `regex` matches, as `(path, line, enclosing function, match)`.
fn sites(files: &[(String, String)], regex: &Regex) -> Vec<(String, usize, String, String)> {
    files
        .iter()
        .flat_map(|(path, text)| {
            regex.find_iter(text).map(move |found| {
                (
                    path.clone(),
                    line_of(text, found.start()),
                    enclosing_fn(text, found.start()),
                    found.as_str().to_owned(),
                )
            })
        })
        .collect()
}

/// Requires `regex` to match exactly once in `files`, in `function` of `path`.
fn exactly_once(
    files: &[(String, String)],
    regex: &Regex,
    path: &str,
    function: &str,
) -> Vec<String> {
    let found = sites(files, regex);
    if let [(found_path, _, found_fn, _)] = found.as_slice()
        && found_path == path
        && found_fn == function
    {
        return Vec::new();
    }
    let places: Vec<String> = found
        .iter()
        .map(|(path, line, function, text)| format!("{path}:{line} in {function}: {text}"))
        .collect();
    vec![format!(
        "want `{}` exactly once, in {function} of {path}; found {places:?}",
        regex.as_str()
    )]
}

/// The method rules: one builder, `GET` in it, `HEAD` in the ping, nothing else.
fn method_problems(files: &[(String, String)]) -> Vec<String> {
    let mut problems = Vec::new();
    // One request built, in one place.
    problems.extend(exactly_once(
        files,
        &pattern(r"\bRequest\s*::\s*\w+|\brequest\s*::\s*Builder\b"),
        CLIENT,
        "request",
    ));
    problems.extend(exactly_once(
        files,
        &pattern(r"\bMethod\s*::\s*GET\b"),
        CLIENT,
        "request",
    ));
    problems.extend(exactly_once(
        files,
        &pattern(r"\bMethod\s*::\s*HEAD\b"),
        CLIENT,
        "ping",
    ));
    // No other method is named: only the two constants, so no conversion (`try_from`,
    // `from_bytes`) and no other constant.
    for (path, line, function, text) in sites(files, &pattern(r"\bMethod\s*::\s*\w+")) {
        if !pattern(r"::\s*(?:GET|HEAD)$").is_match(&text) {
            problems.push(format!(
                "{path}:{line} in {function}: {text}: not a read-only method"
            ));
        }
    }
    // Exactly two method setters, each assigning its constant: `GET` in the builder and `HEAD`
    // in the ping. A setter fed from a parameter would let a caller choose the method.
    let mut setters = Vec::new();
    for (path, line, function, text) in
        sites(files, &pattern(r"\bmethod_mut\s*\(\s*\)(?:\s*=\s*[^;]*)?"))
    {
        let assigned = text
            .split_once('=')
            .map(|(_, value)| value.trim().to_owned());
        let wanted = match (path.as_str(), function.as_str()) {
            (CLIENT, "request") => Some("Method::GET"),
            (CLIENT, "ping") => Some("Method::HEAD"),
            _ => None,
        };
        if wanted.is_none() || assigned.as_deref() != wanted {
            problems.push(format!(
                "{path}:{line} in {function}: {text}: only request sets Method::GET and only \
                 ping sets Method::HEAD"
            ));
        }
        setters.push((path, function));
    }
    setters.sort();
    if setters
        != [
            (CLIENT.to_owned(), "ping".to_owned()),
            (CLIENT.to_owned(), "request".to_owned()),
        ]
    {
        problems.push(format!(
            "method setters in {setters:?}; want one in request and one in ping of {CLIENT}"
        ));
    }
    // Nor set another way: a builder's `.method(..)`, a `Parts` field, a parsed name, an alias.
    let other_method = pattern(
        r"\.method\s*\(\s*[^)\s]|\.method\s*=[^=]|\.parse\s*::\s*<\s*Method|\bMethod\s+as\b|\x22(?:POST|PUT|DELETE|PATCH|OPTIONS|CONNECT|TRACE)\x22",
    );
    for (path, line, function, text) in sites(files, &other_method) {
        problems.push(format!(
            "{path}:{line} in {function}: {text}: not a read-only method"
        ));
    }
    problems
}

/// The path rule: only the daemon's paths, and the pattern still sees them.
fn path_problems(files: &[(String, String)]) -> Vec<String> {
    let mut problems = Vec::new();
    // Only the daemon's paths.
    let request_path = pattern(
        r#"(?:\b(?:get_list|get_object|get)\(\s*(?:&format!\(\s*)?|join_path\(&\[(?:[^\]]*,\s*)?)"(/[^"]*)""#,
    );
    let mut paths = 0;
    for (path, text) in files {
        for captures in request_path.captures_iter(text) {
            paths += 1;
            if !PATHS.contains(&&captures[1]) {
                problems.push(format!(
                    "{path}:{}: {} is not one of the daemon's paths {PATHS:?}",
                    line_of(text, captures.get(1).map_or(0, |found| found.start())),
                    &captures[1]
                ));
            }
        }
    }
    if paths < PATHS.len() {
        problems.push(format!(
            "{paths} request paths found, want at least {}: the path pattern no longer matches",
            PATHS.len()
        ));
    }
    problems
}

/// What is wrong with the client's sources, given as `(path, text)` pairs.
fn problems(sources: &[(String, String)]) -> Vec<String> {
    let files: Vec<(String, String)> = sources
        .iter()
        .map(|(path, text)| (path.clone(), code(text)))
        .collect();
    let mut problems = Vec::new();
    problems.extend(method_problems(&files));
    problems.extend(path_problems(&files));
    // Neither the environment nor TCP.
    let outside = pattern(r"\benv\s*::\s*var|\bTcpStream\b|DOCKER_");
    for (path, line, function, text) in sites(&files, &outside) {
        problems.push(format!(
            "{path}:{line} in {function}: {text}: dials only the unix socket"
        ));
    }
    // Comments included: a comment naming the SDK is how it creeps back.
    problems.extend(sdk_problems(sources));
    // The trait's calls.
    match files.iter().find(|(path, _)| path == API) {
        Some((_, text)) => {
            let declared: Vec<String> = pattern(r"(?s)pub trait DockerApi\b.*?\n\}")
                .find(text)
                .map(|block| {
                    pattern(r"\bfn\s+(\w+)")
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

/// Files under `src/` that name the SDK or the harness, in code or in a comment.
fn sdk_problems(files: &[(String, String)]) -> Vec<String> {
    let sdk = pattern(r"\b(bollard|testenv)\b");
    let mut problems = Vec::new();
    for (path, text) in files {
        for (index, line) in text.lines().enumerate() {
            if let Some(found) = sdk.find(line) {
                problems.push(format!("{path}:{}: names {}", index + 1, found.as_str()));
            }
        }
    }
    problems
}

/// The Docker client's sources: everything under `src/docker` but its unit tests.
fn client_sources() -> Vec<(String, String)> {
    repo::tracked_or_new(&["src/docker/*.rs"])
        .into_iter()
        .filter(|path| repo::path(path).is_file())
        .filter(|path| path.rsplit('/').next() != Some("tests.rs"))
        .map(|path| {
            let text = repo::read(&path);
            (path, text)
        })
        .collect()
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
fn the_docker_client_is_read_only() {
    let files = client_sources();
    for wanted in [CLIENT, API, "src/docker/pool.rs"] {
        assert!(
            files.iter().any(|(path, _)| path == wanted),
            "the scan did not see {wanted}: {:?}",
            files.iter().map(|(path, _)| path).collect::<Vec<_>>()
        );
    }
    let problems = problems(&files);
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// Negative control: each way to send something other than a `GET` or the ping's `HEAD`, or to
/// grow the trait, is refused.
#[test]
fn a_write_path_is_refused() {
    let files = client_sources();
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
            CLIENT,
            "\nfn remove() {\n    *request.method_mut() = Method::DELETE;\n}\n",
        ),
        with(
            CLIENT,
            "\nfn create() {\n    *request.method_mut() = Method::GET;\n}\n",
        ),
        with(
            CLIENT,
            "\nfn stop() {\n    *request.method_mut() = Method::HEAD;\n}\n",
        ),
        with(
            CLIENT,
            "\nfn request_as(&self, method: Method) {\n    let mut request = \
             self.request(target, &[]);\n    *request.method_mut() = method;\n}\n",
        ),
        with(
            CLIENT,
            "\nfn ping() {\n    let method = Method::try_from(\"DELETE\");\n}\n",
        ),
        with(CLIENT, "\nfn ping() {\n    parts.method = verb;\n}\n"),
        with(CLIENT, "\nuse hyper::Method as Verb;\n"),
        with(
            "src/docker/pool.rs",
            "\nfn build() {\n    let request = Request::new(body);\n}\n",
        ),
        with(
            "src/docker/api.rs",
            "\nfn start() {\n    let request = hyper::Request::builder().method(\"POST\");\n}\n",
        ),
        with(
            "src/docker/engine.rs",
            "fn wait() {\n    let method = Method::from_bytes(b\"PUT\");\n}\n",
        ),
        with(
            "src/docker/connect.rs",
            "\nfn host() {\n    let host = std::env::var(\"DOCKER_HOST\");\n}\n",
        ),
        with(
            API,
            "\nfn remove(&self) {\n    self.get_object(&format!(\"/containers/{id}/kill\"));\n}\n",
        ),
        with(
            API,
            "\nfn version(&self) {\n    self.get(\"/version\", \"\", &[]);\n}\n",
        ),
        with(
            "src/docker/client.rs",
            "\n// Unlike bollard, this client is in-tree.\n",
        ),
        with(
            "src/docker/connect.rs",
            "\nfn dial() {\n    let stream = TcpStream::connect(addr);\n}\n",
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
    // A comment naming a method is not a request.
    assert!(problems(&with(CLIENT, "\n// Method::POST would be a write.\n")).is_empty());
    // Every path gone from the pattern's sight reads as a broken pattern, not as a pass.
    let renamed: Vec<(String, String)> = files
        .iter()
        .map(|(path, text)| (path.clone(), text.replace("get_object(", "fetch_object(")))
        .collect();
    assert!(!problems(&renamed).is_empty());
}
