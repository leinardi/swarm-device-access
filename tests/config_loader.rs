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

//! The config loader's behaviour on a corpus of files (`tests/testdata/config/loader-cases.json`):
//! `LoadFile` on the loader cases and the YAML edge cases (booleans, null and tilde, empty
//! documents, anchors and aliases, merge keys, duplicate and unknown keys, explicit tags, several
//! documents, syntax errors, CRLF, a byte order mark, and the number spellings that no longer
//! resolve to numbers), each written to a `config.yaml` whose path the file writes `{path}`; the
//! shipped `deployments/docker/config.yaml`; and `validate_enums` for both sources.
//!
//! The corpus started as the 1.x loader's golden. Each expectation here is what this loader does:
//! where it reads differently from 1.x (README Q1, Q6, Q13 and Q17: `plain` is no log format, the
//! parser's wording, `0x1F90` and `1_0.5` are strings, the operating system's text for a missing
//! file) the file says so. A syntax error is compared up to and including `yaml: `, since the rest
//! is saphyr-parser's wording.
#![expect(
    clippy::unwrap_used,
    reason = "the corpus readers are test helpers outside #[test] functions; a malformed corpus \
              should fail the test at the line that read it"
)]

use std::fs;
use std::path::PathBuf;

use serde_json::Value as Json;
use swarm_device_access::config::{FileSchema, LoadError, load_file, validate_enums};

fn corpus() -> Json {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/testdata/config/loader-cases.json");
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

/// A schema of the corpus; `None` for `null`.
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

/// Compares one load with its expectation, `path` standing for `{path}` in the error.
fn check(name: &str, case: &Json, path: &str, got: Result<FileSchema, LoadError>) {
    let want_error = text(&case["error"]).replace("{path}", path);
    match got {
        Ok(got) => {
            assert_eq!(want_error, "", "{name}: accepted");
            assert_eq!(Some(got), schema(&case["schema"]), "{name}");
        }
        Err(error) => {
            let got = error.to_string();
            // The parser's own words are saphyr-parser's: compare through `yaml: `.
            match want_error.find("yaml: ") {
                Some(at) => {
                    let prefix = &want_error[..at + "yaml: ".len()];
                    assert!(
                        got.starts_with(prefix),
                        "{name}: {got:?} against {want_error:?}"
                    );
                }
                None => assert_eq!(got, want_error, "{name}"),
            }
        }
    }
}

#[test]
fn load_behaves_as_the_corpus_says() {
    let directory = std::env::temp_dir().join(format!("sda-config-loader-{}", std::process::id()));
    let cases = corpus()["load"].as_array().unwrap().clone();
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
    fs::remove_dir_all(&directory).unwrap();
}

/// The number spellings 1.x's yaml.v3 resolved to numbers and the standard library's parsers do
/// not read are strings now, so a string key takes them (Q17).
#[test]
fn unsupported_number_spellings_are_strings() {
    let cases = corpus()["load"].as_array().unwrap().clone();
    for name in ["hex number addr", "dot float with underscore"] {
        let case = cases
            .iter()
            .find(|case| text(&case["name"]) == name)
            .unwrap();
        assert_eq!(text(&case["error"]), "", "{name}");
        assert!(!case["schema"].is_null(), "{name}");
    }
}

#[test]
fn shipped_example_loads() {
    let case = &corpus()["shippedExample"];
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(text(&case["name"]));
    let file = fs::read_to_string(&path).unwrap();
    assert_eq!(file, text(&case["input"]), "the corpus is out of date");
    check(
        "shipped example",
        case,
        "",
        load_file(path.to_str().unwrap()),
    );
}

#[test]
fn validate_enums_behaves_as_the_corpus_says() {
    let cases = corpus()["validateEnums"].as_array().unwrap().clone();
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
