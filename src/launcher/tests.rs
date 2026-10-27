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

//! The launcher tests of `launcher_test.go`, on the paused clock against the recording fake.
//! SRD's launcher tests are ports of the same Go tests (`runs_in_order…`, the stop, the lost
//! log stream, the failed wait), so the Go ones, the behavioural authority, are what is ported;
//! SRD's tests of its node lock, probe, health and aliases have no counterpart here.

use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use super::fake::{
    FakeEngine, STREAM_STDERR, STREAM_STDOUT, TEST_DAEMON_ID, TEST_IMAGE_ID, TEST_LAUNCHER_ID,
    denied, injected, require_stopped, result, start, test_run,
};
use super::self_id::SelfIdError;
use super::spec::LABEL_LAUNCHER;
use super::{BudgetExceeded, EstablishError, Inspected, LaunchError, RemoveError, StaleError};
use crate::docker::{CallContext, ContextError};

/// Go: `TestRun_CallOrder`. Self inspect, list, create, attach, wait, start, in that order; the
/// daemon is created from the launcher's image and owned by its ID, and its status is the
/// launcher's.
#[tokio::test(start_paused = true)]
async fn run_call_order() {
    let run = test_run(&FakeEngine::default());
    let done = start(run.launch, &CallContext::new());
    run.fake.started().await;
    run.fake.exit(0);

    assert_eq!(result(done).await.unwrap(), 0);
    assert_eq!(
        run.fake.calls(),
        [
            format!("inspect {TEST_LAUNCHER_ID}"),
            "list".to_owned(),
            "create".to_owned(),
            "attach".to_owned(),
            "wait".to_owned(),
            "start".to_owned(),
        ]
    );
    let created = run.fake.created().unwrap();
    assert_eq!(created.image, TEST_IMAGE_ID);
    assert_eq!(created.labels[LABEL_LAUNCHER], TEST_LAUNCHER_ID);
    assert!(run.sink.contents().contains(
        "level=INFO daemon started component=launcher id=333333333333 \
         image=sha256:2222222222222222222222222222222222222222222222222222222222222222 \
         launcher=111111111111"
    ));
    assert!(
        run.sink
            .contents()
            .contains("level=INFO daemon exited component=launcher id=333333333333 status=0")
    );
}

/// Go: `TestRun_LongLivedDaemon`. The wait and attach streams have no deadline: a daemon that
/// runs far longer than the call timeout keeps running, and its status is returned.
#[tokio::test(start_paused = true)]
async fn run_long_lived_daemon() {
    const CALL_TIMEOUT: Duration = Duration::from_millis(20);

    let mut run = test_run(&FakeEngine::default());
    run.launch.timing.call = CALL_TIMEOUT;
    let done = start(run.launch, &CallContext::new());
    run.fake.started().await;

    // Negative window: well past the call timeout, which a deadline on either stream would have
    // hit.
    tokio::time::sleep(CALL_TIMEOUT * 10).await;
    assert!(
        !done.is_finished(),
        "launcher returned while the daemon runs"
    );

    run.fake.emit(STREAM_STDOUT, "still here");
    run.fake.exit(3);
    assert_eq!(result(done).await.unwrap(), 3);
    assert!(run.stdout.text().contains("still here"));
}

/// Go: `TestRun_EstablishmentBounded`. A stream whose establishment hangs fails the launch at
/// the call timeout; nothing is started and the created daemon is removed.
#[tokio::test(start_paused = true)]
async fn run_establishment_bounded() {
    for stream in ["attach", "wait"] {
        let fake = FakeEngine::default();
        if stream == "attach" {
            fake.hang_attach();
        } else {
            fake.hang_wait();
        }
        let mut run = test_run(&fake);
        run.launch.timing.call = Duration::from_millis(50);

        let error = result(start(run.launch, &CallContext::new()))
            .await
            .unwrap_err();
        assert!(
            matches!(
                (stream, &error),
                ("attach", LaunchError::Attach(EstablishError::Timeout))
                    | ("wait", LaunchError::Wait(EstablishError::Timeout))
            ),
            "{stream}: {error}"
        );
        assert!(
            !fake.called("start"),
            "daemon started after a failed {stream}"
        );
        assert_eq!(fake.removed(), [TEST_DAEMON_ID]);
    }
}

/// Go: `TestRun_EstablishmentIgnoringCancel`. A Docker call that never answers, whatever its
/// context (the hijacked attach reads the upgrade answer on a raw connection), does not hold the
/// launcher past the timeout. Go releases the result it returns later; here the call is dropped
/// at the timeout, which is its release, and the fake counts it.
#[tokio::test(start_paused = true)]
async fn run_establishment_ignoring_cancel() {
    const CALL_TIMEOUT: Duration = Duration::from_millis(50);

    let fake = FakeEngine::default();
    fake.hang_attach();
    let mut run = test_run(&fake);
    run.launch.timing.call = CALL_TIMEOUT;

    let began = Instant::now();
    let error = result(start(run.launch, &CallContext::new()))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        LaunchError::Attach(EstablishError::Timeout)
    ));
    assert!(began.elapsed() <= CALL_TIMEOUT * 20);
    assert_eq!(fake.removed(), [TEST_DAEMON_ID]);
    assert_eq!(fake.released(), 1, "the hung attach was not released");
}

/// Go: `TestRun_AttachErrorRemovesCreated`.
#[tokio::test(start_paused = true)]
async fn run_attach_error_removes_created() {
    let fake = FakeEngine::default();
    fake.fail_attach(injected());
    let run = test_run(&fake);

    let error = result(start(run.launch, &CallContext::new()))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        LaunchError::Attach(EstablishError::Failed(_))
    ));
    assert_eq!(
        error.to_string(),
        "attach to daemon container: injected failure"
    );
    assert!(!fake.called("start"));
    assert_eq!(fake.removed(), [TEST_DAEMON_ID]);
}

/// Go: `TestRun_StartErrorRemovesCreated`.
#[tokio::test(start_paused = true)]
async fn run_start_error_removes_created() {
    let fake = FakeEngine::default();
    fake.fail_start(injected());
    let run = test_run(&fake);

    let error = result(start(run.launch, &CallContext::new()))
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "start daemon container: injected failure"
    );
    assert_eq!(fake.removed(), [TEST_DAEMON_ID]);
}

/// Go: `TestRun_FinalLogsDrained`. The last lines the daemon prints, after its removal is
/// already reported, still reach the launcher's output before it returns.
#[tokio::test(start_paused = true)]
async fn run_final_logs_drained() {
    let run = test_run(&FakeEngine::default());
    let done = start(run.launch, &CallContext::new());
    run.fake.started().await;

    run.fake.emit(STREAM_STDOUT, "out line");
    run.fake.emit(STREAM_STDERR, "err line");
    run.fake.report_removed(2);
    run.fake.emit(STREAM_STDOUT, "last out");
    run.fake.emit(STREAM_STDERR, "last err");
    run.fake.close_output(None);

    assert_eq!(result(done).await.unwrap(), 2);
    assert_eq!(run.stdout.text(), "out line\nlast out\n");
    assert_eq!(run.stderr.text(), "err line\nlast err\n");
}

/// Go: `TestRun_LogStreamLost`. A log stream that fails while the daemon runs would lose its
/// logs silently: the launcher stops the daemon by ID and fails.
#[tokio::test(start_paused = true)]
async fn run_log_stream_lost() {
    let fake = FakeEngine::default();
    fake.script_stop(Arc::new(|fake| {
        Box::pin(async move { fake.report_removed(0) })
    }));
    let run = test_run(&fake);
    let done = start(run.launch, &CallContext::new());
    fake.started().await;
    fake.close_output(Some("injected failure"));

    let error = result(done).await.unwrap_err();
    assert!(
        matches!(&error, LaunchError::LogStream { copy, shutdown: None } if copy == "injected failure"),
        "{error}"
    );
    assert_eq!(
        error.to_string(),
        "daemon log stream lost while the daemon was running: injected failure"
    );
    require_stopped(&fake);
    assert!(run.sink.contents().contains(
        "level=ERROR daemon log stream lost; stopping the daemon component=launcher \
         err=\"injected failure\""
    ));
}

/// Go: `TestRun_WaitStreamErrorKeepsInspectError`. An inspect that keeps failing while the
/// removal is polled is reported with the budget, not replaced by it.
#[tokio::test(start_paused = true)]
async fn run_wait_stream_error_keeps_inspect_error() {
    let fake = FakeEngine::default();
    fake.script_stop(Arc::new(|_| Box::pin(async {})));
    let mut run = test_run(&fake);
    run.launch.timing.shutdown_budget = Duration::from_millis(100);
    let done = start(run.launch, &CallContext::new());
    fake.started().await;

    fake.fail_inspect(TEST_DAEMON_ID, denied());
    fake.fail_wait("injected failure");

    let error = result(done).await.unwrap_err();
    assert!(
        matches!(
            &error,
            LaunchError::WaitStream {
                removal: Some(BudgetExceeded {
                    last_inspect: Some(_),
                    ..
                }),
                ..
            }
        ),
        "{error}"
    );
    assert_eq!(
        error.to_string(),
        "daemon wait stream failed: injected failure\n\
         daemon not removed within the shutdown budget (100ms)\n\
         last inspect of daemon: permission denied"
    );
}

/// Go: `TestRun_WaitStreamError`. With the wait stream broken, the launcher still stops the
/// daemon by ID, confirms its removal by inspecting it, and drains its last lines.
#[tokio::test(start_paused = true)]
async fn run_wait_stream_error() {
    const STEP: Duration = Duration::from_millis(50);

    let fake = FakeEngine::default();
    fake.script_stop(Arc::new(|fake| {
        Box::pin(async move {
            tokio::spawn(async move {
                tokio::time::sleep(STEP).await;
                fake.emit(STREAM_STDOUT, "daemon last words");
                fake.close_output(None);
                tokio::time::sleep(STEP).await;
                fake.mark_gone();
            });
        })
    }));
    let run = test_run(&fake);
    let done = start(run.launch, &CallContext::new());
    fake.started().await;
    fake.fail_wait("injected failure");

    let error = result(done).await.unwrap_err();
    assert!(
        matches!(&error, LaunchError::WaitStream { wait, removal: None } if wait == "injected failure"),
        "removal not confirmed: {error}"
    );
    require_stopped(&fake);
    let calls = fake.calls();
    let stop = calls
        .iter()
        .position(|call| *call == format!("stop {TEST_DAEMON_ID}"))
        .unwrap();
    assert!(
        calls[stop..].contains(&format!("inspect {TEST_DAEMON_ID}")),
        "calls {calls:?}, want the removal confirmed by inspect after the stop"
    );
    assert!(run.stdout.text().contains("daemon last words"));
    assert!(run.sink.contents().contains(
        "level=ERROR daemon wait stream failed; stopping the daemon component=launcher \
         err=\"injected failure\""
    ));
}

/// Go: `TestRun_WaitStreamErrorBudget`. A daemon that never disappears after a broken wait
/// stream ends the shutdown at the budget.
#[tokio::test(start_paused = true)]
async fn run_wait_stream_error_budget() {
    const BUDGET: Duration = Duration::from_millis(200);

    let fake = FakeEngine::default();
    fake.script_stop(Arc::new(|_| Box::pin(async {})));
    let mut run = test_run(&fake);
    run.launch.timing.shutdown_budget = BUDGET;
    let done = start(run.launch, &CallContext::new());
    fake.started().await;

    let began = Instant::now();
    fake.fail_wait("injected failure");
    let error = result(done).await.unwrap_err();
    assert!(
        matches!(
            &error,
            LaunchError::WaitStream {
                removal: Some(BudgetExceeded {
                    last_inspect: None,
                    ..
                }),
                ..
            }
        ),
        "{error}"
    );
    assert!(began.elapsed() <= BUDGET * 3);
}

/// Go: `TestRun_CancelStopsDaemon`. A signal stops the daemon by ID; the launcher still waits
/// for its removal, drains its output and returns its status.
#[tokio::test(start_paused = true)]
async fn run_cancel_stops_daemon() {
    let context = CallContext::new();
    let run = test_run(&FakeEngine::default());
    let done = start(run.launch, &context);
    run.fake.started().await;
    context.cancel();

    assert_eq!(result(done).await.unwrap(), 0);
    require_stopped(&run.fake);
    assert!(run.stdout.text().contains("daemon stopping"));
    assert!(
        run.sink
            .contents()
            .contains("level=INFO stopping daemon component=launcher id=333333333333")
    );
}

/// Go: `TestRun_ShutdownBudgetShared`. The stop, the wait for the removal and the drain share
/// one budget: each is slow, and together they still end within one budget, not the sum.
#[tokio::test(start_paused = true)]
async fn run_shutdown_budget_shared() {
    const BUDGET: Duration = Duration::from_millis(500);
    const SLOW_STOP: Duration = Duration::from_millis(200);
    const SLOW_REMOVE: Duration = Duration::from_millis(150);

    let fake = FakeEngine::default();
    fake.script_stop(Arc::new(|fake| {
        Box::pin(async move {
            tokio::time::sleep(SLOW_STOP).await;
            tokio::spawn(async move {
                tokio::time::sleep(SLOW_REMOVE).await;
                fake.report_removed(0);
            });
            // The output is never closed: the drain runs out of budget.
        })
    }));
    let context = CallContext::new();
    let mut run = test_run(&fake);
    run.launch.timing.shutdown_budget = BUDGET;
    // Longer than the budget, so a drain bounded by it would show.
    run.launch.timing.call = BUDGET * 10;
    let done = start(run.launch, &context);
    fake.started().await;

    let began = Instant::now();
    context.cancel();
    assert_eq!(result(done).await.unwrap(), 0);
    assert!(
        began.elapsed() <= BUDGET * 2,
        "shutdown took {:?}",
        began.elapsed()
    );
    assert!(run.sink.contents().contains(
        "level=WARN daemon log stream not drained before timeout component=launcher \
         id=333333333333"
    ));
}

/// Go: `TestRun_ShutdownBudgetExhausted`.
#[tokio::test(start_paused = true)]
async fn run_shutdown_budget_exhausted() {
    let fake = FakeEngine::default();
    fake.script_stop(Arc::new(|_| Box::pin(async {})));
    let context = CallContext::new();
    let mut run = test_run(&fake);
    run.launch.timing.shutdown_budget = Duration::from_millis(100);
    let done = start(run.launch, &context);
    fake.started().await;
    context.cancel();

    let error = result(done).await.unwrap_err();
    assert!(matches!(error, LaunchError::Budget(_)), "{error}");
    assert_eq!(
        error.to_string(),
        "daemon not removed within the shutdown budget (100ms)"
    );
    fake.close_output(None);
}

/// Go: `TestRun_EOFBeforeExit`. An output stream that ends before the removal is reported
/// leaves nothing to drain: the status is returned at once, not after a drain timeout.
#[tokio::test(start_paused = true)]
async fn run_eof_before_exit() {
    let mut run = test_run(&FakeEngine::default());
    run.launch.timing.call = Duration::from_secs(10);
    let done = start(run.launch, &CallContext::new());
    run.fake.started().await;
    run.fake.close_output(None);

    let began = Instant::now();
    run.fake.report_removed(5);
    assert_eq!(result(done).await.unwrap(), 5);
    assert!(began.elapsed() <= Duration::from_secs(2));
}

/// Go: `TestRun_SetupErrorsStartNothing`. A setup failure before the create attaches to and
/// starts nothing.
#[tokio::test(start_paused = true)]
async fn run_setup_errors_start_nothing() {
    type Setup = fn(&FakeEngine, &mut super::Launch<FakeEngine>);
    let cases: [(&str, Setup); 4] = [
        ("self id", |_, launch| {
            launch.self_id = Box::new(|| Err(SelfIdError::NotFound));
        }),
        ("self inspect", |fake, _| {
            fake.fail_inspect(TEST_LAUNCHER_ID, injected());
        }),
        ("image tag instead of id", |fake, _| {
            fake.set_inspect(
                TEST_LAUNCHER_ID,
                Inspected {
                    image: "ghcr.io/leinardi/swarm-device-access:1".to_owned(),
                    state: None,
                },
            );
        }),
        ("create", |fake, _| fake.fail_create(injected())),
    ];
    for (name, setup) in cases {
        let fake = FakeEngine::default();
        let mut run = test_run(&fake);
        setup(&fake, &mut run.launch);

        let outcome = result(start(run.launch, &CallContext::new())).await;
        assert!(outcome.is_err(), "{name}: {outcome:?}");
        for call in ["attach", "start"] {
            assert!(
                !fake.called(call),
                "{name}: {call} called: {:?}",
                fake.calls()
            );
        }
    }
}

/// Every call but the two streams is bounded by the call timeout, as Go's per-call contexts
/// bound them (SRD: `a_stalled_body_times_out`, with the timeout here around the call).
#[tokio::test(start_paused = true)]
async fn run_bounds_each_call() {
    let fake = FakeEngine::default();
    fake.hang_inspect(TEST_LAUNCHER_ID);
    let run = test_run(&fake);

    let began = Instant::now();
    let error = result(start(run.launch, &CallContext::new()))
        .await
        .unwrap_err();
    assert_eq!(began.elapsed(), Duration::from_secs(2));
    assert_eq!(
        error.to_string(),
        "inspect launcher container 111111111111: context deadline exceeded"
    );
    assert_eq!(fake.released(), 1);
}

/// A stream is not established once the launcher is stopped: Go's `canceled: <ctx.Err()>`.
#[tokio::test(start_paused = true)]
async fn establish_reports_the_cancellation() {
    let context = CallContext::new();
    context.cancel();
    let error = super::establish::<()>(&context, Duration::from_secs(1), std::future::pending())
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        EstablishError::Canceled(ContextError::Canceled)
    ));
    assert_eq!(error.to_string(), "canceled: context canceled");
}

/// The texts of the Go launcher's errors, `%w` chains and `errors.Join` included.
#[test]
fn errors_read_as_go_words_them() {
    let cases: Vec<(LaunchError, &str)> = vec![
        (
            LaunchError::Identify(SelfIdError::NotFound),
            "identify launcher container: no Docker container ID in the mount table; is the \
             launcher running in a container?",
        ),
        (
            LaunchError::Identify(SelfIdError::Ambiguous),
            "identify launcher container: more than one Docker container ID in the mount table",
        ),
        (
            LaunchError::NoImageId("ghcr.io/leinardi/swarm-device-access:1".to_owned()),
            "launcher container has no content-addressed image ID: \
             \"ghcr.io/leinardi/swarm-device-access:1\"",
        ),
        (
            LaunchError::List(injected()),
            "list swarm-device-access containers: injected failure",
        ),
        (
            LaunchError::Create(injected()),
            "create daemon container: injected failure",
        ),
        (
            LaunchError::Wait(EstablishError::Timeout),
            "wait for daemon container: not established before timeout",
        ),
        (
            LaunchError::Abandoned {
                cause: Box::new(LaunchError::Start(injected())),
                remove: RemoveError {
                    id: TEST_DAEMON_ID.to_owned(),
                    source: denied(),
                },
            },
            "start daemon container: injected failure\n\
             remove container 333333333333: permission denied",
        ),
        (
            LaunchError::RemoveStale {
                id: TEST_DAEMON_ID.to_owned(),
                source: RemoveError {
                    id: TEST_DAEMON_ID.to_owned(),
                    source: denied(),
                },
            },
            "remove stale daemon 333333333333: remove container 333333333333: permission denied",
        ),
        (
            LaunchError::Stale(StaleError::OwnedByRunningLauncher {
                owner: TEST_LAUNCHER_ID.to_owned(),
            }),
            "daemon owned by running launcher 111111111111",
        ),
        (
            LaunchError::LogStream {
                copy: "unrecognized stream: 7".to_owned(),
                shutdown: Some(Box::new(LaunchError::Budget(BudgetExceeded {
                    budget: Duration::from_secs(20),
                    last_inspect: None,
                }))),
            },
            "daemon log stream lost while the daemon was running: unrecognized stream: 7\n\
             daemon not removed within the shutdown budget (20s)",
        ),
    ];
    for (error, want) in cases {
        assert_eq!(error.to_string(), want);
    }
}

/// A stdout nobody reads: every write stays pending.
struct Stuck;

impl tokio::io::AsyncWrite for Stuck {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _context: &mut std::task::Context<'_>,
        _data: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::task::Poll::Pending
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Pending
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Pending
    }
}

/// SRD: `a_blocked_stdout_does_not_stall_the_launcher`. A stdout that blocks, with daemon output
/// queued behind it, stalls nothing but the copy: the stop is still seen, the daemon stopped by
/// ID, and the launcher returns its status within the shutdown budget.
#[tokio::test(start_paused = true)]
async fn a_blocked_stdout_does_not_stall_the_launcher() {
    let context = CallContext::new();
    let mut run = test_run(&FakeEngine::default());
    run.launch.console.stdout = Box::new(Stuck);
    let budget = run.launch.timing.shutdown_budget;
    let done = start(run.launch, &context);
    run.fake.started().await;
    for _ in 0..1024 {
        run.fake.emit(STREAM_STDOUT, "line");
        tokio::task::yield_now().await;
    }

    let began = Instant::now();
    context.cancel();
    assert_eq!(result(done).await.unwrap(), 0);
    assert!(began.elapsed() <= budget, "took {:?}", began.elapsed());
    require_stopped(&run.fake);
    assert!(run.sink.contents().contains(
        "level=WARN daemon log stream not drained before timeout component=launcher \
         id=333333333333"
    ));
}

/// SRD: `a_stalled_drain_still_removes_the_daemon`. An attach stream that stays open after the
/// daemon's removal is drained for at most the call timeout, and the daemon's status is the
/// launcher's.
#[tokio::test(start_paused = true)]
async fn a_stalled_drain_still_returns_the_status() {
    let run = test_run(&FakeEngine::default());
    let call = run.launch.timing.call;
    let done = start(run.launch, &CallContext::new());
    run.fake.started().await;

    let began = Instant::now();
    run.fake.report_removed(7);
    assert_eq!(result(done).await.unwrap(), 7);
    assert_eq!(began.elapsed(), call);
    assert!(run.fake.stops().is_empty());
}
