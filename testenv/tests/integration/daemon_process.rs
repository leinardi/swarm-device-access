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
// Adapted from swarm-scheduler-exporter/testenv/tests/integration/launcher.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The daemon under test as a child process, and the JSON log records the tests wait on: Go's
//! `daemonProc`, `launchDaemon`, `waitN`, `waitReady` and `collect` (`helpers_test.go`), on the
//! child-process plumbing of swarm-scheduler-exporter's exporter launcher.
//!
//! The child runs the binary under test with `--log-format=json --log-level=debug`, then the
//! test's flags, all spelled with two dashes as the Rust binary takes them (Q1). Its stdout and
//! stderr are read line by line, and every line that is a JSON object becomes a record; Go merged
//! the two streams into one pipe, which only changes the order the lines interleave in. The
//! records of a container's log stream ([`follow_logs`]) are collected the same way. The child is
//! killed when the test ends, as Go killed it, and its whole output is logged if the test failed;
//! Go logged every line as it came, which `go test -v` showed for every test.

use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bollard::container::LogOutput;
use bollard::query_parameters::LogsOptions;
use futures_util::StreamExt as _;
use serde_json::{Map, Value};
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::process::Command;
use tokio::sync::watch;
use tokio::time::Instant;

use crate::docker::short_id;
use crate::testing::{Fatal, TestHandle, docker, suite};

/// How long to wait for an expected daemon log record, the startup subscription included.
pub const DETECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Messages of the daemon's log records the tests wait for: the Rust daemon logs Go's.
pub const MSG_READY: &str = "subscribed to docker events";
pub const MSG_PROCESSED: &str = "container processed";
pub const MSG_SKIPPED_POLICY: &str = "container skipped by policy";
pub const MSG_INVALID_LABELS: &str = "container skipped: invalid policy labels";
pub const MSG_WATCHER_STARTED: &str = "systemd reload watcher started";
pub const MSG_WATCHER_UNAVAILABLE: &str =
    "systemd reload handling unavailable; retrying in the background";
/// Logged once per reconcile request, with its epoch.
pub const MSG_REQUEST_DONE: &str = "config reload complete";

/// One JSON log line of the daemon.
pub type Record = Map<String, Value>;

/// A string attribute, or "" when it is missing or not a string.
pub fn text<'record>(record: &'record Record, key: &str) -> &'record str {
    record.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// A numeric attribute; Go decoded numbers as float64 and truncated them to int.
pub fn number(record: &Record, key: &str) -> Option<i64> {
    record.get(key).and_then(Value::as_i64)
}

/// Selects log records.
pub type Matcher<'matcher> = &'matcher dyn Fn(&Record) -> bool;

/// Go's `forContainer`: records with `message` about `container_id`.
pub fn for_container(message: &str, container_id: &str) -> impl Fn(&Record) -> bool {
    let (message, container_id) = (message.to_owned(), container_id.to_owned());
    move |record| text(record, "msg") == message && text(record, "id") == container_id
}

/// Go's `withMsg`: records with `message`.
pub fn with_msg(message: &str) -> impl Fn(&Record) -> bool {
    let message = message.to_owned();
    move |record| text(record, "msg") == message
}

/// What the collecting tasks and the waits share.
struct Shared {
    records: Mutex<Vec<Record>>,
    transcript: Mutex<String>,
}

/// A daemon's JSON log, collected line by line from its sources (a child's two pipes, or a
/// container's log stream) until every source has ended.
pub struct DaemonLog {
    shared: Arc<Shared>,
    /// The number of records so far; changes on every append.
    changed: watch::Receiver<usize>,
    /// True once every source has ended.
    exited: watch::Receiver<bool>,
}

/// The writing side of a [`DaemonLog`]: one per source, the last dropped ending the log.
struct Feed {
    shared: Arc<Shared>,
    changed: Arc<watch::Sender<usize>>,
    open: Arc<watch::Sender<usize>>,
    exited: Arc<watch::Sender<bool>>,
    pending: Vec<u8>,
}

impl DaemonLog {
    /// A log with `sources` feeds.
    fn new(sources: usize) -> (Self, Vec<Feed>) {
        let shared = Arc::new(Shared {
            records: Mutex::new(Vec::new()),
            transcript: Mutex::new(String::new()),
        });
        let (changed_tx, changed) = watch::channel(0);
        let (exited_tx, exited) = watch::channel(false);
        let (changed_tx, exited_tx) = (Arc::new(changed_tx), Arc::new(exited_tx));
        let open = Arc::new(watch::Sender::new(sources));
        let feeds = (0..sources)
            .map(|_| Feed {
                shared: Arc::clone(&shared),
                changed: Arc::clone(&changed_tx),
                open: Arc::clone(&open),
                exited: Arc::clone(&exited_tx),
                pending: Vec::new(),
            })
            .collect();
        (
            Self {
                shared,
                changed,
                exited,
            },
            feeds,
        )
    }

    /// Go's `matching`: the records `matcher` selects so far.
    pub fn matching(&self, matcher: Matcher<'_>) -> Vec<Record> {
        self.shared
            .records
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|record| matcher(record))
            .cloned()
            .collect()
    }

    /// Everything the sources wrote so far.
    pub fn transcript(&self) -> String {
        self.shared
            .transcript
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Waits until every source has ended.
    pub async fn wait_exited(&self) {
        let mut exited = self.exited.clone();
        let _ = exited.wait_for(|done| *done).await;
    }

    /// Go's `waitN`: waits until at least `count` records match and returns them. It fails the
    /// test when [`DETECT_TIMEOUT`] passes or the test's context ends first, or when the daemon
    /// exits.
    pub async fn wait_n(
        &self,
        test: &TestHandle,
        what: &str,
        matcher: Matcher<'_>,
        count: usize,
    ) -> Result<Vec<Record>, Fatal> {
        let deadline = Instant::now() + DETECT_TIMEOUT;
        let mut changed = self.changed.clone();
        loop {
            changed.mark_unchanged();
            let found = self.matching(matcher);
            if found.len() >= count {
                return Ok(found);
            }
            let next = async {
                tokio::select! {
                    _ = changed.changed() => Ok(()),
                    () = self.wait_exited() => Err(None),
                    () = tokio::time::sleep_until(deadline) => {
                        Err(Some("context deadline exceeded".to_owned()))
                    }
                }
            };
            match test.ctx().run(next).await {
                Ok(Ok(())) => {}
                Ok(Err(None)) => {
                    let found = self.matching(matcher);
                    if found.len() >= count {
                        return Ok(found);
                    }
                    return Err(test.fatal(format!(
                        "daemon exited before logging {what} (want {count}, got {})",
                        found.len()
                    )));
                }
                Ok(Err(Some(cause))) => {
                    return Err(self.timed_out(test, what, matcher, count, &cause));
                }
                Err(ended) => {
                    return Err(self.timed_out(test, what, matcher, count, &ended.to_string()));
                }
            }
        }
    }

    fn timed_out(
        &self,
        test: &TestHandle,
        what: &str,
        matcher: Matcher<'_>,
        count: usize,
        cause: &str,
    ) -> Fatal {
        test.fatal(format!(
            "waiting for {what} (want {count}, got {}): {cause}",
            self.matching(matcher).len()
        ))
    }

    /// Go's `wait`: waits until one record matches and returns it.
    pub async fn wait(
        &self,
        test: &TestHandle,
        what: &str,
        matcher: Matcher<'_>,
    ) -> Result<Record, Fatal> {
        let mut found = self.wait_n(test, what, matcher, 1).await?;
        Ok(found.swap_remove(0))
    }

    /// Go's `waitReady`: waits until the daemon has subscribed to Docker events, its reload
    /// watcher has subscribed to systemd or given up for now, and the passes those requested
    /// have finished. The startup pass is one request and a watcher subscription another, so with
    /// both the second completion is the last: every later `container processed` record comes
    /// from what the test does next, not from a pass still running.
    pub async fn wait_ready(&self, test: &TestHandle) -> Result<(), Fatal> {
        self.wait_n(
            test,
            "daemon subscription to docker events",
            &with_msg(MSG_READY),
            1,
        )
        .await?;
        let watcher = self
            .wait(test, "systemd reload watcher outcome", &|record| {
                let message = text(record, "msg");
                message == MSG_WATCHER_STARTED || message == MSG_WATCHER_UNAVAILABLE
            })
            .await?;
        let requests = if text(&watcher, "msg") == MSG_WATCHER_STARTED {
            2
        } else {
            1
        };
        self.wait(test, "startup passes complete", &|record| {
            text(record, "msg") == MSG_REQUEST_DONE && number(record, "epoch") == Some(requests)
        })
        .await?;
        Ok(())
    }
}

impl Feed {
    /// Adds `bytes` to the transcript and every complete line that is a JSON object to the
    /// records. Lines that are not JSON (a panic, say) are only kept in the transcript.
    fn push(&mut self, bytes: &[u8]) {
        self.shared
            .transcript
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_str(&String::from_utf8_lossy(bytes));
        self.pending.extend_from_slice(bytes);
        while let Some(newline) = self.pending.iter().position(|&byte| byte == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=newline).collect();
            self.record(&line);
        }
    }

    fn record(&self, line: &[u8]) {
        let Ok(Value::Object(record)) = serde_json::from_slice::<Value>(line) else {
            return;
        };
        let count = {
            let mut records = self
                .shared
                .records
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            records.push(record);
            records.len()
        };
        self.changed.send_replace(count);
    }
}

impl Drop for Feed {
    /// A source that ends flushes a last line without a newline, and the last one to end marks
    /// the log exited.
    fn drop(&mut self) {
        if !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            self.record(&line);
        }
        self.open.send_modify(|open| *open = open.saturating_sub(1));
        if *self.open.borrow() == 0 {
            self.exited.send_replace(true);
        }
    }
}

/// Copies `pipe` into `feed` until it ends.
fn pump(mut pipe: impl AsyncRead + Unpin + Send + 'static, mut feed: Feed) {
    tokio::spawn(async move {
        let mut chunk = [0_u8; 8192];
        while let Ok(read) = pipe.read(&mut chunk).await {
            if read == 0 {
                break;
            }
            feed.push(&chunk[..read]);
        }
    });
}

/// Go's `launchDaemon`: starts the binary under test with JSON debug logging plus `flags`, waits
/// until it is ready ([`DaemonLog::wait_ready`]) and returns its log. The process is killed when
/// the test ends.
pub async fn launch_daemon(test: &TestHandle, flags: &[&str]) -> Result<Arc<DaemonLog>, Fatal> {
    let mut child = Command::new(&suite().binary)
        .args(["--log-format=json", "--log-level=debug"])
        .args(flags)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| test.fatal(format!("start daemon: {error}")))?;
    let (log, feeds) = DaemonLog::new(2);
    let log = Arc::new(log);
    let mut feeds = feeds.into_iter();
    if let (Some(stdout), Some(stderr), Some(out_feed), Some(err_feed)) = (
        child.stdout.take(),
        child.stderr.take(),
        feeds.next(),
        feeds.next(),
    ) {
        pump(stdout, out_feed);
        pump(stderr, err_feed);
    } else {
        return Err(test.fatal("start daemon: no output pipes"));
    }

    let (cleanup_test, cleanup_log) = (test.clone(), Arc::clone(&log));
    test.cleanup(move || async move {
        let _ = child.start_kill();
        cleanup_log.wait_exited().await;
        let _ = child.wait().await;
        if cleanup_test.failed() {
            cleanup_test.log(format!("daemon output:\n{}", cleanup_log.transcript()));
        }
    });

    log.wait_ready(test).await?;
    Ok(log)
}

/// Go's `followLogs`: collects the JSON log records of `container_id` until its log stream ends.
/// Its output is logged if the test failed.
pub fn follow_logs(test: &TestHandle, container_id: &str) -> Arc<DaemonLog> {
    let (log, feeds) = DaemonLog::new(1);
    let log = Arc::new(log);
    let mut stream = docker().logs(
        container_id,
        Some(LogsOptions {
            follow: true,
            stdout: true,
            stderr: true,
            ..LogsOptions::default()
        }),
    );
    let short = short_id(container_id).to_owned();
    if let Some(mut feed) = feeds.into_iter().next() {
        tokio::spawn(async move {
            while let Some(Ok(output)) = stream.next().await {
                match output {
                    LogOutput::StdOut { message }
                    | LogOutput::StdErr { message }
                    | LogOutput::Console { message } => feed.push(&message),
                    LogOutput::StdIn { .. } => {}
                }
            }
        });
    }
    let (cleanup_test, cleanup_log) = (test.clone(), Arc::clone(&log));
    test.cleanup(move || async move {
        if cleanup_test.failed() {
            cleanup_test.log(format!(
                "container {short} output:\n{}",
                cleanup_log.transcript()
            ));
        }
    });
    log
}
