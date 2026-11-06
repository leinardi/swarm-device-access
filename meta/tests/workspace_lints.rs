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
// Adapted from swarm-scheduler-exporter/meta/tests/workspace_lints.rs at d57dcc220d29, originally
// MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The shared lint set is declared once, in the root `[workspace.lints]`, and a package only obeys
//! it when its own manifest opts in: `[lints]` is not inherited by workspace members. A member that
//! forgets `[lints] workspace = true` builds and passes clippy with the default lints, silently.
//!
//! The checks are functions of the manifests, so each one is also run on manifests built to break
//! it: the negative controls.

use repo_meta::workspace::{lint_opt_in, member_opt_in_problems, root_lint_problems};
use repo_meta::{Problem, report, strings};
use toml::Table;

fn parse(text: &str, origin: &str) -> Table {
    text.parse::<Table>()
        .unwrap_or_else(|error| panic!("parse {origin}: {error}"))
}

/// The root manifest declares `[workspace.lints.rust]` and `[workspace.lints.clippy]`, lists its
/// members explicitly (no glob), and every member declares `[lints] workspace = true`
/// (`repo_meta::workspace::lint_opt_in`).
#[test]
fn every_workspace_member_opts_into_the_workspace_lints() {
    let problems = lint_opt_in(&meta::repo(), None);
    assert!(problems.is_empty(), "{}", report(&problems));
}

/// Negative control: a root without the lint tables is refused.
#[test]
fn a_root_without_workspace_lints_is_refused() {
    let missing = parse("[workspace]\nmembers = [\".\"]\n", "fixture");
    assert_eq!(
        strings(&root_lint_problems(&missing)),
        ["the root Cargo.toml has no [workspace.lints] table"]
    );
    let clippy_only = parse("[workspace.lints.clippy]\npedantic = \"warn\"\n", "fixture");
    assert_eq!(
        strings(&root_lint_problems(&clippy_only)),
        ["[workspace.lints.rust] is missing"]
    );
}

/// Negative control: a member that does not opt in, one that opts out, and a glob are each
/// refused, while the member that opts in passes.
#[test]
fn a_member_without_the_opt_in_is_refused() {
    let root = parse(
        "[workspace]\nmembers = [\".\", \"silent\", \"opted-out\", \"crates/*\"]\n",
        "fixture",
    );
    let problems = member_opt_in_problems(
        &root,
        |member| -> Result<Table, Problem> {
            Ok(match member {
                "." => parse("[lints]\nworkspace = true\n", member),
                "opted-out" => parse("[lints]\nworkspace = false\n", member),
                _ => parse("[package]\nname = \"silent\"\n", member),
            })
        },
        None,
    );
    assert_eq!(
        strings(&problems),
        [
            "silent/Cargo.toml does not declare [lints] workspace = true",
            "opted-out/Cargo.toml does not declare [lints] workspace = true",
            "member \"crates/*\" is a glob; list members explicitly so this check sees each one",
        ]
    );
}

/// Negative control: a member whose manifest cannot be read is a problem, not a pass.
#[test]
fn an_unreadable_member_is_refused() {
    let root = parse("[workspace]\nmembers = [\"gone\"]\n", "fixture");
    let problems = member_opt_in_problems(
        &root,
        |member| {
            Err(Problem::at(
                format!("{member}/Cargo.toml"),
                "cannot be read",
            ))
        },
        None,
    );
    assert_eq!(strings(&problems), ["gone/Cargo.toml: cannot be read"]);
}
