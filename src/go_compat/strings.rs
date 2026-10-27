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

// Adapted from swarm-scheduler-exporter/src/go_compat/strings.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! Ports of the `strings` functions whose Rust counterparts differ: `strings.EqualFold`
//! (Unicode simple case folding, where `eq_ignore_ascii_case` is ASCII only and `to_lowercase`
//! uses full mappings), `strings.ToLower` (per code point, so no final-sigma rule and no
//! two-character results) and `strings.TrimSpace`, all on Go's Unicode 15 tables.

use super::quote::is_space;
use super::unicode_case::{ALTERNATE, SIMPLE_FOLD, TO_LOWER};

/// Maps `character` through one of the generated tables.
fn lookup(table: &[(u32, u32, u32, i32)], character: char) -> char {
    let code = u32::from(character);
    let index = table.partition_point(|&(start, ..)| start <= code);
    let Some(&(start, count, step, delta)) = index.checked_sub(1).and_then(|last| table.get(last))
    else {
        return character;
    };
    let offset = code - start;
    if offset % step != 0 || offset / step >= count {
        return character;
    }
    let mapped = if delta == ALTERNATE {
        if offset % 2 == 0 { code + 1 } else { code - 1 }
    } else {
        code.wrapping_add_signed(delta)
    };
    char::from_u32(mapped).unwrap_or(character)
}

/// Go's `unicode.SimpleFold`: the next code point of `character`'s case-fold orbit, itself when
/// it has none.
#[must_use]
pub fn simple_fold(character: char) -> char {
    lookup(SIMPLE_FOLD, character)
}

/// Go's `unicode.ToLower`.
#[must_use]
pub fn to_lower_char(character: char) -> char {
    lookup(TO_LOWER, character)
}

/// Go's `strings.ToLower`.
#[must_use]
pub fn to_lower(text: &str) -> String {
    text.chars().map(to_lower_char).collect()
}

/// Go's `strings.EqualFold`: equal under Unicode simple case folding.
#[must_use]
pub fn equal_fold(left: &str, right: &str) -> bool {
    let mut right_chars = right.chars();
    for left_char in left.chars() {
        let Some(right_char) = right_chars.next() else {
            return false;
        };
        if left_char == right_char {
            continue;
        }
        let (low, high) = if left_char < right_char {
            (left_char, right_char)
        } else {
            (right_char, left_char)
        };
        if high.is_ascii() {
            if low.is_ascii_uppercase() && u32::from(high) == u32::from(low) + 32 {
                continue;
            }
            return false;
        }
        // The orbit of `low`, walked until it reaches or passes `high`.
        let mut folded = simple_fold(low);
        while folded != low && folded < high {
            folded = simple_fold(folded);
        }
        if folded != high {
            return false;
        }
    }
    right_chars.next().is_none()
}

/// Go's `strings.TrimSpace`: leading and trailing Unicode white space removed.
#[must_use]
pub fn trim_space(text: &str) -> &str {
    text.trim_matches(is_space)
}

/// Go's `strings.Fields`: the runs of text between Unicode white space.
#[must_use]
pub fn fields(text: &str) -> Vec<&str> {
    text.split(is_space)
        .filter(|field| !field.is_empty())
        .collect()
}

/// Go's `strings.TrimSpace` over bytes that need not be UTF-8, as a Go string holds: leading and
/// trailing Unicode white space removed, stopping at the first byte that does not decode, which
/// Go reads as `utf8.RuneError` and keeps.
#[must_use]
pub fn trim_space_bytes(mut bytes: &[u8]) -> &[u8] {
    while let Some((character, width)) = first_char(bytes) {
        if !is_space(character) {
            break;
        }
        bytes = &bytes[width..];
    }
    while let Some((character, width)) = last_char(bytes) {
        if !is_space(character) {
            break;
        }
        bytes = &bytes[..bytes.len() - width];
    }
    bytes
}

/// The character `bytes` starts with, and its width; none when they do not start with one.
fn first_char(bytes: &[u8]) -> Option<(char, usize)> {
    (1..=bytes.len().min(4)).find_map(|width| {
        std::str::from_utf8(&bytes[..width])
            .ok()
            .and_then(|text| text.chars().next())
            .map(|character| (character, width))
    })
}

/// The character `bytes` ends with, and its width; none when they do not end with one.
fn last_char(bytes: &[u8]) -> Option<(char, usize)> {
    (1..=bytes.len().min(4)).find_map(|width| {
        std::str::from_utf8(&bytes[bytes.len() - width..])
            .ok()
            .and_then(|text| text.chars().next())
            .map(|character| (character, width))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Unicode 16 and 17 case pairs Rust knows and Go's Unicode 15 does not.
    /// Go's `TrimSpace` on bytes: Unicode spaces go, an undecodable byte stops the trim.
    #[test]
    fn trim_space_bytes_is_go_s() {
        assert_eq!(trim_space_bytes("\u{85} x \u{3000}".as_bytes()), b"x");
        assert_eq!(trim_space_bytes(b"\xff x \xff"), b"\xff x \xff");
        assert_eq!(trim_space_bytes(b" \t\r\n"), b"");
        assert_eq!(trim_space_bytes(b"  partial reason  "), b"partial reason");
    }

    /// What Go 1.26's `strings.Fields` returns (a scratch Go program).
    #[test]
    fn fields_split_at_go_s_white_space() {
        let empty: [&str; 0] = [];
        assert_eq!(fields(""), empty);
        assert_eq!(fields("  "), empty);
        assert_eq!(fields(" a  b\tc\n"), ["a", "b", "c"]);
        assert_eq!(fields("a\u{a0}b\u{85}c\u{2028}d"), ["a", "b", "c", "d"]);
        assert_eq!(fields("x"), ["x"]);
    }

    #[test]
    fn folding_uses_unicode_15() {
        assert!(!equal_fold("\u{1C89}", "\u{1C8A}"));
        // Rust's tables know the pair.
        assert_eq!("\u{1C89}".to_lowercase(), "\u{1C8A}");
        assert_eq!(to_lower("\u{1C89}"), "\u{1C89}");
    }

    #[test]
    fn folding_walks_whole_orbits() {
        assert!(equal_fold("k", "\u{212A}"));
        assert!(equal_fold("K", "\u{212A}"));
        assert!(equal_fold("S", "\u{17F}"));
        assert!(equal_fold("\u{3C2}", "\u{3A3}"));
        assert!(!equal_fold("ss", "\u{DF}"));
        assert!(!equal_fold("a", "ab"));
        assert!(!equal_fold("ab", "a"));
        assert_eq!(to_lower("ΑΣ"), "ασ");
        assert_eq!(to_lower("İ"), "i");
    }
}
