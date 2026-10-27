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

//! The workspace denies the lint against `unsafe` code, and exactly one module may lift it:
//! `src/cgroup/bpf_sys.rs`, the raw `bpf(2)` calls the cgroup v2 device filter needs
//! (`docs/rust-port.md`, eBPF). This pins both halves:
//!
//! - the root manifest sets the lint to `deny` (not `warn` or `allow`, which would let `unsafe`
//!   through, and not `forbid`, which nothing could lift), and neither Cargo configuration nor a
//!   make target names it;
//! - the lint's name appears in no Rust file, tracked or new, but the allowlisted one, and there
//!   only in one file-level `#![expect(<lint>, reason = "...")]`. Any other mention (an
//!   `#[allow]`, an item-level `#[expect]`, a `cfg_attr`, even a comment) is refused: the rule is
//!   about the name, so no attribute syntax this scan does not model can slip past it.
//!
//! The same module is the only one that may call `libc`: the raw system call is what the lint is
//! lifted for, and a `libc::` call elsewhere would be a way around the audited module even
//! without `unsafe` (a safe `libc` function still reaches the kernel unchecked).
//!
//! The lint's name is assembled at compile time below, so this file does not mention it itself.
//! The checks are functions of the text, and each is also run on input built to break it.

use std::fs;
use std::sync::LazyLock;

use meta::repo;
use regex::Regex;
use toml::Table;

/// The lint, spelled so that this file never contains its name.
const LINT: &str = concat!("unsafe", "_code");

/// This test's own path.
const THIS_FILE: &str = "meta/tests/unsafe_allowlist.rs";

/// The only Rust files that may lift the lint.
const ALLOWLIST: &[&str] = &["src/cgroup/bpf_sys.rs"];

/// Files that could change the lint's level from outside the manifest: Cargo configuration
/// (`rustflags`) and the make targets that run cargo (`RUSTFLAGS`). A missing file is fine.
const LEVEL_OVERRIDES: &[&str] = &[".cargo/config.toml", ".cargo/config", "Makefile"];

fn pattern(text: &str) -> Regex {
    Regex::new(text).unwrap_or_else(|error| panic!("pattern {text}: {error}"))
}

/// The lint's name as a whole word.
static LINT_WORD: LazyLock<Regex> = LazyLock::new(|| pattern(&format!(r"\b{LINT}\b")));

/// The one form the allowlisted module may use: a file-level expect with a non-empty reason.
static FILE_LEVEL_EXPECT: LazyLock<Regex> = LazyLock::new(|| {
    pattern(&format!(
        r#"(?m)^#!\[expect\(\s*{LINT}\s*,\s*reason\s*=\s*"[^"]+"\s*\)\]"#
    ))
});

/// What is wrong with how one Rust file names the lint, if anything.
fn file_problems(path: &str, text: &str) -> Vec<String> {
    let mentions: Vec<usize> = LINT_WORD
        .find_iter(text)
        .map(|found| text[..found.start()].matches('\n').count() + 1)
        .collect();
    if mentions.is_empty() {
        return Vec::new();
    }
    if !ALLOWLIST.contains(&path) {
        return mentions
            .iter()
            .map(|line| format!("{path}:{line}: names {LINT}, which only {ALLOWLIST:?} may lift"))
            .collect();
    }
    let expects = FILE_LEVEL_EXPECT.find_iter(text).count();
    if expects == 1 && mentions.len() == 1 {
        return Vec::new();
    }
    vec![format!(
        "{path}: names {LINT} {} time(s), {expects} of them in a file-level \
         #![expect(..., reason = \"...\")]; want exactly one, and no other mention",
        mentions.len()
    )]
}

/// A use of the `libc` crate in code: a path through it, an import or an `extern crate`.
static LIBC_USE: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"\blibc\s*::|\buse\s+libc\b|\bextern\s+crate\s+libc\b"));

/// Where a Rust file outside the allowlist uses `libc`, if it does.
fn libc_problems(path: &str, text: &str) -> Vec<String> {
    if ALLOWLIST.contains(&path) {
        return Vec::new();
    }
    LIBC_USE
        .find_iter(text)
        .map(|found| {
            let line = text[..found.start()].matches('\n').count() + 1;
            format!("{path}:{line}: uses libc, which only {ALLOWLIST:?} may call")
        })
        .collect()
}

/// What is wrong with the root manifest's level for the lint, if anything.
fn manifest_problems(root: &Table) -> Vec<String> {
    let level = root
        .get("workspace")
        .and_then(|workspace| workspace.get("lints"))
        .and_then(|lints| lints.get("rust"))
        .and_then(|rust| rust.get(LINT));
    let level = level.and_then(|value| {
        value
            .as_str()
            .or_else(|| value.get("level").and_then(toml::Value::as_str))
    });
    match level {
        Some("deny") => Vec::new(),
        Some(other) => vec![format!(
            "[workspace.lints.rust] sets {LINT} = {other:?}; it must be \"deny\""
        )],
        None => vec![format!("[workspace.lints.rust] does not set {LINT}")],
    }
}

#[test]
fn the_workspace_denies_the_lint() {
    let root = repo::read("Cargo.toml")
        .parse::<Table>()
        .unwrap_or_else(|error| panic!("parse Cargo.toml: {error}"));
    let problems = manifest_problems(&root);
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn nothing_outside_the_manifest_names_the_lint() {
    let make_fragments = repo::tracked_or_new(&[".mk/*.mk"]);
    assert!(
        make_fragments.iter().any(|path| path == ".mk/rust.mk"),
        "the scan saw no make fragment, not even .mk/rust.mk: {make_fragments:?}"
    );
    for file in LEVEL_OVERRIDES
        .iter()
        .map(|path| (*path).to_owned())
        .chain(make_fragments)
    {
        let Ok(text) = fs::read_to_string(repo::path(&file)) else {
            continue;
        };
        assert!(
            !LINT_WORD.is_match(&text),
            "{file} names {LINT}: rustflags must not change its level"
        );
    }
}

#[test]
fn only_the_allowlisted_module_lifts_the_lint() {
    let files = repo::tracked_or_new(&["*.rs"]);
    assert!(
        files.iter().any(|path| path == THIS_FILE),
        "the scan saw no Rust file at all, not even this one: {files:?}"
    );
    // A tracked file deleted in the worktree is about to go; it cannot lift anything.
    let problems: Vec<String> = files
        .iter()
        .filter(|path| repo::path(path).is_file())
        .flat_map(|path| file_problems(path, &repo::read(path)))
        .collect();
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn only_the_allowlisted_module_calls_libc() {
    let files = repo::tracked_or_new(&["*.rs"]);
    let allowed = ALLOWLIST[0];
    assert!(
        LIBC_USE.is_match(&repo::read(allowed)),
        "{allowed} no longer calls libc: the scan would pass on nothing"
    );
    // This file names libc in its fixtures; it calls nothing.
    let problems: Vec<String> = files
        .iter()
        .filter(|path| *path != THIS_FILE && repo::path(path).is_file())
        .flat_map(|path| libc_problems(path, &repo::read(path)))
        .collect();
    assert!(
        files.iter().any(|path| path == "src/cgroup/v2ops.rs"),
        "the scan saw no daemon source: {files:?}"
    );
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// Negative control: every way to reach `libc` outside the allowlist is refused, and a mention
/// that is not a use is not.
#[test]
fn a_libc_use_outside_the_allowlist_is_refused() {
    for text in [
        "fn run() { unsafe { libc::getpid() }; }\n",
        "use libc::c_int;\n",
        "use libc;\n",
        "extern crate libc;\n",
        "let id = libc :: SYS_bpf;\n",
    ] {
        assert_eq!(
            libc_problems("src/cgroup/v2ops.rs", text).len(),
            1,
            "accepted outside the allowlist:\n{text}"
        );
        assert!(libc_problems(ALLOWLIST[0], text).is_empty());
    }
    assert!(libc_problems("src/x.rs", "// see src/libcrun/ebpf.c and glibc\n").is_empty());
}

/// Negative control: every way to name the lint outside the allowlist is refused, wherever the
/// mention sits.
#[test]
fn a_mention_outside_the_allowlist_is_refused() {
    for text in [
        format!("#![expect({LINT}, reason = \"x\")]\n"),
        format!("#[allow({LINT})]\nfn run() {{}}\n"),
        format!("#[expect(\n    clippy::too_many_lines,\n    {LINT},\n    reason = \"x\"\n)]\n"),
        format!("#![cfg_attr(test, expect({LINT}, reason = \"x\"))]\n"),
        format!("// {LINT} is fine here, says a comment\n"),
    ] {
        assert_eq!(
            file_problems("src/cgroup/v2.rs", &text).len(),
            1,
            "accepted outside the allowlist:\n{text}"
        );
    }
    let two = format!("fn run() {{}}\n#[allow({LINT})]\n#[allow({LINT})]\nfn walk() {{}}\n");
    assert_eq!(
        file_problems("src/x.rs", &two),
        [
            format!("src/x.rs:2: names {LINT}, which only {ALLOWLIST:?} may lift"),
            format!("src/x.rs:3: names {LINT}, which only {ALLOWLIST:?} may lift"),
        ]
    );
}

/// Negative control: in the allowlisted module, only one file-level expect with a reason passes.
#[test]
fn the_allowlisted_module_uses_only_the_file_level_expect() {
    let allowed = ALLOWLIST[0];
    let good = format!("//! Docs.\n\n#![expect({LINT}, reason = \"raw bpf(2) calls\")]\n");
    assert!(file_problems(allowed, &good).is_empty());
    for text in [
        format!("#![allow({LINT})]\n"),
        format!("#![expect({LINT})]\n"),
        format!("#![expect({LINT}, reason = \"\")]\n"),
        format!("#[expect({LINT}, reason = \"item level\")]\nfn run() {{}}\n"),
        format!("{good}#[allow({LINT})]\nfn run() {{}}\n"),
        format!("{good}{good}"),
    ] {
        assert!(
            !file_problems(allowed, &text).is_empty(),
            "accepted in the allowlisted module:\n{text}"
        );
    }
}

/// Negative control: the manifest check refuses every level but deny, and a missing one.
#[test]
fn a_level_other_than_deny_is_refused() {
    for (level, refused) in [
        ("\"deny\"", false),
        ("{ level = \"deny\", priority = 0 }", false),
        ("\"forbid\"", true),
        ("\"warn\"", true),
        ("\"allow\"", true),
    ] {
        let root = format!("[workspace.lints.rust]\n{LINT} = {level}\n")
            .parse::<Table>()
            .unwrap_or_else(|error| panic!("parse fixture: {error}"));
        assert_eq!(
            !manifest_problems(&root).is_empty(),
            refused,
            "level {level}"
        );
    }
    let missing = "[workspace.lints.rust]\n"
        .parse::<Table>()
        .unwrap_or_else(|error| panic!("parse fixture: {error}"));
    assert!(!manifest_problems(&missing).is_empty());
}
