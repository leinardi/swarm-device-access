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
//! the two write calls of rust-common's docker-engine client, its sanctioned `pub`
//! `Client::exchange` and `Client::exchange_upgrade` (the crate's `write` and `attach`, which
//! only the root package enables: `docker_boundary.rs`). The daemon's `docker::Client` hands
//! that client out through one crate-private hook, `Client::transport`. This pins it on the text
//! of the non-test sources under `src/`:
//!
//! - the hook stays `pub(crate)`, declared once in `src/docker/api.rs`, and only
//!   `src/launcher/engine.rs` calls it or a write call; nothing in-tree declares a function of
//!   the write calls' names, so no wrapper can widen them;
//! - only `src/launcher/engine.rs` sets a request's method or names a write method (`POST`,
//!   `PUT`, `DELETE`, `PATCH`, or one parsed from text): `src/docker` names no method at all
//!   (`docker_boundary.rs`), and the metrics server (`src/server.rs`, on go-http) neither;
//! - outside `src/launcher`, nothing names the engine: its module, `Engine`, `EngineApi`, or a
//!   launcher path to them;
//! - outside `src/launcher` and the CLI entry modules that run the `launch` subcommand
//!   (`src/main.rs`, `src/run.rs`, `src/cli.rs`, `src/cli/**`), nothing names the launcher at
//!   all, by a path or an import: its `run` and `Launch` build the engine from a plain client.
//!
//! The checks are functions of the text, and each is also run on input built to break it.

use std::sync::LazyLock;

use meta::repo;
use regex::Regex;

/// The launcher's write engine.
const ENGINE: &str = "src/launcher/engine.rs";
/// Where the hook is declared.
const CLIENT: &str = "src/docker/api.rs";
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

/// A function of a write call's name or of the hook's, with its visibility.
static HOOK_DECLARED: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"(?:\bpub\b[^\n]*?)?\bfn\s+(?:exchange(?:_upgrade)?|transport)\b"));
/// A function handing out docker-engine's client (whatever its name), or the client renamed so
/// one could hand it out under another type name.
static CLIENT_HANDED_OUT: LazyLock<Regex> = LazyLock::new(|| {
    pattern(
        r"\bfn\s+\w+[^{;]*->[^{;]*\bdocker_engine\s*::\s*Client\b|\bdocker_engine\s*::\s*(?:\{[^}]*\b)?Client\s+as\b",
    )
});
/// A use of a write call or of the hook: a method call or a path to it.
static HOOK_CALLED: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"(?:\.|::)\s*(?:exchange(?:_upgrade)?|transport)\b"));
/// A write method named or set: a `Method` constant or conversion, a `Request` shortcut, a
/// method setter or a write method's name as a string.
static WRITE_METHOD: LazyLock<Regex> = LazyLock::new(|| {
    pattern(
        r"\bMethod\s*::\s*(?:POST|PUT|DELETE|PATCH|from_\w+)\b|\bRequest\s*::\s*(?:post|put|delete|patch)\b|\bmethod_mut\s*\(|\.method\s*\(\s*[^)\s]|\x22(?:POST|PUT|DELETE|PATCH)\x22",
    )
});
/// The launcher's write engine, by module path or by type.
static WRITE_ENGINE: LazyLock<Regex> = LazyLock::new(|| {
    pattern(
        r"\blauncher\s*::\s*(?:engine\b|Engine\b|EngineApi\b|\{[^}]*\bEngine)|\bEngineApi\b|\bEngine\s*::",
    )
});
/// The launcher module, by path or in a `use`.
static LAUNCHER_MODULE: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"\blauncher\s*::|\buse\s[^;]*\blauncher\b"));

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

/// What hands out docker-engine's client besides `transport` in the client file.
fn handout_problems(files: &[(String, String)]) -> Vec<String> {
    let mut problems = Vec::new();
    let mut handed_out = 0;
    for (path, text) in files {
        for found in CLIENT_HANDED_OUT.find_iter(text) {
            if path == CLIENT && found.as_str().starts_with("fn transport(") {
                handed_out += 1;
            } else {
                problems.push(format!(
                    "{path}:{}: {}: only transport in {CLIENT} hands out docker-engine's client",
                    line_of(text, found.start()),
                    found.as_str()
                ));
            }
        }
    }
    if handed_out != 1 {
        problems.push(format!(
            "transport in {CLIENT} hands out docker-engine's client {handed_out} times, want once"
        ));
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

    // The hook: declared crate-private in the client, and nowhere else; it and the write calls
    // used only by the engine.
    let mut hooks = Vec::new();
    for (path, text) in &files {
        for found in HOOK_DECLARED.find_iter(text) {
            let site = format!(
                "{path}:{}: {}",
                line_of(text, found.start()),
                found.as_str()
            );
            if path != CLIENT || found.as_str() != "pub(crate) fn transport" {
                problems.push(format!(
                    "{site}: only the hook is declared, pub(crate) fn transport in {CLIENT}"
                ));
            }
            hooks.push(found.as_str().to_owned());
        }
    }
    if hooks.len() != 1 {
        problems.push(format!("hooks declared: {hooks:?}; want transport alone"));
    }
    problems.extend(handout_problems(&files));
    for site in found_in(&files, &HOOK_CALLED, |path| path == ENGINE) {
        problems.push(format!(
            "{site}: only {ENGINE} takes the transport and calls the writes"
        ));
    }

    // HTTP methods: only the engine sets one or names a write method.
    for site in found_in(&files, &WRITE_METHOD, |path| path == ENGINE) {
        problems.push(format!("{site}: only {ENGINE} names a write method"));
    }

    // The engine itself: named only under src/launcher.
    for site in found_in(&files, &WRITE_ENGINE, |path| {
        path.starts_with("src/launcher/")
    }) {
        problems.push(format!("{site}: only src/launcher names the write engine"));
    }

    // The launcher itself: reached only from the CLI entry.
    for site in found_in(&files, &LAUNCHER_MODULE, |path| {
        path.starts_with("src/launcher/")
            || CLI_ENTRY.iter().any(|entry| entry_matches(entry, path))
    }) {
        problems.push(format!(
            "{site}: only src/launcher and the CLI entry {CLI_ENTRY:?} name the launcher"
        ));
    }
    problems
}

/// The crate's sources: everything under `src/` but the unit tests (`tests.rs` and the modules
/// under a `tests/` directory) and the test-only fakes.
fn sources() -> Vec<(String, String)> {
    repo::tracked_or_new(&["src/*.rs"])
        .into_iter()
        .filter(|path| repo::path(path).is_file())
        .filter(|path| !matches!(path.rsplit('/').next(), Some("tests.rs" | "fake.rs")))
        .filter(|path| !path.contains("/tests/"))
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
            *text = client.replace("pub(crate) fn transport", "pub fn transport");
        }
    }
    assert_ne!(widened, files, "the hook moved");
    let cases = [
        widened,
        with(
            CLIENT,
            "\npub(crate) async fn exchange(&self) {\n    self.inner.exchange(\"/x\", \"\", |_| {}).await;\n}\n",
        ),
        with(
            "src/daemon/mod.rs",
            "\nfn raw(client: &Client) -> &docker_engine::Client {\n    client.transport()\n}\n",
        ),
        with(
            "src/processor/reconcile.rs",
            "\nasync fn kill(client: &Client) {\n    docker_engine::Client::exchange(client.transport(), \"/x\", \"\", |_| {}).await;\n}\n",
        ),
        with(
            "src/docker/api.rs",
            "\nfn shape(request: &mut Request) {\n    *request.method_mut() = Method::POST;\n}\n",
        ),
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
        with(
            "src/server.rs",
            "\nfn verb() -> Method {\n    Method::DELETE\n}\n",
        ),
        with(
            "src/server.rs",
            "\nfn verb() -> Method {\n    Method::from_bytes(b\"KILL\").unwrap()\n}\n",
        ),
        with(
            "src/daemon/mod.rs",
            "\nfn remove() -> Builder {\n    Request::delete(\"/containers/x\")\n}\n",
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

/// Negative control: a second function handing out docker-engine's client from the client
/// file, under another name or through a renamed import, is refused.
#[test]
fn a_second_handout_of_the_client_is_refused() {
    let files = sources();
    assert!(problems(&files).is_empty());
    for extra in [
        "\npub(crate) fn raw(&self) -> &docker_engine::Client {\n    &self.inner\n}\n",
        "\nuse docker_engine::Client as Transport;\nfn raw(client: &Client) -> &Transport {\n    &client.inner\n}\n",
    ] {
        let mut changed = files.clone();
        let Some((_, text)) = changed.iter_mut().find(|(path, _)| path == CLIENT) else {
            panic!("{CLIENT} not scanned");
        };
        text.push_str(extra);
        assert!(!problems(&changed).is_empty(), "accepted: {extra}");
    }
}
