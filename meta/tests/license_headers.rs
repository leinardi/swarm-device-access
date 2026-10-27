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
// Adapted from swarm-scheduler-exporter/meta/tests/license_headers.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! Drives scripts/check-license-headers.sh: on this repository, and on scratch repositories that
//! hold each kind of file it must reject or let through. The Rust port of `test/licenseheaders`,
//! which the cutover removed; the cases are that test's, since the script is this project's
//! (the Apache-2.0 notice, the Rust ports of NVIDIA's four files, the provenance comment),
//! grouped into several tests as SSE's are so they run in parallel. The Go cases went with the Go
//! code: a `.go` file is no longer governed, and the window and testdata cases moved to Rust and
//! shell files.
//!
//! Every case that must be refused is the negative control of one that must pass: a missing
//! header, another licence, another holder, NVIDIA's header outside its files, a header past the
//! window, an incomplete provenance comment and a pasted MIT block. The scratch repositories run
//! git and the script with an isolated git environment (`meta::gitenv`), so under the commit hook
//! they never touch the index being committed.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

use meta::{gitenv, repo};

const SCRIPT_PATH: &str = "scripts/check-license-headers.sh";

/// Scratch files under this prefix are written but never added: the check reads tracked files
/// only.
const UNTRACKED_PREFIX: &str = "untracked/";

/// The Apache-2.0 notice below the copyright line, as a Rust file carries it.
macro_rules! apache_body {
    () => {
        concat!(
            " *\n",
            " * Licensed under the Apache License, Version 2.0 (the \x22License\x22);\n",
            " * you may not use this file except in compliance with the License.\n",
            " * You may obtain a copy of the License at\n",
            " *\n",
            " *     https://www.apache.org/licenses/LICENSE-2.0\n",
            " *\n",
            " * Unless required by applicable law or agreed to in writing, software\n",
            " * distributed under the License is distributed on an \x22AS IS\x22 BASIS,\n",
            " * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.\n",
            " * See the License for the specific language governing permissions and\n",
            " * limitations under the License.\n",
            " */\n",
        )
    };
}

/// This project's Apache-2.0 block as a Rust file carries it.
macro_rules! apache_block {
    () => {
        concat!("/*\n * Copyright 2026 Roberto Leinardi.\n", apache_body!())
    };
}

/// NVIDIA's block, kept by the files preserved from NVIDIA's code.
macro_rules! nvidia_block {
    () => {
        concat!(
            "/*\n * Copyright (c) 2021, NVIDIA CORPORATION.  All rights reserved.\n",
            apache_body!()
        )
    };
}

/// A Rust source with the header, followed by its module comment. Its last required line is its
/// twelfth.
const RUST_HEADER: &str = concat!(apache_block!(), "\n//! A module.\n");

/// The Rust port of an NVIDIA file: NVIDIA's block, then a modification notice.
const RUST_NVIDIA_HEADER: &str = concat!(
    nvidia_block!(),
    "\n// Modified by Roberto Leinardi: ported to Rust.\n\n//! The cgroup API.\n"
);

/// This project's Apache-2.0 block alone.
const APACHE_BLOCK: &str = apache_block!();

/// The provenance comment of a reused Rust file, on one line.
const PROVENANCE_LINE: &str = concat!(
    "// Adapted from swarm-scheduler-exporter/src/clock.rs at d57dcc220d29, ",
    "originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi\n"
);

/// The same comment wrapped over three lines, as a long path forces it to be.
const PROVENANCE_WRAPPED: &str = concat!(
    "// Adapted from swarm-scheduler-exporter/src/collector/reconciler/listener.rs\n",
    "// at d57dcc220d29, originally MIT-licensed,\n",
    "// Copyright (c) 2026 Roberto Leinardi\n"
);

/// Another licence altogether, with the right copyright line.
const MIT_HEADER: &str = concat!(
    "/*\n",
    " * Copyright 2026 Roberto Leinardi.\n",
    " *\n",
    " * Permission is hereby granted, free of charge, to any person obtaining a copy\n",
    " */\n",
    "\n",
    "package x\n"
);

/// The opening of the MIT licence's permission paragraph.
const MIT_PERMISSION: &str =
    " * Permission is hereby granted, free of charge, to any person obtaining a copy\n";

const SHELL_HEADER: &str = concat!(
    "#!/usr/bin/env bash\n",
    "#\n",
    "# Copyright 2026 Roberto Leinardi.\n",
    "#\n",
    "# Licensed under the Apache License, Version 2.0 (the \x22License\x22);\n",
    "# you may not use this file except in compliance with the License.\n",
    "# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.\n",
    "echo ok\n"
);

const MAKEFILE: &str = "MK_COMMON_FILES := docker.mk help.mk\n";

/// The scratch repositories' Makefile, which the check governs too.
const MAKEFILE_WITH_HEADER: &str = concat!(
    "#\n# Copyright 2026 Roberto Leinardi.\n#\n",
    "# Licensed under the Apache License, Version 2.0 (the \x22License\x22);\n",
    "# you may not use this file except in compliance with the License.\n",
    "# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.\n",
    "MK_COMMON_FILES := docker.mk help.mk\n"
);

fn run_git(directory: &Path, args: &[&str]) {
    let output = gitenv::isolate(Command::new("git").args(args).current_dir(directory))
        .output()
        .unwrap_or_else(|error| panic!("run git {args:?}: {error}"));
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A git repository holding the checker, a Makefile (unless the case brings its own) and `files`,
/// every one of them added except those under [`UNTRACKED_PREFIX`].
fn scratch_repo(files: &[(&str, String)]) -> tempfile::TempDir {
    let scratch = tempfile::tempdir().unwrap_or_else(|error| panic!("scratch directory: {error}"));
    let directory = scratch.path();
    let mut files = files.to_vec();
    files.push((SCRIPT_PATH, repo::read(SCRIPT_PATH)));
    if !files.iter().any(|(name, _)| *name == "Makefile") {
        files.push(("Makefile", MAKEFILE_WITH_HEADER.to_owned()));
    }

    for (name, content) in &files {
        let path = directory.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .unwrap_or_else(|error| panic!("create the directory of {name}: {error}"));
        }
        fs::write(&path, content).unwrap_or_else(|error| panic!("write {name}: {error}"));
    }
    fs::set_permissions(
        directory.join(SCRIPT_PATH),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap_or_else(|error| panic!("make the checker executable: {error}"));

    run_git(directory, &["init", "--quiet"]);
    for (name, _) in &files {
        if !name.starts_with(UNTRACKED_PREFIX) {
            run_git(directory, &["add", "--", name]);
        }
    }
    scratch
}

/// Runs the checker kept at [`SCRIPT_PATH`] inside the repository at `directory`.
fn run_checker(directory: &Path) -> Output {
    gitenv::isolate(Command::new(directory.join(SCRIPT_PATH)).current_dir(directory))
        .output()
        .unwrap_or_else(|error| panic!("run the checker: {error}"))
}

/// Go: `TestRepositoryHeaders`. The repository passes its own check. Run with the repository's
/// own git environment: under the commit hook it names the index being committed.
#[test]
fn repository_headers() {
    let output = Command::new(repo::path(SCRIPT_PATH))
        .current_dir(repo::root())
        .output()
        .expect("run the checker");
    assert!(
        output.status.success(),
        "the repository fails its own license header check:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// One scratch repository: its files, and whether the checker must accept it.
struct HeaderCase {
    name: &'static str,
    files: Vec<(&'static str, String)>,
    pass: bool,
}

fn case(name: &'static str, files: &[(&'static str, &str)], pass: bool) -> HeaderCase {
    HeaderCase {
        name,
        files: files
            .iter()
            .map(|(path, content)| (*path, (*content).to_owned()))
            .collect(),
        pass,
    }
}

fn run_cases(cases: Vec<HeaderCase>) {
    let mut failures = Vec::new();
    for HeaderCase { name, files, pass } in cases {
        let scratch = scratch_repo(&files);
        let output = run_checker(scratch.path());
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if pass && !output.status.success() {
            failures.push(format!("{name}: rejected, want accepted:\n{text}"));
        }
        if !pass && output.status.success() {
            failures.push(format!("{name}: accepted, want rejected:\n{text}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Go: `TestHeaderCases`, the edges of the window the header must sit in. `RUST_HEADER`'s last
/// required line is its twelfth: 13 lines before it put it on line 25, the last line of the
/// window, and one more pushes it out.
#[test]
fn header_cases_for_the_window() {
    let at_the_edge = format!("{}{RUST_HEADER}", "// filler\n".repeat(13));
    let past_the_edge = format!("{}{RUST_HEADER}", "// filler\n".repeat(14));
    run_cases(vec![
        case(
            "header ending on the last line of the window",
            &[("src/x.rs", &at_the_edge)],
            true,
        ),
        case(
            "header ending one line past the window",
            &[("src/x.rs", &past_the_edge)],
            false,
        ),
    ]);
}

/// Go files are no longer governed: the Go code was removed at the Rust cutover. A tracked `.go`
/// file passes without any header, NVIDIA's former Go originals included; the window cases above
/// and the `.rs` cases below are its negative controls.
#[test]
fn go_files_are_not_governed() {
    run_cases(vec![
        case(
            "go file without a header",
            &[("internal/x/x.go", "package x\n")],
            true,
        ),
        case(
            "go file with another license",
            &[("internal/x/x.go", MIT_HEADER)],
            true,
        ),
        case(
            "former nvidia go original without a header",
            &[("internal/cgroup/api.go", "package cgroup\n")],
            true,
        ),
    ]);
}

/// Go: `TestHeaderCases`, every other governed file kind with and without the header, and what is
/// never governed: untracked files, make-common's files, Markdown, YAML and TOML.
#[test]
fn header_cases_for_other_kinds_and_exemptions() {
    run_cases(vec![
        case(
            "shell script with the header",
            &[("scripts/a.sh", SHELL_HEADER)],
            true,
        ),
        case(
            "shell script without a header",
            &[("scripts/a.sh", "#!/usr/bin/env bash\necho\n")],
            false,
        ),
        case(
            "dockerfile without a header",
            &[("deployments/docker/Dockerfile", "FROM scratch\n")],
            false,
        ),
        case(
            "shell script under testdata, no header",
            &[("tests/testdata/x.sh", "#!/usr/bin/env bash\necho\n")],
            false,
        ),
        case(
            "local make fragment, no header",
            &[(".mk/local.mk", "x:\n")],
            false,
        ),
        case(
            "makefile without a header",
            &[("Makefile", MAKEFILE)],
            false,
        ),
        case(
            "untracked file without a header",
            &[("untracked/scratch.sh", "#!/usr/bin/env bash\necho\n")],
            true,
        ),
        case(
            "upstream make fragment",
            &[(".mk/docker.mk", "x:\n"), (".mk/help.mk", "y:\n")],
            true,
        ),
        case(
            "bootstrap script",
            &[("scripts/bootstrap-mk-common.sh", "#!/bin/sh\n")],
            true,
        ),
        case(
            "markdown and yaml",
            &[("README.md", "# x\n"), ("a.yaml", "a: 1\n")],
            true,
        ),
        case(
            "cargo manifest and toml",
            &[("Cargo.toml", "[package]\n"), ("clippy.toml", "x = 1\n")],
            true,
        ),
    ]);
}

/// Go: `TestHeaderCases`, Rust: every .rs file of every package is governed, except the fixtures
/// under a testdata/ directory.
#[test]
fn header_cases_for_rust() {
    let other_holder = RUST_HEADER.replace("Roberto Leinardi", "Someone Else");
    run_cases(vec![
        case(
            "rust file with the header",
            &[("src/x.rs", RUST_HEADER)],
            true,
        ),
        case(
            "rust file without a header",
            &[("src/x.rs", "//! A module.\n")],
            false,
        ),
        case(
            "rust file with another license",
            &[("src/x.rs", MIT_HEADER)],
            false,
        ),
        case(
            "rust test without a header",
            &[("tests/run.rs", "fn main() {}\n")],
            false,
        ),
        case(
            "rust build script, no header",
            &[("build.rs", "fn main() {}\n")],
            false,
        ),
        case(
            "meta crate file without a header",
            &[("meta/tests/x.rs", "fn main() {}\n")],
            false,
        ),
        case(
            "testenv file without a header",
            &[("testenv/src/lib.rs", "//! x\n")],
            false,
        ),
        case(
            "another holder on a rust file",
            &[("src/x.rs", &other_holder)],
            false,
        ),
        case(
            "rust fixture under testdata",
            &[("tests/testdata/fixture.rs", "fn main() {}\n")],
            true,
        ),
        case(
            "rust file next to testdata",
            &[("tests/testdatax/x.rs", "fn main() {}\n")],
            false,
        ),
    ]);
}

/// Go: `TestHeaderCases`, NVIDIA's header: the four Rust ports, and no other file.
#[test]
fn header_cases_for_the_nvidia_ports() {
    let nvidia_shell = SHELL_HEADER.replace(
        "Copyright 2026 Roberto Leinardi.",
        "Copyright (c) 2021, NVIDIA CORPORATION.",
    );
    run_cases(vec![
        case(
            "nvidia header on the rust api port",
            &[("src/cgroup/api.rs", RUST_NVIDIA_HEADER)],
            true,
        ),
        case(
            "nvidia header on the rust ebpf port",
            &[("src/cgroup/ebpf.rs", RUST_NVIDIA_HEADER)],
            true,
        ),
        case(
            "nvidia header on the rust v1 port",
            &[("src/cgroup/v1.rs", RUST_NVIDIA_HEADER)],
            true,
        ),
        case(
            "nvidia header on the rust v2 port",
            &[("src/cgroup/v2.rs", RUST_NVIDIA_HEADER)],
            true,
        ),
        case(
            "nvidia header on another rust file",
            &[("src/cgroup/v2plan.rs", RUST_NVIDIA_HEADER)],
            false,
        ),
        case(
            "nvidia header on a shell script",
            &[("scripts/a.sh", &nvidia_shell)],
            false,
        ),
        case(
            "own header on a rust nvidia port",
            &[("src/cgroup/v2.rs", RUST_HEADER)],
            false,
        ),
    ]);
}

/// Go: `TestHeaderCases`, the provenance comment of a reused file: accepted, not required, and
/// complete if present; and the MIT permission text refused.
#[test]
fn header_cases_for_the_provenance_comment() {
    let one_line = format!("{APACHE_BLOCK}{PROVENANCE_LINE}\n//! The clock.\n");
    let wrapped = format!("{APACHE_BLOCK}{PROVENANCE_WRAPPED}\n//! The listener.\n");
    let without_commit = format!(
        "{APACHE_BLOCK}{}",
        PROVENANCE_LINE.replacen(" at d57dcc220d29", "", 1)
    );
    let other_holder = format!(
        "{APACHE_BLOCK}{}",
        PROVENANCE_LINE.replacen("Roberto Leinardi", "Someone Else", 1)
    );
    let cut_short = format!(
        "{APACHE_BLOCK}// Adapted from swarm-scheduler-exporter/src/clock.rs\n\n//! The clock.\n"
    );
    let without_header = format!("{PROVENANCE_LINE}\n//! The clock.\n");
    let pasted_mit = format!(
        "{}{PROVENANCE_LINE}",
        APACHE_BLOCK.replacen(" *\n", &format!(" *\n{MIT_PERMISSION}"), 1)
    );
    run_cases(vec![
        case(
            "provenance on one line",
            &[("src/clock.rs", &one_line)],
            true,
        ),
        case("provenance wrapped", &[("src/listener.rs", &wrapped)], true),
        case(
            "provenance without a commit",
            &[("src/clock.rs", &without_commit)],
            false,
        ),
        case(
            "provenance with another holder",
            &[("src/clock.rs", &other_holder)],
            false,
        ),
        case(
            "provenance cut short",
            &[("src/clock.rs", &cut_short)],
            false,
        ),
        case(
            "provenance without the header",
            &[("src/clock.rs", &without_header)],
            false,
        ),
        case(
            "pasted mit block on a rust file",
            &[("src/clock.rs", &pasted_mit)],
            false,
        ),
    ]);
}
