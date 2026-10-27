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
//! capture through the logger's injectable sink, a bounded [`eventually`], and a fake Docker
//! ([`fake_docker`]).

pub(crate) mod fake_docker;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use jiff::Timestamp;
use jiff::tz::TimeZone;

use crate::clock::FakeClock;
use crate::logger::{Config, Format, Level, LogHandle, Logger, MemorySink};

/// A temporary directory, removed on drop. Moved out of SSE's `src/docker/tests.rs` at the same
/// commit.
#[derive(Debug)]
pub(crate) struct TempDir(PathBuf);

impl TempDir {
    /// A new, empty directory under the system's temporary directory.
    pub(crate) fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "sda-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The bytes of `relative` under `tests/testdata`.
pub(crate) fn testdata(relative: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/testdata")
        .join(relative);
    std::fs::read(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

/// How often [`eventually`] checks its condition.
const EVENTUALLY_INTERVAL: Duration = Duration::from_millis(10);

/// A plain-format logger at `level` behind a handle, writing to the returned sink without record
/// times: the Go tests' handler swap, without a process-wide logger to race on.
pub(crate) fn capture_logs(level: Level) -> (LogHandle, MemorySink) {
    let sink = MemorySink::default();
    let logger = Logger::new(
        Config {
            format: Format::Plain,
            level,
            include_time: false,
        },
        Box::new(sink.clone()),
        TimeZone::UTC,
        Arc::new(FakeClock::new(Timestamp::UNIX_EPOCH)),
    );
    (LogHandle::new(logger), sink)
}

/// Waits until `condition` holds, checking every 10ms of tokio time, and fails the test once
/// `within` has passed: a wait for something that never happens fails instead of hanging, also
/// under a paused clock that keeps advancing.
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

#[cfg(test)]
mod tests;
