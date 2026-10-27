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

//! The daemon's metrics, a port of `internal/observability/metrics.go`: every `sda_` family with
//! Go's name, type, labels and help text, and the methods the daemon records through.
//!
//! The Go `Recorder` registers against the Prometheus Go client's default registry, which also
//! holds the process collector, and promhttp adds its handler metrics there. This one owns its
//! [`Registry`] with the same families (but `go_*`, Q3), so the metrics server serves exactly
//! what it was given. A no-op form ([`Recorder::noop`]) stands for Go's nil `*Recorder`, on which
//! every method does nothing.

use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;

use crate::go_compat::float;

use super::process::ProcessCollector;
use super::promhttp::HandlerMetrics;
use super::{
    Collector, Counter, CounterVec, DEF_BUCKETS, Gauge, Histogram, Kind, Registry, constant_desc,
};

/// `sda_events_total`'s label.
const EVENT_LABEL: &str = "event";
/// `sda_rules_applied_total`'s label.
const RESULT_LABEL: &str = "result";
/// The label of the two skip counters.
const REASON_LABEL: &str = "reason";

/// The label value of a container handled without a container-level failure.
const RESULT_OK: &str = "ok";
/// The label value of a container-level failure.
const RESULT_ERROR: &str = "error";

/// The daemon's metrics, cheap to clone: every clone records into the same families.
#[derive(Debug, Clone)]
pub struct Recorder {
    families: Option<Arc<Families>>,
}

/// The families of a live recorder and the registry that serves them.
#[derive(Debug)]
struct Families {
    registry: Registry,
    handler: HandlerMetrics,
    events_total: Arc<CounterVec>,
    rules_applied: Arc<CounterVec>,
    reload_reapplies: Arc<Counter>,
    docker_reconnects: Arc<Counter>,
    apply_duration: Arc<Histogram>,
    containers_scanned: Arc<Counter>,
    containers_skipped: Arc<CounterVec>,
    device_files_discovered: Arc<Counter>,
    candidates_skipped: Arc<CounterVec>,
    rule_failures: Arc<Counter>,
    dry_run_skips: Arc<Counter>,
    last_event_timestamp: Arc<Gauge>,
    pending_containers: Arc<Gauge>,
    reload_incomplete: Arc<Gauge>,
}

impl Default for Recorder {
    fn default() -> Self {
        Self::new()
    }
}

impl Recorder {
    /// Go's `NewRecorder`: every family, registered in a registry of its own beside the process
    /// collector and promhttp's handler metrics.
    ///
    /// # Panics
    ///
    /// Never: the names, help texts and labels are constants, distinct and valid, which the
    /// tests check by building one.
    #[must_use]
    pub fn new() -> Self {
        let families = Families {
            registry: Registry::new(),
            handler: HandlerMetrics::new(),
            events_total: counter_vec(
                "sda_events_total",
                "Total Docker container events received.",
                EVENT_LABEL,
            ),
            rules_applied: counter_vec(
                "sda_rules_applied_total",
                "Containers handled, by result: ok = no container-level failure (includes \
                 policy, label and no-pid skips and partial per-device failures), error = \
                 container-level failure.",
                RESULT_LABEL,
            ),
            reload_reapplies: counter(
                "sda_reload_reapplies_total",
                "Times device rules were re-applied after a systemd daemon-reload.",
            ),
            docker_reconnects: counter(
                "sda_docker_reconnects_total",
                "Times the Docker event stream reconnected after an error.",
            ),
            apply_duration: Arc::new(Histogram::new(
                constant_desc(
                    "sda_apply_duration_seconds",
                    "Time spent applying device rules per container.",
                    Kind::Histogram,
                    &[],
                ),
                &DEF_BUCKETS,
            )),
            containers_scanned: counter(
                "sda_containers_scanned_total",
                "Containers that passed policy and entered the device-rule pipeline.",
            ),
            containers_skipped: counter_vec(
                "sda_containers_skipped_total",
                "Containers skipped before rule collection.",
                REASON_LABEL,
            ),
            device_files_discovered: counter(
                "sda_device_files_discovered_total",
                "Device files for which rules were collected.",
            ),
            candidates_skipped: counter_vec(
                "sda_device_candidates_skipped_total",
                "Names under a /dev mount that name no device (outside_dev, dangling, \
                 not_device).",
                REASON_LABEL,
            ),
            rule_failures: counter(
                "sda_rule_failures_total",
                "Per-device rule failures (entries that passed policy but could not be turned \
                 into a rule, and walk errors).",
            ),
            dry_run_skips: counter(
                "sda_dry_run_skips_total",
                "Rules logged but not written due to dry-run mode.",
            ),
            last_event_timestamp: gauge(
                "sda_last_event_timestamp_seconds",
                "Unix timestamp of the last container processed successfully (event, startup or \
                 reload).",
            ),
            pending_containers: gauge(
                "sda_reconcile_pending_containers",
                "Containers whose last reconciliation failed and that are being retried with \
                 backoff.",
            ),
            reload_incomplete: gauge(
                "sda_reload_incomplete",
                "1 until a reconciliation pass for the latest config and trigger has visited \
                 every running container, else 0.",
            ),
        };
        families.register();
        Self {
            families: Some(Arc::new(families)),
        }
    }

    /// The recorder that records nothing, Go's nil `*Recorder`: for tests and for the code paths
    /// that run without metrics.
    #[must_use]
    pub fn noop() -> Self {
        Self { families: None }
    }

    /// The registry the metrics server gathers, or none for the no-op recorder.
    #[must_use]
    pub fn registry(&self) -> Option<&Registry> {
        self.families.as_deref().map(|families| &families.registry)
    }

    /// promhttp's handler metrics, which the metrics server counts its scrapes in, or none for
    /// the no-op recorder.
    #[must_use]
    pub fn handler_metrics(&self) -> Option<&HandlerMetrics> {
        self.families.as_deref().map(|families| &families.handler)
    }

    /// Go's `RecordEvent`: counts a Docker event by its action.
    pub fn record_event(&self, action: &str) {
        if let Some(families) = &self.families {
            inc(&families.events_total, action);
        }
    }

    /// Go's `RecordRuleApplied`: counts a handled container, `ok` on success and `error`
    /// otherwise.
    pub fn record_rule_applied(&self, success: bool) {
        if let Some(families) = &self.families {
            let result = if success { RESULT_OK } else { RESULT_ERROR };
            inc(&families.rules_applied, result);
        }
    }

    /// Go's `IncReloadReapply`: counts a re-apply after a systemd daemon-reload.
    pub fn inc_reload_reapply(&self) {
        if let Some(families) = &self.families {
            families.reload_reapplies.inc();
        }
    }

    /// Go's `IncDockerReconnect`: counts a reconnect of the Docker event stream.
    pub fn inc_docker_reconnect(&self) {
        if let Some(families) = &self.families {
            families.docker_reconnects.inc();
        }
    }

    /// Go's `ObserveApplyDuration`: records how long a container's apply took, in seconds as
    /// Go's `Duration.Seconds` computes them.
    pub fn observe_apply_duration(&self, duration: Duration) {
        if let Some(families) = &self.families {
            families.apply_duration.observe(duration.as_secs_f64());
        }
    }

    /// Go's `RecordContainerScanned`: counts a container that entered the device-rule pipeline.
    pub fn record_container_scanned(&self) {
        if let Some(families) = &self.families {
            families.containers_scanned.inc();
        }
    }

    /// Go's `RecordContainerSkipped`: counts a container skipped before rule collection.
    pub fn record_container_skipped(&self, reason: &str) {
        if let Some(families) = &self.families {
            inc(&families.containers_skipped, reason);
        }
    }

    /// Go's `AddDeviceFilesDiscovered`.
    pub fn add_device_files_discovered(&self, count: usize) {
        if let Some(families) = &self.families {
            families.device_files_discovered.add(count_value(count));
        }
    }

    /// Go's `AddCandidatesSkipped`: `count` names skipped for `reason`; a count of 0 still
    /// creates the series, as `WithLabelValues` does.
    pub fn add_candidates_skipped(&self, reason: &str, count: usize) {
        if let Some(families) = &self.families {
            // One label value for one label name cannot mismatch.
            let _ = families
                .candidates_skipped
                .add(&[reason], count_value(count));
        }
    }

    /// Go's `AddRuleFailures`.
    pub fn add_rule_failures(&self, count: usize) {
        if let Some(families) = &self.families {
            families.rule_failures.add(count_value(count));
        }
    }

    /// Go's `AddDryRunSkips`.
    pub fn add_dry_run_skips(&self, count: usize) {
        if let Some(families) = &self.families {
            families.dry_run_skips.add(count_value(count));
        }
    }

    /// Go's `SetLastEvent`: the Unix time of `event_time` in whole seconds, rounded down as Go's
    /// `Time.Unix` does.
    pub fn set_last_event(&self, event_time: Timestamp) {
        if let Some(families) = &self.families {
            let mut seconds = event_time.as_second();
            // jiff truncates toward zero; Go floors.
            if event_time.subsec_nanosecond() < 0 {
                seconds -= 1;
            }
            families.last_event_timestamp.set(float::from_int(seconds));
        }
    }

    /// Go's `SetPendingContainers`.
    pub fn set_pending_containers(&self, count: usize) {
        if let Some(families) = &self.families {
            families.pending_containers.set(count_value(count));
        }
    }

    /// Go's `SetReloadIncomplete`: 1 while the latest pass has yet to visit every running
    /// container, else 0.
    pub fn set_reload_incomplete(&self, incomplete: bool) {
        if let Some(families) = &self.families {
            families
                .reload_incomplete
                .set(if incomplete { 1.0 } else { 0.0 });
        }
    }
}

impl Families {
    /// Registers every family, the process collector and the handler metrics.
    fn register(&self) {
        let collectors: [Arc<dyn Collector>; 15] = [
            Arc::clone(&self.events_total) as Arc<dyn Collector>,
            Arc::clone(&self.rules_applied) as Arc<dyn Collector>,
            Arc::clone(&self.reload_reapplies) as Arc<dyn Collector>,
            Arc::clone(&self.docker_reconnects) as Arc<dyn Collector>,
            Arc::clone(&self.apply_duration) as Arc<dyn Collector>,
            Arc::clone(&self.containers_scanned) as Arc<dyn Collector>,
            Arc::clone(&self.containers_skipped) as Arc<dyn Collector>,
            Arc::clone(&self.device_files_discovered) as Arc<dyn Collector>,
            Arc::clone(&self.candidates_skipped) as Arc<dyn Collector>,
            Arc::clone(&self.rule_failures) as Arc<dyn Collector>,
            Arc::clone(&self.dry_run_skips) as Arc<dyn Collector>,
            Arc::clone(&self.last_event_timestamp) as Arc<dyn Collector>,
            Arc::clone(&self.pending_containers) as Arc<dyn Collector>,
            Arc::clone(&self.reload_incomplete) as Arc<dyn Collector>,
            Arc::new(ProcessCollector::new()),
        ];
        for collector in collectors {
            self.registry
                .register(collector)
                .unwrap_or_else(|error| panic!("constant family refused: {error}"));
        }
        self.handler
            .register(&self.registry)
            .unwrap_or_else(|error| panic!("constant family refused: {error}"));
    }
}

fn counter(name: &str, help: &str) -> Arc<Counter> {
    Arc::new(Counter::new(constant_desc(name, help, Kind::Counter, &[])))
}

fn counter_vec(name: &str, help: &str, label: &str) -> Arc<CounterVec> {
    Arc::new(CounterVec::new(constant_desc(
        name,
        help,
        Kind::Counter,
        &[label],
    )))
}

fn gauge(name: &str, help: &str) -> Arc<Gauge> {
    Arc::new(Gauge::new(constant_desc(name, help, Kind::Gauge, &[])))
}

/// Adds 1 to the series of `value`.
fn inc(vec: &CounterVec, value: &str) {
    // One label value for one label name cannot mismatch.
    let _ = vec.inc(&[value]);
}

/// Go's `float64(count)` of an `int` count.
fn count_value(count: usize) -> f64 {
    float::from_int(u64::try_from(count).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests;
