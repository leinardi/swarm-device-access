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

//! The Go package has no unit test of `metrics.go`: its behaviour is pinned by
//! `TestGoldenRecorderExposition` (`tests/metrics_golden.rs` here) and by the Go tests that pass a
//! nil `*Recorder`. These cover what the golden cannot: the no-op form, the family set, shared
//! clones and the rounding of the last-event time.

use std::time::Duration;

use jiff::Timestamp;

use super::Recorder;
use crate::metrics::{Gathered, Value};

fn gather(recorder: &Recorder) -> Vec<Gathered> {
    recorder.registry().expect("a live recorder").gather()
}

/// The value of the series of `name` with `labels`, if gathered.
fn value(recorder: &Recorder, name: &str, labels: &[(&str, &str)]) -> Option<Value> {
    gather(recorder)
        .into_iter()
        .find(|family| family.name == name)?
        .series
        .into_iter()
        .find(|series| {
            series
                .labels
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str()))
                .eq(labels.iter().copied())
        })
        .map(|series| series.value)
}

fn scalar(recorder: &Recorder, name: &str, labels: &[(&str, &str)]) -> f64 {
    match value(recorder, name, labels) {
        Some(Value::Scalar(value)) => value,
        other @ (Some(Value::Histogram { .. }) | None) => {
            panic!("{name} {labels:?}: {other:?}")
        }
    }
}

/// Go's nil `*Recorder`: every method is a no-op, and there is nothing to serve.
#[test]
fn noop_records_nothing() {
    let recorder = Recorder::noop();
    recorder.record_event("start");
    recorder.record_rule_applied(false);
    recorder.inc_reload_reapply();
    recorder.inc_docker_reconnect();
    recorder.observe_apply_duration(Duration::from_secs(1));
    recorder.record_container_scanned();
    recorder.record_container_skipped("policy");
    recorder.add_device_files_discovered(1);
    recorder.add_candidates_skipped("dangling", 1);
    recorder.add_rule_failures(1);
    recorder.add_dry_run_skips(1);
    recorder.set_last_event(Timestamp::UNIX_EPOCH);
    recorder.set_pending_containers(1);
    recorder.set_reload_incomplete(true);
    assert!(recorder.registry().is_none());
    assert!(recorder.handler_metrics().is_none());
}

/// The families Go's default registry serves beside the `go_*` ones (Q3): the daemon's, the
/// process collector's and promhttp's.
#[test]
fn registers_the_go_family_set() {
    let recorder = Recorder::new();
    let names: Vec<String> = recorder
        .registry()
        .expect("a live recorder")
        .described_names()
        .into_iter()
        .collect();
    assert_eq!(
        names,
        [
            "process_cpu_seconds_total",
            "process_max_fds",
            "process_network_receive_bytes_total",
            "process_network_transmit_bytes_total",
            "process_open_fds",
            "process_resident_memory_bytes",
            "process_start_time_seconds",
            "process_virtual_memory_bytes",
            "process_virtual_memory_max_bytes",
            "promhttp_metric_handler_requests_in_flight",
            "promhttp_metric_handler_requests_total",
            "sda_apply_duration_seconds",
            "sda_containers_scanned_total",
            "sda_containers_skipped_total",
            "sda_device_candidates_skipped_total",
            "sda_device_files_discovered_total",
            "sda_docker_reconnects_total",
            "sda_dry_run_skips_total",
            "sda_events_total",
            "sda_last_event_timestamp_seconds",
            "sda_reconcile_pending_containers",
            "sda_reload_incomplete",
            "sda_reload_reapplies_total",
            "sda_rule_failures_total",
            "sda_rules_applied_total",
        ]
    );
}

/// Every clone records into the same families, and each method into its own.
#[test]
fn clones_share_the_families() {
    let recorder = Recorder::new();
    let clone = recorder.clone();
    clone.record_event("die");
    recorder.record_event("die");
    clone.record_rule_applied(true);
    clone.record_rule_applied(false);
    clone.record_container_skipped("policy");
    clone.add_candidates_skipped("not_device", 0);
    clone.add_device_files_discovered(3);
    clone.set_pending_containers(7);
    clone.set_reload_incomplete(true);
    assert!((scalar(&recorder, "sda_events_total", &[("event", "die")]) - 2.0).abs() < 1e-9);
    assert!((scalar(&recorder, "sda_rules_applied_total", &[("result", "ok")]) - 1.0).abs() < 1e-9);
    assert!(
        (scalar(&recorder, "sda_rules_applied_total", &[("result", "error")]) - 1.0).abs() < 1e-9
    );
    assert!(
        (scalar(
            &recorder,
            "sda_containers_skipped_total",
            &[("reason", "policy")]
        ) - 1.0)
            .abs()
            < 1e-9
    );
    assert!(
        scalar(
            &recorder,
            "sda_device_candidates_skipped_total",
            &[("reason", "not_device")]
        )
        .abs()
            < 1e-9
    );
    assert!((scalar(&recorder, "sda_device_files_discovered_total", &[]) - 3.0).abs() < 1e-9);
    assert!((scalar(&recorder, "sda_reconcile_pending_containers", &[]) - 7.0).abs() < 1e-9);
    assert!((scalar(&recorder, "sda_reload_incomplete", &[]) - 1.0).abs() < 1e-9);
}

/// `Time.Unix` rounds down: a time before the epoch with a fraction is the earlier second, where
/// jiff's whole seconds truncate toward zero.
#[test]
fn last_event_rounds_down_as_go() {
    let recorder = Recorder::new();
    for (time, want) in [
        ("2026-10-25T13:03:04.999999999Z", 1_792_933_384.0),
        ("1969-12-31T23:59:58.5Z", -2.0),
        ("1970-01-01T00:00:00Z", 0.0),
    ] {
        recorder.set_last_event(time.parse().expect("timestamp"));
        assert!(
            (scalar(&recorder, "sda_last_event_timestamp_seconds", &[]) - want).abs() < 1e-9,
            "{time}"
        );
    }
}
