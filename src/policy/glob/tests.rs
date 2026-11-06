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

//! The glob table of the 1.x matcher (Go's `filepath.Match`, 50 cases of the 1.x golden) read
//! against this matcher: a case is Go's answer unless it is listed as a divergence, and every
//! spelling the two read differently is refused or listed.

use super::{compile, matches};

/// `(pattern, name, Go matched, Go refused the pattern)`.
const GO: &[(&str, &str, bool, bool)] = &[
    ("*", "", true, false),
    ("*", "abc", true, false),
    ("*", "a/b", false, false),
    ("/dev/*", "/dev/sda", true, false),
    ("/dev/*", "/dev/dri/card0", false, false),
    ("/dev/*/*", "/dev/dri/card0", true, false),
    ("/dev/sd?", "/dev/sda", true, false),
    ("/dev/sd?", "/dev/sda1", false, false),
    ("/dev/sd?", "/dev/sd/", false, false),
    ("/dev/sd[a-c]", "/dev/sdb", true, false),
    ("/dev/sd[a-c]", "/dev/sdd", false, false),
    ("/dev/sd[!a-c]", "/dev/sdd", false, false),
    ("/dev/sd[^a-c]", "/dev/sdd", true, false),
    ("/dev/sd[^a-c]", "/dev/sda", false, false),
    ("/dev/sd[a-c", "/dev/sda", false, true),
    ("/dev/sd[", "/dev/sda", false, true),
    ("/dev/\\*", "/dev/*", true, false),
    ("/dev/\\*", "/dev/a", false, false),
    ("/dev/\\", "/dev/", false, true),
    ("a\\", "a", false, true),
    ("[]a]", "]", false, true),
    ("[]a]", "a", false, true),
    ("[-]", "-", false, true),
    ("[x-]", "x", false, true),
    ("[a-]", "b", false, true),
    ("[\\]]", "]", true, false),
    ("[\\-]", "-", true, false),
    ("[z-a]", "m", false, false),
    ("*x", "xxx", true, false),
    ("a*b*c", "abxbc", true, false),
    ("a*b*c", "abxbcx", false, false),
    ("*/b", "a/b", true, false),
    ("a*", "a/b", false, false),
    ("[!]", "a", false, false),
    ("[", "[", false, true),
    ("?", "日", true, false),
    ("[日本]", "本", true, false),
    ("[^日]", "本", true, false),
    ("a?b", "a/b", false, false),
    ("", "", true, false),
    ("", "a", false, false),
    ("*[", "a", false, true),
    ("a[", "a", false, true),
    ("/dev/dri/*", "/dev/dri/", true, false),
    ("/dev/[a", "/dev/b", false, true),
    ("[a-b-c]", "a", false, true),
    ("\\[", "[", true, false),
    ("[\\", "a", false, true),
    ("*?", "", false, false),
    ("**", "ab", true, false),
];

/// The cases where this matcher answers differently from Go's, with its answer.
const DIVERGENT: &[(&str, &str, bool)] = &[];

#[test]
fn the_go_table_reads_the_same_but_for_the_listed_divergences() {
    for &(pattern, name, go_matched, go_refused) in GO {
        let refused = compile(pattern).is_err();
        let got = matches(pattern, name);
        if let Some(&(_, _, want)) = DIVERGENT
            .iter()
            .find(|(divergent, at, _)| *divergent == pattern && *at == name)
        {
            assert_eq!(got, want, "{pattern:?} vs {name:?}");
            continue;
        }
        // A pattern Go refuses is refused here, or at least matches nothing; one Go accepts is
        // accepted unless it uses a refused spelling.
        if go_refused {
            assert!(refused, "{pattern:?} is refused by Go");
        } else if !refused {
            assert_eq!(got, go_matched, "{pattern:?} vs {name:?}");
        }
    }
}
