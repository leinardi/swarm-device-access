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

//! The `Run` tests of `daemon_test.go` and `timeout_test.go`, on the paused clock against the
//! fake Docker of `test_support` and a real processor that inspects through it (Go's
//! `recordingInspector`: a container without a running state, which reconciles without touching
//! a cgroup).

use std::future::Future;
use std::sync::{Arc, Mutex};

use jiff::Timestamp;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::coordinator::testing::{WITHIN, base};
use super::{DOCKER_CALL_TIMEOUT, OnSubscribed, Options, Readiness, run_with};
use crate::clock::{Clock, TokioClock};
use crate::config::{Runtime, new_store};
use crate::docker::CallContext;
use crate::docker::types::{Actor, Event};
use crate::logger::{Level, MemorySink};
use crate::metrics::{Recorder, Value};
use crate::policy::Global;
use crate::processor::{self, Processor};
use crate::systemd::OnReload;
use crate::test_support::fake_docker::{Call, FakeDocker};
use crate::test_support::{capture_logs, eventually};

/// A running daemon over the fake Docker.
struct Daemon {
    docker: Arc<FakeDocker>,
    metrics: Recorder,
    readiness: Readiness,
    sink: MemorySink,
    context: CallContext,
    task: JoinHandle<()>,
}

impl Daemon {
    /// Go's `run` with `start_watcher` as the reload watcher.
    fn start<W, F>(docker: FakeDocker, start_watcher: W) -> Self
    where
        W: FnOnce(CancellationToken, OnReload, OnSubscribed) -> F + Send + 'static,
        F: Future<Output = ()> + Send + 'static,
    {
        let clock: Arc<dyn Clock> = Arc::new(TokioClock::new(base()));
        let docker = Arc::new(FakeDocker {
            clock: Some(Arc::clone(&clock)),
            ..docker
        });
        let (log, sink) = capture_logs(Level::DEBUG);
        let (store, publisher) = new_store(Runtime {
            dry_run: false,
            policy: Global::new("all", None, None).unwrap(),
        });
        let processor = Processor::new(processor::Settings {
            inspector: Arc::clone(&docker),
            store,
            publisher,
            metrics: Recorder::noop(),
            host_root: "/host".to_owned(),
            proc_root: "/".to_owned(),
            call_timeout: Some(DOCKER_CALL_TIMEOUT),
            clock: Arc::clone(&clock),
            log: log.clone(),
        });
        let metrics = Recorder::new();
        let readiness = Readiness::default();
        let options = Options {
            docker: Arc::clone(&docker),
            processor,
            metrics: metrics.clone(),
            readiness: readiness.clone(),
            call_timeout: DOCKER_CALL_TIMEOUT,
            clock,
            log,
        };
        let context = CallContext::new();
        let running = context.clone();
        let task = tokio::spawn(async move { run_with(&running, options, start_watcher).await });
        Self {
            docker,
            metrics,
            readiness,
            sink,
            context,
            task,
        }
    }

    /// Cancels the daemon and waits for run to return.
    async fn stop(self) -> Arc<FakeDocker> {
        self.context.cancel();
        self.task.await.unwrap();
        self.docker
    }
}

/// Go's `noopWatcher`.
async fn noop_watcher(
    _cancel: CancellationToken,
    _on_reload: OnReload,
    _on_subscribed: OnSubscribed,
) {
}

/// An event with only an actor and a time, as the Go tests send them.
fn message(container_id: &str, at: Timestamp) -> Event {
    Event {
        actor: Actor {
            id: container_id.to_owned(),
            ..Actor::default()
        },
        time_nano: i64::try_from(at.as_nanosecond()).unwrap(),
        ..Event::default()
    }
}

/// Go: `TestRun_SubscribesBeforeEnumerating`. The event stream is opened (with a `since` not
/// after the list time) before containers are listed, and an event for a container started
/// during the enumeration is applied.
#[tokio::test(start_paused = true)]
async fn run_subscribes_before_enumerating() {
    let mut docker = FakeDocker::listing(&["listed"]);
    let sender = docker.first_stream();
    let clock = TokioClock::new(base());
    docker.on_list = Some(Box::new(move || {
        sender.send(Ok(message("unlisted", clock.now()))).unwrap();
    }));
    let daemon = Daemon::start(docker, noop_watcher);

    let docker = Arc::clone(&daemon.docker);
    eventually("the unlisted container", WITHIN, || {
        docker.inspected().iter().any(|id| id == "unlisted")
    })
    .await;
    let docker = daemon.stop().await;

    let order: Vec<Call> = docker
        .calls()
        .into_iter()
        .filter(|call| matches!(call, Call::Events(_) | Call::List))
        .take(2)
        .collect();
    assert!(
        matches!(order.as_slice(), [Call::Events(_), Call::List]),
        "calls = {order:?}, want events before list"
    );
    assert!(
        docker.sinces()[0] <= docker.list_times()[0],
        "since is after the list"
    );
    assert_eq!(docker.inspected(), ["listed", "unlisted"]);
}

/// Go: `TestRun_WatcherRunsBesideStartupAndEvents`. The reload watcher cannot hold up the startup
/// pass or the event loop (it blocks here for the whole run, like a wedged system bus), and each
/// subscription it reports requests a pass, covering a reload it could not have seen.
#[tokio::test(start_paused = true)]
async fn run_watcher_runs_beside_startup_and_events() {
    let mut docker = FakeDocker::listing(&["listed"]);
    let sender = docker.first_stream();
    let (subscribed_sender, subscribed) = oneshot::channel::<OnSubscribed>();
    let blocking_watcher = move |cancel: CancellationToken, _on_reload, on_subscribed| async move {
        let _ = subscribed_sender.send(on_subscribed);
        cancel.cancelled().await;
    };
    let daemon = Daemon::start(docker, blocking_watcher);
    let docker = Arc::clone(&daemon.docker);

    sender.send(Ok(message("started", base()))).unwrap();
    eventually("the started container", WITHIN, || {
        docker.inspected().iter().any(|id| id == "started")
    })
    .await;

    let before = docker.list_calls();
    let mut on_subscribed = subscribed.await.unwrap();
    on_subscribed();
    eventually("the pass", WITHIN, || docker.list_calls() > before).await;
    assert!(daemon.readiness.is_ready());
    daemon.stop().await;
}

/// A completed systemd reload counts a re-apply and requests a pass.
#[tokio::test(start_paused = true)]
async fn run_reapplies_on_a_completed_reload() {
    let (reload_sender, reloaded) = oneshot::channel::<OnReload>();
    let watcher = move |cancel: CancellationToken, on_reload, _on_subscribed| async move {
        let _ = reload_sender.send(on_reload);
        cancel.cancelled().await;
    };
    let daemon = Daemon::start(FakeDocker::listing(&["listed"]), watcher);
    let docker = Arc::clone(&daemon.docker);
    eventually("the startup pass", WITHIN, || docker.list_calls() == 1).await;

    let on_reload = reloaded.await.unwrap();
    on_reload();
    eventually("the re-apply pass", WITHIN, || docker.list_calls() == 2).await;
    let reapplies = daemon
        .metrics
        .registry()
        .unwrap()
        .gather()
        .into_iter()
        .find(|family| family.name == "sda_reload_reapplies_total")
        .map(|family| family.series[0].value.clone());
    assert_eq!(reapplies, Some(Value::Scalar(1.0)));
    daemon.stop().await;
}

/// Go: `TestRun_InitialEventsHangStillEnumeratesAndResubscribes`. A hung initial subscription
/// blocks neither the startup enumeration nor the event loop, which resubscribes from the
/// original `since`.
#[tokio::test(start_paused = true)]
async fn run_initial_events_hang_still_enumerates_and_resubscribes() {
    let docker = FakeDocker {
        events_block: [1].into(),
        ..FakeDocker::listing(&["listed"])
    };
    let daemon = Daemon::start(docker, noop_watcher);
    let docker = Arc::clone(&daemon.docker);

    eventually("the enumeration and the re-subscription", WITHIN, || {
        docker.events_calls().0 >= 2 && docker.inspected().iter().any(|id| id == "listed")
    })
    .await;
    let logs = daemon.sink.contents();
    let docker = daemon.stop().await;

    let sinces = docker.sinces();
    assert_eq!(
        sinces[1], sinces[0],
        "the re-subscription's since is not the initial one"
    );
    let (started, returned) = docker.events_calls();
    assert_eq!(started, returned, "an attempt leaked after shutdown");
    assert!(logs.contains(
        "level=WARN could not subscribe to docker events; will retry \
         err=docker event stream not established before timeout"
    ));
}

/// Run returns only once the reload watcher it started has ended.
#[tokio::test(start_paused = true)]
async fn run_waits_for_its_background_tasks() {
    let ended = Arc::new(Mutex::new(false));
    let watcher_ended = Arc::clone(&ended);
    let watcher = move |cancel: CancellationToken, _on_reload, _on_subscribed| async move {
        cancel.cancelled().await;
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        *watcher_ended.lock().unwrap() = true;
    };
    let daemon = Daemon::start(FakeDocker::default(), watcher);
    let docker = Arc::clone(&daemon.docker);
    eventually("the startup pass", WITHIN, || docker.list_calls() == 1).await;

    daemon.stop().await;
    assert!(
        *ended.lock().unwrap(),
        "run returned before the watcher ended"
    );
}
