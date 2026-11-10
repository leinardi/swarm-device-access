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

//! The rust-common pins in the root `Cargo.toml`: every dependency on rust-common names a release
//! tag `vX.Y.Z` with `version = "X.Y"`, and all of them name the same tag, so a bump moves the
//! whole set together. The check is `repo_meta::git_pins`'s; the negative control runs it on an
//! altered copy of the real manifest.

use repo_meta::git_pins::{RUST_COMMON_URL, git_pins, root_git_pins};
use repo_meta::report;

const MANIFEST: &str = "Cargo.toml";

#[test]
fn rust_common_dependencies_are_pinned_to_one_release_tag() {
    let problems = root_git_pins(&meta::repo(), RUST_COMMON_URL);
    assert!(problems.is_empty(), "{}", report(&problems));
}

/// A tag that moved on one line only and a `version` that does not match its tag are both
/// refused, each as a problem of its own.
#[test]
fn a_mismatched_tag_or_version_is_refused() {
    let manifest = meta::repo().read(MANIFEST);
    let mut lines = manifest
        .lines()
        .filter(|line| line.contains(RUST_COMMON_URL) && line.contains("tag = "));
    let (first, second) = (
        lines.next().expect("a rust-common dependency"),
        lines.next().expect("a second rust-common dependency"),
    );
    let tag = first
        .split("tag = \"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("the first line's tag");
    let version = second
        .split("version = \"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("the second line's version");
    let altered = manifest
        .replacen(first, &first.replacen(tag, "v0.0.1", 1), 1)
        .replacen(
            second,
            &second.replacen(&format!("version = \"{version}\""), "version = \"9.9\"", 1),
            1,
        );
    assert_ne!(altered, manifest, "the edit changed nothing");
    let problems = git_pins(&[(MANIFEST, &altered)], RUST_COMMON_URL);
    assert!(
        problems.len() >= 2,
        "an altered pin went unreported: {}",
        report(&problems)
    );
}
