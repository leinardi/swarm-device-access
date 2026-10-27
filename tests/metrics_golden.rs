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

//! The `Recorder` against the Go capture `tests/testdata/metrics/recorder.prom` and
//! `recorder.headers` (`TestGoldenRecorderExposition` in
//! `internal/observability/golden_capture_test.go`): the same scripted sequence of every method,
//! then the answer to a `/metrics` request without `Accept` or `Accept-Encoding`, whose `sda_`
//! lines and `Content-Type` must match Go's byte for byte.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use jiff::civil::date;
use jiff::tz::{Offset, TimeZone};
use swarm_device_access::metrics::{Recorder, exposition};

fn testdata(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/testdata/metrics")
        .join(name);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

/// Go's `recorderSequence`.
fn recorder_sequence(recorder: &Recorder) {
    for action in ["start", "start", "unpause", "die", "destroy", "destroy"] {
        recorder.record_event(action);
    }
    recorder.record_rule_applied(true);
    recorder.record_rule_applied(true);
    recorder.record_rule_applied(true);
    recorder.record_rule_applied(false);
    recorder.inc_reload_reapply();
    recorder.inc_docker_reconnect();
    recorder.inc_docker_reconnect();
    for duration in [
        Duration::from_millis(1),
        Duration::from_millis(5),
        Duration::from_millis(7),
        Duration::from_millis(250),
        Duration::from_secs(1),
        Duration::from_secs(3),
        Duration::from_secs(12),
        Duration::from_micros(1500),
        Duration::ZERO,
    ] {
        recorder.observe_apply_duration(duration);
    }
    recorder.record_container_scanned();
    recorder.record_container_scanned();
    recorder.record_container_skipped("policy");
    recorder.record_container_skipped("invalid_labels");
    recorder.record_container_skipped("policy");
    recorder.add_device_files_discovered(5);
    recorder.add_candidates_skipped("dangling", 2);
    recorder.add_candidates_skipped("outside_dev", 1);
    recorder.add_candidates_skipped("not_device", 0);
    recorder.add_rule_failures(1);
    recorder.add_dry_run_skips(3);
    let cet = TimeZone::fixed(Offset::constant(1));
    let event_time = date(2026, 10, 25)
        .at(14, 3, 4, 567_891_234)
        .to_zoned(cet)
        .unwrap_or_else(|error| panic!("event time: {error}"))
        .timestamp();
    recorder.set_last_event(event_time);
    recorder.set_pending_containers(4);
    recorder.set_pending_containers(2);
    recorder.set_reload_incomplete(true);
    recorder.set_reload_incomplete(false);
}

#[test]
fn recorder_exposition_matches_go() {
    let recorder = Recorder::new();
    recorder_sequence(&recorder);
    let registry = recorder
        .registry()
        .unwrap_or_else(|| panic!("a live recorder has a registry"));
    let answer = exposition::render(&registry.gather(), None, &[], None);
    assert_eq!(answer.content_encoding, None);
    let body = String::from_utf8(answer.body).unwrap_or_else(|error| panic!("body: {error}"));
    let sda: String = body
        .split_inclusive('\n')
        .filter(|line| {
            line.starts_with("sda_")
                || line.starts_with("# HELP sda_")
                || line.starts_with("# TYPE sda_")
        })
        .collect();
    assert_eq!(sda, testdata("recorder.prom"));
    assert_eq!(
        format!("Content-Type: {}\n", answer.content_type),
        testdata("recorder.headers")
    );
}
