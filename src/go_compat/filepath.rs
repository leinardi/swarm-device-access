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

//! Go's `path/filepath.Match` and `path/filepath.Clean` on Unix, ported line for line from the
//! Go 1.26 sources, because the device policy is a list of these globs and a glob that matches
//! differently grants or denies a different device.
//!
//! `Match` works on bytes, as Go does: `*` never crosses `/`, `?` and a class read one character
//! (an invalid UTF-8 sequence, which a `*` skip can leave in the middle of a character, reads as
//! U+FFFD and one byte), a class is negated by `^` only (`!` is an ordinary character), `\`
//! escapes the next character, and a malformed pattern is an error even where the name has
//! already failed to match, because the rest of the pattern is still checked.

use std::fmt;

/// Go's `filepath.ErrBadPattern`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadPattern;

impl fmt::Display for BadPattern {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("syntax error in pattern")
    }
}

impl std::error::Error for BadPattern {}

/// The path separator.
const SEPARATOR: u8 = b'/';

/// `filepath.Match`: whether `name` matches the shell pattern `pattern` whole.
///
/// # Errors
///
/// [`BadPattern`] when the pattern is malformed.
pub fn matches(pattern: &str, name: &str) -> Result<bool, BadPattern> {
    let mut pattern = pattern.as_bytes();
    let mut name = name.as_bytes();
    'pattern: while !pattern.is_empty() {
        let (star, chunk, rest) = scan_chunk(pattern);
        pattern = rest;
        if star && chunk.is_empty() {
            // A trailing * matches the rest of the name unless it holds a /.
            return Ok(!name.contains(&SEPARATOR));
        }
        // A match at the current position; the last chunk must exhaust the name.
        let (remaining, matched) = match_chunk(chunk, name)?;
        if matched && (remaining.is_empty() || !pattern.is_empty()) {
            name = remaining;
            continue;
        }
        if star {
            // A match after skipping index + 1 bytes; a * cannot skip a /.
            let mut index = 0;
            while index < name.len() && name[index] != SEPARATOR {
                let (remaining, matched) = match_chunk(chunk, &name[index + 1..])?;
                if matched && (!pattern.is_empty() || remaining.is_empty()) {
                    name = remaining;
                    continue 'pattern;
                }
                index += 1;
            }
        }
        // Before reporting no match, check that the rest of the pattern is well-formed.
        while !pattern.is_empty() {
            let (_, chunk, rest) = scan_chunk(pattern);
            pattern = rest;
            match_chunk(chunk, b"")?;
        }
        return Ok(false);
    }
    Ok(name.is_empty())
}

/// `scanChunk`: the leading stars, then the chunk up to the next star outside a class.
fn scan_chunk(mut pattern: &[u8]) -> (bool, &[u8], &[u8]) {
    let mut star = false;
    while let [b'*', rest @ ..] = pattern {
        pattern = rest;
        star = true;
    }
    let mut in_range = false;
    let mut index = 0;
    while index < pattern.len() {
        match pattern[index] {
            // A trailing \ is reported by match_chunk.
            b'\\' if index + 1 < pattern.len() => index += 1,
            b'[' => in_range = true,
            b']' => in_range = false,
            b'*' if !in_range => break,
            _ => {}
        }
        index += 1;
    }
    (star, &pattern[..index], &pattern[index..])
}

/// `matchChunk`: whether `chunk`, which holds no star, matches a prefix of `name`, and what is
/// left of `name`. Once the match has failed, the chunk is still read to the end for its syntax.
fn match_chunk<'name>(
    mut chunk: &[u8],
    mut name: &'name [u8],
) -> Result<(&'name [u8], bool), BadPattern> {
    let mut failed = false;
    while let Some(&first) = chunk.first() {
        if !failed && name.is_empty() {
            failed = true;
        }
        match first {
            b'[' => {
                let mut character = 0;
                if !failed {
                    let (decoded, width) = decode_rune(name);
                    character = decoded;
                    name = &name[width..];
                }
                chunk = &chunk[1..];
                let negated = chunk.first() == Some(&b'^');
                if negated {
                    chunk = &chunk[1..];
                }
                let mut matched = false;
                let mut ranges = 0;
                loop {
                    if chunk.first() == Some(&b']') && ranges > 0 {
                        chunk = &chunk[1..];
                        break;
                    }
                    let (low, rest) = get_escaped(chunk)?;
                    chunk = rest;
                    let mut high = low;
                    if chunk.first() == Some(&b'-') {
                        let (escaped, rest) = get_escaped(&chunk[1..])?;
                        high = escaped;
                        chunk = rest;
                    }
                    if low <= character && character <= high {
                        matched = true;
                    }
                    ranges += 1;
                }
                if matched == negated {
                    failed = true;
                }
            }
            b'?' => {
                if !failed {
                    if name.first() == Some(&SEPARATOR) {
                        failed = true;
                    }
                    let (_, width) = decode_rune(name);
                    name = &name[width..];
                }
                chunk = &chunk[1..];
            }
            _ => {
                if first == b'\\' {
                    chunk = &chunk[1..];
                    if chunk.is_empty() {
                        return Err(BadPattern);
                    }
                }
                if !failed {
                    if chunk.first() != name.first() {
                        failed = true;
                    }
                    name = &name[1..];
                }
                chunk = &chunk[1..];
            }
        }
    }
    if failed {
        return Ok((b"", false));
    }
    Ok((name, true))
}

/// `getEsc`: one possibly escaped character of a class, and the rest of the chunk, which must not
/// be empty.
fn get_escaped(mut chunk: &[u8]) -> Result<(u32, &[u8]), BadPattern> {
    if matches!(chunk.first(), None | Some(b'-' | b']')) {
        return Err(BadPattern);
    }
    if chunk.first() == Some(&b'\\') {
        chunk = &chunk[1..];
        if chunk.is_empty() {
            return Err(BadPattern);
        }
    }
    let (character, width) = decode_rune(chunk);
    let rest = &chunk[width..];
    if (character == REPLACEMENT && width == 1) || rest.is_empty() {
        return Err(BadPattern);
    }
    Ok((character, rest))
}

/// `utf8.RuneError`.
const REPLACEMENT: u32 = 0xFFFD;

/// `utf8.DecodeRuneInString`: the first character and its width; U+FFFD and one byte for an
/// invalid sequence, and U+FFFD and nothing for an empty input.
fn decode_rune(bytes: &[u8]) -> (u32, usize) {
    if bytes.is_empty() {
        return (REPLACEMENT, 0);
    }
    (1..=bytes.len().min(4))
        .find_map(|width| {
            std::str::from_utf8(&bytes[..width])
                .ok()
                .and_then(|text| text.chars().next())
                .map(|character| (u32::from(character), width))
        })
        .unwrap_or((REPLACEMENT, 1))
}

/// `filepath.Base`: the last element of `path`, trailing separators dropped first; `.` for an
/// empty path and `/` for one made of separators only.
#[must_use]
pub fn base(path: &str) -> &str {
    if path.is_empty() {
        return ".";
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/";
    }
    trimmed.rsplit_once('/').map_or(trimmed, |(_, last)| last)
}

/// `filepath.Join`: the non-empty elements joined with separators and cleaned; empty when every
/// element is.
#[must_use]
pub fn join(elements: &[&str]) -> String {
    let joined: Vec<&str> = elements
        .iter()
        .copied()
        .filter(|element| !element.is_empty())
        .collect();
    if joined.is_empty() {
        return String::new();
    }
    clean(&joined.join("/"))
}

/// `filepath.Clean`: the shortest path naming the same file by lexical processing: repeated
/// separators collapse, `.` elements go, a `..` element removes the element before it (and is
/// dropped at the root), a trailing separator goes, and an empty result is `.`.
#[must_use]
pub fn clean(path: &str) -> String {
    if path.is_empty() {
        return ".".to_owned();
    }
    let bytes = path.as_bytes();
    let rooted = bytes[0] == SEPARATOR;
    let length = bytes.len();
    let mut out = String::with_capacity(length);
    // out.len() is Go's out.w; dotdot is where a leading .. run ends and backtracking stops.
    let mut read = 0;
    let mut dotdot = 0;
    if rooted {
        out.push('/');
        read = 1;
        dotdot = 1;
    }
    while read < length {
        let at = |offset: usize| bytes.get(read + offset).copied();
        let ends_element = |offset: usize| matches!(at(offset), None | Some(SEPARATOR));
        if bytes[read] == SEPARATOR || (bytes[read] == b'.' && ends_element(1)) {
            // An empty or a `.` element.
            read += 1;
        } else if bytes[read] == b'.' && at(1) == Some(b'.') && ends_element(2) {
            read += 2;
            if out.len() > dotdot {
                let mut write = out.len() - 1;
                while write > dotdot && out.as_bytes()[write] != SEPARATOR {
                    write -= 1;
                }
                out.truncate(write);
            } else if !rooted {
                if !out.is_empty() {
                    out.push('/');
                }
                out.push_str("..");
                dotdot = out.len();
            }
        } else {
            if (rooted && out.len() != 1) || (!rooted && !out.is_empty()) {
                out.push('/');
            }
            let end = bytes[read..]
                .iter()
                .position(|&byte| byte == SEPARATOR)
                .map_or(length, |offset| read + offset);
            out.push_str(&path[read..end]);
            read = end;
        }
    }
    if out.is_empty() {
        out.push('.');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What Go 1.26's `filepath.Base` returns (a scratch Go program).
    #[test]
    fn base_is_go_s() {
        let cases = [
            ("", "."),
            ("/", "/"),
            ("//", "/"),
            ("devices", "devices"),
            ("/sys/fs/cgroup/devices", "devices"),
            ("/sys/fs/cgroup/devices/", "devices"),
            ("/a//", "a"),
            ("a/b", "b"),
            ("./", "."),
            (".", "."),
        ];
        for (path, want) in cases {
            assert_eq!(base(path), want, "{path:?}");
        }
    }

    #[test]
    fn star_does_not_cross_a_separator() {
        assert_eq!(matches("/dev/*", "/dev/sda"), Ok(true));
        assert_eq!(matches("/dev/*", "/dev/dri/card0"), Ok(false));
        assert_eq!(matches("/dev/*/*", "/dev/dri/card0"), Ok(true));
        assert_eq!(matches("a?b", "a/b"), Ok(false));
    }

    #[test]
    fn classes_negate_with_a_caret_only() {
        assert_eq!(matches("/dev/sd[^a-c]", "/dev/sdd"), Ok(true));
        assert_eq!(matches("/dev/sd[^a-c]", "/dev/sda"), Ok(false));
        assert_eq!(matches("[!a]", "!"), Ok(true));
        assert_eq!(matches("[!a]", "b"), Ok(false));
    }

    #[test]
    fn malformed_patterns_are_errors_even_without_a_match() {
        assert_eq!(matches("/dev/[a", "/dev/b"), Err(BadPattern));
        assert_eq!(matches("a\\", "a"), Err(BadPattern));
        assert_eq!(matches("x*[", "a"), Err(BadPattern));
        assert_eq!(BadPattern.to_string(), "syntax error in pattern");
    }

    #[test]
    fn a_skip_into_a_character_reads_it_as_invalid() {
        // The * skips one byte into the two-byte é; the ? reads the stray byte alone.
        assert_eq!(matches("*?", "é"), Ok(true));
        assert_eq!(decode_rune(&[0xC3]), (REPLACEMENT, 1));
        assert_eq!(decode_rune(b""), (REPLACEMENT, 0));
    }

    #[test]
    fn clean_is_lexical() {
        for (input, want) in [
            ("", "."),
            ("/", "/"),
            ("//", "/"),
            ("/dev/../etc", "/etc"),
            ("/../dev", "/dev"),
            ("a/../../b", "../b"),
            ("../../a", "../../a"),
            ("a/b/../..", "."),
            ("/dev/sda/", "/dev/sda"),
        ] {
            assert_eq!(clean(input), want, "{input:?}");
        }
    }

    /// Go's `filepath.Join`, checked against the Go function.
    #[test]
    fn join_skips_empty_elements_and_cleans() {
        let cases: [(&[&str], &str); 6] = [
            (&[], ""),
            (&["", ""], ""),
            (&["/dev", "dri"], "/dev/dri"),
            (&["/dev", "", "dri/../sda"], "/dev/sda"),
            (&["dri", "card0"], "dri/card0"),
            (
                &["/sys", "dev", "char", "1:3", "uevent"],
                "/sys/dev/char/1:3/uevent",
            ),
        ];
        for (elements, want) in cases {
            assert_eq!(join(elements), want, "{elements:?}");
        }
    }
}
