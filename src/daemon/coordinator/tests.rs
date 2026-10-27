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

//! The coordinator tests of `coordinator_test.go`, with the coordinator halves of
//! `events_test.go`, `daemon_test.go` and `timeout_test.go`, on the paused clock: the backoffs
//! are the production ones, which the paused clock skips.
//!
//! Not ported here: `TestCoordinator_DedupSharedWithConsumer`,
//! `TestCoordinator_DedupAcrossReconnectUnderRace` and `TestListenEvents_ReconnectReenumerates`
//! drive the event loop (`consumeEvents`, `listenEvents`) and are ported with it.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::{
    ContainerLister, Coordinator, Inspected, PROCESSED_TTL, PassOutcome, Reconciler, Settings,
};
use crate::clock::{Clock, FakeClock, TokioClock};
use crate::config::{Runtime, new_store};
use crate::daemon::{DOCKER_CALL_TIMEOUT, MIN_BACKOFF};
use crate::docker::types::{Actor, ContainerInspect, ContainerSummary, Event};
use crate::docker::{self, CallContext, ErrorKind};
use crate::logger::{Level, MemorySink};
use crate::metrics::{Recorder, Value};
use crate::policy::Global;
use crate::processor::{self, Inspector, Processor};
use crate::task::finish;
use crate::test_support::{capture_logs, eventually};

/// How long a wait for something the coordinator will do may take, in paused tokio time.
const WITHIN: Duration = Duration::from_secs(300);

/// The start of the tests' clock.
fn base() -> Timestamp {
    Timestamp::from_second(1_700_000_000).unwrap()
}

/// Go's `fakeDocker`, as far as the coordinator uses it: the container list.
#[derive(Default)]
struct FakeDocker {
    containers: Vec<&'static str>,
    list_failures: AtomicUsize,
    hang_list: bool,
    list_calls: AtomicUsize,
}

impl FakeDocker {
    fn listing(containers: &[&'static str]) -> Self {
        Self {
            containers: containers.to_vec(),
            ..Self::default()
        }
    }

    fn list_calls(&self) -> usize {
        self.list_calls.load(Ordering::SeqCst)
    }
}

impl ContainerLister for Arc<FakeDocker> {
    async fn container_list(&self) -> Result<Vec<ContainerSummary>, docker::Error> {
        self.list_calls.fetch_add(1, Ordering::SeqCst);
        if self.hang_list {
            std::future::pending::<()>().await;
        }
        let failing = self
            .list_failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok();
        if failing {
            return Err(docker::Error::connection_failed("list failed".to_owned()));
        }
        Ok(self
            .containers
            .iter()
            .map(|id| ContainerSummary {
                id: (*id).to_owned(),
            })
            .collect())
    }
}

/// The errors of the fakes: Go's `errApplyFailed`, and a container Docker no longer knows.
#[derive(Debug)]
enum FakeError {
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
struct Gate {
    entered: CancellationToken,
    open: CancellationToken,
}

impl Gate {
    async fn pass(&self) {
        self.entered.cancel();
        self.open.cancelled().await;
    }
}

/// Go's `recordingApply`, with the terminate and sweep the Go tests set on the coordinator.
#[derive(Default)]
struct FakeReconciler {
    ids: Mutex<Vec<String>>,
    /// Remaining failures per container.
    fail: Mutex<BTreeMap<String, usize>>,
    /// Holds the first reconcile.
    gate: Option<Gate>,
    terminated: Mutex<Vec<String>>,
    /// Holds every termination.
    terminate_gate: Option<Gate>,
    /// What each sweep reports released.
    released: usize,
    sweeps: AtomicUsize,
}

impl FakeReconciler {
    fn gated() -> Self {
        Self {
            gate: Some(Gate::default()),
            ..Self::default()
        }
    }

    fn failing(container_id: &str, times: usize) -> Self {
        let reconciler = Self::default();
        reconciler.set_failures(container_id, times);
        reconciler
    }

    fn set_failures(&self, container_id: &str, times: usize) {
        self.fail
            .lock()
            .unwrap()
            .insert(container_id.to_owned(), times);
    }

    fn seen(&self) -> Vec<String> {
        self.ids.lock().unwrap().clone()
    }

    fn terminated(&self) -> Vec<String> {
        self.terminated.lock().unwrap().clone()
    }

    fn gate(&self) -> &Gate {
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

type FakeCoordinator<R = Arc<FakeReconciler>> = Coordinator<Arc<FakeDocker>, R>;

/// A coordinator over the fakes, its metrics, log and clock.
struct Harness<R = Arc<FakeReconciler>> {
    coordinator: Arc<FakeCoordinator<R>>,
    docker: Arc<FakeDocker>,
    reconciler: R,
    metrics: Recorder,
    sink: MemorySink,
    clock: Arc<dyn Clock>,
}

impl<R: Reconciler + Clone> Harness<R> {
    fn with_clock(docker: FakeDocker, reconciler: R, clock: Arc<dyn Clock>) -> Self {
        let docker = Arc::new(docker);
        let metrics = Recorder::new();
        let (log, sink) = capture_logs(Level::DEBUG);
        let coordinator = Coordinator::new(Settings {
            docker: Arc::clone(&docker),
            reconciler: reconciler.clone(),
            metrics: metrics.clone(),
            timeout: DOCKER_CALL_TIMEOUT,
            clock: Arc::clone(&clock),
            log,
        });
        Self {
            coordinator,
            docker,
            reconciler,
            metrics,
            sink,
            clock,
        }
    }

    /// Go's `newCoordinator`, on a clock that follows tokio's.
    fn new(docker: FakeDocker, reconciler: R) -> Self {
        Self::with_clock(docker, reconciler, Arc::new(TokioClock::new(base())))
    }

    /// Go's `startCoordinator`: the coordinator runs until the runtime ends with the test.
    fn start(docker: FakeDocker, reconciler: R) -> Self {
        let harness = Self::new(docker, reconciler);
        let coordinator = Arc::clone(&harness.coordinator);
        tokio::spawn(async move { coordinator.run(&CallContext::new()).await });
        harness
    }

    fn now(&self) -> Timestamp {
        self.clock.now()
    }

    fn logs(&self) -> String {
        self.sink.contents()
    }

    /// The generation of every "config reload complete" line.
    fn completions(&self) -> Vec<String> {
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

    fn warn_count(&self) -> usize {
        self.logs()
            .matches("reconciliation incomplete; retrying")
            .count()
    }

    fn state(&self) -> (usize, bool) {
        let state = self.coordinator.lock();
        (state.pending.len(), state.incomplete)
    }

    fn settled(&self) -> bool {
        self.state() == (0, false)
    }

    /// Whether no container is being handled.
    fn idle(&self) -> bool {
        self.coordinator.lock().in_flight.is_empty()
    }

    fn processed(&self, container_id: &str) -> bool {
        self.coordinator.lock().processed.contains_key(container_id)
    }

    fn scalar(&self, name: &str, labels: &[(&str, &str)]) -> f64 {
        match self.value(name, labels) {
            Some(Value::Scalar(value)) => value,
            other @ (Some(Value::Histogram { .. }) | None) => panic!("{name}: {other:?}"),
        }
    }

    fn samples(&self, name: &str) -> u64 {
        match self.value(name, &[]) {
            Some(Value::Histogram { count, .. }) => count,
            other @ (Some(Value::Scalar(_)) | None) => panic!("{name}: {other:?}"),
        }
    }

    fn value(&self, name: &str, labels: &[(&str, &str)]) -> Option<Value> {
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
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            applied_ok: self.counter_or_zero("ok"),
            applied_error: self.counter_or_zero("error"),
            duration_count: self.samples("sda_apply_duration_seconds"),
            last_event: self.scalar("sda_last_event_timestamp_seconds", &[]),
        }
    }

    fn counter_or_zero(&self, result: &str) -> f64 {
        match self.value("sda_rules_applied_total", &[("result", result)]) {
            Some(Value::Scalar(value)) => value,
            Some(Value::Histogram { .. }) | None => 0.0,
        }
    }
}

impl Harness {
    fn seen(&self) -> Vec<String> {
        self.reconciler.seen()
    }
}

/// Go's `metricsSnapshot`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Snapshot {
    applied_ok: f64,
    applied_error: f64,
    duration_count: u64,
    last_event: f64,
}

/// Go's `assertFailureReported`.
fn assert_failure_reported(before: Snapshot, after: Snapshot, logs: &str, message: &str) {
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

fn event(action: &str, container_id: &str, at: Timestamp) -> Event {
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
fn start_event(container_id: &str, at: Timestamp) -> Event {
    event("start", container_id, at)
}

/// Runs `handle_event` on a task of its own, as the event loop runs it beside the test.
fn spawn_event<R: Reconciler>(
    coordinator: &Arc<FakeCoordinator<R>>,
    event: Event,
) -> JoinHandle<()> {
    let coordinator = Arc::clone(coordinator);
    tokio::spawn(async move { coordinator.handle_event(&CallContext::new(), &event).await })
}

/// Go: `TestCoordinator_ListFailsThenSucceeds`
#[tokio::test(start_paused = true)]
async fn list_fails_then_succeeds() {
    let docker = FakeDocker {
        list_failures: AtomicUsize::new(2),
        ..FakeDocker::listing(&["c1"])
    };
    let harness = Harness::start(docker, Arc::new(FakeReconciler::default()));
    let started = tokio::time::Instant::now();

    harness.coordinator.request(1);
    eventually("the reload to complete", WITHIN, || {
        harness.settled() && harness.completions().len() == 1
    })
    .await;

    assert_eq!(
        harness.docker.list_calls(),
        3,
        "two failures, then the pass"
    );
    assert_eq!(harness.seen(), ["c1"]);
    // The list is retried after one second, then two.
    assert!(started.elapsed() >= Duration::from_secs(3));
    assert!(
        harness.logs().contains(
            "level=WARN could not list containers; reconciliation incomplete, retrying \
             err=list containers: list failed generation=1"
        ),
        "{}",
        harness.logs()
    );
    assert!(harness.logs().contains(
        "level=WARN reconciliation incomplete; retrying pending_containers=0 \
         reload_incomplete=true generation=1"
    ));
    assert!(harness.scalar("sda_reload_incomplete", &[]).abs() < f64::EPSILON);
}

/// Go: `TestCoordinator_PendingGaugeReturnsToZero`
#[tokio::test(start_paused = true)]
async fn pending_gauge_returns_to_zero() {
    let harness = Harness::start(
        FakeDocker::listing(&["c1", "c2"]),
        Arc::new(FakeReconciler::failing("c2", 2)),
    );

    harness.coordinator.request(1);
    eventually("c2 to be pending", WITHIN, || harness.state().0 == 1).await;
    assert!(
        harness.completions().is_empty(),
        "completion logged with a container pending"
    );

    eventually("the reload to complete", WITHIN, || {
        harness.settled() && harness.completions().len() == 1
    })
    .await;
    assert!(
        harness
            .scalar("sda_reconcile_pending_containers", &[])
            .abs()
            < f64::EPSILON
    );
    assert_eq!(
        harness.seen(),
        ["c1", "c2", "c2", "c2"],
        "c2 retried until it succeeds"
    );
    assert!(
        harness
            .logs()
            .contains("level=WARN could not reconcile pending container id=c2 err=apply failed")
    );
}

/// Go: `TestCoordinator_OverlappingPublishesCompleteOnlyNewest`. A publication arriving while
/// the pass for the previous one runs supersedes it; only the newer generation is reported
/// complete.
#[tokio::test(start_paused = true)]
async fn overlapping_publishes_complete_only_newest() {
    let harness = Harness::start(
        FakeDocker::listing(&["c1", "c2"]),
        Arc::new(FakeReconciler::gated()),
    );

    harness.coordinator.request(1);
    harness.reconciler.gate().entered.cancelled().await;
    harness.coordinator.request(2);
    harness.reconciler.gate().open.cancel();

    eventually("a completion", WITHIN, || {
        harness.settled() && !harness.completions().is_empty()
    })
    .await;
    assert_eq!(harness.completions(), ["2"]);
    // The superseded pass stopped after its current container.
    assert_eq!(harness.seen(), ["c1", "c1", "c2"]);
}

/// Go: `TestCoordinator_OlderResultCannotClearGauges`. A success that started before a newer
/// request neither clears the re-tagged pending entry nor completes the reload.
#[tokio::test(start_paused = true)]
async fn older_result_cannot_clear_gauges() {
    let harness = Harness::new(FakeDocker::default(), Arc::new(FakeReconciler::default()));
    let coordinator = &harness.coordinator;

    coordinator.request(1);
    let older = coordinator.current();
    coordinator.settle("c1", older, Some(&FakeError::Failed));
    coordinator.request(2);

    coordinator.settle("c1", older, None);
    coordinator.finish_pass(older);

    assert_eq!(harness.state(), (1, true), "after an older result");
    assert!((harness.scalar("sda_reconcile_pending_containers", &[]) - 1.0).abs() < f64::EPSILON);
    assert!((harness.scalar("sda_reload_incomplete", &[]) - 1.0).abs() < f64::EPSILON);
    assert!(
        harness.completions().is_empty(),
        "an older result logged completion"
    );

    let newer = coordinator.current();
    coordinator.settle("c1", newer, None);
    coordinator.finish_pass(newer);

    assert!(harness.settled());
    assert_eq!(harness.completions(), ["2"]);
    assert!(
        harness
            .logs()
            .contains("level=INFO config reload complete generation=2 epoch=2")
    );
}

/// Go: `TestCoordinator_SameGenerationTriggersRunTrailingPass`. Two systemd reloads under one
/// generation while a pass runs coalesce into one trailing pass.
#[tokio::test(start_paused = true)]
async fn same_generation_triggers_run_trailing_pass() {
    let harness = Harness::start(
        FakeDocker::listing(&["c1"]),
        Arc::new(FakeReconciler::gated()),
    );

    harness.coordinator.request(1);
    harness.reconciler.gate().entered.cancelled().await;
    harness.coordinator.request(1);
    harness.coordinator.request(1);
    harness.reconciler.gate().open.cancel();

    eventually("the trailing pass", WITHIN, || {
        harness.settled() && harness.seen().len() == 2
    })
    .await;

    // Negative assertion: nothing else is pending, so no further pass may start within a window
    // well past every backoff.
    tokio::time::sleep(Duration::from_mins(2)).await;
    assert_eq!(
        harness.docker.list_calls(),
        2,
        "the pass and one trailing pass"
    );
}

/// An inspector the processor of these tests is never answered by: a call hangs until its
/// context is done.
struct HangingInspector;

impl Inspector for HangingInspector {
    fn container_inspect(
        &self,
        _id: &str,
    ) -> impl Future<Output = Result<ContainerInspect, docker::Error>> + Send {
        std::future::pending()
    }
}

fn hanging_processor() -> Arc<Processor<HangingInspector>> {
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

/// Go: `TestPublishAndReconcile_VisitsRunningContainers`. A `SIGHUP` publication reaches every
/// running container through the coordinator.
#[tokio::test(start_paused = true)]
async fn publish_and_reconcile_visits_running_containers() {
    let proc = hanging_processor();
    let harness = Harness::start(
        FakeDocker::listing(&["a", "b"]),
        Arc::new(FakeReconciler::default()),
    );
    proc.set_pass_requester(harness.coordinator.pass_requester())
        .await;

    let generation = proc
        .publish_and_reconcile(
            &CallContext::new(),
            Runtime {
                dry_run: false,
                policy: Global::new("opt-in", None, None).unwrap(),
            },
        )
        .await;

    eventually("both containers", WITHIN, || {
        harness.settled() && harness.seen().len() == 2
    })
    .await;
    assert_eq!(generation, 2);
    assert_eq!(harness.coordinator.current().generation, 2);
    assert_eq!(harness.seen(), ["a", "b"]);
}

/// The requester holds the coordinator weakly: the processor it is installed in does not keep
/// the coordinator alive, and a request after the coordinator is gone does nothing.
#[tokio::test(start_paused = true)]
async fn pass_requester_does_not_keep_the_coordinator_alive() {
    let harness = Harness::new(FakeDocker::default(), Arc::new(FakeReconciler::default()));
    let requester = harness.coordinator.pass_requester();
    requester(&CallContext::new(), 7);
    assert_eq!(harness.coordinator.current().generation, 7);

    let coordinator = Arc::downgrade(&harness.coordinator);
    drop(harness);
    assert!(coordinator.upgrade().is_none());
    requester(&CallContext::new(), 8);
}

/// Go: `TestCoordinator_GoneContainerLeavesPending`. A container Docker no longer knows ends its
/// retries instead of keeping the reload incomplete forever.
#[tokio::test(start_paused = true)]
async fn gone_container_leaves_pending() {
    let harness = Harness::new(FakeDocker::default(), Arc::new(FakeReconciler::default()));
    let coordinator = &harness.coordinator;
    coordinator.request(1);
    let key = coordinator.current();

    coordinator.settle("c1", key, Some(&FakeError::Failed));
    coordinator.settle("c1", key, Some(&FakeError::Gone));
    coordinator.settle("c2", key, Some(&FakeError::Gone));
    coordinator.finish_pass(key);

    assert!(
        harness.settled(),
        "{:?}: want the gone containers dropped",
        harness.state()
    );
}

/// Go: `TestCoordinator_EventDuringEnumerationAppliesOnce`. A start event for the container an
/// enumeration is applying, and not newer than that apply, coalesces into it (exactly one
/// apply); a newer event (a restart) queues exactly one follow-up.
#[tokio::test(start_paused = true)]
async fn event_during_enumeration_applies_once() {
    for (name, delay, want) in [
        (
            "event covered by the apply",
            SignedDuration::from_secs(-1),
            1,
        ),
        ("newer event", SignedDuration::from_hours(1), 2),
    ] {
        let harness = Harness::start(
            FakeDocker::listing(&["c1"]),
            Arc::new(FakeReconciler::gated()),
        );

        harness.coordinator.request(1);
        harness.reconciler.gate().entered.cancelled().await;

        let at = harness.now().checked_add(delay).unwrap();
        harness
            .coordinator
            .handle_event(&CallContext::new(), &start_event("c1", at))
            .await;
        harness.reconciler.gate().open.cancel();

        eventually(name, WITHIN, || harness.settled() && harness.idle()).await;
        assert_eq!(harness.seen().len(), want, "{name}");
    }
}

/// Go: `TestCoordinator_DieEventTerminates`. `die` and `destroy` are adjudicated by the
/// coordinator; one arriving while the container is being reconciled runs after that reconcile,
/// not alongside it.
#[tokio::test(start_paused = true)]
async fn die_event_terminates() {
    let harness = Harness::start(
        FakeDocker::listing(&["c1"]),
        Arc::new(FakeReconciler::gated()),
    );

    harness.coordinator.request(1);
    harness.reconciler.gate().entered.cancelled().await;

    harness
        .coordinator
        .handle_event(&CallContext::new(), &event("die", "c1", harness.now()))
        .await;
    assert!(
        harness.reconciler.terminated().is_empty(),
        "die was handled while the container was still being reconciled"
    );

    harness.reconciler.gate().open.cancel();
    eventually("the reservation to end", WITHIN, || harness.idle()).await;

    harness
        .coordinator
        .handle_event(&CallContext::new(), &event("destroy", "c2", harness.now()))
        .await;

    assert_eq!(
        harness.reconciler.terminated(),
        ["c1", "c2"],
        "the queued die for c1, then the destroy for c2"
    );
}

/// Go: `TestCoordinator_ReservedPendingDoesNotSpin`. A pending container that another holder is
/// handling is not retried (nor its due time polled) until the reservation ends; then it is
/// retried once.
#[tokio::test(start_paused = true)]
async fn reserved_pending_does_not_spin() {
    let harness = Harness::start(FakeDocker::default(), Arc::new(FakeReconciler::default()));
    let coordinator = &harness.coordinator;

    assert!(
        coordinator.reserve("c1", None, false),
        "could not reserve c1"
    );
    coordinator.settle("c1", coordinator.current(), Some(&FakeError::Failed));
    coordinator.wake.notify_one();

    // Negative assertion: a window well past the entry's backoff.
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert!(
        harness.warn_count() <= 1 && harness.seen().is_empty(),
        "while reserved: {} warnings, applies {:?}; want no retry and no spinning",
        harness.warn_count(),
        harness.seen()
    );

    assert_eq!(
        coordinator.next("c1"),
        (false, None),
        "a skipped retry must not queue work on the holder"
    );

    eventually("the retry", WITHIN, || harness.settled()).await;
    assert_eq!(
        harness.seen(),
        ["c1"],
        "one retry after the reservation ended"
    );
}

/// Go: `TestCoordinator_StartDuringCleanupQueuesReconcile`. A cleanup covers no start event, so a
/// start (a restart) arriving while a die is being handled is reconciled afterwards, whatever its
/// timestamp.
#[tokio::test(start_paused = true)]
async fn start_during_cleanup_queues_reconcile() {
    let reconciler = Arc::new(FakeReconciler {
        terminate_gate: Some(Gate::default()),
        ..FakeReconciler::default()
    });
    let harness = Harness::start(FakeDocker::default(), Arc::clone(&reconciler));
    let gate = reconciler.terminate_gate.as_ref().unwrap();

    let die = spawn_event(&harness.coordinator, event("die", "c1", harness.now()));
    gate.entered.cancelled().await;

    let earlier = harness
        .now()
        .checked_sub(SignedDuration::from_secs(1))
        .unwrap();
    harness
        .coordinator
        .handle_event(&CallContext::new(), &start_event("c1", earlier))
        .await;
    gate.open.cancel();
    die.await.unwrap();

    assert_eq!(
        harness.seen(),
        ["c1"],
        "the start reconciled after the cleanup"
    );
}

/// Go: `TestCoordinator_NoCompletionWhileReconcileRunning`. A pass that finishes while another
/// reconcile is still running must not report completion until that reconcile has settled.
#[tokio::test(start_paused = true)]
async fn no_completion_while_reconcile_running() {
    let harness = Harness::start(FakeDocker::default(), Arc::new(FakeReconciler::gated()));

    let start = spawn_event(&harness.coordinator, start_event("c1", harness.now()));
    harness.reconciler.gate().entered.cancelled().await;
    harness.coordinator.request(1);

    eventually("the pass to finish", WITHIN, || !harness.state().1).await;
    assert!(
        harness.completions().is_empty(),
        "completion logged while a reconcile was running"
    );

    harness.reconciler.gate().open.cancel();
    start.await.unwrap();
    eventually("the completion", WITHIN, || {
        harness.completions().len() == 1
    })
    .await;
}

/// An inspect made now, at wall time `at`.
fn inspected(at: Timestamp) -> Inspected {
    Inspected {
        at,
        monotonic: Instant::now(),
    }
}

/// Go: `TestCoordinator_ProcessedEntriesExpire`. An enumeration entry older than the TTL neither
/// suppresses an event nor survives a prune.
#[tokio::test(start_paused = true)]
async fn processed_entries_expire() {
    let harness = Harness::new(FakeDocker::default(), Arc::new(FakeReconciler::default()));
    let coordinator = &harness.coordinator;
    let inspected_at = harness.now();
    coordinator.mark_processed("stale-a", inspected(inspected_at));
    coordinator.mark_processed("stale-b", inspected(inspected_at));
    tokio::time::advance(PROCESSED_TTL).await;
    coordinator.mark_processed("fresh", inspected(harness.now()));
    tokio::time::advance(Duration::from_secs(1)).await;

    let earlier = inspected_at
        .checked_sub(SignedDuration::from_secs(1))
        .unwrap();
    assert!(
        !coordinator.skip_event("stale-a", earlier),
        "an expired entry must not suppress an event"
    );

    coordinator.prune_processed();
    let processed: Vec<String> = coordinator.lock().processed.keys().cloned().collect();
    assert_eq!(
        processed,
        ["fresh"],
        "only the fresh entry survives the prune"
    );
}

/// An entry within the TTL suppresses an event not newer than its inspect, and only the first
/// event for the container.
#[tokio::test(start_paused = true)]
async fn processed_entry_skips_one_covered_event() {
    let harness = Harness::new(FakeDocker::default(), Arc::new(FakeReconciler::default()));
    let coordinator = &harness.coordinator;
    let inspected_at = harness.now();

    coordinator.mark_processed("c1", inspected(inspected_at));
    assert!(coordinator.skip_event("c1", inspected_at));
    assert!(
        !coordinator.skip_event("c1", inspected_at),
        "the entry is consumed"
    );

    coordinator.mark_processed("c1", inspected(inspected_at));
    let newer = inspected_at
        .checked_add(SignedDuration::from_nanos(1))
        .unwrap();
    assert!(
        !coordinator.skip_event("c1", newer),
        "a newer event is a restart"
    );
    assert!(!harness.processed("c1"));
}

/// Retries and the dedup's expiry are timed on the monotonic clock, as Go's are: a wall-clock
/// step neither delays a retry nor ages an entry.
#[tokio::test(start_paused = true)]
async fn wall_clock_steps_move_neither_retries_nor_expiry() {
    let clock = Arc::new(FakeClock::new(base()));
    let harness = Harness::with_clock(
        FakeDocker::default(),
        Arc::new(FakeReconciler::default()),
        Arc::clone(&clock) as Arc<dyn Clock>,
    );
    let coordinator = &harness.coordinator;

    coordinator.settle("c1", coordinator.current(), Some(&FakeError::Failed));
    let due = coordinator.next_due();
    clock.set(base().checked_sub(SignedDuration::from_hours(1)).unwrap());
    assert_eq!(coordinator.next_due(), due);
    tokio::time::advance(MIN_BACKOFF).await;
    assert_eq!(
        coordinator.due_ids(),
        ["c1"],
        "a backward step delayed the retry"
    );

    coordinator.mark_processed("c2", inspected(base()));
    clock.set(base().checked_add(SignedDuration::from_hours(1)).unwrap());
    coordinator.prune_processed();
    assert!(
        coordinator.skip_event("c2", base()),
        "a forward step expired the entry"
    );
}

/// Go: `TestCoordinator_PruneArmedOnlyWithEntries`. The prune timer is armed only while the
/// processed map has entries, and an armed timer is kept rather than restarted on every loop
/// iteration.
#[tokio::test(start_paused = true)]
async fn prune_armed_only_with_entries() {
    let harness = Harness::new(FakeDocker::default(), Arc::new(FakeReconciler::default()));
    let coordinator = &harness.coordinator;

    assert_eq!(coordinator.arm_prune(None), None, "armed for an empty map");

    coordinator.mark_processed("c1", inspected(harness.now()));
    let armed = coordinator.arm_prune(None);
    assert!(armed.is_some(), "not armed with an entry to expire");
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(
        coordinator.arm_prune(armed),
        armed,
        "an armed timer was replaced"
    );

    tokio::time::advance(PROCESSED_TTL).await;
    coordinator.prune_processed();
    assert_eq!(
        coordinator.arm_prune(None),
        None,
        "re-armed after the prune emptied the map"
    );
}

/// The run loop prunes an expired entry once the armed timer fires.
#[tokio::test(start_paused = true)]
async fn run_prunes_after_the_ttl() {
    let harness = Harness::start(
        FakeDocker::listing(&["c1"]),
        Arc::new(FakeReconciler::default()),
    );
    harness.coordinator.request(1);
    eventually("the pass", WITHIN, || harness.processed("c1")).await;

    eventually("the prune", WITHIN, || !harness.processed("c1")).await;
}

/// The lifecycle history is swept every minute, and a sweep that released records says so.
#[tokio::test(start_paused = true)]
async fn run_sweeps_every_minute() {
    let reconciler = Arc::new(FakeReconciler {
        released: 2,
        ..FakeReconciler::default()
    });
    let harness = Harness::start(FakeDocker::default(), Arc::clone(&reconciler));

    tokio::time::sleep(Duration::from_secs(59)).await;
    assert_eq!(reconciler.sweeps.load(Ordering::SeqCst), 0);
    tokio::time::sleep(Duration::from_secs(62)).await;
    assert_eq!(reconciler.sweeps.load(Ordering::SeqCst), 2);
    assert!(
        harness
            .logs()
            .contains("level=DEBUG lifecycle sweep released records released=2")
    );
}

/// The first pass is signalled when it ends, even when its list failed.
#[tokio::test(start_paused = true)]
async fn first_pass_is_signalled_after_a_failed_list() {
    let docker = FakeDocker {
        list_failures: AtomicUsize::new(1),
        ..FakeDocker::default()
    };
    let harness = Harness::start(docker, Arc::new(FakeReconciler::default()));
    let context = CallContext::new();

    let waited = tokio::time::timeout(
        Duration::from_millis(1),
        harness.coordinator.await_first_pass(&context),
    )
    .await;
    assert!(waited.is_err(), "signalled before any pass");

    harness.coordinator.request(1);
    harness.coordinator.await_first_pass(&context).await;
    assert_eq!(harness.docker.list_calls(), 1);

    let cancelled = CallContext::new();
    cancelled.cancel();
    let fresh = Harness::new(FakeDocker::default(), Arc::new(FakeReconciler::default()));
    fresh.coordinator.await_first_pass(&cancelled).await;
}

/// Go: `TestProcessOne_StartupFailureMatchesEventPath`. A failing apply during the startup
/// enumeration records the same metrics and warning as the same failure on the event path.
#[tokio::test(start_paused = true)]
async fn process_one_startup_failure_matches_event_path() {
    let harness = Harness::new(
        FakeDocker::listing(&["bad"]),
        Arc::new(FakeReconciler::failing("bad", 2)),
    );
    let before = harness.snapshot();

    let outcome = harness
        .coordinator
        .pass(&CallContext::new(), harness.coordinator.current())
        .await;
    assert_eq!(outcome, PassOutcome::Done);
    assert!(
        !harness.processed("bad"),
        "a failed container must not be recorded as processed"
    );
    assert_failure_reported(
        before,
        harness.snapshot(),
        &harness.logs(),
        "could not process running container",
    );

    let before = harness.snapshot();
    harness
        .coordinator
        .handle_event(&CallContext::new(), &start_event("bad", harness.now()))
        .await;
    assert_failure_reported(
        before,
        harness.snapshot(),
        &harness.logs(),
        "could not process container",
    );
}

/// Go: `TestProcessOne_StartupSuccessRecordsMetrics`. The success path at startup: an ok result,
/// a duration sample and the last-event time.
#[tokio::test(start_paused = true)]
async fn process_one_startup_success_records_metrics() {
    let harness = Harness::new(
        FakeDocker::listing(&["good"]),
        Arc::new(FakeReconciler::default()),
    );
    let before = harness.snapshot();

    let outcome = harness
        .coordinator
        .pass(&CallContext::new(), harness.coordinator.current())
        .await;
    assert_eq!(outcome, PassOutcome::Done);

    let after = harness.snapshot();
    assert!((after.applied_ok - before.applied_ok - 1.0).abs() < f64::EPSILON);
    assert_eq!(after.duration_count - before.duration_count, 1);
    let now = f64::from(i32::try_from(harness.now().as_second()).unwrap());
    assert!((after.last_event - now).abs() < f64::EPSILON);
    assert!(
        harness.processed("good"),
        "a successful container is recorded as processed"
    );
}

/// Go: `TestCoordinatorPass_HungContainerListIsBounded`.
#[tokio::test(start_paused = true)]
async fn hung_container_list_is_bounded() {
    let docker = FakeDocker {
        hang_list: true,
        ..FakeDocker::default()
    };
    let harness = Harness::new(docker, Arc::new(FakeReconciler::default()));

    let outcome = tokio::time::timeout(
        DOCKER_CALL_TIMEOUT * 20,
        harness
            .coordinator
            .pass(&CallContext::new(), harness.coordinator.current()),
    )
    .await
    .expect("the hung list is bounded");
    assert_eq!(outcome, PassOutcome::ListFailed);
    assert!(
        harness
            .logs()
            .contains("err=list containers: context deadline exceeded")
    );
}

/// Go: `TestProcessOne_HungContainerInspectIsBounded`.
#[tokio::test(start_paused = true)]
async fn hung_container_inspect_is_bounded() {
    let docker = Arc::new(FakeDocker::default());
    let (log, _sink) = capture_logs(Level::DEBUG);
    let coordinator = Coordinator::new(Settings {
        docker,
        reconciler: hanging_processor(),
        metrics: Recorder::noop(),
        timeout: DOCKER_CALL_TIMEOUT,
        clock: Arc::new(TokioClock::new(base())),
        log,
    });

    let result = tokio::time::timeout(
        DOCKER_CALL_TIMEOUT * 20,
        coordinator.process_one(&CallContext::new(), "c1", "fail"),
    )
    .await
    .expect("the hung inspect is bounded");
    let error = result.unwrap_err();
    assert!(error.is_docker(ErrorKind::DeadlineExceeded), "{error}");
}

/// What the regression tests record of the transaction of [`Transactional`].
type Journal = Arc<Mutex<Vec<String>>>;

/// A reconciler shaped like [`Processor::reconcile`]: its mutation is a blocking transaction it
/// awaits to completion, and the first one blocks until the test releases it, whatever the
/// caller's context says.
struct Transactional {
    journal: Journal,
    entered: CancellationToken,
    release: Mutex<Option<Receiver<()>>>,
}

impl Transactional {
    fn new() -> (Arc<Self>, SyncSender<()>) {
        let (release, released) = sync_channel(0);
        let reconciler = Arc::new(Self {
            journal: Arc::default(),
            entered: CancellationToken::new(),
            release: Mutex::new(Some(released)),
        });
        (reconciler, release)
    }

    fn journal(&self) -> Vec<String> {
        self.journal.lock().unwrap().clone()
    }
}

impl Reconciler for Arc<Transactional> {
    type Error = FakeError;

    async fn reconcile(&self, context: &CallContext, container_id: &str) -> Result<(), FakeError> {
        let journal = Arc::clone(&self.journal);
        let entered = self.entered.clone();
        let release = self.release.lock().unwrap().take();
        let container_id = container_id.to_owned();
        let transaction = tokio::task::spawn_blocking(move || {
            journal
                .lock()
                .unwrap()
                .push(format!("begin {container_id}"));
            entered.cancel();
            if let Some(release) = release {
                release.recv().unwrap();
            }
            journal.lock().unwrap().push(format!("end {container_id}"));
        });
        finish(transaction).await;
        self.journal
            .lock()
            .unwrap()
            .push(format!("context {:?}", context.err()));
        Ok(())
    }

    fn terminate(
        &self,
        _container_id: &str,
        _event_time: Timestamp,
    ) -> impl Future<Output = Result<(), FakeError>> + Send {
        std::future::ready(Ok(()))
    }

    fn sweep(&self) -> impl Future<Output = usize> + Send {
        std::future::ready(0)
    }

    async fn close_and_wait(&self) {}

    fn is_container_gone(_error: &FakeError) -> bool {
        false
    }
}

/// Starts an event whose caller gives up after the reconcile timeout, as a timeout around the
/// event loop would, and waits until its transaction has begun.
async fn timed_out_caller(harness: &Harness<Arc<Transactional>>) {
    let coordinator = Arc::clone(&harness.coordinator);
    let start = start_event("c1", harness.now());
    let caller = tokio::spawn(async move {
        tokio::time::timeout(
            DOCKER_CALL_TIMEOUT,
            coordinator.handle_event(&CallContext::new(), &start),
        )
        .await
    });
    harness.reconciler.entered.cancelled().await;

    // The blocking transaction keeps the paused clock from advancing by itself.
    tokio::time::advance(DOCKER_CALL_TIMEOUT + Duration::from_secs(1)).await;
    assert!(
        caller.await.unwrap().is_err(),
        "the caller gave up and its future was dropped"
    );
}

/// A reconcile whose timeout passes during its blocking transaction, and whose caller's future
/// is dropped then, keeps the container reserved until the transaction ends; an event for the
/// container meanwhile is queued behind it and runs after it, never beside it.
#[tokio::test(start_paused = true)]
async fn timeout_during_apply_keeps_the_reservation_until_the_transaction_ends() {
    let (reconciler, release) = Transactional::new();
    let harness = Harness::new(FakeDocker::default(), Arc::clone(&reconciler));

    timed_out_caller(&harness).await;
    assert!(
        !harness.idle(),
        "the reservation ended with the caller's future"
    );

    let restart = harness.now();
    harness
        .coordinator
        .handle_event(&CallContext::new(), &start_event("c1", restart))
        .await;
    assert_eq!(
        reconciler.journal(),
        ["begin c1"],
        "the event ran beside the transaction"
    );

    release.send(()).unwrap();
    eventually("the queued reconcile", WITHIN, || harness.idle()).await;
    assert_eq!(
        reconciler.journal(),
        [
            "begin c1",
            "end c1",
            "context Some(DeadlineExceeded)",
            "begin c1",
            "end c1",
            "context None",
        ]
    );
}

/// Shutdown waits for a hold whose caller is gone, until its transaction has ended.
#[tokio::test(start_paused = true)]
async fn close_and_wait_waits_for_an_abandoned_transaction() {
    let (reconciler, release) = Transactional::new();
    let harness = Harness::new(FakeDocker::default(), Arc::clone(&reconciler));

    timed_out_caller(&harness).await;

    let coordinator = Arc::clone(&harness.coordinator);
    let closing = tokio::spawn(async move { coordinator.close_and_wait().await });
    tokio::task::yield_now().await;
    assert!(
        !closing.is_finished(),
        "shutdown did not wait for the transaction"
    );

    release.send(()).unwrap();
    closing.await.unwrap();
    assert_eq!(reconciler.journal()[..2], ["begin c1", "end c1"]);
    assert!(harness.idle());
}
