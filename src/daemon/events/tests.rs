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

//! The event-loop tests of `events_test.go`, `daemon_test.go` and `timeout_test.go`, with the
//! three tests of `coordinator_test.go` that drive the event loop, on the paused clock against
//! the fake Docker of `test_support`. Go's `cancelAfter` is the fake reconciler's
//! `cancel_after`, and Go's `consumeUntilSkipped` waits for the skip log with `eventually`.

use std::sync::Arc;
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::Instant;

use super::{
    Listener, Subscription, event_filters, next_backoff, resubscribe_since, sleep_ctx, subscribe,
};
use crate::config::Runtime;
use crate::daemon::coordinator::testing::{FakeReconciler, Harness, WITHIN, base, start_event};
use crate::daemon::{DOCKER_CALL_TIMEOUT, MAX_BACKOFF, MIN_BACKOFF, Readiness};
use crate::docker::types::{Actor, Event};
use crate::docker::{CallContext, ContextError, StreamError, StreamErrorKind};
use crate::policy::Global;
use crate::processor::{self, Processor};
use crate::test_support::eventually;
use crate::test_support::fake_docker::{FakeDocker, FakeEvent, FakeEvents};

/// A coordinator harness and the readiness flag the event loop sets.
struct Loop {
    harness: Harness,
    readiness: Readiness,
}

impl Loop {
    fn new(docker: FakeDocker, reconciler: FakeReconciler) -> Self {
        Self {
            harness: Harness::new(docker, Arc::new(reconciler)),
            readiness: Readiness::default(),
        }
    }

    /// With the coordinator running, as Go's `startCoordinator`.
    fn started(docker: FakeDocker, reconciler: FakeReconciler) -> Self {
        Self {
            harness: Harness::start(docker, Arc::new(reconciler)),
            readiness: Readiness::default(),
        }
    }

    fn listener(&self) -> Listener<'_, Arc<FakeDocker>, Arc<FakeDocker>, Arc<FakeReconciler>> {
        Listener {
            docker: &self.harness.docker,
            coordinator: &self.harness.coordinator,
            metrics: &self.harness.metrics,
            readiness: &self.readiness,
            log: &self.harness.log,
            timeout: DOCKER_CALL_TIMEOUT,
        }
    }

    /// Go's `consumeEvents` over `stream`, from `backoff`.
    async fn consume(
        &self,
        context: &CallContext,
        stream: FakeEvents,
        backoff: &mut Duration,
        last_event_nano: &mut i64,
    ) -> bool {
        let mut subscription = Subscription::Open(stream);
        self.listener()
            .consume_events(context, &mut subscription, backoff, last_event_nano)
            .await
    }

    /// [`Loop::consume`] from the first backoff, before any event.
    async fn consume_once(&self, context: &CallContext, stream: FakeEvents) -> bool {
        let mut backoff = MIN_BACKOFF;
        self.consume(context, stream, &mut backoff, &mut 0).await
    }

    fn reconnects(&self) -> f64 {
        self.harness.scalar("sda_docker_reconnects_total", &[])
    }
}

/// An event with only an actor and a time, as the Go tests send them (no action: a reconcile).
fn message(container_id: &str, time_nano: i64) -> Event {
    Event {
        actor: Actor {
            id: container_id.to_owned(),
            ..Actor::default()
        },
        time_nano,
        ..Event::default()
    }
}

fn nanos(at: Timestamp) -> i64 {
    i64::try_from(at.as_nanosecond()).unwrap()
}

fn send(sender: &UnboundedSender<FakeEvent>, event: Event) {
    sender.send(Ok(event)).unwrap();
}

/// Ends the stream with `error`.
fn fail(sender: &UnboundedSender<FakeEvent>, error: StreamError) {
    sender.send(Err(error)).unwrap();
}

/// Go: `TestNextBackoff_Doubles`
#[test]
fn next_backoff_doubles() {
    assert_eq!(next_backoff(Duration::from_secs(1)), Duration::from_secs(2));
}

/// Go: `TestNextBackoff_Caps`
#[test]
fn next_backoff_caps() {
    assert_eq!(next_backoff(MAX_BACKOFF), MAX_BACKOFF);
    assert_eq!(
        next_backoff(MAX_BACKOFF.checked_sub(Duration::from_secs(1)).unwrap()),
        MAX_BACKOFF
    );
}

/// Go: `TestSleepCtx_RespectsCancellation`
#[tokio::test(start_paused = true)]
async fn sleep_ctx_respects_cancellation() {
    let context = CallContext::new();
    context.cancel();
    let start = Instant::now();
    sleep_ctx(&context, Duration::from_secs(5)).await;
    assert_eq!(start.elapsed(), Duration::ZERO);
}

/// Go: `TestSleepCtx_WaitsForDuration`
#[tokio::test(start_paused = true)]
async fn sleep_ctx_waits_for_duration() {
    let start = Instant::now();
    sleep_ctx(&CallContext::new(), Duration::from_millis(50)).await;
    assert!(start.elapsed() >= Duration::from_millis(50));
}

/// Go: `TestConsumeEvents_ContextCancelledReturnsNoReconnect`
#[tokio::test(start_paused = true)]
async fn consume_events_context_cancelled_returns_no_reconnect() {
    let fixture = Loop::new(FakeDocker::default(), FakeReconciler::default());
    let context = CallContext::new();
    context.cancel();
    let (_sender, stream) = FakeEvents::channel();

    let reconnect = fixture.consume_once(&context, stream).await;
    assert!(!reconnect, "no reconnect once the context is canceled");
}

/// Go: `TestConsumeEvents_StreamErrorReturnsReconnect`
#[tokio::test(start_paused = true)]
async fn consume_events_stream_error_returns_reconnect() {
    let fixture = Loop::new(FakeDocker::default(), FakeReconciler::default());
    fixture.readiness.set(true);
    let (sender, stream) = FakeEvents::channel();
    fail(
        &sender,
        StreamError::new(StreamErrorKind::UnexpectedEof, "transport EOF"),
    );
    let mut backoff = MIN_BACKOFF;

    let reconnect = fixture
        .consume(&CallContext::new(), stream, &mut backoff, &mut 0)
        .await;
    assert!(reconnect, "a stream error reconnects");
    assert_eq!(backoff, next_backoff(MIN_BACKOFF));
    assert!(!fixture.readiness.is_ready());
    assert!((fixture.reconnects() - 1.0).abs() < f64::EPSILON);
    assert!(
        fixture.harness.logs().contains(
            "level=ERROR docker events stream error, reconnecting err=transport EOF backoff=1s"
        ),
        "{}",
        fixture.harness.logs()
    );
}

/// Go: `TestConsumeEvents_ContextErrFromStreamErrorNoReconnect`
#[tokio::test(start_paused = true)]
async fn consume_events_context_err_from_stream_error_no_reconnect() {
    let fixture = Loop::new(FakeDocker::default(), FakeReconciler::default());
    let (sender, stream) = FakeEvents::channel();
    fail(
        &sender,
        StreamError::new(StreamErrorKind::Decode, "context canceled"),
    );
    let context = CallContext::new();
    context.cancel();

    let reconnect = fixture.consume_once(&context, stream).await;
    assert!(
        !reconnect,
        "no reconnect when the stream ends with the context"
    );
}

/// Go: `TestConsumeEvents_ArbitraryStreamErrorAfterCancelNoReconnect`. A canceled stream can end
/// with a read error rather than the context's; it is a shutdown, not a stream failure. Go pinned
/// the select taking the error with a context whose `Done` never closes; here the stream error and
/// the cancellation are both ready, and the select picks either at random, so the case is run
/// until both orders have certainly been taken.
#[tokio::test(start_paused = true)]
async fn consume_events_arbitrary_stream_error_after_cancel_no_reconnect() {
    let fixture = Loop::new(FakeDocker::default(), FakeReconciler::default());
    for _ in 0..64 {
        let (sender, stream) = FakeEvents::channel();
        fail(
            &sender,
            StreamError::new(StreamErrorKind::UnexpectedEof, "transport EOF"),
        );
        let context = CallContext::new();
        context.cancel();
        let mut backoff = MIN_BACKOFF;

        let reconnect = fixture
            .consume(&context, stream, &mut backoff, &mut 0)
            .await;
        assert!(!reconnect);
        assert_eq!(
            backoff, MIN_BACKOFF,
            "a shutdown must not count as a failure"
        );
    }
    assert!(
        !fixture
            .harness
            .logs()
            .contains("docker events stream error"),
        "unexpected stream-error log at shutdown:\n{}",
        fixture.harness.logs()
    );
}

/// Go: `TestEventListOptions`. The `since` of a subscription is the one asked for, and the
/// filter is the four actions.
#[tokio::test(start_paused = true)]
async fn event_list_options() {
    assert_eq!(
        event_filters().to_json(),
        r#"{"event":{"destroy":true,"die":true,"start":true,"unpause":true}}"#
    );

    let docker = FakeDocker::default();
    let since = Timestamp::from_nanosecond(1_767_323_045_000_000_006).unwrap();
    let _stream = subscribe(&CallContext::new(), &docker, since, DOCKER_CALL_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(docker.sinces(), [since]);
}

/// Go: `TestConsumeEvents_ChannelCloseReturnsReconnect`
#[tokio::test(start_paused = true)]
async fn consume_events_channel_close_returns_reconnect() {
    let fixture = Loop::new(FakeDocker::default(), FakeReconciler::default());
    let (sender, stream) = FakeEvents::channel();
    drop(sender);

    let reconnect = fixture.consume_once(&CallContext::new(), stream).await;
    assert!(reconnect, "a closed channel reconnects");
    assert!(
        fixture
            .harness
            .logs()
            .contains("level=WARN docker events channel closed, reconnecting backoff=1s")
    );
}

/// Go: `TestConsumeEvents_EventCallsApply`
#[tokio::test(start_paused = true)]
async fn consume_events_event_calls_apply() {
    let context = CallContext::new();
    let fixture = Loop::new(
        FakeDocker::default(),
        FakeReconciler {
            cancel_after: Some((2, context.clone())),
            ..FakeReconciler::default()
        },
    );
    let (sender, stream) = FakeEvents::channel();
    send(&sender, message("container-1", 0));
    send(&sender, message("container-2", 0));

    fixture.consume_once(&context, stream).await;
    assert_eq!(fixture.harness.seen(), ["container-1", "container-2"]);
    assert!(
        (fixture.harness.scalar("sda_events_total", &[("event", "")]) - 2.0).abs() < f64::EPSILON
    );
}

/// Go: `TestConsumeEvents_DeduplicatesProcessedIDs`
#[tokio::test(start_paused = true)]
async fn consume_events_deduplicates_processed_ids() {
    let fixture = Loop::new(FakeDocker::default(), FakeReconciler::default());
    fixture
        .harness
        .mark_processed("already-seen", fixture.harness.now());
    let (sender, stream) = FakeEvents::channel();
    send(&sender, message("already-seen", 0));
    let context = CallContext::new();

    let consumed = fixture.consume_once(&context, stream);
    let skipped = async {
        eventually("the skip", WITHIN, || {
            fixture
                .harness
                .logs()
                .contains("event already covered by a pass; skipped id=already-seen")
        })
        .await;
        context.cancel();
    };
    tokio::join!(consumed, skipped);

    assert!(
        fixture.harness.seen().is_empty(),
        "a covered event was applied"
    );
    assert!(
        !fixture.harness.processed("already-seen"),
        "the entry is removed after the event"
    );
}

/// Go: `TestConsumeEvents_BackoffResetsOnSuccessfulEvent`
#[tokio::test(start_paused = true)]
async fn consume_events_backoff_resets_on_successful_event() {
    let context = CallContext::new();
    let fixture = Loop::new(
        FakeDocker::default(),
        FakeReconciler {
            cancel_after: Some((1, context.clone())),
            ..FakeReconciler::default()
        },
    );
    let (sender, stream) = FakeEvents::channel();
    send(&sender, message("c1", 0));
    let mut backoff = MAX_BACKOFF;

    fixture
        .consume(&context, stream, &mut backoff, &mut 0)
        .await;
    assert_eq!(backoff, MIN_BACKOFF);
}

/// Go: `TestConsumeEvents_RestartWithinWindow`. An event newer than the startup inspect is a new
/// run (restart, unpause) and is applied; an older one was already seen by the inspect and is
/// skipped. The entry is removed after either.
#[tokio::test(start_paused = true)]
async fn consume_events_restart_within_window() {
    for (name, offset, want) in [
        ("newer event applied", SignedDuration::from_secs(1), 1),
        ("older event skipped", SignedDuration::from_millis(-1), 0),
    ] {
        let context = CallContext::new();
        let fixture = Loop::new(
            FakeDocker::default(),
            FakeReconciler {
                cancel_after: Some((1, context.clone())),
                ..FakeReconciler::default()
            },
        );
        let recorded_at = fixture.harness.now();
        fixture.harness.mark_processed("c1", recorded_at);
        let (sender, stream) = FakeEvents::channel();
        send(
            &sender,
            message("c1", nanos(recorded_at.checked_add(offset).unwrap())),
        );

        let consumed = fixture.consume_once(&context, stream);
        let ended = async {
            // A positive case ends from inside the reconcile; a skipped event has none to end
            // it, so the skip log ends it.
            eventually(name, WITHIN, || {
                context.err().is_some()
                    || fixture
                        .harness
                        .logs()
                        .contains("event already covered by a pass; skipped")
            })
            .await;
            context.cancel();
        };
        tokio::join!(consumed, ended);

        assert_eq!(fixture.harness.seen().len(), want, "{name}");
        assert!(
            !fixture.harness.processed("c1"),
            "{name}: the entry is removed after the first event"
        );
    }
}

/// Go: `TestConsumeEvents_TracksLastEventNano`
#[tokio::test(start_paused = true)]
async fn consume_events_tracks_last_event_nano() {
    let context = CallContext::new();
    let fixture = Loop::new(
        FakeDocker::default(),
        FakeReconciler {
            cancel_after: Some((2, context.clone())),
            ..FakeReconciler::default()
        },
    );
    let (sender, stream) = FakeEvents::channel();
    send(&sender, message("c1", 200));
    send(&sender, message("c2", 300));
    let mut backoff = MIN_BACKOFF;
    let mut last_event_nano = 0;

    fixture
        .consume(&context, stream, &mut backoff, &mut last_event_nano)
        .await;
    assert_eq!(last_event_nano, 300);
}

/// Go: `TestResubscribeSince`
#[test]
fn resubscribe_since_after_the_last_event() {
    let initial: Timestamp = "2026-09-14T10:00:00Z".parse().unwrap();
    assert_eq!(resubscribe_since(initial, 0), initial);

    let last: Timestamp = "2026-09-14T10:00:05.123456789Z".parse().unwrap();
    assert_eq!(
        resubscribe_since(initial, nanos(last)),
        last.checked_add(SignedDuration::from_nanos(1)).unwrap()
    );
}

/// Go: `TestReconcile_HungInspectIsBoundedPerCall`. The processor's own per-call timeout bounds
/// the inspect when the caller's context has no deadline.
#[tokio::test(start_paused = true)]
async fn reconcile_hung_inspect_is_bounded_per_call() {
    let docker = Arc::new(FakeDocker {
        inspect_blocks: true,
        ..FakeDocker::default()
    });
    let (store, publisher) = crate::config::new_store(Runtime {
        dry_run: false,
        policy: Global::new("all", None, None).unwrap(),
    });
    let proc = Processor::new(processor::Settings {
        inspector: Arc::clone(&docker),
        store,
        publisher,
        metrics: crate::metrics::Recorder::noop(),
        host_root: "/host".to_owned(),
        proc_root: "/".to_owned(),
        call_timeout: Some(Duration::from_millis(50)),
        clock: Arc::new(crate::clock::TokioClock::new(base())),
        log: crate::test_support::capture_logs(crate::logger::Level::DEBUG).0,
    });

    let reconciled = tokio::time::timeout(
        Duration::from_secs(1),
        proc.reconcile(&CallContext::new(), "c1"),
    )
    .await;
    assert!(reconciled.is_ok(), "the hung inspect is not bounded");
    assert_eq!(docker.inspects_ended(), [ContextError::DeadlineExceeded]);
}

/// Go: `TestSubscribe_HungEventsTimesOutWithoutLeak`
#[tokio::test(start_paused = true)]
async fn subscribe_hung_events_times_out_without_leak() {
    let docker = FakeDocker {
        events_block: [1].into(),
        ..FakeDocker::default()
    };

    let subscribed = tokio::time::timeout(
        DOCKER_CALL_TIMEOUT * 20,
        subscribe(&CallContext::new(), &docker, base(), DOCKER_CALL_TIMEOUT),
    )
    .await
    .expect("the hung subscription is bounded");
    let error = subscribed.unwrap_err();
    assert_eq!(
        error.to_string(),
        "docker event stream not established before timeout"
    );
    // The attempt is dropped before subscribe returns, not eventually.
    assert_eq!(docker.events_calls(), (1, 1), "the attempt leaked");
}

/// Go: `TestSubscribe_SuccessKeepsStreamOpenUntilCancel`
#[tokio::test(start_paused = true)]
async fn subscribe_success_keeps_stream_open_until_cancel() {
    let docker = FakeDocker::default();
    let stream = subscribe(&CallContext::new(), &docker, base(), DOCKER_CALL_TIMEOUT)
        .await
        .unwrap();
    assert!(matches!(stream, Subscription::Open(_)));

    // Well past the establishment timeout, the stream must still be open: it does not inherit
    // that deadline.
    tokio::time::sleep(DOCKER_CALL_TIMEOUT * 2).await;
    assert_eq!(docker.open_streams(), 1);

    drop(stream);
    assert_eq!(
        docker.open_streams(),
        0,
        "dropping the stream did not end it"
    );
}

/// A ping that fails ends the subscription before any `events` call, with Go's wording; a
/// canceled context ends it with the context's.
#[tokio::test(start_paused = true)]
async fn subscribe_failures_are_worded_as_go() {
    let docker = FakeDocker {
        ping_err: Some(crate::docker::Error::connection_failed(
            "daemon down".to_owned(),
        )),
        ..FakeDocker::default()
    };
    let error = subscribe(&CallContext::new(), &docker, base(), DOCKER_CALL_TIMEOUT)
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "ping docker: daemon down");
    assert_eq!(docker.events_calls(), (0, 0));

    let docker = FakeDocker {
        events_block: [1].into(),
        ..FakeDocker::default()
    };
    let context = CallContext::new();
    let subscribing = subscribe(&context, &docker, base(), DOCKER_CALL_TIMEOUT);
    let canceling = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        context.cancel();
    };
    let (subscribed, ()) = tokio::join!(subscribing, canceling);
    assert_eq!(
        subscribed.unwrap_err().to_string(),
        "subscribe to docker events: context canceled"
    );
}

/// moby reports a refused connect on the stream: the subscription succeeds, and its first item
/// is the error, which reconnects as a stream error.
#[tokio::test(start_paused = true)]
async fn refused_connect_is_a_stream_error() {
    let fixture = Loop::new(
        FakeDocker {
            events_err: Some(crate::docker::Error::connection_failed(
                "connection refused".to_owned(),
            )),
            ..FakeDocker::default()
        },
        FakeReconciler::default(),
    );
    let mut subscription = subscribe(
        &CallContext::new(),
        &fixture.harness.docker,
        base(),
        DOCKER_CALL_TIMEOUT,
    )
    .await
    .unwrap();
    assert!(matches!(subscription, Subscription::Refused(_)));

    let mut backoff = MIN_BACKOFF;
    let reconnect = fixture
        .listener()
        .consume_events(&CallContext::new(), &mut subscription, &mut backoff, &mut 0)
        .await;
    assert!(reconnect);
    assert!(fixture.harness.logs().contains(
        "level=ERROR docker events stream error, reconnecting err=connection refused backoff=1s"
    ));
}

/// Runs the event loop on a task of its own until the returned context is canceled.
fn spawn_listener(
    fixture: &Arc<Loop>,
    since: Timestamp,
    stream: Option<FakeEvents>,
) -> (CallContext, tokio::task::JoinHandle<()>) {
    let context = CallContext::new();
    let fixture = Arc::clone(fixture);
    let listening = context.clone();
    let task = tokio::spawn(async move {
        fixture
            .listener()
            .listen_events(&listening, since, stream.map(Subscription::Open))
            .await;
    });
    (context, task)
}

/// Go: `TestListenEvents_ReconnectEventsHangRetries`. A hung reconnect is abandoned after the
/// timeout and retried through the backoff loop.
#[tokio::test(start_paused = true)]
async fn listen_events_reconnect_events_hang_retries() {
    let fixture = Arc::new(Loop::new(
        FakeDocker {
            events_block: [1].into(),
            ..FakeDocker::default()
        },
        FakeReconciler::default(),
    ));
    let docker = Arc::clone(&fixture.harness.docker);

    // No stream makes the loop subscribe at once, as after a dropped connection.
    let (context, task) = spawn_listener(&fixture, base(), None);
    eventually("the second attempt", WITHIN, || {
        docker.events_calls().0 >= 2
    })
    .await;
    context.cancel();
    task.await.unwrap();

    let (started, returned) = docker.events_calls();
    assert_eq!(started, returned, "an attempt leaked after shutdown");
    assert!(
        fixture.harness.logs().contains(
            "level=ERROR could not subscribe to docker events, retrying \
             err=docker event stream not established before timeout backoff=1s"
        ),
        "{}",
        fixture.harness.logs()
    );
}

/// Go: `TestListenEvents_ReconnectSinceAfterLastEvent`. A re-subscription starts one nanosecond
/// after the last received event.
#[tokio::test(start_paused = true)]
async fn listen_events_reconnect_since_after_last_event() {
    let fixture = Arc::new(Loop::new(FakeDocker::default(), FakeReconciler::default()));
    let docker = Arc::clone(&fixture.harness.docker);
    let last_event = base().checked_sub(SignedDuration::from_mins(1)).unwrap();
    let (sender, stream) = FakeEvents::channel();
    send(&sender, message("c1", nanos(last_event)));
    drop(sender);

    let initial = base().checked_sub(SignedDuration::from_hours(1)).unwrap();
    let (context, task) = spawn_listener(&fixture, initial, Some(stream));
    eventually("the re-subscription", WITHIN, || docker.sinces().len() == 1).await;
    context.cancel();
    task.await.unwrap();

    assert_eq!(
        docker.sinces(),
        [last_event
            .checked_add(SignedDuration::from_nanos(1))
            .unwrap()]
    );
}

/// Go: `TestListenEvents_ReconnectBeforeAnyEventUsesInitialSince`
#[tokio::test(start_paused = true)]
async fn listen_events_reconnect_before_any_event_uses_initial_since() {
    let fixture = Arc::new(Loop::new(FakeDocker::default(), FakeReconciler::default()));
    let docker = Arc::clone(&fixture.harness.docker);
    let (sender, stream) = FakeEvents::channel();
    drop(sender);

    let initial = base().checked_sub(SignedDuration::from_hours(1)).unwrap();
    let (context, task) = spawn_listener(&fixture, initial, Some(stream));
    eventually("the re-subscription", WITHIN, || docker.sinces().len() == 1).await;
    context.cancel();
    task.await.unwrap();

    assert_eq!(docker.sinces(), [initial]);
}

/// The readiness flag follows the stream: set once subscribed, cleared when the stream fails,
/// set again on the re-subscription, which also requests a pass.
#[tokio::test(start_paused = true)]
async fn listen_events_readiness_follows_the_stream() {
    let fixture = Arc::new(Loop::new(FakeDocker::default(), FakeReconciler::default()));
    let (sender, stream) = FakeEvents::channel();

    let (context, task) = spawn_listener(&fixture, base(), Some(stream));
    eventually("readiness", WITHIN, || fixture.readiness.is_ready()).await;

    fail(
        &sender,
        StreamError::new(StreamErrorKind::UnexpectedEof, "unexpected EOF"),
    );
    eventually("not ready", WITHIN, || !fixture.readiness.is_ready()).await;
    eventually("ready again", WITHIN, || fixture.readiness.is_ready()).await;
    assert_eq!(fixture.harness.epoch(), 1, "no pass requested");
    assert!(
        fixture.harness.logs().contains(
            "level=INFO docker event stream re-subscribed; reconciling running containers"
        )
    );
    assert!(
        fixture
            .harness
            .logs()
            .contains("level=DEBUG subscribed to docker events")
    );

    context.cancel();
    task.await.unwrap();
}

/// Go: `TestCoordinator_DedupSharedWithConsumer`. Passes (which write the processed map) run
/// while the event consumer reads and consumes it, without a reconnect.
#[tokio::test(start_paused = true)]
async fn coordinator_dedup_shared_with_consumer() {
    let fixture = Arc::new(Loop::started(
        FakeDocker::listing(&["a", "b", "c"]),
        FakeReconciler::default(),
    ));
    let (sender, stream) = FakeEvents::channel();
    let context = CallContext::new();
    let consuming = {
        let fixture = Arc::clone(&fixture);
        let context = context.clone();
        tokio::spawn(async move { fixture.consume_once(&context, stream).await })
    };

    for round in 0..50 {
        fixture.harness.coordinator.request(1);
        send(
            &sender,
            message(["a", "b", "c"][round % 3], nanos(fixture.harness.now())),
        );
        tokio::task::yield_now().await;
    }

    eventually("the passes", WITHIN, || fixture.harness.settled()).await;
    context.cancel();
    assert!(!consuming.await.unwrap());
}

/// Go: `TestCoordinator_DedupAcrossReconnectUnderRace`. The event loop goes through a stream
/// close and a re-subscription while passes run, so the consumer and the coordinator touch the
/// processed map on both sides of a reconnect.
#[tokio::test(start_paused = true)]
async fn coordinator_dedup_across_reconnect() {
    let mut docker = FakeDocker::listing(&["a", "b"]);
    let sender = docker.first_stream();
    let fixture = Arc::new(Loop::started(docker, FakeReconciler::default()));
    let docker = Arc::clone(&fixture.harness.docker);

    let (context, task) = spawn_listener(&fixture, base(), None);
    for round in 0..20 {
        fixture.harness.coordinator.request(1);
        send(
            &sender,
            message(["a", "b"][round % 2], nanos(fixture.harness.now())),
        );
        tokio::task::yield_now().await;
    }
    drop(sender);

    // While the loop backs off and re-subscribes, keep the passes going.
    eventually("the re-subscription", WITHIN, || {
        fixture.harness.coordinator.request(1);
        docker.events_calls().0 >= 2
    })
    .await;
    eventually("the passes", WITHIN, || fixture.harness.settled()).await;
    context.cancel();
    task.await.unwrap();
}

/// Go: `TestListenEvents_ReconnectReenumerates`. A re-subscription requests a pass on its own
/// (dockerd's replay buffer is empty after a restart), while events on the new stream are
/// consumed beside that pass.
#[tokio::test(start_paused = true)]
async fn listen_events_reconnect_reenumerates() {
    let mut docker = FakeDocker::listing(&["a", "b"]);
    let sender = docker.first_stream();
    let fixture = Arc::new(Loop::started(docker, FakeReconciler::default()));
    let docker = Arc::clone(&fixture.harness.docker);
    let (closed, stream) = FakeEvents::channel();
    drop(closed);

    let (context, task) = spawn_listener(&fixture, base(), Some(stream));
    // The new stream delivers events while the pass the reconnect requested runs.
    for round in 0..20 {
        send(
            &sender,
            start_event(["a", "b"][round % 2], fixture.harness.now()),
        );
    }

    eventually("the pass", WITHIN, || docker.list_calls() >= 1).await;
    eventually("the reconciles", WITHIN, || fixture.harness.settled()).await;
    context.cancel();
    task.await.unwrap();

    let seen = fixture.harness.seen();
    assert!(
        seen.iter().any(|id| id == "a") && seen.iter().any(|id| id == "b"),
        "reconciled {seen:?}, want both running containers"
    );
}
