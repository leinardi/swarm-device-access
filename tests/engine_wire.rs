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

// Adapted from swarm-scheduler-exporter/tests/engine_wire.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The Rust Docker client against the Go daemon's wire (`tests/testdata/engine_wire`): driven
//! through the calls the Go daemon made, in the same order, against a fake engine replaying what
//! the real daemon answered, it must send the same requests (method, URI, `Host`, every other
//! header and its values in order, and which connection each one took) and decode the same
//! answers.

mod support;

use jiff::Timestamp;
use swarm_device_access::docker::{
    Client, DockerApi as _, ErrorKind, EventStream as _, Filters, StreamErrorKind,
};

use support::OrPanic as _;
use support::fake_engine::{Capture, FakeEngine};

/// The Go daemon's event filter.
fn daemon_filters() -> Filters {
    Filters::new().add("event", &["start", "unpause", "die", "destroy"])
}

/// The `since` of a captured events URI, back as the time the Go daemon passed.
fn since_of(uri: &str) -> Timestamp {
    let since = uri
        .split(['?', '&'])
        .find_map(|pair| pair.strip_prefix("since="))
        .unwrap_or_else(|| panic!("no since in {uri}"));
    let (seconds, nanos) = since
        .split_once('.')
        .unwrap_or_else(|| panic!("since {since}"));
    Timestamp::new(seconds.parse().or_panic(), nanos.parse().or_panic()).or_panic()
}

/// The container IDs the Go daemon inspected, in order.
fn inspected_ids(capture: &Capture) -> Vec<String> {
    capture
        .requests()
        .iter()
        .filter_map(|request| {
            request
                .uri
                .strip_prefix("/v1.56/containers/")?
                .strip_suffix("/json")
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
        })
        .collect()
}

async fn inspect(client: &Client, id: &str) {
    let inspect = client.container_inspect(id).await.or_panic();
    assert_eq!(inspect.id, id);
}

fn assert_same_wire(engine: &FakeEngine, capture: &Capture) {
    assert_eq!(engine.unexpected_requests(), Vec::<String>::new());
    let sent = engine.requests();
    let captured = capture.requests();
    for (index, (sent, captured)) in sent.iter().zip(&captured).enumerate() {
        assert_eq!(sent, captured, "request {}", index + 1);
    }
    assert_eq!(sent.len(), captured.len(), "{sent:#?}");
}

/// `TestCaptureDaemonWire`: the startup ping, the subscription (negotiating on its way), the
/// enumeration, the inspects that the start and unpause events cause, the cut of the stream,
/// the ping and resubscription since the last event plus one nanosecond, the re-enumeration,
/// and the die and destroy events, which cause no request.
#[tokio::test]
async fn the_daemon_s_calls_send_the_go_daemon_s_requests() {
    let capture = Capture::load("daemon.jsonl");
    let engine = FakeEngine::start(capture.clone());
    let client = engine.client();
    let inspected = inspected_ids(&capture);
    assert_eq!(inspected.len(), 5);

    client.ping().await.or_panic();
    let mut events = client
        .events(
            since_of(&capture.uri("GET", "/v1.56/events", 0)),
            &daemon_filters(),
        )
        .await
        .or_panic();
    let listed = client.container_list().await.or_panic();
    assert_eq!(client.container_list().await.or_panic(), listed);
    // The first container was running at startup; the second started after the list, and
    // its start and unpause events caused the next two inspects.
    assert!(listed.iter().any(|summary| summary.id == inspected[0]));
    for id in &inspected[..3] {
        inspect(&client, id).await;
    }

    let mut actions = Vec::new();
    let end = loop {
        match events.next().await {
            Ok(event) => actions.push(event.action),
            Err(error) => break error,
        }
    };
    assert_eq!(actions, ["start", "unpause"]);
    // The capture cut the stream: the end of a chunked body without its last chunk.
    assert_eq!(end.kind(), StreamErrorKind::UnexpectedEof, "{end}");

    client.ping().await.or_panic();
    let mut events = client
        .events(
            since_of(&capture.uri("GET", "/v1.56/events", 1)),
            &daemon_filters(),
        )
        .await
        .or_panic();
    let relisted = client.container_list().await.or_panic();
    assert_eq!(relisted.len(), 2);
    for id in &inspected[3..] {
        inspect(&client, id).await;
    }
    let mut actions = Vec::new();
    let end = loop {
        match events.next().await {
            Ok(event) => actions.push(event.action),
            Err(error) => break error,
        }
    };
    assert_eq!(actions, ["die", "destroy"]);
    assert!(end.is_eof(), "{end}");

    assert_same_wire(&engine, &capture);
}

/// `TestCaptureDaemonPingFallback`: an Engine that refuses `HEAD /_ping` gets `GET /_ping`, for
/// the startup ping and again for the negotiation of the subscription.
#[tokio::test]
async fn a_refused_head_ping_falls_back_to_get_as_the_go_daemon_did() {
    let capture = Capture::load("daemon-ping-fallback.jsonl");
    let engine = FakeEngine::start(capture.clone());
    let client = engine.client();

    client.ping().await.or_panic();
    let mut events = client
        .events(
            since_of(&capture.uri("GET", "/v1.56/events", 0)),
            &daemon_filters(),
        )
        .await
        .or_panic();
    assert_eq!(client.container_list().await.or_panic(), []);
    assert_eq!(client.container_list().await.or_panic(), []);
    assert!(events.next().await.or_panic_err().is_eof());
    assert_eq!(client.client_version().await, "1.56");

    assert_same_wire(&engine, &capture);
}

/// The launcher capture's inspect of a container that does not exist: moby's 404 is not found,
/// with the daemon's message, on the request the Go client sent.
#[tokio::test]
async fn a_404_inspect_is_not_found() {
    let capture = Capture::load("launcher.jsonl");
    let missing = "0".repeat(64);
    let engine = FakeEngine::start(capture.clone());
    let client = engine.client();

    let error = client.container_inspect(&missing).await.or_panic_err();
    assert_eq!(error.kind(), ErrorKind::Response);
    assert!(error.is_not_found());
    assert_eq!(
        error.to_string(),
        format!("Error response from daemon: No such container: {missing}")
    );

    assert_eq!(engine.unexpected_requests(), Vec::<String>::new());
    let wanted: Vec<_> = capture
        .requests()
        .into_iter()
        .filter(|request| {
            request.uri == "/_ping" || request.uri == format!("/v1.56/containers/{missing}/json")
        })
        .collect();
    assert_eq!(wanted.len(), 2, "{wanted:#?}");
    let sent = engine.requests();
    assert_eq!(sent.len(), 2);
    for (sent, wanted) in sent.iter().zip(&wanted) {
        // The launcher's connections are not this client's: compare the requests only.
        assert_eq!(
            (&sent.method, &sent.uri, &sent.host, &sent.header),
            (&wanted.method, &wanted.uri, &wanted.host, &wanted.header)
        );
    }
}
