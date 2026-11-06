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
//! capture through the logger's injectable sink, a bounded `eventually`, and a fake Docker
//! ([`fake_docker`]).

pub(crate) mod fake_docker;

use std::path::Path;
use std::sync::Arc;

use jiff::Timestamp;
use jiff::tz::TimeZone;

use clock::FakeClock;
use go_slog::{Config, Format, Level, LogHandle, Logger, MemorySink};

/// A temporary directory, removed on drop (`TempDir::new("sda-test")`), and a bounded wait:
/// docker-engine's test helpers.
pub(crate) use crate::docker::test_util::{TempDir, eventually};

/// The bytes of `relative` under `tests/testdata`.
pub(crate) fn testdata(relative: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/testdata")
        .join(relative);
    std::fs::read(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

/// A plain-format logger at `level` behind a handle, writing to the returned sink without record
/// times: the Go tests' handler swap, without a process-wide logger to race on.
pub(crate) fn capture_logs(level: Level) -> (LogHandle, MemorySink) {
    let sink = MemorySink::default();
    let logger = Logger::new(
        Config {
            format: Format::Plain,
            level,
            include_time: false,
            plain_style: go_slog::PlainStyle::Raw,
        },
        Box::new(sink.clone()),
        TimeZone::UTC,
        Arc::new(FakeClock::new(Timestamp::UNIX_EPOCH)),
    );
    (LogHandle::new(logger), sink)
}

#[cfg(test)]
mod tests;
