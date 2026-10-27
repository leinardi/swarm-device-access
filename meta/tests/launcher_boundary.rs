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

//! Only the launcher reaches the Docker write calls (`docs/rust-port.md`, the Docker launcher
//! path; the `trust-boundary` skill): the privileged daemon can create, start, stop or remove
//! nothing. The write calls are the launcher's engine, `src/launcher/engine.rs`, sent through
//! the two crate-private hooks of the shared client, `Client::exchange` and
//! `Client::exchange_upgrade`. This pins it on the text of the non-test sources under `src/`:
//!
//! - the hooks stay `pub(crate)`, declared in `src/docker/client.rs`, and only
//!   `src/launcher/engine.rs` calls them;
//! - outside `src/docker` (whose own methods `docker_boundary.rs` pins), only
//!   `src/launcher/engine.rs` names an HTTP method or sets one;
//! - outside `src/launcher`, nothing names the engine: its module, `Engine`, `EngineApi`, or a
//!   launcher path to them;
//! - outside `src/launcher` and the CLI entry modules that run the `launch` subcommand
//!   (`src/main.rs`, `src/run.rs`, `src/cli.rs`, `src/cli/**`), nothing names the launcher at
//!   all, by a path or an import: its `run` and `Launch` build the engine from a plain client.
//!
//! The checks are functions of the text, and each is also run on input built to break it.

use meta::repo;
use regex::Regex;

/// The launcher's write engine.
const ENGINE: &str = "src/launcher/engine.rs";
/// Where the hooks are declared.
const CLIENT: &str = "src/docker/client.rs";
/// The modules that run the `launch` subcommand, a trailing `/` standing for a directory.
const CLI_ENTRY: &[&str] = &["src/main.rs", "src/run.rs", "src/cli.rs", "src/cli/"];

fn entry_matches(entry: &str, path: &str) -> bool {
    if entry.ends_with('/') {
        path.starts_with(entry)
    } else {
        path == entry
    }
}

fn pattern(text: &str) -> Regex {
    Regex::new(text).unwrap_or_else(|error| panic!("pattern {text}: {error}"))
}

/// The text without its line comments, so a comment naming the engine is not counted; the line
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

/// Every place `regex` matches in a file `outside` allows, as `path:line: match`.
fn found_in(
    files: &[(String, String)],
    regex: &Regex,
    allowed: impl Fn(&str) -> bool,
) -> Vec<String> {
    files
        .iter()
        .filter(|(path, _)| !allowed(path))
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

/// What is wrong with the sources, given as `(path, text)` pairs.
fn problems(sources: &[(String, String)]) -> Vec<String> {
    let files: Vec<(String, String)> = sources
        .iter()
        .map(|(path, text)| (path.clone(), code(text)))
        .collect();
    let mut problems = Vec::new();

    // The hooks: declared crate-private in the client, and nowhere else, called only by the
    // engine.
    let declared = pattern(r"(?:\bpub\b[^\n]*?)?\bfn\s+exchange(?:_upgrade)?\b");
    let mut hooks = Vec::new();
    for (path, text) in &files {
        for found in declared.find_iter(text) {
            let site = format!(
                "{path}:{}: {}",
                line_of(text, found.start()),
                found.as_str()
            );
            if path != CLIENT || !found.as_str().starts_with("pub(crate) async fn exchange") {
                problems.push(format!(
                    "{site}: the write hooks are pub(crate), in {CLIENT}"
                ));
            }
            hooks.push(found.as_str().to_owned());
        }
    }
    if hooks.len() != 2 {
        problems.push(format!(
            "write hooks declared: {hooks:?}; want exchange and exchange_upgrade"
        ));
    }
    let calls = pattern(r"\.\s*exchange(?:_upgrade)?\s*\(");
    for site in found_in(&files, &calls, |path| path == ENGINE) {
        problems.push(format!("{site}: only {ENGINE} calls the write hooks"));
    }

    // HTTP methods: outside the client, only the engine names or sets one.
    let methods = pattern(
        r"\bMethod\s*::\s*\w+|\bmethod_mut\s*\(|\.method\s*\(\s*[^)\s]|\x22(?:POST|PUT|DELETE|PATCH)\x22",
    );
    for site in found_in(&files, &methods, |path| {
        path == ENGINE || path.starts_with("src/docker/")
    }) {
        problems.push(format!("{site}: only {ENGINE} names a write method"));
    }

    // The engine itself: named only under src/launcher.
    let engine = pattern(
        r"\blauncher\s*::\s*(?:engine\b|Engine\b|EngineApi\b|\{[^}]*\bEngine)|\bEngineApi\b|\bEngine\s*::",
    );
    for site in found_in(&files, &engine, |path| path.starts_with("src/launcher/")) {
        problems.push(format!("{site}: only src/launcher names the write engine"));
    }

    // The launcher itself: reached only from the CLI entry.
    let launcher = pattern(r"\blauncher\s*::|\buse\s[^;]*\blauncher\b");
    for site in found_in(&files, &launcher, |path| {
        path.starts_with("src/launcher/")
            || CLI_ENTRY.iter().any(|entry| entry_matches(entry, path))
    }) {
        problems.push(format!(
            "{site}: only src/launcher and the CLI entry {CLI_ENTRY:?} name the launcher"
        ));
    }
    problems
}

/// The crate's sources: everything under `src/` but the unit tests and the test-only fakes.
fn sources() -> Vec<(String, String)> {
    repo::tracked_or_new(&["src/*.rs"])
        .into_iter()
        .filter(|path| repo::path(path).is_file())
        .filter(|path| !matches!(path.rsplit('/').next(), Some("tests.rs" | "fake.rs")))
        .map(|path| {
            let text = repo::read(&path);
            (path, text)
        })
        .collect()
}

#[test]
fn only_the_launcher_reaches_the_write_calls() {
    let files = sources();
    for wanted in [ENGINE, CLIENT, "src/launcher/mod.rs", "src/daemon/mod.rs"] {
        assert!(
            files.iter().any(|(path, _)| path == wanted),
            "the scan did not see {wanted}"
        );
    }
    let problems = problems(&files);
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// Negative control: each way for code outside the launcher to reach a write call, or for the
/// hooks to widen, is refused.
#[test]
fn a_write_call_outside_the_launcher_is_refused() {
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
    let client = files
        .iter()
        .find(|(path, _)| path == CLIENT)
        .map(|(_, text)| text.clone())
        .unwrap_or_default();
    let mut widened = files.clone();
    for (path, text) in &mut widened {
        if path == CLIENT {
            *text = client.replace("pub(crate) async fn exchange(", "pub async fn exchange(");
        }
    }
    let cases = [
        widened,
        with(
            "src/processor/reconcile.rs",
            "\nasync fn kill(client: &Client) {\n    client.exchange(\"/containers/x/kill\", \"\", |_| {}).await;\n}\n",
        ),
        with(
            "src/daemon/mod.rs",
            "\nasync fn attach(client: &Client) {\n    client.exchange_upgrade(\"/x\", \"\", |_| {}).await;\n}\n",
        ),
        with(
            "src/daemon/mod.rs",
            "\nfn shape(request: &mut Request) {\n    *request.method_mut() = Method::POST;\n}\n",
        ),
        with(
            "src/server.rs",
            "\nfn verb() -> &'static str {\n    \"DELETE\"\n}\n",
        ),
        with("src/daemon/mod.rs", "\nuse crate::launcher::Engine;\n"),
        with(
            "src/daemon/coordinator.rs",
            "\nuse crate::launcher::{Console, Engine};\n",
        ),
        with(
            "src/processor/mod.rs",
            "\nfn engine(client: Client) {\n    let _ = Engine::new(client);\n}\n",
        ),
        with(
            "src/run.rs",
            "\nfn api<E: crate::launcher::EngineApi>() {}\n",
        ),
        with("src/run.rs", "\nuse crate::launcher::engine;\n"),
        with(
            "src/daemon/mod.rs",
            "\nasync fn spawn(client: Client) {\n    crate::launcher::run(&context, client, options, &log).await;\n}\n",
        ),
        with("src/processor/mod.rs", "\nuse crate::launcher;\n"),
        with(
            "src/server.rs",
            "\nuse crate::{docker, launcher::Launch};\n",
        ),
    ];
    for (index, case) in cases.iter().enumerate() {
        assert!(!problems(case).is_empty(), "case {index} was not refused");
    }
}
