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

//! The single owner of reconciliation passes: Go's `coordinator`.
//!
//! Startup, `SIGHUP` (through [`Processor::publish_and_reconcile`]) and systemd reloads only
//! request a pass; one task ([`Coordinator::run`]) runs them, retries the containers that failed,
//! maintains the pending and reload-incomplete gauges and logs completion. Requests arriving while
//! a pass runs coalesce: the pass finishes its current container, then starts again under the
//! newest request, so no result is reported on behalf of an older one.
//!
//! It also adjudicates and applies container events: the event consumer only submits them
//! ([`Coordinator::handle_event`]). Every reconcile or cleanup of a container first reserves it,
//! so work requested for a container that is already being handled coalesces into one follow-up
//! instead of running twice; a start event not newer than the running apply is already covered by
//! it. The coordinator sweeps the processor's lifecycle history periodically, and owns the
//! processed map that skips start events an enumeration already covered.
//!
//! The work for a reserved container (a hold) runs on a task of its own, which the caller awaits.
//! A caller whose future is dropped (a timeout around it, a cancelled event loop) therefore ends
//! neither the work nor the reservation: the hold awaits each blocking apply transaction to its
//! end, runs what was queued for the container meanwhile, and only then releases it. Go's caller
//! cannot be dropped mid-call, so this is how the same guarantee is kept.
//! [`Coordinator::close_and_wait`] waits for every hold and transaction at shutdown.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use jiff::Timestamp;
use tokio::sync::Notify;
use tokio::time::{Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use super::{MAX_BACKOFF, MIN_BACKOFF};
use crate::clock::Clock;
use crate::docker::types::{ContainerSummary, Event};
use crate::docker::{self, CallContext, DockerApi, with_context};
use crate::logger::{Attr, LogHandle};
use crate::metrics::Recorder;
use crate::processor::{self, Inspector, PassRequester, Processor};
use crate::task::finish;

/// How long an enumerated container's entry suppresses a start event that is not newer than its
/// inspect. After twice [`MAX_BACKOFF`] the overlap window between enumeration and the event
/// stream has certainly passed; a remaining entry belongs to a container that exited before its
/// event was consumed.
const PROCESSED_TTL: Duration = Duration::from_mins(1);

/// How often the lifecycle history is re-checked for runs whose cgroup is gone.
const SWEEP_INTERVAL: Duration = Duration::from_mins(1);

/// The event actions that end a run; every other action of the filtered stream (`start`,
/// `unpause`) reconciles the container.
const ACTION_DIE: &str = "die";
const ACTION_DESTROY: &str = "destroy";

/// The messages the reconcile of a container logs its failure with, by what asked for it.
const MSG_EVENT_FAILED: &str = "could not process container";
const MSG_RUNNING_FAILED: &str = "could not process running container";
const MSG_PENDING_FAILED: &str = "could not reconcile pending container";

/// The Docker call a pass makes: moby's `ContainerList` of the running containers.
pub trait ContainerLister: Send + Sync + 'static {
    /// The running containers.
    fn container_list(
        &self,
    ) -> impl Future<Output = Result<Vec<ContainerSummary>, docker::Error>> + Send;
}

impl<Api: DockerApi + 'static> ContainerLister for Api {
    fn container_list(
        &self,
    ) -> impl Future<Output = Result<Vec<ContainerSummary>, docker::Error>> + Send {
        DockerApi::container_list(self)
    }
}

/// What the coordinator runs for a container: the processor's reconcile, termination and sweep
/// (Go's `applyFn`, `Processor.Terminate` and `Processor.Sweep`).
pub trait Reconciler: Send + Sync + 'static {
    /// Why a reconcile or a termination failed.
    type Error: fmt::Display + Send + 'static;

    /// Reconciles the container under `context`.
    fn reconcile(
        &self,
        context: &CallContext,
        container_id: &str,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Cleans up after a run of the container that ended at `event_time`.
    fn terminate(
        &self,
        container_id: &str,
        event_time: Timestamp,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Re-checks the lifecycle history; returns how many records it released.
    fn sweep(&self) -> impl Future<Output = usize> + Send;

    /// Waits for every transaction started so far, those whose caller is gone included.
    fn close_and_wait(&self) -> impl Future<Output = ()> + Send;

    /// Whether the container is gone from Docker, so its retries end (Go's
    /// `errors.Is(err, processor.ErrContainerGone)`).
    fn is_container_gone(error: &Self::Error) -> bool;
}

impl<I: Inspector> Reconciler for Arc<Processor<I>> {
    type Error = processor::Error;

    fn reconcile(
        &self,
        context: &CallContext,
        container_id: &str,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        Processor::reconcile(self, context, container_id)
    }

    fn terminate(
        &self,
        container_id: &str,
        event_time: Timestamp,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        Processor::terminate(self, container_id, event_time)
    }

    fn sweep(&self) -> impl Future<Output = usize> + Send {
        Processor::sweep(self)
    }

    fn close_and_wait(&self) -> impl Future<Output = ()> + Send {
        Processor::close_and_wait(self)
    }

    fn is_container_gone(error: &Self::Error) -> bool {
        error.is_container_gone()
    }
}

/// What a coordinator is built from.
pub struct Settings<D, R> {
    pub docker: D,
    pub reconciler: R,
    pub metrics: Recorder,
    /// Bounds the container list and each container's reconcile ([`super::DOCKER_CALL_TIMEOUT`]
    /// outside tests).
    pub timeout: Duration,
    pub clock: Arc<dyn Clock>,
    pub log: LogHandle,
}

/// What a pass reconciles for: a config generation and a trigger epoch. Every request bumps the
/// epoch, so two systemd reloads under one generation are still two passes (the second can wipe
/// what the first restored).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PassKey {
    generation: u64,
    epoch: u64,
}

/// A container whose last reconcile failed.
#[derive(Debug)]
struct PendingEntry {
    /// The request the entry must be reconciled for; it is re-tagged with every newer request,
    /// so an older result cannot clear it.
    key: PassKey,
    backoff: Duration,
    /// Measured on the monotonic clock, as Go's `due.Sub(now)` is, so a wall-clock step never
    /// delays or hurries a retry.
    due: Instant,
}

/// When an enumeration inspected a container: the wall time its start events are compared with,
/// and the monotonic time its entry ages by (Go's `time.Time` carries both).
#[derive(Debug, Clone, Copy)]
struct Inspected {
    at: Timestamp,
    monotonic: Instant,
}

/// How a pass ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PassOutcome {
    Done,
    ListFailed,
    Superseded,
}

/// Marks a container as being handled and collects the work requested for it meanwhile.
#[derive(Debug, Default)]
struct Reservation {
    /// When the current reconcile began; a start event not newer than it is covered by that
    /// reconcile. `None` while the holder only cleans up, which covers no start event.
    apply_start: Option<Timestamp>,
    /// Set while the holder's work includes a reconcile, whose outcome completion must wait for.
    running: bool,
    /// Asks for another reconcile after the current work.
    reconcile: bool,
    /// The latest end-of-run event received meanwhile.
    terminate_at: Option<Timestamp>,
}

/// What the coordinator lock guards.
#[derive(Debug, Default)]
struct State {
    /// The newest request.
    latest: PassKey,
    /// No pass has started for `latest`.
    dirty: bool,
    /// Set from a request until a pass for the latest request has visited every listed
    /// container.
    incomplete: bool,
    /// The request whose completion was last logged.
    announced: PassKey,
    pending: BTreeMap<String, PendingEntry>,
    processed: BTreeMap<String, Inspected>,
    /// A reservation for every container being handled.
    in_flight: BTreeMap<String, Reservation>,
}

/// The single owner of reconciliation passes, retries and container events (see the module
/// comment).
pub struct Coordinator<D, R> {
    docker: D,
    reconciler: R,
    metrics: Recorder,
    timeout: Duration,
    clock: Arc<dyn Clock>,
    log: LogHandle,
    /// Go's one-slot wake channel: a notification while nothing waits is kept as one permit, so
    /// a request never blocks and never gets lost.
    wake: Notify,
    /// Cancelled after the first pass attempt, so the event loop starts consuming events after
    /// the enumeration.
    first_pass: CancellationToken,
    /// Every hold, so shutdown can wait for those whose caller is gone.
    holds: TaskTracker,
    state: Mutex<State>,
}

impl<D: ContainerLister, R: Reconciler> Coordinator<D, R> {
    /// A coordinator with no request yet.
    #[must_use]
    pub fn new(settings: Settings<D, R>) -> Arc<Self> {
        Arc::new(Self {
            docker: settings.docker,
            reconciler: settings.reconciler,
            metrics: settings.metrics,
            timeout: settings.timeout,
            clock: settings.clock,
            log: settings.log,
            wake: Notify::new(),
            first_pass: CancellationToken::new(),
            holds: TaskTracker::new(),
            state: Mutex::new(State::default()),
        })
    }

    /// Asks for a pass under `generation` (or the newest generation already requested, whichever
    /// is higher). It never blocks.
    pub fn request(&self, generation: u64) {
        {
            let mut state = self.lock();
            state.latest.epoch = state.latest.epoch.wrapping_add(1);
            state.latest.generation = state.latest.generation.max(generation);
            state.dirty = true;
            state.incomplete = true;
            let latest = state.latest;
            for entry in state.pending.values_mut() {
                entry.key = latest;
            }
            self.metrics.set_reload_incomplete(true);
        }
        self.wake.notify_one();
    }

    /// Asks for a pass under the newest generation already requested; the event loop calls it
    /// after re-subscribing.
    pub fn request_reenumeration(&self) {
        self.request(0);
    }

    /// [`Coordinator::request`] as the processor's [`PassRequester`]. It holds the coordinator
    /// weakly, so the processor it is installed in does not keep its own coordinator alive.
    #[must_use]
    pub fn pass_requester(self: &Arc<Self>) -> PassRequester {
        let coordinator: Weak<Self> = Arc::downgrade(self);
        Arc::new(move |_context: &CallContext, generation: u64| {
            if let Some(coordinator) = coordinator.upgrade() {
                coordinator.request(generation);
            }
        })
    }

    /// Resolves once the first pass has ended (completed, failed to list, or been superseded) or
    /// `context` is done.
    pub async fn await_first_pass(&self, context: &CallContext) {
        tokio::select! {
            () = self.first_pass.cancelled() => {}
            _ = context.done() => {}
        }
    }

    /// Executes passes and retries until `context` is done.
    pub async fn run(self: &Arc<Self>, context: &CallContext) {
        let mut list_backoff = MIN_BACKOFF;
        let mut list_retry: Option<Instant> = None;
        let mut prune: Option<Instant> = None;
        // A ticker's first tick is one interval away, and a missed tick is dropped, as Go's is.
        let mut sweep = tokio::time::interval_at(Instant::now() + SWEEP_INTERVAL, SWEEP_INTERVAL);
        sweep.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            if context.err().is_some() {
                return;
            }

            if let Some(key) = self.take_request() {
                match self.pass(context, key).await {
                    PassOutcome::ListFailed => {
                        list_retry = Some(Instant::now() + list_backoff);
                        list_backoff = (list_backoff * 2).min(MAX_BACKOFF);
                    }
                    PassOutcome::Done => {
                        list_retry = None;
                        list_backoff = MIN_BACKOFF;
                    }
                    PassOutcome::Superseded => {}
                }
                continue;
            }

            let retry = self.next_due();
            prune = self.arm_prune(prune);

            tokio::select! {
                _ = context.done() => return,
                () = self.wake.notified() => {}
                () = sleep_until(list_retry) => {
                    list_retry = None;
                    self.redo();
                }
                () = sleep_until(retry) => self.retry_due(context).await,
                () = sleep_until(prune) => {
                    prune = None;
                    self.prune_processed();
                }
                _ = sweep.tick() => self.run_sweep().await,
            }
        }
    }

    /// Adjudicates one container event from the stream: `start` and `unpause` reconcile the
    /// container unless an enumeration already covered the event; `die` and `destroy` end a run
    /// and clean up after it.
    pub async fn handle_event(self: &Arc<Self>, context: &CallContext, event: &Event) {
        let event_time = event_time(event.time_nano);
        let container_id = event.actor.id.as_str();

        match event.action.as_str() {
            ACTION_DIE | ACTION_DESTROY => {
                self.terminate_container(context, container_id, event_time)
                    .await;
            }
            // The stream is filtered to four actions; any other is reconciled, which computes
            // the container's set under the policy and never grants beyond it.
            _ => {
                if self.skip_event(container_id, event_time) {
                    self.log.debug(
                        "event already covered by a pass; skipped",
                        &[
                            Attr::string("id", container_id),
                            Attr::string("action", event.action.as_str()),
                        ],
                    );
                    return;
                }

                let failed = self
                    .reconcile_container(context, container_id, Some(event_time), MSG_EVENT_FAILED)
                    .await
                    .is_some();
                if failed {
                    // Wake run so the new pending entry's retry is scheduled.
                    self.wake.notify_one();
                }
            }
        }
    }

    /// Waits for every hold started so far (and those started meanwhile), then for every
    /// transaction of the reconciler: what the daemon's shutdown waits for, within its budget,
    /// after [`Coordinator::run`] has returned.
    pub async fn close_and_wait(&self) {
        self.holds.close();
        self.holds.wait().await;
        self.reconciler.close_and_wait().await;
    }

    /// The coordinator state. A panic while it is held aborts the process in release builds
    /// (`panic = "abort"`), so a poisoned lock is only reachable in tests, where the state is
    /// still the last consistent one.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The latest request if no pass has started for it.
    fn take_request(&self) -> Option<PassKey> {
        let mut state = self.lock();
        if !state.dirty {
            return None;
        }
        state.dirty = false;
        Some(state.latest)
    }

    /// Marks the latest request as needing a pass again (its list failed).
    fn redo(&self) {
        self.lock().dirty = true;
    }

    /// Lists the running containers and reconciles each under `key`.
    async fn pass(self: &Arc<Self>, context: &CallContext, key: PassKey) -> PassOutcome {
        let outcome = self.list_and_reconcile(context, key).await;
        self.mark_first_pass_ended();
        outcome
    }

    async fn list_and_reconcile(
        self: &Arc<Self>,
        context: &CallContext,
        key: PassKey,
    ) -> PassOutcome {
        let listed = with_context(
            &context.with_timeout(self.timeout),
            self.docker.container_list(),
        )
        .await;
        let containers = match listed {
            Ok(containers) => containers,
            Err(error) => {
                self.log.warn(
                    "could not list containers; reconciliation incomplete, retrying",
                    &[
                        Attr::error("err", &format_args!("list containers: {error}")),
                        Attr::uint("generation", key.generation),
                    ],
                );
                self.warn_incomplete();
                return PassOutcome::ListFailed;
            }
        };

        self.log.debug(
            "reconciling running containers",
            &[
                Attr::int("count", count(containers.len())),
                Attr::uint("generation", key.generation),
                Attr::uint("epoch", key.epoch),
            ],
        );

        for container in &containers {
            if context.err().is_some() || self.superseded(key) {
                return PassOutcome::Superseded;
            }
            let _ = self
                .reconcile_container(context, &container.id, None, MSG_RUNNING_FAILED)
                .await;
        }

        self.finish_pass(key);
        PassOutcome::Done
    }

    /// Reconciles every pending container whose backoff has elapsed.
    async fn retry_due(self: &Arc<Self>, context: &CallContext) {
        for container_id in self.due_ids() {
            if context.err().is_some() {
                return;
            }
            let _ = self
                .reconcile_container(context, &container_id, None, MSG_PENDING_FAILED)
                .await;
        }
        self.report_locked(&mut self.lock());
    }

    /// Reconciles one container unless it is already being handled, in which case the work is
    /// left to the holder (see `reserve`). Returns the error of the last reconcile it ran.
    async fn reconcile_container(
        self: &Arc<Self>,
        context: &CallContext,
        container_id: &str,
        event_time: Option<Timestamp>,
        message: &'static str,
    ) -> Option<R::Error> {
        if !self.reserve(container_id, event_time, false) {
            return None;
        }
        // Only an enumeration (no event) marks the container processed: its inspect is what
        // covers start events that are delivered later.
        let work = Work {
            reconcile: true,
            terminate_at: None,
            enumerated: event_time.is_none(),
            message,
        };
        self.spawn_hold(context, container_id, work).await
    }

    /// Cleans up after a run that ended at `event_time`.
    async fn terminate_container(
        self: &Arc<Self>,
        context: &CallContext,
        container_id: &str,
        event_time: Timestamp,
    ) {
        if !self.reserve(container_id, Some(event_time), true) {
            return;
        }
        let work = Work {
            reconcile: false,
            terminate_at: Some(event_time),
            enumerated: false,
            message: MSG_EVENT_FAILED,
        };
        let _ = self.spawn_hold(context, container_id, work).await;
    }

    /// Runs `hold` on a task of its own and awaits it: dropping the caller leaves the hold
    /// running to the end, so the reservation is released only after the last transaction.
    async fn spawn_hold(
        self: &Arc<Self>,
        context: &CallContext,
        container_id: &str,
        work: Work,
    ) -> Option<R::Error> {
        let this = Arc::clone(self);
        let context = context.clone();
        let container_id = container_id.to_owned();
        let hold = self
            .holds
            .spawn(async move { this.hold(&context, &container_id, work).await });
        finish(hold).await.flatten()
    }

    /// Does the work for a reserved container, then whatever was requested for it meanwhile,
    /// until nothing is left; it then releases the reservation. A cleanup runs before a
    /// reconcile, so the reconcile sees the final state.
    async fn hold(
        &self,
        context: &CallContext,
        container_id: &str,
        work: Work,
    ) -> Option<R::Error> {
        let Work {
            mut reconcile,
            mut terminate_at,
            enumerated,
            message,
        } = work;
        let mut last_error = None;

        loop {
            if let Some(event_time) = terminate_at
                && let Err(error) = self.reconciler.terminate(container_id, event_time).await
            {
                self.log.warn(
                    "could not clean up after container",
                    &[Attr::string("id", container_id), Attr::error("err", &error)],
                );
            }

            if reconcile {
                let key = self.current();
                let started = Inspected {
                    at: self.clock.now(),
                    monotonic: Instant::now(),
                };
                last_error = self.process_one(context, container_id, message).await.err();
                self.settle(container_id, key, last_error.as_ref());
                if last_error.is_none() && enumerated {
                    self.mark_processed(container_id, started);
                }
            }

            (reconcile, terminate_at) = self.next(container_id);
            if !reconcile && terminate_at.is_none() {
                return last_error;
            }
        }
    }

    /// Applies device rules to one container and records the outcome: the rules-applied result,
    /// the apply duration, the last-event time on success, and a warning with `message` on
    /// failure. The enumeration, the retries and the event stream all go through it, so they
    /// report identically. The reconcile runs under a per-container timeout, so a hung Docker
    /// call made while processing (inspect) cannot stall what is queued behind it.
    async fn process_one(
        &self,
        context: &CallContext,
        container_id: &str,
        message: &str,
    ) -> Result<(), R::Error> {
        let start = Instant::now();
        let result = self
            .reconciler
            .reconcile(&context.with_timeout(self.timeout), container_id)
            .await;
        match &result {
            Ok(()) => {
                self.metrics.record_rule_applied(true);
                self.metrics.set_last_event(self.clock.now());
            }
            Err(error) => {
                self.log.warn(
                    message,
                    &[Attr::string("id", container_id), Attr::error("err", error)],
                );
                self.metrics.record_rule_applied(false);
            }
        }
        self.metrics.observe_apply_duration(start.elapsed());
        result
    }

    /// Claims the container for the caller and reports whether it got it. If the container is
    /// already being handled, the request is left to the holder: an end-of-run event is queued; a
    /// reconcile is queued unless it is a start event not newer than the running apply, which
    /// that apply already covers.
    fn reserve(&self, container_id: &str, event_time: Option<Timestamp>, terminal: bool) -> bool {
        let now = self.clock.now();
        let mut state = self.lock();

        let Some(held) = state.in_flight.get_mut(container_id) else {
            state.in_flight.insert(
                container_id.to_owned(),
                Reservation {
                    apply_start: (!terminal).then_some(now),
                    running: !terminal,
                    ..Reservation::default()
                },
            );
            return true;
        };

        if terminal {
            if let Some(event_time) = event_time
                && held.terminate_at.is_none_or(|queued| event_time > queued)
            {
                held.terminate_at = Some(event_time);
            }
        } else if event_time
            .zip(held.apply_start)
            .is_none_or(|(event_time, apply_start)| event_time > apply_start)
        {
            held.reconcile = true;
        }
        false
    }

    /// The work queued for a reserved container, or the release of the reservation when there
    /// is none.
    fn next(&self, container_id: &str) -> (bool, Option<Timestamp>) {
        let now = self.clock.now();
        let mut state = self.lock();

        let Some(held) = state.in_flight.get_mut(container_id) else {
            return (false, None);
        };
        if held.reconcile || held.terminate_at.is_some() {
            let reconcile = held.reconcile;
            let terminate_at = held.terminate_at.take();
            held.reconcile = false;
            held.running = reconcile;
            held.apply_start = reconcile.then_some(now);
            return (reconcile, terminate_at);
        }

        state.in_flight.remove(container_id);
        // Its retries were held back while the container was reserved.
        if state.pending.contains_key(container_id) {
            self.wake.notify_one();
        }
        self.announce_locked(&mut state);
        (false, None)
    }

    /// Re-checks the lifecycle history.
    async fn run_sweep(&self) {
        let released = self.reconciler.sweep().await;
        if released > 0 {
            self.log.debug(
                "lifecycle sweep released records",
                &[Attr::int("released", count(released))],
            );
        }
    }

    /// Records one reconcile that started under `key`. Success clears the container's pending
    /// entry only if no newer request re-tagged it since; a container Docker no longer knows is
    /// dropped; any other failure is (re)scheduled with backoff.
    fn settle(&self, container_id: &str, key: PassKey, error: Option<&R::Error>) {
        let now = Instant::now();
        let mut state = self.lock();
        let latest = state.latest;
        let settled = error.is_none_or(R::is_container_gone);

        match state.pending.get_mut(container_id) {
            Some(entry) if settled && entry.key == key => {
                state.pending.remove(container_id);
            }
            Some(entry) if settled => entry.due = now,
            Some(entry) => {
                entry.backoff = (entry.backoff * 2).min(MAX_BACKOFF);
                entry.due = now + entry.backoff;
            }
            None if settled => {}
            None => {
                state.pending.insert(
                    container_id.to_owned(),
                    PendingEntry {
                        key: latest,
                        backoff: MIN_BACKOFF,
                        due: now + MIN_BACKOFF,
                    },
                );
            }
        }

        self.metrics.set_pending_containers(state.pending.len());
    }

    /// Records that a pass for `key` visited every listed container.
    fn finish_pass(&self, key: PassKey) {
        let mut state = self.lock();
        if key == state.latest {
            state.incomplete = false;
            self.metrics.set_reload_incomplete(false);
        }
        self.report_locked(&mut state);
    }

    /// Logs completion once per request when nothing is left for the latest request, and warns
    /// while anything is.
    fn report_locked(&self, state: &mut State) {
        if state.incomplete || !state.pending.is_empty() {
            self.log.warn(
                "reconciliation incomplete; retrying",
                &[
                    Attr::int("pending_containers", count(state.pending.len())),
                    Attr::bool("reload_incomplete", state.incomplete),
                    Attr::uint("generation", state.latest.generation),
                ],
            );
            return;
        }
        self.announce_locked(state);
    }

    /// Logs completion once per request, when nothing is left for the latest request: no pass
    /// outstanding, nothing pending, and no follow-up queued behind a reservation.
    fn announce_locked(&self, state: &mut State) {
        if state.incomplete || !state.pending.is_empty() || state.announced == state.latest {
            return;
        }
        if state
            .in_flight
            .values()
            .any(|held| held.reconcile || held.running)
        {
            return;
        }
        state.announced = state.latest;
        self.log.info(
            "config reload complete",
            &[
                Attr::uint("generation", state.latest.generation),
                Attr::uint("epoch", state.latest.epoch),
            ],
        );
    }

    fn warn_incomplete(&self) {
        self.report_locked(&mut self.lock());
    }

    fn superseded(&self, key: PassKey) -> bool {
        self.lock().latest != key
    }

    fn current(&self) -> PassKey {
        self.lock().latest
    }

    /// When the next pending container is due. A reserved container is retried by its holder's
    /// settle, or once the reservation ends (`next` wakes `run`).
    fn next_due(&self) -> Option<Instant> {
        let state = self.lock();
        state
            .pending
            .iter()
            .filter(|(container_id, _)| !state.in_flight.contains_key(*container_id))
            .map(|(_, entry)| entry.due)
            .min()
    }

    /// The pending containers that are due and not reserved.
    fn due_ids(&self) -> Vec<String> {
        let now = Instant::now();
        let state = self.lock();
        state
            .pending
            .iter()
            .filter(|(container_id, entry)| {
                !state.in_flight.contains_key(*container_id) && entry.due <= now
            })
            .map(|(container_id, _)| container_id.clone())
            .collect()
    }

    fn mark_first_pass_ended(&self) {
        self.first_pass.cancel();
    }

    /// Records that the container was reconciled from an inspect made at `inspected_at`.
    fn mark_processed(&self, container_id: &str, inspected_at: Inspected) {
        self.lock()
            .processed
            .insert(container_id.to_owned(), inspected_at);
    }

    /// Whether an event for the container at `event_time` is already covered by an enumeration:
    /// Docker emits `start` after the container is running, so an event not newer than the
    /// inspect was visible to it. The entry is consumed by the first event for the container
    /// either way, and ignored once older than [`PROCESSED_TTL`].
    fn skip_event(&self, container_id: &str, event_time: Timestamp) -> bool {
        let Some(inspected) = self.lock().processed.remove(container_id) else {
            return false;
        };
        if expired(Instant::now(), inspected) {
            return false;
        }
        event_time <= inspected.at
    }

    /// The prune deadline to wait for: the armed one if there is one, a new one when the
    /// processed map has entries, or none, which never fires. Entries only come from a pass and
    /// expire [`PROCESSED_TTL`] later, so on a quiet node the timer fires once or twice after a
    /// pass and then stays off, instead of waking the daemon every [`PROCESSED_TTL`] for an empty
    /// map.
    fn arm_prune(&self, armed: Option<Instant>) -> Option<Instant> {
        if armed.is_some() {
            return armed;
        }
        if self.lock().processed.is_empty() {
            return None;
        }
        Some(Instant::now() + PROCESSED_TTL)
    }

    fn prune_processed(&self) {
        let now = Instant::now();
        self.lock()
            .processed
            .retain(|_, inspected| !expired(now, *inspected));
    }
}

/// What a hold does first, before whatever is queued for the container meanwhile.
#[derive(Debug, Clone, Copy)]
struct Work {
    reconcile: bool,
    terminate_at: Option<Timestamp>,
    /// The reconcile comes from an enumeration, whose inspect covers later start events.
    enumerated: bool,
    message: &'static str,
}

/// Go's `time.Unix(0, timeNano)`. Every `i64` of nanoseconds is within jiff's range, so the
/// fallback is never taken.
fn event_time(time_nano: i64) -> Timestamp {
    Timestamp::from_nanosecond(i128::from(time_nano)).unwrap_or(Timestamp::UNIX_EPOCH)
}

/// Whether an entry is older than [`PROCESSED_TTL`] at `now`, on the monotonic clock.
fn expired(now: Instant, inspected: Inspected) -> bool {
    now.saturating_duration_since(inspected.monotonic) > PROCESSED_TTL
}

/// A count as Go's `int` logs it.
fn count(len: usize) -> i64 {
    i64::try_from(len).unwrap_or(i64::MAX)
}

/// Sleeps until `deadline`, or forever without one: Go's nil timer channel in a select.
async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
pub(super) mod testing;
#[cfg(test)]
mod tests;
