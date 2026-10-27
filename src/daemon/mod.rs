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

//! The daemon loop ([`run`]): the Docker event loop and the [`Coordinator`] that owns every
//! reconciliation pass, retry and container event. Go's `internal/daemon`.

mod coordinator;
mod events;

pub use coordinator::{ContainerLister, Coordinator, Reconciler, Settings};

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use self::events::{Listener, subscribe};
use crate::clock::Clock;
use crate::docker::{CallContext, DockerApi};
use crate::logger::{Attr, LogHandle};
use crate::metrics::Recorder;
use crate::processor::{Inspector, Processor};
use crate::systemd::{self, OnReload};

/// The first delay of a retry: a failed container, a failed list, a lost event stream.
pub const MIN_BACKOFF: Duration = Duration::from_secs(1);

/// The longest delay of a retry; each failure doubles the delay up to it.
pub const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Bounds every request/response Docker API call and the establishment of the event stream.
/// Without it a wedged dockerd blocks the caller forever, and nothing behind it (retries,
/// reconnects, shutdown) ever runs.
pub const DOCKER_CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Whether the daemon is subscribed to the Docker event stream: what `/readyz` reports. Clones
/// share one flag; it starts not ready.
#[derive(Debug, Clone, Default)]
pub struct Readiness(Arc<AtomicBool>);

impl Readiness {
    /// Go's `SetReady`.
    pub fn set(&self, ready: bool) {
        self.0.store(ready, Ordering::SeqCst);
    }

    /// Whether the daemon is ready.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// What [`run`] works with.
pub struct Options<D, I> {
    /// The Docker client, shared by the coordinator's lists and the event loop.
    pub docker: Arc<D>,
    pub processor: Arc<Processor<I>>,
    pub metrics: Recorder,
    pub readiness: Readiness,
    /// Bounds each Docker call and the stream's establishment ([`DOCKER_CALL_TIMEOUT`] outside
    /// tests).
    pub call_timeout: Duration,
    pub clock: Arc<dyn Clock>,
    pub log: LogHandle,
}

/// The systemd watcher's callback on each successful subscription.
type OnSubscribed = Box<dyn FnMut() + Send>;

/// Subscribes to the Docker event stream, reconciles every running container, subscribes to the
/// systemd reload signal, and then follows the event stream until `context` is done. It returns
/// once every task it started has ended and every hold and apply transaction has completed.
///
/// The event stream is opened before the enumeration, from the time just before subscribing, so
/// a container started while the list is being processed is still delivered as an event instead
/// of being missed.
pub async fn run<D, I>(context: &CallContext, options: Options<D, I>)
where
    D: DockerApi + 'static,
    I: Inspector,
{
    let log = options.log.clone();
    run_with(context, options, move |cancel, on_reload, on_subscribed| {
        systemd::supervise(cancel, log, on_reload, on_subscribed)
    })
    .await;
}

/// [`run`] with the systemd watcher a test replaces: Go's `startWatcher` seam.
async fn run_with<D, I, W, F>(context: &CallContext, options: Options<D, I>, start_watcher: W)
where
    D: DockerApi + 'static,
    I: Inspector,
    W: FnOnce(CancellationToken, OnReload, OnSubscribed) -> F,
    F: Future<Output = ()> + Send + 'static,
{
    let Options {
        docker,
        processor,
        metrics,
        readiness,
        call_timeout,
        clock,
        log,
    } = options;
    let since = clock.now();

    // Events that arrive during the enumeration below wait, with backpressure on the socket,
    // until the listener reads them. A failed subscription leaves none; the listener then
    // resubscribes from since, so the events of the enumeration window are still replayed.
    let stream = match subscribe(context, &docker, since, call_timeout).await {
        Ok(stream) => Some(stream),
        Err(error) => {
            log.warn(
                "could not subscribe to docker events; will retry",
                &[Attr::error("err", &error)],
            );
            None
        }
    };

    let coordinator = Coordinator::new(Settings {
        docker: Arc::clone(&docker),
        reconciler: Arc::clone(&processor),
        metrics: metrics.clone(),
        timeout: call_timeout,
        clock,
        log: log.clone(),
    });
    // Every later publication goes to the coordinator; installed before the startup request,
    // so no generation published from here on is missed.
    processor
        .set_pass_requester(coordinator.pass_requester())
        .await;

    // Waited for before run returns, with every hold, so shutdown does not cut off a reconcile
    // in the middle of its cgroup mutation.
    let background = TaskTracker::new();
    let coordinator_run = Arc::clone(&coordinator);
    let coordinator_context = context.clone();
    background.spawn(async move { coordinator_run.run(&coordinator_context).await });

    // The reload watcher starts before the startup pass, on its own task, so a wedged system bus
    // never delays the pass or the event loop. Every time it subscribes, the first time
    // included, it requests a pass: a daemon-reload that completed before it was listening (for
    // example during the startup pass) was not seen.
    let reloaded = Arc::clone(&coordinator);
    let reloaded_processor = Arc::clone(&processor);
    let reload_metrics = metrics.clone();
    let on_reload: OnReload = Arc::new(move || {
        reload_metrics.inc_reload_reapply();
        reloaded.request(reloaded_processor.generation());
    });
    let subscribed = Arc::clone(&coordinator);
    let subscribed_processor = Arc::clone(&processor);
    let on_subscribed: OnSubscribed =
        Box::new(move || subscribed.request(subscribed_processor.generation()));
    background.spawn(start_watcher(
        context.cancellation(),
        on_reload,
        on_subscribed,
    ));

    // The startup pass reconciles every running container under the startup config. In live
    // mode that replaces or strips grants a previous instance left behind.
    coordinator.request(processor.generation());
    coordinator.await_first_pass(context).await;

    let listener = Listener {
        docker: &docker,
        coordinator: &coordinator,
        metrics: &metrics,
        readiness: &readiness,
        log: &log,
        timeout: call_timeout,
    };
    listener.listen_events(context, since, stream).await;

    background.close();
    background.wait().await;
    coordinator.close_and_wait().await;
}

#[cfg(test)]
mod tests;
