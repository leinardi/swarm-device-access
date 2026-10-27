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

// Adapted from swarm-scheduler-exporter/src/server/tests.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The server's tests: Go's endpoint tests (`http_test.go`) and the framing rules here, the Go
//! wire captures in [`wire`], the deadlines in [`timeouts`], and the listener, the drain, the
//! start and the stop in [`shutdown`]. Go's `TestDebugServer_ProfileCompletes` has no port:
//! there is no debug server, and `refuse_debug_addr_is_q2` stands in its place.

use std::time::Duration;

use bytes::Bytes;
use jiff::Timestamp;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, DuplexStream};

use super::*;
use crate::clock::FakeClock;
use crate::logger::Level;
use crate::test_support::capture_logs;

mod shutdown;
mod timeouts;
mod wire;

/// The time of the Go captures, so `Date` compares exactly.
fn capture_time() -> Timestamp {
    "2026-10-27T15:18:56Z".parse().unwrap()
}

/// An exposition of `body` in the text format, as promhttp answers a scrape with no `Accept`.
fn text_exposition(body: &'static [u8]) -> Exposition {
    Exposition {
        content_type: exposition::content_type(None),
        content_encoding: None,
        body: body.to_vec(),
    }
}

/// An app with readiness `ready` and a recorder of its own.
fn app_ready(ready: bool) -> App {
    let readiness = Readiness::default();
    readiness.set(ready);
    App::new(
        readiness,
        Recorder::new(),
        Arc::new(FakeClock::new(capture_time())),
        capture_logs(Level::INFO).0,
    )
}

/// A ready app answering every scrape with `metrics`.
fn app_with(metrics: Exposition) -> App {
    let mut app = app_ready(true);
    app.exposition = Some(Box::new(move |_| metrics.clone()));
    app
}

fn healthy_app() -> App {
    app_with(text_exposition(b"m 1\n"))
}

/// Serves one connection of `app` over a duplex and returns the client end.
fn connect(app: App) -> DuplexStream {
    let (client, server_io) = tokio::io::duplex(1 << 20);
    let connection = connection(server_io, Arc::new(app), Timeouts::DAEMON);
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

/// Sends `request` on a fresh connection of `app` and returns everything the server wrote
/// before it closed.
async fn exchange(app: App, request: &[u8]) -> Vec<u8> {
    let mut client = connect(app);
    client.write_all(request).await.unwrap();
    let mut response = Vec::new();
    let _ = client.read_to_end(&mut response).await;
    response
}

async fn exchange_text(app: App, request: &str) -> String {
    String::from_utf8(exchange(app, request.as_bytes()).await).unwrap()
}

/// Go: `TestMetricsServer_Endpoints`. `/healthz` is always `ok`, `/readyz` answers 503 until the
/// daemon is ready and 200 after, and `/metrics` serves the recorder's registry (its process
/// and handler families; Go's `go_*` families are gone, Q3).
#[tokio::test(start_paused = true)]
async fn metrics_server_endpoints() {
    let get = |path: &str| format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    let readiness = Readiness::default();
    let app = || {
        Arc::new(App::new(
            readiness.clone(),
            Recorder::new(),
            Arc::new(FakeClock::new(capture_time())),
            capture_logs(Level::INFO).0,
        ))
    };
    let send = |app: Arc<App>, request: String| async move {
        let (mut client, server_io) = tokio::io::duplex(1 << 20);
        let served = connection(server_io, app, Timeouts::DAEMON);
        tokio::spawn(async move {
            let _ = served.await;
        });
        client.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        let _ = client.read_to_end(&mut response).await;
        String::from_utf8(response).unwrap()
    };

    let healthz = send(app(), get("/healthz")).await;
    assert!(healthz.starts_with("HTTP/1.1 200 OK\r\n"), "{healthz}");
    assert!(healthz.ends_with("\r\n\r\nok"), "{healthz}");

    let readyz = send(app(), get("/readyz")).await;
    assert!(
        readyz.starts_with("HTTP/1.1 503 Service Unavailable\r\n"),
        "{readyz}"
    );
    assert!(readyz.ends_with("\r\n\r\nnot ready"), "{readyz}");

    readiness.set(true);
    let readyz = send(app(), get("/readyz")).await;
    assert!(readyz.starts_with("HTTP/1.1 200 OK\r\n"), "{readyz}");
    assert!(readyz.ends_with("\r\n\r\nok"), "{readyz}");

    let metrics = send(app(), get("/metrics")).await;
    assert!(metrics.starts_with("HTTP/1.1 200 OK\r\n"), "{metrics}");
    assert!(
        metrics.contains("promhttp_metric_handler_requests_in_flight 1\n"),
        "{metrics}"
    );
    assert!(metrics.contains("process_"), "{metrics}");
    assert!(!metrics.contains("go_goroutines"), "{metrics}");
}

/// The scrape is counted under its status once answered, as promhttp's instrumentation counts
/// it: the second scrape sees the first.
#[tokio::test(start_paused = true)]
async fn scrapes_are_counted() {
    let recorder = Recorder::new();
    let app = || {
        App::new(
            Readiness::default(),
            recorder.clone(),
            Arc::new(FakeClock::new(capture_time())),
            capture_logs(Level::INFO).0,
        )
    };
    let request = "GET /metrics HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n";
    exchange_text(app(), request).await;
    let second = exchange_text(app(), request).await;
    assert!(
        second.contains("promhttp_metric_handler_requests_total{code=\"200\"} 1\n"),
        "{second}"
    );
}

/// Q2 (Go: `TestDebugServer_ProfileCompletes`, whose pprof server is gone): an empty
/// `debug-addr` is accepted, and any other value refused with its reason.
#[test]
fn refuse_debug_addr_is_q2() {
    assert_eq!(refuse_debug_addr(""), Ok(()));
    let refused = refuse_debug_addr("127.0.0.1:6060").unwrap_err();
    assert_eq!(
        refused.to_string(),
        "debug-addr \"127.0.0.1:6060\": the pprof debug server was removed in 2.0; leave \
         debug-addr empty"
    );
}

/// Go's framing: up to 2 KiB a `Content-Length`, above it chunks of 2 KiB.
#[tokio::test(start_paused = true)]
async fn bodies_above_2_kib_are_chunked() {
    let exact = vec![b'x'; 2048].leak();
    let response = exchange(
        app_with(text_exposition(exact)),
        b"GET /metrics HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;
    let text = String::from_utf8_lossy(&response);
    assert!(text.contains("\r\nContent-Length: 2048\r\n"), "{text}");
    assert!(!text.contains("Transfer-Encoding"), "{text}");

    let over = vec![b'x'; 4097].leak();
    let response = exchange(
        app_with(text_exposition(over)),
        b"GET /metrics HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;
    let text = String::from_utf8_lossy(&response);
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    assert!(
        head.ends_with("\r\nConnection: close\r\nTransfer-Encoding: chunked"),
        "{head}"
    );
    assert!(!head.contains("Content-Length"), "{head}");
    let x2048 = "x".repeat(2048);
    assert_eq!(
        body,
        format!("800\r\n{x2048}\r\n800\r\n{x2048}\r\n1\r\nx\r\n0\r\n\r\n")
    );

    // HEAD: neither header, as Go writes it.
    let response = exchange(
        app_with(text_exposition(over)),
        b"HEAD /metrics HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;
    let text = String::from_utf8_lossy(&response);
    assert!(!text.contains("Content-Length"), "{text}");
    assert!(!text.contains("Transfer-Encoding"), "{text}");
    assert!(text.ends_with("\r\n\r\n"), "{text}");
}

/// promhttp reads the first `Accept`, every `Accept-Encoding` and the `name[]` filter; a gzip
/// answer carries its `Content-Encoding` ahead of the `Content-Type`.
#[tokio::test(start_paused = true)]
async fn the_scrape_request_reaches_the_metrics_function() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    let mut app = app_ready(true);
    app.exposition = Some(Box::new(move |scrape: &MetricsRequest| {
        recorded.lock().unwrap().push(scrape.clone());
        Exposition {
            content_type: exposition::content_type(None),
            content_encoding: Some("gzip"),
            body: b"z".to_vec(),
        }
    }));
    let response = exchange_text(
        app,
        "GET /metrics?name[]=up&name%5B%5D=x&other=1 HTTP/1.1\r\nHost: x\r\nAccept: text/plain\r\n\
         Accept: application/json\r\nAccept-Encoding: gzip\r\nAccept-Encoding: br\r\n\
         Connection: close\r\n\r\n",
    )
    .await;
    assert!(
        response.starts_with(
            "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Type: text/plain; \
             version=0.0.4; charset=utf-8; escaping=underscores\r\nDate: "
        ),
        "{response}"
    );
    let seen = seen.lock().unwrap();
    assert_eq!(
        *seen,
        [MetricsRequest {
            accept: Some("text/plain".to_owned()),
            accept_encoding: vec!["gzip".to_owned(), "br".to_owned()],
            names: Some(BTreeSet::from(["up".to_owned(), "x".to_owned()])),
        }]
    );
}

/// Without `name[]`, no filter.
#[tokio::test(start_paused = true)]
async fn a_scrape_without_names_has_no_filter() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    let mut app = app_ready(true);
    app.exposition = Some(Box::new(move |scrape: &MetricsRequest| {
        recorded.lock().unwrap().push(scrape.clone());
        text_exposition(b"")
    }));
    exchange(
        app,
        b"GET /metrics?a=b HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(*seen.lock().unwrap(), [MetricsRequest::default()]);
}

/// Go's `LookupPort` answers, from a static Go program run without `/etc/services`, as in the
/// daemon's image.
#[test]
fn ports_parse_as_go_s() {
    assert_eq!(parse_port("8888"), Ok(8888));
    assert_eq!(parse_port("+80"), Ok(80));
    assert_eq!(
        parse_port("70000"),
        Err("address 70000: invalid port".to_owned())
    );
    assert_eq!(parse_port("-1"), Err("address -1: invalid port".to_owned()));
    assert_eq!(parse_port("http"), Ok(80));
    assert_eq!(parse_port("HTTP"), Ok(80));
    assert_eq!(parse_port("ssh"), Ok(22));
    assert_eq!(
        parse_port("domain"),
        Err("lookup tcp/domain: unknown port".to_owned())
    );
    assert_eq!(parse_port(""), Ok(0));
    assert_eq!(parse_port("+"), Ok(0));
    assert_eq!(parse_port("-0"), Ok(0));
    assert_eq!(parse_port("08"), Ok(8));
    assert_eq!(
        parse_port("0x10"),
        Err("lookup tcp/0x10: unknown port".to_owned())
    );
}

/// A slow route for the deadline and drain tests: it waits `duration`.
fn sleeping(duration: Duration) -> SlowRoute {
    Arc::new(move || Box::pin(tokio::time::sleep(duration)))
}

fn slow_app(duration: Duration) -> App {
    let mut app = healthy_app();
    app.slow = Some(sleeping(duration));
    app
}
