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

// Adapted from swarm-rootless-docker/src/docker/fake.rs at 16913a4d87d8,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! A recording, in-memory [`EngineApi`] for one launcher run: Go's `fakeDocker`
//! (`fake_test.go`), on SRD's recording fake (one shared state, every call logged in order,
//! failures injected per call). Clones share the state, so a scripted stop can act on the engine
//! from a task of its own.
//!
//! The daemon's side of the attach is a channel the test writes frames to and closes, with or
//! without an error; the wait answers when the test reports the daemon's removal, or fails when
//! it injects an error.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;

use super::engine::{
    Attached, Created, EngineApi, Inspected, State, Summary, WaitAnswer, WaitResponse,
};
use super::spec::{DEFAULT_HOST_DOCKER_SOCKET, DaemonSpec, Options};
use super::{Console, Launch, LaunchError, Timing};
use crate::docker::attach::Output;
use crate::docker::{CallContext, ClientError, Error, stream_failure};
use crate::test_support::{Logs, capture_logs};
use tracing::level_filters::LevelFilter;

pub(super) const TEST_LAUNCHER_ID: &str =
    "1111111111111111111111111111111111111111111111111111111111111111";
pub(super) const TEST_IMAGE_ID: &str =
    "sha256:2222222222222222222222222222222222222222222222222222222222222222";
pub(super) const TEST_DAEMON_ID: &str =
    "3333333333333333333333333333333333333333333333333333333333333333";

/// How many pieces of output the fake holds for the launcher.
const OUTPUT_PIECES: usize = 1024;

/// The stdout and stderr streams of the attach.
pub(super) const STREAM_STDOUT: u8 = 1;
pub(super) const STREAM_STDERR: u8 = 2;

/// Go's `errInjected`.
pub(super) fn injected() -> Error {
    Error::unavailable("injected failure".to_owned())
}

/// Go's `errDenied`.
pub(super) fn denied() -> Error {
    Error::response(403, "permission denied".to_owned())
}

/// A 404, as moby reports a missing container.
pub(super) fn not_found(id: &str) -> Error {
    Error::response(404, format!("No such container: {id}"))
}

/// What a scripted stop does, given the engine.
pub(super) type StopScript =
    Arc<dyn Fn(FakeEngine) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// A stop call, as made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StopCall {
    pub(super) id: String,
    pub(super) timeout: i64,
}

#[derive(Default)]
struct Script {
    inspect: HashMap<String, Inspected>,
    inspect_errors: HashMap<String, Error>,
    list: Vec<Summary>,
    create_error: Option<Error>,
    attach_error: Option<Error>,
    start_error: Option<Error>,
    /// The inspect of this container, the attach or the wait never answers, until the launcher
    /// drops it.
    inspect_hang: Option<String>,
    attach_hang: bool,
    wait_hang: bool,
    /// Replaces the default stop, which ends the daemon.
    stop: Option<StopScript>,
}

#[derive(Default)]
struct Recorded {
    calls: Vec<String>,
    removed: Vec<String>,
    created: Option<DaemonSpec>,
    stops: Vec<StopCall>,
    daemon_gone: bool,
    /// The daemon's side of the attach.
    output: Option<mpsc::Sender<Result<Output, ClientError>>>,
    /// Hung calls the launcher dropped.
    released: usize,
}

struct Inner {
    script: Mutex<Script>,
    recorded: Mutex<Recorded>,
    wait_results: mpsc::UnboundedSender<WaitResponse>,
    wait_errors: mpsc::UnboundedSender<String>,
    /// The wait's receivers, taken by the one wait of a run.
    wait_receivers: Mutex<
        Option<(
            mpsc::UnboundedReceiver<WaitResponse>,
            mpsc::UnboundedReceiver<String>,
        )>,
    >,
    started: Notify,
}

/// The fake engine.
#[derive(Clone)]
pub(super) struct FakeEngine(Arc<Inner>);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A test's panic poisons it; what it recorded is still what it was.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Default for FakeEngine {
    /// Go's `newFakeDocker`: the launcher container runs, from [`TEST_IMAGE_ID`].
    fn default() -> Self {
        let (wait_results, results) = mpsc::unbounded_channel();
        let (wait_errors, errors) = mpsc::unbounded_channel();
        let fake = Self(Arc::new(Inner {
            script: Mutex::default(),
            recorded: Mutex::default(),
            wait_results,
            wait_errors,
            wait_receivers: Mutex::new(Some((results, errors))),
            started: Notify::new(),
        }));
        fake.set_inspect(TEST_LAUNCHER_ID, running(TEST_IMAGE_ID, true));
        fake
    }
}

/// An inspect answer with a state.
pub(super) fn running(image: &str, running: bool) -> Inspected {
    Inspected {
        image: image.to_owned(),
        state: Some(State { running }),
    }
}

impl FakeEngine {
    pub(super) fn set_inspect(&self, id: &str, inspected: Inspected) {
        lock(&self.0.script)
            .inspect
            .insert(id.to_owned(), inspected);
    }

    pub(super) fn fail_inspect(&self, id: &str, error: Error) {
        lock(&self.0.script)
            .inspect_errors
            .insert(id.to_owned(), error);
    }

    pub(super) fn set_list(&self, list: Vec<Summary>) {
        lock(&self.0.script).list = list;
    }

    pub(super) fn fail_create(&self, error: Error) {
        lock(&self.0.script).create_error = Some(error);
    }

    pub(super) fn fail_attach(&self, error: Error) {
        lock(&self.0.script).attach_error = Some(error);
    }

    pub(super) fn fail_start(&self, error: Error) {
        lock(&self.0.script).start_error = Some(error);
    }

    pub(super) fn hang_inspect(&self, id: &str) {
        lock(&self.0.script).inspect_hang = Some(id.to_owned());
    }

    pub(super) fn hang_attach(&self) {
        lock(&self.0.script).attach_hang = true;
    }

    pub(super) fn hang_wait(&self) {
        lock(&self.0.script).wait_hang = true;
    }

    pub(super) fn script_stop(&self, stop: StopScript) {
        lock(&self.0.script).stop = Some(stop);
    }

    /// Every call so far, in order.
    pub(super) fn calls(&self) -> Vec<String> {
        lock(&self.0.recorded).calls.clone()
    }

    pub(super) fn called(&self, call: &str) -> bool {
        self.calls().iter().any(|made| made.starts_with(call))
    }

    pub(super) fn removed(&self) -> Vec<String> {
        lock(&self.0.recorded).removed.clone()
    }

    pub(super) fn created(&self) -> Option<DaemonSpec> {
        lock(&self.0.recorded).created.clone()
    }

    pub(super) fn stops(&self) -> Vec<StopCall> {
        lock(&self.0.recorded).stops.clone()
    }

    /// How many hung calls the launcher dropped.
    pub(super) fn released(&self) -> usize {
        lock(&self.0.recorded).released
    }

    /// Waits until the daemon was started.
    pub(super) async fn started(&self) {
        self.0.started.notified().await;
    }

    /// Writes one line of daemon output to `stream`.
    pub(super) fn emit(&self, stream: u8, line: &str) {
        let bytes = format!("{line}\n").into_bytes();
        let piece = if stream == STREAM_STDERR {
            Output::Stderr(bytes)
        } else {
            Output::Stdout(bytes)
        };
        if let Some(output) = &lock(&self.0.recorded).output {
            let _ = output.try_send(Ok(piece));
        }
    }

    /// Ends the attach stream, with an error when `error` is set.
    pub(super) fn close_output(&self, error: Option<&str>) {
        if let Some(output) = lock(&self.0.recorded).output.take()
            && let Some(error) = error
        {
            let _ = output.try_send(Err(stream_failure(error)));
        }
    }

    /// Ends the daemon's output and reports its removal with `status`.
    pub(super) fn exit(&self, status: i64) {
        self.close_output(None);
        self.mark_gone();
        self.report_removed(status);
    }

    /// Answers the wait with `status`, the output left as it is.
    pub(super) fn report_removed(&self, status: i64) {
        let _ = self.0.wait_results.send(WaitResponse {
            status_code: status,
            error: None,
        });
    }

    /// Fails the wait stream with `error`.
    pub(super) fn fail_wait(&self, error: &str) {
        let _ = self.0.wait_errors.send(error.to_owned());
    }

    /// Makes inspect report the daemon removed.
    pub(super) fn mark_gone(&self) {
        lock(&self.0.recorded).daemon_gone = true;
    }

    fn record(&self, call: String) {
        lock(&self.0.recorded).calls.push(call);
    }

    /// Never answers; counts its own drop as the launcher releasing it.
    async fn hang<T>(&self) -> T {
        let _release = Release(self.clone());
        std::future::pending().await
    }
}

/// Counts a dropped hung call.
struct Release(FakeEngine);

impl Drop for Release {
    fn drop(&mut self) {
        lock(&self.0.0.recorded).released += 1;
    }
}

impl EngineApi for FakeEngine {
    async fn inspect(&self, id: &str) -> Result<Inspected, Error> {
        self.record(format!("inspect {id}"));
        let hang = lock(&self.0.script).inspect_hang.as_deref() == Some(id);
        if hang {
            return self.hang().await;
        }
        let script = lock(&self.0.script);
        if let Some(error) = script.inspect_errors.get(id) {
            return Err(error.clone());
        }
        if id == TEST_DAEMON_ID && !lock(&self.0.recorded).daemon_gone {
            return Ok(running("", true));
        }
        script.inspect.get(id).cloned().ok_or_else(|| not_found(id))
    }

    fn list_daemons(&self) -> impl Future<Output = Result<Vec<Summary>, Error>> + Send {
        self.record("list".to_owned());
        std::future::ready(Ok(lock(&self.0.script).list.clone()))
    }

    fn remove(&self, id: &str) -> impl Future<Output = Result<(), Error>> + Send {
        self.record(format!("remove {id}"));
        lock(&self.0.recorded).removed.push(id.to_owned());
        std::future::ready(Ok(()))
    }

    fn create(&self, spec: &DaemonSpec) -> impl Future<Output = Result<Created, Error>> + Send {
        self.record("create".to_owned());
        let failure = lock(&self.0.script).create_error.clone();
        let created = failure.map_or_else(
            || {
                lock(&self.0.recorded).created = Some(spec.clone());
                Ok(Created {
                    id: TEST_DAEMON_ID.to_owned(),
                    warnings: Vec::new(),
                })
            },
            Err,
        );
        std::future::ready(created)
    }

    async fn attach(&self, _id: &str) -> Result<Attached, Error> {
        self.record("attach".to_owned());
        let (hang, error) = {
            let script = lock(&self.0.script);
            (script.attach_hang, script.attach_error.clone())
        };
        if hang {
            return self.hang().await;
        }
        if let Some(error) = error {
            return Err(error);
        }
        let (sender, receiver) = mpsc::channel(OUTPUT_PIECES);
        lock(&self.0.recorded).output = Some(sender);
        Ok(receiver)
    }

    async fn wait_removed(&self, _id: &str) -> Result<WaitAnswer, Error> {
        self.record("wait".to_owned());
        if lock(&self.0.script).wait_hang {
            return self.hang().await;
        }
        let Some((mut results, mut errors)) = lock(&self.0.wait_receivers).take() else {
            return Err(injected());
        };
        Ok(Box::pin(async move {
            tokio::select! {
                Some(result) = results.recv() => Ok(result),
                Some(error) = errors.recv() => Err(error),
            }
        }))
    }

    fn start(&self, _id: &str) -> impl Future<Output = Result<(), Error>> + Send {
        self.record("start".to_owned());
        let failure = lock(&self.0.script).start_error.clone();
        let started = failure.map_or_else(
            || {
                self.0.started.notify_one();
                Ok(())
            },
            Err,
        );
        std::future::ready(started)
    }

    async fn stop(&self, id: &str, timeout_seconds: i64) -> Result<(), Error> {
        self.record(format!("stop {id}"));
        lock(&self.0.recorded).stops.push(StopCall {
            id: id.to_owned(),
            timeout: timeout_seconds,
        });
        let script = lock(&self.0.script).stop.clone();
        if let Some(stop) = script {
            stop(self.clone()).await;
        } else {
            self.emit(STREAM_STDOUT, "daemon stopping");
            self.exit(0);
        }
        Ok(())
    }
}

/// A writer the test reads back.
#[derive(Clone, Default)]
pub(super) struct Buffer(Arc<Mutex<Vec<u8>>>);

impl Buffer {
    pub(super) fn text(&self) -> String {
        String::from_utf8_lossy(&lock(&self.0)).into_owned()
    }
}

impl tokio::io::AsyncWrite for Buffer {
    fn poll_write(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        lock(&self.0).extend_from_slice(data);
        Poll::Ready(Ok(data.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// Go's `testRun`: a launch on the fake, its output and its log.
pub(super) struct TestRun {
    pub(super) fake: FakeEngine,
    pub(super) stdout: Buffer,
    pub(super) stderr: Buffer,
    pub(super) logs: Logs,
    pub(super) launch: Launch<FakeEngine>,
}

/// Go's `newTestRun`: the default host socket, the test launcher's ID, a 2 s call timeout, a
/// 5 s budget and a 10 ms poll.
pub(super) fn test_run(fake: &FakeEngine) -> TestRun {
    let logs = capture_logs(LevelFilter::DEBUG);
    let stdout = Buffer::default();
    let stderr = Buffer::default();
    let mut launch = Launch::new(
        Arc::new(fake.clone()),
        Options {
            host_docker_socket: DEFAULT_HOST_DOCKER_SOCKET.to_owned(),
            ..Options::default()
        },
        Console {
            stdout: Box::new(stdout.clone()),
            stderr: Box::new(stderr.clone()),
        },
    );
    launch.self_id = Box::new(|| Ok(TEST_LAUNCHER_ID.to_owned()));
    launch.timing = Timing {
        call: Duration::from_secs(2),
        shutdown_budget: Duration::from_secs(5),
        poll: Duration::from_millis(10),
    };
    TestRun {
        fake: fake.clone(),
        stdout,
        stderr,
        logs,
        launch,
    }
}

/// How long a test waits for the launcher to return: Go's `result` bound, on the paused clock.
pub(super) const RESULT_WITHIN: Duration = Duration::from_secs(300);

/// Runs the launch in the background under `context`.
pub(super) fn start(
    launch: Launch<FakeEngine>,
    context: &CallContext,
) -> JoinHandle<Result<i64, LaunchError>> {
    let context = context.clone();
    tokio::spawn(async move { launch.run(&context).await })
}

/// Go's `result`: waits for the launcher to return.
pub(super) async fn result(done: JoinHandle<Result<i64, LaunchError>>) -> Result<i64, LaunchError> {
    tokio::time::timeout(RESULT_WITHIN, done)
        .await
        .expect("launcher did not return")
        .expect("launcher panicked")
}

/// Go's `requireStopped`: one stop, of the daemon, with the ten-second timeout.
pub(super) fn require_stopped(fake: &FakeEngine) {
    assert_eq!(
        fake.stops(),
        [StopCall {
            id: TEST_DAEMON_ID.to_owned(),
            timeout: 10,
        }]
    );
}
