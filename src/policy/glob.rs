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

//! The device globs of the policy, on the `glob` crate: `*`, `?` and a class never match `/`, the
//! match is case-sensitive, and a leading dot is no special case.
//!
//! The 1.x matcher was Go's `filepath.Match`, and the two agree on `*`, `?`, plain classes and
//! ranges. They disagree on five spellings, and a glob is how an operator allows or denies a
//! device, so none of them may change meaning silently: [`compile`] reads the Go spelling of a
//! negated class, `[^x]`, as the crate's `[!x]` (the same set, so the same decision), and refuses
//! the rest, which means the pattern is rejected when the config or the label is loaded:
//!
//! - `\` (Go's escape, which the crate has none of): refused, write the character in a class,
//!   `[*]`;
//! - `**` (a recursive match in the crate, `*` twice in Go): refused;
//! - `[!x]` (a class of `!` and `x` in Go, a negated class in the crate): refused, write `[^x]`;
//! - a class with an empty body, a dash that is not between two characters (`[a-]`, `[-a]`,
//!   `[a-b-c]`) or no closing bracket: refused, as Go refuses them (the crate accepts some);
//! - a negated class matches `/` in Go and not here, so `/dev/sd[^a]` no longer matches
//!   `/dev/sd/`, a name no device has.

use std::fmt;

use glob::{MatchOptions, Pattern};

/// Why a glob is refused: Go's `filepath.ErrBadPattern`, with what is wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadPattern {
    reason: &'static str,
}

impl fmt::Display for BadPattern {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "syntax error in pattern: {}", self.reason)
    }
}

impl std::error::Error for BadPattern {}

const fn bad(reason: &'static str) -> BadPattern {
    BadPattern { reason }
}

const OPTIONS: MatchOptions = MatchOptions {
    case_sensitive: true,
    require_literal_separator: true,
    require_literal_leading_dot: false,
};

/// The pattern as the crate reads it, or why it is refused (see the module comment).
///
/// # Errors
///
/// [`BadPattern`] for a spelling the two matchers read differently, or a malformed one.
pub fn compile(pattern: &str) -> Result<Pattern, BadPattern> {
    let translated = translate(pattern)?;
    Pattern::new(&translated).map_err(|_| bad("malformed pattern"))
}

/// Whether `name` matches `pattern`; a refused pattern matches nothing.
#[must_use]
pub fn matches(pattern: &str, name: &str) -> bool {
    compile(pattern).is_ok_and(|compiled| compiled.matches_with(name, OPTIONS))
}

/// Checks the spellings and returns the pattern with Go's `[^` written as the crate's `[!`.
fn translate(pattern: &str) -> Result<String, BadPattern> {
    if pattern.contains('\\') {
        return Err(bad(
            "a backslash escape is not supported, use a class like [*]",
        ));
    }
    if pattern.contains("**") {
        return Err(bad("** is not supported"));
    }
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::with_capacity(pattern.len());
    let mut at = 0;
    while at < chars.len() {
        let character = chars[at];
        at += 1;
        if character != '[' {
            out.push(character);
            continue;
        }
        out.push('[');
        match chars.get(at) {
            Some('!') => return Err(bad("[!x] is not supported, negate a class with [^x]")),
            Some('^') => {
                out.push('!');
                at += 1;
            }
            _ => {}
        }
        at = class_body(&chars, at, &mut out)?;
    }
    Ok(out)
}

/// Copies a class's items up to and including its `]`, refusing an empty body, a dash that is not
/// between two characters and a missing `]`. Returns the index after the `]`.
fn class_body(chars: &[char], mut at: usize, out: &mut String) -> Result<usize, BadPattern> {
    let mut items = 0;
    loop {
        let Some(&low) = chars.get(at) else {
            return Err(bad("missing ]"));
        };
        at += 1;
        if low == ']' && items > 0 {
            out.push(']');
            return Ok(at);
        }
        if low == ']' || low == '-' {
            return Err(bad("empty class or a dash outside a range"));
        }
        out.push(low);
        if chars.get(at) == Some(&'-') {
            let Some(&high) = chars.get(at + 1) else {
                return Err(bad("missing ]"));
            };
            if high == ']' || high == '-' {
                return Err(bad("a dash outside a range"));
            }
            out.push('-');
            out.push(high);
            at += 2;
        }
        items += 1;
    }
}

#[cfg(test)]
mod tests;
