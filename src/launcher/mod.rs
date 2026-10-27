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

// Adapted from swarm-rootless-docker/src/launcher/mod.rs at 16913a4d87d8,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The `launch` mode, Go's `internal/launcher`: run the privileged daemon container from the
//! image the launcher itself runs, and supervise it. The launcher is an unprivileged Swarm
//! service task, so Swarm's rolling updates, rollbacks and image watchers act on the daemon too.
//!
//! 1. Identify the launcher's own container from its mount table, and its content-addressed
//!    image ID (a tag would let the daemon run a different image than the launcher).
//! 2. Remove the daemon a previous launcher left behind, only when its owner is confirmed gone
//!    ([`stale`]).
//! 3. Create the daemon from [`spec::daemon_spec`], attach to its output and start copying it,
//!    register the wait for its removal, and only then start it, in the order the `docker run`
//!    CLI uses: nothing the daemon prints, nor its exit, is missed.
//! 4. Supervise: the daemon's removal ends the launcher with the daemon's status; a broken wait
//!    stream or a lost log stream stops the daemon and fails; the launcher's own stop (the
//!    context) stops the daemon by ID and still returns its status. The stop, the wait for the
//!    removal and the drain of the last output share one budget, detached from the context.
//!
//! From SRD's launcher: the supervision `select!`, the timing, the output pump and the setup
//! order around one created container, removed by ID on every failed path. SRD's node lock,
//! probe, subordinate ID upload, health polling and status socket have no counterpart here.
//! The streams are established with a bounded wait raced against the context, as Go's
//! `establish` does; a call that loses the race is dropped, which closes its connection, where
//! Go left a goroutine to release it.

mod engine;
mod error;
pub mod self_id;
pub mod spec;
mod stale;
mod stdcopy;

#[cfg(test)]
mod fake;
#[cfg(test)]
mod tests;

pub use engine::{
    Attached, Created, Engine, EngineApi, Inspected, State, Summary, WaitAnswer, WaitExitError,
    WaitResponse,
};
pub use error::{BudgetExceeded, EstablishError, LaunchError, RemoveError, StaleError};
pub use self_id::SelfIdError;
pub use spec::{DaemonSpec, Mount, Options};

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncWrite;
use tokio::task::JoinHandle;

use crate::daemon::DOCKER_CALL_TIMEOUT;
use crate::docker::{CallContext, Client, Error, with_context};
use crate::logger::{Attr, LogHandle};

/// The `component` attribute of every launcher log line.
const COMPONENT: &str = "launcher";

/// The prefix of a content-addressed image ID.
const IMAGE_ID_PREFIX: &str = "sha256:";

/// The length of a short container ID.
const SHORT_ID_LEN: usize = 12;

/// How long each step may take. The defaults are the Go launcher's; tests shorten them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// One Docker call, and the establishment of each stream.
    pub call: Duration,
    /// Everything a shutdown does: the stop, the wait for the removal and the drain together.
    /// It stays inside the compose files' `stop_grace_period` of 30 s, so Swarm never kills a
    /// launcher that is still stopping its daemon.
    pub shutdown_budget: Duration,
    /// Between two inspects that confirm a removal the wait stream can no longer report.
    pub poll: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            call: DOCKER_CALL_TIMEOUT,
            shutdown_budget: spec::STOP_TIMEOUT + DOCKER_CALL_TIMEOUT,
            poll: Duration::from_millis(250),
        }
    }
}

/// A writer of the daemon's output.
pub type Output = Box<dyn AsyncWrite + Send + Unpin>;

/// Where the daemon's output goes: the launcher's own stdout and stderr, so Swarm's service logs
/// carry the daemon's.
pub struct Console {
    /// Where the daemon's stdout goes.
    pub stdout: Output,
    /// Where the daemon's stderr goes.
    pub stderr: Output,
}

impl Console {
    /// The process's stdout and stderr.
    #[must_use]
    pub fn process() -> Self {
        Self {
            stdout: Box::new(tokio::io::stdout()),
            stderr: Box::new(tokio::io::stderr()),
        }
    }
}

impl std::fmt::Debug for Console {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Console").finish_non_exhaustive()
    }
}

/// How the launcher finds its own container ID.
pub type SelfId = Box<dyn Fn() -> Result<String, SelfIdError> + Send + Sync>;

/// Go's `launcher.Run`: the launch on the Engine of `client`, the launcher's own socket at
/// [`spec::CONTAINER_DOCKER_SOCKET`] (Go's `DockerHost`), with the daemon's output on the
/// process's stdout and stderr.
///
/// # Errors
///
/// [`LaunchError`] when the launcher failed; its status is then 1.
pub async fn run(
    context: &CallContext,
    client: Client,
    options: Options,
    log: &LogHandle,
) -> Result<i64, LaunchError> {
    Launch::new(
        Arc::new(Engine::new(client)),
        options,
        Console::process(),
        log,
    )
    .run(context)
    .await
}

/// One launch: what it needs from outside, injected so the tests can drive it.
pub struct Launch<E> {
    /// The Engine on the launcher's socket.
    pub engine: Arc<E>,
    /// The daemon container to create.
    pub options: Options,
    /// Where the daemon's output goes.
    pub console: Console,
    /// The launcher's own container ID: [`self_id::self_id_from_mountinfo`] by default.
    pub self_id: SelfId,
    /// The step bounds.
    pub timing: Timing,
    /// The launcher's log, every line with `component=launcher`.
    pub log: LogHandle,
}

impl<E: EngineApi> Launch<E> {
    /// Go's `Run` with its defaults: the mount table, the Go timing, `log` with the launcher's
    /// component.
    #[must_use]
    pub fn new(engine: Arc<E>, options: Options, console: Console, log: &LogHandle) -> Self {
        Self {
            engine,
            options,
            console,
            self_id: Box::new(self_id::self_id_from_mountinfo),
            timing: Timing::default(),
            log: log.with_attrs(&[Attr::string("component", COMPONENT)]),
        }
    }

    /// Creates the daemon container from the launcher's own image, streams its output to the
    /// console and returns its exit status once it is removed. `context` ends the daemon: it is
    /// stopped and its status still returned.
    ///
    /// # Errors
    ///
    /// [`LaunchError`] when the launcher failed; its status is then 1. A daemon it created is
    /// removed on every failed setup.
    pub async fn run(self, context: &CallContext) -> Result<i64, LaunchError> {
        let Self {
            engine,
            options,
            console,
            self_id,
            timing,
            log,
        } = self;
        Runner {
            engine,
            self_id,
            timing,
            log,
        }
        .run(context, &options, console)
        .await
    }
}

/// Go's `runner`: what a launch works with once it runs.
struct Runner<E> {
    engine: Arc<E>,
    self_id: SelfId,
    timing: Timing,
    log: LogHandle,
}

impl<E: EngineApi> Runner<E> {
    async fn run(
        &self,
        context: &CallContext,
        options: &Options,
        console: Console,
    ) -> Result<i64, LaunchError> {
        let (launcher_id, image_id) = self.identify(context).await?;
        self.remove_stale(context, &launcher_id).await?;

        let spec = spec::daemon_spec(options, &image_id, &launcher_id);
        let created = with_context(
            &context.with_timeout(self.timing.call),
            self.engine.create(&spec),
        )
        .await
        .map_err(LaunchError::Create)?;
        for warning in &created.warnings {
            self.log.warn(
                "docker warning on daemon create",
                &[Attr::string("warning", warning.as_str())],
            );
        }

        let mut session = match self.prepare(context, &created.id, console).await {
            Ok(session) => session,
            Err(cause) => return Err(self.abandon(&created.id, cause).await),
        };

        let started = with_context(
            &context.with_timeout(self.timing.call),
            self.engine.start(&created.id),
        )
        .await;
        if let Err(source) = started {
            return Err(self.abandon(&created.id, LaunchError::Start(source)).await);
        }
        self.log.info(
            "daemon started",
            &[
                Attr::string("id", short_id(&created.id)),
                Attr::string("image", image_id.as_str()),
                Attr::string("launcher", short_id(&launcher_id)),
            ],
        );
        self.supervise(context, &mut session).await
    }

    /// The launcher's container ID and the ID of the image it runs. Swarm already pulled the
    /// task's digest, so no pull is needed.
    async fn identify(&self, context: &CallContext) -> Result<(String, String), LaunchError> {
        let launcher_id = (self.self_id)().map_err(LaunchError::Identify)?;
        let inspected = with_context(
            &context.with_timeout(self.timing.call),
            self.engine.inspect(&launcher_id),
        )
        .await
        .map_err(|source| LaunchError::InspectSelf {
            launcher: launcher_id.clone(),
            source,
        })?;
        if !inspected.image.starts_with(IMAGE_ID_PREFIX) {
            return Err(LaunchError::NoImageId(inspected.image));
        }
        Ok((launcher_id, inspected.image))
    }

    /// Removes a daemon container whose setup failed, so a failed launcher never leaves a
    /// daemon behind: Go's `errors.Join(cause, removeErr)`.
    async fn abandon(&self, container_id: &str, cause: LaunchError) -> LaunchError {
        match self.remove_container(container_id).await {
            Ok(()) => cause,
            Err(remove) => LaunchError::Abandoned {
                cause: Box::new(cause),
                remove,
            },
        }
    }

    /// Attaches to the daemon's output, starts copying it and registers the wait for its
    /// removal, all before the daemon starts.
    async fn prepare(
        &self,
        context: &CallContext,
        container_id: &str,
        console: Console,
    ) -> Result<Session, LaunchError> {
        let attached = establish(context, self.timing.call, self.engine.attach(container_id))
            .await
            .map_err(LaunchError::Attach)?;
        // Started before the daemon: copying only after the start could block the daemon on a
        // full attach buffer and deadlock the start.
        let pump = Pump::spawn(attached, console);
        let wait = establish(
            context,
            self.timing.call,
            self.engine.wait_removed(container_id),
        )
        .await
        .map_err(LaunchError::Wait)?;
        Ok(Session {
            id: container_id.to_owned(),
            pump,
            wait,
        })
    }

    /// Waits for the first of: the daemon's removal, a broken wait stream, a lost log stream,
    /// or the launcher's stop.
    async fn supervise(
        &self,
        context: &CallContext,
        session: &mut Session,
    ) -> Result<i64, LaunchError> {
        loop {
            tokio::select! {
                answer = &mut session.wait => match answer {
                    Ok(response) => {
                        let drain = CallContext::new().with_timeout(self.timing.call);
                        return Ok(self.finish(session, &drain, &response).await);
                    }
                    Err(wait_error) => {
                        self.log.error(
                            "daemon wait stream failed; stopping the daemon",
                            &[Attr::string("err", wait_error.as_str())],
                        );
                        return self.shutdown(session, Some(wait_error)).await;
                    }
                },
                copied = session.pump.ended(), if session.pump.running() => {
                    // The end of the stream: the daemon is exiting; its status follows.
                    let Err(copy_error) = copied else { continue };
                    self.log.error(
                        "daemon log stream lost; stopping the daemon",
                        &[Attr::string("err", copy_error.as_str())],
                    );
                    let shutdown = self.shutdown(session, None).await.err();
                    return Err(LaunchError::LogStream {
                        copy: copy_error,
                        shutdown: shutdown.map(Box::new),
                    });
                }
                _ = context.done() => {
                    self.log.info(
                        "stopping daemon",
                        &[Attr::string("id", short_id(&session.id))],
                    );
                    return self.shutdown(session, None).await;
                }
            }
        }
    }

    /// Stops the daemon and waits for its removal and its last log lines, all within one
    /// [`Timing::shutdown_budget`]. `wait_error` is set when the wait stream has already
    /// failed, or it fails here: the removal is then confirmed by polling inspect instead, the
    /// output is still drained, and the status, which nothing reports any more, is 1.
    async fn shutdown(
        &self,
        session: &mut Session,
        wait_error: Option<String>,
    ) -> Result<i64, LaunchError> {
        let budget = CallContext::new().with_timeout(self.timing.shutdown_budget);
        self.stop(&budget, &session.id).await;

        let wait_error = if let Some(wait_error) = wait_error {
            wait_error
        } else {
            tokio::select! {
                answer = &mut session.wait => match answer {
                    Ok(response) => return Ok(self.finish(session, &budget, &response).await),
                    Err(wait_error) => wait_error,
                },
                _ = budget.done() => return Err(LaunchError::Budget(self.budget_exceeded(None))),
            }
        };

        let removal = self.await_removal(&budget, &session.id).await.err();
        self.drain(session, &budget).await;
        Err(LaunchError::WaitStream {
            wait: wait_error,
            removal,
        })
    }

    /// Polls until inspect reports `container_id` gone, or `budget` ends. At the budget, the
    /// latest inspect error, if the last inspect failed, is returned with it.
    async fn await_removal(
        &self,
        budget: &CallContext,
        container_id: &str,
    ) -> Result<(), BudgetExceeded> {
        let mut ticker = tokio::time::interval_at(
            tokio::time::Instant::now() + self.timing.poll,
            self.timing.poll,
        );
        // Go's Ticker keeps one pending tick: a slow inspect is not followed by a burst.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let inspected = with_context(
                &budget.with_timeout(self.timing.call),
                self.engine.inspect(container_id),
            )
            .await;
            let last_inspect = match inspected {
                Err(error) if error.is_not_found() => return Ok(()),
                Err(error) => Some(error),
                Ok(_) => None,
            };
            tokio::select! {
                _ = ticker.tick() => {}
                _ = budget.done() => return Err(self.budget_exceeded(last_inspect)),
            }
        }
    }

    fn budget_exceeded(&self, last_inspect: Option<Error>) -> BudgetExceeded {
        BudgetExceeded {
            budget: self.timing.shutdown_budget,
            last_inspect,
        }
    }

    /// Stops the daemon by ID, never by name, so a launcher cannot stop a daemon that a newer
    /// launcher created.
    async fn stop(&self, budget: &CallContext, container_id: &str) {
        let seconds = i64::try_from(spec::STOP_TIMEOUT.as_secs()).unwrap_or(i64::MAX);
        let stopped = with_context(budget, self.engine.stop(container_id, seconds)).await;
        if let Err(error) = stopped
            && !error.is_not_found()
        {
            self.log.warn(
                "stop daemon failed",
                &[
                    Attr::string("id", short_id(container_id)),
                    Attr::error("err", &error),
                ],
            );
        }
    }

    /// Drains the daemon's remaining output within `deadline` and returns its exit status.
    async fn finish(
        &self,
        session: &mut Session,
        deadline: &CallContext,
        response: &WaitResponse,
    ) -> i64 {
        self.drain(session, deadline).await;
        if let Some(error) = &response.error
            && !error.message.is_empty()
        {
            self.log.warn(
                "daemon wait reported an error",
                &[Attr::string("err", error.message.as_str())],
            );
        }
        self.log.info(
            "daemon exited",
            &[
                Attr::string("id", short_id(&session.id)),
                Attr::int("status", response.status_code),
            ],
        );
        response.status_code
    }

    /// Waits, within `deadline`, for the daemon's output to end.
    async fn drain(&self, session: &mut Session, deadline: &CallContext) {
        if session.pump.running() {
            tokio::select! {
                _ = session.pump.ended() => {}
                _ = deadline.done() => self.log.warn(
                    "daemon log stream not drained before timeout",
                    &[Attr::string("id", short_id(&session.id))],
                ),
            }
        }
        if let Some(Err(copy_error)) = &session.pump.outcome {
            self.log.warn(
                "daemon log stream ended with an error",
                &[Attr::string("err", copy_error.as_str())],
            );
        }
    }
}

/// Go's `establish`: a streaming call with a bounded establishment. The stream it returns is
/// detached from `context` (a signal must not cut it before the daemon's status and last lines
/// arrive) and has no deadline (that would kill it once established); the call is raced against
/// a timer and `context` instead, and dropped, which ends it, when either wins.
async fn establish<T>(
    context: &CallContext,
    timeout: Duration,
    call: impl Future<Output = Result<T, Error>>,
) -> Result<T, EstablishError> {
    tokio::select! {
        biased;
        established = call => established.map_err(EstablishError::Failed),
        () = tokio::time::sleep(timeout) => Err(EstablishError::Timeout),
        why = context.done() => Err(EstablishError::Canceled(why)),
    }
}

/// A created daemon container with its log and wait streams. Dropping it releases both.
struct Session {
    id: String,
    pump: Pump,
    wait: WaitAnswer,
}

/// The copy of the daemon's output to the console, on a task of its own.
struct Pump {
    task: JoinHandle<Result<u64, String>>,
    /// The copy's result once it ended: kept, so a later drain reads it instead of the task.
    outcome: Option<Result<(), String>>,
}

impl Pump {
    fn spawn(mut attached: Attached, console: Console) -> Self {
        let Console {
            mut stdout,
            mut stderr,
        } = console;
        Self {
            task: tokio::spawn(async move {
                stdcopy::std_copy(&mut stdout, &mut stderr, &mut attached).await
            }),
            outcome: None,
        }
    }

    fn running(&self) -> bool {
        self.outcome.is_none()
    }

    /// The copy's end: `Ok` at the end of the stream, its error otherwise. Cancel-safe: the
    /// result is recorded only once the task has ended.
    async fn ended(&mut self) -> Result<(), String> {
        if let Some(outcome) = &self.outcome {
            return outcome.clone();
        }
        let outcome = match (&mut self.task).await {
            Ok(copied) => copied.map(|_| ()),
            Err(join_error) if join_error.is_panic() => {
                std::panic::resume_unwind(join_error.into_panic())
            }
            Err(join_error) => Err(join_error.to_string()),
        };
        self.outcome = Some(outcome.clone());
        outcome
    }
}

impl Drop for Pump {
    fn drop(&mut self) {
        // Go closes the hijacked connection, which ends the copy.
        self.task.abort();
    }
}

/// The first 12 characters of a container ID.
fn short_id(full_id: &str) -> &str {
    full_id.get(..SHORT_ID_LEN).unwrap_or(full_id)
}
