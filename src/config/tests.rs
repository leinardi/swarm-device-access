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
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{env, fs, process};

use super::*;

/// A fresh directory per call, removed when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = env::temp_dir().join(format!(
            "sda-config-{}-{}",
            process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    /// Go's `writeConfig`: `content` in a `config.yaml` here.
    fn write_config(&self, content: &str) -> String {
        let path = self.0.join("config.yaml");
        fs::write(&path, content).unwrap();
        path.to_str().unwrap().to_owned()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|&value| value.to_owned()).collect()
}

/// Go: `TestLoadFile_ValidFile`
#[test]
fn load_file_valid_file() {
    let directory = TempDir::new();
    let path = directory.write_config(
        r#"
log-format: json
log-level: debug
log-time: true
docker-socket: /run/docker.sock
dry-run: false
policy-mode: all
device-allow:
  - /dev/nvidia*
  - /dev/dri/*
device-deny: []
metrics-addr: ""
debug-addr: "127.0.0.1:6060"
"#,
    );
    let want = FileSchema {
        log_format: "json".to_owned(),
        log_level: "debug".to_owned(),
        log_time: Some(true),
        docker_socket: "/run/docker.sock".to_owned(),
        dry_run: Some(false),
        policy_mode: "all".to_owned(),
        device_allow: Some(strings(&["/dev/nvidia*", "/dev/dri/*"])),
        device_deny: Some(Vec::new()),
        metrics_addr: String::new(),
        debug_addr: "127.0.0.1:6060".to_owned(),
    };
    assert_eq!(load_file(&path).unwrap(), want);
}

/// Go: `TestLoadFile_EmptyInputs`
#[test]
fn load_file_empty_inputs() {
    assert_eq!(load_file("").unwrap(), FileSchema::default(), "empty path");
    for (name, content) in [
        ("empty file", ""),
        ("only comments", "# nothing set\n"),
        ("bare doc marker", "---\n"),
        ("shipped template", "# log-level: info\n# device-deny: []\n"),
    ] {
        let directory = TempDir::new();
        let got = load_file(&directory.write_config(content));
        assert_eq!(got.unwrap(), FileSchema::default(), "{name}");
    }
}

/// Go: `TestLoadFile_ShippedExample`
#[test]
fn load_file_shipped_example() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("deployments/docker/config.yaml");
    if let Err(error) = load_file(path.to_str().unwrap()) {
        panic!("the shipped example must load: {error}");
    }
}

/// Go: `TestLoadFile_UnreadablePath`, plus Go's `PathError` text for the open and the read.
#[test]
fn load_file_unreadable_path() {
    let directory = TempDir::new();
    let missing = directory.0.join("missing.yaml");
    let missing = missing.to_str().unwrap();
    let error = load_file(missing).unwrap_err().to_string();
    assert_eq!(
        error,
        format!(
            "read config file {}: open {missing}: no such file or directory",
            quote(missing)
        )
    );
    let directory_path = directory.0.to_str().unwrap();
    let error = load_file(directory_path).unwrap_err().to_string();
    assert_eq!(
        error,
        format!(
            "read config file {}: read {directory_path}: is a directory",
            quote(directory_path)
        )
    );
}

/// The files Go's `TestLoadFile_Rejects` refuses, with a fragment of the error.
const REJECTS: [(&str, &str, &str); 26] = [
    (
        "unknown key",
        "device_deny:\n  - /dev/sda\n",
        "field device_deny not found",
    ),
    (
        "second document",
        "dry-run: true\n---\npolicy-mode: all\n",
        "multiple YAML documents",
    ),
    (
        "empty second document",
        "dry-run: true\n---\n",
        "multiple YAML documents",
    ),
    (
        "document after an empty one",
        "---\n---\ndry-run: true\n",
        "multiple YAML documents",
    ),
    (
        "malformed trailing content",
        "dry-run: true\n---\n: : [\n",
        "trailing content",
    ),
    (
        "duplicate key",
        "policy-mode: all\npolicy-mode: opt-in\n",
        "already defined",
    ),
    ("root not a mapping", "- dry-run\n", "must be a mapping"),
    ("explicit null document", "~\n", "must be a mapping"),
    (
        "dry-run as a string",
        "dry-run: \"yes\"\n",
        r#"key "dry-run" must be a boolean"#,
    ),
    (
        "dry-run null",
        "dry-run: null\n",
        r#"key "dry-run" must be a boolean"#,
    ),
    (
        "policy-mode null",
        "policy-mode: ~\n",
        r#"key "policy-mode" must be a non-empty string"#,
    ),
    (
        "policy-mode empty",
        "policy-mode: \"\"\n",
        r#"key "policy-mode" must be a non-empty string"#,
    ),
    (
        "device-allow null",
        "device-allow:\n",
        r#"key "device-allow" must be a list"#,
    ),
    (
        "device-deny null",
        "device-deny: null\n",
        r#"key "device-deny" must be a list"#,
    ),
    (
        "device-allow null element",
        "device-allow: [null]\n",
        r#"key "device-allow" must be a list"#,
    ),
    (
        "device-deny empty element",
        "device-deny: [\"\"]\n",
        r#"key "device-deny" must be a list"#,
    ),
    (
        "device-deny scalar",
        "device-deny: /dev/sda\n",
        r#"key "device-deny" must be a list"#,
    ),
    (
        "log-time null",
        "log-time: null\n",
        r#"key "log-time" must be a boolean"#,
    ),
    (
        "metrics-addr number",
        "metrics-addr: 9090\n",
        r#"key "metrics-addr" must be a string"#,
    ),
    (
        "null behind an alias",
        "x: &n null\ndevice-deny: *n\n",
        r#"key "device-deny" must be a list"#,
    ),
    (
        "null element behind an alias",
        "x: &n null\ndevice-allow: [*n]\n",
        r#"key "device-allow" must be a list"#,
    ),
    (
        "inline merge",
        "<<: {device-deny: null}\n",
        "YAML merge keys are not supported",
    ),
    (
        "aliased merge",
        "base: &b {dry-run: false}\n<<: *b\n",
        "YAML merge keys are not supported",
    ),
    (
        "unknown log-format",
        "log-format: xml\n",
        r#"key "log-format": unknown value "xml""#,
    ),
    (
        "retired log-level alias",
        "log-level: warning\n",
        r#"key "log-level": unknown value "warning""#,
    ),
    (
        "unknown policy-mode",
        "policy-mode: everything\n",
        r#"key "policy-mode""#,
    ),
];

/// Go: `TestLoadFile_Rejects`: every file the loader must refuse, with a fragment the error must
/// contain, and the file's path.
#[test]
fn load_file_rejects() {
    for (name, content, want) in REJECTS {
        let directory = TempDir::new();
        let path = directory.write_config(content);
        let Err(error) = load_file(&path) else {
            panic!("{name}: load_file accepted {content:?}");
        };
        let error = error.to_string();
        assert!(
            error.contains(want) && error.contains(&path),
            "{name}: {error:?} should name the file and contain {want:?}"
        );
    }
}

/// Go: `TestLoadFile_LogTimeAbsentVsNull`: an absent `log-time` leaves the flag in force; an
/// explicit null is an error like any other key, not a silent "unset".
#[test]
fn load_file_log_time_absent_vs_null() {
    let directory = TempDir::new();
    let got = load_file(&directory.write_config("log-level: info\n")).unwrap();
    assert_eq!(got.log_time, None);
    assert!(load_file(&directory.write_config("log-time: null\n")).is_err());
}

/// Go: `TestValidateEnums`
#[test]
fn validate_enums_cases() {
    assert_eq!(validate_enums("plain", "warn", "opt-in", "flag"), Ok(()));
    assert_eq!(validate_enums("", "", "", "flag"), Ok(()));
    for alias in ["warning", "fatal", "panic"] {
        let error = validate_enums("", alias, "", "flag")
            .unwrap_err()
            .to_string();
        assert!(error.contains(r#"flag "log-level""#), "{alias}: {error}");
    }
}

fn runtime(dry_run: bool) -> Runtime {
    Runtime {
        dry_run,
        policy: Global::new("opt-in", None, None).unwrap(),
    }
}

/// Go: `TestPublisher_GenerationsGrow`
#[test]
fn publisher_generations_grow() {
    let (store, publisher) = new_store(runtime(false));
    assert_eq!(store.generation(), 1);
    let generation = publisher.publish(runtime(true));
    let (current, snapshot_generation) = store.snapshot();
    assert_eq!((generation, snapshot_generation), (2, 2));
    assert!(current.dry_run);
    assert!(store.load().dry_run);
}

/// Nesting is refused at libyaml's depth, without recursing: the loader runs on a thread with a
/// small stack, as a reload on a blocking thread would, and refuses instead of overflowing it.
#[test]
fn deep_nesting_is_refused_without_recursion() {
    let directory = TempDir::new();
    let block = directory.write_config(&format!("device-allow:\n{}x\n", "- ".repeat(50_000)));
    let flow = format!("device-allow: {}\n", "[".repeat(50_000));
    let flow_directory = TempDir::new();
    let flow = flow_directory.write_config(&flow);
    let errors = std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(move || {
            [block, flow].map(|path| load_file(&path).map(|_| ()).unwrap_err().to_string())
        })
        .unwrap()
        .join()
        .unwrap();
    assert!(
        errors[0].contains("yaml: line 2: exceeded max depth of 10000"),
        "{}",
        errors[0]
    );
    assert!(errors[1].starts_with("parse config file "), "{}", errors[1]);
}
