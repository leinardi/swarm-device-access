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

//! What the coordinator and event-loop tests share: the fake reconciler (Go's `recordingApply`
//! with the terminate and sweep the Go tests set on the coordinator), a coordinator harness over
//! it and the fake Docker, and the event and metrics helpers.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use jiff::Timestamp;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::{Coordinator, Reconciler, Settings};
use crate::clock::{Clock, TokioClock};
use crate::config::{Runtime, new_store};
use crate::daemon::DOCKER_CALL_TIMEOUT;
use crate::docker::types::{Actor, ContainerInspect, Event};
use crate::docker::{self, CallContext};
use crate::logger::{Level, LogHandle, MemorySink};
use crate::metrics::{Recorder, Value};
use crate::policy::Global;
use crate::processor::{self, Inspector, Processor};
use crate::test_support::capture_logs;
use crate::test_support::fake_docker::FakeDocker;

/// How long a wait for something the coordinator will do may take, in paused tokio time.
pub(crate) const WITHIN: Duration = Duration::from_secs(300);

/// The start of the tests' clock.
pub(crate) fn base() -> Timestamp {
    Timestamp::from_second(1_700_000_000).unwrap()
}

/// The errors of the fakes: Go's `errApplyFailed`, and a container Docker no longer knows.
#[derive(Debug)]
pub(crate) enum FakeError {
    Failed,
    Gone,
}

impl fmt::Display for FakeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Failed => formatter.write_str("apply failed"),
            Self::Gone => formatter.write_str("container \"c1\": container no longer exists"),
        }
    }
}

/// Holds a call until the test opens it, after telling the test it was entered.
#[derive(Default)]
pub(crate) struct Gate {
    pub(crate) entered: CancellationToken,
    pub(crate) open: CancellationToken,
}

impl Gate {
    pub(crate) async fn pass(&self) {
        self.entered.cancel();
        self.open.cancelled().await;
    }
}

/// Go's `recordingApply`, with the terminate and sweep the Go tests set on the coordinator.
#[derive(Default)]
pub(crate) struct FakeReconciler {
    pub(crate) ids: Mutex<Vec<String>>,
    /// Remaining failures per container.
    pub(crate) fail: Mutex<BTreeMap<String, usize>>,
    /// Holds the first reconcile.
    pub(crate) gate: Option<Gate>,
    pub(crate) terminated: Mutex<Vec<String>>,
    /// Holds every termination.
    pub(crate) terminate_gate: Option<Gate>,
    /// What each sweep reports released.
    pub(crate) released: usize,
    pub(crate) sweeps: AtomicUsize,
    /// Cancelled once this many reconciles have been made: Go's `cancelAfter`, so a test that
    /// expects that many ends as soon as the last one is observed and never earlier.
    pub(crate) cancel_after: Option<(usize, CallContext)>,
}

impl FakeReconciler {
    pub(crate) fn gated() -> Self {
        Self {
            gate: Some(Gate::default()),
            ..Self::default()
        }
    }

    pub(crate) fn failing(container_id: &str, times: usize) -> Self {
        let reconciler = Self::default();
        reconciler.set_failures(container_id, times);
        reconciler
    }

    pub(crate) fn set_failures(&self, container_id: &str, times: usize) {
        self.fail
            .lock()
            .unwrap()
            .insert(container_id.to_owned(), times);
    }

    pub(crate) fn seen(&self) -> Vec<String> {
        self.ids.lock().unwrap().clone()
    }

    pub(crate) fn terminated(&self) -> Vec<String> {
        self.terminated.lock().unwrap().clone()
    }

    pub(crate) fn gate(&self) -> &Gate {
        self.gate.as_ref().unwrap()
    }
}

impl Reconciler for Arc<FakeReconciler> {
    type Error = FakeError;

    async fn reconcile(&self, _context: &CallContext, container_id: &str) -> Result<(), FakeError> {
        let (first, failing) = {
            let mut ids = self.ids.lock().unwrap();
            ids.push(container_id.to_owned());
            let mut fail = self.fail.lock().unwrap();
            let left = fail.entry(container_id.to_owned()).or_default();
            let failing = *left > 0;
            *left = left.saturating_sub(1);
            if let Some((count, context)) = &self.cancel_after
                && ids.len() == *count
            {
                context.cancel();
            }
            (ids.len() == 1, failing)
        };
        if first && let Some(gate) = &self.gate {
            gate.pass().await;
        }
        if failing {
            return Err(FakeError::Failed);
        }
        Ok(())
    }

    async fn terminate(&self, container_id: &str, _event_time: Timestamp) -> Result<(), FakeError> {
        if let Some(gate) = &self.terminate_gate {
            gate.pass().await;
        }
        self.terminated
            .lock()
            .unwrap()
            .push(container_id.to_owned());
        Ok(())
    }

    fn sweep(&self) -> impl Future<Output = usize> + Send {
        self.sweeps.fetch_add(1, Ordering::SeqCst);
        std::future::ready(self.released)
    }

    async fn close_and_wait(&self) {}

    fn is_container_gone(error: &FakeError) -> bool {
        matches!(error, FakeError::Gone)
    }
}

pub(crate) type FakeCoordinator<R = Arc<FakeReconciler>> = Coordinator<Arc<FakeDocker>, R>;

/// A coordinator over the fakes, its metrics, log and clock.
pub(crate) struct Harness<R = Arc<FakeReconciler>> {
    pub(crate) coordinator: Arc<FakeCoordinator<R>>,
    pub(crate) docker: Arc<FakeDocker>,
    pub(crate) reconciler: R,
    pub(crate) metrics: Recorder,
    pub(crate) sink: MemorySink,
    pub(crate) log: LogHandle,
    pub(crate) clock: Arc<dyn Clock>,
}

impl<R: Reconciler + Clone> Harness<R> {
    pub(crate) fn with_clock(docker: FakeDocker, reconciler: R, clock: Arc<dyn Clock>) -> Self {
        let docker = Arc::new(docker);
        let metrics = Recorder::new();
        let (log, sink) = capture_logs(Level::DEBUG);
        let coordinator = Coordinator::new(Settings {
            docker: Arc::clone(&docker),
            reconciler: reconciler.clone(),
            metrics: metrics.clone(),
            timeout: DOCKER_CALL_TIMEOUT,
            clock: Arc::clone(&clock),
            log: log.clone(),
        });
        Self {
            coordinator,
            docker,
            reconciler,
            metrics,
            sink,
            log,
            clock,
        }
    }

    /// The epoch of the latest request.
    pub(crate) fn epoch(&self) -> u64 {
        self.coordinator.current().epoch
    }

    /// Records an enumeration's inspect of the container at wall time `at`, made now.
    pub(crate) fn mark_processed(&self, container_id: &str, at: Timestamp) {
        self.coordinator.mark_processed(
            container_id,
            super::Inspected {
                at,
                monotonic: tokio::time::Instant::now(),
            },
        );
    }

    /// Go's `newCoordinator`, on a clock that follows tokio's.
    pub(crate) fn new(docker: FakeDocker, reconciler: R) -> Self {
        Self::with_clock(docker, reconciler, Arc::new(TokioClock::new(base())))
    }

    /// Go's `startCoordinator`: the coordinator runs until the runtime ends with the test.
    pub(crate) fn start(docker: FakeDocker, reconciler: R) -> Self {
        let harness = Self::new(docker, reconciler);
        let coordinator = Arc::clone(&harness.coordinator);
        tokio::spawn(async move { coordinator.run(&CallContext::new()).await });
        harness
    }

    pub(crate) fn now(&self) -> Timestamp {
        self.clock.now()
    }

    pub(crate) fn logs(&self) -> String {
        self.sink.contents()
    }

    /// The generation of every "config reload complete" line.
    pub(crate) fn completions(&self) -> Vec<String> {
        self.logs()
            .lines()
            .filter(|line| line.contains("config reload complete"))
            .flat_map(|line| {
                line.split(' ')
                    .filter_map(|field| field.strip_prefix("generation="))
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    pub(crate) fn warn_count(&self) -> usize {
        self.logs()
            .matches("reconciliation incomplete; retrying")
            .count()
    }

    pub(crate) fn state(&self) -> (usize, bool) {
        let state = self.coordinator.lock();
        (state.pending.len(), state.incomplete)
    }

    pub(crate) fn settled(&self) -> bool {
        self.state() == (0, false)
    }

    /// Whether no container is being handled.
    pub(crate) fn idle(&self) -> bool {
        self.coordinator.lock().in_flight.is_empty()
    }

    pub(crate) fn processed(&self, container_id: &str) -> bool {
        self.coordinator.lock().processed.contains_key(container_id)
    }

    pub(crate) fn scalar(&self, name: &str, labels: &[(&str, &str)]) -> f64 {
        match self.value(name, labels) {
            Some(Value::Scalar(value)) => value,
            other @ (Some(Value::Histogram { .. }) | None) => panic!("{name}: {other:?}"),
        }
    }

    pub(crate) fn samples(&self, name: &str) -> u64 {
        match self.value(name, &[]) {
            Some(Value::Histogram { count, .. }) => count,
            other @ (Some(Value::Scalar(_)) | None) => panic!("{name}: {other:?}"),
        }
    }

    pub(crate) fn value(&self, name: &str, labels: &[(&str, &str)]) -> Option<Value> {
        self.metrics
            .registry()
            .unwrap()
            .gather()
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

    /// The values `process_one` is responsible for (Go's `metricsSnapshot`).
    pub(crate) fn snapshot(&self) -> Snapshot {
        Snapshot {
            applied_ok: self.counter_or_zero("ok"),
            applied_error: self.counter_or_zero("error"),
            duration_count: self.samples("sda_apply_duration_seconds"),
            last_event: self.scalar("sda_last_event_timestamp_seconds", &[]),
        }
    }

    pub(crate) fn counter_or_zero(&self, result: &str) -> f64 {
        match self.value("sda_rules_applied_total", &[("result", result)]) {
            Some(Value::Scalar(value)) => value,
            Some(Value::Histogram { .. }) | None => 0.0,
        }
    }
}

impl Harness {
    pub(crate) fn seen(&self) -> Vec<String> {
        self.reconciler.seen()
    }
}

/// Go's `metricsSnapshot`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Snapshot {
    pub(crate) applied_ok: f64,
    pub(crate) applied_error: f64,
    pub(crate) duration_count: u64,
    pub(crate) last_event: f64,
}

/// Go's `assertFailureReported`.
pub(crate) fn assert_failure_reported(
    before: Snapshot,
    after: Snapshot,
    logs: &str,
    message: &str,
) {
    assert!(
        (after.applied_error - before.applied_error - 1.0).abs() < f64::EPSILON,
        "rules_applied{{result=error}}: {before:?} -> {after:?}"
    );
    assert!(
        (after.applied_ok - before.applied_ok).abs() < f64::EPSILON,
        "rules_applied{{result=ok}} changed on failure: {before:?} -> {after:?}"
    );
    assert_eq!(after.duration_count - before.duration_count, 1);
    assert!(
        (after.last_event - before.last_event).abs() < f64::EPSILON,
        "last_event_timestamp changed on failure: {before:?} -> {after:?}"
    );
    let line = format!("level=WARN {message} id=bad err=apply failed");
    assert!(
        logs.lines().any(|logged| logged == line),
        "want {line:?} in:\n{logs}"
    );
}

pub(crate) fn event(action: &str, container_id: &str, at: Timestamp) -> Event {
    Event {
        action: action.to_owned(),
        actor: Actor {
            id: container_id.to_owned(),
            ..Actor::default()
        },
        time_nano: i64::try_from(at.as_nanosecond()).unwrap(),
        ..Event::default()
    }
}

/// Go's `startEvent`.
pub(crate) fn start_event(container_id: &str, at: Timestamp) -> Event {
    event("start", container_id, at)
}

/// Runs `handle_event` on a task of its own, as the event loop runs it beside the test.
pub(crate) fn spawn_event<R: Reconciler>(
    coordinator: &Arc<FakeCoordinator<R>>,
    event: Event,
) -> JoinHandle<()> {
    let coordinator = Arc::clone(coordinator);
    tokio::spawn(async move { coordinator.handle_event(&CallContext::new(), &event).await })
}

/// An inspector the processor of these tests is never answered by: a call hangs until its
/// context is done.
pub(crate) struct HangingInspector;

impl Inspector for HangingInspector {
    fn container_inspect(
        &self,
        _id: &str,
    ) -> impl Future<Output = Result<ContainerInspect, docker::Error>> + Send {
        std::future::pending()
    }
}

pub(crate) fn hanging_processor() -> Arc<Processor<HangingInspector>> {
    let (store, publisher) = new_store(Runtime {
        dry_run: false,
        policy: Global::new("all", None, None).unwrap(),
    });
    Processor::new(processor::Settings {
        inspector: HangingInspector,
        store,
        publisher,
        metrics: Recorder::noop(),
        host_root: "/host".to_owned(),
        proc_root: "/".to_owned(),
        call_timeout: None,
        clock: Arc::new(TokioClock::new(base())),
        log: capture_logs(Level::DEBUG).0,
    })
}
