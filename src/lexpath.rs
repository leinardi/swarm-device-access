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

//! Lexical path handling, the three functions of Go's `path/filepath` the daemon's path checks
//! are written against (`Clean`, `Join` and `Base` on Unix): the standard library has no lexical
//! `Clean`, and the checks that a path is clean and has no `..` are security checks, so their
//! algorithm stays exactly Go's, ported line for line from rust-common's `go-compat`.

const SEPARATOR: u8 = b'/';

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
    use super::{base, clean, join};

    #[test]
    fn clean_is_go_s() {
        for (input, want) in [
            ("", "."),
            ("/", "/"),
            ("//a//b/", "/a/b"),
            ("/dev/./null", "/dev/null"),
            ("/dev/../etc", "/etc"),
            ("/../..", "/"),
            ("../a/..", ".."),
            ("a/b/../../..", ".."),
            ("/dev/sd[a-c]", "/dev/sd[a-c]"),
            ("/dev/", "/dev"),
        ] {
            assert_eq!(clean(input), want, "{input:?}");
        }
    }

    #[test]
    fn join_and_base_are_go_s() {
        assert_eq!(join(&["/proc", "", "1"]), "/proc/1");
        assert_eq!(join(&["", ""]), "");
        assert_eq!(join(&["a", "../b"]), "b");
        assert_eq!(base(""), ".");
        assert_eq!(base("//"), "/");
        assert_eq!(base("/sys/fs/cgroup/devices/"), "devices");
    }
}
