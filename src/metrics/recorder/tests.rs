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

//! The recorder's tests: they replace the byte comparison with the Go capture
//! (`TestGoldenRecorderExposition`) by what it was there to prove. The inventory (names, types,
//! label names and help texts) is the committed list `tests/testdata/metrics-inventory.tsv`, taken
//! from that capture; the values are those of the same scripted sequence; the buckets are
//! the Go client's `DefBuckets`; and the exposition holds together while it is recorded into. The
//! no-op form, shared clones and the rounding of the last-event time are checked too.
//!
//! This daemon has no snapshot swapped per poll, so there is no series that disappears from a
//! scrape: its labelled families gain series when recorded (a count of 0 included), and the
//! unlabelled ones read 0 from the start, which the pre-seeding test checks.

use std::collections::BTreeMap;
use std::time::Duration;

use jiff::Timestamp;
use jiff::civil::date;
use jiff::tz::{Offset, TimeZone};

use super::Recorder;
use crate::metrics::CONTENT_TYPE;
use crate::metrics::testing::Scrape;
use crate::test_support::testdata;

/// A sample the sequence must give: its name, its labels and its value.
type Case<'a> = (&'a str, &'a [(&'a str, &'a str)], f64);

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
        .expect("event time")
        .timestamp();
    recorder.set_last_event(event_time);
    recorder.set_pending_containers(4);
    recorder.set_pending_containers(2);
    recorder.set_reload_incomplete(true);
    recorder.set_reload_incomplete(false);
}

fn near(got: Option<f64>, want: f64, what: &str) {
    let got = got.unwrap_or_else(|| panic!("{what}: no such sample"));
    assert!((got - want).abs() < 1e-9, "{what}: got {got}, want {want}");
}

/// Go's nil `*Recorder`: every method is a no-op, and its exposition is empty.
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
    let scrape = Scrape::of(&recorder);
    assert!(scrape.samples.is_empty() && scrape.types.is_empty());
    assert!(scrape.ended);
}

/// The inventory: the `sda_` families are the committed list, no more and no fewer, each with
/// its type, its label names and its help text, and nothing of promhttp's or Go's runtime.
#[test]
fn the_sda_inventory_is_the_golden_s() {
    let recorder = Recorder::new();
    recorder_sequence(&recorder);
    let scrape = Scrape::of(&recorder);
    let mut got = Vec::new();
    for (family, kind) in &scrape.types {
        if !family.starts_with("sda_") {
            continue;
        }
        // OpenMetrics names a counter's family without `_total`; the Go capture used the sample's.
        let name = if kind == "counter" {
            format!("{family}_total")
        } else {
            family.clone()
        };
        let help = scrape.helps[family].clone();
        got.push(format!(
            "{name}\t{kind}\t{}\t{help}",
            scrape.label_names(family)
        ));
    }
    got.sort();
    let want: Vec<String> = String::from_utf8(testdata("metrics-inventory.tsv"))
        .expect("text")
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
        .map(str::to_owned)
        .collect();
    assert_eq!(got, want);
    for family in scrape.types.keys() {
        assert!(
            !family.starts_with("promhttp_") && !family.starts_with("go_"),
            "{family}"
        );
    }
}

/// The `process_*` families come from service-kit's collector.
#[test]
fn the_process_collector_is_registered() {
    let scrape = Scrape::of(&Recorder::new());
    for family in [
        "process_cpu_seconds",
        "process_open_fds",
        "process_resident_memory_bytes",
        "process_start_time_seconds",
        "process_virtual_memory_bytes",
    ] {
        assert!(scrape.types.contains_key(family), "{family}");
    }
}

/// Values: the sequence of every method gives the Go capture's samples exactly.
#[test]
fn the_values_are_the_golden_s() {
    let recorder = Recorder::new();
    recorder_sequence(&recorder);
    let scrape = Scrape::of(&recorder);
    let cases: [Case<'_>; 24] = [
        ("sda_events_total", &[("event", "destroy")], 2.0),
        ("sda_events_total", &[("event", "die")], 1.0),
        ("sda_events_total", &[("event", "start")], 2.0),
        ("sda_events_total", &[("event", "unpause")], 1.0),
        ("sda_rules_applied_total", &[("result", "error")], 1.0),
        ("sda_rules_applied_total", &[("result", "ok")], 3.0),
        ("sda_reload_reapplies_total", &[], 1.0),
        ("sda_docker_reconnects_total", &[], 2.0),
        ("sda_containers_scanned_total", &[], 2.0),
        (
            "sda_containers_skipped_total",
            &[("reason", "invalid_labels")],
            1.0,
        ),
        ("sda_containers_skipped_total", &[("reason", "policy")], 2.0),
        ("sda_device_files_discovered_total", &[], 5.0),
        (
            "sda_device_candidates_skipped_total",
            &[("reason", "dangling")],
            2.0,
        ),
        (
            "sda_device_candidates_skipped_total",
            &[("reason", "not_device")],
            0.0,
        ),
        (
            "sda_device_candidates_skipped_total",
            &[("reason", "outside_dev")],
            1.0,
        ),
        ("sda_rule_failures_total", &[], 1.0),
        ("sda_dry_run_skips_total", &[], 3.0),
        ("sda_last_event_timestamp_seconds", &[], 1_792_933_384.0),
        ("sda_reconcile_pending_containers", &[], 2.0),
        ("sda_reload_incomplete", &[], 0.0),
        ("sda_apply_duration_seconds_count", &[], 9.0),
        (
            "sda_apply_duration_seconds_sum",
            &[],
            16.264_499_999_999_998,
        ),
        ("sda_apply_duration_seconds_bucket", &[("le", "+Inf")], 9.0),
        ("sda_apply_duration_seconds_bucket", &[("le", "10.0")], 8.0),
    ];
    for (name, labels, want) in cases {
        near(
            scrape.value(name, labels),
            want,
            &format!("{name} {labels:?}"),
        );
    }
    // Exactly those series of the daemon: nothing else under `sda_` but the other buckets.
    let others = scrape
        .samples
        .iter()
        .filter(|sample| sample.name.starts_with("sda_") && !sample.name.ends_with("_bucket"))
        .count();
    assert_eq!(others, 22, "{:?}", scrape.samples);
}

/// Buckets: the bounds are the Go client's `DefBuckets`, spelled here independently of the
/// constant, with the golden's cumulative counts; `_count` is the `+Inf` bucket and `_sum` the sum
/// of the observations.
#[test]
fn the_histogram_buckets_are_def_buckets() {
    let recorder = Recorder::new();
    recorder_sequence(&recorder);
    let scrape = Scrape::of(&recorder);
    let buckets: Vec<(f64, f64)> = scrape
        .samples
        .iter()
        .filter(|sample| sample.name == "sda_apply_duration_seconds_bucket")
        .map(|sample| (sample.labels[0].1.parse().expect("a bound"), sample.value))
        .collect();
    let want = [
        (0.005, 4.0),
        (0.01, 5.0),
        (0.025, 5.0),
        (0.05, 5.0),
        (0.1, 5.0),
        (0.25, 6.0),
        (0.5, 6.0),
        (1.0, 7.0),
        (2.5, 7.0),
        (5.0, 8.0),
        (10.0, 8.0),
        (f64::INFINITY, 9.0),
    ];
    assert_eq!(buckets, want);
    assert_eq!(
        crate::metrics::DEF_BUCKETS.map(f64::to_bits),
        want.map(|(bound, _)| bound.to_bits())[..11]
    );
    near(
        scrape.value("sda_apply_duration_seconds_count", &[]),
        9.0,
        "count",
    );
    let sum = 0.001 + 0.005 + 0.007 + 0.25 + 1.0 + 3.0 + 12.0 + 0.0015 + 0.0;
    near(
        scrape.value("sda_apply_duration_seconds_sum", &[]),
        sum,
        "sum",
    );
}

/// A bound is inclusive: an observation equal to it lands in that bucket, one past it in the next.
#[test]
fn observations_land_in_the_right_buckets() {
    let recorder = Recorder::new();
    recorder.observe_apply_duration(Duration::from_millis(10));
    recorder.observe_apply_duration(Duration::from_millis(10) + Duration::from_nanos(1));
    let scrape = Scrape::of(&recorder);
    near(
        scrape.value("sda_apply_duration_seconds_bucket", &[("le", "0.01")]),
        1.0,
        "le 0.01",
    );
    near(
        scrape.value("sda_apply_duration_seconds_bucket", &[("le", "0.025")]),
        2.0,
        "le 0.025",
    );
}

/// Series that exist from the start: the unlabelled families read 0 before anything is
/// recorded, and a labelled one has no series until it is recorded, a count of 0 included.
#[test]
fn unlabelled_families_start_at_zero_and_a_zero_count_creates_the_series() {
    let recorder = Recorder::new();
    let scrape = Scrape::of(&recorder);
    for name in [
        "sda_reload_reapplies_total",
        "sda_docker_reconnects_total",
        "sda_containers_scanned_total",
        "sda_device_files_discovered_total",
        "sda_rule_failures_total",
        "sda_dry_run_skips_total",
        "sda_last_event_timestamp_seconds",
        "sda_reconcile_pending_containers",
        "sda_reload_incomplete",
        "sda_apply_duration_seconds_count",
    ] {
        near(scrape.value(name, &[]), 0.0, name);
    }
    for name in [
        "sda_events_total",
        "sda_rules_applied_total",
        "sda_containers_skipped_total",
        "sda_device_candidates_skipped_total",
    ] {
        assert!(
            scrape.samples.iter().all(|sample| sample.name != name),
            "{name} has a series before it is recorded"
        );
    }
    recorder.add_candidates_skipped("dangling", 0);
    near(
        Scrape::of(&recorder).value(
            "sda_device_candidates_skipped_total",
            &[("reason", "dangling")],
        ),
        0.0,
        "a zero count",
    );
}

/// The exposition is OpenMetrics text: its content type, an `# EOF`, counters with `_total`, no
/// `_created` lines, no promhttp and no Go runtime families.
#[test]
fn the_exposition_is_openmetrics_text() {
    let recorder = Recorder::new();
    recorder_sequence(&recorder);
    let exposition = recorder.exposition();
    assert_eq!(
        exposition.content_type,
        "application/openmetrics-text; version=1.0.0; charset=utf-8"
    );
    assert_eq!(exposition.content_type, CONTENT_TYPE);
    let body = String::from_utf8(exposition.body).expect("text");
    assert!(body.ends_with("# EOF\n"), "{body}");
    assert!(!body.contains("_created"), "{body}");
    assert!(!body.contains("promhttp_"), "{body}");
    assert!(!body.contains("go_goroutines"), "{body}");
    assert!(
        body.contains("sda_events_total{event=\"start\"} 2\n"),
        "{body}"
    );
}

/// Consistency under concurrency: while threads record, repeated scrapes always parse, every
/// histogram is internally consistent (cumulative buckets that never fall, `+Inf` equal to
/// `_count`), counters never fall from one scrape to the next, and the totals at the end are
/// exact.
#[test]
fn scrapes_stay_consistent_while_recording() {
    const THREADS: usize = 4;
    const ROUNDS: usize = 2_000;
    let recorder = Recorder::new();
    let writers: Vec<_> = (0..THREADS)
        .map(|thread| {
            let recorder = recorder.clone();
            std::thread::spawn(move || {
                for round in 0..ROUNDS {
                    recorder.record_event(["start", "die"][round % 2]);
                    recorder.record_rule_applied(round % 3 != 0);
                    recorder.observe_apply_duration(Duration::from_micros(
                        (thread * ROUNDS + round) as u64 % 20_000,
                    ));
                    recorder.add_candidates_skipped("dangling", 1);
                }
            })
        })
        .collect();
    let mut last: BTreeMap<String, f64> = BTreeMap::new();
    let mut scrapes = 0;
    while writers.iter().any(|writer| !writer.is_finished()) || scrapes < 5 {
        let scrape = Scrape::of(&recorder);
        scrapes += 1;
        assert!(scrape.ended);
        let buckets: Vec<f64> = scrape
            .samples
            .iter()
            .filter(|sample| sample.name == "sda_apply_duration_seconds_bucket")
            .map(|sample| sample.value)
            .collect();
        assert!(
            buckets.windows(2).all(|pair| pair[0] <= pair[1]),
            "{buckets:?}"
        );
        let count = scrape
            .value("sda_apply_duration_seconds_count", &[])
            .unwrap();
        assert_eq!(buckets.last().copied(), Some(count), "+Inf is the count");
        for sample in &scrape.samples {
            if sample.name.ends_with("_total") && sample.name.starts_with("sda_") {
                let key = format!("{} {:?}", sample.name, sample.labels);
                let before = last.insert(key.clone(), sample.value).unwrap_or(0.0);
                assert!(
                    sample.value >= before,
                    "{key} fell from {before} to {}",
                    sample.value
                );
            }
        }
    }
    for writer in writers {
        writer.join().expect("a writer panicked");
    }
    let scrape = Scrape::of(&recorder);
    let total = f64::from(u32::try_from(THREADS * ROUNDS).expect("a small total"));
    near(
        scrape.value("sda_events_total", &[("event", "start")]),
        total / 2.0,
        "start",
    );
    near(
        scrape.value("sda_events_total", &[("event", "die")]),
        total / 2.0,
        "die",
    );
    let ok = scrape
        .value("sda_rules_applied_total", &[("result", "ok")])
        .unwrap();
    let error = scrape
        .value("sda_rules_applied_total", &[("result", "error")])
        .unwrap();
    near(Some(ok + error), total, "ok + error");
    near(
        scrape.value(
            "sda_device_candidates_skipped_total",
            &[("reason", "dangling")],
        ),
        total,
        "dangling",
    );
    near(
        scrape.value("sda_apply_duration_seconds_count", &[]),
        total,
        "count",
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
    let scrape = Scrape::of(&recorder);
    near(
        scrape.value("sda_events_total", &[("event", "die")]),
        2.0,
        "events",
    );
    near(
        scrape.value("sda_rules_applied_total", &[("result", "ok")]),
        1.0,
        "ok",
    );
    near(
        scrape.value("sda_rules_applied_total", &[("result", "error")]),
        1.0,
        "error",
    );
    near(
        scrape.value("sda_containers_skipped_total", &[("reason", "policy")]),
        1.0,
        "skipped",
    );
    near(
        scrape.value(
            "sda_device_candidates_skipped_total",
            &[("reason", "not_device")],
        ),
        0.0,
        "candidates",
    );
    near(
        scrape.value("sda_device_files_discovered_total", &[]),
        3.0,
        "files",
    );
    near(
        scrape.value("sda_reconcile_pending_containers", &[]),
        7.0,
        "pending",
    );
    near(
        scrape.value("sda_reload_incomplete", &[]),
        1.0,
        "incomplete",
    );
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
        near(
            Scrape::of(&recorder).value("sda_last_event_timestamp_seconds", &[]),
            want,
            time,
        );
    }
}
