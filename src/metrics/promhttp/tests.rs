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

// Adapted from swarm-scheduler-exporter/src/metrics/promhttp/tests.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use super::{HandlerMetrics, sanitize_code};
use crate::metrics::process::ProcessCollector;
use crate::metrics::{Registry, text};

/// The family set, types and help texts match the Go capture byte for byte
/// (`tests/testdata/metrics/runtime.prom`, from `TestGoldenRuntimeFamilies`), and so do the
/// handler samples after the same four scrapes, seen from inside the fifth; the `go_*` families
/// are not served (Q3).
#[test]
fn runtime_families_match_the_go_capture() {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/testdata/metrics/runtime.prom");
    let want = fs::read_to_string(&path).expect("read runtime.prom");
    let registry = Registry::new();
    registry
        .register(Arc::new(ProcessCollector::new()))
        .unwrap();
    let handler = HandlerMetrics::new();
    handler.register(&registry).unwrap();
    for _ in 0..4 {
        handler.begin().finish(200);
    }
    let _scrape = handler.begin();
    let mut out = String::new();
    text::encode(&mut out, &registry.gather());
    let mut kept = String::new();
    for line in out
        .lines()
        .filter(|line| line.starts_with('#') || line.starts_with("promhttp_"))
    {
        kept.push_str(line);
        kept.push('\n');
    }
    assert_eq!(kept, want);
}

/// The guard counts under the sanitised code on finish, and only lowers the gauge when dropped
/// unfinished (a handler that panicked, which Go does not count either).
#[test]
fn in_flight_guard_counts_on_finish_only() {
    let handler = HandlerMetrics::new();
    let first = handler.begin();
    let second = handler.begin();
    assert!((handler.in_flight.get() - 2.0).abs() < f64::EPSILON);
    first.finish(503);
    drop(second);
    assert!(handler.in_flight.get().abs() < f64::EPSILON);
    assert_eq!(handler.requests.get(&["503"]), Some(1.0));
    assert_eq!(handler.requests.get(&["200"]), Some(0.0));
    handler.begin().finish(0);
    handler.begin().finish(404);
    handler.begin().finish(600);
    assert_eq!(handler.requests.get(&["200"]), Some(1.0));
    assert_eq!(handler.requests.get(&["404"]), Some(1.0));
    assert_eq!(handler.requests.get(&["unknown"]), Some(1.0));
}

/// promhttp's `sanitizeCode`.
#[test]
fn sanitize_code_follows_promhttp() {
    for (status, want) in [
        (0, "200"),
        (99, "unknown"),
        (100, "100"),
        (200, "200"),
        (307, "307"),
        (431, "431"),
        (599, "599"),
        (600, "unknown"),
        (999, "unknown"),
    ] {
        assert_eq!(sanitize_code(status), want, "{status}");
    }
}

/// Registering twice is refused, as `prometheus.Register` would (promhttp reuses the existing
/// collectors there; the daemon registers once, so it never reaches that path).
#[test]
fn registering_twice_is_refused() {
    let registry = Registry::new();
    let handler = HandlerMetrics::new();
    handler.register(&registry).unwrap();
    assert!(handler.register(&registry).is_err());
}
