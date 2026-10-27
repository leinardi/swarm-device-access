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

// Adapted from swarm-scheduler-exporter/tests/go_compat_golden.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The Go formatters against values Go printed, each file a capture from a throwaway Go program
//! of swarm-scheduler-exporter's spikes, reused with the formatters they check:
//!
//! - `floats/go-corpus.tsv` (its S2): bits, `FormatFloat(f, 'g', -1, 64)`, expfmt's `writeFloat`
//!   and a note, for 34 edge values;
//! - `floats/fuzz_numbers.txt` (its S4): bits, `FormatFloat 'g'`, `json.Marshal` (or `ERR`), the
//!   bits shifted into an `int64` and its `Duration.String`, for the first 2,000 of S4's 20,000
//!   random values;
//! - `time/go-layouts.tsv` (its S7): unix seconds, nanoseconds, the zone offset, `RFC3339Nano` and
//!   `2006-01-02T15:04:05.000Z07:00`, for 70 instant and zone pairs;
//! - `strings/go-strings.tsv` (go1.26.10): every pair of 50 words `strings.EqualFold` calls
//!   equal, their `strings.ToLower`, and `strings.TrimSpace` of 10 texts, strings as the hex of
//!   their bytes (`-` for empty).

use std::fs;
use std::path::PathBuf;

use jiff::Timestamp;
use jiff::tz::Offset;
use swarm_device_access::go_compat::{duration, float, strings, time_layout};

fn testdata(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/testdata")
        .join(name);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

fn bits(hex: &str) -> f64 {
    f64::from_bits(u64::from_str_radix(hex, 16).unwrap_or_else(|error| panic!("{hex}: {error}")))
}

/// Compares every line, then fails once with the count and the first mismatches.
fn expect_all(name: &str, mismatches: &[String], checked: usize) {
    assert!(checked > 0, "{name}: no line checked");
    assert!(
        mismatches.is_empty(),
        "{name}: {} of {checked} lines differ from Go, first: {:#?}",
        mismatches.len(),
        &mismatches[..mismatches.len().min(10)]
    );
}

fn check_float_file(name: &str) -> usize {
    let data = testdata(name);
    let mut mismatches = Vec::new();
    let mut checked = 0;
    for line in data.lines() {
        let columns: Vec<&str> = line.split('\t').collect();
        let value = bits(columns[0]);
        let got = float::format_g(value);
        if got != columns[1] {
            mismatches.push(format!("{}: Go {} Rust {got}", columns[0], columns[1]));
        }
        checked += 1;
    }
    expect_all(name, &mismatches, checked);
    checked
}

#[test]
fn format_g_matches_go_on_the_s2_corpus() {
    assert_eq!(check_float_file("floats/go-corpus.tsv"), 34);
}

#[test]
fn floats_json_and_durations_match_go_on_2000_values() {
    let data = testdata("floats/fuzz_numbers.txt");
    let mut mismatches = Vec::new();
    let mut checked = 0;
    for line in data.lines() {
        let columns: Vec<&str> = line.split(' ').collect();
        assert_eq!(columns.len(), 5, "malformed line {line:?}");
        let value = bits(columns[0]);
        let shortest = float::format_g(value);
        let json = float::format_json(value).unwrap_or_else(|_| "ERR".to_owned());
        let nanos: i64 = columns[3]
            .parse()
            .unwrap_or_else(|error| panic!("{line}: {error}"));
        let duration = duration::format(nanos);
        if (shortest.as_str(), json.as_str(), duration.as_str())
            != (columns[1], columns[2], columns[4])
        {
            mismatches.push(format!("Go {line} Rust {shortest} {json} {duration}"));
        }
        checked += 1;
    }
    expect_all("floats/fuzz_numbers.txt", &mismatches, checked);
    assert_eq!(checked, 2_000);
}

#[test]
fn time_layouts_match_go_on_the_s7_corpus() {
    let data = testdata("time/go-layouts.tsv");
    let mut mismatches = Vec::new();
    let mut checked = 0;
    for line in data.lines() {
        let columns: Vec<&str> = line.split('\t').collect();
        let parse = |index: usize| -> i64 {
            columns[index]
                .parse()
                .unwrap_or_else(|error| panic!("{line}: {error}"))
        };
        let timestamp = Timestamp::new(parse(0), i32::try_from(parse(1)).unwrap_or_default())
            .unwrap_or_else(|error| panic!("{line}: {error}"));
        let offset = Offset::from_seconds(i32::try_from(parse(2)).unwrap_or_default())
            .unwrap_or_else(|error| panic!("{line}: {error}"));
        let nano = time_layout::format_rfc3339_nano(timestamp, offset);
        let mut millis = String::new();
        time_layout::push_rfc3339_millis(&mut millis, timestamp, offset);
        if (nano.as_str(), millis.as_str()) != (columns[3], columns[4]) {
            mismatches.push(format!("Go {line} Rust {nano} {millis}"));
        }
        checked += 1;
    }
    expect_all("time/go-layouts.tsv", &mismatches, checked);
    assert_eq!(checked, 70);
}

fn unhex(hex: &str) -> String {
    if hex == "-" {
        return String::new();
    }
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&hex[index..index + 2], 16)
                .unwrap_or_else(|error| panic!("{hex}: {error}"))
        })
        .collect();
    String::from_utf8(bytes).unwrap_or_else(|error| panic!("{hex}: {error}"))
}

#[test]
fn strings_match_go_on_its_unicode_15_tables() {
    let data = testdata("strings/go-strings.tsv");
    let lines: Vec<Vec<&str>> = data
        .lines()
        .map(|line| line.split('\t').collect())
        .collect();
    let words: Vec<String> = lines
        .iter()
        .filter(|fields| fields[0] == "lower")
        .map(|fields| unhex(fields[1]))
        .collect();
    let folds: std::collections::BTreeSet<(String, String)> = lines
        .iter()
        .filter(|fields| fields[0] == "fold")
        .map(|fields| (unhex(fields[1]), unhex(fields[2])))
        .collect();
    let mut mismatches = Vec::new();
    let mut checked = 0;
    for left in &words {
        for right in &words {
            let want = folds.contains(&(left.clone(), right.clone()));
            if strings::equal_fold(left, right) != want {
                mismatches.push(format!("EqualFold({left:?}, {right:?}): Go {want}"));
            }
            checked += 1;
        }
    }
    for fields in &lines {
        let (input, want) = (unhex(fields[1]), unhex(fields[2]));
        let got = match fields[0] {
            "lower" => strings::to_lower(&input),
            "trim" => strings::trim_space(&input).to_owned(),
            _ => continue,
        };
        if got != want {
            mismatches.push(format!(
                "{}({input:?}): Go {want:?} Rust {got:?}",
                fields[0]
            ));
        }
        checked += 1;
    }
    expect_all("strings/go-strings.tsv", &mismatches, checked);
    assert_eq!((words.len(), folds.len()), (50, 124));
}
