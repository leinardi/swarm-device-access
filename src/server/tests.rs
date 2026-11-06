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

//! The server's tests: Go's endpoint tests (`http_test.go`) and the scrape the `/metrics` route
//! hands promhttp here, the Go wire captures in [`wire`], the deadlines in [`timeouts`], and the
//! start and the stop in [`shutdown`]. Go's `TestDebugServer_ProfileCompletes` has no port:
//! there is no debug server, and `refuse_debug_addr_is_q2` stands in its place. The routing,
//! framing, bind and drain tests are go-http's.

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;

use bytes::Bytes;
use go_http::TEXT_PLAIN;
use hyper::header::{CONTENT_TYPE, HeaderValue};
use jiff::Timestamp;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, DuplexStream};

use super::*;
use clock::FakeClock;

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
        content_type: exposition::content_type(go_prom::ExpfmtVersion::V0_70_1, None),
        content_encoding: None,
        body: body.to_vec(),
    }
}

/// Readiness set to `ready`.
fn readiness(ready: bool) -> Readiness {
    let readiness = Readiness::default();
    readiness.set(ready);
    readiness
}

/// The daemon's mux, ready or not, answering every scrape with `metrics`.
fn mux_with(
    ready: bool,
    metrics: impl Fn(&MetricsRequest) -> Exposition + Send + Sync + 'static,
) -> Mux {
    mux(readiness(ready), metrics)
}

/// `mux` served at the captures' time, its messages captured.
fn app_of(mux: Mux) -> App {
    App::with_mux(mux, Arc::new(FakeClock::new(capture_time())))
}

/// A ready app answering every scrape with `metrics`.
fn app_with(metrics: Exposition) -> App {
    app_of(mux_with(true, move |_| metrics.clone()))
}

fn healthy_app() -> App {
    app_with(text_exposition(b"m 1\n"))
}

/// Serves one connection of `app` over a duplex and returns the client end.
fn connect(app: App) -> DuplexStream {
    let (client, server_io) = tokio::io::duplex(1 << 20);
    let connection = go_http::connection(server_io, app.server, TIMEOUTS);
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
        App::new(
            readiness.clone(),
            Recorder::new(),
            Arc::new(FakeClock::new(capture_time())),
        )
    };
    let send = |app: App, request: String| async move {
        let (mut client, server_io) = tokio::io::duplex(1 << 20);
        let served = go_http::connection(server_io, app.server, TIMEOUTS);
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

/// promhttp reads the first `Accept`, every `Accept-Encoding` and the `name[]` filter; a gzip
/// answer carries its `Content-Encoding` ahead of the `Content-Type`.
#[tokio::test(start_paused = true)]
async fn the_scrape_request_reaches_the_metrics_function() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    let app = app_of(mux_with(true, move |scrape: &MetricsRequest| {
        recorded.lock().unwrap().push(scrape.clone());
        Exposition {
            content_type: exposition::content_type(go_prom::ExpfmtVersion::V0_70_1, None),
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
    let app = app_of(mux_with(true, move |scrape: &MetricsRequest| {
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

/// The test-only route that waits.
const SLOW_PATH: &str = "/slow";

/// A slow route's wait, made per request.
type SlowRoute = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// A slow route for the deadline and stop tests: it waits `duration`.
fn sleeping(duration: Duration) -> SlowRoute {
    Arc::new(move || Box::pin(tokio::time::sleep(duration)))
}

/// The daemon's mux, ready, that also serves [`SLOW_PATH`], answered once `wait` is done.
fn slow_mux(wait: SlowRoute) -> Mux {
    let metrics = text_exposition(b"m 1\n");
    mux_with(true, move |_| metrics.clone()).route_async(SLOW_PATH, move |_| {
        let waiting = wait();
        async move {
            waiting.await;
            Reply::new(
                StatusCode::OK,
                vec![(CONTENT_TYPE, HeaderValue::from_static(TEXT_PLAIN))],
                Bytes::from_static(b"slow done\n"),
            )
        }
    })
}

/// A healthy app whose [`SLOW_PATH`] waits `duration`.
fn slow_app(duration: Duration) -> App {
    app_of(slow_mux(sleeping(duration)))
}
