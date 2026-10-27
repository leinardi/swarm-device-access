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
//! The configuration changes only through the processor (Go's
//! `TestPublisher_OnlyProcessorPublishes`): every other path would publish without taking the
//! processor's lock or requesting a reconciliation pass. Outside tests, `Publisher::publish` may
//! be named only under `src/processor/`, by a call, a path or a method reference alike, and there
//! exactly once.
//!
//! Until the processor's reconcile is ported (`docs/rust-port.md`), nothing may name it at all;
//! from the commit that defines `publish_and_reconcile` under `src/processor/`, exactly one
//! reference there is required, as in Go, which guards the scan itself. The device discovery
//! under `src/processor/` lands a phase earlier and publishes nothing. The checks are functions of the text, each also run on input built to
//! break it.

use meta::repo;
use regex::Regex;

/// Where the one publication lives.
const PROCESSOR: &str = "src/processor/";

/// The processor entry point that publishes; once it exists, the one reference is required.
const PUBLISHER: &str = "fn publish_and_reconcile(";

/// Where `publish` is defined; the scan must read it.
const DEFINITION: &str = "src/config/mod.rs";

fn reference() -> Regex {
    Regex::new(r"(?:\.|::)\s*publish\b").unwrap_or_else(|error| panic!("pattern: {error}"))
}

/// Whether a Rust file is test code: unit-test modules in their own file, integration tests,
/// and the test-only crates.
fn is_test_code(path: &str) -> bool {
    path.ends_with("/tests.rs")
        || path.contains("/tests/")
        || path.starts_with("tests/")
        || path.starts_with("meta/")
        || path.starts_with("testenv/")
}

/// The 1-based lines of `text` that name `publish` in code: comments and the contents of string
/// and character literals are blanked first, so neither a comment nor a `//` inside a string
/// hides or fakes a reference.
fn references(text: &str) -> Vec<usize> {
    code_only(text)
        .lines()
        .enumerate()
        .filter(|(_, line)| reference().is_match(line))
        .map(|(index, _)| index + 1)
        .collect()
}

/// A space for a blanked character, keeping newlines so line numbers hold.
fn blank(character: char) -> char {
    if character == '\n' { '\n' } else { ' ' }
}

/// `text` with comments and literal contents replaced by spaces, newlines kept: a small Rust
/// lexer for line and nested block comments, strings with escapes, raw strings and character
/// literals (a `'` that does not close within the literal's length is a lifetime).
fn code_only(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < chars.len() {
        let end = match &chars[index..] {
            ['/', '/', ..] => chars[index..]
                .iter()
                .position(|&character| character == '\n')
                .map_or(chars.len(), |offset| index + offset),
            ['/', '*', ..] => block_comment_end(&chars, index),
            ['r', '#' | '"', ..] if index == 0 || !is_ident(chars[index - 1]) => {
                raw_string_end(&chars, index).unwrap_or(index)
            }
            ['"', ..] => quoted_end(&chars, index, '"'),
            ['\'', '\\', ..] => quoted_end(&chars, index, '\''),
            ['\'', _, '\'', ..] => index + 3,
            _ => index,
        };
        if end == index {
            out.push(chars[index]);
            index += 1;
        } else {
            out.extend(chars[index..end].iter().map(|&character| blank(character)));
            index = end;
        }
    }
    out
}

/// The index after a block comment starting at `start`, nested comments included.
fn block_comment_end(chars: &[char], start: usize) -> usize {
    let mut depth = 0_usize;
    let mut index = start;
    while index < chars.len() {
        match &chars[index..] {
            ['/', '*', ..] => {
                depth += 1;
                index += 2;
            }
            ['*', '/', ..] => {
                depth -= 1;
                index += 2;
                if depth == 0 {
                    return index;
                }
            }
            _ => index += 1,
        }
    }
    chars.len()
}

/// The index after a raw string starting at `start` (`r`, hashes, `"`), if it is one.
fn raw_string_end(chars: &[char], start: usize) -> Option<usize> {
    let hashes = chars[start + 1..]
        .iter()
        .take_while(|&&character| character == '#')
        .count();
    if chars.get(start + 1 + hashes) != Some(&'"') {
        return None;
    }
    let close: Vec<char> = std::iter::once('"')
        .chain(std::iter::repeat_n('#', hashes))
        .collect();
    let mut index = start + 2 + hashes;
    while index < chars.len() && !chars[index..].starts_with(&close) {
        index += 1;
    }
    Some((index + close.len()).min(chars.len()))
}

/// The index after a literal opened by `quote` at `start`, backslash escapes skipped.
fn quoted_end(chars: &[char], start: usize, quote: char) -> usize {
    let mut index = start + 1;
    while index < chars.len() && chars[index] != quote {
        index += if chars[index] == '\\' { 2 } else { 1 };
    }
    (index + 1).min(chars.len())
}

/// Whether a character continues an identifier, so an `r` after it is not a raw-string prefix.
fn is_ident(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}
/// The references inside and outside the processor, over `(path, text)` pairs.
fn scan<'file>(files: impl Iterator<Item = (&'file str, String)>) -> (usize, Vec<String>) {
    let mut allowed = 0;
    let mut offenders = Vec::new();
    for (path, text) in files {
        if is_test_code(path) {
            continue;
        }
        for line in references(&text) {
            if path.starts_with(PROCESSOR) {
                allowed += 1;
            } else {
                offenders.push(format!("{path}:{line}"));
            }
        }
    }
    (allowed, offenders)
}

#[test]
fn only_the_processor_publishes() {
    let files = repo::tracked_or_new(&["*.rs"]);
    assert!(
        files.iter().any(|path| path == DEFINITION),
        "the scan does not see {DEFINITION}: {files:?}"
    );
    assert!(
        repo::read(DEFINITION).contains("pub fn publish("),
        "{DEFINITION} no longer defines publish; this test guards the wrong name"
    );
    let present: Vec<&String> = files
        .iter()
        .filter(|path| repo::path(path).is_file())
        .collect();
    let (allowed, offenders) = scan(present.iter().map(|path| (path.as_str(), repo::read(path))));
    assert!(
        offenders.is_empty(),
        "the config is published outside the processor: {offenders:?}"
    );
    let want = usize::from(
        present
            .iter()
            .any(|path| path.starts_with(PROCESSOR) && repo::read(path).contains(PUBLISHER)),
    );
    assert_eq!(
        allowed, want,
        "references to publish under {PROCESSOR}: {allowed}, want {want}"
    );
}

/// Negative control: every way of reaching `publish` outside the processor is refused, and a
/// comment or test code is not a reference.
#[test]
fn a_publication_outside_the_processor_is_refused() {
    for text in [
        "fn reload(publisher: &Publisher) {\n    publisher.publish(next);\n}\n",
        "Publisher::publish(&publisher, next);\n",
        "let publish = Publisher::publish;\n",
        "publisher\n    . publish(next);\n",
    ] {
        let (_, offenders) = scan([("src/run.rs", text.to_owned())].into_iter());
        assert_eq!(offenders.len(), 1, "accepted:\n{text}");
    }
    for text in [
        "let url = \"http://h\"; publisher.publish(next);\n",
        "let raw = r#\"a // b\"#; publisher.publish(next);\n",
        "let slash = '/'; let quote = '\"'; publisher.publish(next);\n",
        "/* a */ publisher.publish(next);\n",
        "fn run<'a>(publisher: &'a Publisher) { publisher.publish(next); }\n",
    ] {
        let (_, offenders) = scan([("src/run.rs", text.to_owned())].into_iter());
        assert_eq!(offenders, ["src/run.rs:1"], "missed:\n{text}");
    }
    for quiet in [
        "// publisher.publish(next) is the processor's job\nfn publish() {}\n",
        "/* publisher.publish(next);\n /* nested */ publisher.publish(next); */\n",
        "let text = \"publisher.publish(next)\";\n",
        "let text = r#\"publisher.publish(\"next\")\"#;\n",
    ] {
        assert_eq!(
            scan([("src/run.rs", quiet.to_owned())].into_iter()),
            (0, Vec::new()),
            "{quiet}"
        );
    }
    let test = "publisher.publish(next);\n";
    for path in [
        "src/config/tests.rs",
        "tests/config.rs",
        "src/daemon/tests/reload.rs",
    ] {
        assert_eq!(
            scan([(path, test.to_owned())].into_iter()),
            (0, Vec::new()),
            "{path}"
        );
    }
    assert_eq!(
        scan([("src/processor/mod.rs", test.to_owned())].into_iter()),
        (1, Vec::new())
    );
}
