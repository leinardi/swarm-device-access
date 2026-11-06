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
//! The level, override and allowlist checks are `repo_meta::workspace`'s and
//! `repo_meta::allowlist`'s (extracted from this file); the libc confinement is this
//! repository's. Each check is also run on input built to break it.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;
use repo_meta::allowlist::{Exemption, LintAllowlist, allowlist_problems};
use repo_meta::workspace::{LintLevel, lint_level_problems, override_problems};
use repo_meta::{report, strings};
use toml::Table;

/// The lint, spelled so that this file never contains its name.
const LINT: &str = concat!("unsafe", "_code");

/// This test's own path.
const THIS_FILE: &str = "meta/tests/unsafe_allowlist.rs";

/// The only Rust file that may lift the lint.
const ALLOWED: &str = "src/cgroup/bpf_sys.rs";

/// The only Rust files that may lift the lint (or call libc).
const ALLOWLIST: &[&str] = &[ALLOWED];

/// The lint lifted only by the allowlisted module's one file-level expect, which must be there
/// (not reserved): an exemption that matches nothing is a problem, so it cannot outlive the code.
const UNSAFE_ALLOWLIST: LintAllowlist<'static> = LintAllowlist {
    lint: LINT,
    exemptions: &[Exemption::FileLevel {
        file: ALLOWED,
        reserved: false,
    }],
};

/// The level the root manifest sets: deny, which only `#[expect]` can lift.
const LEVEL: LintLevel<'static> = LintLevel {
    table: "rust",
    lint: LINT,
    level: "deny",
};

/// Files that could change the lint's level from outside the manifest: Cargo configuration
/// (`rustflags`) and the make targets that run cargo (`RUSTFLAGS`). A missing file is fine.
const LEVEL_OVERRIDES: &[&str] = &[".cargo/config.toml", ".cargo/config", "Makefile"];

fn pattern(text: &str) -> Regex {
    Regex::new(text).unwrap_or_else(|error| panic!("pattern {text}: {error}"))
}

/// The allowlist's problems on `files`.
fn lint_problems(files: &[(&str, &str)]) -> Vec<String> {
    let files: BTreeMap<String, String> = files
        .iter()
        .map(|(path, text)| ((*path).to_owned(), (*text).to_owned()))
        .collect();
    strings(&allowlist_problems(&files, &[UNSAFE_ALLOWLIST], &[]))
}

/// The allowlisted module as it must read: one file-level expect with a reason.
fn good_module() -> String {
    format!("//! Docs.\n\n#![expect({LINT}, reason = \"raw bpf(2) calls\")]\n")
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

fn parse(text: &str) -> Table {
    text.parse::<Table>()
        .unwrap_or_else(|error| panic!("parse fixture: {error}"))
}

#[test]
fn the_workspace_denies_the_lint() {
    let root = meta::repo()
        .read("Cargo.toml")
        .parse::<Table>()
        .unwrap_or_else(|error| panic!("parse Cargo.toml: {error}"));
    let problems = lint_level_problems(&root, &[LEVEL]);
    assert!(problems.is_empty(), "{}", report(&problems));
}

/// The Cargo configuration, the Makefile and every make fragment that exists.
fn override_files() -> Vec<(String, String)> {
    let repo = meta::repo();
    let make_fragments = repo.tracked_or_new(&[".mk/*.mk"]);
    assert!(
        make_fragments.iter().any(|path| path == ".mk/rust.mk"),
        "the scan saw no make fragment, not even .mk/rust.mk: {make_fragments:?}"
    );
    let paths: Vec<String> = LEVEL_OVERRIDES
        .iter()
        .map(|path| (*path).to_owned())
        .filter(|path| repo.path(path).is_file())
        .chain(make_fragments)
        .collect();
    repo.read_all(&paths)
}

/// Neither Cargo configuration nor a make target names the lint, in a flag or anywhere else: the
/// match is on the text (`repo_meta::workspace::override_problems`).
#[test]
fn nothing_outside_the_manifest_names_the_lint() {
    let problems = override_problems(&override_files(), &[LINT]);
    assert!(problems.is_empty(), "{}", report(&problems));
}

/// Negative control: a make target or Cargo configuration naming the lint is refused, wherever
/// the mention sits.
#[test]
fn an_override_of_the_level_is_refused() {
    let files = override_files();
    assert!(override_problems(&files, &[LINT]).is_empty());
    for (file, extra) in [
        ("Makefile", format!("\nexport RUSTFLAGS += -A {LINT}\n")),
        (".mk/rust.mk", format!("\n# {LINT} is lifted here\n")),
        (
            ".cargo/config.toml",
            format!("[build]\nrustflags = [\"--warn={LINT}\"]\n"),
        ),
    ] {
        let mut changed = files.clone();
        match changed.iter_mut().find(|(path, _)| path == file) {
            Some((_, text)) => text.push_str(&extra),
            None => changed.push((file.to_owned(), extra)),
        }
        let problems = override_problems(&changed, &[LINT]);
        assert!(
            problems.len() == 1 && problems[0].location == file,
            "{file}: want one problem there, got {problems:?}"
        );
    }
}

/// The lint's name appears in no Rust file, tracked or new, but the allowlisted one, and there
/// only in its one file-level expect (`repo_meta::allowlist`).
#[test]
fn only_the_allowlisted_module_lifts_the_lint() {
    let repo = meta::repo();
    let paths = repo.tracked_or_new(&["*.rs"]);
    assert!(
        paths.iter().any(|path| path == THIS_FILE),
        "the scan saw no Rust file at all, not even this one: {paths:?}"
    );
    let files: BTreeMap<String, String> = repo.read_all(&paths).into_iter().collect();
    let problems = allowlist_problems(&files, &[UNSAFE_ALLOWLIST], &[]);
    assert!(problems.is_empty(), "{}", report(&problems));
}

#[test]
fn only_the_allowlisted_module_calls_libc() {
    let repo = meta::repo();
    let files = repo.tracked_or_new(&["*.rs"]);
    let allowed = ALLOWLIST[0];
    assert!(
        LIBC_USE.is_match(&repo.read(allowed)),
        "{allowed} no longer calls libc: the scan would pass on nothing"
    );
    // This file names libc in its fixtures; it calls nothing.
    let problems: Vec<String> = files
        .iter()
        .filter(|path| *path != THIS_FILE)
        .flat_map(|path| libc_problems(path, &repo.read(path)))
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
/// mention sits; the allowlisted module beside it passes.
#[test]
fn a_mention_outside_the_allowlist_is_refused() {
    let good = good_module();
    assert_eq!(lint_problems(&[(ALLOWED, &good)]), Vec::<String>::new());
    for text in [
        format!("#![expect({LINT}, reason = \"x\")]\n"),
        format!("#[allow({LINT})]\nfn run() {{}}\n"),
        format!("#[expect(\n    clippy::too_many_lines,\n    {LINT},\n    reason = \"x\"\n)]\n"),
        format!("#![cfg_attr(test, expect({LINT}, reason = \"x\"))]\n"),
        format!("// {LINT} is fine here, says a comment\n"),
    ] {
        let problems = lint_problems(&[(ALLOWED, &good), ("src/cgroup/v2.rs", &text)]);
        assert!(
            problems.len() == 1 && problems[0].starts_with("src/cgroup/v2.rs:"),
            "accepted outside the allowlist, or refused other than once:\n{text}\n{problems:?}"
        );
    }
    let two = format!("fn run() {{}}\n#[allow({LINT})]\n#[allow({LINT})]\nfn walk() {{}}\n");
    let problems = lint_problems(&[(ALLOWED, &good), ("src/x.rs", &two)]);
    let lines: Vec<&str> = problems
        .iter()
        .filter_map(|problem| problem.split(": ").next())
        .collect();
    assert_eq!(lines, ["src/x.rs:2", "src/x.rs:3"], "{problems:?}");
}

/// Negative control: in the allowlisted module, only one file-level expect with a reason passes,
/// and a module that no longer lifts the lint leaves an exemption that matches nothing.
#[test]
fn the_allowlisted_module_uses_only_the_file_level_expect() {
    let good = good_module();
    for text in [
        format!("#![allow({LINT})]\n"),
        format!("#![expect({LINT})]\n"),
        format!("#![expect({LINT}, reason = \"\")]\n"),
        format!("#[expect({LINT}, reason = \"item level\")]\nfn run() {{}}\n"),
        format!("{good}#[allow({LINT})]\nfn run() {{}}\n"),
        format!("{good}{good}"),
        "//! Docs, and no expect.\n".to_owned(),
    ] {
        assert!(
            !lint_problems(&[(ALLOWED, &text)]).is_empty(),
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
        let root = parse(&format!("[workspace.lints.rust]\n{LINT} = {level}\n"));
        assert_eq!(
            !lint_level_problems(&root, &[LEVEL]).is_empty(),
            refused,
            "level {level}"
        );
    }
    let missing = parse("[workspace.lints.rust]\n");
    assert!(!lint_level_problems(&missing, &[LEVEL]).is_empty());
}
