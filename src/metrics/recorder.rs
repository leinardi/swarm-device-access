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

//! The daemon's metrics: every `sda_` family of the 1.x daemon with its name, type, labels and
//! help text, and the methods the daemon records through, on prometheus-client.
//!
//! The recorder owns its [`Registry`], so the metrics server exposes exactly what it was given:
//! the `sda_*` families and service-kit's `process_*` collector. Counters are registered without
//! the `_total` suffix, which prometheus-client appends. A no-op form ([`Recorder::noop`]) stands
//! for Go's nil `*Recorder`, on which every method does nothing.

use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::{Metric, Registry};
use service_kit::process::ProcessCollector;

use super::{CONTENT_TYPE, DEF_BUCKETS, Exposition};

/// The label value of a container handled without a container-level failure.
const RESULT_OK: &str = "ok";
/// The label value of a container-level failure.
const RESULT_ERROR: &str = "error";

/// `sda_events_total`'s labels.
#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct EventLabels {
    event: String,
}

/// `sda_rules_applied_total`'s labels.
#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct ResultLabels {
    result: &'static str,
}

/// The labels of the two skip counters.
#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct ReasonLabels {
    reason: String,
}

/// The daemon's metrics, cheap to clone: every clone records into the same families.
#[derive(Debug, Clone)]
pub struct Recorder {
    families: Option<Arc<Families>>,
}

/// The families of a live recorder and the registry that exposes them.
#[derive(Debug)]
struct Families {
    registry: Registry,
    events: Family<EventLabels, Counter>,
    rules_applied: Family<ResultLabels, Counter>,
    reload_reapplies: Counter,
    docker_reconnects: Counter,
    apply_duration: Histogram,
    containers_scanned: Counter,
    containers_skipped: Family<ReasonLabels, Counter>,
    device_files_discovered: Counter,
    candidates_skipped: Family<ReasonLabels, Counter>,
    rule_failures: Counter,
    dry_run_skips: Counter,
    last_event_timestamp: Gauge,
    pending_containers: Gauge,
    reload_incomplete: Gauge,
}

impl Default for Recorder {
    fn default() -> Self {
        Self::new()
    }
}

impl Recorder {
    /// Go's `NewRecorder`: every family, registered in a registry of its own beside the process
    /// collector. A family with labels has no series until it is recorded (a count of 0 creates
    /// one); the others read 0 from the start.
    #[must_use]
    pub fn new() -> Self {
        let mut registry = Registry::default();
        registry.register_collector(Box::new(ProcessCollector::new()));
        let families = Families {
            events: register(
                &mut registry,
                "sda_events",
                "Total Docker container events received.",
            ),
            rules_applied: register(
                &mut registry,
                "sda_rules_applied",
                "Containers handled, by result: ok = no container-level failure (includes \
                 policy, label and no-pid skips and partial per-device failures), error = \
                 container-level failure.",
            ),
            reload_reapplies: register(
                &mut registry,
                "sda_reload_reapplies",
                "Times device rules were re-applied after a systemd daemon-reload.",
            ),
            docker_reconnects: register(
                &mut registry,
                "sda_docker_reconnects",
                "Times the Docker event stream reconnected after an error.",
            ),
            apply_duration: register_with(
                &mut registry,
                "sda_apply_duration_seconds",
                "Time spent applying device rules per container.",
                Histogram::new(DEF_BUCKETS),
            ),
            containers_scanned: register(
                &mut registry,
                "sda_containers_scanned",
                "Containers that passed policy and entered the device-rule pipeline.",
            ),
            containers_skipped: register(
                &mut registry,
                "sda_containers_skipped",
                "Containers skipped before rule collection.",
            ),
            device_files_discovered: register(
                &mut registry,
                "sda_device_files_discovered",
                "Device files for which rules were collected.",
            ),
            candidates_skipped: register(
                &mut registry,
                "sda_device_candidates_skipped",
                "Names under a /dev mount that name no device (outside_dev, dangling, \
                 not_device).",
            ),
            rule_failures: register(
                &mut registry,
                "sda_rule_failures",
                "Per-device rule failures (entries that passed policy but could not be turned \
                 into a rule, and walk errors).",
            ),
            dry_run_skips: register(
                &mut registry,
                "sda_dry_run_skips",
                "Rules logged but not written due to dry-run mode.",
            ),
            last_event_timestamp: register(
                &mut registry,
                "sda_last_event_timestamp_seconds",
                "Unix timestamp of the last container processed successfully (event, startup or \
                 reload).",
            ),
            pending_containers: register(
                &mut registry,
                "sda_reconcile_pending_containers",
                "Containers whose last reconciliation failed and that are being retried with \
                 backoff.",
            ),
            reload_incomplete: register(
                &mut registry,
                "sda_reload_incomplete",
                "1 until a reconciliation pass for the latest config and trigger has visited \
                 every running container, else 0.",
            ),
            registry,
        };
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

    /// The exposition the metrics server answers a scrape with: the registry as OpenMetrics text
    /// (empty but for its `# EOF` for the no-op recorder).
    #[must_use]
    pub fn exposition(&self) -> Exposition {
        let mut body = String::new();
        // Writing to a String cannot fail.
        let _ = match &self.families {
            Some(families) => {
                prometheus_client::encoding::text::encode(&mut body, &families.registry)
            }
            None => prometheus_client::encoding::text::encode_eof(&mut body),
        };
        Exposition {
            content_type: CONTENT_TYPE,
            body: body.into_bytes(),
        }
    }

    /// Go's `RecordEvent`: counts a Docker event by its action.
    pub fn record_event(&self, action: &str) {
        if let Some(families) = &self.families {
            families
                .events
                .get_or_create(&EventLabels {
                    event: action.to_owned(),
                })
                .inc();
        }
    }

    /// Go's `RecordRuleApplied`: counts a handled container, `ok` on success and `error`
    /// otherwise.
    pub fn record_rule_applied(&self, success: bool) {
        if let Some(families) = &self.families {
            let result = if success { RESULT_OK } else { RESULT_ERROR };
            families
                .rules_applied
                .get_or_create(&ResultLabels { result })
                .inc();
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

    /// Go's `ObserveApplyDuration`: records how long a container's apply took, in seconds.
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
            families
                .containers_skipped
                .get_or_create(&ReasonLabels {
                    reason: reason.to_owned(),
                })
                .inc();
        }
    }

    /// Go's `AddDeviceFilesDiscovered`.
    pub fn add_device_files_discovered(&self, count: usize) {
        if let Some(families) = &self.families {
            families.device_files_discovered.inc_by(count_value(count));
        }
    }

    /// Go's `AddCandidatesSkipped`: `count` names skipped for `reason`; a count of 0 still
    /// creates the series, as `WithLabelValues` does.
    pub fn add_candidates_skipped(&self, reason: &str, count: usize) {
        if let Some(families) = &self.families {
            families
                .candidates_skipped
                .get_or_create(&ReasonLabels {
                    reason: reason.to_owned(),
                })
                .inc_by(count_value(count));
        }
    }

    /// Go's `AddRuleFailures`.
    pub fn add_rule_failures(&self, count: usize) {
        if let Some(families) = &self.families {
            families.rule_failures.inc_by(count_value(count));
        }
    }

    /// Go's `AddDryRunSkips`.
    pub fn add_dry_run_skips(&self, count: usize) {
        if let Some(families) = &self.families {
            families.dry_run_skips.inc_by(count_value(count));
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
            families.last_event_timestamp.set(seconds);
        }
    }

    /// Go's `SetPendingContainers`.
    pub fn set_pending_containers(&self, count: usize) {
        if let Some(families) = &self.families {
            families
                .pending_containers
                .set(i64::try_from(count).unwrap_or(i64::MAX));
        }
    }

    /// Go's `SetReloadIncomplete`: 1 while the latest pass has yet to visit every running
    /// container, else 0.
    pub fn set_reload_incomplete(&self, incomplete: bool) {
        if let Some(families) = &self.families {
            families.reload_incomplete.set(i64::from(incomplete));
        }
    }
}

/// Registers a new metric of the default kind under `name` and returns it.
fn register<M: Default + Clone + Metric>(registry: &mut Registry, name: &str, help: &str) -> M {
    register_with(registry, name, help, M::default())
}

/// Registers `metric` under `name` and returns a handle to it.
fn register_with<M: Clone + Metric>(
    registry: &mut Registry,
    name: &str,
    help: &str,
    metric: M,
) -> M {
    // The encoder ends the help text with a period.
    registry.register(name, help.trim_end_matches('.'), metric.clone());
    metric
}

/// A count as the counters take it.
fn count_value(count: usize) -> u64 {
    u64::try_from(count).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests;
