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

// Adapted from swarm-scheduler-exporter/src/test_support/mod.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! Helpers the unit tests share: a temporary directory, the files under `tests/testdata`, log
//! capture, a bounded `eventually`, and a fake Docker ([`fake_docker`]).

pub(crate) mod fake_docker;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::Value;
use tracing::dispatcher::DefaultGuard;
use tracing::level_filters::LevelFilter;
use tracing_setup::testing::{Capture, set_default};
use tracing_setup::{Format, Options};

/// A temporary directory, removed with what it holds on drop (`TempDir::new("sda-test")`).
#[derive(Debug)]
pub(crate) struct TempDir(PathBuf);

impl TempDir {
    /// A new, empty directory under the system's temporary directory, named after `prefix`, the
    /// process and a counter.
    ///
    /// # Panics
    ///
    /// When the directory cannot be created.
    #[must_use]
    #[track_caller]
    pub(crate) fn new(prefix: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&path).unwrap_or_else(|error| {
            panic!("create {}: {error}", path.display());
        });
        Self(path)
    }

    /// The directory.
    #[must_use]
    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// How often [`eventually`] checks its condition.
const EVENTUALLY_INTERVAL: Duration = Duration::from_millis(10);

/// Waits until `condition` holds, checking every 10ms of tokio time, and fails the test once
/// `within` has passed: a wait for something that never happens fails instead of hanging, also
/// under a paused clock that keeps advancing.
///
/// # Panics
///
/// When `within` passes first, naming `what`.
pub(crate) async fn eventually(what: &str, within: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + within;
    while !condition() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out after {within:?} waiting for {what}"
        );
        tokio::time::sleep(EVENTUALLY_INTERVAL).await;
    }
}

/// The bytes of `relative` under `tests/testdata`.
pub(crate) fn testdata(relative: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/testdata")
        .join(relative);
    std::fs::read(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

/// What a test logged: the records of the subscriber [`capture_logs`] made this thread's default,
/// kept as the JSON objects tracing-setup wrote. Dropping it restores the previous default.
pub(crate) struct Logs {
    capture: Capture,
    _scope: DefaultGuard,
}

/// Makes a subscriber that keeps the events at `level` and above, without the timestamp, this
/// thread's default for as long as the result lives: a test on a current-thread runtime sees what
/// the tasks it spawns log, and a blocking transaction logs where its caller does
/// ([`crate::logging::inherit`]).
pub(crate) fn capture_logs(level: LevelFilter) -> Logs {
    let capture = Capture::default();
    let dispatch = tracing_setup::build(
        capture.clone(),
        Format::Json,
        level,
        Options::new().target(false).span_fields(false).time(false),
    );
    Logs {
        _scope: set_default(&dispatch),
        capture,
    }
}

impl Logs {
    /// Every record so far, as the flat JSON object it was written as: `level`, `message` and
    /// one key per field, in the order the event named them.
    pub(crate) fn records(&self) -> Vec<Value> {
        self.capture.json_lines()
    }

    /// The records so far, one per line, as `LEVEL message key=value key=value`: the fields in
    /// the order the event named them, each value as text without quotes, for assertions on a
    /// message and its fields.
    pub(crate) fn contents(&self) -> String {
        self.records()
            .iter()
            .fold(String::new(), |mut lines, record| {
                let _ = writeln!(lines, "{}", render(record));
                lines
            })
    }
}

/// One record as `LEVEL message key=value ...`.
pub(crate) fn render(record: &Value) -> String {
    let Some(fields) = record.as_object() else {
        panic!("a log record is a JSON object: {record}");
    };
    let text = |value: &Value| {
        value
            .as_str()
            .map_or_else(|| value.to_string(), str::to_owned)
    };
    let mut line = format!(
        "level={} {}",
        fields.get("level").map_or_else(String::new, text),
        fields.get("message").map_or_else(String::new, text),
    );
    for (key, value) in fields {
        if !matches!(key.as_str(), "level" | "message") {
            let _ = write!(line, " {key}={}", text(value));
        }
    }
    line
}

#[cfg(test)]
mod tests;
