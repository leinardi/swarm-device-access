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

//! What the daemon's client does on the wire, against docker-client's fake engine on a unix
//! socket: the conversion of the answers, the negotiated version and the `User-Agent`, a missing
//! container, an event stream that is closed and opened again from where it was, a body that
//! stalls after its headers, and an engine that is gone. The bollard calls themselves are
//! docker-client's to test.

use std::time::Duration;

use docker_client::ErrorKind as ClientKind;
use docker_client::test_util::{FakeEngine, Reply, Request};
use jiff::Timestamp;

use super::{
    CallContext, Client, DockerApi as _, Error, ErrorKind, EventStream as _, Filters,
    StreamErrorKind, with_context,
};

const ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const CALL: Duration = Duration::from_millis(300);

fn client(engine: &FakeEngine, call: Duration) -> Client {
    Client::unix(engine.socket().to_str().unwrap(), call, call).unwrap()
}

fn event(action: &str, id: &str, nano: i64) -> String {
    format!(
        r#"{{"Type":"container","Action":"{action}","Actor":{{"ID":"{id}","Attributes":{{"name":"web"}}}},"time":{},"timeNano":{nano}}}"#,
        nano / 1_000_000_000
    )
}

/// The requests that are not the version negotiation.
fn calls(engine: &FakeEngine) -> Vec<Request> {
    engine
        .requests()
        .into_iter()
        .filter(|request| request.path() != "/version")
        .collect()
}

/// The answers convert to the fields the daemon reads, the first call asks the engine for its API
/// version (bollard 0.21.1 sends its requests without a version prefix whatever it negotiated:
/// its `Url::join` of an absolute path drops it) and every request carries the program's
/// `User-Agent`.
#[tokio::test]
async fn the_answers_convert_and_the_requests_carry_the_user_agent() {
    let inspect = format!(
        r#"{{"Id":"{ID}","State":{{"Running":true,"Pid":42,"StartedAt":"2026-01-02T03:04:05.000000006Z"}},"Config":{{"Labels":{{"a":"b"}}}},"HostConfig":{{"Privileged":false}},"Mounts":[{{"Type":"bind","Source":"/dev/null","Destination":"/dev/null"}}]}}"#
    );
    let engine = FakeEngine::start(&[
        (
            "/version",
            Reply::ok(r#"{"ApiVersion":"1.45","MinAPIVersion":"1.24"}"#),
        ),
        ("/_ping", Reply::ok("OK")),
        (
            "/containers/json",
            Reply::ok(&format!(r#"[{{"Id":"{ID}"}}]"#)),
        ),
        (&format!("/containers/{ID}/json"), Reply::ok(&inspect)),
    ])
    .unwrap();
    let docker = client(&engine, CALL);

    docker.ping().await.unwrap();
    let listed = docker.container_list().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, ID);
    let inspected = docker.container_inspect(&format!(" {ID}\n")).await.unwrap();
    assert_eq!(inspected.id, ID);
    let state = inspected.state.unwrap();
    assert!(state.running);
    assert_eq!(state.pid, 42);
    assert_eq!(state.started_at, "2026-01-02T03:04:05.000000006Z");
    assert_eq!(inspected.config.unwrap().labels["a"], "b");
    assert!(!inspected.host_config.unwrap().privileged);
    assert_eq!(inspected.mounts[0].destination, "/dev/null");

    // The version is asked for once, before the first call.
    let all = engine.requests();
    assert_eq!(all[0].path(), "/version");
    assert_eq!(
        all.iter()
            .filter(|request| request.path() == "/version")
            .count(),
        1
    );
    let requests = calls(&engine);
    let paths: Vec<_> = requests.iter().map(Request::path).collect();
    assert_eq!(
        paths,
        [
            "/_ping",
            "/containers/json",
            &format!("/containers/{ID}/json")
        ]
    );
    assert!(requests.iter().all(|request| request.method == "GET"));
    assert!(requests.iter().all(|request| {
        request
            .header("user-agent")
            .is_some_and(|agent| agent.starts_with("swarm-device-access/"))
    }));
    // The list is of the running containers: no `all`, no filter.
    assert_eq!(requests[1].query(), "");
}

/// An engine that cannot negotiate (no `/version`) is spoken to at bollard's version, once: the
/// failed negotiation is not repeated before every call.
#[tokio::test]
async fn a_failed_negotiation_is_not_an_error_and_not_repeated() {
    let engine = FakeEngine::start(&[("/_ping", Reply::ok("OK"))]).unwrap();
    let docker = client(&engine, CALL);
    docker.ping().await.unwrap();
    docker.ping().await.unwrap();
    let versions = engine
        .requests()
        .iter()
        .filter(|request| request.path() == "/version")
        .count();
    assert_eq!(versions, 1);
}

/// A container the engine does not have is a not-found error, the kind the processor reads as
/// "the container is gone", and its text is the engine's.
#[tokio::test]
async fn a_missing_container_is_not_found() {
    let engine = FakeEngine::start(&[]).unwrap();
    let docker = client(&engine, CALL);
    let error = docker.container_inspect(ID).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound);
    assert!(error.is_not_found());
    assert!(error.to_string().contains("No such object"), "{error}");
    // Not a failure to reach the engine: nothing else was asked of it.
    assert_eq!(calls(&engine).len(), 1);
}

/// An empty ID is refused before any request, as moby refuses it.
#[tokio::test]
async fn an_empty_id_is_refused_without_a_request() {
    let engine = FakeEngine::start(&[]).unwrap();
    let docker = client(&engine, CALL);
    let error = docker.container_inspect("  ").await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "invalid container name or ID: value is empty"
    );
    assert!(engine.requests().is_empty());
}

/// The subscription asks for the `since` it was given and the event filter, and yields the events
/// in order, then the engine's close as an EOF.
#[tokio::test]
async fn the_events_come_with_since_and_filters_until_the_stream_closes() {
    let first = event("start", "c1", 1_767_323_046_000_000_001);
    let second = event("die", "c1", 1_767_323_047_000_000_002);
    let engine = FakeEngine::start(&[("/events", Reply::events(&[&first, &second]))]).unwrap();
    let docker = client(&engine, CALL);
    let since = Timestamp::from_nanosecond(1_767_323_045_000_000_006).unwrap();
    let filters = Filters::new().add("event", &["start", "die"]);

    let mut stream = docker.events(since, &filters).await.unwrap();
    let start = stream.next().await.unwrap();
    assert_eq!(
        (
            start.kind.as_str(),
            start.action.as_str(),
            start.actor.id.as_str()
        ),
        ("container", "start", "c1")
    );
    assert_eq!(start.actor.attributes["name"], "web");
    assert_eq!(start.time, 1_767_323_046);
    assert_eq!(start.time_nano, 1_767_323_046_000_000_001);
    assert_eq!(stream.next().await.unwrap().action, "die");
    let end = stream.next().await.unwrap_err();
    assert_eq!(end.kind(), StreamErrorKind::Eof);
    assert_eq!(end.to_string(), "EOF");
    // The stream is over for good.
    assert_eq!(
        stream.next().await.unwrap_err().kind(),
        StreamErrorKind::Eof
    );

    let request = calls(&engine).pop().unwrap();
    assert_eq!(request.method, "GET");
    let query = request.query();
    assert!(query.contains("since=1767323045.000000006"), "{query}");
    // moby's filter encoding: `{"event":["die","start"]}` here, percent-encoded.
    assert!(query.contains("filters="), "{query}");
    assert!(query.contains("start") && query.contains("die"), "{query}");
}

/// A stream the engine closed is opened again from one nanosecond after its last event, which is
/// what the event loop asks of the client, and answers from where it left off.
#[tokio::test]
async fn a_closed_stream_is_opened_again_from_the_last_event() {
    let first = event("start", "c1", 1_767_323_046_000_000_001);
    let engine = FakeEngine::start(&[("/events", Reply::events(&[&first]))]).unwrap();
    let docker = client(&engine, CALL);
    let filters = Filters::new().add("event", &["start"]);
    let initial = Timestamp::from_nanosecond(1_767_323_045_000_000_000).unwrap();

    let mut stream = docker.events(initial, &filters).await.unwrap();
    let last = stream.next().await.unwrap().time_nano;
    assert_eq!(
        stream.next().await.unwrap_err().kind(),
        StreamErrorKind::Eof
    );
    drop(stream);

    let again = Timestamp::from_nanosecond(i128::from(last) + 1).unwrap();
    let mut stream = docker.events(again, &filters).await.unwrap();
    assert_eq!(stream.next().await.unwrap().action, "start");

    let sinces: Vec<_> = calls(&engine)
        .iter()
        .map(|request| {
            request
                .query()
                .split('&')
                .find_map(|pair| pair.strip_prefix("since="))
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(sinces, ["1767323045.000000000", "1767323046.000000002"]);
}

/// A record that is not an event ends the stream with a decode error, and the stream stays over.
#[tokio::test]
async fn a_record_that_is_not_an_event_ends_the_stream() {
    let mut body =
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n".to_vec();
    body.extend_from_slice(b"{\"Action\": [not json\n");
    let engine = FakeEngine::start(&[("/events", Reply::Raw(body))]).unwrap();
    let docker = client(&engine, CALL);
    let mut stream = docker
        .events(Timestamp::UNIX_EPOCH, &Filters::new())
        .await
        .unwrap();
    let end = stream.next().await.unwrap_err();
    assert_ne!(end.kind(), StreamErrorKind::Eof, "{end}");
    assert_eq!(
        stream.next().await.unwrap_err().kind(),
        StreamErrorKind::Eof
    );
}

/// A body that stalls after its headers outlasts bollard's own timeout (the headers), so the
/// client's whole-call bound ends the call, as a timeout.
#[tokio::test]
async fn a_stalled_body_hits_the_whole_call_timeout() {
    let engine = FakeEngine::start(&[("/containers/json", Reply::Stalled)]).unwrap();
    let docker = client(&engine, Duration::from_millis(200));
    let error = tokio::time::timeout(Duration::from_secs(5), docker.container_list())
        .await
        .expect("the call hung")
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Timeout);
    assert!(!error.is_canceled());
}

/// The caller's deadline ends the same stalled call first when it is the shorter one, with Go's
/// bare `context deadline exceeded`, and its cancellation with `context canceled`.
#[tokio::test]
async fn the_callers_context_ends_a_stalled_call() {
    let engine = FakeEngine::start(&[("/containers/json", Reply::Stalled)]).unwrap();
    let docker = client(&engine, Duration::from_secs(30));

    let context = CallContext::new().with_timeout(Duration::from_millis(100));
    let error = with_context(&context, docker.container_list())
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "context deadline exceeded");
    assert_eq!(error.kind(), ErrorKind::Timeout);

    let context = CallContext::new();
    let canceler = context.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        canceler.cancel();
    });
    let error = with_context(&context, docker.container_list())
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "context canceled");
    assert!(error.is_canceled());
}

/// An engine that is gone is unavailable, which is neither a missing container nor a timeout.
#[tokio::test]
async fn an_engine_that_is_gone_is_unavailable() {
    let engine = FakeEngine::start(&[]).unwrap();
    let docker = client(&engine, CALL);
    drop(engine);
    let error: Error = docker.ping().await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    assert_eq!(ClientKind::Unavailable, error.kind());
}

/// A socket that does not exist is refused when the client is built, with the path in the text.
#[test]
fn a_socket_that_does_not_exist_is_refused_up_front() {
    let error = Client::unix("/nonexistent/docker.sock", CALL, CALL).unwrap_err();
    assert!(
        error.to_string().contains("/nonexistent/docker.sock"),
        "{error}"
    );
}
