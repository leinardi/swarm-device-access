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

// Adapted from swarm-scheduler-exporter/tests/logger_golden.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The logger against the Go capture: the records of `tests/testdata/logger/corpus.json`,
//! logged by Go through slog's text and JSON handlers and this project's plain handler, each
//! with and without the time (`tests/testdata/logger/{text,json,plain}_{time,notime}.log`,
//! written by `internal/logger/golden_capture_test.go`), compared whole, byte for byte.
//!
//! The corpus is read rather than rebuilt here, so the Rust side logs exactly the records the Go
//! side logged: each attribute carries its slog kind and its value as text (numbers as decimal
//! strings, durations in nanoseconds, times in RFC 3339 with their offset), and `with` lists the
//! `With` and `WithGroup` calls in order.
#![expect(
    clippy::unwrap_used,
    reason = "the corpus readers are test helpers outside #[test] functions; a malformed corpus \
              should fail the test at the line that read it"
)]

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use jiff::Timestamp;
use jiff::tz::{Offset, TimeZone};
use serde_json::Value as Json;
use swarm_device_access::logger::{
    Attr, Config, Format, Level, Logger, MemorySink, Time, Value, parse_level,
};

fn testdata(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/testdata/logger")
        .join(name);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn corpus() -> Json {
    serde_json::from_str(&testdata("corpus.json")).unwrap()
}

fn text(json: &Json) -> &str {
    json.as_str()
        .unwrap_or_else(|| panic!("not a string: {json}"))
}

/// A Go `time.Time` in RFC 3339, with the offset it was written at.
fn time(rfc3339: &str) -> Time {
    let timestamp: Timestamp = rfc3339.parse().unwrap();
    let offset = if rfc3339.ends_with('Z') {
        Offset::UTC
    } else {
        let zone = &rfc3339[rfc3339.len() - 6..];
        let sign = if zone.starts_with('-') { -1 } else { 1 };
        let hours: i32 = zone[1..3].parse().unwrap();
        let minutes: i32 = zone[4..6].parse().unwrap();
        Offset::from_seconds(sign * (hours * 3600 + minutes * 60)).unwrap()
    };
    Time { timestamp, offset }
}

/// One corpus attribute, by its kind.
fn attr(json: &Json) -> Attr {
    let key = text(&json["key"]).to_owned();
    let value = &json["value"];
    let value = match text(&json["kind"]) {
        "string" => Value::Str(text(value).to_owned()),
        "stringer" => Value::Stringer(text(value).to_owned()),
        "error" => Value::Error(text(value).to_owned()),
        "int" => Value::Int(text(value).parse().unwrap()),
        "uint" => Value::Uint(text(value).parse().unwrap()),
        "float" => Value::Float(text(value).parse().unwrap()),
        "bool" => Value::Bool(value.as_bool().unwrap()),
        "duration" => Value::Duration(text(value).parse().unwrap()),
        "time" => Value::Time(time(text(value))),
        "strings" if value.is_null() => Value::NilStrings,
        "strings" => Value::Strings(
            value
                .as_array()
                .unwrap()
                .iter()
                .map(|item| text(item).to_owned())
                .collect(),
        ),
        "nil" => Value::Nil,
        "group" => return Attr::group(key, attrs(&json["attrs"])),
        kind => panic!("unknown kind {kind}"),
    };
    Attr::new(key, value)
}

/// A list of corpus attributes; absent is empty.
fn attrs(json: &Json) -> Vec<Attr> {
    json.as_array()
        .map(|items| items.iter().map(attr).collect())
        .unwrap_or_default()
}

/// Logs every corpus record through a `format` logger and returns the output.
fn log_corpus(format: Format, include_time: bool) -> String {
    let corpus = corpus();
    let record_time = time(text(&corpus["time"]));
    let sink = MemorySink::default();
    let base = Logger::new(
        Config {
            format,
            level: Level::DEBUG,
            include_time,
        },
        Box::new(sink.clone()),
        TimeZone::fixed(record_time.offset),
        Arc::new(move || record_time.timestamp),
    );
    for record in corpus["records"].as_array().unwrap() {
        let mut logger = base.clone();
        for step in record["with"].as_array().into_iter().flatten() {
            logger = match step.get("group") {
                Some(name) => logger.with_group(text(name)),
                None => logger.with_attrs(&attrs(&step["attrs"])),
            };
        }
        let level = i32::try_from(record["level"].as_i64().unwrap()).unwrap();
        logger.log(
            Level(level),
            text(&record["message"]),
            &attrs(&record["attrs"]),
        );
    }
    sink.contents()
}

fn compare(format: Format, include_time: bool, file: &str) {
    let got = log_corpus(format, include_time);
    let want = testdata(file);
    if got != want {
        let first = got
            .lines()
            .zip(want.lines())
            .position(|(got, want)| got != want);
        panic!("{file} differs, first at line {first:?}\n--- got\n{got}\n--- want\n{want}");
    }
}

#[test]
fn text_with_time_matches_go() {
    compare(Format::Text, true, "text_time.log");
}

#[test]
fn text_without_time_matches_go() {
    compare(Format::Text, false, "text_notime.log");
}

#[test]
fn json_with_time_matches_go() {
    compare(Format::Json, true, "json_time.log");
}

#[test]
fn json_without_time_matches_go() {
    compare(Format::Json, false, "json_notime.log");
}

#[test]
fn plain_with_time_matches_go() {
    compare(Format::Plain, true, "plain_time.log");
}

#[test]
fn plain_without_time_matches_go() {
    compare(Format::Plain, false, "plain_notime.log");
}

/// The level names of the corpus read as Go's `parseLevel` read them.
#[test]
fn levels_match_go() {
    let corpus = corpus();
    for case in corpus["levels"].as_array().unwrap() {
        let input = text(&case["input"]);
        let want = i32::try_from(case["level"].as_i64().unwrap()).unwrap();
        assert_eq!(parse_level(input), Level(want), "parse_level({input:?})");
    }
}
