/*
 * Copyright 2026 Roberto Leinardi
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

// Package observability owns Prometheus metrics and optional HTTP servers
// (metrics, health/readiness, pprof) for the swarm-device-access daemon.
package observability

import (
	"time"

	"github.com/prometheus/client_golang/prometheus"
	"github.com/prometheus/client_golang/prometheus/promauto"
)

// Recorder holds Prometheus metric collectors for the daemon. All methods are
// nil-safe so tests can pass nil without triggering Prometheus registration.
type Recorder struct {
	eventsTotal           *prometheus.CounterVec
	rulesApplied          *prometheus.CounterVec
	reloadReapplies       prometheus.Counter
	dockerReconnects      prometheus.Counter
	applyDuration         prometheus.Histogram
	containersScanned     prometheus.Counter
	containersSkipped     *prometheus.CounterVec
	deviceFilesDiscovered prometheus.Counter
	ruleFailures          prometheus.Counter
	dryRunSkips           prometheus.Counter
	lastEventTimestamp    prometheus.Gauge
}

// NewRecorder registers all metric collectors against Prometheus' default
// registry and returns the populated Recorder. Call once at startup.
func NewRecorder() *Recorder {
	return &Recorder{
		eventsTotal: promauto.NewCounterVec(prometheus.CounterOpts{
			Name: "sda_events_total",
			Help: "Total Docker container events received.",
		}, []string{"event"}),

		rulesApplied: promauto.NewCounterVec(prometheus.CounterOpts{
			Name: "sda_rules_applied_total",
			Help: "Total device rules applied (or skipped in dry-run).",
		}, []string{"result"}),

		reloadReapplies: promauto.NewCounter(prometheus.CounterOpts{
			Name: "sda_reload_reapplies_total",
			Help: "Times device rules were re-applied after a systemd daemon-reload.",
		}),

		dockerReconnects: promauto.NewCounter(prometheus.CounterOpts{
			Name: "sda_docker_reconnects_total",
			Help: "Times the Docker event stream reconnected after an error.",
		}),

		applyDuration: promauto.NewHistogram(prometheus.HistogramOpts{
			Name:    "sda_apply_duration_seconds",
			Help:    "Time spent applying device rules per container.",
			Buckets: prometheus.DefBuckets,
		}),

		containersScanned: promauto.NewCounter(prometheus.CounterOpts{
			Name: "sda_containers_scanned_total",
			Help: "Containers that passed policy and entered the device-rule pipeline.",
		}),

		containersSkipped: promauto.NewCounterVec(prometheus.CounterOpts{
			Name: "sda_containers_skipped_total",
			Help: "Containers skipped before rule collection.",
		}, []string{"reason"}),

		deviceFilesDiscovered: promauto.NewCounter(prometheus.CounterOpts{
			Name: "sda_device_files_discovered_total",
			Help: "Device files for which rules were collected.",
		}),

		ruleFailures: promauto.NewCounter(prometheus.CounterOpts{
			Name: "sda_rule_failures_total",
			Help: "Errors during device rule collection or application.",
		}),

		dryRunSkips: promauto.NewCounter(prometheus.CounterOpts{
			Name: "sda_dry_run_skips_total",
			Help: "Rules logged but not written due to dry-run mode.",
		}),

		lastEventTimestamp: promauto.NewGauge(prometheus.GaugeOpts{
			Name: "sda_last_event_timestamp_seconds",
			Help: "Unix timestamp of the last successfully processed Docker event.",
		}),
	}
}

// RecordEvent increments the total events counter for the given action.
func (rec *Recorder) RecordEvent(action string) {
	if rec == nil {
		return
	}

	rec.eventsTotal.WithLabelValues(action).Inc()
}

// RecordRuleApplied increments the rules-applied counter. success=true → "ok", false → "error".
func (rec *Recorder) RecordRuleApplied(success bool) {
	if rec == nil {
		return
	}

	result := "ok"
	if !success {
		result = "error"
	}

	rec.rulesApplied.WithLabelValues(result).Inc()
}

// IncReloadReapply increments the systemd-reload re-apply counter.
func (rec *Recorder) IncReloadReapply() {
	if rec == nil {
		return
	}

	rec.reloadReapplies.Inc()
}

// IncDockerReconnect increments the Docker event-stream reconnect counter.
func (rec *Recorder) IncDockerReconnect() {
	if rec == nil {
		return
	}

	rec.dockerReconnects.Inc()
}

// ObserveApplyDuration records how long a container rule-apply took.
func (rec *Recorder) ObserveApplyDuration(dur time.Duration) {
	if rec == nil {
		return
	}

	rec.applyDuration.Observe(dur.Seconds())
}

// RecordContainerScanned increments the containers-scanned counter.
func (rec *Recorder) RecordContainerScanned() {
	if rec == nil {
		return
	}

	rec.containersScanned.Inc()
}

// RecordContainerSkipped increments the containers-skipped counter for the given reason.
func (rec *Recorder) RecordContainerSkipped(reason string) {
	if rec == nil {
		return
	}

	rec.containersSkipped.WithLabelValues(reason).Inc()
}

// AddDeviceFilesDiscovered adds count to the device-files-discovered counter.
func (rec *Recorder) AddDeviceFilesDiscovered(count int) {
	if rec == nil {
		return
	}

	rec.deviceFilesDiscovered.Add(float64(count))
}

// AddRuleFailures adds count to the rule-failures counter.
func (rec *Recorder) AddRuleFailures(count int) {
	if rec == nil {
		return
	}

	rec.ruleFailures.Add(float64(count))
}

// AddDryRunSkips adds count to the dry-run-skips counter.
func (rec *Recorder) AddDryRunSkips(count int) {
	if rec == nil {
		return
	}

	rec.dryRunSkips.Add(float64(count))
}

// SetLastEvent records the Unix timestamp of the most recently processed event.
func (rec *Recorder) SetLastEvent(eventTime time.Time) {
	if rec == nil {
		return
	}

	rec.lastEventTimestamp.Set(float64(eventTime.Unix()))
}
