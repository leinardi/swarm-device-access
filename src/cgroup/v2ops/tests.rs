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

use super::kernel_at_least;

/// Go: `TestKernelAtLeast`, with more releases, each verdict printed by the Go function.
#[test]
fn kernel_at_least_parses_like_go() {
    let cases = [
        ("4.16.0", true),
        ("4.15.18", false),
        ("5.4.0-100-generic", true),
        ("3.99", false),
        ("junk", false),
        (" 5.0", true),
        ("\t4.16", true),
        ("\n4.16", false),
        ("\r\n4.16", false),
        ("\r4.16", true),
        ("+4.16", true),
        ("-5.0", false),
        ("4. 16", true),
        ("4 .16", false),
        ("4.", false),
        ("4", false),
        ("", false),
        ("99999999999999999999.0", false),
        ("5.-1", true),
        ("4.+16", true),
        ("\u{664}.\u{661}\u{666}", false),
        ("4.16\n", true),
        ("\u{a0}4.16", true),
        ("4.0x10", false),
        ("04.016", true),
        ("4.15", false),
        ("4.17", true),
        ("5.0", true),
        ("4.16x", true),
        ("4.\n16", false),
        ("4.\u{3000}16", true),
        ("4_0.1", false),
        ("9223372036854775807.0", true),
        ("-9223372036854775808.0", false),
        ("+-4.16", false),
    ];
    for (release, want) in cases {
        assert_eq!(kernel_at_least(release, 4, 16), want, "{release:?}");
    }
}
