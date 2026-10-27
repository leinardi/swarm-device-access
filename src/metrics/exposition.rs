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

// Adapted from swarm-scheduler-exporter/src/metrics/exposition.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The `/metrics` answer: promhttp's content negotiation, and the body.
//!
//! promhttp (the Prometheus Go client v1.24.1, common v0.70.1) picks a format with expfmt's
//! `Negotiate` among Go's four (protobuf delimited, text and compact-text, and text 0.0.4) from the
//! first `Accept` header, which goautoneg parses and sorts, and appends `; escaping=` from the
//! `escaping` parameters it meets on the way. It picks a compression with gddo's
//! `NegotiateContentEncoding` over every `Accept-Encoding` header, offering identity then gzip.
//!
//! Q4: the daemon only writes text 0.0.4. The negotiation is ported in full anyway, protobuf
//! included, because it decides the `escaping` parameter: Go stops at the first clause that
//! matches any of its formats, so a protobuf clause ahead of a text one keeps the default
//! `underscores`. The `Content-Type` is then always the text one, with the escaping Go would have
//! echoed; the body is the same for every escaping, because every name the daemon writes is
//! legacy-valid.
//!
//! swarm-scheduler-exporter ports common v0.71.0, whose `NegotiateAccept` lets a wildcard type or
//! subtype match a format; v0.70.1, which this daemon's Go build uses, matches the protobuf
//! formats on their exact type and the text format on `text/plain` only, so a clause such as
//! `application/*;proto=...;encoding=delimited` is passed over and a later clause's `escaping`
//! counts.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;

use flate2::Compression;
use flate2::write::GzEncoder;

use crate::go_compat::float;
use crate::go_compat::parse_float::{ParseFloatError, parse_float32};
use crate::go_compat::sort;

use super::Gathered;
use super::text::{self, TEXT_CONTENT_TYPE};

/// One clause of an `Accept` header, as goautoneg's `Accept`.
#[derive(Debug, Clone, PartialEq)]
struct AcceptClause {
    media_type: String,
    subtype: String,
    quality: f64,
    // A Go map: a parameter given twice keeps its last value.
    params: BTreeMap<String, String>,
}

impl AcceptClause {
    fn param(&self, key: &str) -> &str {
        self.params.get(key).map_or("", String::as_str)
    }
}

/// goautoneg's `nextSplitElement`: before and after the first `separator`.
fn next_split(text: &str, separator: char) -> (&str, &str) {
    text.split_once(separator).unwrap_or((text, ""))
}

/// goautoneg's trim, of spaces only.
fn trim_spaces(text: &str) -> &str {
    text.trim_matches(' ')
}

/// goautoneg's `ParseAccept`: the clauses, sorted by Go's `sort.Sort` with goautoneg's
/// comparison (higher q first, then a concrete type before `*`, then a concrete subtype before
/// `*`), which is not a total order, so only Go's algorithm gives Go's order.
fn parse_accept(header: &str) -> Vec<AcceptClause> {
    let mut clauses = Vec::new();
    let mut remaining = header;
    while !remaining.is_empty() {
        let (part, rest) = next_split(remaining, ',');
        remaining = rest;
        let part = trim_spaces(part);
        let (media_range, mut params) = next_split(part, ';');
        let (media_type, subtype_rest) = next_split(media_range, '/');
        let media_type = trim_spaces(media_type).to_owned();
        let subtype = if subtype_rest.is_empty() {
            if media_type != "*" {
                continue;
            }
            "*".to_owned()
        } else {
            let (subtype, extra) = next_split(subtype_rest, '/');
            if !extra.is_empty() {
                continue;
            }
            trim_spaces(subtype).to_owned()
        };
        let mut clause = AcceptClause {
            media_type,
            subtype,
            quality: 1.0,
            params: BTreeMap::new(),
        };
        while !params.is_empty() {
            let (param, rest) = next_split(params, ';');
            params = rest;
            let (key, value_rest) = next_split(param, '=');
            if value_rest.is_empty() {
                continue;
            }
            let (value, extra) = next_split(value_rest, '=');
            if !extra.is_empty() {
                continue;
            }
            let token = trim_spaces(key);
            if token == "q" {
                // a.Q, _ = strconv.ParseFloat(value, 32): untrimmed, 0 on a syntax error, the
                // infinity on a range error.
                clause.quality = f64::from(match parse_float32(value) {
                    Ok(quality) | Err(ParseFloatError::Range(quality)) => quality,
                    Err(ParseFloatError::Syntax) => 0.0,
                });
            } else {
                clause
                    .params
                    .insert(token.to_owned(), trim_spaces(value).to_owned());
            }
        }
        clauses.push(clause);
    }
    sort::sort(&mut clauses, |left, right| {
        left.quality > right.quality
            || (left.media_type != "*" && right.media_type == "*")
            || (left.subtype != "*" && right.subtype == "*")
    });
    clauses
}

const PROTOBUF: &str = "vnd.google.protobuf";
const METRIC_FAMILY: &str = "io.prometheus.client.MetricFamily";

/// `Negotiate`'s formats, in its order: `FmtProtoDelim`, `FmtProtoText`, `FmtProtoCompact`,
/// `FmtText`.
const GO_FORMATS: [&str; 4] = [
    "application/vnd.google.protobuf; proto=io.prometheus.client.MetricFamily; encoding=delimited",
    "application/vnd.google.protobuf; proto=io.prometheus.client.MetricFamily; encoding=text",
    "application/vnd.google.protobuf; proto=io.prometheus.client.MetricFamily; \
     encoding=compact-text",
    TEXT_CONTENT_TYPE,
];

/// The index of the text format in [`GO_FORMATS`].
const GO_TEXT: usize = 3;

/// expfmt's `Negotiate` (common v0.70.1) for an `Accept` header: the format among
/// [`GO_FORMATS`], and the escaping scheme. A clause picks a protobuf format when its type is
/// exactly `application/vnd.google.protobuf` with the `MetricFamily` proto and a known encoding,
/// and the text format when it is exactly `text/plain` with version 0.0.4 or none; any other
/// clause is passed over, its `escaping` still counted.
fn negotiate_accept(accept: &str) -> (usize, String) {
    let mut escaping = "underscores".to_owned();
    for clause in parse_accept(accept) {
        let escape_param = clause.param("escaping");
        // An unknown scheme is ignored.
        if matches!(
            escape_param,
            "allow-utf-8" | "underscores" | "dots" | "values"
        ) {
            escape_param.clone_into(&mut escaping);
        }
        if clause.media_type == "application"
            && clause.subtype == PROTOBUF
            && clause.param("proto") == METRIC_FAMILY
        {
            let format = match clause.param("encoding") {
                "delimited" => Some(0),
                "text" => Some(1),
                "compact-text" => Some(2),
                _ => None,
            };
            if let Some(format) = format {
                return (format, escaping);
            }
        }
        let version = clause.param("version");
        if clause.media_type == "text"
            && clause.subtype == "plain"
            && (version == "0.0.4" || version.is_empty())
        {
            return (GO_TEXT, escaping);
        }
    }
    (GO_TEXT, escaping)
}

/// The `Content-Type` Go's promhttp answers for the first `Accept` header (`None` without one).
#[must_use]
pub fn go_content_type(accept: Option<&str>) -> String {
    let (format, escaping) = negotiate_accept(accept.unwrap_or_default());
    format!("{}; escaping={escaping}", GO_FORMATS[format])
}

/// The `Content-Type` the daemon answers (Q4): always text 0.0.4, with the escaping Go would
/// have echoed for the first `Accept` header.
#[must_use]
pub fn content_type(accept: Option<&str>) -> String {
    let (_, escaping) = negotiate_accept(accept.unwrap_or_default());
    format!("{TEXT_CONTENT_TYPE}; escaping={escaping}")
}

/// A compression promhttp offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// No compression, and no `Content-Encoding`.
    Identity,
    /// `Content-Encoding: gzip`.
    Gzip,
}

impl Encoding {
    /// The `Content-Encoding` value, or none for identity.
    #[must_use]
    pub fn header(self) -> Option<&'static str> {
        match self {
            Self::Identity => None,
            Self::Gzip => Some("gzip"),
        }
    }
}

/// One `Accept-Encoding` element, as gddo's `AcceptSpec`.
struct AcceptSpec<'a> {
    value: &'a str,
    quality: f64,
}

/// gddo's `octetTypes`: a token character.
fn is_token(byte: u8) -> bool {
    byte.is_ascii() && !byte.is_ascii_control() && !b" \t\"(),/:;<=>?@[]\\{}".contains(&byte)
}

/// gddo's `isSpace`: space, tab, CR or LF.
fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}

fn skip_space(text: &str) -> &str {
    text.trim_start_matches(|character: char| u8::try_from(character).is_ok_and(is_space))
}

/// gddo's `expectTokenSlash`: the leading run of token characters and `/`.
fn expect_token_slash(text: &str) -> (&str, &str) {
    let end = text
        .bytes()
        .position(|byte| !is_token(byte) && byte != b'/')
        .unwrap_or(text.len());
    text.split_at(end)
}

/// gddo's `expectQuality`: `0` or `1`, then optionally a point and digits; -1 when it starts
/// otherwise. The digits accumulate in Go `int`s, wrapping as they do.
fn expect_quality(text: &str) -> (f64, &str) {
    let quality = match text.as_bytes().first() {
        Some(b'0') => 0.0,
        Some(b'1') => 1.0,
        _ => return (-1.0, ""),
    };
    let rest = &text[1..];
    let Some(rest) = rest.strip_prefix('.') else {
        return (quality, rest);
    };
    let end = rest
        .bytes()
        .position(|byte| !byte.is_ascii_digit())
        .unwrap_or(rest.len());
    let (mut numerator, mut denominator) = (0_i64, 1_i64);
    for byte in rest[..end].bytes() {
        numerator = numerator
            .wrapping_mul(10)
            .wrapping_add(i64::from(byte - b'0'));
        denominator = denominator.wrapping_mul(10);
    }
    (
        quality + float::from_int(numerator) / float::from_int(denominator),
        &rest[end..],
    )
}

/// gddo's `header.ParseAccept` over every value of the header.
fn parse_accept_encoding<'a>(values: &[&'a str]) -> Vec<AcceptSpec<'a>> {
    let mut specs = Vec::new();
    'values: for &header in values {
        let mut text = header;
        loop {
            let (value, rest) = expect_token_slash(text);
            if value.is_empty() {
                continue 'values;
            }
            let mut quality = 1.0;
            text = skip_space(rest);
            if let Some(after) = text.strip_prefix(';') {
                let Some(after) = skip_space(after).strip_prefix("q=") else {
                    continue 'values;
                };
                let rest;
                (quality, rest) = expect_quality(after);
                text = rest;
                if quality < 0.0 {
                    continue 'values;
                }
            }
            specs.push(AcceptSpec { value, quality });
            text = skip_space(text);
            let Some(after) = text.strip_prefix(',') else {
                continue 'values;
            };
            text = skip_space(after);
        }
    }
    specs
}

/// gddo's `NegotiateContentEncoding` over promhttp's offers, identity then gzip: the first offer
/// with a strictly higher q wins, so a tie goes to identity, and a best q of 0 (which gddo turns
/// into no encoding) is identity too. Values are compared case-sensitively.
#[must_use]
pub fn negotiate_encoding(accept_encoding: &[&str]) -> Encoding {
    let specs = parse_accept_encoding(accept_encoding);
    let mut best = Encoding::Identity;
    let mut best_q = -1.0;
    for (offer, name) in [(Encoding::Identity, "identity"), (Encoding::Gzip, "gzip")] {
        for spec in &specs {
            if spec.quality > best_q && (spec.value == "*" || spec.value == name) {
                best_q = spec.quality;
                best = offer;
            }
        }
    }
    if best_q == 0.0 {
        return Encoding::Identity;
    }
    best
}

/// A `/metrics` answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exposition {
    /// The `Content-Type`.
    pub content_type: String,
    /// The `Content-Encoding`, if compressed.
    pub content_encoding: Option<&'static str>,
    /// The body, compressed if `content_encoding` says so.
    pub body: Vec<u8>,
}

/// The answer for `families`, given the first `Accept` header and every `Accept-Encoding` one.
/// `names` is promhttp's `name[]` query filter: when set, only those families are written.
#[must_use]
pub fn render(
    families: &[Gathered],
    accept: Option<&str>,
    accept_encoding: &[&str],
    names: Option<&BTreeSet<String>>,
) -> Exposition {
    let mut body = String::new();
    let selected: Vec<Gathered> = families
        .iter()
        .filter(|family| names.is_none_or(|names| names.contains(&family.name)))
        .cloned()
        .collect();
    text::encode(&mut body, &selected);
    let encoding = negotiate_encoding(accept_encoding);
    let body = match encoding {
        Encoding::Identity => body.into_bytes(),
        Encoding::Gzip => gzip(body.as_bytes()),
    };
    Exposition {
        content_type: content_type(accept),
        content_encoding: encoding.header(),
        body,
    }
}

/// gzip at the default level, with Go's header: no name, mtime 0 and OS 255 (unknown).
fn gzip(data: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    // Writing to and finishing into a Vec cannot fail.
    if encoder.write_all(data).is_err() {
        return Vec::new();
    }
    encoder.finish().unwrap_or_default()
}

#[cfg(test)]
mod tests;
