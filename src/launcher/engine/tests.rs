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

//! The launcher's write calls on bollard, against docker-client's fake engine on a unix socket:
//! each request's shape, the daemon spec as bollard's create model, the attach's demultiplexed
//! output, the wait's answers (an exit status, and a container that is already gone), and what a
//! failed or stalled write does: it ends the call and is sent once, never again.

use std::time::Duration;

use super::super::spec::{DAEMON_NAME, Options, daemon_spec};
use super::{Created, Engine, EngineApi as _, WaitResponse};
use crate::docker::attach::Output;
use crate::docker::bollard::models::ContainerCreateBody;
use crate::docker::test_util::{FakeEngine, Reply, Request};
use crate::docker::{Client, ErrorKind};

const ID: &str = "3333333333333333333333333333333333333333333333333333333333333333";
const CALL: Duration = Duration::from_millis(300);

fn engine(routes: &[(&str, Reply)]) -> (FakeEngine, Engine) {
    let fake = FakeEngine::start(routes).unwrap();
    let client = Client::unix(fake.socket().to_str().unwrap(), CALL, CALL).unwrap();
    (fake, Engine::new(client))
}

fn calls(fake: &FakeEngine) -> Vec<Request> {
    fake.requests()
        .into_iter()
        .filter(|request| request.path() != "/version")
        .collect()
}

fn options() -> Options {
    Options {
        host_docker_socket: "/var/run/docker.sock".to_owned(),
        ..Options::default()
    }
}

/// One frame of the multiplexed attach stream: the stream's number, three zeros, the size.
fn frame(stream: u8, payload: &str) -> Vec<u8> {
    let mut bytes = vec![stream, 0, 0, 0];
    bytes.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_be_bytes());
    bytes.extend_from_slice(payload.as_bytes());
    bytes
}

/// The reads: an inspect keeps the image and the running flag, and the list asks for every
/// container in every state named as the daemon.
#[tokio::test]
async fn inspect_and_list_ask_for_what_the_launcher_reads() {
    let (fake, engine) = engine(&[
        (
            &format!("/containers/{ID}/json"),
            Reply::ok(r#"{"Image":"sha256:abc","State":{"Running":true}}"#),
        ),
        (
            "/containers/json",
            Reply::ok(r#"[{"Id":"d1","Labels":{"k":"v"}}]"#),
        ),
    ]);

    let inspected = engine.inspect(ID).await.unwrap();
    assert_eq!(inspected.image, "sha256:abc");
    assert!(inspected.state.unwrap().running);
    let listed = engine.list_daemons().await.unwrap();
    assert_eq!(listed[0].id, "d1");
    assert_eq!(listed[0].labels["k"], "v");

    let requests = calls(&fake);
    assert_eq!(requests[0].method, "GET");
    let query = requests[1].query();
    assert!(
        query.contains("all=true") || query.contains("all=1"),
        "{query}"
    );
    assert!(query.contains("filters="), "{query}");
    assert!(
        query.contains(&format!("%5E%2F{DAEMON_NAME}%24")) || query.contains(DAEMON_NAME),
        "{query}"
    );
}

/// The writes: a forced remove, a create named as the daemon, a start and a stop with its
/// timeout in seconds, each a request of its own.
#[tokio::test]
async fn the_writes_are_one_request_each() {
    let (fake, engine) = engine(&[
        (&format!("/containers/{ID}"), Reply::status(204)),
        (
            "/containers/create",
            Reply::Json {
                status: 201,
                body: format!(r#"{{"Id":"{ID}","Warnings":["careful"]}}"#),
            },
        ),
        (&format!("/containers/{ID}/start"), Reply::status(204)),
        (&format!("/containers/{ID}/stop"), Reply::status(204)),
    ]);
    let spec = daemon_spec(&options(), "sha256:abc", ID);

    engine.remove(ID).await.unwrap();
    let created = engine.create(&spec).await.unwrap();
    assert_eq!(
        created,
        Created {
            id: ID.to_owned(),
            warnings: vec!["careful".to_owned()]
        }
    );
    engine.start(ID).await.unwrap();
    engine.stop(ID, 10).await.unwrap();

    let requests = calls(&fake);
    let shapes: Vec<_> = requests
        .iter()
        .map(|request| (request.method.as_str(), request.path().to_owned()))
        .collect();
    assert_eq!(
        shapes,
        [
            ("DELETE", format!("/containers/{ID}")),
            ("POST", "/containers/create".to_owned()),
            ("POST", format!("/containers/{ID}/start")),
            ("POST", format!("/containers/{ID}/stop")),
        ]
    );
    assert!(requests[0].query().contains("force=1") || requests[0].query().contains("force=true"));
    assert!(requests[1].query().contains(&format!("name={DAEMON_NAME}")));
    assert_eq!(requests[3].query(), "t=10");
}

/// The daemon spec, written by hand as Go's JSON, is a body bollard's create model reads whole:
/// what the engine is sent is that model, so a field it dropped would be a field the daemon lost.
#[test]
fn the_daemon_spec_is_a_create_body_that_keeps_what_the_daemon_needs() {
    let spec = daemon_spec(&options(), "sha256:abc", ID);
    let body: ContainerCreateBody = serde_json::from_str(&spec.body()).unwrap();
    assert_eq!(body.image.as_deref(), Some(spec.image.as_str()));
    assert_eq!(body.cmd, spec.cmd);
    assert_eq!(
        body.stop_timeout,
        Some(i64::try_from(spec.stop_timeout).unwrap())
    );
    assert_eq!(
        body.labels
            .unwrap()
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>(),
        spec.labels
    );
    let host = body.host_config.unwrap();
    assert_eq!(host.privileged, Some(true));
    assert_eq!(host.auto_remove, Some(spec.auto_remove));
    assert_eq!(host.pid_mode.as_deref(), Some(spec.pid_mode));
    assert_eq!(host.userns_mode.as_deref(), Some(spec.userns_mode));
    assert_eq!(host.network_mode.as_deref(), Some(spec.network_mode));
    assert_eq!(
        host.cgroupns_mode.map(|mode| mode.to_string()).as_deref(),
        Some(spec.cgroupns_mode)
    );
    let mounts = host.mounts.unwrap();
    assert_eq!(mounts.len(), spec.mounts.len());
    for (mount, wanted) in mounts.iter().zip(&spec.mounts) {
        assert_eq!(mount.source.as_deref(), Some(wanted.source.as_str()));
        assert_eq!(mount.target.as_deref(), Some(wanted.target.as_str()));
        assert_eq!(mount.read_only.unwrap_or(false), wanted.read_only);
    }
}

/// The attach's output arrives demultiplexed, stdout and stderr apart, and ends when the daemon's
/// output does.
#[tokio::test]
async fn attach_demultiplexes_the_output() {
    let mut stream = frame(1, "out line\n");
    stream.extend(frame(2, "err line\n"));
    let (fake, engine) = engine(&[(&format!("/containers/{ID}/attach"), Reply::upgrade(&stream))]);

    let mut attached = engine.attach(ID).await.unwrap();
    let mut pieces = Vec::new();
    while let Some(piece) = attached.recv().await {
        pieces.push(piece.unwrap());
    }
    assert_eq!(
        pieces,
        [
            Output::Stdout(b"out line\n".to_vec()),
            Output::Stderr(b"err line\n".to_vec())
        ]
    );
    let request = calls(&fake).remove(0);
    assert_eq!(request.method, "POST");
    for parameter in ["stdout=true", "stderr=true", "stream=true"] {
        assert!(
            request.query().contains(parameter)
                || request.query().contains(&parameter.replace("true", "1")),
            "{}",
            request.query()
        );
    }
}

/// The wait for the removal answers with the exit status, a container the engine no longer has
/// (an auto-removed daemon the wait came late for) as removed with its status unknown, and any
/// other failure as the text of the failure.
#[tokio::test]
async fn wait_answers_the_exit_status_or_that_the_container_is_gone() {
    let (fake, engine) = engine(&[(
        &format!("/containers/{ID}/wait"),
        Reply::ok(r#"{"StatusCode":3}"#),
    )]);
    let answer = engine.wait_removed(ID).await.unwrap().await.unwrap();
    assert_eq!(
        answer,
        WaitResponse {
            status_code: 3,
            error: None
        }
    );
    let request = calls(&fake).remove(0);
    assert_eq!(request.method, "POST");
    assert!(
        request.query().contains("condition=removed"),
        "{}",
        request.query()
    );

    let (_gone, engine) = engine_without_routes();
    let answer = engine.wait_removed(ID).await.unwrap().await.unwrap();
    assert_eq!(answer.status_code, 1);
    assert!(answer.error.unwrap().message.contains("already removed"));

    // An engine that refuses the wait fails the call itself: the waiter was never registered.
    let (_broken, engine) = self::engine(&[(
        &format!("/containers/{ID}/wait"),
        Reply::error(500, "engine broke"),
    )]);
    let refusal = engine.wait_removed(ID).await.err().unwrap();
    assert!(refusal.to_string().contains("engine broke"), "{refusal}");

    // A body that is not an answer is the answer's failure, after the waiter was registered.
    let (_garbled, engine) =
        self::engine(&[(&format!("/containers/{ID}/wait"), Reply::ok("not json"))]);
    let failure = engine.wait_removed(ID).await.unwrap().await.unwrap_err();
    assert!(!failure.is_empty());
}

/// The wait for the removal is registered with the engine, its headers answered, before
/// `wait_removed` returns, so the start that follows cannot overtake it: a daemon that exits the
/// moment it starts (and is auto-removed) still answers its exit code. The engine here answers
/// the wait's headers late and its body at once, as one whose container has already exited does.
#[tokio::test]
async fn a_daemon_that_exits_at_once_still_answers_its_exit_code() {
    let (fake, engine) = engine(&[
        (
            &format!("/containers/{ID}/wait"),
            Reply::Delayed {
                delay: Duration::from_millis(100),
                reply: Box::new(Reply::HeadersThenBody {
                    body_delay: Duration::ZERO,
                    body: r#"{"StatusCode":7}"#.to_owned(),
                }),
            },
        ),
        (&format!("/containers/{ID}/start"), Reply::status(204)),
    ]);

    let answer = engine.wait_removed(ID).await.unwrap();
    engine.start(ID).await.unwrap();

    let order: Vec<_> = calls(&fake)
        .iter()
        .map(|request| request.path().to_owned())
        .collect();
    assert_eq!(
        order,
        [
            format!("/containers/{ID}/wait"),
            format!("/containers/{ID}/start")
        ],
        "the start was sent before the wait was registered"
    );
    let answer = answer.await.unwrap();
    assert_eq!(answer.status_code, 7);
    assert_eq!(answer.error, None);
}

fn engine_without_routes() -> (FakeEngine, Engine) {
    engine(&[])
}

/// A write the engine refuses fails the call with the engine's answer and is sent once: the
/// launcher decides what a failure means, and nothing here sends a create, a start, a stop or a
/// remove a second time.
#[tokio::test]
async fn a_failed_write_is_reported_and_not_retried() {
    let (fake, engine) = engine(&[
        ("/containers/create", Reply::error(500, "create refused")),
        (
            &format!("/containers/{ID}/start"),
            Reply::error(500, "start refused"),
        ),
        (
            &format!("/containers/{ID}/stop"),
            Reply::error(500, "stop refused"),
        ),
        (&format!("/containers/{ID}"), Reply::conflict()),
    ]);
    let spec = daemon_spec(&options(), "sha256:abc", ID);

    let created = engine.create(&spec).await.unwrap_err();
    assert!(created.to_string().contains("create refused"), "{created}");
    assert_eq!(created.kind(), ErrorKind::Other);
    let started = engine.start(ID).await.unwrap_err();
    assert!(started.to_string().contains("start refused"), "{started}");
    let stopped = engine.stop(ID, 10).await.unwrap_err();
    assert!(stopped.to_string().contains("stop refused"), "{stopped}");
    let removed = engine.remove(ID).await.unwrap_err();
    assert_eq!(removed.kind(), ErrorKind::Conflict);

    let paths: Vec<_> = calls(&fake)
        .iter()
        .map(|request| request.path().to_owned())
        .collect();
    assert_eq!(
        paths,
        [
            "/containers/create".to_owned(),
            format!("/containers/{ID}/start"),
            format!("/containers/{ID}/stop"),
            format!("/containers/{ID}"),
        ],
        "each write was sent once"
    );
}

/// A write whose answer stalls after its headers is cut by the client's whole-call bound, as a
/// timeout, and is not sent again. (The create is the write whose body is read; the others'
/// answers are their status.)
#[tokio::test]
async fn a_stalled_write_times_out_and_is_not_retried() {
    let (fake, engine) = engine(&[("/containers/create", Reply::Stalled)]);
    let spec = daemon_spec(&options(), "sha256:abc", ID);
    let error = tokio::time::timeout(Duration::from_secs(5), engine.create(&spec))
        .await
        .expect("the create hung")
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Timeout);
    assert_eq!(calls(&fake).len(), 1);
}

/// A container the engine does not have is a not-found error on every call by ID, which is what
/// the launcher reads as "already gone".
#[tokio::test]
async fn a_missing_container_is_not_found() {
    let (_fake, engine) = engine_without_routes();
    assert!(engine.inspect(ID).await.unwrap_err().is_not_found());
    assert!(engine.remove(ID).await.unwrap_err().is_not_found());
    assert!(engine.stop(ID, 10).await.unwrap_err().is_not_found());
}

/// An empty ID is refused before any request.
#[tokio::test]
async fn an_empty_id_is_refused_without_a_request() {
    let (fake, engine) = engine_without_routes();
    assert!(engine.stop(" ", 10).await.is_err());
    assert!(engine.remove("").await.is_err());
    assert!(fake.requests().is_empty());
}
