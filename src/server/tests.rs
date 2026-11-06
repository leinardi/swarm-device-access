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

//! The server's tests, on real loopback sockets: the routes (liveness, readiness before and after
//! the daemon is ready, a 404, the scrape), the deadlines (a client slow to send its head is cut),
//! and, in [`shutdown`], the start and the stop. Go's `TestDebugServer_ProfileCompletes` has no
//! port: there is no debug server, and `refuse_debug_addr_is_q2` stands in its place.

use axum::routing::any;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::time::Instant;

use super::*;

mod shutdown;

/// How long a test waits for something that should happen at once.
const BOUND: Duration = Duration::from_secs(5);

/// The test-only route that waits.
const SLOW_PATH: &str = "/slow";

/// Readiness set to `ready`.
fn readiness(ready: bool) -> Readiness {
    let readiness = Readiness::default();
    readiness.set(ready);
    readiness
}

/// The daemon's routes, ready, that also serve [`SLOW_PATH`], answered after `wait`.
fn slow_router(wait: Duration) -> Router {
    router(readiness(true), Recorder::noop()).route(
        SLOW_PATH,
        any(move || async move {
            tokio::time::sleep(wait).await;
            "slow done\n"
        }),
    )
}

/// A ready app whose [`SLOW_PATH`] waits `wait`.
fn slow_app(wait: Duration) -> App {
    App::with_router(slow_router(wait))
}

/// The deadlines of a test: a head within 200 ms, and the given drain.
fn short(drain: Duration) -> ServerConfig {
    ServerConfig::new(Duration::from_millis(200), WRITE_STALL_TIMEOUT, drain)
}

/// `app` listening on a free loopback port, with `config`.
async fn serving(app: App, config: ServerConfig) -> (MetricsServer, SocketAddr) {
    let server = start_with(&CancellationToken::new(), "127.0.0.1:0", app, config)
        .await
        .unwrap();
    let address = server.local_addr().unwrap();
    (server, address)
}

/// `request` on a fresh connection to `address`, and everything the server answered before it
/// closed.
async fn exchange(address: SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    let _ = tokio::time::timeout(BOUND, stream.read_to_end(&mut response)).await;
    String::from_utf8_lossy(&response).into_owned()
}

/// `GET path` with `Connection: close`.
async fn get(address: SocketAddr, path: &str) -> String {
    exchange(
        address,
        &format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"),
    )
    .await
}

/// The status line and the body of a response without the chunked framing of a long one.
fn status(response: &str) -> &str {
    response.lines().next().unwrap_or_default()
}

/// `/healthz` is always `ok`, `/readyz` answers 503 until the daemon is ready and 200 after, and
/// anything else is a 404.
#[tokio::test]
async fn the_routes_answer() {
    let ready = Readiness::default();
    let app = App::new(ready.clone(), Recorder::noop());
    let (server, address) = serving(app, short(SHUTDOWN_TIMEOUT)).await;

    let healthz = get(address, "/healthz").await;
    assert_eq!(status(&healthz), "HTTP/1.1 200 OK", "{healthz}");
    assert!(healthz.ends_with("\r\n\r\nok"), "{healthz}");

    let before = get(address, "/readyz").await;
    assert_eq!(
        status(&before),
        "HTTP/1.1 503 Service Unavailable",
        "{before}"
    );
    assert!(before.ends_with("\r\n\r\nnot ready"), "{before}");

    ready.set(true);
    let after = get(address, "/readyz").await;
    assert_eq!(status(&after), "HTTP/1.1 200 OK", "{after}");
    assert!(after.ends_with("\r\n\r\nok"), "{after}");

    let unknown = get(address, "/nope").await;
    assert_eq!(status(&unknown), "HTTP/1.1 404 Not Found", "{unknown}");
    let post = exchange(
        address,
        "POST /healthz HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(
        status(&post),
        "HTTP/1.1 200 OK",
        "any method is answered: {post}"
    );
    server.stop().await;
}

/// `/metrics` serves the recorder's registry as OpenMetrics text: its `sda_` and process
/// families, no promhttp or Go runtime ones, and never compressed.
#[tokio::test]
async fn metrics_serves_openmetrics_text() {
    let recorder = Recorder::new();
    let app = App::new(readiness(true), recorder.clone());
    let (server, address) = serving(app, short(SHUTDOWN_TIMEOUT)).await;

    let request = "GET /metrics HTTP/1.1\r\nHost: x\r\nAccept-Encoding: gzip\r\n\
                   Connection: close\r\n\r\n";
    let first = exchange(address, request).await;
    assert_eq!(status(&first), "HTTP/1.1 200 OK", "{first}");
    assert!(
        first.to_lowercase().contains(
            "content-type: application/openmetrics-text; version=1.0.0; charset=utf-8\r\n"
        ),
        "{first}"
    );
    assert!(
        !first.to_lowercase().contains("content-encoding"),
        "{first}"
    );
    assert!(first.contains("# EOF\n"), "{first}");
    assert!(first.contains("process_"), "{first}");
    assert!(!first.contains("promhttp_"), "{first}");
    assert!(!first.contains("go_goroutines"), "{first}");
    assert!(!first.contains("sda_events_total{"), "{first}");

    recorder.record_event("start");
    let second = exchange(address, request).await;
    assert!(
        second.contains("sda_events_total{event=\"start\"} 1\n"),
        "{second}"
    );
    server.stop().await;
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

/// A client that sends only part of its head is cut at the header-read timeout (200 ms here, the
/// daemon's 5 s), not left to hold the connection.
#[tokio::test]
async fn a_client_slow_to_send_its_head_is_cut() {
    let (server, address) = serving(slow_app(Duration::ZERO), short(SHUTDOWN_TIMEOUT)).await;
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\n")
        .await
        .unwrap();
    let start = Instant::now();
    let mut rest = Vec::new();
    tokio::time::timeout(BOUND, stream.read_to_end(&mut rest))
        .await
        .expect("the server held the connection past the timeout")
        .ok();
    assert!(
        start.elapsed() >= Duration::from_millis(150),
        "cut before the timeout: {:?}",
        start.elapsed()
    );
    // The daemon's own limits are the documented ones.
    assert_eq!(HEADER_READ_TIMEOUT, Duration::from_secs(5));
    assert_eq!(WRITE_STALL_TIMEOUT, Duration::from_secs(30));
    assert_eq!(SHUTDOWN_TIMEOUT, Duration::from_secs(5));
    server.stop().await;
}
