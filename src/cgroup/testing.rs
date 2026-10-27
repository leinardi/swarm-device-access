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

//! Helpers the cgroup tests share: rule constructors, the runtime device filter fixtures and the
//! Go goldens under `tests/testdata/devfilter`.
//!
//! Go: the helpers of `v1set_test.go` and `fixtures_test.go`.

use std::fmt::Write as _;

use serde::Deserialize;

use super::api::DeviceRule;
use crate::test_support::testdata;

/// An allow rule.
pub(super) fn rule(kind: &str, major: i64, minor: i64, access: &str) -> DeviceRule {
    rule_of(true, kind, major, minor, access)
}

/// A rule with its verdict.
pub(super) fn rule_of(allow: bool, kind: &str, major: i64, minor: i64, access: &str) -> DeviceRule {
    DeviceRule {
        allow,
        kind: kind.to_owned(),
        major: Some(major),
        minor: Some(minor),
        access: access.to_owned(),
    }
}

/// The runtimes whose device filters were captured from the kernel.
pub(super) const RUNTIME_FIXTURES: [&str; 3] = ["runc", "crun", "systemd"];

/// The nonce the captured `.wrapped` fixtures were produced with.
pub(super) const FIXTURE_NONCE: u64 = 0x0123_4567_89ab_cdef;

/// The rules the captured `.wrapped` fixtures were produced with.
pub(super) fn fixture_rules() -> Vec<DeviceRule> {
    vec![rule("c", 10, 200, "rwm"), rule("b", 8, 0, "r")]
}

/// A fixture under `tests/testdata/devfilter`: one instruction per line, in hex.
pub(super) fn read_fixture(name: &str) -> Vec<u8> {
    let text = String::from_utf8(testdata(&format!("devfilter/{name}"))).unwrap();
    unhex(&text.split_whitespace().collect::<String>())
}

/// The bytes `text` spells in hex.
pub(super) fn unhex(text: &str) -> Vec<u8> {
    assert!(text.len().is_multiple_of(2), "odd hex length: {text}");
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
        .collect()
}

/// `raw` in lower-case hex.
pub(super) fn hex(raw: &[u8]) -> String {
    raw.iter().fold(String::new(), |mut out, byte| {
        write!(out, "{byte:02x}").unwrap();
        out
    })
}

/// One instruction slot per string, as the goldens hold them.
pub(super) fn hex_lines(raw: &[u8]) -> Vec<String> {
    raw.chunks(8).map(hex).collect()
}

/// `tests/testdata/devfilter/golden.json`, written by the Go `TestGoldenDevfilter`.
#[derive(Debug, Deserialize)]
pub(super) struct Golden {
    pub fixtures: Vec<FixtureGolden>,
    pub gate: Vec<GateGolden>,
    pub builder: Vec<BuilderGolden>,
}

impl Golden {
    pub(super) fn load() -> Self {
        serde_json::from_slice(&testdata("devfilter/golden.json")).unwrap()
    }
}

/// What the Go code made of one runtime fixture.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct FixtureGolden {
    pub name: String,
    pub canonical: Vec<String>,
    pub gate: Option<String>,
    pub orig_parse: ParseGolden,
    pub wrapped_parse: ParseGolden,
    pub wrappers: Vec<WrapperGolden>,
    pub conflicts: Vec<ParseCase>,
}

/// The wrapper the Go code emitted for a rule set and nonce, or its error.
#[derive(Debug, Deserialize)]
pub(super) struct WrapperGolden {
    pub case: String,
    pub rules: Option<Vec<DeviceRule>>,
    pub nonce: String,
    pub wrapper: Option<Vec<String>>,
    pub error: Option<String>,
    pub gate: Option<String>,
    pub parse: Option<ParseGolden>,
}

/// What the Go `parseOwned` made of a program.
#[derive(Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ParseGolden {
    pub owned: bool,
    #[serde(default)]
    pub nonce: String,
    #[serde(default)]
    pub rules: Option<Vec<DeviceRule>>,
    #[serde(default)]
    pub original_is_fixture: bool,
    pub error: Option<String>,
}

/// A program and its parse.
#[derive(Debug, Deserialize)]
pub(super) struct ParseCase {
    pub case: String,
    pub program: Vec<String>,
    pub parse: ParseGolden,
}

/// A program and the Go gate's verdict on it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct GateGolden {
    pub case: String,
    pub program: Vec<String>,
    pub map_ids_known: bool,
    pub map_count: usize,
    pub verdict: Option<String>,
}

/// The Go program builder's output for a rule sequence.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct BuilderGolden {
    pub case: String,
    pub appends: Vec<AppendGolden>,
    pub has_wild_card: bool,
    pub original: Vec<String>,
    pub program: Vec<String>,
    pub error: Option<String>,
    pub after_finalize: Option<String>,
}

/// One `appendDevice` call and its error.
#[derive(Debug, Deserialize)]
pub(super) struct AppendGolden {
    pub rule: DeviceRule,
    pub error: Option<String>,
}
