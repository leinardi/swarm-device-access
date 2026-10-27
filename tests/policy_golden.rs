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
//! The policy port against the Go capture `tests/testdata/policy/policy.json`
//! (`internal/policy/golden_capture_test.go`): `strconv.ParseBool`, `ParseMode`,
//! `ParseContainer` with `UnknownLabels`, `ValidateGlobs`, `Global.Validate`, every predicate over
//! globals, containers and paths, and `filepath.Match` and `filepath.Clean`. An error is compared
//! by its text; `""` is no error.
//!
//! A decision's container is rebuilt from labels, the only way a `Container` is made: an empty
//! list, which Go's predicates treat as a nil one, is no label. A decision whose global has an
//! empty or unknown mode cannot be made at all, since `Global::new` refuses it; for those the
//! test checks that Go enabled nothing and that the Rust side refuses the mode.
#![expect(
    clippy::unwrap_used,
    reason = "the golden readers are test helpers outside #[test] functions; a malformed golden \
              should fail the test at the line that read it"
)]

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use serde_json::Value as Json;
use swarm_device_access::go_compat::filepath;
use swarm_device_access::go_compat::parse_bool::parse_bool;
use swarm_device_access::policy::{
    Container, Global, LABEL_DEVICE_ALLOW, LABEL_DEVICE_DENY, LABEL_ENABLE, Mode, parse_mode,
    unknown_labels, validate_globs,
};

fn golden() -> Json {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/testdata/policy/policy.json");
    serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap()
}

fn cases(section: &str) -> Vec<Json> {
    let cases = golden()[section].as_array().unwrap().clone();
    assert!(!cases.is_empty(), "{section}: no case");
    cases
}

fn text(json: &Json) -> &str {
    json.as_str()
        .unwrap_or_else(|| panic!("not a string: {json}"))
}

/// A Go `[]string`: `None` for nil.
fn list(json: &Json) -> Option<Vec<String>> {
    json.as_array()
        .map(|items| items.iter().map(|item| text(item).to_owned()).collect())
}

/// An error's text, `""` for none, as the golden writes it.
fn error_text<T, E: ToString>(result: &Result<T, E>) -> String {
    result
        .as_ref()
        .err()
        .map(ToString::to_string)
        .unwrap_or_default()
}

fn labels(json: &Json) -> HashMap<String, String> {
    json.as_object()
        .unwrap()
        .iter()
        .map(|(key, value)| (key.clone(), text(value).to_owned()))
        .collect()
}

fn global(json: &Json) -> Result<Global, String> {
    Global::new(
        text(&json["mode"]),
        list(&json["deviceAllow"]),
        list(&json["deviceDeny"]),
    )
    .map_err(|error| error.to_string())
}

/// The labels a golden container parses back from.
fn container_labels(json: &Json) -> HashMap<String, String> {
    let mut labels = HashMap::new();
    if let Some(enable) = json["enable"].as_bool() {
        labels.insert(LABEL_ENABLE.to_owned(), enable.to_string());
    }
    for (key, label) in [
        ("deviceAllow", LABEL_DEVICE_ALLOW),
        ("deviceDeny", LABEL_DEVICE_DENY),
    ] {
        if let Some(patterns) = list(&json[key]).filter(|patterns| !patterns.is_empty()) {
            labels.insert(label.to_owned(), patterns.join(","));
        }
    }
    labels
}

#[test]
fn parse_bool_matches_go() {
    for case in cases("parseBool") {
        let got = parse_bool(text(&case["input"]));
        assert_eq!(error_text(&got), text(&case["error"]), "{case}");
        if let Ok(value) = got {
            assert_eq!(Some(value), case["value"].as_bool(), "{case}");
        }
    }
}

#[test]
fn parse_mode_matches_go() {
    for case in cases("parseMode") {
        let got = parse_mode(text(&case["input"]));
        assert_eq!(error_text(&got), text(&case["error"]), "{case}");
        let mode = got.map(Mode::as_str).unwrap_or_default();
        assert_eq!(mode, text(&case["mode"]), "{case}");
    }
}

#[test]
fn parse_container_matches_go() {
    for case in cases("parseContainer") {
        let labels = labels(&case["labels"]);
        let got = Container::parse(&labels);
        assert_eq!(error_text(&got), text(&case["error"]), "{case}");
        let want = &case["container"];
        match got {
            Ok(container) => {
                assert_eq!(container.enable(), want["enable"].as_bool(), "{case}");
                let allow = list(&want["deviceAllow"]);
                let deny = list(&want["deviceDeny"]);
                assert_eq!(container.device_allow(), allow.as_deref(), "{case}");
                assert_eq!(container.device_deny(), deny.as_deref(), "{case}");
            }
            Err(_) => assert!(want.is_null(), "{case}"),
        }
        let unknown = list(&case["unknownLabels"]);
        assert_ne!(
            unknown.as_deref(),
            Some(&[][..]),
            "Go returns nil, never empty"
        );
        assert_eq!(
            unknown_labels(&labels),
            unknown.unwrap_or_default(),
            "{case}"
        );
    }
}

#[test]
fn validate_globs_matches_go() {
    for case in cases("validateGlobs") {
        let patterns = list(&case["patterns"]).unwrap_or_default();
        let got = validate_globs(&patterns);
        assert_eq!(error_text(&got), text(&case["error"]), "{case}");
    }
}

#[test]
fn global_validate_matches_go() {
    for case in cases("globalValidate") {
        let got = global(&case);
        assert_eq!(error_text(&got), text(&case["validateError"]), "{case}");
    }
}

#[test]
fn decisions_match_go() {
    let mut refused = 0;
    for case in cases("decisions") {
        let path = text(&case["path"]);
        let Ok(global) = global(&case["global"]) else {
            // Go evaluated this policy anyway, and must have enabled nothing.
            assert_eq!(case["enabled"].as_bool(), Some(false), "{case}");
            refused += 1;
            continue;
        };
        let container = Container::parse(&container_labels(&case["container"])).unwrap();
        let got = [
            global.enabled(&container),
            global.denied(&container, path),
            global.authorized(&container, path),
            global.device_allowed(&container, path),
            global.explicitly_allowed(&container, path),
        ];
        let want = [
            "enabled",
            "denied",
            "authorized",
            "deviceAllowed",
            "explicitlyAllowed",
        ]
        .map(|key| case[key].as_bool().unwrap());
        assert_eq!(got, want, "{case}");
    }
    // The two invalid modes, over 7 containers and 5 paths each.
    assert_eq!(refused, 70);
}

#[test]
fn filepath_match_matches_go() {
    for case in cases("filepathMatch") {
        let got = filepath::matches(text(&case["pattern"]), text(&case["name"]));
        assert_eq!(error_text(&got), text(&case["error"]), "{case}");
        assert_eq!(
            got.unwrap_or(false),
            case["matched"].as_bool().unwrap(),
            "{case}"
        );
    }
}

#[test]
fn filepath_clean_matches_go() {
    for case in cases("filepathClean") {
        assert_eq!(
            filepath::clean(text(&case["input"])),
            text(&case["output"]),
            "{case}"
        );
    }
}
