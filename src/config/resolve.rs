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

//! yaml.v3's tag resolution (`resolve.go`), ported line for line because it decides which plain
//! scalars are strings: a config value that resolves to anything else fails its key's shape. The
//! numeric checks port what `resolve` asks of `strconv.ParseInt`, `ParseUint` and `ParseFloat`,
//! and the timestamp check what it asks of `time.Parse` with its four layouts; only whether each
//! succeeds matters here, never the value.

/// `!!null`.
pub(super) const NULL_TAG: &str = "!!null";
/// `!!bool`.
pub(super) const BOOL_TAG: &str = "!!bool";
/// `!!str`.
pub(super) const STR_TAG: &str = "!!str";
/// `!!int`.
pub(super) const INT_TAG: &str = "!!int";
/// `!!float`.
pub(super) const FLOAT_TAG: &str = "!!float";
/// `!!timestamp`.
pub(super) const TIMESTAMP_TAG: &str = "!!timestamp";
/// `!!seq`.
pub(super) const SEQ_TAG: &str = "!!seq";
/// `!!map`.
pub(super) const MAP_TAG: &str = "!!map";
/// `!!binary`.
pub(super) const BINARY_TAG: &str = "!!binary";
/// `!!merge`.
pub(super) const MERGE_TAG: &str = "!!merge";

/// The prefix of the long form of the core tags.
const LONG_TAG_PREFIX: &str = "tag:yaml.org,2002:";

/// What a scalar resolved to; numbers and times carry no value, since nothing here reads one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Resolved {
    /// `nil`.
    Null,
    /// A bool.
    Bool(bool),
    /// The text itself: a string, or a scalar under a tag `resolve` does not handle.
    Text(String),
    /// An integer that fits an `int64`.
    Int,
    /// An integer that only fits a `uint64`.
    Uint,
    /// A float.
    Float,
    /// A timestamp.
    Timestamp,
}

/// `shortTag`: `tag:yaml.org,2002:x` as `!!x`; any other tag unchanged.
pub(super) fn short_tag(tag: &str) -> String {
    match tag.strip_prefix(LONG_TAG_PREFIX) {
        Some(suffix) => format!("!!{suffix}"),
        None => tag.to_owned(),
    }
}

/// `resolvableTag`.
fn resolvable_tag(tag: &str) -> bool {
    matches!(
        tag,
        "" | STR_TAG | BOOL_TAG | INT_TAG | FLOAT_TAG | NULL_TAG | TIMESTAMP_TAG
    )
}

/// `resolveMap`: the spellings resolved by lookup.
fn resolve_map(input: &str) -> Option<(&'static str, Resolved)> {
    Some(match input {
        "true" | "True" | "TRUE" => (BOOL_TAG, Resolved::Bool(true)),
        "false" | "False" | "FALSE" => (BOOL_TAG, Resolved::Bool(false)),
        "" | "~" | "null" | "Null" | "NULL" => (NULL_TAG, Resolved::Null),
        ".nan" | ".NaN" | ".NAN" | ".inf" | ".Inf" | ".INF" | "+.inf" | "+.Inf" | "+.INF"
        | "-.inf" | "-.Inf" | "-.INF" => (FLOAT_TAG, Resolved::Float),
        "<<" => (MERGE_TAG, Resolved::Text(input.to_owned())),
        _ => return None,
    })
}

/// `resolveTable`: the hint the first byte gives.
fn hint(input: &str) -> Option<u8> {
    let Some(&first) = input.as_bytes().first() else {
        return Some(b'N');
    };
    match first {
        b'+' | b'-' => Some(b'S'),
        b'0'..=b'9' => Some(b'D'),
        b'y' | b'Y' | b'n' | b'N' | b't' | b'T' | b'f' | b'F' | b'o' | b'O' | b'~' => Some(b'M'),
        b'.' => Some(b'.'),
        _ => None,
    }
}

/// `resolve(tag, in)`: the tag a scalar resolves to under `tag` (empty when the node has none)
/// and its value.
///
/// # Errors
///
/// yaml.v3's `failf` text when an explicit core tag does not fit the value:
/// ``cannot decode !!str `x` as a !!int``.
pub(super) fn resolve(tag: &str, input: &str) -> Result<(String, Resolved), String> {
    let tag = short_tag(tag);
    if !resolvable_tag(&tag) {
        return Ok((tag, Resolved::Text(input.to_owned())));
    }
    let (mut resolved_tag, resolved) = resolve_resolvable(&tag, input);
    // The deferred check of resolve: an explicit tag must be the one the value resolves to, an
    // integer reading as a float under !!float.
    if !(tag.is_empty() || tag == resolved_tag || tag == STR_TAG) {
        // Only a value Go holds as an int or an int64 converts, never a uint64.
        if tag == FLOAT_TAG && matches!(resolved, Resolved::Int) {
            resolved_tag = FLOAT_TAG;
        } else {
            return Err(format!(
                "cannot decode {} `{input}` as a {}",
                short_tag(resolved_tag),
                short_tag(&tag)
            ));
        }
    }
    let resolved = if resolved_tag == FLOAT_TAG && matches!(resolved, Resolved::Int) {
        Resolved::Float
    } else {
        resolved
    };
    Ok((resolved_tag.to_owned(), resolved))
}

/// The body of `resolve` for a resolvable tag.
fn resolve_resolvable(tag: &str, input: &str) -> (&'static str, Resolved) {
    let text = || (STR_TAG, Resolved::Text(input.to_owned()));
    let Some(hint) = hint(input) else {
        return text();
    };
    if tag == STR_TAG {
        return text();
    }
    if let Some(found) = resolve_map(input) {
        return found;
    }
    match hint {
        b'.' if go_float_syntax(input) => (FLOAT_TAG, Resolved::Float),
        b'D' | b'S' => resolve_number(tag, input).unwrap_or_else(text),
        // 'M' was looked up above; '.' that is not a float is a string.
        _ => text(),
    }
}

/// The digit and sign branch of `resolve`: a timestamp, an integer or a float, else `None`.
fn resolve_number(tag: &str, input: &str) -> Option<(&'static str, Resolved)> {
    if (tag.is_empty() || tag == TIMESTAMP_TAG) && is_timestamp(input) {
        return Some((TIMESTAMP_TAG, Resolved::Timestamp));
    }
    let plain = input.replace('_', "");
    let integer = |digits: &str, base: u32| {
        if parse_int(digits, base) {
            Some((INT_TAG, Resolved::Int))
        } else if parse_uint(digits, base) {
            Some((INT_TAG, Resolved::Uint))
        } else {
            None
        }
    };
    if let Some(found) = integer(&plain, 0) {
        return Some(found);
    }
    if yaml_style_float(&plain) && decimal_is_finite(&plain) {
        return Some((FLOAT_TAG, Resolved::Float));
    }
    for (prefix, negative_prefix, base) in [("0b", "-0b", 2), ("0o", "-0o", 8)] {
        if let Some(digits) = plain.strip_prefix(prefix) {
            if let Some(found) = integer(digits, base) {
                return Some(found);
            }
        } else if let Some(digits) = plain.strip_prefix(negative_prefix)
            && parse_int(&format!("-{digits}"), base)
        {
            return Some((INT_TAG, Resolved::Int));
        }
    }
    None
}

/// `yamlStyleFloat`: `^[-+]?(\.[0-9]+|[0-9]+(\.[0-9]*)?)([eE][-+]?[0-9]+)?$`.
fn yaml_style_float(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut index = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    let digits_from = |start: usize| {
        bytes[start..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count()
    };
    if bytes.get(index) == Some(&b'.') {
        let fraction = digits_from(index + 1);
        if fraction == 0 {
            return false;
        }
        index += 1 + fraction;
    } else {
        let integer = digits_from(index);
        if integer == 0 {
            return false;
        }
        index += integer;
        if bytes.get(index) == Some(&b'.') {
            index += 1 + digits_from(index + 1);
        }
    }
    if matches!(bytes.get(index), Some(b'e' | b'E')) {
        index += 1;
        if matches!(bytes.get(index), Some(b'+' | b'-')) {
            index += 1;
        }
        let exponent = digits_from(index);
        if exponent == 0 {
            return false;
        }
        index += exponent;
    }
    index == bytes.len()
}

/// Whether a decimal float without underscores is within `float64`'s range, where Go's
/// `ParseFloat` reports `ErrRange` (Rust reads the value as infinite).
fn decimal_is_finite(text: &str) -> bool {
    text.parse::<f64>().is_ok_and(f64::is_finite)
}

/// `strconv.ParseFloat(in, 64)` succeeding, for the `.` branch: a decimal float in Go syntax,
/// underscores between digits allowed, within range.
fn go_float_syntax(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut index = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    let mut digits = false;
    let mut seen_point = false;
    while let Some(&byte) = bytes.get(index) {
        match byte {
            b'0'..=b'9' => digits = true,
            b'_' => {}
            b'.' if !seen_point => seen_point = true,
            _ => break,
        }
        index += 1;
    }
    if !digits {
        return false;
    }
    if matches!(bytes.get(index), Some(b'e' | b'E')) {
        index += 1;
        if matches!(bytes.get(index), Some(b'+' | b'-')) {
            index += 1;
        }
        let start = index;
        while matches!(bytes.get(index), Some(b'0'..=b'9' | b'_')) {
            index += 1;
        }
        if !bytes[start..index].iter().any(u8::is_ascii_digit) {
            return false;
        }
    }
    index == bytes.len() && underscore_ok(text) && decimal_is_finite(&text.replace('_', ""))
}

/// `strconv.underscoreOK`: every underscore sits between digits, or between a base prefix and a
/// digit.
fn underscore_ok(text: &str) -> bool {
    let mut bytes = text.as_bytes();
    if let [b'+' | b'-', rest @ ..] = bytes {
        bytes = rest;
    }
    let mut saw = b'^';
    let mut index = 0;
    let mut hex = false;
    if bytes.len() >= 2
        && bytes[0] == b'0'
        && matches!(bytes[1].to_ascii_lowercase(), b'b' | b'o' | b'x')
    {
        index = 2;
        saw = b'0';
        hex = bytes[1].eq_ignore_ascii_case(&b'x');
    }
    for &byte in &bytes[index..] {
        if byte.is_ascii_digit() || (hex && byte.is_ascii_hexdigit()) {
            saw = b'0';
            continue;
        }
        if byte == b'_' {
            if saw != b'0' {
                return false;
            }
            saw = b'_';
            continue;
        }
        if saw == b'_' {
            return false;
        }
        saw = b'!';
    }
    saw != b'_'
}

/// `strconv.ParseUint(text, base, 64)` succeeding (`text` holds no underscore). Base 0 reads
/// Go's prefixes: `0b`, `0o`, `0x`, and a leading `0` for octal.
fn parse_uint(text: &str, base: u32) -> bool {
    if text.is_empty() {
        return false;
    }
    let (base, digits) = if base == 0 {
        base_prefix(text)
    } else {
        (base, text)
    };
    let mut value: u64 = 0;
    for byte in digits.bytes() {
        let Some(digit) = char::from(byte).to_digit(36) else {
            return false;
        };
        if digit >= base {
            return false;
        }
        let Some(next) = value
            .checked_mul(u64::from(base))
            .and_then(|scaled| scaled.checked_add(u64::from(digit)))
        else {
            return false;
        };
        value = next;
    }
    true
}

/// `strconv.ParseInt(text, base, 64)` succeeding: a sign, then `ParseUint`'s digits, within
/// `int64`.
fn parse_int(text: &str, base: u32) -> bool {
    if text.is_empty() {
        return false;
    }
    let (negative, unsigned) = match text.as_bytes()[0] {
        b'+' => (false, &text[1..]),
        b'-' => (true, &text[1..]),
        _ => (false, text),
    };
    if !parse_uint(unsigned, base) {
        return false;
    }
    let (base, digits) = if base == 0 {
        base_prefix(unsigned)
    } else {
        (base, unsigned)
    };
    let magnitude = u64::from_str_radix(digits, base).unwrap_or(0);
    let cutoff = 1_u64 << 63;
    if negative {
        magnitude <= cutoff
    } else {
        magnitude < cutoff
    }
}

/// The base and digits of a base-0 `ParseUint` input.
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

/// `parseTimestamp`: four digits and a dash, then one of yaml.v3's layouts as `time.Parse`
/// reads it: `2006-1-2T15:4:5.999999999Z07:00` (or with a lower-case `t`),
/// `2006-1-2 15:4:5.999999999` and `2006-1-2`.
fn is_timestamp(text: &str) -> bool {
    let bytes = text.as_bytes();
    let year_digits = bytes
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if year_digits != 4 || bytes.get(4) != Some(&b'-') {
        return false;
    }
    let Some((year, month, day, rest)) = parse_date(bytes) else {
        return false;
    };
    if day < 1 || day > days_in(month, year) {
        return false;
    }
    match rest {
        [] => true,
        [b'T' | b't', time @ ..] => parse_time(time).is_some_and(parse_zone),
        [b' ', time @ ..] => parse_time(time).is_some_and(<[u8]>::is_empty),
        _ => false,
    }
}

/// `2006-1-2`: the year, the month (1 to 12) and the day (unchecked), and what follows.
fn parse_date(bytes: &[u8]) -> Option<(u32, u32, u32, &[u8])> {
    let year = digits_value(&bytes[..4]);
    let rest = bytes[4..].strip_prefix(b"-")?;
    let (month, rest) = get_number(rest)?;
    if !(1..=12).contains(&month) {
        return None;
    }
    let rest = rest.strip_prefix(b"-")?;
    let (day, rest) = get_number(rest)?;
    Some((year, month, day, rest))
}

/// `15:4:5.999999999`: hours below 24, minutes and seconds below 60, an optional fraction after
/// `.` or `,`; what follows.
fn parse_time(bytes: &[u8]) -> Option<&[u8]> {
    let (hour, rest) = get_number(bytes)?;
    let rest = rest.strip_prefix(b":")?;
    let (minute, rest) = get_number(rest)?;
    let rest = rest.strip_prefix(b":")?;
    let (second, mut rest) = get_number(rest)?;
    if hour >= 24 || minute >= 60 || second >= 60 {
        return None;
    }
    if let [b'.' | b',', digit, ..] = rest
        && digit.is_ascii_digit()
    {
        let fraction = rest[1..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        rest = &rest[1 + fraction..];
    }
    Some(rest)
}

/// `Z07:00`: `Z`, or a sign, two-digit hours up to 24, `:` and two-digit minutes up to 60, and
/// nothing after.
fn parse_zone(bytes: &[u8]) -> bool {
    match bytes {
        [b'Z'] => true,
        [b'+' | b'-', hour @ .., b':', _, _] if hour.len() == 2 => {
            let minute = &bytes[4..6];
            hour.iter().chain(minute).all(u8::is_ascii_digit)
                && digits_value(hour) <= 24
                && digits_value(minute) <= 60
        }
        _ => false,
    }
}

/// `getnum(s, false)`: one or two digits.
fn get_number(bytes: &[u8]) -> Option<(u32, &[u8])> {
    let width = bytes
        .iter()
        .take(2)
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    (width > 0).then(|| (digits_value(&bytes[..width]), &bytes[width..]))
}

/// The value of ASCII digits.
fn digits_value(digits: &[u8]) -> u32 {
    digits
        .iter()
        .fold(0, |value, digit| value * 10 + u32::from(digit - b'0'))
}

/// `daysIn`.
fn days_in(month: u32, year: u32) -> u32 {
    match month {
        2 if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) => {
            29
        }
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// `base64.StdEncoding.DecodeString`, with the line breaks it skips: the decoded bytes, or
/// `None` for anything that is not padded standard base64.
pub(super) fn decode_base64(text: &str) -> Option<Vec<u8>> {
    let symbols: Vec<u8> = text
        .bytes()
        .filter(|byte| !matches!(byte, b'\r' | b'\n'))
        .collect();
    if !symbols.len().is_multiple_of(4) {
        return None;
    }
    let mut decoded = Vec::with_capacity(symbols.len() / 4 * 3);
    let quads = symbols.chunks(4).count();
    for (index, quad) in symbols.chunks(4).enumerate() {
        let padding = quad.iter().rev().take_while(|&&byte| byte == b'=').count();
        if padding > 2 || (padding > 0 && index + 1 != quads) {
            return None;
        }
        let mut bits: u32 = 0;
        for &byte in &quad[..4 - padding] {
            bits = (bits << 6) | base64_value(byte)?;
        }
        bits <<= 6 * u32::try_from(padding).ok()?;
        let bytes = bits.to_be_bytes();
        decoded.extend_from_slice(&bytes[1..4 - padding]);
    }
    Some(decoded)
}

/// One base64 symbol's value.
fn base64_value(byte: u8) -> Option<u32> {
    let value = match byte {
        b'A'..=b'Z' => byte - b'A',
        b'a'..=b'z' => byte - b'a' + 26,
        b'0'..=b'9' => byte - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => return None,
    };
    Some(u32::from(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag_of(input: &str) -> String {
        resolve("", input).unwrap().0
    }

    #[test]
    fn plain_scalars_resolve_as_yaml_v3() {
        for (input, tag) in [
            ("", NULL_TAG),
            ("~", NULL_TAG),
            ("Null", NULL_TAG),
            ("nULL", STR_TAG),
            ("True", BOOL_TAG),
            ("tRUE", STR_TAG),
            ("yes", STR_TAG),
            ("on", STR_TAG),
            // The parser, not resolve, tags a plain << as a merge: its first byte has no hint.
            ("<<", STR_TAG),
            ("9090", INT_TAG),
            ("1_000", INT_TAG),
            ("0x10", INT_TAG),
            ("0o17", INT_TAG),
            ("0777", INT_TAG),
            ("09", FLOAT_TAG),
            ("0b101", INT_TAG),
            ("-0b101", INT_TAG),
            ("18446744073709551615", INT_TAG),
            ("18446744073709551616", FLOAT_TAG),
            ("1.5", FLOAT_TAG),
            ("1e999", STR_TAG),
            (".5", FLOAT_TAG),
            (".5_5", FLOAT_TAG),
            ("._5", STR_TAG),
            (".inf", FLOAT_TAG),
            (".iNf", STR_TAG),
            ("0x1p-2", STR_TAG),
            ("2026-01-02", TIMESTAMP_TAG),
            ("2026-1-2", TIMESTAMP_TAG),
            ("2026-02-29", STR_TAG),
            ("2024-02-29", TIMESTAMP_TAG),
            ("2026-13-01", STR_TAG),
            ("2026-01-02T03:04:05Z", TIMESTAMP_TAG),
            ("2026-01-02t3:4:5.123+01:00", TIMESTAMP_TAG),
            ("2026-01-02 03:04:05.5", TIMESTAMP_TAG),
            ("2026-01-02 03:04:05Z", STR_TAG),
            ("2026-01-02T24:00:00Z", STR_TAG),
            (":9090", STR_TAG),
            ("127.0.0.1:6060", STR_TAG),
            ("/dev/sda", STR_TAG),
        ] {
            assert_eq!(tag_of(input), tag, "{input:?}");
        }
    }

    #[test]
    fn explicit_tags_must_fit() {
        assert_eq!(
            resolve("tag:yaml.org,2002:bool", "true"),
            Ok((BOOL_TAG.to_owned(), Resolved::Bool(true)))
        );
        assert_eq!(
            resolve("!!bool", "yes"),
            Err("cannot decode !!str `yes` as a !!bool".to_owned())
        );
        assert_eq!(
            resolve("!!float", "1"),
            Ok((FLOAT_TAG.to_owned(), Resolved::Float))
        );
        assert_eq!(
            resolve("!!float", "18446744073709551615"),
            Err("cannot decode !!int `18446744073709551615` as a !!float".to_owned())
        );
        assert_eq!(
            resolve("!custom", "x"),
            Ok(("!custom".to_owned(), Resolved::Text("x".to_owned())))
        );
        assert_eq!(
            resolve("!!str", "true"),
            Ok((STR_TAG.to_owned(), Resolved::Text("true".to_owned())))
        );
    }

    #[test]
    fn base64_is_padded_standard() {
        assert_eq!(decode_base64("aGk="), Some(b"hi".to_vec()));
        assert_eq!(decode_base64("aGVs\nbG8="), Some(b"hello".to_vec()));
        assert_eq!(decode_base64("aGk"), None);
        assert_eq!(decode_base64("a=k="), None);
        assert_eq!(decode_base64("aGk=aGk="), None);
        assert_eq!(decode_base64(""), Some(Vec::new()));
    }
}
