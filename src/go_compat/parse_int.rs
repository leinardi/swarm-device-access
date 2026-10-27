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

//! Go's `strconv.ParseInt` and `strconv.ParseUint` with a bit size of 64: the base (2 to 36, or 0
//! for Go's `0b`, `0o`, `0x` and leading-`0` prefixes and `_` separators), and the errors, a
//! syntax error and a range error that ends the scan at the digit that overflows (so
//! `99999999999999999999x` is out of range, not invalid). `devices.list` numbers are read in base
//! 10; the config loader asks base 0 and the YAML prefixes whether a scalar is an integer.

use std::fmt;

use super::parse_float::underscore_ok;
use super::quote;

/// Why a `strconv` number does not parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumErrorKind {
    /// `strconv.ErrSyntax`.
    Syntax,
    /// `strconv.ErrRange`.
    Range,
    /// A base other than 0 or 2 to 36.
    Base(u32),
}

/// Go's `*strconv.NumError`: `strconv.<Func>: parsing "<text>": <reason>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumError {
    function: &'static str,
    text: String,
    kind: NumErrorKind,
}

impl NumError {
    /// Why the number does not parse.
    #[must_use]
    pub fn kind(&self) -> NumErrorKind {
        self.kind
    }
}

impl fmt::Display for NumError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "strconv.{}: parsing {}: ",
            self.function,
            quote::quote(&self.text)
        )?;
        match self.kind {
            NumErrorKind::Syntax => formatter.write_str("invalid syntax"),
            NumErrorKind::Range => formatter.write_str("value out of range"),
            NumErrorKind::Base(base) => write!(formatter, "invalid base {base}"),
        }
    }
}

impl std::error::Error for NumError {}

/// `strconv.ParseUint(text, base, 64)`.
///
/// # Errors
///
/// [`NumError`] for an empty number, a byte that is not a digit of the base, a misplaced `_`, a
/// value beyond `u64`, or an invalid base.
pub fn parse_uint(text: &str, base: u32) -> Result<u64, NumError> {
    parse_unsigned(text, base).map_err(|kind| NumError {
        function: "ParseUint",
        text: text.to_owned(),
        kind,
    })
}

/// `strconv.ParseInt(text, base, 64)`: a sign, then [`parse_uint`]'s digits, within `i64`.
///
/// # Errors
///
/// [`NumError`] as [`parse_uint`] fails, or for a value beyond `i64`.
pub fn parse_int(text: &str, base: u32) -> Result<i64, NumError> {
    let error = |kind| NumError {
        function: "ParseInt",
        text: text.to_owned(),
        kind,
    };
    if text.is_empty() {
        return Err(error(NumErrorKind::Syntax));
    }
    let (negative, unsigned) = match text.as_bytes()[0] {
        b'+' => (false, &text[1..]),
        b'-' => (true, &text[1..]),
        _ => (false, text),
    };
    let magnitude = match parse_unsigned(unsigned, base) {
        Ok(magnitude) => magnitude,
        // ParseInt goes on with the saturated value and reports its own range error.
        Err(NumErrorKind::Range) => u64::MAX,
        Err(kind @ (NumErrorKind::Syntax | NumErrorKind::Base(_))) => return Err(error(kind)),
    };
    if negative {
        0_i64
            .checked_sub_unsigned(magnitude)
            .ok_or_else(|| error(NumErrorKind::Range))
    } else {
        i64::try_from(magnitude).map_err(|_| error(NumErrorKind::Range))
    }
}

/// `ParseUint`'s loop, returning the error kind.
fn parse_unsigned(text: &str, base: u32) -> Result<u64, NumErrorKind> {
    if text.is_empty() {
        return Err(NumErrorKind::Syntax);
    }
    let base_zero = base == 0;
    let (base, digits) = match base {
        0 => base_prefix(text),
        2..=36 => (base, text),
        _ => return Err(NumErrorKind::Base(base)),
    };
    let mut underscores = false;
    let mut value: u64 = 0;
    for byte in digits.bytes() {
        if byte == b'_' && base_zero {
            underscores = true;
            continue;
        }
        let Some(digit) = char::from(byte).to_digit(36) else {
            return Err(NumErrorKind::Syntax);
        };
        if digit >= base {
            return Err(NumErrorKind::Syntax);
        }
        value = value
            .checked_mul(u64::from(base))
            .and_then(|scaled| scaled.checked_add(u64::from(digit)))
            .ok_or(NumErrorKind::Range)?;
    }
    if underscores && !underscore_ok(text.as_bytes()) {
        return Err(NumErrorKind::Syntax);
    }
    Ok(value)
}

/// The base and digits of a base-0 number: `0b`, `0o` and `0x` (either case) need a digit after
/// them, and any other leading `0` is octal.
fn base_prefix(text: &str) -> (u32, &str) {
    let bytes = text.as_bytes();
    if bytes.first() != Some(&b'0') {
        return (10, text);
    }
    if bytes.len() >= 3 {
        match bytes[1].to_ascii_lowercase() {
            b'b' => return (2, &text[2..]),
            b'o' => return (8, &text[2..]),
            b'x' => return (16, &text[2..]),
            _ => {}
        }
    }
    (8, &text[1..])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What Go 1.26's `strconv.ParseInt(text, 10, 64)` returns (a scratch Go program).
    #[test]
    fn parse_int_base_10_matches_go() {
        let cases: [(&str, Result<i64, &str>); 19] = [
            ("", Err("invalid syntax")),
            ("0", Ok(0)),
            ("-0", Ok(0)),
            ("+7", Ok(7)),
            ("-", Err("invalid syntax")),
            ("+", Err("invalid syntax")),
            ("12a", Err("invalid syntax")),
            ("a12", Err("invalid syntax")),
            ("9223372036854775807", Ok(i64::MAX)),
            ("9223372036854775808", Err("value out of range")),
            ("-9223372036854775808", Ok(i64::MIN)),
            ("-9223372036854775809", Err("value out of range")),
            ("99999999999999999999x", Err("value out of range")),
            ("1_000", Err("invalid syntax")),
            (" 1", Err("invalid syntax")),
            ("0x10", Err("invalid syntax")),
            ("007", Ok(7)),
            ("18446744073709551616", Err("value out of range")),
            ("1844674407370955161x", Err("invalid syntax")),
        ];
        for (text, want) in cases {
            match (parse_int(text, 10), want) {
                (Ok(got), Ok(want)) => assert_eq!(got, want, "{text}"),
                (Err(got), Err(reason)) => assert_eq!(
                    got.to_string(),
                    format!("strconv.ParseInt: parsing {}: {reason}", quote::quote(text))
                ),
                (got, want) => panic!("{text}: got {got:?}, want {want:?}"),
            }
        }
    }

    /// What Go 1.26's `ParseInt` and `ParseUint` return in base 0 and for a bad base (a scratch
    /// Go program).
    #[test]
    fn base_zero_and_bad_bases_match_go() {
        let int_cases: [(&str, Result<i64, &str>); 12] = [
            ("0x1F", Ok(31)),
            ("-0b101", Ok(-5)),
            ("0o17", Ok(15)),
            ("017", Ok(15)),
            ("0x", Err("invalid syntax")),
            ("08", Err("invalid syntax")),
            ("1_000", Ok(1000)),
            ("0x_1F", Ok(31)),
            ("1__0", Err("invalid syntax")),
            ("_1", Err("invalid syntax")),
            ("1_", Err("invalid syntax")),
            ("-0x8000000000000000", Ok(i64::MIN)),
        ];
        for (text, want) in int_cases {
            assert_eq!(
                parse_int(text, 0).map_err(|error| error.to_string()),
                want.map_err(|reason| format!(
                    "strconv.ParseInt: parsing {}: {reason}",
                    quote::quote(text)
                )),
                "{text}"
            );
        }
        assert_eq!(parse_uint("18446744073709551615", 0), Ok(u64::MAX));
        assert_eq!(
            parse_uint("-1", 0).unwrap_err().to_string(),
            r#"strconv.ParseUint: parsing "-1": invalid syntax"#
        );
        assert_eq!(
            parse_uint("18446744073709551616", 10)
                .unwrap_err()
                .to_string(),
            r#"strconv.ParseUint: parsing "18446744073709551616": value out of range"#
        );
        assert_eq!(
            parse_int("1", 1).unwrap_err().to_string(),
            r#"strconv.ParseInt: parsing "1": invalid base 1"#
        );
        assert_eq!(
            parse_int("1", 37).unwrap_err().kind(),
            NumErrorKind::Base(37)
        );
    }
}
