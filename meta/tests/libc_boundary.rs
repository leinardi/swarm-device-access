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

//! Only `src/cgroup/bpf_sys.rs`, the audited module of raw `bpf(2)` calls the cgroup v2 device
//! filter needs (`docs/rust-port.md`, eBPF), may call `libc`: a `libc::` call elsewhere would be a
//! way around the audited module even without `unsafe` (a safe `libc` function still reaches the
//! kernel unchecked). The check is also run on input built to break it.

use std::sync::LazyLock;

use regex::Regex;

/// This test's own path: it names libc in its fixtures and calls nothing.
const THIS_FILE: &str = "meta/tests/libc_boundary.rs";

/// The only Rust files that may call libc.
const ALLOWLIST: &[&str] = &["src/cgroup/bpf_sys.rs"];

/// A use of the `libc` crate in code: a path through it, an import or an `extern crate`.
static LIBC_USE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\blibc\s*::|\buse\s+libc\b|\bextern\s+crate\s+libc\b")
        .unwrap_or_else(|error| panic!("pattern: {error}"))
});

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

#[test]
fn only_the_allowlisted_module_calls_libc() {
    let repo = meta::repo();
    let files = repo.tracked_or_new(&["*.rs"]);
    let allowed = ALLOWLIST[0];
    assert!(
        LIBC_USE.is_match(&repo.read(allowed)),
        "{allowed} no longer calls libc: the scan would pass on nothing"
    );
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
