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

//! The Docker event loop: Go's `listenEvents`, `subscribe` and `consumeEvents`.
//!
//! The reconnect structure follows swarm-scheduler-exporter's listener
//! (`src/collector/reconciler/listener.rs` at `d57dcc220d29`, patterns only: a loop that opens a
//! stream, drains it under the cancellation and backs off before the next one), with this
//! project's semantics: a ping and an establishment timeout before each subscription, a
//! reconnect from one nanosecond after the last event, a backoff from one to thirty seconds that
//! an event resets, the readiness flag, a pass after every re-subscription, and every event
//! handled before the next is read.
//!
//! moby's `Events` reports a refused connect on the stream's error channel, after the call has
//! returned, so Go treated it as a subscription that failed at once: logged as subscribed, then
//! as a stream error. [`Subscription::Refused`] keeps that order.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;

use super::coordinator::{ContainerLister, Coordinator, Reconciler};
use super::{MAX_BACKOFF, MIN_BACKOFF, Readiness};
use crate::docker::types::Event;
use crate::docker::{
    self, CallContext, ContextError, DockerApi, EventStream, Filters, StreamError, StreamErrorKind,
    with_context,
};
use crate::logger::{Attr, LogHandle};
use crate::metrics::Recorder;

/// The event actions subscribed to: `start` and `unpause` reconcile a container, `die` and
/// `destroy` end a run and clean up after it.
const EVENT_ACTIONS: [&str; 4] = ["start", "unpause", "die", "destroy"];

/// Go's `eventListOptions` filter.
pub(super) fn event_filters() -> Filters {
    Filters::new().add("event", &EVENT_ACTIONS)
}

/// The `since` of a re-subscription: one nanosecond after the last event received, so neither
/// the events emitted during the disconnect nor the last event itself are replayed (a replayed
/// event would re-apply rules, and cgroup v2 programs grow on every apply). Before any event has
/// been received it is the initial `since`.
pub(super) fn resubscribe_since(initial: Timestamp, last_event_nano: i64) -> Timestamp {
    if last_event_nano == 0 {
        return initial;
    }
    // Every i64 of nanoseconds plus one is within jiff's range.
    Timestamp::from_nanosecond(i128::from(last_event_nano) + 1).unwrap_or(initial)
}

/// The backoff after `current`: doubled, capped at [`MAX_BACKOFF`].
pub(super) fn next_backoff(current: Duration) -> Duration {
    current.saturating_mul(2).min(MAX_BACKOFF)
}

/// Sleeps for `duration`, or until `context` is done.
pub(super) async fn sleep_ctx(context: &CallContext, duration: Duration) {
    tokio::select! {
        _ = context.done() => {}
        () = tokio::time::sleep(duration) => {}
    }
}

/// An event stream as Go's `client.EventsResult` delivered it.
#[derive(Debug)]
pub(super) enum Subscription<S> {
    Open(S),
    /// The connect failed; the stream's first and only item is the error.
    Refused(docker::Error),
}

/// Why a stream ended.
#[derive(Debug)]
pub(super) enum StreamEnd {
    Stream(StreamError),
    Refused(docker::Error),
}

impl StreamEnd {
    /// The source of the events went away without an error: Go's closed message channel.
    fn is_closed(&self) -> bool {
        matches!(self, Self::Stream(error) if error.kind() == StreamErrorKind::Closed)
    }
}

impl fmt::Display for StreamEnd {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stream(error) => error.fmt(formatter),
            Self::Refused(error) => error.fmt(formatter),
        }
    }
}

impl<S: EventStream> Subscription<S> {
    async fn next(&mut self) -> Result<Event, StreamEnd> {
        match self {
            Self::Open(stream) => stream.next().await.map_err(StreamEnd::Stream),
            Self::Refused(error) => Err(StreamEnd::Refused(error.clone())),
        }
    }
}

/// Why a subscription could not be made.
#[derive(Debug)]
pub(super) enum SubscribeError {
    /// `ping docker: <cause>`.
    Ping(docker::Error),
    /// Go's `errSubscribeTimeout`.
    Timeout,
    /// `subscribe to docker events: <context error>`.
    Canceled(ContextError),
}

impl fmt::Display for SubscribeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ping(error) => write!(formatter, "ping docker: {error}"),
            Self::Timeout => {
                formatter.write_str("docker event stream not established before timeout")
            }
            Self::Canceled(why) => write!(formatter, "subscribe to docker events: {}", why.text()),
        }
    }
}

impl std::error::Error for SubscribeError {}

/// Opens the Docker event stream with a bounded establishment.
///
/// The `events` call does not resolve until dockerd has sent the response headers, so a hung
/// dockerd would block the caller with nothing able to act as a watchdog, and a deadline on the
/// stream itself would also cut it once established. The call is therefore raced against a timer
/// here and dropped, which ends the attempt, when the timer or `context` wins; the stream it
/// returns has no deadline.
pub(super) async fn subscribe<D: DockerApi>(
    context: &CallContext,
    docker: &D,
    since: Timestamp,
    timeout: Duration,
) -> Result<Subscription<D::Events>, SubscribeError> {
    with_context(&context.with_timeout(timeout), docker.ping())
        .await
        .map_err(SubscribeError::Ping)?;

    let filters = event_filters();
    tokio::select! {
        established = docker.events(since, &filters) => Ok(match established {
            Ok(stream) => Subscription::Open(stream),
            Err(error) => Subscription::Refused(error),
        }),
        () = tokio::time::sleep(timeout) => Err(SubscribeError::Timeout),
        why = context.done() => Err(SubscribeError::Canceled(why)),
    }
}

/// What the event loop works with.
pub(super) struct Listener<'run, D, L, R> {
    pub(super) docker: &'run D,
    pub(super) coordinator: &'run Arc<Coordinator<L, R>>,
    pub(super) metrics: &'run Recorder,
    pub(super) readiness: &'run Readiness,
    pub(super) log: &'run LogHandle,
    pub(super) timeout: Duration,
}

impl<D: DockerApi, L: ContainerLister, R: Reconciler> Listener<'_, D, L, R> {
    /// Consumes Docker container events until `context` is done. `stream` is the subscription Run
    /// opened before the startup enumeration (none when that failed); every later subscription,
    /// after the stream ended or failed, requests a coordinator pass. A failure reconnects with
    /// capped exponential backoff rather than ending the daemon. The readiness flag follows the
    /// stream's health.
    pub(super) async fn listen_events(
        &self,
        context: &CallContext,
        since: Timestamp,
        mut stream: Option<Subscription<D::Events>>,
    ) {
        let mut backoff = MIN_BACKOFF;
        let mut last_event_nano = 0;

        loop {
            if context.err().is_some() {
                return;
            }

            let mut current = if let Some(current) = stream.take() {
                current
            } else {
                let resubscribed = subscribe(
                    context,
                    self.docker,
                    resubscribe_since(since, last_event_nano),
                    self.timeout,
                )
                .await;
                match resubscribed {
                    Ok(current) => {
                        // since only replays what dockerd still buffers, and a restarted dockerd
                        // has nothing buffered, so events of the gap can be lost: re-list and
                        // reconcile every running container. The coordinator's processed map
                        // skips replayed start events the pass already covered.
                        self.log.info(
                            "docker event stream re-subscribed; reconciling running containers",
                            &[],
                        );
                        self.coordinator.request_reenumeration();
                        current
                    }
                    Err(error) => {
                        if context.err().is_some() {
                            return;
                        }
                        self.log.error(
                            "could not subscribe to docker events, retrying",
                            &[Attr::error("err", &error), backoff_attr(backoff)],
                        );
                        self.retry_after(context, &mut backoff).await;
                        continue;
                    }
                }
            };

            self.readiness.set(true);
            self.log.debug("subscribed to docker events", &[]);

            let disconnected = self
                .consume_events(context, &mut current, &mut backoff, &mut last_event_nano)
                .await;
            // The stream lives exactly as long as this subscription: a reconnect opens a new one
            // and shutdown must release the request.
            drop(current);
            if !disconnected {
                return;
            }
            self.readiness.set(false);
        }
    }

    /// Drains one subscription. Returns whether the caller should reconnect: not when `context`
    /// is done. `last_event_nano` follows the time of every event received.
    ///
    /// Each event is handed to the coordinator, and handled, before the next one is read: an
    /// event already covered by an enumeration is skipped, a newer one (restart, unpause) is
    /// applied, and a failed apply is left to the coordinator's retries.
    pub(super) async fn consume_events(
        &self,
        context: &CallContext,
        stream: &mut Subscription<D::Events>,
        backoff: &mut Duration,
        last_event_nano: &mut i64,
    ) -> bool {
        loop {
            let next = tokio::select! {
                _ = context.done() => return false,
                next = stream.next() => next,
            };
            match next {
                // Checked before the error itself: a stream that ends once the context is done
                // is a shutdown, not a stream failure.
                Err(_) if context.err().is_some() => return false,
                Err(end) if end.is_closed() => {
                    self.log.warn(
                        "docker events channel closed, reconnecting",
                        &[backoff_attr(*backoff)],
                    );
                    self.retry_after(context, backoff).await;
                    return context.err().is_none();
                }
                Err(end) => {
                    self.log.error(
                        "docker events stream error, reconnecting",
                        &[Attr::error("err", &end), backoff_attr(*backoff)],
                    );
                    self.retry_after(context, backoff).await;
                    return context.err().is_none();
                }
                Ok(event) => {
                    *backoff = MIN_BACKOFF;
                    *last_event_nano = (*last_event_nano).max(event.time_nano);
                    self.metrics.record_event(&event.action);
                    self.coordinator.handle_event(context, &event).await;
                }
            }
        }
    }

    /// Counts a reconnect, marks the daemon not ready and waits `backoff`, which then grows.
    async fn retry_after(&self, context: &CallContext, backoff: &mut Duration) {
        self.metrics.inc_docker_reconnect();
        self.readiness.set(false);
        sleep_ctx(context, *backoff).await;
        *backoff = next_backoff(*backoff);
    }
}

/// The `backoff` attribute, as Go's `time.Duration` logs it.
fn backoff_attr(backoff: Duration) -> Attr {
    Attr::duration(
        "backoff",
        i64::try_from(backoff.as_nanos()).unwrap_or(i64::MAX),
    )
}

#[cfg(test)]
mod tests;
