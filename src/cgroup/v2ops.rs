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

//! The kernel side of the cgroup v2 controller. So far only the kernel version check, which the
//! controller uses to decide whether `BPF_PROG_QUERY` reports attached programs completely.

use crate::go_compat::quote::is_space;

/// Whether `release` (a `uname -r` string) is at least `want_major.want_minor`, parsed as Go's
/// `fmt.Sscanf(release, "%d.%d", ...)` parses its leading numbers: spaces before each number are
/// skipped but a newline is not, a sign is allowed, and anything after the second number is
/// ignored. A release that does not parse is not at least anything.
#[must_use]
pub fn kernel_at_least(release: &str, want_major: i64, want_minor: i64) -> bool {
    let Some((major, rest)) = scan_decimal(release) else {
        return false;
    };
    let Some((minor, _)) = rest.strip_prefix('.').and_then(scan_decimal) else {
        return false;
    };
    major > want_major || (major == want_major && minor >= want_minor)
}

/// Go's `%d` verb: leading spaces, an optional sign, then at least one ASCII digit, as a 64-bit
/// integer.
fn scan_decimal(input: &str) -> Option<(i64, &str)> {
    let start = input.trim_start_matches(|character| character != '\n' && is_space(character));
    let unsigned = start.strip_prefix(['+', '-']).unwrap_or(start);
    let digits = unsigned
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(unsigned.len());
    if digits == 0 {
        return None;
    }
    let (token, rest) = start.split_at(start.len() - unsigned.len() + digits);
    Some((token.parse().ok()?, rest))
}

#[cfg(test)]
mod tests;
