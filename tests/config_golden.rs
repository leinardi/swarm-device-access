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
//! The config loader against the Go capture `tests/testdata/config/loader.json`
//! (`internal/config/golden_capture_test.go`): `LoadFile` on the loader test corpus and the
//! yaml.v3 edge cases, each written to a `config.yaml` whose path the golden writes `{path}`;
//! the shipped `deployments/docker/config.yaml`; and `ValidateEnums` for both sources.
//!
//! Every case must be accepted or refused as Go does, with the same schema or the same error
//! text. The one exception is Q6: where Go's text is a yaml.v3 syntax error that saphyr-parser
//! words differently, the error must be the same kind of error with the same text up to and
//! including `yaml: `.
#![expect(
    clippy::unwrap_used,
    reason = "the golden readers are test helpers outside #[test] functions; a malformed golden \
              should fail the test at the line that read it"
)]

use std::fs;
use std::path::PathBuf;

use serde_json::Value as Json;
use swarm_device_access::config::{FileSchema, LoadError, load_file, validate_enums};

/// The cases whose Go error is a yaml.v3 syntax error saphyr-parser words differently (Q6).
const SYNTAX_WORDING: [&str; 6] = [
    "malformed trailing content",
    "deep flow nesting",
    "unclosed flow",
    "tab indent",
    "unterminated quote",
    "bad indentation",
];

/// The cases Go refuses past the parse and saphyr-parser refuses as a syntax error: flow
/// collections nested deeper than its limit of 255, which no accepted config file has (Q6).
const FLOW_DEPTH: [&str; 1] = ["flow nesting within the limit"];

/// The cases Go accepts and saphyr-parser refuses as a syntax error: a quoted value continued
/// on a line indented no more than its key, which YAML 1.2 forbids and libyaml allows (Q6).
const UNINDENTED_CONTINUATION: [&str; 2] = [
    "directive text in a quoted value",
    "unindented quoted continuation",
];

fn golden() -> Json {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/testdata/config/loader.json");
    serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap()
}

fn text(json: &Json) -> &str {
    json.as_str()
        .unwrap_or_else(|| panic!("not a string: {json}"))
}

fn list(json: &Json) -> Option<Vec<String>> {
    json.as_array()
        .map(|items| items.iter().map(|item| text(item).to_owned()).collect())
}

/// A golden schema; `None` for `null`.
fn schema(json: &Json) -> Option<FileSchema> {
    if json.is_null() {
        return None;
    }
    Some(FileSchema {
        log_format: text(&json["logFormat"]).to_owned(),
        log_level: text(&json["logLevel"]).to_owned(),
        log_time: json["logTime"].as_bool(),
        docker_socket: text(&json["dockerSocket"]).to_owned(),
        dry_run: json["dryRun"].as_bool(),
        policy_mode: text(&json["policyMode"]).to_owned(),
        device_allow: list(&json["deviceAllow"]),
        device_deny: list(&json["deviceDeny"]),
        metrics_addr: text(&json["metricsAddr"]).to_owned(),
        debug_addr: text(&json["debugAddr"]).to_owned(),
    })
}

/// Compares one load with its golden, `path` standing for `{path}` in the error.
fn check(name: &str, case: &Json, path: &str, got: Result<FileSchema, LoadError>) {
    let want_error = text(&case["error"]).replace("{path}", path);
    match got {
        Err(error) if UNINDENTED_CONTINUATION.contains(&name) => {
            assert_eq!(want_error, "", "{name}: Go refused it");
            assert!(matches!(error, LoadError::Parse { .. }), "{name}: {error}");
        }
        Ok(got) => {
            assert!(!UNINDENTED_CONTINUATION.contains(&name), "{name}: accepted");
            assert_eq!(want_error, "", "{name}: accepted, Go refused");
            assert_eq!(Some(got), schema(&case["schema"]), "{name}");
        }
        Err(error) => {
            let got = error.to_string();
            if FLOW_DEPTH.contains(&name) {
                assert_ne!(want_error, "", "{name}: Go accepted it");
                assert!(got.starts_with("parse config file "), "{name}: {got:?}");
            } else if SYNTAX_WORDING.contains(&name) {
                let prefix = &want_error[..want_error.find("yaml: ").unwrap() + "yaml: ".len()];
                assert!(
                    got.starts_with(prefix),
                    "{name}: {got:?} against Go's {want_error:?}"
                );
            } else {
                assert_eq!(got, want_error, "{name}");
            }
        }
    }
}

#[test]
fn load_matches_go() {
    let directory = std::env::temp_dir().join(format!("sda-config-golden-{}", std::process::id()));
    let cases = golden()["load"].as_array().unwrap().clone();
    assert!(cases.len() > 100, "the corpus shrank: {}", cases.len());
    for (index, case) in cases.iter().enumerate() {
        let name = text(&case["name"]);
        let case_directory = directory.join(index.to_string());
        fs::create_dir_all(&case_directory).unwrap();
        let path = case_directory.join("config.yaml");
        let path_text = path.to_str().unwrap().to_owned();
        match name {
            "empty path" => check(name, case, "", load_file("")),
            "missing file" => check(name, case, &path_text, load_file(&path_text)),
            _ => {
                fs::write(&path, text(&case["input"])).unwrap();
                check(name, case, &path_text, load_file(&path_text));
            }
        }
    }
    for name in SYNTAX_WORDING
        .iter()
        .chain(&FLOW_DEPTH)
        .chain(&UNINDENTED_CONTINUATION)
    {
        assert!(
            cases.iter().any(|case| text(&case["name"]) == *name),
            "{name}: not in the corpus"
        );
    }
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn shipped_example_matches_go() {
    let case = &golden()["shippedExample"];
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(text(&case["name"]));
    let file = fs::read_to_string(&path).unwrap();
    assert_eq!(file, text(&case["input"]), "the golden is out of date");
    check(
        "shipped example",
        case,
        "",
        load_file(path.to_str().unwrap()),
    );
}

#[test]
fn validate_enums_matches_go() {
    let cases = golden()["validateEnums"].as_array().unwrap().clone();
    assert!(!cases.is_empty());
    for case in cases {
        let got = validate_enums(
            text(&case["logFormat"]),
            text(&case["logLevel"]),
            text(&case["policyMode"]),
            text(&case["source"]),
        );
        let got = got.err().map(|error| error.to_string()).unwrap_or_default();
        assert_eq!(got, text(&case["error"]), "{case}");
    }
}
